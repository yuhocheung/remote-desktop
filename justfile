# Remote desktop control system - dev tasks
# Single Cargo workspace now spans shared/ + core/ + cloud/ (see root Cargo.toml).
cargo := "cargo"

# Build the desktop client (Rust core)
build-client:
    {{cargo}} build --release -p rdcore-desktop

# Build the cloud control plane
build-cloud:
    {{cargo}} build --release -p gateway -p auth -p registry -p signaling-svc -p permission -p audit

# Run all Rust tests (single workspace)
test:
    {{cargo}} test --workspace

# Lint + format check (local equivalent of CI)
ci:
    {{cargo}} fmt --check
    {{cargo}} clippy --workspace --all-targets -- -D warnings
    just check-config

# On Windows, plain `python3` in PATH is often the Microsoft Store shim, which
# exits with code 49 when no real Python is installed through it. The `py`
# launcher resolves the actually-installed interpreter instead.
py := if os() == "windows" { "py -3" } else { "python3" }

# Sync Flutter ICE/signaling defaults from the single source of truth
# (core/crates/rdcore-desktop/src/config.rs) into flutter/lib/models/default_config.dart.
sync-config:
    {{py}} tool/sync_flutter_config.py

# CI guard: regenerate defaults from config.rs and fail if the committed
# flutter/lib/models/default_config.dart is stale or hand-edited out of sync.
check-config:
    {{py}} tool/sync_flutter_config.py
    git -C {{justfile_directory()}} diff --quiet -- flutter/lib/models/default_config.dart || { echo "ERROR: flutter/lib/models/default_config.dart is out of sync with config.rs (single source of truth). Run 'just sync-config' and commit the regenerated file."; exit 1; }

# Build the Flutter UI for a given target, e.g. `just build-flutter ios`.
# sync-config ensures the generated defaults match config.rs before building.
build-flutter target: sync-config
    cd flutter && flutter pub get && flutter build {{target}}

# Same as build-flutter, but routes pub/Flutter downloads through the
# flutter-io.cn mirror for networks where pub.dev is unreachable.
build-flutter-cn target: sync-config
    export PUB_HOSTED_URL="https://pub.flutter-io.cn" && \
    export FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn" && \
    cd flutter && flutter pub get && flutter build {{target}}

# Build the Rust FFI static library for iOS BEFORE `flutter run`/`flutter build ios`.
# flutter does NOT auto-build it; if flutter/ios/rdcore/librdcore_ffi.a is missing,
# the link step fails with "Library 'rdcore_ffi' not found".
build-ffi-ios:
    bash tool/build_ffi.sh ios

# Build the Rust FFI native library for desktop targets BEFORE `flutter build <target>`.
# Same rule as iOS: flutter does NOT auto-build it. e.g. `just build-ffi macos`.
build-ffi target:
    bash tool/build_ffi.sh {{target}}

# Build the Windows Host (受控端) NSIS installer:
#   scripts/installer/RdCore-Host-Setup.exe
# 前置：MSVC 构建工具 + NASM + NSIS(makensis)，详见 scripts/build_installer_nsis.sh
installer-win:
    bash scripts/build_installer_nsis.sh

# Build the macOS Host (受控端) .app bundle:
#   scripts/installer/RdCore Host.app
# 解决「弹了屏幕录制提示但设置里没有」：授权按 bundle 记，不挑启动终端。
package-mac:
    bash scripts/package_mac.sh

# 以 profile 模式在已连接 iOS 设备/模拟器上重新编译并运行 Flutter app
# （AOT、近 release 性能、保留热重载；规避 debug 模式 SkSL 着色器首次编译卡顿）。
# 前置：首次 / 改过 Rust 后先 `just build-ffi-ios` 生成 flutter/ios/rdcore/librdcore_ffi.a。
run-profile:
    bash scripts/flutter_run_profile.sh
