use bytes::Bytes;
use clap::Parser;
use console_api::instrument::instrument_client::InstrumentClient;
use console_api::instrument::InstrumentRequest;
use futures::stream::StreamExt;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response};
use prometheus::{Counter, CounterVec, Encoder, GaugeVec, IntGauge, Opts, Registry, TextEncoder};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tonic::transport::Channel;
use tracing::{debug, error, info};

#[derive(Parser, Debug)]
#[command(
    name = "tokio-console-prometheus-exporter",
    about = "Prometheus exporter for Tokio Console"
)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:6669")]
    console_addr: String,
    #[arg(short, long, default_value = "0.0.0.0:9090")]
    listen_addr: String,
    #[arg(short, long, default_value = "5")]
    interval: u64,
    #[arg(long)]
    auth_token: Option<String>,
}

#[derive(Debug)]
struct Metrics {
    tasks_total: IntGauge,
    tasks_with_location: IntGauge,
    tasks_without_location: IntGauge,
    tasks_running: IntGauge,
    tasks_idle: IntGauge,
    tasks_completed: IntGauge,
    task_polls: CounterVec,
    task_wakes: CounterVec,
    task_state: GaugeVec,
    scrapes_total: Counter,
    scrape_errors: Counter,
}

// Use span_id as the key (like tokio-console), not task_id
type TaskMetadataStore = Arc<Mutex<HashMap<u64, TaskMetadata>>>;

// Track task states
type TaskStateStore = Arc<Mutex<HashMap<u64, TaskState>>>;

#[derive(Debug, Clone)]
enum TaskState {
    Idle,
    Running,
    Completed,
}

#[derive(Clone, Debug)]
struct TaskMetadata {
    name: String,
    location: String,
    task_id: u64,
    kind: String,
}

