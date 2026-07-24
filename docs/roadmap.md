# 远程桌面控制系统 — 开发路线图

> 评估时间：2026-07-20（末次更新：2026-07-24 据二维码配对 UI 落地修订，落地提交 2026-07-22 `3d3fadc`）
> 范围：Rust 核心 + Flutter UI 远程桌面控制系统（双轨：A = WorkBuddy 媒体面 / B = kimi-k3 韧性面）
> 结论：协议 + 安全 + 跨平台骨架 + 媒体面 + 跨轨通道零冲突 + **重连/身份/文件·剪贴板/配对** **已全部闭环**；当前处于「同机可 demo」向「生产可用远程控制 + 真机验收」过渡阶段。WebRTC RTP(J) 也已落地。

---

## 1. 总体进度评分

| 维度 | 完成度 | 说明 |
|---|---|---|
| 协议 / 握手 / 签名（P0–P2） | 100% | Rust + Flutter 双端全绿 |
| 安全 / E2E 加密（P4–P5） | 100% | e2e 全绿，密钥零化 + 脱敏日志 |
| 媒体面 抓屏→编码→传输→解码→渲染→输入（Track A） | 100% | 真实 scrap 抓屏 + 真实 H.264 编解码 + 真实 WebRTC RTP 传输 + Flutter 双端测试通过 |
| WebRTC RTP 视频轨道（J, Track A） | 100% | `rdcore-rtc` real 后端 `h264_rtp.rs` 解包器(5 测) + `setup_video_track`/`push_video_frame`/`video_receiver`/`request_keyframe`(3 测)；e2e P2P 通过；owner=A |
| OS 不可伪造横幅 | 95% | Windows 编译 + 单测绿；沙箱无显示器未实跑 |
| §9 跨轨通道零冲突 | 100% | A 注入点 + B `supervisor::start` 注入均落地，21 测绿 |
| 心跳 / 活性判死（supervisor, B） | 100% | lifecycle 单测绿 |
| 重连 / 会话恢复（F, B） | 100% | `ConnectionSupervisor` 自动重连环（Dead→线性退避→`conn.reconnect()`）+ `max_reconnect_attempts`/`reconnect_backoff` 纯函数，13/13 测绿 |
| 身份持久化基础设施（B） | 100% | `rdcore-identity::persist` 已实现 + 单测绿，已接入 `Connection`（A0 放宽签名，B4 完成） |
| 身份接入 Connection（E / A0, A+B） | 100% | `new_host/new_viewer` 接受 `Arc<Mutex<dyn IdentityStore+Send+Sync>>` + `create_pairing()` + `reconnect()`/`establish()`；B4 接 `PersistentIdentityStore` 完成 |
| per-session token（B2, B） | 100% | `TokenStore::register` 已就绪，signaling-svc per-session token 库完成 |
| 配对 FFI + UI（B3, B） | 100% | 配对 FFI + Dart UI 完成 |
| 文件传输端到端（G, B+A） | 100% | Rust `file_transfer.rs` 状态机 + FFI `rdcore_*_file` + `file_transfer_*.dart` 完成（B6） |
| 剪贴板端到端（G, B） | 100% | FFI `rdcore_*_clipboard` + `clipboard_*.dart` 完成（B6） |
| 媒体健壮性测试 / CI / Runbook | 100% | 18 测绿、4 平台 CI、Runbook 就绪 |
| 音频（C, A） | 100% | Rust `rdcore-audio`(capture/encode/decode 抽象 + Raw 直通 + `real` 下 CPAL/Opus) + 独立 `AudioChannel` 泛型字节管道 + WebRTC `audio` DataChannel(id=2) + FFI `RdAudioFrame`/`pull_audio` + Flutter `RdAudioFrame`/控制器音频循环(静音·音量·电平)/RemoteScreen 音频控件（C1-C6 + Flutter 音频 UI 全绿） |
| 跨平台真机（I, A） | 30% | Windows headless 通过；iOS staticlib 构建脚本已修（命名/路径对齐 pbxproj），macOS/iOS 仅 staticlib 未真机验；Linux 未构建 |
| 生产级 UI（H） | 95% | 主壳完成：HomeScreen 会话列表/多会话/设置页 + 真实 WebSocket 信令配对；**二维码配对 UI 已完成**（2026-07-22 `3d3fadc`：Host `QrImageView` 展示 + 刷新/撤销/退出失效，Viewer 扫码 + 输码双路径）；NFC 配对待做 |
| NAT / TURN 穿透（D, B） | 20% | 仅 localhost 验证，无 TURN 部署 |
| Windows Host Agent 打包（A5, A） | 100% | 纯 Rust 服务（`rdcore-desktop`）：`run`/`install`/`uninstall`/`service` 子命令；`create_pairing()`→同机 token 文件注册（B2 对接点）→`Connection::new_host`→`establish`(等 Viewer Offer)→`start_capture`(factory，专用线程承载 `!Send` 捕获源)→输入注入→`rdcore-banner` 横幅推送；默认 + `service` feature 均 `check`/`test`/`clippy -D warnings` 全绿，7 单测通过 |

