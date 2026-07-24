# 双轨并行开发计划：iPhone 公网控制 Windows（端到端真实跑通）

> 目标：让 **iPhone（viewer，走蜂窝/公网）** 经 **信令(wss) + TURN** 连上 **Windows PC（host，家庭/办公 NAT 后）**，真实看到压缩画面、真实用触摸/键盘控制，E2E 加密、可配对、可重连。
> 本计划为 Track A（媒体面/端点原生/体验，本 agent）与 Track B（韧性面/配对/身份/传输加固，kimi-k3）的**并行开发清单**。双方据此同时开工，按里程碑对齐。

---

## 0. 完成定义（Definition of Done）

端到端"完全真实跑通"须同时满足：

1. iPhone 走**蜂窝网络**，Windows 在**家庭 NAT** 之后，两端不经同一局域网。
2. 经**公网信令(wss) + TURN** 建立连接（直连 P2P 仅作优化，非必需）。
3. 视频为**压缩编码（H.264）**，可观看、目标端到端延迟 < 300ms。
4. iPhone **触摸/键盘**真实控制 Windows；**OS 不可伪造横幅**显示对端指纹与连接状态。
5. **E2E 加密**；配对用**一次性 token（扫码/输码）**；重启后身份保持（TOFU 不重告警）。
6. 断网/切网**自动重连**，用户无感恢复。
7. 线上**无未压缩 RGBA 帧**（带宽可控）。

---

## 1. 双轨边界（基于 `docs/双轨开发契约.md`，本次需补 1 条）

| 资源 | 归属 | 说明 |
|---|---|---|
| `remote_screen.dart` / `rdcore_bindings.dart`(媒体) / `rdcore-media` / `rdcore-encode` / `rdcore-decode` / `rdcore-capture` / `rdcore-audio`(新建) | **A** | 媒体面、端点原生、体验 |
| `rdcore-rtc` | **A 主拥有**（新补） | `Connection` 构造 `WebRtcPeer`、媒体通道包裹其传输；B **仅**经 `Connection` 公共 API 与 `RTCPeerConnectionState` 观测，**禁改其内部** |
| `connection_lifecycle.rs` / `file_transfer.rs` / 配对·剪贴板·文件 UI / 身份持久化接入 | **B** | 韧性面、配对、身份、传输加固 |
| `cloud/crates/signaling-svc` | **B** | 信令服务（部署/加固/per-session token/TLS gateway）；A 仅消费其 WebSocket 协议，禁改其实现 |
| `rdcore-ffi/src/lib.rs` | A 已加 8 个媒体函数；**B 追加** `rdcore_*_pairing` / `rdcore_*_file` / `rdcore_*_identity_persist` | append-only，不互改对方函数体（§8 注1） |
| `flutter/ios/Runner` | **A**（端点原生） | §8 补一行：iOS 工程 A 拥有维护 |

> **M0 必须落地**：在契约 §8 补 `rdcore-rtc` 与 `flutter/ios` 两行 owner 声明，避免后续争用。

**锁步铁律（双方共守）**：
- `Message` 变体/字段新增 → §5 锁步（下标追加、通知对方）。
- `Connection` 签名/公共 API 变更 → §9 流程（A 改 + B 在 lifecycle 同步）。
- 每完成一个里程碑，双方各跑 `cargo test --workspace` + `flutter test` + 对方 e2e，确保零回归。

---

## 2. 里程碑总览

| 里程碑 | 周期 | Track A 交付 | Track B 交付 | 对齐验证 |
|---|---|---|---|---|
| **M0 启动** | 第 0 周 | **A0 Connection 签名放宽（草案评审+落地）** + 确认 §8 补 rdcore-rtc/iOS owner | 在公网 VPS 部署 coturn + signaling(wss)；敲定 token/session_id 方案 | 两端能经 TURN 建 WebRTC 链路；A0 合入后 B 可启动 B3/B4/B5 |
| **M1 同机压缩** | 第 1–2 周 | A1 H.264 编解码 + A4 渲染 | B2 每会话 token + wss；B3 配对码 | 同机：配对→出**压缩**画面 |
| **M2 iOS 成型** | 第 2–3 周 | A2 iOS 链接 + A3 触摸映射 | B4 身份持久化；B1 TURN 配置 | iOS 模拟器/真机可连 |
| **M3 公网真实跑通** | 第 3–4 周 | A5 Windows Agent | B5 自动重连 | **iPhone 蜂窝 → TURN → Windows 家庭 NAT 真实控制** |
| **M4 生产增强** | 第 4–6 周 | A6 音频 + A7 带宽自适应 | B6 文件/剪贴板端到端 | 完整体感；协调 J(RTP 优化) |

