#!/bin/bash

# Function to restart the service on exit or interrupt
cleanup() {
    echo "Restarting t2fanrd service..."
    systemctl start t2fanrd
    echo "Done."
}

# Build the cargo binary
echo "Building cargo binary..."
cargo build

# Trap SIGINT (Ctrl-C) and EXIT to ensure cleanup runs
trap cleanup SIGINT EXIT

# Stop the t2fanrd service
echo "Stopping t2fanrd service..."
systemctl stop t2fanrd

# Run the cargo binary
echo "Running cargo binary (cargo run)..."
cargo run t2fanrd
