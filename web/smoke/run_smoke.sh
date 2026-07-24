#!/usr/bin/env bash
# 全链路 smoke 编排：信令服务 + Host + vite preview → Playwright 驱动真实 Chrome。
# 用法：
#   bash web/smoke/run_smoke.sh                 # 默认 headless 合成源（CI 友好）
#   bash web/smoke/run_smoke.sh --realcapture   # 真实 DXGI 抓屏（本机屏幕，浏览器有头运行）
#   bash web/smoke/run_smoke.sh --reconnect     # 断线重连场景（headless：刷新 + 关页重连，同一配对码）
#   bash web/smoke/run_smoke.sh --input         # 键鼠输入场景（真实注入 Host + 有头浏览器，
#                                               #   阶段1 映射断言 + 阶段2 OS 回环注入断言）
#   bash web/smoke/run_smoke.sh --autoreconnect # 断线自动重连场景（headless：页内掐断信令+P2P，
#                                               #   断言指数退避 + 同码自动恢复 + 密钥轮换）
#   bash web/smoke/run_smoke.sh --clipboard     # M3-A 剪贴板同步（probe-host 对端：双向 + 系统剪贴板读回）
#   bash web/smoke/run_smoke.sh --filetransfer  # M3-B 文件传输（probe-host 对端：双向 + sha256 对拍）
#   bash web/smoke/run_smoke.sh --audio         # M3-C 音频播放（probe-host 合成 440Hz 正弦 → WebAudio RMS 断言）
# 结束时 trap 兜底杀掉全部后台进程，不留孤儿。
set -u

REALCAPTURE=0
RECONNECT=0
INPUT=0
AUTORECONNECT=0
CLIPBOARD=0
FILETRANSFER=0
AUDIO=0
for a in "$@"; do
  case "$a" in
    --realcapture) REALCAPTURE=1 ;;
    --reconnect) RECONNECT=1 ;;
    --input) INPUT=1 ;;
    --autoreconnect) AUTORECONNECT=1 ;;
    --clipboard) CLIPBOARD=1 ;;
    --filetransfer) FILETRANSFER=1 ;;
    --audio) AUDIO=1 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_POSIX="$(cd "$SCRIPT_DIR/../.." && pwd)"
