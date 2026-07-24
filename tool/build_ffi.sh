#!/usr/bin/env bash
# 跨平台构建 rdcore-ffi 原生库，并布署到 Flutter 对应平台目录。
#
# 用法:
#   ./tool/build_ffi.sh windows   # -> flutter/rdcore_ffi.dll
#   ./tool/build_ffi.sh macos     # -> flutter/macos/rdcore/librdcore_ffi.dylib
#   ./tool/build_ffi.sh ios       # -> flutter/ios/rdcore/librdcore_ffi.a (device+sim 合并)
#   ./tool/build_ffi.sh linux     # -> flutter/linux/bundle/lib/librdcore_ffi.so
#   ./tool/build_ffi.sh android   # -> flutter/android/app/src/main/jniLibs/<abi>/librdcore_ffi.so
#
# 说明:
# - windows / macos / linux 产出 cdylib（动态库），由 Flutter 在运行期经 FFI 加载。
# - ios 产出 staticlib（.a）；因 cdylib 在 iOS 交叉编译需链接器/iOS SDK，故用
#   `cargo rustc --crate-type staticlib`，由 Xcode 在 link 阶段静态链入。
# - android 经 cargo-ndk 交叉编译（需 `cargo install cargo-ndk` 及 ANDROID_NDK_HOME），
#   直接按 jniLibs/<abi>/ 布局布署，覆盖 arm64-v8a / armeabi-v7a / x86_64。
# - 本脚本为 Track A（WorkBuddy）维护；Track B 的跨平台构建（B6）直接调用本脚本，
#   不自行实现构建逻辑，避免重复与漂移。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FLUTTER_DIR="$ROOT/flutter"
PROFILE="${PROFILE:-release}"
CARGO_PROFILE_FLAG="$([ "$PROFILE" = "release" ] && echo --release || echo --debug)"
TARGET_SUBDIR="$([ "$PROFILE" = "release" ] && echo release || echo debug)"

PLATFORM="${1:-}"
if [[ -z "$PLATFORM" ]]; then
  echo "用法: $0 <windows|macos|ios|linux|android>" >&2
  exit 2
fi

build_cdylib() {
  # $1 = target triple (可选，空=本机)
  # 注意：本仓库是单一 Cargo 工作区，cargo 产物落在工作区根 target/（而非 crate 子目录）。
  local tgt="${1:-}"
  if [[ -n "$tgt" ]]; then
    cargo build -p rdcore-ffi $CARGO_PROFILE_FLAG --target "$tgt"
    echo "$ROOT/target/$tgt/$TARGET_SUBDIR"
  else
    cargo build -p rdcore-ffi $CARGO_PROFILE_FLAG
    echo "$ROOT/target/$TARGET_SUBDIR"
  fi
}