**加权判断**：
- 「骨架 + 同机 demo + 韧性闭环」≈ **90%**（核心链路齐全、可编译可测、跨轨无冲突、断网可恢复、文件/剪贴板/配对可用）。
- 「生产可用远程控制」≈ **85%**（跨网/真机 UI 仍有缺口；A5 Host Agent、H 生产 UI 主壳与二维码配对、C 音频管线已完成）。
- 真实体验最大空白：**跨网 NAT（D）/ 真机 UI 验收（I）/ NFC 配对（H 唯一剩余）**（C 音频管线、二维码配对已完成）。

---

## 2. 进度记分卡（按能力域）

| 能力域 | 状态 | 验证 | 归属 |
|---|---|---|---|
| 协议 / 握手 / 签名 | ✅ 完成 | Rust+Flutter 绿 | A+B 共建 |
| 安全 / E2E | ✅ 完成 | e2e 绿 | A |
| 媒体面全链路 | ✅ 完成 | Rust+Flutter 双端绿 | A |
| WebRTC RTP 视频轨道（J） | ✅ 完成 | h264_rtp 解包 + RTP track，e2e 绿 | A（owner） |
| OS 不可伪造横幅 | ✅ 完成(未实跑) | 编译+单测绿 | A |
| §9 跨轨零冲突 | ✅ 完成 | 21 测绿 | A+B |
| 心跳 / 判死 | ✅ 完成 | lifecycle 绿 | B |
| 重连 / 会话恢复（F） | ✅ 完成 | 13/13 测绿 | B |
| 身份持久化基础设施 | ✅ 完成 | persist.rs 单测绿 + 接入 | B |
| 身份接入 Connection（E / A0） | ✅ 完成 | create_pairing 等 25 测绿 | A+B |
| per-session token（B2） | ✅ 完成 | TokenStore::register | B |
| 配对 FFI + UI（B3） | ✅ 完成 | flutter 18/18 | B |
| 文件传输状态机 + 端到端（G） | ✅ 完成 | Rust+FFI+Dart | B(+A) |
| 剪贴板端到端（G） | ✅ 完成 | FFI+Dart | B |
| 音频（C） | ✅ 完成 | Rust `rdcore-audio`+`AudioChannel`+WebRTC audio DC(id=2)+FFI+Flutter 音频 UI | A |
| 跨平台真机（I） | 🟡 部分 | Windows headless；iOS staticlib 脚本已修 | A(+硬件) |
| 生产 UI（H） | ✅ 主壳完成 | 21 测绿 + 3 skip；analyze 0 错 | A（+B3 配对 UI 复用） |
| NAT/TURN（D） | 🟡 localhost | — | B(+部署) |
| Windows Host Agent（A5） | ✅ 完成 | check/test/clippy 全绿，7 单测 | A（纯 Rust 服务） |

---

## 3. 分阶段开发计划

按「让连接真能用 → 让控制名副其实 → 体验增强 → 生产化 → 跨网跨平台」递进。每阶段标注归属、关键交付、依赖与完成标准。

### Phase 1 — 韧性闭环（最高优先级，B 主导）✅ 已完成
**目标：从 demo 变 usable，断网/重启不再丢会话。**

