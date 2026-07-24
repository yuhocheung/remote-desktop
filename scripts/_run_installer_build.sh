#!/usr/bin/env bash
# 内部：在 MSVC 环境下跑安装包构建（供后台任务调用）。
# 直接用 bash 执行本脚本；脚本内部把"注入 MSVC 环境 + 构建"写成 .bat 交给 cmd 执行。
# 关键：bat 路径必须转成 Windows 反斜杠（cygpath -w），否则 cmd /c "C:/.../x.bat"
#       会把开头的 /c 当成 cmd 的 /c 开关而失败。
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$PATH:/c/Program Files (x86)/NSIS:/c/Users/69497/AppData/Local/bin/NASM"
export CARGO_TERM_COLOR=never
LOG=/tmp/build_installer.log
exec > >(stdbuf -oL tee "$LOG") 2>&1

echo "[$(date +%T)] nasm:    $(command -v nasm || echo MISSING)"
echo "[$(date +%T)] makensis: $(command -v makensis || ls '/c/Program Files (x86)/NSIS/makensis.exe' 2>/dev/null || echo MISSING)"
VSBAT="$("C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe" -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>/dev/null)/VC/Auxiliary/Build/vcvars64.bat"
echo "[$(date +%T)] vcvars:  $VSBAT"
if [ ! -f "$VSBAT" ]; then echo "ERROR: 找不到 vcvars64.bat"; exit 1; fi

BAT_UNIX="$ROOT/_msvc_build.bat"
BAT_WIN="$(cygpath -w "$BAT_UNIX" 2>/dev/null || echo "$BAT_UNIX")"
cat > "$BAT_UNIX" <<EOF
@echo off
call "$VSBAT" >nul 2>&1
bash scripts/build_installer_nsis.sh
EOF
echo "[$(date +%T)] bat=$BAT_WIN"
echo "[$(date +%T)] 启动构建 ..."
cmd /c "$BAT_WIN"
RC=$?
rm -f "$BAT_UNIX"
echo "[$(date +%T)] 构建脚本退出码: $RC"
exit $RC
