#!/bin/sh
# Check README structure, documentation links, CLI names and config examples.
set -eu

cd "$(dirname "$0")/.."

cargo test --locked -p lu --test docs
