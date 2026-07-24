# iOS 集成 Rust 核心（rdcore-ffi）指南

本文件说明如何把远程桌面控制系统的 Rust 核心（`rdcore-ffi` 的 C ABI）编译为 iOS
静态库并接入 Flutter/iOS App，使 **iPhone/iPad Viewer → Windows Host** 链路可在真机跑通。

> 背景：iPhone 是 **Viewer**（拉屏 + 收键鼠），不需要抓屏/注入；Windows 是 **Host**
> （`rdcore-desktop` 用 `real` feature 直接调 `rdcore-app`，真实 DXGI 抓屏 + Enigo 注入）。
> 因此 iOS 侧只需编进 webrtc-rs + H.264 解码 + 渲染，不依赖 scrap/enigo。

## 前置条件（必须在本机满足）

- **macOS + Xcode**（含 Command Line Tools）：提供 `xcrun` / `clang` / `lipo`。
- **Rust 工具链**（`rustup`）：本脚本会自动 `rustup target add` iOS 目标。
- 已执行过 `cd flutter && flutter pub get`（生成 `Flutter/Generated.xcconfig`）。

> ⚠️ 当前沙箱（Linux）无 Xcode，无法实际编译验证；以下步骤需在 macOS+Xcode 执行。

## 步骤 1：编译 iOS 静态库

```bash
./tool/build_ffi.sh ios           # 真机(aarch64-apple-ios) + 模拟器(aarch64-apple-ios-simulator)，lipo 合并为 fat
# 注：ios 分支固定 device+sim 合并（fat）。仅调试真机、想跳过模拟器编译时，可临时只跑：
#   cargo rustc -p rdcore-ffi --crate-type staticlib --target aarch64-apple-ios --release
#   mkdir -p flutter/ios/rdcore && cp target/aarch64-apple-ios/release/librdcore_ffi.a flutter/ios/rdcore/
```

产物写入：**`flutter/ios/rdcore/librdcore_ffi.a`**
（这正是 `Runner.xcodeproj/project.pbxproj` 已引用、但此前物理缺失的路径；注意路径是
`flutter/ios/rdcore/`，**没有** `Runner/` 这一层——pbxproj 的 `rdcore` group 直接挂在
项目根 group 下，`sourceTree=<group>` 解析到 `flutter/ios/rdcore/`）。

要点：
- 默认**不传 `real` feature**。`rdcore-capture`/`rdcore-audio` 的 `real`（scrap/enigo/cpal）
  是可选、非默认 feature，故 iOS 编译不会把 scrap（不支持 iOS）编进去。
- `rdcore-rtc` 的 `real`（webrtc-rs）是**默认开**，`rdcore-decode` 的 H.264（openh264）也默认开
  —— 正是 Viewer 需要的。
- openh264 是 C++，由 `cc` crate 驱动交叉编译。macOS 宿主上 `cc` 检测到 `aarch64-apple-ios` 目标会自动经 `xcrun` 定位 Xcode 的 clang 与 iOS SDK（含 `-target`/`-isysroot`），无需手设 `CC`/`CFLAGS`。

## 步骤 2：Xcode 工程链接（已基本就绪，仅需 -force_load）

`project.pbxproj` 仅保留 `librdcore_ffi.a` 的**文件引用**（位于根 `rdcore` group，**不再**加入
"Link Binary With Libraries" 阶段）。链接完全由 `Debug.xcconfig`/`Release.xcconfig` 里的
`-force_load` 完成，**无需手动拖库**。

> ⚠️ **不要把 `librdcore_ffi.a` 重新加回 Frameworks 链接阶段。** Xcode 对这类手动引用的
> group 路径解析不稳定，会报 `Library 'rdcore_ffi' not found`。保持只用 `-force_load` 即可。

唯一需要的是防符号被 strip：在 `flutter/ios/Flutter/Debug.xcconfig` 与 `Release.xcconfig`
末尾已追加：

```
OTHER_LDFLAGS = $(inherited) -force_load "$(SRCROOT)/rdcore/librdcore_ffi.a"
```

原因：Dart 侧用 `DynamicLibrary.process()` 在运行时按名查找 C 符号。iOS 链接器默认会
`dead_strip` 未被 Swift/ObjC 显式引用的导出符号，导致这些 Rust 符号在最终可执行文件中
消失、初始化失败。`-force_load` 强制整库链入。

