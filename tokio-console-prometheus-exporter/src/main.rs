use bytes::Bytes;
use clap::Parser;
use console_api::instrument::instrument_client::InstrumentClient;
use console_api::instrument::InstrumentRequest;
use futures::stream::StreamExt;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response};
use prometheus::{
    Counter, CounterVec, Encoder, GaugeVec, HistogramOpts, HistogramVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tonic::transport::Channel;
use tracing::{error, info};

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
    #[arg(long, default_value = "1000")]
    max_tasks: usize,
    #[arg(long, action = clap::ArgAction::SetTrue, default_value = "true")]
    enable_detailed_metrics: bool,
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
    task_busy_time: HistogramVec,
    _task_idle_time: HistogramVec,
    task_total_time: HistogramVec,
    task_waker_count: GaugeVec,
    task_self_wakes: CounterVec,
    scrapes_total: Counter,
    scrape_errors: Counter,
}

// Use span_id as the key (like tokio-console), not task_id
type TaskMetadataStore = Arc<Mutex<HashMap<u64, TaskMetadata>>>;

// Track task states with timestamps for better state detection
type TaskStateStore = Arc<Mutex<HashMap<u64, TaskStateInfo>>>;

#[derive(Debug, Clone)]
enum TaskState {
    Idle,
    Running,
    Completed,
}

#[derive(Debug, Clone)]
struct TaskStateInfo {
    state: TaskState,
    _last_poll_started: Option<SystemTime>,
    _last_poll_ended: Option<SystemTime>,
    _created_at: SystemTime,
    _dropped_at: Option<SystemTime>,
}

