#!/usr/bin/env bash
# 构建可部署的 Web Viewer 静态产物（web/app/dist）。
#
# 用法（两种部署形态）：
#   # A) HTTPS 生产（Caddy 终结 TLS，裸主机 → 自动 wss:// + /signaling）：
#   VITE_SIGNALING_DEFAULT='8.138.237.243' bash web/deploy/build-dist.sh
#
#   # B) HTTP 测试（明文 ws，页面也只能用 http 访问）：
#   VITE_SIGNALING_DEFAULT='ws://8.138.237.243:8080' bash web/deploy/build-dist.sh
#
#   # 可选：注入 ICE 服务器（跨 NAT 必须含 TURN；JSON 单行）：
#   VITE_ICE_SERVERS='[{"urls":["stun:8.138.237.243:3478"],"username":"rdcore","credential":"***"}]' \
#     VITE_SIGNALING_DEFAULT='8.138.237.243' bash web/deploy/build-dist.sh
#
# 注意：VITE_* 会被 vite 内联进公开 bundle——TURN 凭据对任何能打开页面的人可见，
#       生产环境应改用动态凭据（见 web/deploy/README.md「TURN 凭据」一节）。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# 1) WASM 绑定（pkg 为构建产物、不入库；已存在则跳过）
if [ ! -f web/rdcore-web/pkg/rdcore_web.js ]; then
  echo "==> 构建 rdcore-web WASM"
  cargo build --release --target wasm32-unknown-unknown -p rdcore-web
  wasm-bindgen --target web --out-dir web/rdcore-web/pkg \
    target/wasm32-unknown-unknown/release/rdcore_web.wasm
fi

# 2) 前端构建（tsc 类型检查 + vite 打包，VITE_* 内联）
cd web/app
[ -d node_modules ] || npm install
npm run build

echo "==> dist 就绪: web/app/dist"
echo "    信令默认值: ${VITE_SIGNALING_DEFAULT:-<未注入，页面手输>}"
echo "    ICE 注入:   ${VITE_ICE_SERVERS:+是}${VITE_ICE_SERVERS:-否（回退公共 STUN）}"
