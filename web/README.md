# rdcore-web — 浏览器 Viewer（Web 控制端）

复用 `shared/*` 三 crate（协议 / 加密 / 身份）编译到 WASM，用浏览器原生 WebRTC
连接现有 Windows Host 的第一个开发增量（M0/M1）。

## 布局

- `rdcore-web/` — Rust crate（`cdylib + rlib`，workspace 成员）。wasm-bindgen facade：
  - 身份：`generate_identity` / `identity_from_seed` / `identity_public` /
    `identity_export` / `identity_import`（口令经 10 万次 SHA-256 KDF + XChaCha20Poly1305，
    格式 `[salt16][nonce24][密文]`，与 `rdcore-identity::persist` 同思路；持久化交 TS/IndexedDB）。
  - 握手：`WebHandshake`（`peer_hello` / `build_signed_offer` / `handle_answer` /
    `build_session_key_exchange` / `handle_session_key_exchange` / `build_ice` / `handle_message`），
    镜像 `rdcore-session` 与 `rdcore-app::Connection::establish` 的 Viewer 分支。
  - 帧管线：`FramePipeline`（SCTP 1 字节标签分片重组 + 4 字节小端长度前缀 +
    媒体帧 / 控制消息 E2E 加解密），镜像 `rdcore-rtc::real` 与 `rdcore-media` 帧格式。
  - 发送侧构造：`build_input_*` / `build_heartbeat` / `build_clipboard_request` /
    `frame_wrap` / `sctp_chunk` / `sctp_chunk_count`（M2 用）。
- `rdcore-web/pkg/` — wasm-bindgen `--target web` 生成的 JS/WASM 绑定（构建产物，不入库）。
- `testvectors/` — 黄金测试向量（`*.json`，由 `cargo test -p rdcore-web` 确定性生成）
  与 Node 对拍脚本 `parity.mjs`。
- `app/` — Vite + TypeScript 浏览器 Viewer 骨架（P3；`npm install` 未执行）。

## 构建与验证

```bash
# Rust 测试（含黄金向量生成 + rdcore-session 互认交叉验证）
cargo test -p rdcore-web

# WASM 构建 + 生成 JS 绑定
cargo build --release --target wasm32-unknown-unknown -p rdcore-web
wasm-bindgen --target web --out-dir web/rdcore-web/pkg \
  target/wasm32-unknown-unknown/release/rdcore_web.wasm

# Node 对拍（WASM 重算 vs 黄金向量）
node web/testvectors/parity.mjs

# 浏览器 App 骨架
cd web/app && npm install && npm run dev
```

## 线格式要点（以 shared/core 源码为准）

- 信令：`wss://<host>/signaling/<32hex session>?token=<64hex token>`，二进制帧 =
  postcard(`Message`)，单帧 ≤ 64 KiB。
- Offer/Answer 签名：Ed25519 over `canonical_signing_bytes(SigningPayload
  {session_id, from, sdp, capabilities, frame})`（postcard 规范字节）。
- 会话密钥：已签名的 X25519 临时公钥（签名覆盖 `session_id ‖ from ‖ ephemeral`），
  ECDH + SHA-256 派生 32 字节密钥。
- 数据通道：`media` id=0 / `control` id=1 / `audio` id=2（negotiated, ordered）。
  每条 SCTP 消息首字节为分片标签（0 整包 / 1 首 / 2 中 / 3 末），大帧按 16 KiB 切片；
  重组后为 `[4 字节小端长度][postcard 负载]`。
- 媒体帧：postcard(`MediaFrame`)，`data` = postcard(`Ciphertext{nonce[24], data}`)，
  XChaCha20Poly1305 解密后是 H.264 Annex-B。
- 控制消息：postcard(`AppMessage`)（与 `rdcore-app::AppMessage` 同构）整条 AEAD 后以
  `Message::Encrypted` 承载。
- ICE 候选：`IceCandidate.candidate` 字段承载整段 JSON 序列化的 RTCIceCandidateInit。
