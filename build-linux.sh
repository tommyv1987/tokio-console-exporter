#!/bin/bash

# Build Linux Binary Script
# This script builds a Linux binary for the Tokio Console Prometheus Exporter

set -e

echo "🚀 Building Linux binary for Tokio Console Prometheus Exporter..."

# Check if we're in the right directory
if [ ! -f "tokio-console-prometheus-exporter/Cargo.toml" ]; then
    echo "❌ Error: Please run this script from the project root directory"
    exit 1
fi

# Create release directory
echo "📁 Creating release directory..."
rm -rf release
mkdir -p release

# Build the binary
echo "🔨 Building release binary..."
cd tokio-console-prometheus-exporter
cargo build --release
cd ..

# Copy files to release directory
echo "📋 Copying files to release directory..."
cp tokio-console-prometheus-exporter/target/release/tokio-console-prometheus-exporter release/
cp tokio-console-exporter.service release/
cp -r monitoring release/
cp README.md release/

# Create archive
echo "📦 Creating archive..."
cd release
tar -czf tokio-console-prometheus-exporter-linux-x86_64.tar.gz *
cd ..

echo "✅ Build complete!"
echo "📁 Binary location: release/tokio-console-prometheus-exporter"
echo "📦 Archive location: release/tokio-console-prometheus-exporter-linux-x86_64.tar.gz"
echo ""
echo "🚀 To deploy:"
echo "   1. Extract the archive: tar -xzf release/tokio-console-prometheus-exporter-linux-x86_64.tar.gz"
echo "   2. Run the exporter: ./tokio-console-prometheus-exporter --no-auth"
echo "   3. Setup monitoring: cd monitoring && ./setup.sh" 