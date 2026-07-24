#!/usr/bin/env bash
# M3 公网验收 · 健康检查（Track B / kimi-k3）。
# 校验 VPS 上 coturn + signaling(wss) 是否就绪。用法：./acceptance_check.sh <DOMAIN>
set -euo pipefail
DOMAIN="${1:?用法: $0 <DOMAIN>}"

echo "==> [1/3] TURN UDP 3478 连通性 …"
if command -v nc >/dev/null 2>&1; then
  nc -zvu -w3 "$DOMAIN" 3478 && echo "  ✅ TURN 3478/udp 可达" || echo "  ❌ TURN 3478/udp 不可达"
else
  echo "  ⚠️ 无 nc，跳过（可用 nmap -sU -p 3478 $DOMAIN）"
fi

echo "==> [2/3] wss TLS 终结（caddy /healthz）…"
if curl -fsS "https://${DOMAIN}/healthz" >/dev/null 2>&1; then
  echo "  ✅ https://${DOMAIN}/healthz 正常"
else
  echo "  ❌ https://${DOMAIN}/healthz 不可达（证书未就绪或 caddy 未起）"
fi

echo "==> [3/3] 信令握手（应返回 400/401 而非连接失败）…"
# 无 token/无 session 应被拒（400 缺 session / 401 缺 token），证明 signaling 在工作。
code=$(curl -s -o /dev/null -w "%{http_code}" "https://${DOMAIN}/signaling/" || echo "000")
echo "  /signaling/ HTTP $code（400/401/426 均表示信令服务在响应）"

echo
echo "✅ 健康检查完成。下一步见 docs/acceptance_m3.md 的客户端步骤。"
