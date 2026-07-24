#!/usr/bin/env bash
# M3 公网验收 · VPS 侧一键起服务（Track B / kimi-k3）。
#
# 在公网 VPS 上部署 coturn(TURN/STUN) + signaling-svc(wss) + caddy(TLS 终结)。
# 前置：VPS 已装 docker + docker compose；80/443/3478(udp)/49152-65535(udp) 已放行。
#
# 用法：
#   cd cloud/deploy
#   cp .env.example .env   # 填 DOMAIN / TURN_EXTERNAL_IP / TURN_USER / TURN_PASS
#   ./acceptance_up.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if [[ ! -f .env ]]; then
  echo "❌ 缺少 .env（先 cp .env.example .env 并填 DOMAIN/TURN_EXTERNAL_IP/TURN_USER/TURN_PASS）" >&2
  exit 1
fi
# 校验必填项。
set -a; source .env; set +a
: "${DOMAIN:?填 DOMAIN（对外域名）}"
: "${TURN_EXTERNAL_IP:?填 TURN_EXTERNAL_IP（VPS 公网 IP）}"
: "${TURN_USER:=rdcore}"
: "${TURN_PASS:?填 TURN_PASS（TURN 凭据）}"

echo "==> 构建并启动 coturn + signaling + caddy …"
docker compose up -d --build

echo "==> 等待 caddy 申请 TLS 证书（首次需数十秒）…"
sleep 5
docker compose ps

echo
echo "✅ 控制平面已启动。客户端验收配置（host 与 viewer 都设）："
cat <<EOF
  export RDCORE_STUN="stun:${DOMAIN}:3478"
  export RDCORE_TURN_URL="turn:${DOMAIN}:3478?transport=udp"
  export RDCORE_TURN_USER="${TURN_USER}"
  export RDCORE_TURN_PASS="<同 .env 的 TURN_PASS>"
  # 信令：wss://${DOMAIN}/signaling/<32hex session_id>?token=<64hex token>
EOF

echo
echo "==> 健康检查："
echo "  TURN 端口:  nc -zvu ${DOMAIN} 3478"
echo "  wss 健康:   curl -fsS https://${DOMAIN}/healthz && echo"
