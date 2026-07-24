# 媒体面架构与 Runbook（Track A）

> 本文档描述远程桌面控制系统的**媒体面**（Track A）架构、线格式、测试与运维方式。
> 控制面 / 韧性面（Track B：连接生命周期、文件传输、剪贴板、身份持久化）见 `docs/双轨开发契约.md`。

---

## 1. 三条独立通道（架构基线）

一条远程桌面连接被拆成三条互不干扰的通道（详见契约 §1/§5）：

| 通道 | 承载内容 | 实现位置 | 云端可见？ |
|---|---|---|---|
| **信令通道** | SDP / ICE（Offer/Answer/Ice） | `rdcore-signaling` + 真实 `signaling-svc`（WebSocket） | 可见（仅元数据） |
| **媒体通道** | 屏幕视频流（`MediaFrame`） | `rdcore-media` `MediaChannel` | 不可见（端到端加密后走 P2P） |
| **数据通道** | 输入 / 剪贴板 / 心跳等控制流量 | `rdcore-media` `DataChannel` | 不可见 |

> 云端控制面**永远看不到媒体或输入内容**——这是架构红线。视频与输入仅经 P2P（WebRTC / 本地 TCP 回环）直连两端。

---

## 2. 线格式（framing）与护栏

媒体/数据通道不直接碰网络，只收发"已帧化的字节"。帧格式：

```
[ 4 字节小端长度前缀 ][ postcard 序列化负载 ]
```

- **自定界**：长度前缀让帧在流式（TCP）或数据报传输上都能精确还原每一帧。
- **F3 限长护栏**（防分配炸弹）：收发两侧都强制最大长度上限。
  - `MAX_MEDIA_FRAME_LEN = 64 MiB`（Raw 1080p ≈ 8 MiB，留足余量）
  - `MAX_DATA_FRAME_LEN = 8 MiB`（容纳剪贴板 `MAX_CLIPBOARD_SIZE = 5 MiB` + 开销）
- 越界 / 超长 / 长度与负载不符一律返回 `Err`（见 `rdcore-media` 单测 `frame_decode_rejects_*`）。

> `MediaFrame` 是共享契约类型（`rdcore_proto::MediaFrame`，带 `Serialize/Deserialize`）。
> 媒体通道 crate 不反向依赖 loopback 假实现；线格式统一，泛型 postcard 用 `postcard` crate 直编。

---

## 3. 媒体管线（抓屏 → 渲染 → 输入）

```
Host 侧：CaptureSource → (H.264) Encode → E2E AEAD → RTP 视频轨道（生产路径）
                                    ↘ 回退：media DataChannel（旧端 / Web Viewer）
                                    ↘ DataChannel.send(InputEvent) ← Viewer 侧输入
Viewer 侧：RTP 重组 / MediaChannel.recv_frame → E2E 解密 → Decode → Render → RemoteScreen
```

> **视频走 RTP 轨道（gap J 生产路径）**：整帧（postcard `MediaFrame`，像素已 AEAD）
> 经 `rdcore-rtc::video_rtp` 分片逐包发出（`TrackLocalStaticRTP`）、对端抗丢包重组——
> 丢包经「Viewer 关键帧请求 + 周期 IDR 兜底」快速自愈（见本节末编码策略），消除了
> 可靠有序 SCTP 的队头阻塞与发送缓冲延迟累积。接收端队列为直播语义（`LatestFrameQueue`，
> 满载丢最旧）。
> 两端按 Offer/Answer SDP 的视频 m-line 自动协商（`sdp_has_active_video`）；旧端
> （Offer 无视频 m-line，如 Web Viewer）自动回退 `media` DataChannel，Viewer 侧另有
> 3 秒首帧兜底（协商看似激活但一帧未收即回退）。环境变量 `RDCORE_VIDEO_TRANSPORT=dc`
> 可强制 DataChannel 路径（排障 / 对照实验）。
>
> **DC 回退路径的发送侧丢帧**（web 端 / 旧 Viewer 走这条路径）：DataChannel 可靠有序，
> 发送缓冲一旦积压，缓冲里全是过期画面，延迟单调累积。`Connection::send_media` 发送前
> 检查 media 通道 `buffered_amount`，超过阈值（默认 256 KiB，`RDCORE_DC_MAX_BUFFERED_KB`
> 可调）直接丢帧不发送——SCTP 流控会把链路慢 / 对端消费慢都反映成发送缓冲上涨，
> 从源头把延迟钉在「阈值 / 链路带宽」以内；丢帧同时登记关键帧请求，下一帧即强制
> IDR，Viewer 最坏滞后一帧自愈。
>
> **编码码率随帧率缩放**（`rdcore-encode` ffmpeg 硬编路径）：码率与 fps 成正比：
> `宽×高×4 bits × fps/30`（基线 30fps ≈ 0.133 bit/像素/帧），钳制 [1, 10] Mbps
> （上限对齐 DataChannel 回退路径 ~9.7 Mbps 吞吐天花板）。不随 fps 缩放时 60fps
> 每帧预算减半、画面明显变糊。`RDCORE_VIDEO_BITRATE_KBPS` 可显式覆盖（弱上行 /
> 经 TURN 时调低保帧率）。P 帧流（2026-07-24 起按需 IDR）下同码率画质显著优于
> 旧「逐帧强制 I」策略，且编解码开销大降（软解端 Flutter/CLI 流畅度关键）。

- **捕获**：`rdcore-capture`（`CaptureSource` / `HostInputInjector` trait 接缝）。
  - `NullCaptureSource` / `NullInputInjector`：headless 实现（测试 / 无显示器）。
  - `real` feature：`ScrapCaptureSource`（scrap 抓主显示器 BGRA→RGBA）、`EnigoInputInjector`（enigo 注入键鼠滚轮）。