- **F 自动重连 + 会话恢复** ✅：`ConnectionSupervisor` 自动重连环（Dead→线性退避→`conn.reconnect()`），`max_reconnect_attempts`/`reconnect_backoff` 纯函数，13/13 测绿。
- **E 身份接入 `Connection`** ✅：A 放宽 `new_host/new_viewer` 签名（A0），B 接入 `PersistentIdentityStore` + `SupervisorConfig::identity_dir`（B4）。
- **完成标准** ✅：同机拔网 10s 内自动恢复画面；进程重启后 TOFU 指纹不丢、无需重新确认。

### Phase 2 — 功能完整性（B 主导，A 配合 FFI 签名）✅ 已完成
**目标：远程控制「名副其实」。**

- **G 文件传输端到端** ✅：FFI `rdcore_*_file` + `file_transfer_*.dart`（B6），复用 `file_transfer.rs` 状态机。
- **G 剪贴板端到端** ✅：FFI `rdcore_*_clipboard` + `clipboard_*.dart`（B6），经 §9 broadcast 业务通道路由。
- **完成标准** ✅：同机 P2P 下双向传文件、同步剪贴板，受 `ConsentScope` 门控。

### Phase 3 — 体验增强（A 主导）
**目标：从「能看」到「好用」。**

- **C 音频管线**（✅ 已完成，本会话）：`rdcore-audio`（capture/encode/decode 抽象 + Raw 直通 + `real` feature 下 CPAL/Opus）+ 独立 `AudioChannel` 泛型字节管道 + WebRTC `audio` DataChannel(id=2) + FFI `RdAudioFrame`/`attach_loopback_audio`/`pull_audio` + Flutter `RdAudioFrame` 模型/控制器音频循环(静音·音量·电平)/RemoteScreen 音频控件。零侵入视频/输入管线，对 B 零影响。
- **J WebRTC 媒体改 RTP**（✅ 已完成，本会话）：`h264_rtp.rs` 解包器 + `setup_video_track`/`push_video_frame`/`video_receiver`/`request_keyframe`；`rdcore-rtc` owner=A，对外 `channels()` 签名未变，对 B 零影响。
- **完成标准**：同机可听可看可操作；高帧率下 CPU/延迟更优。

### Phase 4 — 生产化 UI（A+B 共建）
**目标：非开发者也能完成一次远程连接。**

- **H 真实配对流程**：二维码配对（✅ 2026-07-22 已落地）/ NFC 配对（待做）、连接管理、设置页、多会话。
- A 拥有主壳与 `remote_screen`；B 拥有 `file_transfer_*.dart` / `clipboard_*.dart` 区块（§8 owner 划分）。
- **完成情况（2026-07-20）**：H 主壳已完成 —— `ConnectionManager`（多会话增删查 + `AppSettings` 配置）、`HomeScreen`（会话列表/阶段芯片/进入·删除/新建连接 FAB/本地演示/双栏演示入口）、`SettingsScreen`（设备名/信令基址/默认权限范围）、`WebSocketSignaling`（真实 `dart:io` WebSocket 信令，URL `ws(s)://host/<hex>?token=...` 匹配 signaling-svc 路径解析）、`main.dart` 接入新主壳；`flutter analyze` 0 错、`flutter test` 21 passed + 3 skipped（原生 FFI 集成测试因沙箱缺 VC++ 运行库优雅跳过）。**二维码配对 UI 已于 2026-07-22 完成**（`3d3fadc`：`PairingPage` Host 端 `QrImageView` 二维码展示 + 刷新（重新发布、旧码即失效）/ 取消配对（撤销发布）/ 退出页面自动失效 / 整卡点击复制；Viewer 端 `mobile_scanner` 扫码连接 + 输码连接双路径，均可解析 `<32hex>:<64hex>` 配对码）；NFC 配对 UI 为唯一剩余子项。
- **完成标准**：从安装到建立连接无需开发者介入（输码与扫码配对路径均已达成；NFC 待补）。

### Phase 5 — 跨平台真机 + 部署（A 主导 + 运维）
**目标：跨网络、跨 OS 可用。**

