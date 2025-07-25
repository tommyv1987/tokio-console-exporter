# Tokio Console Monitoring

Local monitoring setup for Tokio tasks with Prometheus + Grafana.

## Quick Start

```bash
# Setup monitoring stack
cd monitoring
./setup.sh

# Start your exporter
cd ../tokio-console-prometheus-exporter
cargo run -- --no-auth

# Access dashboard
open http://localhost:3000  # admin/admin
```

## Configuration

### No Auth (Local Dev)
```bash
# .env file:
EXPORTER_TARGET=host.docker.internal:9090
EXPORTER_TOKEN=  # Empty = no auth
```

### With Auth
```bash
# .env file:
EXPORTER_TARGET=host.docker.internal:9090
EXPORTER_TOKEN=your-secure-token
```

### Remote Server
```bash
# .env file:
EXPORTER_TARGET=remote-server-ip:9090
EXPORTER_TOKEN=production-token
```

## Dashboard Features

- **Real-time Task Metrics**: Poll count, wake count, busy time
- **Source Location Tracking**: See where tasks are spawned
- **Task State Monitoring**: Running, idle, completed tasks
- **Performance Analysis**: Task efficiency and bottlenecks

## Stop

```bash
docker-compose down
```

## Troubleshooting

- **Connection issues**: Check `EXPORTER_TARGET` points to correct address
- **Auth errors**: Ensure token matches between .env and exporter
- **Docker issues**: Ensure Docker Desktop is running 