- **编解码**：`rdcore-encode` / `rdcore-decode`。生产路径为 H.264（NVENC/QSV/AMF 硬编
  优先、openh264 软编兜底）。**按需 IDR + P 帧流**（2026-07-24 起）：编码器启动连发
  3 帧 IDR（防首帧被丢整段不可解），之后仅在「被请求 / 周期兜底（硬编 1s、软编 2s）」
  时出 IDR，逐帧前置 SPS/PPS；Host 侧任何丢帧（DC 缓冲 / 泵背压）自动补 IDR，
  Viewer 解码错误/积压也会发 `AppMessage::RequestKeyframe`（PLI/FIR 语义）主动请求——
  丢帧/花屏典型 <100ms 自愈，最坏一个周期 IDR 兜底。
  Raw（未压缩）仅用于回环 / 单测。
- **渲染**：`rdcore-render` 输出 `Frame`（RGBA 像素），供 Flutter `RemoteFrameView` 显示。
- **输入**：Viewer 捕获鼠标/滚轮/键盘 → `RdInputEvent`（`*const` 结构体，非 JSON）→ `DataChannel` → Host `poll_input`。

---

## 4. 可插拔传输后端（`ByteTransport`）

`MediaChannel` / `DataChannel` 只关心"收发 `MediaFrame` / `Message`"，字节走哪条线由 `ByteTransport` 决定：

| 后端 | 类型 | 用途 |
|---|---|---|
| `InMemoryTransport` | 进程内 mpsc | 回环 / 单测 / P1 假传输 |
| `TcpTransport` | 真实 localhost TCP | P7 起媒体/数据真正走网络（非进程内占位） |
| WebRTC DataChannel / RTP | `rdcore-rtc` `real` 后端 | 真实 P2P（同机 localhost 已实测） |

> **加新后端只需实现 `ByteTransport` 一个 trait**，媒体/数据通道与上层一行都不用改。

---

## 5. 本地跑通（开发 / 联调）

### 5.1 回环联调（headless，最快）
```bash
# Rust 单测：媒体 + 输入全链路（含真实 Raw 编解码无损往返）
cargo test -p rdcore-app
cargo test -p rdcore-media
# FFI 媒体/输入桥接单测
cargo test -p rdcore-ffi
```

### 5.2 真实 WebRTC（同机 P2P）
```bash
# 1) 启动信令服务（WebSocket）
cd cloud/crates/signaling-svc && cargo run
# 2) 跑 P2P e2e（经真实 signaling-svc + 真实 WebRTC，同机回环）
cd core/crates/rdcore-rtc && cargo test --features real --test e2e_real_p2p
```
> 同机直连需关 mDNS：`SettingEngine::set_include_loopback_candidate(true)` + `set_ice_multicast_dns_mode(Disabled)`。

### 5.3 Flutter 端到端
```bash
cd flutter && flutter test            # 含真实 FFI 媒体端到端
cd flutter && flutter analyze         # 0 问题门禁
```
> 关键坑：`testWidgets` 下 `InMemorySignaling` 的 FakeAsync 会吞掉握手级联消息，
> 须把整条握手 + 渲染 + 输入捕获包进 `tester.runAsync(() async {...})`；
> `RemoteFrameView` 须 `size: Size.infinite` 填满父容器才能接收指针。

---

## 6. 健壮性测试清单（已落地）

`rdcore-media` 单测覆盖：

- **F3 护栏**：截断帧、超长长度前缀、零长脏负载、脏 postcard、超上限编码 —— 全部拒绝。
- **保序 / 吞吐**：200 帧进程内无损往返且保序。
- **大帧**：1024×768 RGBA（~3 MiB）进程内 / 真实 TCP 无损往返。
- **关闭语义**：发送端关闭后接收端返回 `Ok(None)`，不挂起。
- **丢帧容忍**：传输层确定性丢帧（每 3 丢 1），通道不崩溃、收完剩余帧。

新后端 / 新编解码器接入时，复用上述 `media_channel_pair` / `tcp_channel_pair` + `RawEncoder/Decoder` 夹具即可。

---

## 7. 如何扩展

### 加一个新传输后端
实现 `ByteTransport`（`send_bytes` / `recv_bytes`），返回 `[4字节长度][postcard]` 字节；
用 `SocketMediaChannel::new(backend)` / `SocketDataChannel::new(backend)` 装配即可。

### 加一个新编解码器
1. `rdcore-proto` 的 `VideoCodec` 枚举**追加变体到末尾**（下标不可重排）；
2. 在 `rdcore-encode` / `rdcore-decode` 加对应 `Encoder` / `Decoder`；
3. 单测覆盖往返无损。

### 加一个新平台
1. `rdcore-ffi` 编译目标静态库（`cargo build -p rdcore-ffi --target <triple>`）；
2. Flutter `rdcore_bindings.dart` 保持 C ABI 镜像（8 个媒体/输入符号 + 加密符号）；
3. `tool/build_ffi.sh`（Track A 维护，B 直接调用）按需扩展目标。

---

## 8. 已知缺口（媒体面以外，待 Track B / 后续阶段）

- **音频**：完全未开发（缺口 C）。
- **RTP 迁移**：桌面端视频已走 RTP 轨道（gap J 已落地，见 §3）；Web Viewer 仍走
  `media` DataChannel 回退（浏览器拿不到自定义 RTP 载荷），由发送侧 `buffered_amount`
  丢帧控制延迟（见 §3）。
- **NAT/TURN**：仅 localhost 验证，无 TURN 中继部署（缺口 D）。
- **生产 UI / 跨平台真机 / 重连 / 身份持久化 / 文件·剪贴板端到端**：见 `docs/双轨开发契约.md`。