> ⚠️ **`Runner/RdCoreTexturePlugin.swift` 必须已登记进 `project.pbxproj`**（文件引用 +
> Compile Sources 阶段）。该插件导出 `rdcore_texture_submit` C 符号供 Rust 推送像素，
> 并承载 `rdcore.texture` MethodChannel；未登记时编译报 "Cannot find ... in scope"，
> 或运行时纹理路径缺失、静默回退字节路径（性能大降）。新增/移动该文件后务必检查
> pbxproj 中的引用仍然存在。

> ⚠️ **顺序要求**：必须先跑步骤 1 生成 `.a`，再 `flutter build ios`。否则 `-force_load`
> 指向的文件不存在，链接阶段报错。

## 步骤 3：构建 / 运行验证

```bash
cd flutter
flutter pub get
flutter build ios          # 产物 Runner.app（真机需签名）
# 或 flutter run            # 连真机/模拟器直接跑
```

验证清单：
- App 能启动、不报 "rdcore 初始化失败"（说明 `DynamicLibrary.process()` 找到了符号）。
- 在「设置」里填好 STUN/TURN（见缺口 P0 闭环），与同一账号下的 Windows Host 配对。
- Viewer 能拉到 Windows 画面、能发送键鼠（视频 + 输入链路打通）。
- 渲染路径：debug 控制台出现 `[rdcore:tex] 纹理路径已激活（收到首个 tex-frame）`
  即走在真零拷贝纹理路径（见下节）；出现 `回退字节路径` 则说明走了降级路径，应查明原因。

## Viewer 真零拷贝纹理（推送模型）

渲染主链路：**Rust 解码帧 → 全局 C 函数 `rdcore_texture_submit`（由
`RdCoreTexturePlugin.swift` 导出，`rdcore_texture_set_submit_fn` 注册）→ 直接写入
`CVPixelBuffer` → Dart 仅以 `Texture(textureId)` 合成**。像素绝不进入 Dart 堆；
帧通知统一由 Dart 侧 `tex-frame` → `markFrameAvailable` 驱动（插件只搬像素、不通知）。

要点（均为真机踩坑后的既定约束）：

- `CVPixelBufferCreate` 必须带 `kCVPixelBufferIOSurfacePropertiesKey` 等属性——缺
  IOSurface 支持时 Flutter 合成路径读不到像素，整片空白（间歇性，最难查的一类）。
- `CVPixelBuffer` 不能永久持有 CPU 锁；`submit` 每次写入时 Lock/写完立即 Unlock。
- 分辨率变化（如 Host 3440×1440 vs 初始纹理 1280×720）走「销毁旧纹理、新建带新
  textureId 的纹理」——原地替换缓冲会让已缓存 IOSurface 的纹理整片空白。
- 消费慢于生产时 `render_to_texture` 会「追帧丢旧」：积压帧全部解码（维持 H.264
  参考链）但只提交最新帧，延迟不累积。
- 回退边界：桌面 / headless / 无插件 / 旧库缺符号 / resize 超时未落地 → 自动回退
  `pull_frame` 字节路径（`RemoteFrameView`），画面可用但像素过 Dart 堆、性能大降。

## 已知限制（真机跑通后仍需关注）

1. **Viewer 端音频解码（缺口 C 在 iOS 上的延伸）**：默认 `rdcore-audio` 的 `real`
   （cpal/opus）未启用，故 Viewer 收不到 Host 的 Opus 音频（视频/输入不受影响）。
   如需音频，可在 iOS 构建时给 `rdcore-audio` 开 `real`（cpal/opus 在 iOS 可用），
   相应调整 `tool/build_ffi.sh` 中 ios 分支 `cargo rustc` 的 features。
2. **iOS 退后台断连**：iOS 会挂起网络 socket（含 WebRTC），Viewer 退后台连接必断。
   需要后台保活（音频/VoIP push），当前未实现。
3. **模拟器架构**：脚本默认编 `aarch64-apple-ios-sim`（Apple Silicon Mac）。Intel Mac
   模拟器需改用 `x86_64-apple-ios`。
4. **签名/权限**：真机运行需 Apple Developer 账号签名；若用 `wss://` 信令，确保证书受信。

## 与项目缺口的关系

- 缺口 **M**（Viewer 真实 WebRTC Peer）：已闭环（Rust `webrtc-rs` 经 FFI）。
- 缺口 **P0-D**（移动端 TURN 注入）：已闭环（App 设置 STUN/TURN → FFI `ice_servers`）。
- 本文件解决的是把它们**真正装进 iOS 工程并跑通**的最后一步（编译静态库 + 链接 + 防 strip）。
- 仍待解：iOS 后台保活（平台特性）、音频解码（上）、信令鉴权/持久化（缺口 L）。