---

## 3. Track A 任务清单（本 agent）

### A0. Connection 签名放宽（跨轨接口契约，M0 首项，阻塞 B3/B4/B5）
- **目标**：把 `Connection::new_host/new_viewer` 写死的 `store: InMemoryIdentityStore` 放宽为可注入的持久化身份存储，并新增 `create_pairing()` / `reconnect()` 两个公共入口，解除 B3/B4/B5 对 A 的硬依赖。
- **改动 crate**：`rdcore-app/src/lib.rs`（`Connection` 结构体字段 + 构造函数参数 + 两个新 `pub` 方法）。
- **目标签名草案（供 kimi-k3 评审）**：
  ```rust
  // 结构体字段变更
  store: Arc<Mutex<dyn IdentityStore + Send + Sync>>,   // 原：InMemoryIdentityStore

  // 构造函数：去掉冗余的 _identity 参数（store 已含 local_identity）；
  // store/secret 由 PersistentIdentityStore::load_or_create 返回的 (Self, SecretKey) 直接喂入。
  pub async fn new_host(
      url: &str,
      session: SessionId,
      store: Arc<Mutex<dyn IdentityStore + Send + Sync>>,
      secret: SecretKey,
      rtc_cfg: RtcConfig,
      heartbeat_timeout: Duration,
  ) -> Result<Self, AppError>
  // new_viewer 同形

  // Host 配对入口：生成 session_id + 一次性 token，供 UI 展示（二维码/输码）。
  pub fn create_pairing() -> PairingInfo   // PairingInfo { session_id: SessionId, token: String }

  // 断网/切网重连：复用持久化身份与 session_id，经信令重新握手、重建通道。
  // 必须是 &self —— Connection 在 B 的 supervisor 中以 Arc<Connection> 持有，取不到 &mut。
  pub async fn reconnect(&self) -> Result<(), AppError>
  ```
- **待 kimi-k3 评审的 2 个设计点**：
  1. `store` 用 `Arc<Mutex<dyn IdentityStore + Send + Sync>>`：因 trait 的 `remember(&mut self)` 需内部可变性。若你更希望改 `IdentityStore` 让 `remember` 为 `&self`（内部 Mutex），也可，但波及 `InMemoryIdentityStore`/`PersistentIdentityStore` 两实现；A 默认走 `Arc<Mutex<>>`。
  2. `reconnect` 默认 `&self` + 内部可变性重建 `peer`/`session_key`/`consent`。若你倾向「lifecycle 层新建 Connection 并替换 supervisor 持有的连接」，也行，但需你同步调整 supervisor 持有方式；A 默认 `reconnect(&self)`。
- **验收**：`cargo test -p rdcore-app` 含「注入 `PersistentIdentityStore` 后 `new_host` 跑通 + `create_pairing` 返回 session_id/token + `reconnect` 重建通道」。
- **依赖**：无，M0 立即开始。
- **锁步**：属 §9 流程（A 改 `Connection` + B 在 `connection_lifecycle` 同步接收 `Arc<Mutex<dyn IdentityStore>>`）；B 在 A0 合入前可临时用 `InMemoryIdentityStore` 满足签名，待 A0 落地切换。

### A1. 视频压缩（H.264 编码/解码）— 公网第一前提
- **目标**：host 抓屏 RGBA → H.264；viewer H.264 → RGBA 渲染。1024×768@30fps 由 ~90MB/s 降到 ~1–3MB/s。
- **改动 crate**：`rdcore-encode`（`H264Encoder` 真实实现，默认启用 `h264` feature；优先硬件编码：Windows 经 FFmpeg 统一后端 NVENC/QSV/AMF——低驱动地板构建与接入见 `docs/ffmpeg_hw_lowfloor.md`，iOS 走 VideoToolbox，回退 `openh264`）、`rdcore-decode`（`H264Decoder`，iOS 走 VideoToolbox/openh264）。
- **接口契约**：保持 `Encoder`/`Decoder` trait 签名不变；`VideoCodec::H264` 走真实分支；握手期在 offer/answer 协商 `video_codec`（新增可选字段，默认 `Raw` 兼容旧端）。
- **锁步**：codec 协商字段属协议变更 → §5 通知 B（仅知会）。
- **验收**：`cargo test -p rdcore-encode -p rdcore-decode` 含"编码→解码像素一致"；A 的 e2e 用 `H264Encoder` 经真实 WebRTC 链路出画面。
- **依赖**：无，立即开始。

