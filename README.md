# 远程桌面控制系统 · Remote Desktop Control System

基于 **Rust 核心 + Flutter UI** 的远程桌面控制系统。传输层采用 WebRTC，屏幕与键鼠数据走
端到端加密的 P2P 通道；云端仅承担**控制平面**（身份、会话、权限、审计等元数据），
不接触任何屏幕或输入内容。

## 核心特性

- **端到端加密传输**：媒体经 DTLS-SRTP、DataChannel 经 DTLS 加密；P2P 优先，TURN 中继仅作兜底且只见密文。
- **设备身份与防冒充**：每台设备持有 Ed25519 密钥对，首连通过带外方式（扫码 / 输码）核对 SHA256 指纹，防止信令劫持与 MITM。
- **安全同意模型**：连接需 Host 用户同意（交互模式）或设备预授权 + 临时 PIN（无人值守模式）；Host 端常驻由独立高权限进程绘制的**不可伪造横幅**，可随时终止会话。
- **完整媒体管线**：屏幕捕获 → H.264 编码（GPU 硬编优先：NVENC/QSV/AMF，失败自动回退 openh264 软编）→ WebRTC 传输 → 解码 → 原生纹理渲染（像素不经过 Dart），另含音频与文件 / 剪贴板传输。
- **会话韧性**：心跳判死、断线自动重连与会话恢复、身份持久化。
- **单 Cargo workspace**：`shared/` + `core/` + `cloud/` 统一构建、统一测试。

## 架构概览

- **传输通道**：单个 WebRTC PeerConnection 复用 1 路 RTP 视频轨道与 3 条协商式 DataChannel
  —— `media`（id=0，媒体兜底）、`control`（id=1，输入 / 剪贴板 / 心跳）、`audio`（id=2）。
- **信任模型**：两端（Viewer / Host）为可信端点；云为**半可信**，只能看到"谁连谁"的元数据。
- **云端控制平面**：API Gateway、Authentication、Device Registry、Signaling、Permission、Audit 六个服务，
  信令服务仅转发 SDP/ICE，不参与视频转发。

详见 [`docs/架构图/架构说明.md`](docs/架构图/架构说明.md) 及同目录 4 张架构图（拓扑与信任边界、
媒体管线、传输通道分工、PeerConnection 内部结构）。

## 仓库结构

| 目录 | 职责 |
|------|------|
| `shared/` | 客户端与云端共用的 Rust 库：`rdcore-proto`（线路协议）、`rdcore-identity`（设备身份）、`rdcore-crypto`（加密原语） |
| `core/` | 客户端 Rust 核心（`core/crates/`）：捕获、编解码、WebRTC、会话编排、FFI、受控端 Host Agent（`rdcore-desktop`）等 |
| `flutter/` | Flutter UI（Viewer / Host / 配对），经 `dart:ffi` 调用 Rust 核心（`rdcore-ffi`） |
| `cloud/` | 云端控制平面（`cloud/crates/`）：gateway / auth / registry / signaling-svc / permission / audit |
| `cloud/deploy/` | 公网部署（docker compose：coturn + signaling-svc + caddy），见 [`cloud/deploy/README.md`](cloud/deploy/README.md) |
| `docs/` | 架构图、路线图、媒体管线与验收文档 |
| `scripts/` | 构建、打包（NSIS 安装器）、pub 镜像等脚本 |
| `tool/` | 开发工具（FFI 构建、Flutter 配置同步） |
| `.github/workflows/` | CI 与发布工作流 |

桌面客户端 = `core/`（Rust 核心）+ `flutter/`（Dart UI），由 `rdcore-ffi` 以 C ABI 桥接；
`cloud/` 为独立部署的服务端。根 `Cargo.toml` 将三者纳入同一 workspace。

## 环境要求

- **Rust**：stable（`rust-toolchain.toml` 已固定，`rustup show` 自动安装，含 rustfmt / clippy）
- **Flutter**：3.44.6（`.fvmrc` 已固定，建议使用 FVM）
- **just**（可选）：任务运行器，封装常用构建命令
- **Python 3**：配置同步脚本（`tool/sync_flutter_config.py`）。
  Windows 下 justfile 通过 `py -3` 启动器调用（需安装真实 Python 3）；
  注意系统自带的 `WindowsApps\python3.exe` 商店占位程序不是真 Python，
  会报 exit code 49，可在「应用执行别名」中关闭
- Windows 安装器打包另需 MSVC 构建工具 + NASM + NSIS（见 `scripts/build_installer_nsis.sh`）
- **FFmpeg 7.1.2 低驱动地板开发库**（可选，仅 `--features hwcodec` 硬件编码需要）：
  `scripts\build_ffmpeg_lowfloor.ps1` 一键构建，`FFMPEG_DIR` 指向产物目录，详见
  [`docs/ffmpeg_hw_lowfloor.md`](docs/ffmpeg_hw_lowfloor.md)
  > hwcodec 下 `rdcore_ffi.dll` 在**运行期**动态依赖 FFmpeg DLL
  > （`avcodec-61.dll` / `avdevice-61.dll` / `avutil-59.dll` 等），
  > 必须与 `rdcore_ffi.dll` 放在同一目录，否则 App 启动即报
  > `RdCoreNativeUnavailable: 原生库未加载`。
  > `just build-ffi windows`（`tool/build_ffi.sh`）会自动从
  > `scripts/installer/dist/` 同步这些 DLL 到 `flutter/windows/rdcore/`，
  > 打包时由 CMake install 一并带入 bundle。

## 快速开始

### 受控端（Host，被远程控制的一方）

