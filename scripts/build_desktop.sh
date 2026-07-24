#!/usr/bin/env bash
# Build the desktop client: Rust core + Flutter UI.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT/core"
cargo build --release
cd "$ROOT/flutter"
flutter pub get
# flutter build <windows|macos|linux>