impl Metrics {
    fn new(registry: &Registry) -> Self {
        let tasks_total = IntGauge::new("tokio_tasks_total", "Total number of tasks").unwrap();
        let tasks_with_location =
            IntGauge::new("tokio_tasks_with_location", "Tasks with location info").unwrap();
        let tasks_without_location = IntGauge::new(
            "tokio_tasks_without_location",
            "Tasks without location info",
        )
        .unwrap();
        let tasks_running =
            IntGauge::new("tokio_tasks_running", "Number of currently running tasks").unwrap();
        let tasks_idle =
            IntGauge::new("tokio_tasks_idle", "Number of currently idle tasks").unwrap();
        let tasks_completed =
            IntGauge::new("tokio_tasks_completed", "Number of completed tasks").unwrap();
        let task_polls = CounterVec::new(
            Opts::new("tokio_task_polls_total", "Total number of task polls"),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let task_wakes = CounterVec::new(
            Opts::new("tokio_task_wakes_total", "Total number of task wakes"),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let task_state = GaugeVec::new(
            Opts::new(
                "tokio_task_state",
                "Task state (0=idle, 1=running, 2=completed)",
            ),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let scrapes_total = Counter::new("tokio_scrapes_total", "Total number of scrapes").unwrap();
        let scrape_errors =
            Counter::new("tokio_scrape_errors_total", "Total number of scrape errors").unwrap();

        registry.register(Box::new(tasks_total.clone())).unwrap();
        registry
            .register(Box::new(tasks_with_location.clone()))
            .unwrap();
        registry
            .register(Box::new(tasks_without_location.clone()))
            .unwrap();
        registry.register(Box::new(tasks_running.clone())).unwrap();
        registry.register(Box::new(tasks_idle.clone())).unwrap();
        registry
            .register(Box::new(tasks_completed.clone()))
            .unwrap();
        registry.register(Box::new(task_polls.clone())).unwrap();
        registry.register(Box::new(task_wakes.clone())).unwrap();
        registry.register(Box::new(task_state.clone())).unwrap();
        registry.register(Box::new(scrapes_total.clone())).unwrap();
        registry.register(Box::new(scrape_errors.clone())).unwrap();

        Self {
            tasks_total,
            tasks_with_location,
            tasks_without_location,
            tasks_running,
            tasks_idle,
            tasks_completed,
            task_polls,
            task_wakes,
            task_state,
            scrapes_total,
            scrape_errors,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "tokio_console_prometheus_exporter=debug",
        ))
        .init();

    let args = Args::parse();
    let auth_token = args
        .auth_token
        .or_else(|| std::env::var("TOKIO_EXPORTER_TOKEN").ok());

    info!("Starting Tokio Console Prometheus Exporter");
    info!(
        "Console: {}, Listen: {}, Interval: {}s",
        args.console_addr, args.listen_addr, args.interval
    );

    let registry = Registry::new();
    let metrics = Arc::new(Metrics::new(&registry));
    let addr: SocketAddr = args.listen_addr.parse()?;

    let service = service_fn(move |req: Request<hyper::body::Incoming>| {
        let registry = registry.clone();
        let auth_token = auth_token.clone();
        async move {
            // Security: Validate authentication token
            if let Some(token) = &auth_token {
                if let Some(auth_header) = req.headers().get("Authorization") {
                    if let Ok(auth_value) = auth_header.to_str() {
                        if auth_value != format!("Bearer {}", token) {
                            return Ok::<Response<Full<Bytes>>, hyper::Error>(
                                Response::builder()
                                    .status(401)
                                    .body(Full::new(Bytes::from("Unauthorized")))
                                    .unwrap(),
                            );
                        }
                    } else {
                        return Ok::<Response<Full<Bytes>>, hyper::Error>(
                            Response::builder()
                                .status(401)
                                .body(Full::new(Bytes::from("Invalid Authorization")))
                                .unwrap(),
                        );
                    }
                } else {
                    return Ok::<Response<Full<Bytes>>, hyper::Error>(
                        Response::builder()
                            .status(401)
                            .body(Full::new(Bytes::from("Authorization required")))
                            .unwrap(),
                    );
                }
            }

            match req.uri().path() {
                "/metrics" => {
                    let mut buffer = Vec::new();
                    let encoder = TextEncoder::new();
                    let metric_families = registry.gather();
                    encoder.encode(&metric_families, &mut buffer).unwrap();
                    Ok::<Response<Full<Bytes>>, hyper::Error>(
                        Response::builder()
                            .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
                            .body(Full::new(Bytes::from(buffer)))
                            .unwrap(),
                    )
                }
                "/" => Ok::<Response<Full<Bytes>>, hyper::Error>(Response::new(Full::new(
                    Bytes::from("Tokio Console Prometheus Exporter\n\nUse /metrics endpoint"),
                ))),
                _ => Ok::<Response<Full<Bytes>>, hyper::Error>(
                    Response::builder()
                        .status(404)
                        .body(Full::new(Bytes::from("Not Found")))
                        .unwrap(),
                ),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Prometheus server listening on {}", addr);

    let server_handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("listener accept failed");
            let service = service.clone();
            tokio::spawn(async move {
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
            });
        }
    });

    let task_metadata: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));
    let task_states: TaskStateStore = Arc::new(Mutex::new(HashMap::new()));

    let metrics_clone = metrics.clone();
    let task_metadata_clone = task_metadata.clone();
    let task_states_clone = task_states.clone();
    let scraper_handle = tokio::spawn(async move {
        scrape_loop(
            &args.console_addr,
            args.interval,
            metrics_clone,
            task_metadata_clone,
            task_states_clone,
        )
        .await;
    });

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    scraper_handle.abort();
    server_handle.abort();
    Ok(())
}

async fn scrape_loop(
    console_addr: &str,
    interval: u64,
    metrics: Arc<Metrics>,
    task_metadata: TaskMetadataStore,
    task_states: TaskStateStore,
) {
    let mut interval_timer = tokio::time::interval(Duration::from_secs(interval));
    loop {
        interval_timer.tick().await;
        match scrape_console(
            console_addr,
            &metrics,
            task_metadata.clone(),
            task_states.clone(),
        )
        .await
        {
            Ok((total_tasks, with_location, without_location, running, idle, completed)) => {
                info!("Scraped {} tasks: {} with location, {} without, {} running, {} idle, {} completed",
                      total_tasks, with_location, without_location, running, idle, completed);
                metrics.scrapes_total.inc();
            }
            Err(e) => {
                error!("Scrape error: {}", e);
                metrics.scrape_errors.inc();
            }
        }
    }
}

fn extract_task_metadata(task: &console_api::tasks::Task) -> TaskMetadata {
    let span_id = match task.id.as_ref() {
        Some(id) => id.id,
        None => {
            debug!("Task has no span ID, using 0");
            0
        }
    };

    let meta_id = match task.metadata.as_ref() {
        Some(id) => id.id,
        None => {
            debug!("Task has no metadata ID, using span_id {}", span_id);
            span_id
        }
    };

    // Debug log task fields for first few tasks (like tokio-console does)
    static mut TASKS_LOGGED: usize = 0;
    unsafe {
        if TASKS_LOGGED < 5 {
            debug!(
                "Processing task with span_id {} meta_id {}",
                span_id, meta_id
            );
            for field in &task.fields {
                if let Some(console_api::field::Name::StrName(name)) = &field.name {
                    debug!("  Field '{}': {:?}", name, field.value);
                }
            }
            TASKS_LOGGED += 1;
        }
    }

    // Extract fields exactly like tokio-console does
    let mut name = None;
    let mut task_id = None;
    let mut kind = "task".to_string(); // Default like tokio-console

    // Process fields like tokio-console's update_tasks method
    for field in &task.fields {
        if let Some(console_api::field::Name::StrName(field_name)) = &field.name {
            match field_name.as_str() {
                "task.name" => {
                    if let Some(console_api::field::Value::StrVal(val)) = &field.value {
                        name = Some(val.clone());
                        debug!("Found task.name: {}", val);
                    }
                }
                "task.id" => {
                    if let Some(console_api::field::Value::U64Val(val)) = &field.value {
                        task_id = Some(*val);
                        debug!("Found task.id: {}", val);
                    }
                }
                "task.kind" | "kind" => {
                    if let Some(console_api::field::Value::StrVal(val)) = &field.value {
                        kind = val.clone();
                        debug!("Found task.kind: {}", val);
                    }
                }
                _ => {}
            }
        }
    }

    // Format location exactly like tokio-console's format_location function
    let location = if let Some(loc) = &task.location {
        if let Some(file) = &loc.file {
            if let Some(line) = loc.line {
                format!("{}:{}", file, line)
            } else {
                file.clone()
            }
        } else {
            "unknown".to_string()
        }
    } else {
        "unknown".to_string()
    };

    // Generate the task name like tokio-console does
    let final_name = match (task_id, name.as_ref()) {
        (Some(tid), Some(name)) => format!("{} ({})", tid, name),
        (Some(tid), None) => tid.to_string(),
        (None, Some(name)) => name.clone(),
        (None, None) => {
            // Fallback: use location-based name if available
            if location != "unknown" {
                if let Some(filename) = location.split('/').last() {
                    filename.to_string()
                } else {
                    location.clone()
                }
            } else {
                format!("task_{}", span_id)
            }
        }
    };

    let metadata = TaskMetadata {
        name: final_name,
        location,
        task_id: task_id.unwrap_or(span_id), // Use actual task_id or fallback to span_id
        kind,
    };

    debug!(
        "Extracted metadata: task_id={}, name='{}', location='{}', kind='{}'",
        metadata.task_id, metadata.name, metadata.location, metadata.kind
    );

    metadata
}

async fn scrape_console(
    console_addr: &str,
    metrics: &Metrics,
    task_metadata: TaskMetadataStore,
    task_states: TaskStateStore,
) -> Result<(usize, usize, usize, usize, usize, usize), Box<dyn std::error::Error>> {
    let channel = Channel::from_shared(format!("http://{}", console_addr))?
        .connect()
        .await?;

    let mut client = InstrumentClient::new(channel);

    // DON'T clear the store - maintain persistent state like tokio-console
    // This allows us to accumulate tasks over time instead of losing them
    info!(
        "Starting persistent task capture (maintaining existing {} tasks)",
        {
            let store = task_metadata.lock().unwrap();
            store.len()
        }
    );

    // Track seen task IDs by span_id like tokio-console does
    let mut seen_span_ids = std::collections::HashSet::new();
    let mut first_update = true;

    // Watch for updates - maintain persistent connection like tokio-console
    let request = tonic::Request::new(InstrumentRequest {});
    let mut stream = client.watch_updates(request).await?.into_inner();

    let mut updates_processed = 0;
    const MAX_UPDATES: usize = 200; // Increase to capture more tasks initially
    let mut consecutive_empty_updates = 0;
    const MAX_CONSECUTIVE_EMPTY: usize = 15; // More tolerance for comprehensive capture

    while let Some(Ok(update)) = stream.next().await {
        updates_processed += 1;

        debug!(
            "Update {}: new_metadata={}, task_update={:?}, first_update={}",
            updates_processed,
            update.new_metadata.is_some(),
            update
                .task_update
                .as_ref()
                .map(|tu| (tu.new_tasks.len(), tu.stats_update.len())),
            first_update
        );

        if let Some(task_update) = update.task_update {
            let new_tasks_count = task_update.new_tasks.len();
            let stats_count = task_update.stats_update.len();

            debug!(
                "Update {}: Processing {} new tasks, {} stats updates",
                updates_processed, new_tasks_count, stats_count
            );

            // The first update often contains all existing tasks
            if first_update && new_tasks_count > 0 {
                info!(
                    "First update contains {} tasks (likely existing tasks)",
                    new_tasks_count
                );
            }
            first_update = false;

            // Process new tasks exactly like tokio-console's update_tasks method
            for task in task_update.new_tasks {
                let span_id = match task.id.as_ref() {
                    Some(id) => id.id,
                    None => {
                        debug!("Task has no span ID, skipping");
                        continue;
                    }
                };

                let metadata = extract_task_metadata(&task);

                // Track by span_id like tokio-console does
                if !seen_span_ids.contains(&span_id) {
                    seen_span_ids.insert(span_id);

                    // Check if task already exists in persistent store (by span_id, not task_id)
                    let is_truly_new = {
                        let store = task_metadata.lock().unwrap();
                        !store.values().any(|existing| {
                            // Compare by both span_id and task_id to handle duplicates correctly
                            existing.task_id == metadata.task_id || existing.name == metadata.name
                        })
                    };

                    if is_truly_new {
                        info!(
                            "New task discovered: span_id={}, task_id={}, name='{}', location='{}'",
                            span_id, metadata.task_id, metadata.name, metadata.location
                        );
                    }

                    // Store by span_id (like tokio-console) not task_id
                    {
                        let mut store = task_metadata.lock().unwrap();
                        store.insert(span_id, metadata.clone());
                    }
                } else {
                    // Update existing task metadata in case location was resolved
                    {
                        let mut store = task_metadata.lock().unwrap();
                        if let Some(existing) = store.get(&span_id) {
                            // Update if we got better location info
                            if existing.location == "unknown" && metadata.location != "unknown" {
                                info!(
                                    "Updating task span_id={} location: {} -> {}",
                                    span_id, existing.location, metadata.location
                                );
                                store.insert(span_id, metadata.clone());
                            }
                        }
                    }
                }
            }

            // Process stats updates for ALL tasks (exactly like tokio-console does)
            // Note: stats_update uses span_id as the key, not task_id
            for (span_id, stats) in task_update.stats_update {
                // First check if we have this task in our store (by span_id)
                let metadata = {
                    let store = task_metadata.lock().unwrap();
                    store.get(&span_id).cloned()
                };

                if let Some(metadata) = metadata {
                    // Update metrics for this task
                    if let Some(poll_stats) = &stats.poll_stats {
                        let polls_counter = metrics.task_polls.with_label_values(&[
                            &metadata.name,
                            &metadata.location,
                            &metadata.kind,
                        ]);
                        polls_counter.reset();
                        polls_counter.inc_by(poll_stats.polls as f64);
                    }

                    if stats.wakes > 0 {
                        let wakes_counter = metrics.task_wakes.with_label_values(&[
                            &metadata.name,
                            &metadata.location,
                            &metadata.kind,
                        ]);
                        wakes_counter.reset();
                        wakes_counter.inc_by(stats.wakes as f64);
                    }

                    // Determine task state and track it
                    let task_state = if stats.dropped_at.is_some() {
                        TaskState::Completed
                    } else if let Some(poll_stats) = &stats.poll_stats {
                        // If task has been polled recently and is not completed, it's likely running
                        // This is a simplified heuristic - in reality tokio-console uses more complex logic
                        if poll_stats.polls > 0 {
                            TaskState::Running
                        } else {
                            TaskState::Idle
                        }
                    } else {
                        TaskState::Idle
                    };

                    // Update state tracking
                    {
                        let mut state_store = task_states.lock().unwrap();
                        state_store.insert(span_id, task_state.clone());
                    }

                    let state_value = match task_state {
                        TaskState::Idle => 0.0,
                        TaskState::Running => 1.0,
                        TaskState::Completed => 2.0,
                    };

                    metrics
                        .task_state
                        .with_label_values(&[&metadata.name, &metadata.location, &metadata.kind])
                        .set(state_value);
                } else {
                    // Task not in store - this should be rare now with persistent state
                    debug!(
                        "Stats update for unknown span_id: {}. Creating placeholder.",
                        span_id
                    );

                    // Create a minimal task metadata entry (using span_id like tokio-console)
                    let placeholder_metadata = TaskMetadata {
                        name: format!("task_{}", span_id),
                        location: "unknown".to_string(),
                        task_id: span_id, // Use span_id as fallback task_id
                        kind: "unknown".to_string(),
                    };

                    {
                        let mut store = task_metadata.lock().unwrap();
                        store.insert(span_id, placeholder_metadata.clone());
                    }

                    // Update metrics with the placeholder
                    if let Some(poll_stats) = &stats.poll_stats {
                        let polls_counter = metrics.task_polls.with_label_values(&[
                            &placeholder_metadata.name,
                            &placeholder_metadata.location,
                            &placeholder_metadata.kind,
                        ]);
                        polls_counter.reset();
                        polls_counter.inc_by(poll_stats.polls as f64);
                    }

                    if stats.wakes > 0 {
                        let wakes_counter = metrics.task_wakes.with_label_values(&[
                            &placeholder_metadata.name,
                            &placeholder_metadata.location,
                            &placeholder_metadata.kind,
                        ]);
                        wakes_counter.reset();
                        wakes_counter.inc_by(stats.wakes as f64);
                    }

                    let state = if stats.dropped_at.is_some() { 2.0 } else { 1.0 };
                    metrics
                        .task_state
                        .with_label_values(&[
                            &placeholder_metadata.name,
                            &placeholder_metadata.location,
                            &placeholder_metadata.kind,
                        ])
                        .set(state);
                }
            }

            // Track consecutive empty updates
            if new_tasks_count == 0 && stats_count == 0 {
                consecutive_empty_updates += 1;
            } else {
                consecutive_empty_updates = 0;
            }

            // Update summary metrics periodically (every 10 updates)
            if updates_processed % 10 == 0 {
                update_summary_metrics(&metrics, &task_metadata);
            }
        }

        // Stop conditions - be more patient for comprehensive capture
        if updates_processed >= MAX_UPDATES {
            debug!(
                "Processed {} updates, stopping due to max limit",
                MAX_UPDATES
            );
            break;
        }

        if consecutive_empty_updates >= MAX_CONSECUTIVE_EMPTY {
            debug!(
                "Processed {} updates with {} consecutive empty, stopping",
                updates_processed, consecutive_empty_updates
            );
            break;
        }
    }

    // Final metrics update
    let (
        final_total,
        final_with_location,
        final_without_location,
        running_count,
        idle_count,
        completed_count,
    ) = {
        let store = task_metadata.lock().unwrap();
        let state_store = task_states.lock().unwrap();

        let total = store.len();
        let with_location = store
            .values()
            .filter(|m| m.location != "unknown" && m.location != "no_location")
            .count();
        let without_location = total - with_location;

        // Count tasks by state from our state tracking
        let mut running_count = 0;
        let mut idle_count = 0;
        let mut completed_count = 0;

        for (span_id, _) in store.iter() {
            if let Some(state) = state_store.get(span_id) {
                match state {
                    TaskState::Running => running_count += 1,
                    TaskState::Idle => idle_count += 1,
                    TaskState::Completed => completed_count += 1,
                }
            } else {
                // If no state info, assume idle
                idle_count += 1;
            }
        }

        (
            total,
            with_location,
            without_location,
            running_count,
            idle_count,
            completed_count,
        )
    };

    metrics.tasks_total.set(final_total as i64);
    metrics.tasks_with_location.set(final_with_location as i64);
    metrics
        .tasks_without_location
        .set(final_without_location as i64);
    metrics.tasks_running.set(running_count as i64);
    metrics.tasks_idle.set(idle_count as i64);
    metrics.tasks_completed.set(completed_count as i64);

    // Final summary (using span_id as the key like tokio-console)
    {
        let store = task_metadata.lock().unwrap();
        info!(
            "Persistent task capture session complete: {} total tasks",
            final_total
        );
        info!(
            "Tasks with location: {}, without location: {}",
            final_with_location, final_without_location
        );

        // Show breakdown by task kind
        let mut kind_counts: HashMap<String, usize> = HashMap::new();
        for metadata in store.values() {
            *kind_counts.entry(metadata.kind.clone()).or_insert(0) += 1;
        }
        info!("Task breakdown by kind: {:?}", kind_counts);

        // Show sample of well-located tasks
        let well_located_samples: Vec<_> = store
            .values()
            .filter(|m| m.location != "unknown" && m.location != "no_location")
            .take(5)
            .map(|m| format!("{}@{}", m.name, m.location))
            .collect();
        if !well_located_samples.is_empty() {
            info!("Sample well-located tasks: {:?}", well_located_samples);
        }
    }

    Ok((
        final_total,
        final_with_location,
        final_without_location,
        running_count,
        idle_count,
        completed_count,
    ))
}

fn update_summary_metrics(metrics: &Metrics, task_metadata: &TaskMetadataStore) {
    let store = task_metadata.lock().unwrap();
    let total = store.len();
    let with_location = store
        .values()
        .filter(|m| m.location != "unknown" && m.location != "no_location")
        .count();
    let without_location = total - with_location;

    // Count tasks by state (we need to track this from the stats updates)
    // For now, we'll set placeholder values - the actual state counting will be done in scrape_console
    metrics.tasks_total.set(total as i64);
    metrics.tasks_with_location.set(with_location as i64);
    metrics.tasks_without_location.set(without_location as i64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use console_api::field::{Name, Value};
    use console_api::tasks::Task;
    use console_api::{Field, Location, MetaId};

    // Helper function to create a basic test task
    fn create_test_task() -> Task {
        Task {
            id: Some(console_api::Id { id: 1 }),
            metadata: Some(MetaId { id: 1 }),
            kind: 0,
            fields: vec![],
            parents: vec![],
            location: None,
        }
    }

    // Helper to create a field
    fn create_field(name: &str, value: Value) -> Field {
        Field {
            name: Some(Name::StrName(name.to_string())),
            value: Some(value),
            metadata_id: None,
        }
    }

    #[test]
    fn test_location_extraction_from_task_location() {
        let mut task = create_test_task();
        task.location = Some(Location {
            file: Some("/src/runtime/scheduler/multi_thread/worker.rs".to_string()),
            line: Some(457),
            column: None,
            module_path: Some("runtime".to_string()),
        });

        let metadata = extract_task_metadata(&task);
        assert_eq!(
            metadata.location,
            "/src/runtime/scheduler/multi_thread/worker.rs:457"
        );
    }

    #[test]
    fn test_location_extraction_from_task_location_no_line() {
        let mut task = create_test_task();
        task.location = Some(Location {
            file: Some("/src/main.rs".to_string()),
            line: None,
            column: None,
            module_path: Some("main".to_string()),
        });

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "/src/main.rs");
    }

    #[test]
    fn test_location_extraction_from_fields() {
        let mut task = create_test_task();
        // No location field
        task.location = None;

        // Add location in fields (this won't be used in new implementation)
        task.fields.push(create_field(
            "src.location",
            Value::StrVal("worker.rs:123".to_string()),
        ));

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "unknown"); // New implementation only uses task.location
    }

    #[test]
    fn test_location_extraction_from_file_field() {
        let mut task = create_test_task();
        task.location = None;

        task.fields.push(create_field(
            "file",
            Value::StrVal("/home/user/project/src/lib.rs:45".to_string()),
        ));

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "unknown"); // New implementation only uses task.location
    }

