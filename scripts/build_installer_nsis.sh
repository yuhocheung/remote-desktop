#!/usr/bin/env bash
# 构建 Windows 受控端 NSIS 安装包。
#
# 前置（Windows 本机 / CI windows-latest）：
#   1. Rust 工具链（rust-toolchain.toml 已固定；rustup show 自动安装）
#   2. MSVC 构建工具：Visual Studio Build Tools 勾选 “使用 C++ 的桌面开发”
#      （含 Windows SDK，rdcore-capture / windows-service / windows-sys 需要）
#   3. NASM：openh264 编译期从源码构建，需要 nasm 在 PATH（https://www.nasm.us/）
#   4. NSIS：makensis 在 PATH（https://nsis.sourceforge.io/）
#
# 产出： scripts/installer/RdCore-Host-Setup.exe
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/scripts/installer/dist"
INSTALLER_DIR="$ROOT/scripts/installer"

echo "==> 检查 makensis"
if ! command -v makensis >/dev/null 2>&1; then
  echo "错误：未找到 makensis。请安装 NSIS 并将其加入 PATH。" >&2
  exit 1
fi

mkdir -p "$DIST"

# 1) 编译受控端主程序（带 service feature）
echo "==> cargo build rdcore-desktop --features service"
( cd "$ROOT/core" && cargo build --release -p rdcore-desktop --features service )

# 2) 编译横幅子进程（真实 Windows 置顶窗口）
echo "==> cargo build rdcore-banner --features windows-native,consent"
( cd "$ROOT/core" && cargo build --release -p rdcore-banner --features windows-native,consent )

# 3) 收集产物
#    注意：本仓库是单一 Cargo 工作区（根 Cargo.toml 含 [workspace]），
#    在 core/ 下执行 cargo build 的输出仍落在工作区根目录的 target/release/，
#    而不是 core/target/。故从这里拷贝。
cp "$ROOT/target/release/rdcore-desktop.exe" "$DIST/"
cp "$ROOT/target/release/rdcore-banner.exe"  "$DIST/"
cp "$ROOT/LICENSE"                              "$DIST/"
#    品牌托盘图标（rdcore-banner build.rs 会把它拷到 exe 同目录，但安装包
#    仍需独立随附一份到 $INSTDIR，使运行时 GetModuleFileNameW 能定位到）。
cp "$ROOT/icon.ico" "$DIST/"

# 3.5) 随附 FFmpeg 运行库（hwcodec 硬件编码依赖，ffmpeg 7.1.2 低地板构建）
#    rdcore-desktop.exe 启用 hwcodec 后动态链接 av*-61 系 DLL，安装包必须携带，
#    否则目标机启动报「找不到 avdevice-61.dll」。来源：$FFMPEG_DIR/bin
#    （默认 C:\ffmpeg-7.1.2-lowfloor-dev，见 docs/ffmpeg_hw_lowfloor.md）。
FFMPEG_HOME="$(cygpath "${FFMPEG_DIR:-C:/ffmpeg-7.1.2-lowfloor-dev}" 2>/dev/null || echo "${FFMPEG_DIR:-C:/ffmpeg-7.1.2-lowfloor-dev}")"
if [ -d "$FFMPEG_HOME/bin" ]; then
  for dll in avcodec-61 avdevice-61 avfilter-10 avformat-61 avutil-59 swresample-5 swscale-8; do
    cp "$FFMPEG_HOME/bin/$dll.dll" "$DIST/"
  done
  echo "==> 已随附 7 个 ffmpeg DLL（来自 $FFMPEG_HOME/bin）"
else
  echo "警告：未找到 ffmpeg 运行库目录（$FFMPEG_HOME/bin）。若 rdcore-desktop 启用了 hwcodec，" >&2
  echo "      安装包在目标机将报「找不到 avdevice-61.dll」。请设置 FFMPEG_DIR 后重跑。" >&2
fi

# 4) 生成安装包
echo "==> makensis"
#    NSIS 是原生 Windows 程序，DIST 必须是 Windows 风格路径（反斜杠）；
#    本脚本在 Git Bash 下运行时 $DIST 是 /c/Users/... 的 Unix 路径，直接传给
#    makensis 会因 /c/... 前导斜杠解析不到文件。用 cygpath -w 转换（不可用时回退原值）。
DIST_WIN="$(cygpath -w "$DIST" 2>/dev/null || echo "$DIST")"
( cd "$INSTALLER_DIR" && makensis /DDIST="$DIST_WIN" installer.nsi )

echo "==> 完成： $INSTALLER_DIR/RdCore-Host-Setup.exe"