```bash
# 桌面客户端 Rust 核心（受控端 Host Agent，跨平台）
just build-client          # = cargo build --release -p rdcore-desktop
```

生成 Windows Host（受控端）.exe 安装包：`just installer-win`，产物路径为
`scripts/installer/RdCore-Host-Setup.exe`。

**macOS Host【开发中】**：同一 crate 直接编译即得 macOS 可执行文件，抓屏（Core Graphics）与
输入注入（CGEvent）开箱可用；首次运行需在 系统设置 → 隐私与安全性 中授予
「屏幕录制」与「辅助功能」权限。

```bash
# 前台运行（生成配对码 / 终端二维码，Viewer 扫码连接）
./target/release/rdcore-desktop run

# 常驻（launchd LaunchAgent，用户登录后自启；权限随用户会话授予）
./target/release/rdcore-desktop install    # 写 ~/Library/LaunchAgents 并 launchctl load
./target/release/rdcore-desktop uninstall  # launchctl unload 并删除 plist
# 服务化子命令需先以 `cargo build --release -p rdcore-desktop --features service` 构建

# 打包为 .app bundle（解决「弹了屏幕录制提示但设置里没有」：授权按 bundle 记，不挑终端）
just package-mac   # 产物 scripts/installer/RdCore Host.app，双击 / open 运行
```

### 控制端（Viewer，发起远程控制的一方）

Flutter UI（含 Viewer / 配对界面，经 `dart:ffi` 调用 Rust 核心）：

```bash
cd flutter && flutter pub get && flutter analyze
```

iOS 需先在 macOS 上构建 Rust FFI 静态库，再构建 Flutter 应用：

```bash
just build-ffi-ios         # 生成 flutter/ios/rdcore/librdcore_ffi.a
just build-flutter ios     # flutter pub get && flutter build ios
```

Flutter UI 支持的平台（均需对应平台的 `rdcore_ffi` 库就位后才能链接成功）：

| 平台 | 目录 | FFI 前置（首次 / 改过 Rust 后） | 构建命令 |
|---|---|---|---|
| iOS | `flutter/ios/` | `just build-ffi-ios`（需 macOS） | `just build-flutter ios` |
| Android | `flutter/android/` | 无需构建（已随附预编译 `jniLibs/<abi>/librdcore_ffi.so`，覆盖 arm64-v8a / armeabi-v7a / x86_64；改过 Rust 后用 `just build-ffi android` 重新生成，需 cargo-ndk + `ANDROID_NDK_HOME`） | `just build-flutter apk` |
| Windows 桌面 | `flutter/windows/` | `just build-ffi windows` | `just build-flutter windows` |
| macOS | `flutter/macos/` | `just build-ffi macos`（需 macOS） | `just build-flutter macos` |
| Linux | `flutter/linux/` | `just build-ffi linux` | `just build-flutter linux` |

> `just build-flutter <target>` 内部已包含 `cd flutter && flutter pub get`，
> 请在仓库根目录执行（just 需在 justfile 所在目录运行）。

> 国内网络访问不了 pub.dev 时（`flutter pub get` 报 socket error / exit code 69），
> 改用 `just build-flutter-cn <target>`：它会自动注入 flutter-io.cn 镜像源
> （`PUB_HOSTED_URL` / `FLUTTER_STORAGE_BASE_URL`）后再构建。

> 注意：`just build-ffi <target>` 生成的是**本机平台**的原生库，因此 macOS / Linux
> 的 FFI 前置必须在对应系统的机器上执行；Windows 桌面已随附 `rdcore_ffi.dll`。

浏览器端（Web 控制端 Viewer，独立于 Flutter，Rust 编到 WASM + Vite/TS 前端）：

| 平台 | 目录 | 构建命令 |
|---|---|---|
| 浏览器（Web Viewer） | `web/` | `cargo build --release --target wasm32-unknown-unknown -p rdcore-web` → `wasm-bindgen --target web --out-dir web/rdcore-web/pkg target/wasm32-unknown-unknown/release/rdcore_web.wasm` → `cd web/app && npm install && npm run dev`（详见 [`web/README.md`](web/README.md)） |

参考演示地址：https://8.138.237.243

### 云端控制平面

```bash
# 云端控制平面（6 个服务）
just build-cloud
```

## 测试与质量检查

```bash
just test                  # cargo test --workspace
just ci                    # fmt --check + clippy -D warnings + 配置同步检查
```

`rdcore-viewer-cli` 提供无显示器的 headless 验收：用真实信令服务器 + 真实 WebRTC（localhost 回环）
自动跑通完整握手（Ed25519 验签 → ICE → E2E 会话密钥 → 同意门控）并验证加密媒体 / 控制往返，
用于 CI 环境证明 Host↔Viewer 链路。

## 配置单一数据源

ICE / 信令默认值的唯一来源是 `core/crates/rdcore-desktop/src/config.rs`，
`flutter/lib/models/default_config.dart` 由脚本生成、**不要手改**：

```bash
just sync-config           # 重新生成 Dart 默认值
just check-config          # CI 守卫：生成物过期或与 config.rs 不一致即失败
```

## 文档

- 架构说明与架构图：[`docs/架构图/架构说明.md`](docs/架构图/架构说明.md)
- 媒体管线：[`docs/media_pipeline.md`](docs/media_pipeline.md)
- FFmpeg 低驱动地板硬编（构建 / 接入 / NVENC 排错实录）：[`docs/ffmpeg_hw_lowfloor.md`](docs/ffmpeg_hw_lowfloor.md)
- 公网部署（coturn / 信令 / TLS）：[`cloud/deploy/README.md`](cloud/deploy/README.md)