case "$PLATFORM" in
  windows)
    OUT="$(build_cdylib)"
    DST_DIR="$FLUTTER_DIR/windows/rdcore"
    mkdir -p "$DST_DIR"
    cp "$OUT/rdcore_ffi.dll" "$DST_DIR/rdcore_ffi.dll"
    echo "✓ windows: $DST_DIR/rdcore_ffi.dll"
    # hwcodec 特性下 rdcore_ffi.dll 在运行期动态依赖 FFmpeg DLL
    # （avcodec-61 / avdevice-61 / avutil-59 等）。把它们一并 stage，
    # 否则 Dart 侧 DynamicLibrary.open 会因缺依赖而加载失败。
    FF_DIST="$ROOT/scripts/installer/dist"
    if [ -f "$FF_DIST/avcodec-61.dll" ]; then
      cp "$FF_DIST"/av*.dll "$FF_DIST"/sw*.dll "$DST_DIR/" 2>/dev/null
      echo "✓ windows: FFmpeg 依赖 DLL 已同步到 $DST_DIR"
    else
      echo "警告: 未在 $FF_DIST 找到 FFmpeg DLL（avcodec-61.dll 等）。"
      echo "      若 rdcore-ffi 启用了 hwcodec，运行时加载会失败；"
      echo "      请先运行 scripts/build_ffmpeg_lowfloor.ps1 或从安装器 dist 复制。"
    fi
    ;;
  macos)
    OUT="$(build_cdylib)"
    SRC="$OUT/librdcore_ffi.dylib"
    # 输出路径须与 flutter/macos/Runner.xcodeproj/project.pbxproj 一致：
    #   PBXGroup "rdcore" -> path = rdcore；PBXFileReference -> librdcore_ffi.dylib
    #   即 Xcode 在 Bundle Framework 阶段查找 flutter/macos/rdcore/librdcore_ffi.dylib
    DST="$FLUTTER_DIR/macos/rdcore/librdcore_ffi.dylib"
    mkdir -p "$(dirname "$DST")"
    cp "$SRC" "$DST"
    # 让 dylib 经 @rpath 在 .app 内被找到
    install_name_tool -id "@rpath/librdcore_ffi.dylib" "$DST"
    echo "✓ macos: $DST"
    ;;
  ios)
    # 仅构建真机 (device) 目标。openh264-sys2 目前不支持 aarch64-apple-ios-sim
    # （build.rs 报 "Unknown target env: sim"），且你正在跑真机调试，无需 sim 切片。
    DEV="aarch64-apple-ios"
    cargo rustc -p rdcore-ffi --crate-type staticlib --target "$DEV" $CARGO_PROFILE_FLAG
    # 注意：cargo 从仓库根（workspace）调用，-p rdcore-ffi 的产物落在仓库根 target/，
    # 而非包内 core/crates/rdcore-ffi/target/。务必从这里取。
    DEV_A="$ROOT/target/$DEV/$TARGET_SUBDIR/librdcore_ffi.a"
    # 输出路径/文件名须与 flutter/ios/Runner.xcodeproj/project.pbxproj 一致：
    #   PBXGroup "rdcore" -> path = rdcore；PBXFileReference -> librdcore_ffi.a
    #   即 Xcode 在 link 阶段查找 flutter/ios/rdcore/librdcore_ffi.a
    DST="$FLUTTER_DIR/ios/rdcore/librdcore_ffi.a"
    mkdir -p "$(dirname "$DST")"
    cp "$DEV_A" "$DST"
    echo "✓ ios: $DST (device only)"
    ;;
  linux)
    OUT="$(build_cdylib)"
    SRC="$OUT/librdcore_ffi.so"
    DST="$FLUTTER_DIR/linux/bundle/lib/librdcore_ffi.so"
    mkdir -p "$(dirname "$DST")"
    cp "$SRC" "$DST"
    patchelf --set-rpath '$ORIGIN' "$DST" 2>/dev/null || \
      echo "  (patchelf 不可用，请手动确保运行期于 \$ORIGIN/lib 命中)"
    echo "✓ linux: $DST"
    ;;
  android)
    # 经 cargo-ndk 交叉编译三个 ABI，直接按 jniLibs/<abi>/ 布局布署。
    # 前置：cargo install cargo-ndk；ANDROID_NDK_HOME 指向 NDK（或 ANDROID_NDK_ROOT）。
    if ! command -v cargo-ndk >/dev/null 2>&1 && ! cargo ndk --version >/dev/null 2>&1; then
      echo "错误：未找到 cargo-ndk。请执行 cargo install cargo-ndk 后重试。" >&2
      exit 1
    fi
    if [[ -z "${ANDROID_NDK_HOME:-}" && -z "${ANDROID_NDK_ROOT:-}" ]]; then
      echo "错误：未设置 ANDROID_NDK_HOME（或 ANDROID_NDK_ROOT）。" >&2
      echo "      通常位于 \$ANDROID_HOME/ndk/<version>。" >&2
      exit 1
    fi
    DST_DIR="$FLUTTER_DIR/android/app/src/main/jniLibs"
    mkdir -p "$DST_DIR"
    cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
      -o "$DST_DIR" \
      build -p rdcore-ffi $CARGO_PROFILE_FLAG
    echo "✓ android: $DST_DIR/<arm64-v8a|armeabi-v7a|x86_64>/librdcore_ffi.so"
    ;;
  *)
    echo "未知平台: $PLATFORM (支持: windows|macos|ios|linux|android)" >&2
    exit 2
    ;;
esac
