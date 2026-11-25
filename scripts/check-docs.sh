#!/bin/sh
# Check README structure, documentation links, CLI names, config examples and schema claims.
set -eu

cd "$(dirname "$0")/.."

cargo test --locked -p lu --test docs
cargo test --locked -p lu-store --test docs_schema
