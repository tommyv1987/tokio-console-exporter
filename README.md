# Tokio Console Prometheus Exporter

Prometheus exporter for Tokio Console metrics, written in Rust.

*Inspired by [Tokio Console](https://github.com/tokio-rs/console) and [grafana-tokio-console-datasource](https://github.com/sd2k/grafana-tokio-console-datasource)*

## Quick Start

```
# Build and run
cd tokio-console-prometheus-exporter
cargo run -- --no-auth

# With authentication
TOKIO_EXPORTER_TOKEN=your-token cargo run

# Setup monitoring
cd monitoring
./setup.sh
```

## Features

- **Real-time Metrics**: Live task data from Tokio Console
- **Authentication**: Bearer token support
- **Rich Labels**: Task names, locations, and types
- **Monitoring Stack**: Complete Prometheus + Grafana setup

## Configuration

### Command Line Options
```
--console-addr 127.0.0.1:6669  # Tokio Console server
--listen-addr 0.0.0.0:9090     # Exporter listen address
--interval 30                   # Scrape interval (seconds)
--auth-token your-token         # Authentication token
```

### Environment Variables
- `TOKIO_EXPORTER_TOKEN`: Authentication token

## Key Metrics

- `tokio_tasks_total`: Total task count
- `tokio_task_polls_total`: Task poll counts (with labels)
- `tokio_task_wakes_total`: Task wake counts
- `tokio_task_state`: Task states (idle/running/completed)

## Monitoring Setup

The `monitoring/` directory provides a complete stack:

```
cd monitoring
./setup.sh  # One-command setup
```

Access:
- **Prometheus**: http://localhost:9090
- **Grafana**: http://localhost:3000 (admin/admin)

## Deployment

### Systemd Service
```
sudo cp tokio-console-exporter.service /etc/systemd/system/
sudo systemctl enable tokio-console-exporter
sudo systemctl start tokio-console-exporter
```

## Troubleshooting

- **Connection issues**: Check Tokio Console server is running
- **Auth errors**: Verify Bearer token format
- **No metrics**: Ensure Tokio Console has active tasks

## TODO

- [ ] **CPU Optimization**: Improve performance with connection pooling
- [ ] **Additional Metrics**: Add task wake patterns and waker counts
- [ ] **Health Checks**: Built-in health check endpoints
- [ ] **Docker Optimization**: Multi-stage builds and smaller images
- [ ] **Example Apps**: Sample Tokio applications with monitoring

## License

MIT License