ROOT_WIN="$(cd "$SCRIPT_DIR/../.." && pwd -W)"
RUN_DIR="$SCRIPT_DIR/run"
RUN_DIR_WIN="$ROOT_WIN\\web\\smoke\\run"
mkdir -p "$RUN_DIR"
rm -f "$RUN_DIR/token_db.txt" "$RUN_DIR"/*.log \
  "$RUN_DIR"/clipboard_received.txt "$RUN_DIR"/ft_recv_* "$RUN_DIR"/ft_offer.bin "$RUN_DIR"/upload.bin

SIG_PORT="${SIG_PORT:-18081}"
WEB_PORT="${WEB_PORT:-14171}"

# ── 模式相关参数 ──
HOST_MODE_ARGS="--headless"
HOST_KIND="desktop"   # desktop = rdcore-desktop agent；probe = M3 探针 Host（web/smoke/probe-host）
OUT_PNG="connected.png"
OUT_JSON="result.json"
HEADED=0
EXPECTED_W=0
EXPECTED_H=0
if [ "$REALCAPTURE" = 1 ]; then
  HOST_MODE_ARGS=""   # 去掉 --headless → agent 走 ScrapCaptureSource（DXGI 真实抓屏）
  OUT_PNG="connected-realcapture.png"
  OUT_JSON="result-realcapture.json"
  HEADED=1            # 有头 Chrome：Viewer 窗口进入被采画面，递归变化保证帧差分
  # 取真实主屏分辨率做断言（Win32_VideoController 报适配器当前输出）
  RES="$('/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe' -NoProfile -Command \
    "Get-WmiObject Win32_VideoController | Where-Object { \$_.CurrentHorizontalResolution } | Select-Object -First 1 CurrentHorizontalResolution, CurrentVerticalResolution | Format-Table -HideTableHeaders" 2>/dev/null \
    | tr -d '\r' | grep -oE '[0-9]+' | head -2 | tr '\n' ' ')"
  EXPECTED_W="$(echo "$RES" | awk '{print $1}')"
  EXPECTED_H="$(echo "$RES" | awk '{print $2}')"
  EXPECTED_W="${EXPECTED_W:-0}"; EXPECTED_H="${EXPECTED_H:-0}"
  echo "== 真实抓屏模式：主屏分辨率 ${EXPECTED_W}x${EXPECTED_H}"
fi
if [ "$RECONNECT" = 1 ]; then
  OUT_PNG="reconnected.png"
  OUT_JSON="result-reconnect.json"
  echo "== 断线重连模式：headless 合成源，三轮连接（首连 → reload → 关页重开）"
fi
if [ "$INPUT" = 1 ]; then
  HOST_MODE_ARGS=""   # 真实 enigo 注入需要非 headless Host（headless 是 NullInputInjector）
  OUT_PNG="input.png"
  OUT_JSON="result-input.json"
  HEADED=1            # 有头 Chrome：OS 回环断言需要真实窗口接收注入事件
  echo "== 键鼠输入模式：真实抓屏+真实注入 Host，有头浏览器"
fi
if [ "$AUTORECONNECT" = 1 ]; then
  OUT_PNG="autoreconnected.png"
  OUT_JSON="result-autoreconnect.json"
  echo "== 断线自动重连模式：headless 合成源，两轮掐断-恢复（退避 1s→2s）"
fi
if [ "$CLIPBOARD" = 1 ]; then
  HOST_KIND="probe"
  OUT_PNG="clipboard.png"
  OUT_JSON="result-clipboard.json"
  echo "== M3-A 剪贴板同步模式：probe-host 对端，双向 + 系统剪贴板读回"
fi
if [ "$FILETRANSFER" = 1 ]; then
  HOST_KIND="probe"
  OUT_PNG="filetransfer.png"
  OUT_JSON="result-filetransfer.json"
  echo "== M3-B 文件传输模式：probe-host 对端，双向 + sha256 对拍"
fi
if [ "$AUDIO" = 1 ]; then
  HOST_KIND="probe"
  OUT_PNG="audio.png"
  OUT_JSON="result-audio.json"
  echo "== M3-C 音频播放模式：probe-host 合成 440Hz 正弦 → WebAudio RMS 断言"
fi

export SIGNALING_ADDR="127.0.0.1:$SIG_PORT"
export SIGNALING_TOKEN_DB="$RUN_DIR_WIN\\token_db.txt"   # Rust 进程需要 Windows 路径
export PATH="/c/Program Files/nodejs:$PATH"

echo "== 启动 signaling-svc ($SIGNALING_ADDR, token_db=$SIGNALING_TOKEN_DB)"
"$ROOT_POSIX/target/debug/signaling-svc.exe" >"$RUN_DIR/signaling.log" 2>&1 &
SIG_PID=$!

if [ "$HOST_KIND" = "probe" ]; then
  echo "== 启动 probe-host（M3 探针 Host：剪贴板/文件/音频协议对端）"
  "$ROOT_POSIX/target/debug/probe-host.exe" \
    --signal "ws://127.0.0.1:$SIG_PORT" \
    --identity-dir "$RUN_DIR_WIN\\identity" \
    --identity-pass smoke-test \
    --run-dir "$RUN_DIR_WIN" \
    >"$RUN_DIR/host.log" 2>&1 &
  HOST_PID=$!
else
  echo "== 启动 rdcore-desktop run ${HOST_MODE_ARGS:-(真实抓屏)} --loopback --no-banner"
  # HOST_MODE_ARGS 有意不加引号：为空时不产生空参数
  "$ROOT_POSIX/target/debug/rdcore-desktop.exe" run \
    --signal "ws://127.0.0.1:$SIG_PORT" \
    $HOST_MODE_ARGS --loopback --no-banner --fps 15 \
    --identity-dir "$RUN_DIR_WIN\\identity" \
    --identity-pass smoke-test \
    >"$RUN_DIR/host.log" 2>&1 &
  HOST_PID=$!
fi

echo "== 启动 vite preview (127.0.0.1:$WEB_PORT)"
(cd "$ROOT_POSIX/web/app" && node node_modules/vite/bin/vite.js preview \
  --host 127.0.0.1 --port "$WEB_PORT" --strictPort) >"$RUN_DIR/web.log" 2>&1 &
WEB_PID=$!

PIDS="$SIG_PID $HOST_PID $WEB_PID"
cleanup() {
  echo "== 清理后台进程 ($PIDS)"
  kill $PIDS 2>/dev/null
  sleep 1
  kill -9 $PIDS 2>/dev/null
  rm -f "$RUN_DIR/token_db.txt"
}
trap cleanup EXIT

# ── 等待三个服务就绪 ──
wait_tcp() { # host port name timeout
  for _ in $(seq "$4"); do
    # /dev/tcp：纯 TCP 连通性探测（WS 服务不会回 HTTP，curl 会误判）
    if (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null; then exec 3>&- 3<&-; return 0; fi
    sleep 1
  done
  echo "!! $3 端口 $2 未在 $4s 内就绪"; return 1
}

wait_tcp 127.0.0.1 "$SIG_PORT" signaling 15 || { tail -20 "$RUN_DIR/signaling.log"; exit 1; }
wait_tcp 127.0.0.1 "$WEB_PORT" web 20 || { tail -20 "$RUN_DIR/web.log"; exit 1; }

# 真实抓屏 / 输入注入：确认 Host 的抓屏源初始化成功（失败时 agent 直接退出并写错误日志）
if [ "$REALCAPTURE" = 1 ] || [ "$INPUT" = 1 ]; then
  sleep 2
  if ! kill -0 "$HOST_PID" 2>/dev/null; then
    echo "!! Host 进程已退出（真实抓屏初始化失败？）"; tail -20 "$RUN_DIR/host.log"; exit 1
  fi
fi

PAIRING=""
for _ in $(seq 30); do
  PAIRING="$(grep -oE '[0-9a-f]{32}:[0-9a-f]{64}' "$RUN_DIR/host.log" | head -1 || true)"
  [ -n "$PAIRING" ] && break
  sleep 1
done
if [ -z "$PAIRING" ]; then
  echo "!! 30s 内未从 Host 日志拿到配对码"; tail -30 "$RUN_DIR/host.log"; exit 1
fi
echo "== 配对码: $PAIRING"

# ── 跑浏览器 smoke ──
PAIRING_CODE="$PAIRING" \
HOST_ADDR="ws://127.0.0.1:$SIG_PORT" \
WEB_URL="http://127.0.0.1:$WEB_PORT/?probe=1" \
OUT_DIR="$ROOT_WIN\\web\\smoke" \
OUT_PNG="$OUT_PNG" \
OUT_JSON="$OUT_JSON" \
HEADED="$HEADED" \
REALCAPTURE="$REALCAPTURE" \
RECONNECT="$RECONNECT" \
INPUT="$INPUT" \
AUTORECONNECT="$AUTORECONNECT" \
CLIPBOARD="$CLIPBOARD" \
FILETRANSFER="$FILETRANSFER" \
AUDIO="$AUDIO" \
EXPECTED_W="$EXPECTED_W" \
EXPECTED_H="$EXPECTED_H" \
node "$SCRIPT_DIR/smoke.mjs"
RC=$?

# ── 场景 B：Host 侧无感证据（仅重连模式；浏览器断言过后 Host 仍在原地等待）──
if [ "$RECONNECT" = 1 ]; then
  ESTABLISHED="$(grep -c '连接已建立' "$RUN_DIR/host.log" || true)"
  RESCAN_WAIT="$(grep -c 'Viewer 已断开，等待重新连接' "$RUN_DIR/host.log" || true)"
  HOST_ALIVE=false; kill -0 "$HOST_PID" 2>/dev/null && HOST_ALIVE=true
  TOKEN_DB="$RUN_DIR/token_db.txt"
  TOKEN_FRESH=false; TOKEN_AGE=-1
  if [ -f "$TOKEN_DB" ]; then
    NOW=$(date +%s); MT=$(stat -c %Y "$TOKEN_DB"); TOKEN_AGE=$((NOW - MT))
    [ "$TOKEN_AGE" -lt 90 ] && TOKEN_FRESH=true
  fi
  cat > "$ROOT_POSIX/web/smoke/result-reconnect-host.json" <<EOF
{
  "hostProcessAliveAtEnd": $HOST_ALIVE,
  "pairingUnchanged": true,
  "establishedCount": $ESTABLISHED,
  "disconnectWaitCount": $RESCAN_WAIT,
  "tokenFileFreshAfterReconnect": $TOKEN_FRESH,
  "tokenFileAgeSeconds": $TOKEN_AGE,
  "note": "配对 token 不焚毁：同一 session+token 可重复入房（cloud/crates/signaling-svc/src/lib.rs §B2，reload_from_file 以文件为事实 reconcile）；Host 进程全程未重启、配对码未变。"
}
EOF
  echo "== Host 侧证据: established=$ESTABLISHED rescanWaits=$RESCAN_WAIT alive=$HOST_ALIVE tokenAge=${TOKEN_AGE}s"
  if [ "$ESTABLISHED" -lt 3 ] || [ "$HOST_ALIVE" != true ] || [ "$TOKEN_FRESH" != true ]; then
    echo "!! Host 侧重连证据不完整"; RC=1
  fi
fi

echo "== Host 日志尾部 =="; tail -8 "$RUN_DIR/host.log"
echo "== 信令日志尾部 =="; tail -4 "$RUN_DIR/signaling.log"
echo "SMOKE_RC=$RC"
exit "$RC"
