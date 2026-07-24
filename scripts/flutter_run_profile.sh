#!/usr/bin/env bash
# 以 profile 模式（AOT、近 release 性能、保留热重载）在已连接 iOS 设备/模拟器上
# 重新编译并运行 Flutter app。用于验证真机流畅度，规避 debug 模式的 SkSL 着色器
# 首次编译卡顿。
#
# 前置：
#   - 已连接 iOS 设备并信任；或先启动 iOS 模拟器。
#   - （首次 / 改过 Rust 后）先 `just build-ffi-ios` 生成
#     flutter/ios/rdcore/librdcore_ffi.a，否则 link 阶段报
#     "Library 'rdcore_ffi' not found"。纯 Dart/UI 改动无需重编 FFI。
#
# 用法：
#   bash scripts/flutter_run_profile.sh
#   bash scripts/flutter_run_profile.sh --device-id 00008110-...   # 多设备时指定
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1
cd flutter
exec flutter run --profile "$@"
