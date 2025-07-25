#!/bin/bash

# Tokio Console Monitoring Setup
# Quick setup for Prometheus + Grafana monitoring stack

set -e

echo "Setting up Tokio Console monitoring..."

# Check Docker
if ! docker info > /dev/null 2>&1; then
    echo "Docker is not running. Please start Docker first."
    exit 1
fi

# Check if .env exists
if [ ! -f .env ]; then
    echo "Creating environment configuration..."
    cp env.template .env
    echo "Created .env from template"
    echo ""
    echo "Default configuration:"
    echo "   • Target: host.docker.internal:9090 (local exporter)"
    echo "   • Token: (empty = no auth)"
    echo ""
    echo "🔧 To customize:"
    echo "   • Edit .env with your exporter details"
    echo "   • For auth: set EXPORTER_TOKEN=your-token"
    echo "   • For remote server: change EXPORTER_TARGET"
    echo ""
    echo "Run './setup.sh' again to start monitoring"
    exit 0
fi

echo "Configuring Prometheus..."
source .env


if [ -z "$EXPORTER_TOKEN" ]; then
    echo "No auth token - running in no-auth mode"
    # Generate config without auth
    cat > config/prometheus.yml << EOF
global:
  scrape_interval: ${SCRAPE_INTERVAL:-15s}
  evaluation_interval: 15s

rule_files:

scrape_configs:
  # Prometheus itself
  - job_name: 'prometheus'
    static_configs:
      - targets: ['localhost:9090']

  # Tokio Console Exporter (no auth)
  - job_name: 'tokio-console-exporter'
    static_configs:
      - targets: ['${EXPORTER_TARGET}']
    metrics_path: /metrics
    honor_labels: true
    scrape_interval: ${SCRAPE_INTERVAL:-10s}
    scrape_timeout: 9s
EOF
else
    echo "Using auth token: ${EXPORTER_TOKEN:0:10}..."
    cat > config/prometheus.yml << EOF
global:
  scrape_interval: ${SCRAPE_INTERVAL:-15s}
  evaluation_interval: 15s

rule_files:

scrape_configs:
  # Prometheus itself
  - job_name: 'prometheus'
    static_configs:
      - targets: ['localhost:9090']

  # Tokio Console Exporter (with auth)
  - job_name: 'tokio-console-exporter'
    static_configs:
      - targets: ['${EXPORTER_TARGET}']
    authorization:
      type: Bearer
      credentials: '${EXPORTER_TOKEN}'
    metrics_path: /metrics
    honor_labels: true
    scrape_interval: ${SCRAPE_INTERVAL:-10s}
    scrape_timeout: 9s
EOF
fi

echo "Starting monitoring stack..."
docker-compose up -d

echo "Waiting for services to start..."
sleep 10

# Check services
if docker-compose ps | grep -q "Up"; then
    echo ""
    echo "Monitoring stack is running!"
    echo ""
    echo "Access your services:"
    echo "   Prometheus: http://localhost:9090"
    echo "   Grafana:    http://localhost:3000 (admin/admin)"
    echo ""
    echo "Next steps:"
    echo "   1. Start your Tokio Console Exporter:"
    echo "      cd ../tokio-console-prometheus-exporter"
    if [ -z "$EXPORTER_TOKEN" ]; then
        echo "      cargo run -- --no-auth"
    else
        echo "      TOKIO_EXPORTER_TOKEN=$EXPORTER_TOKEN cargo run"
    fi
    echo "   2. Open Grafana dashboard at http://localhost:3000"
    echo "   3. Dashboard should auto-load from grafana-dashboard.json"
    echo ""
    echo  To stop: docker-compose down"
else
    echo "Failed to start services"
    echo "Logs:"
    docker-compose logs
    exit 1
fi 