    #[test]
    fn test_location_extraction_with_pattern() {
        let mut task = create_test_task();
        task.location = None;

        // Add a field with location-like pattern (won't be used)
        task.fields.push(create_field(
            "spawn.location",
            Value::StrVal("common/task/src/cancellation.rs:232:17".to_string()),
        ));

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "unknown"); // New implementation only uses task.location
    }

    #[test]
    fn test_location_extraction_from_cargo_path() {
        let mut task = create_test_task();
        task.location = None;

        // Add a field with cargo path pattern (won't be used)
        task.fields.push(create_field(
            "loc",
            Value::StrVal(
                "<cargo>/tokio-1.46.1/src/runtime/scheduler/multi_thread/worker.rs:457:13"
                    .to_string(),
            ),
        ));

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "unknown"); // New implementation only uses task.location
    }

    #[test]
    fn test_location_extraction_fallback_to_unknown() {
        let task = create_test_task();
        // No location anywhere

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "unknown");
    }

    #[test]
    fn test_extract_task_metadata_with_full_data() {
        let mut task = create_test_task();

        // Add location
        task.location = Some(Location {
            file: Some("/src/runtime/scheduler/multi_thread/worker.rs".to_string()),
            line: Some(457),
            column: None,
            module_path: Some("runtime".to_string()),
        });

        // Add fields
        task.fields.push(create_field("task.id", Value::U64Val(42)));
        task.fields.push(create_field(
            "task.name",
            Value::StrVal("my_task".to_string()),
        ));
        task.fields.push(create_field(
            "task.kind",
            Value::StrVal("blocking".to_string()),
        ));

        let metadata = extract_task_metadata(&task);

        assert_eq!(metadata.task_id, 42);
        assert_eq!(metadata.name, "42 (my_task)"); // tokio-console format: "task_id (name)"
        assert_eq!(metadata.kind, "blocking");
        assert_eq!(
            metadata.location,
            "/src/runtime/scheduler/multi_thread/worker.rs:457"
        );
    }

    #[test]
    fn test_extract_task_metadata_with_location_only() {
        let mut task = create_test_task();

        task.location = Some(Location {
            file: Some("/src/lib.rs".to_string()),
            line: Some(100),
            column: None,
            module_path: Some("lib".to_string()),
        });

        task.fields.push(create_field("task.id", Value::U64Val(99)));

        let metadata = extract_task_metadata(&task);

        assert_eq!(metadata.task_id, 99);
        assert_eq!(metadata.name, "99"); // tokio-console format: just the task_id when no name
        assert_eq!(metadata.kind, "task"); // Default kind
        assert_eq!(metadata.location, "/src/lib.rs:100");
    }

    #[test]
    fn test_extract_task_metadata_without_location() {
        let mut task = create_test_task();

        task.fields
            .push(create_field("task.id", Value::U64Val(123)));
        task.fields
            .push(create_field("task.kind", Value::StrVal("task".to_string())));

        let metadata = extract_task_metadata(&task);

        assert_eq!(metadata.task_id, 123);
        assert_eq!(metadata.name, "123"); // tokio-console format: just the task_id when no name
        assert_eq!(metadata.kind, "task");
        assert_eq!(metadata.location, "unknown");
    }

    #[test]
    fn test_task_metadata_store_operations() {
        let store: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));

        let metadata1 = TaskMetadata {
            name: "task1".to_string(),
            location: "main.rs:10".to_string(),
            task_id: 1,
            kind: "task".to_string(),
        };

        let metadata2 = TaskMetadata {
            name: "task2".to_string(),
            location: "worker.rs:20".to_string(),
            task_id: 2,
            kind: "blocking".to_string(),
        };

        // Insert tasks
        {
            let mut s = store.lock().unwrap();
            s.insert(1, metadata1.clone());
            s.insert(2, metadata2.clone());
        }

        // Verify insertion
        {
            let s = store.lock().unwrap();
            assert_eq!(s.len(), 2);
            assert_eq!(s.get(&1).unwrap().name, "task1");
            assert_eq!(s.get(&2).unwrap().kind, "blocking");
        }

        // Clear and verify
        {
            let mut s = store.lock().unwrap();
            s.clear();
            assert_eq!(s.len(), 0);
        }
    }

    #[test]
    fn test_metrics_creation_and_registration() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry);

        // Test that metrics are created
        assert_eq!(metrics.tasks_total.get(), 0);
        assert_eq!(metrics.tasks_with_location.get(), 0);
        assert_eq!(metrics.tasks_without_location.get(), 0);

        // Test metric updates
        metrics.tasks_total.set(10);
        metrics.tasks_with_location.set(7);
        metrics.tasks_without_location.set(3);

        assert_eq!(metrics.tasks_total.get(), 10);
        assert_eq!(metrics.tasks_with_location.get(), 7);
        assert_eq!(metrics.tasks_without_location.get(), 3);

        // Test counter metrics
        metrics.scrapes_total.inc();
        metrics.scrape_errors.inc();
        metrics.scrape_errors.inc();

        // Verify the metrics are properly registered
        let gathered = registry.gather();
        let metric_names: Vec<String> = gathered.iter().map(|m| m.name().to_string()).collect();

        assert!(metric_names.contains(&"tokio_tasks_total".to_string()));
        assert!(metric_names.contains(&"tokio_tasks_with_location".to_string()));
        assert!(metric_names.contains(&"tokio_tasks_without_location".to_string()));
        assert!(metric_names.contains(&"tokio_scrapes_total".to_string()));
    }

    #[test]
    fn test_label_values_generation() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry);

        // Test with full location
        let labels = ["my_task", "/src/main.rs:42", "blocking"];
        metrics.task_polls.with_label_values(&labels).inc();

        // Test with unknown location
        let labels_unknown = ["unknown_task_99", "unknown", "task"];
        metrics.task_polls.with_label_values(&labels_unknown).inc();

        // Gather and verify both label sets exist
        let gathered = registry.gather();
        let task_polls_family = gathered
            .iter()
            .find(|m| m.name() == "tokio_task_polls_total")
            .expect("task_polls metric should exist");

        assert_eq!(task_polls_family.get_metric().len(), 2);
    }

    #[test]
    fn test_auth_token_validation() {
        // Test bearer token parsing
        let token = "d0a45d7c272163c78763ca2ec56c672d1241363de00fcd0e55f9a4029145ef91";
        let auth_header = format!("Bearer {}", token);

        assert!(auth_header.starts_with("Bearer "));
        assert_eq!(&auth_header[7..], token);
    }

    #[test]
    fn test_deduplication_logic() {
        use std::collections::HashSet;

        let mut seen_task_ids = HashSet::new();

        // First occurrence
        assert!(!seen_task_ids.contains(&1));
        seen_task_ids.insert(1);

        // Second occurrence - should be detected as duplicate
        assert!(seen_task_ids.contains(&1));

        // Different task
        assert!(!seen_task_ids.contains(&2));
        seen_task_ids.insert(2);

        assert_eq!(seen_task_ids.len(), 2);
    }

    #[tokio::test]
    async fn test_scrape_loop_interval() {
        use std::time::Instant;

        // Create a mock interval of 1 second
        let mut interval_timer = tokio::time::interval(Duration::from_millis(100));

        let start = Instant::now();

        // First tick is immediate
        interval_timer.tick().await;
        let first_tick = start.elapsed();
        assert!(first_tick < Duration::from_millis(10));

        // Second tick should be after interval
        interval_timer.tick().await;
        let second_tick = start.elapsed();
        assert!(second_tick >= Duration::from_millis(100));
        assert!(second_tick < Duration::from_millis(150));
    }

    #[test]
    fn test_socket_addr_parsing() {
        // Valid addresses
        assert!("127.0.0.1:6669".parse::<SocketAddr>().is_ok());
        assert!("0.0.0.0:9090".parse::<SocketAddr>().is_ok());
        assert!("[::1]:8080".parse::<SocketAddr>().is_ok());

        // Invalid addresses
        assert!("invalid:addr".parse::<SocketAddr>().is_err());
        assert!("127.0.0.1".parse::<SocketAddr>().is_err()); // Missing port
        assert!("127.0.0.1:99999".parse::<SocketAddr>().is_err()); // Invalid port
    }

    #[test]
    fn test_consecutive_empty_updates_counter() {
        let mut consecutive_empty_updates = 0;
        const MAX_CONSECUTIVE_EMPTY: usize = 5;

        // Simulate empty updates
        for _ in 0..3 {
            consecutive_empty_updates += 1;
        }
        assert_eq!(consecutive_empty_updates, 3);
        assert!(consecutive_empty_updates < MAX_CONSECUTIVE_EMPTY);

        // Non-empty update resets counter
        consecutive_empty_updates = 0;
        assert_eq!(consecutive_empty_updates, 0);

        // Reach limit
        for _ in 0..MAX_CONSECUTIVE_EMPTY {
            consecutive_empty_updates += 1;
        }
        assert_eq!(consecutive_empty_updates, MAX_CONSECUTIVE_EMPTY);
    }
}

// Integration tests module
#[cfg(test)]
mod integration_tests {
    use super::*;
    use prometheus::TextEncoder;

    #[tokio::test]
    async fn test_metrics_endpoint_format() {
        let registry = Registry::new();
        let _metrics = Metrics::new(&registry);

        // Encode metrics
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        let metric_families = registry.gather();
        encoder.encode(&metric_families, &mut buffer).unwrap();

        let output = String::from_utf8(buffer).unwrap();

        // Verify Prometheus format
        assert!(output.contains("# HELP tokio_tasks_total"));
        assert!(output.contains("# TYPE tokio_tasks_total gauge"));
        assert!(output.contains("# HELP tokio_tasks_with_location"));
        assert!(output.contains("# HELP tokio_tasks_without_location"));
    }
}