### A2. iOS 原生集成（Rust core → Flutter iOS）
- **目标**：iPhone App 真正包含 Rust core，能建连/收帧/发输入。
- **改动**：新增/扩展 `tool/build_ffi.sh`：`cargo build --target aarch64-apple-ios` + `aarch64-apple-ios-sim` → 合并 `.xcframework`；在 `flutter/ios/Runner` 链接（修改 `Runner.xcodeproj` / FFI 加载），验证 `webrtc-rs` 在 iOS aarch64 编译（纯 Rust，需验证 tokio 依赖）。
- **验收**：`flutter build ios --no-codesign` 通过；真机/模拟器启动能走到"等待配对"。
- **依赖**：与 A1 可并行；真机验证需 M1 后。
- **风险**：`webrtc-rs` iOS 交叉编译为第一风险，M2 前必须验证。

### A3. iOS 触摸 → InputEvent 映射
- **目标**：`remote_screen.dart` 加手势识别，映射为 `RdInputEvent` 经 `sendInputEvent` 发送。
- **映射规则**：tap=左键 down/up；长按=右键；单指拖动=mouse move（按 host 分辨率缩放坐标）；双指捏合=wheel delta；屏幕键盘=Key 事件。
- **验收**：Flutter 集成测试模拟手势 → host 端 `rdcore_host_poll_input` 收到对应事件（A 的 e2e 用 MockConnection 验证映射逻辑）。
- **依赖**：A2 后可真验；逻辑可先写。

### A4. Viewer 解码 + 渲染管线打通
- **目标**：viewer 收 `MediaFrame`(H.264 字节) → decode → `RemoteFrameView` 渲染；性能上用 Flutter Texture（IOSurface/CVPixelBuffer）替代 CustomPainter 逐帧绘制。
- **验收**：A1 编码端 + A4 渲染端经真实 WebRTC 链路出画面。
- **依赖**：A1。

### A5. Windows Host Agent 打包
- **目标**：可安装/后台常驻 Windows 程序：开机自启、常驻等待连接、开抓屏+注入、连接时显示 OS 横幅。
- **改动**：新增 `host-agent/` Rust 二进制（或 Flutter Windows 桌面壳），调用 `rdcore-app` + `rdcore-capture` real 后端；安装器（cargo wix / NSIS）或至少可运行 exe + 服务注册；Windows Defender 防火墙**出站**规则（TURN 模式只需出站，通常默认允许）。
- **验收**：Windows 启动 Agent → 注册信令 → iPhone 配对后看到画面并控制。
- **依赖**：A1/A4（媒体）、B3（配对触发）。

### A6. 音频管线（C，体验增强，可并行起步）
- **目标**：Windows 捕获音频 → Opus 编码 → 独立音频通道 → iOS 播放。
- **改动**：新建 `rdcore-audio`（capture trait + Null + real(CPAL/WASAPI)）、Opus encode/decode、新增音频 `ByteTransport` 通道、FFI + Flutter 绑定。
- **验收**：`cargo test -p rdcore-audio` + 同机听到声音。
- **依赖**：无（独立 crate，append-only）。

### A7. 带宽自适应
- **目标**：按 RTT/丢包动态调 fps/分辨率（首版做简单档位）。
- **依赖**：A1、A4；（RTP 可选）。

---

## 4. Track B 任务清单（kimi-k3）

### B1. TURN 配置与部署协调
- **目标**：公网 VPS 起 coturn；host/viewer 经 `RDCORE_TURN_*` 环境变量带 TURN 候选。
- **改动**：`RtcConfig::from_env` 的 TURN 校验（已具备）；提供 **coturn docker-compose 部署清单**；在 `connection_lifecycle` 确保收集 TURN 候选。
- **验收**：host(家庭 NAT) ↔ viewer(蜂窝) 经 TURN 连通。
- **依赖**：M0 服务器就位。
- **注**：TURN **服务器部署**本身是运维；B 负责配置与文档，A 拥有 `rdcore-rtc` 代码。