- **I 跨平台真机**：macOS/iOS 真机集成（staticlib 已产出、构建脚本命名/路径已修对齐 pbxproj）、Linux 构建、OS 横幅各平台原生实现实跑。
- **A5 Windows Host Agent**（✅ 已完成，纯 Rust 服务）：被控端服务（`rdcore-desktop` 二进制 `run`/`install`/`uninstall`/`service`）。`create_pairing()` 拿 session/token → 写 `SIGNALING_TOKEN_DB` 同机 token 文件（B2 对接点，signaling-svc 每次握手前 `reload_from_file` 刷新）→ `Connection::new_host` → `establish`(Host 侧阻塞等 Viewer Offer) → `start_capture`(factory 形式，专用 OS 线程承载 `!Send` 的 `ScrapCaptureSource`) → `EnigoInputInjector` 输入注入 → `rdcore-banner` 横幅推送（独立进程 + UDP 回环）。默认 + `service` feature 均 `check`/`test`/`clippy -D warnings` 全绿，7 单测通过。
- **D NAT/TURN**：TURN 中继部署；`signaling-svc` 生产化（鉴权/TLS/持久化/横向扩展）。
- **完成标准**：跨网络（非同机）可连；多 OS 客户端可用。

---

## 4. 跨轨依赖与风险

1. **E 身份接入（✅ 已闭环）**：A 放宽 `Connection` 构造签名（A0），B 接 `PersistentIdentityStore`（B4）。
2. **J WebRTC RTP（✅ 已闭环，owner=A）**：§8 未显式列，经双轨对齐确认 `rdcore-rtc` 归 A 维护；B 的 lifecycle/supervisor 仅经 `Connection` 公共 API 与 `RTCPeerConnectionState` 观测，不依赖 `channels()` 返回类型，零影响。
3. **`Message` 变体锁步（§5）**：未来新增业务变体仍须尾部追加 + 同步 Dart `MessageType`。
4. **OS 横幅实跑（I）**：沙箱无显示器，Windows 原生窗口未实跑；各平台原生实现需真机验证。
5. **双脚本漂移（✅ 已解决）**：删除 `flutter/tool/build_ffi.sh` 副本，统一以 `tool/build_ffi.sh`（Track A 维护，§8）为准。
6. **A5 Host Agent 形态（✅ 已闭环）**：采纳 kimi-k3 建议**纯 Rust 服务**（`rdcore-desktop` 二进制）。已落地：`run`/`install`/`uninstall`/`service` 子命令；信令复用 `rdcore-signaling::SignalingClient` + `rdcore-app::Connection`；配对用 `Connection::create_pairing()` 拿 token 写同机 `SIGNALING_TOKEN_DB`（B2 对接点，signaling-svc 每次握手前 `reload_from_file` 刷新）；横幅复用 `rdcore-banner`（独立进程 + UDP 回环推送，Host 仅作推送方）。默认 + `service` feature 均 `check`/`test`/`clippy -D warnings` 全绿。
7. **许可红旗（⚠️ 发布阻断，用户暂缓）**：全 workspace 仍有 15 个 crate 是 `AGPL-3.0-only`（含 `rdcore-crypto`/`signaling-svc`/`rdcore-rtc`/`rdcore-app`/`rdcore-ffi` 自身），与闭源 Windows Agent / iOS App 目标冲突（GPL 传染性）。动打包前必须解决。

---

## 5. 本周立即行动

- **A**：✅ **Phase 3 C（音频管线）已完成**（见 §3 / §4）——Rust C1-C6 + Flutter 音频 UI 全绿。
- **A**：✅ **A5 Host Agent 形态已拍板（纯 Rust 服务）并完成**（见 §4 风险 #6 / Phase 5）。下一步启动 **C 音频管线**。
- **B**：Phase 1/2 与 B1–B6 已全部完成；可进入 **D NAT/TURN 部署** 准备。

---

## 6. 验证状态说明

- 已验证：§9 双轨闭环；`cargo test --workspace` 全绿（A 侧：媒体/J/A0；B 侧：F/E/G/B2/B3/B4/B6 共 55 ok；flutter 21 passed + 3 skipped（原生 FFI 集成测试因沙箱缺 VC++ 运行库优雅跳过），`flutter analyze` 0 错）。
- CI 门禁：`cargo clippy --workspace --all-targets -- -D warnings` 全绿（已修 rdcore-banner 3 处 `field_reassign_with_default`、rdcore-rtc `needless_update`）。
- 未实跑项：OS 横幅原生窗口、iOS 真机交叉编译（沙箱无 Mac/iOS SDK）、跨网 TURN。
