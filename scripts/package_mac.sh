#!/usr/bin/env bash
# 把 macOS 受控端（rdcore-desktop + rdcore-banner）打成 RdCore Host.app bundle。
#
# 为什么需要 .app：macOS 屏幕录制 / 辅助功能权限（TCC）按「可执行文件 / bundle」记授权。
# 从终端直接跑裸二进制时，授权会记到「终端 App」头上，弹了提示但设置里找不到、
# 或勾选了仍抓不到屏。打成 .app 后授权记到 bundle 本身，双击 / open 启动即永久生效，
# 且 launchd 常驻也更干净（plist 直接指向 bundle 内二进制）。
#
# 产出： scripts/installer/RdCore Host.app
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT/scripts/installer/RdCore Host.app"
MACOS_DIR="$APP_DIR/Contents/MacOS"
RES_DIR="$APP_DIR/Contents/Resources"

APP_NAME="RdCore Host"
BUNDLE_ID="com.rdcore.host-agent"
# 主可执行文件名必须与 Info.plist 的 CFBundleExecutable 一致。
# launchd / 双击启动时由它拉起（内部再 spawn 同目录的 rdcore-banner）。
DESKTOP_BIN="rdcore-desktop"
BANNER_BIN="rdcore-banner"

echo "==> cargo build rdcore-desktop --features service"
( cd "$ROOT/core" && cargo build --release -p rdcore-desktop --features service )

echo "==> cargo build rdcore-banner --features macos-native,consent"
( cd "$ROOT/core" && cargo build --release -p rdcore-banner --features macos-native,consent )

echo "==> 组装 $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RES_DIR"

# 主程序与横幅子进程放同一目录（banner.rs 的 resolve_banner_bin 在同目录查找）。
cp "$ROOT/target/release/$DESKTOP_BIN" "$MACOS_DIR/"
# 横幅二进制缺失（如尚未编译）时容忍：desktop 会以无 OS 横幅模式运行（状态仅打日志）。
if [ -f "$ROOT/target/release/$BANNER_BIN" ]; then
  cp "$ROOT/target/release/$BANNER_BIN" "$MACOS_DIR/"
else
  echo "警告：未找到 $BANNER_BIN，将以无横幅模式打包。可先 cargo build --release -p rdcore-banner --features consent 后重跑。" >&2
fi
cp "$ROOT/LICENSE"                      "$RES_DIR/"

# 图标：沿用仓库根的 icon.ico（macOS 也支持读 ico 里的 png 帧；后续可换 .icns 更美观）。
cp "$ROOT/icon.ico" "$RES_DIR/AppIcon.ico"

# launcher：.app 的真正入口。LaunchServices 只负责拉起它，它再 exec 主程序的 `run`，
# 避免「双击后 rdcore-desktop 无子命令直接退出（等价 --help）导致看起来没反应」。
# 必须用编译型二进制（不能用 bash 脚本）：脚本的 designated requirement 会锚定到
# com.apple.bash，Gatekeeper 要求 Apple 系统签名，ad-hoc 重签必然 rejected。
cat > /tmp/rdcore_launcher.c <<EOF
#include <unistd.h>
#include <stdlib.h>
int main(int argc, char *argv[]) {
    (void)argc; (void)argv;
    char *desktop = "$MACOS_DIR/$DESKTOP_BIN";
    char *args[] = { desktop, "run", NULL };
    execv(desktop, args);
    return 1;
}
EOF
cc -O2 -o "$MACOS_DIR/launcher" /tmp/rdcore_launcher.c
rm -f /tmp/rdcore_launcher.c

# Info.plist：声明 bundle 标识 + 屏幕录制用途描述（系统弹窗显示文案）。
# CFBundleExecutable 必须等于上面生成的入口文件名（launcher）。
cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleExecutable</key>
    <string>launcher</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <!-- 不设 LSUIElement：远程桌面 Host 是后台服务，但该属性会让 LaunchServices
         对「ad-hoc 签名 + 未公证」的 bundle 直接冻结并报「没有响应」。
         去掉后 ad-hoc 签名即可正常双击启动；Dock 图标出现是可接受的折中。 -->
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>RdCore 受控端需要录制屏幕，以便控制端远程查看本机画面。</string>
</dict>
</plist>
EOF

# ad-hoc 签名：macOS Gatekeeper 对「未签名的下载/新建 .app」会冻结启动并抛
# 「没有响应」。本地自签（--sign -）即可让 LaunchServices 正常拉起；分发给他机
# 时应换成 Apple Developer ID 签名 + notarize（见 docs 或后续 notarize 脚本）。
echo "==> ad-hoc 签名 $APP_DIR"
codesign --force --deep --sign - "$APP_DIR"

# 移除隔离属性：ad-hoc 签名没有「已公证」标记，LaunchServices 双击路径会拦截。
# 本地自用时清掉 com.apple.quarantine 即可正常双击；分发给他机应改用
# Developer ID 签名 + notarize（见 docs 或后续 notarize 脚本）。
xattr -dr com.apple.quarantine "$APP_DIR" 2>/dev/null || true

echo "==> 完成： $APP_DIR"
echo ""
echo "使用："
echo "  双击运行： open \"$APP_DIR\""
echo "  首次运行会弹「屏幕录制」授权提示，允许后永久生效（授权记在 $APP_NAME 上，不挑终端）。"
echo "  常驻： \"$MACOS_DIR/$DESKTOP_BIN\" install   # launchd LaunchAgent"