### B2. 信令公网部署 + TLS + 每会话 token
- **目标**：`signaling-svc` 部署公网，前置 gateway（caddy/nginx）终结 wss；把单共享 `auth_token` 改为"host 注册时生成**一次性 token** 绑定 session_id"模型。
- **改动**：`signaling-svc` 增 per-session token 校验；提供 Docker + caddy TLS 部署清单。
- **验收**：iPhone 经 wss 带 token 连上；无/错 token 拒。
- **依赖**：M0 敲定 token/session_id 方案。

### B3. 配对/发现流程（H 部分）
- **目标**：Windows host 生成配对码/二维码（session_id + 一次性 token）；iPhone 扫码/输入 → 带 token 连信令 → 建连。
- **改动（§8 归 B）**：新增 `pairing_*.dart`（UI + 调 FFI）；FFI `rdcore_*_pairing`（B 追加到 `rdcore-ffi`）；`rdcore-app` 暴露 `create_pairing()` 返回 session_id+token（**A 放宽 `Connection` 签名**，B 接）。
- **验收**：host 显示码 → iPhone 输入 → 自动连上出画面。
- **依赖**：A0（Connection 暴露 `create_pairing`）、B2（token）、B4（身份）。

### B4. 身份持久化接入 Connection（E，跨轨小协作）
- **目标**：配对重启不丢 TOFU 指纹；`Connection::new_host/new_viewer` 接 `PersistentIdentityStore`（基础设施 `rdcore-identity::persist` 已就绪，仅未接线）。
- **改动**：A 放宽 `Connection` 构造函数（注入 `Arc<dyn IdentityStore>` 或默认 Persistent）；B 在 `connection_lifecycle` 用 `PersistentIdentityStore::load_or_create`。
- **验收**：重启后同对端指纹一致、E2E 不重新告警。
- **依赖**：A0（Connection 签名放宽，注入 `Arc<Mutex<dyn IdentityStore>>`；基础设施 `rdcore-identity::persist` 已就绪且测试绿，仅缺「被 Connection 接收」）。

### B5. 自动重连 + 会话恢复（F）
- **目标**：网络抖动/切换后自动重连，恢复媒体+控制通道，重发 consent，用户无感。
- **改动**：`connection_lifecycle` 的 supervisor 加重连循环（经信令重新 offer/answer、重 attach 通道）；`Connection` 暴露 `reconnect()`（A 提供或 B 在 lifecycle 层做）。
- **验收**：连接中途断 WiFi→恢复，画面自动回来、输入仍可用。
- **依赖**：A0（`reconnect` 入口）、B2/B3（信令+配对）、A4（通道重建）。

### B6. 文件传输 + 剪贴板端到端（G）
- **目标**：Rust 状态机已就绪，补齐 FFI + Flutter。
- **改动（§8 归 B）**：FFI `rdcore_*_file`（B 加）、`file_transfer_*.dart`（B 建）、`clipboard_*.dart`（B 建）。
- **验收**：host↔viewer 互传文件/剪贴板。
- **依赖**：无硬阻塞，可并行。

---

## 5. 跨轨协调点（双方必须对齐）

1. **session_id + token 方案（M0 钉死，B2/B3 前置）**：
   - **session_id**：
     - 二进制类型：`SessionId(pub [u8; 16])` —— **16 字节**（128 bits），由 CSPRNG 生成（与 `flutter/lib/connection/demo_session.dart:_randomSessionId()` 现状一致，**勿改成 32 字节**）。
     - 展示/传输串：小写 hex 编码 → **32 字符** `[0-9a-f]`。⚠️ 注意：是"16 字节二进制 → hex 后 32 字符"，**不是 32 字节**；文档此前"32 字节 hex session_id"措辞即指此展示串，已纠正。
     - 用途：信令房间标识，出现在 `?session=<32hex>`。
   - **token（一次性配对 token）**：
     - 长度：**32 字节**（256 bits）CSPRNG。
     - 字符集：小写 hex 编码 → **64 字符** `[0-9a-f]`。
     - 生成：host 调 `Connection::create_pairing()` 时与 session_id 一同生成；token **不进 `Message` 线协议**，仅用于信令注册鉴权与 QR/输码展示。
     - 失效时机（钉死）：**建连即失效 —— 一次性、首次成功校验即焚**。即 viewer 首次携带 `?token=` 连信令并被服务器成功校验通过那一刻，该 token 立即标记 consumed、永不可再用（同一 viewer 重放亦拒绝）。
     - 配套 TTL：**自生成起 15 分钟绝对过期**，过期未用亦失效。→ 双重保险：重放不可行 + 僵尸 token 自清。
     - 重连例外：**B5 自动重连不依赖此 token**；重连复用持久化身份（TOFU）+ session_id，经身份签名重新鉴权，无需再次配对码。因此 token 一次性失效不影响 B5。
   - 以上格式为双方契约：A 的 `create_pairing()` 按此生成，B 的 signaling-svc(B2)/pairing UI(B3) 按此校验与展示；Dart 侧 `session_id` 展示串统一为 32 hex 字符。