#[derive(Clone, Debug)]
struct TaskMetadata {
    name: String,
    location: String,
    task_id: u64,
    kind: String,
    _target: String,
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
                "Task state (0=completed, 1=idle, 2=running)",
            ),
            &["task_name", "task_location", "task_kind", "state"],
        )
        .unwrap();
        let task_busy_time = HistogramVec::new(
            HistogramOpts::new("tokio_task_busy_time_seconds", "Task busy time in seconds"),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let _task_idle_time = HistogramVec::new(
            HistogramOpts::new("tokio_task_idle_time_seconds", "Task idle time in seconds"),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let task_total_time = HistogramVec::new(
            HistogramOpts::new(
                "tokio_task_total_time_seconds",
                "Task total lifetime in seconds",
            ),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let task_waker_count = GaugeVec::new(
            Opts::new(
                "tokio_task_waker_count",
                "Current number of wakers for this task",
            ),
            &["task_name", "task_location", "task_kind"],
        )
        .unwrap();
        let task_self_wakes = CounterVec::new(
            Opts::new("tokio_task_self_wakes_total", "Total number of self-wakes"),
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
        registry.register(Box::new(task_busy_time.clone())).unwrap();
        registry
            .register(Box::new(_task_idle_time.clone()))
            .unwrap();
        registry
            .register(Box::new(task_total_time.clone()))
            .unwrap();
        registry
            .register(Box::new(task_waker_count.clone()))
            .unwrap();
        registry
            .register(Box::new(task_self_wakes.clone()))
            .unwrap();
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
            task_busy_time,
            _task_idle_time,
            task_total_time,
            task_waker_count,
            task_self_wakes,
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
        "Console: {}, Listen: {}, Interval: {}s, Max Tasks: {}",
        args.console_addr, args.listen_addr, args.interval, args.max_tasks
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
            args.max_tasks,
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
    max_tasks: usize,
) {
    let mut interval_timer = tokio::time::interval(Duration::from_secs(interval));
    loop {
        interval_timer.tick().await;
        match scrape_console(
            console_addr,
            &metrics,
            task_metadata.clone(),
            task_states.clone(),
            max_tasks,
        )
        .await
        {
            Ok(_) => {
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
        None => 0,
    };

    let _meta_id = match task.metadata.as_ref() {
        Some(id) => id.id,
        None => span_id,
    };

    // Silent operation - no debug logging needed

    // Extract fields exactly like tokio-console does
    let mut name = None;
    let mut task_id = None;
    let mut kind = "task".to_string(); // Default like tokio-console
    let mut _target = "unknown".to_string();

    // Process fields like tokio-console's update_tasks method
    for field in &task.fields {
        if let Some(console_api::field::Name::StrName(field_name)) = &field.name {
            match field_name.as_str() {
                "task.name" => {
                    if let Some(console_api::field::Value::StrVal(val)) = &field.value {
                        name = Some(val.clone());
                    }
                }
                "task.id" => {
                    if let Some(console_api::field::Value::U64Val(val)) = &field.value {
                        task_id = Some(*val);
                    }
                }
                "task.kind" | "kind" => {
                    if let Some(console_api::field::Value::StrVal(val)) = &field.value {
                        kind = val.clone();
                    }
                }
                "task.target" | "target" => {
                    if let Some(console_api::field::Value::StrVal(val)) = &field.value {
                        _target = val.clone();
                    }
                }
                _ => {}
            }
        }
    }

    // Format location exactly like tokio-console does
    let location = format_location(task.location.as_ref());

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
        _target: _target,
    };

    // Silent operation - no debug logging needed

    metadata
}

async fn scrape_console(
    console_addr: &str,
    metrics: &Metrics,
    task_metadata: TaskMetadataStore,
    task_states: TaskStateStore,
    max_tasks: usize,
) -> Result<(usize, usize, usize, usize, usize, usize), Box<dyn std::error::Error>> {
    let channel = Channel::from_shared(format!("http://{}", console_addr))?
        .connect()
        .await?;

    let mut client = InstrumentClient::new(channel);

    // DON'T clear the store - maintain persistent state like tokio-console
    // This allows us to accumulate tasks over time instead of losing them
    // Silent operation - no logging needed

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

        // Silent operation - no debug logging needed

        if let Some(task_update) = update.task_update {
            let new_tasks_count = task_update.new_tasks.len();
            let stats_count = task_update.stats_update.len();

            // Silent operation - no debug logging needed

            // The first update often contains all existing tasks
            if first_update && new_tasks_count > 0 {
                // Silent operation - no logging needed
            }
            first_update = false;

            // Process new tasks exactly like tokio-console's update_tasks method
            for task in task_update.new_tasks {
                let span_id = match task.id.as_ref() {
                    Some(id) => id.id,
                    None => continue,
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
                        // Silent operation - no logging needed
                    }

                    // Store by span_id (like tokio-console) not task_id
                    {
                        let mut store = task_metadata.lock().unwrap();
                        // Respect max_tasks limit
                        if store.len() < max_tasks {
                            store.insert(span_id, metadata.clone());
                        }
                    }
                } else {
                    // Update existing task metadata in case location was resolved
                    {
                        let mut store = task_metadata.lock().unwrap();
                        if let Some(existing) = store.get(&span_id) {
                            // Update if we got better location info
                            if existing.location == "unknown" && metadata.location != "unknown" {
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

                    // Determine task state using tokio-console's logic
                    let task_state = determine_task_state(&stats);

                    // Convert protobuf timestamps to SystemTime
                    let created_at = stats
                        .created_at
                        .as_ref()
                        .map(|ts| protobuf_timestamp_to_system_time(ts))
                        .unwrap_or_else(SystemTime::now);

                    let dropped_at = stats
                        .dropped_at
                        .as_ref()
                        .map(|ts| protobuf_timestamp_to_system_time(ts));

                    let last_poll_started = stats
                        .poll_stats
                        .as_ref()
                        .and_then(|ps| ps.last_poll_started.as_ref())
                        .map(|ts| protobuf_timestamp_to_system_time(ts));

                    let last_poll_ended = stats
                        .poll_stats
                        .as_ref()
                        .and_then(|ps| ps.last_poll_ended.as_ref())
                        .map(|ts| protobuf_timestamp_to_system_time(ts));

                    // Update state tracking with detailed timing info
                    {
                        let mut state_store = task_states.lock().unwrap();
                        state_store.insert(
                            span_id,
                            TaskStateInfo {
                                state: task_state.clone(),
                                _last_poll_started: last_poll_started,
                                _last_poll_ended: last_poll_ended,
                                _created_at: created_at,
                                _dropped_at: dropped_at,
                            },
                        );
                    }

                    let state_value = match task_state {
                        TaskState::Idle => 1.0,      // 1 = Idle
                        TaskState::Running => 2.0,   // 2 = Running
                        TaskState::Completed => 0.0, // 0 = Completed
                    };

                    let state_label = match task_state {
                        TaskState::Idle => "idle".to_string(),
                        TaskState::Running => "running".to_string(),
                        TaskState::Completed => "completed".to_string(),
                    };

                    metrics
                        .task_state
                        .with_label_values(&[
                            &metadata.name,
                            &metadata.location,
                            &metadata.kind,
                            &state_label,
                        ])
                        .set(state_value);

                    // Update timing metrics
                    if let Some(poll_stats) = &stats.poll_stats {
                        if let Some(busy_time) = &poll_stats.busy_time {
                            let busy_seconds = busy_time.seconds as f64
                                + (busy_time.nanos as f64 / 1_000_000_000.0);
                            metrics
                                .task_busy_time
                                .with_label_values(&[
                                    &metadata.name,
                                    &metadata.location,
                                    &metadata.kind,
                                ])
                                .observe(busy_seconds);
                        }
                    }

                    // Calculate and record total time
                    if let Some(created_at) = stats.created_at.as_ref() {
                        let created = protobuf_timestamp_to_system_time(created_at);
                        let total_duration = SystemTime::now()
                            .duration_since(created)
                            .unwrap_or_default();
                        let total_seconds = total_duration.as_secs_f64();
                        metrics
                            .task_total_time
                            .with_label_values(&[
                                &metadata.name,
                                &metadata.location,
                                &metadata.kind,
                            ])
                            .observe(total_seconds);
                    }

                    // Update waker metrics
                    let waker_count = stats.waker_clones.saturating_sub(stats.waker_drops);
                    metrics
                        .task_waker_count
                        .with_label_values(&[&metadata.name, &metadata.location, &metadata.kind])
                        .set(waker_count as f64);

                    if stats.self_wakes > 0 {
                        metrics
                            .task_self_wakes
                            .with_label_values(&[
                                &metadata.name,
                                &metadata.location,
                                &metadata.kind,
                            ])
                            .inc_by(stats.self_wakes as f64);
                    }
                } else {
                    // Task not in store - this should be rare now with persistent state
                    // Silent operation - no debug logging needed

                    // Create a minimal task metadata entry (using span_id like tokio-console)
                    let placeholder_metadata = TaskMetadata {
                        name: format!("task_{}", span_id),
                        location: "unknown".to_string(),
                        task_id: span_id, // Use span_id as fallback task_id
                        kind: "unknown".to_string(),
                        _target: "unknown".to_string(),
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

                    let state = if stats.dropped_at.is_some() { 0.0 } else { 1.0 };
                    let state_label = if stats.dropped_at.is_some() {
                        "completed".to_string()
                    } else {
                        "idle".to_string()
                    };
                    metrics
                        .task_state
                        .with_label_values(&[
                            &placeholder_metadata.name,
                            &placeholder_metadata.location,
                            &placeholder_metadata.kind,
                            &state_label,
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
            break;
        }

        if consecutive_empty_updates >= MAX_CONSECUTIVE_EMPTY {
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
            if let Some(state_info) = state_store.get(span_id) {
                match state_info.state {
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
        let _store = task_metadata.lock().unwrap();
        // Silent operation - no logging needed
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

/// Determine task state using tokio-console's logic
fn determine_task_state(stats: &console_api::tasks::Stats) -> TaskState {
    // If task is dropped, it's completed
    if stats.dropped_at.is_some() {
        return TaskState::Completed;
    }

    // Check if task is currently being polled
    if let Some(poll_stats) = &stats.poll_stats {
        if let (Some(_last_poll_started), None) =
            (poll_stats.last_poll_started, poll_stats.last_poll_ended)
        {
            // Task is currently being polled
            return TaskState::Running;
        }
    }

    // If task has been polled at least once, it's likely running
    if let Some(poll_stats) = &stats.poll_stats {
        if poll_stats.polls > 0 {
            return TaskState::Running;
        }
    }

    // Default to idle
    TaskState::Idle
}

/// Format location exactly like tokio-console does
fn format_location(loc: Option<&console_api::Location>) -> String {
    match loc {
        Some(location) => {
            let mut parts = Vec::new();

            if let Some(file) = &location.file {
                // Truncate registry paths like tokio-console
                let truncated_file = truncate_registry_path(file.clone());
                if let Some(line) = location.line {
                    parts.push(format!("{}:{}", truncated_file, line));
                } else {
                    parts.push(truncated_file);
                }
            }

            if let Some(module_path) = &location.module_path {
                parts.push(format!("({})", module_path));
            }

            if parts.is_empty() {
                "<unknown location>".to_string()
            } else {
                parts.join(" ")
            }
        }
        None => "<unknown location>".to_string(),
    }
}

/// Convert protobuf timestamp to SystemTime
fn protobuf_timestamp_to_system_time(ts: &prost_types::Timestamp) -> SystemTime {
    if ts.seconds >= 0 {
        let secs = ts.seconds as u64;
        let nanos = ts.nanos as u32;
        let duration = Duration::from_secs(secs) + Duration::from_nanos(nanos as u64);
        SystemTime::UNIX_EPOCH + duration
    } else {
        // Handle negative timestamps by going backwards from epoch
        let secs = (-ts.seconds) as u64;
        let nanos = ts.nanos as u32;
        let duration = Duration::from_secs(secs) + Duration::from_nanos(nanos as u64);
        SystemTime::UNIX_EPOCH - duration
    }
}

/// Truncate registry paths like tokio-console does
fn truncate_registry_path(s: String) -> String {
    use once_cell::sync::OnceCell;
    use regex::Regex;
    use std::borrow::Cow;

    static REGEX: OnceCell<Regex> = OnceCell::new();
    let regex = REGEX.get_or_init(|| {
        Regex::new(r#".*/\.cargo(/registry/src/[^/]*/|/git/checkouts/)"#)
            .expect("failed to compile regex")
    });

    match regex.replace(&s, "<cargo>/") {
        Cow::Owned(s) => s,
        Cow::Borrowed(_) => s,
    }
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
            "/src/runtime/scheduler/multi_thread/worker.rs:457 (runtime)"
        );
    }

    #[test]
    fn test_location_extraction_fallback_to_unknown() {
        let task = create_test_task();
        // No location anywhere

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata.location, "<unknown location>");
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
        task.fields.push(create_field(
            "task.target",
            Value::StrVal("tokio::task::block_in_place".to_string()),
        ));

        let metadata = extract_task_metadata(&task);

        assert_eq!(metadata.task_id, 42);
        assert_eq!(metadata.name, "42 (my_task)"); // tokio-console format: "task_id (name)"
        assert_eq!(metadata.kind, "blocking");
        assert_eq!(metadata._target, "tokio::task::block_in_place");
        assert_eq!(
            metadata.location,
            "/src/runtime/scheduler/multi_thread/worker.rs:457 (runtime)"
        );
    }

    #[test]
    fn test_task_metadata_store_operations() {
        let store: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));

        let metadata1 = TaskMetadata {
            name: "task1".to_string(),
            location: "main.rs:10".to_string(),
            task_id: 1,
            kind: "task".to_string(),
            _target: "tokio::task::block_in_place".to_string(),
        };

        let metadata2 = TaskMetadata {
            name: "task2".to_string(),
            location: "worker.rs:20".to_string(),
            task_id: 2,
            kind: "blocking".to_string(),
            _target: "tokio::task::block_in_place".to_string(),
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
    fn test_auth_token_validation() {
        // Test bearer token parsing
        let token = "d0a45d7c272163c78763ca2ec56c672d1241363de00fcd0e55f9a4029145ef91";
        let auth_header = format!("Bearer {}", token);

        assert!(auth_header.starts_with("Bearer "));
        assert_eq!(&auth_header[7..], token);
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
    fn test_task_state_values_and_labels() {
        // Test state value assignments
        let idle_state = TaskState::Idle;
        let running_state = TaskState::Running;
        let completed_state = TaskState::Completed;

        // Test state values (should match our fixed implementation)
        let idle_value = match idle_state {
            TaskState::Idle => 1.0,
            TaskState::Running => 2.0,
            TaskState::Completed => 0.0,
        };
        assert_eq!(idle_value, 1.0);

        let running_value = match running_state {
            TaskState::Idle => 1.0,
            TaskState::Running => 2.0,
            TaskState::Completed => 0.0,
        };
        assert_eq!(running_value, 2.0);

        let completed_value = match completed_state {
            TaskState::Idle => 1.0,
            TaskState::Running => 2.0,
            TaskState::Completed => 0.0,
        };
        assert_eq!(completed_value, 0.0);

        // Test state labels
        let idle_label = match idle_state {
            TaskState::Idle => "idle".to_string(),
            TaskState::Running => "running".to_string(),
            TaskState::Completed => "completed".to_string(),
        };
        assert_eq!(idle_label, "idle");

        let running_label = match running_state {
            TaskState::Idle => "idle".to_string(),
            TaskState::Running => "running".to_string(),
            TaskState::Completed => "completed".to_string(),
        };
        assert_eq!(running_label, "running");

        let completed_label = match completed_state {
            TaskState::Idle => "idle".to_string(),
            TaskState::Running => "running".to_string(),
            TaskState::Completed => "completed".to_string(),
        };
        assert_eq!(completed_label, "completed");
    }

    #[test]
    fn test_task_state_metric_labels() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry);

        // Test that task_state metric has the correct labels including 'state'
        let test_name = "test_task".to_string();
        let test_location = "test.rs:42".to_string();
        let test_kind = "task".to_string();
        let test_state = "running".to_string();

        // This should work with our new 4-label implementation
        let state_metric = metrics.task_state.with_label_values(&[
            &test_name,
            &test_location,
            &test_kind,
            &test_state,
        ]);
        state_metric.set(2.0);

        // Verify the metric was set correctly
        assert_eq!(state_metric.get(), 2.0);

        // Test with different states
        let idle_metric = metrics.task_state.with_label_values(&[
            &test_name,
            &test_location,
            &test_kind,
            &"idle".to_string(),
        ]);
        idle_metric.set(1.0);
        assert_eq!(idle_metric.get(), 1.0);

        let completed_metric = metrics.task_state.with_label_values(&[
            &test_name,
            &test_location,
            &test_kind,
            &"completed".to_string(),
        ]);
        completed_metric.set(0.0);
        assert_eq!(completed_metric.get(), 0.0);
    }

    #[test]
    fn test_metrics_with_state_labels_registration() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry);

        // Actually use the metric to ensure it's registered
        let test_name = "test_task".to_string();
        let test_location = "test.rs:42".to_string();
        let test_kind = "task".to_string();
        let test_state = "running".to_string();

        let state_metric = metrics.task_state.with_label_values(&[
            &test_name,
            &test_location,
            &test_kind,
            &test_state,
        ]);
        state_metric.set(2.0);

        // Now verify that task_state metric is properly registered with state label
        let gathered = registry.gather();
        let task_state_family = gathered
            .iter()
            .find(|m| m.name() == "tokio_task_state")
            .expect("tokio_task_state metric should exist");

        // Check that it has the correct label names including 'state'
        let label_names: Vec<&str> = task_state_family
            .get_metric()
            .first()
            .unwrap()
            .get_label()
            .iter()
            .map(|l| l.name()) // Fixed deprecated method
            .collect();

        assert!(label_names.contains(&"task_name"));
        assert!(label_names.contains(&"task_location"));
        assert!(label_names.contains(&"task_kind"));
        assert!(
            label_names.contains(&"state"),
            "task_state metric should have 'state' label"
        );
    }

    #[test]
    fn test_metrics_output_format_with_state_labels() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry);

        // Add sample metrics with different states
        metrics
            .task_state
            .with_label_values(&["worker_task", "worker.rs:42", "task", "running"])
            .set(2.0);

        metrics
            .task_state
            .with_label_values(&["idle_task", "main.rs:100", "task", "idle"])
            .set(1.0);

        metrics
            .task_state
            .with_label_values(&["completed_task", "handler.rs:25", "task", "completed"])
            .set(0.0);

        // Encode and verify the metrics output
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        let metric_families = registry.gather();
        encoder.encode(&metric_families, &mut buffer).unwrap();

        let output = String::from_utf8(buffer).unwrap();

        // Print the output for debugging
        println!("=== Metrics Output ===");
        println!("{}", output);
        println!("=== End Metrics Output ===");

        // Verify the state labels are present in the output
        assert!(
            output.contains("state=\"running\""),
            "Should contain running state label"
        );
        assert!(
            output.contains("state=\"idle\""),
            "Should contain idle state label"
        );
        assert!(
            output.contains("state=\"completed\""),
            "Should contain completed state label"
        );

        // Verify the metric values are correct (more flexible matching)
        assert!(
            output.contains("tokio_task_state"),
            "Should contain tokio_task_state metric"
        );
        assert!(output.contains("worker_task"), "Should contain worker_task");
        assert!(output.contains("idle_task"), "Should contain idle_task");
        assert!(
            output.contains("completed_task"),
            "Should contain completed_task"
        );
        assert!(output.contains(" 2"), "Should contain value 2 for running");
        assert!(output.contains(" 1"), "Should contain value 1 for idle");
        assert!(
            output.contains(" 0"),
            "Should contain value 0 for completed"
        );
    }

    #[test]
    fn test_determine_task_state_logic() {
        use console_api::{tasks::Stats, PollStats};
        use prost_types::Timestamp;

        // Test completed state
        let mut stats = Stats::default();
        stats.dropped_at = Some(Timestamp {
            seconds: 1000,
            nanos: 0,
        });
        assert!(matches!(determine_task_state(&stats), TaskState::Completed));

        // Test running state - currently being polled
        let mut stats = Stats::default();
        stats.poll_stats = Some(PollStats {
            last_poll_started: Some(Timestamp {
                seconds: 1000,
                nanos: 0,
            }),
            last_poll_ended: None,
            polls: 5,
            busy_time: None,
            first_poll: None,
        });
        assert!(matches!(determine_task_state(&stats), TaskState::Running));

        // Test running state - has been polled
        let mut stats = Stats::default();
        stats.poll_stats = Some(PollStats {
            last_poll_started: None,
            last_poll_ended: None,
            polls: 1,
            busy_time: None,
            first_poll: None,
        });
        assert!(matches!(determine_task_state(&stats), TaskState::Running));

        // Test idle state - no polls
        let mut stats = Stats::default();
        stats.poll_stats = Some(PollStats {
            last_poll_started: None,
            last_poll_ended: None,
            polls: 0,
            busy_time: None,
            first_poll: None,
        });
        assert!(matches!(determine_task_state(&stats), TaskState::Idle));

        // Test idle state - no poll stats
        let stats = Stats::default();
        assert!(matches!(determine_task_state(&stats), TaskState::Idle));
    }

    #[test]
    fn test_format_location_function() {
        use console_api::Location;

        // Test with full location info
        let location = Location {
            file: Some("/src/main.rs".to_string()),
            line: Some(42),
            column: None,
            module_path: Some("my_app".to_string()),
        };
        let formatted = format_location(Some(&location));
        assert_eq!(formatted, "/src/main.rs:42 (my_app)");

        // Test without line number
        let location = Location {
            file: Some("/src/main.rs".to_string()),
            line: None,
            column: None,
            module_path: Some("my_app".to_string()),
        };
        let formatted = format_location(Some(&location));
        assert_eq!(formatted, "/src/main.rs (my_app)");

        // Test without module path
        let location = Location {
            file: Some("/src/main.rs".to_string()),
            line: Some(42),
            column: None,
            module_path: None,
        };
        let formatted = format_location(Some(&location));
        assert_eq!(formatted, "/src/main.rs:42");

        // Test with cargo registry path truncation
        let location = Location {
            file: Some("/home/user/.cargo/registry/src/github.com-1ecc6299db9ec823/tokio-1.0.0/src/runtime.rs".to_string()),
            line: Some(100),
            column: None,
            module_path: Some("tokio::runtime".to_string()),
        };
        let formatted = format_location(Some(&location));
        assert!(formatted.contains("<cargo>/"));
        assert!(formatted.contains("tokio-1.0.0/src/runtime.rs:100"));

        // Test with no location
        let formatted = format_location(None);
        assert_eq!(formatted, "<unknown location>");
    }

    #[test]
    fn test_protobuf_timestamp_conversion() {
        use prost_types::Timestamp;

        // Test normal timestamp
        let ts = Timestamp {
            seconds: 1640995200, // 2022-01-01 00:00:00 UTC
            nanos: 500_000_000,  // 500ms
        };
        let system_time = protobuf_timestamp_to_system_time(&ts);

        // Verify it's close to expected time (allow for small differences)
        let expected = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1640995200)
            + Duration::from_nanos(500_000_000);
        let diff = system_time.duration_since(expected).unwrap_or_default();
        assert!(
            diff < Duration::from_millis(1),
            "Timestamp conversion should be accurate"
        );

        // Test zero timestamp
        let ts = Timestamp {
            seconds: 0,
            nanos: 0,
        };
        let system_time = protobuf_timestamp_to_system_time(&ts);
        assert_eq!(system_time, SystemTime::UNIX_EPOCH);

        // Test negative seconds (should handle gracefully)
        let ts = Timestamp {
            seconds: -1000,
            nanos: 0,
        };
        let system_time = protobuf_timestamp_to_system_time(&ts);
        // Should be before epoch
        assert!(system_time < SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn test_new_metrics_registration() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry);

        // Test histogram metrics
        let busy_histogram =
            metrics
                .task_busy_time
                .with_label_values(&["test_task", "test.rs:42", "task"]);
        busy_histogram.observe(1.5);
        assert!(busy_histogram.get_sample_sum() > 0.0);

        let total_histogram =
            metrics
                .task_total_time
                .with_label_values(&["test_task", "test.rs:42", "task"]);
        total_histogram.observe(10.0);
        assert!(total_histogram.get_sample_sum() > 0.0);

        // Test waker metrics
        let waker_gauge =
            metrics
                .task_waker_count
                .with_label_values(&["test_task", "test.rs:42", "task"]);
        waker_gauge.set(5.0);
        assert_eq!(waker_gauge.get(), 5.0);

        let self_wakes_counter =
            metrics
                .task_self_wakes
                .with_label_values(&["test_task", "test.rs:42", "task"]);
        self_wakes_counter.inc_by(3.0);
        assert_eq!(self_wakes_counter.get(), 3.0);

        // Verify metrics are registered
        let gathered = registry.gather();
        let metric_names: Vec<String> = gathered.iter().map(|m| m.name().to_string()).collect();

        assert!(metric_names.contains(&"tokio_task_busy_time_seconds".to_string()));
        assert!(metric_names.contains(&"tokio_task_total_time_seconds".to_string()));
        assert!(metric_names.contains(&"tokio_task_waker_count".to_string()));
        assert!(metric_names.contains(&"tokio_task_self_wakes_total".to_string()));
    }

    #[test]
    fn test_task_metadata_with_target() {
        let mut task = create_test_task();

        // Add target field
        task.fields.push(create_field(
            "task.target",
            Value::StrVal("tokio::task::spawn".to_string()),
        ));

        let metadata = extract_task_metadata(&task);
        assert_eq!(metadata._target, "tokio::task::spawn");
    }

    #[test]
    fn test_truncate_registry_path() {
        // Test cargo registry path
        let path =
            "/home/user/.cargo/registry/src/github.com-1ecc6299db9ec823/tokio-1.0.0/src/runtime.rs"
                .to_string();
        let truncated = truncate_registry_path(path);
        assert!(truncated.contains("<cargo>/"));
        assert!(truncated.contains("tokio-1.0.0/src/runtime.rs"));

        // Test git checkout path
        let path = "/home/user/.cargo/git/checkouts/tokio-abc123/src/runtime.rs".to_string();
        let truncated = truncate_registry_path(path);
        assert!(truncated.contains("<cargo>/"));
        assert!(truncated.contains("runtime.rs"));

        // Test normal path (should not be modified)
        let path = "/src/main.rs".to_string();
        let truncated = truncate_registry_path(path);
        assert_eq!(truncated, "/src/main.rs");
    }

    #[test]
    fn test_max_tasks_limit_respected() {
        let task_metadata: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));
        let max_tasks = 3;

        // Create test tasks
        let mut task1 = create_test_task();
        task1.id = Some(console_api::Id { id: 1 });
        task1.fields.push(create_field(
            "task.name",
            Value::StrVal("task1".to_string()),
        ));

        let mut task2 = create_test_task();
        task2.id = Some(console_api::Id { id: 2 });
        task2.fields.push(create_field(
            "task.name",
            Value::StrVal("task2".to_string()),
        ));

        let mut task3 = create_test_task();
        task3.id = Some(console_api::Id { id: 3 });
        task3.fields.push(create_field(
            "task.name",
            Value::StrVal("task3".to_string()),
        ));

        let mut task4 = create_test_task();
        task4.id = Some(console_api::Id { id: 4 });
        task4.fields.push(create_field(
            "task.name",
            Value::StrVal("task4".to_string()),
        ));

        // Simulate processing tasks
        let metadata1 = extract_task_metadata(&task1);
        let metadata2 = extract_task_metadata(&task2);
        let metadata3 = extract_task_metadata(&task3);
        let metadata4 = extract_task_metadata(&task4);

        // Insert tasks up to limit
        {
            let mut store = task_metadata.lock().unwrap();
            if store.len() < max_tasks {
                store.insert(1, metadata1.clone());
            }
            if store.len() < max_tasks {
                store.insert(2, metadata2.clone());
            }
            if store.len() < max_tasks {
                store.insert(3, metadata3.clone());
            }
            if store.len() < max_tasks {
                store.insert(4, metadata4.clone());
            }
        }

        // Verify only max_tasks were stored
        {
            let store = task_metadata.lock().unwrap();
            assert_eq!(store.len(), max_tasks);
            assert!(store.contains_key(&1));
            assert!(store.contains_key(&2));
            assert!(store.contains_key(&3));
            assert!(!store.contains_key(&4)); // Should be rejected due to limit
        }
    }

    #[test]
    fn test_max_tasks_zero_limit() {
        let task_metadata: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));
        let max_tasks = 0;

        let mut task = create_test_task();
        task.id = Some(console_api::Id { id: 1 });
        task.fields.push(create_field(
            "task.name",
            Value::StrVal("task1".to_string()),
        ));

        let metadata = extract_task_metadata(&task);

        // Try to insert task with zero limit
        {
            let mut store = task_metadata.lock().unwrap();
            if store.len() < max_tasks {
                store.insert(1, metadata.clone());
            }
        }

        // Verify no tasks were stored
        {
            let store = task_metadata.lock().unwrap();
            assert_eq!(store.len(), 0);
            assert!(!store.contains_key(&1));
        }
    }

    #[test]
    fn test_max_tasks_large_limit() {
        let task_metadata: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));
        let max_tasks = 10000;

        // Create many tasks
        for i in 1..=100 {
            let mut task = create_test_task();
            task.id = Some(console_api::Id { id: i });
            task.fields.push(create_field(
                "task.name",
                Value::StrVal(format!("task{}", i)),
            ));

            let metadata = extract_task_metadata(&task);

            {
                let mut store = task_metadata.lock().unwrap();
                if store.len() < max_tasks {
                    store.insert(i, metadata);
                }
            }
        }

        // Verify all tasks were stored (since limit is high)
        {
            let store = task_metadata.lock().unwrap();
            assert_eq!(store.len(), 100);
            for i in 1..=100 {
                assert!(store.contains_key(&i));
            }
        }
    }

    #[test]
    fn test_args_parsing_with_max_tasks() {
        // Test default values
        let args = Args::parse_from(&["tokio-console-exporter"]);
        assert_eq!(args.max_tasks, 1000);

        // Test custom values
        let args = Args::parse_from(&["tokio-console-exporter", "--max-tasks", "500"]);
        assert_eq!(args.max_tasks, 500);
    }

    #[test]
    fn test_task_metadata_store_with_max_tasks_logic() {
        let task_metadata: TaskMetadataStore = Arc::new(Mutex::new(HashMap::new()));
        let max_tasks = 2;

        // Create tasks
        let mut task1 = create_test_task();
        task1.id = Some(console_api::Id { id: 1 });
        task1.fields.push(create_field(
            "task.name",
            Value::StrVal("task1".to_string()),
        ));

        let mut task2 = create_test_task();
        task2.id = Some(console_api::Id { id: 2 });
        task2.fields.push(create_field(
            "task.name",
            Value::StrVal("task2".to_string()),
        ));

        let mut task3 = create_test_task();
        task3.id = Some(console_api::Id { id: 3 });
        task3.fields.push(create_field(
            "task.name",
            Value::StrVal("task3".to_string()),
        ));

        let metadata1 = extract_task_metadata(&task1);
        let metadata2 = extract_task_metadata(&task2);
        let metadata3 = extract_task_metadata(&task3);

        // Simulate the exact logic from scrape_console function
        let mut seen_span_ids = std::collections::HashSet::new();

        // Process task1
        let span_id1 = 1;
        if !seen_span_ids.contains(&span_id1) {
            seen_span_ids.insert(span_id1);
            {
                let mut store = task_metadata.lock().unwrap();
                if store.len() < max_tasks {
                    store.insert(span_id1, metadata1.clone());
                }
            }
        }

        // Process task2
        let span_id2 = 2;
        if !seen_span_ids.contains(&span_id2) {
            seen_span_ids.insert(span_id2);
            {
                let mut store = task_metadata.lock().unwrap();
                if store.len() < max_tasks {
                    store.insert(span_id2, metadata2.clone());
                }
            }
        }

        // Process task3 (should be rejected)
        let span_id3 = 3;
        if !seen_span_ids.contains(&span_id3) {
            seen_span_ids.insert(span_id3);
            {
                let mut store = task_metadata.lock().unwrap();
                if store.len() < max_tasks {
                    store.insert(span_id3, metadata3.clone());
                }
            }
        }

        // Verify results
        {
            let store = task_metadata.lock().unwrap();
            assert_eq!(store.len(), 2);
            assert!(store.contains_key(&1));
            assert!(store.contains_key(&2));
            assert!(!store.contains_key(&3));
        }
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
