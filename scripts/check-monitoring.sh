#!/bin/sh
# Parse the Grafana dashboard and Prometheus rule file shipped under contrib/monitoring.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec cargo run --quiet --locked --manifest-path "$root/scripts/check-monitoring/Cargo.toml" -- "$root"