2. **codec 协商字段**：A1 在 offer/answer 加 `video_codec` 可选字段 → §5 锁步。
3. **Connection 签名变更（= A0 任务）**：B3/B4/B5 需要 `Connection` 暴露 `create_pairing` / 接 `IdentityStore` / `reconnect` → §9 流程，A 改（`Arc<Mutex<dyn IdentityStore>>` + 两入口）、B 在 `connection_lifecycle` 同步接收。
4. **TURN 配置**：B1 与 A（rdcore-rtc owner）确认 `RtcConfig::from_env` 行为一致。
5. **RTP 优化（J）**：M4 由 A 在 `rdcore-rtc` 把 media 通道由 negotiated DataChannel 改 RTP `TrackLocalStaticSample`，**保持 `SocketMediaChannel`/`SocketDataChannel` 公共 API 不变** → B 不受影响。
6. **B6 变体追加锁步**：文件/剪贴板如需新 `Message` 变体，从下标 9 起（`FileTransfer=8` 已占）；B 追加后须在 `signaling.dart` 的 `MessageType` 末尾同步，并 §5 知会 A（契约 §5 Dart 锁步铁律）。

---

## 6. 关键决策与风险

- **首版媒体用 DataChannel 承载 H.264 字节**（A 可控，无需等 RTP），RTP(J) 作 M4 优化 → 避免 `rdcore-rtc` 改动阻塞关键路径。公网可用性与性能均达标。
- **对称 NAT 必须 TURN**：直连 P2P 仅优化；无 TURN 蜂窝↔家庭 NAT 大概率失败。
- **iOS 后台保活**：MVP 前台运行即可；长期加 VoIP/后台模式（不在本计划范围）。
- **webrtc-rs iOS 交叉编译**：A2 第一风险，M2 前必须验证。
- **codec 回退**：协商失败/旧端 → 回退 `Raw`（兼容），保证不崩。

---

## 7. 验收脚本（证明"完全真实跑通"）

M3 验收步骤（双方共跑）：
1. 公网 VPS：coturn + signaling(wss) 运行；host/viewer 设 `RDCORE_TURN_*`、`RDCORE_STUN`。
2. Windows：启动 Host Agent → 显示配对码。
3. iPhone（**关闭 WiFi 走蜂窝**）：输码/扫码 → 建连。
4. 观察：iOS 出 Windows 压缩画面（H.264，非 RGBA）；触摸/键盘控制生效；OS 横幅显示对端指纹+已连接。
5. 中间断 iPhone 网络 5s → 恢复后自动重连、画面回来。
6. 抓包确认：线上帧为 H.264，无未压缩 RGBA；TURN 中继上仅见 DTLS-SRTP 密文、无明文 RGBA（E2E 加密不依赖 TURN 层，TURN 只见密文）。
7. 重启 Windows Agent → 重新配对指纹一致，不重告警（身份持久化）。

---

## 8. 给 kimi-k3 的明确分工摘要

- **B 负责**：M0 部署 coturn+signaling(wss) + token 方案；B1 TURN 配置文档；B2 每会话 token+wss；B3 配对流程（FFI `rdcore_*_pairing` + `pairing_*.dart`）；B4 身份接入（`PersistentIdentityStore`，A 放宽签名）；B5 重连；B6 文件/剪贴板端到端。
- **B 不碰**：`rdcore-rtc` 内部、`rdcore-media`/`rdcore-encode`/`rdcore-decode`/`rdcore-capture`、`remote_screen.dart`、`rdcore_bindings.dart` 媒体部分。
- **B 与 A 的接口面**：仅 `Connection` 公共 API、`RTCPeerConnectionState`、§5/§9 锁步通道。
- **A 负责 A0（M0 首项）**：`Connection` 签名放宽——`Arc<Mutex<dyn IdentityStore>>` + `create_pairing()` + `reconnect()`，是 B3/B4/B5 的前置依赖。
