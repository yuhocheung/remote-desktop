#!/usr/bin/env bash
# Build the cloud control plane.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT/cloud"
cargo build --release
