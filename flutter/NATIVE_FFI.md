# rdcore-ffi 原生库跨平台打包

Flutter UI 通过 `dart:ffi` 加载 Rust 核心 `rdcore-ffi`。`lib/ffi/rdcore_bindings.dart`
的 `_openRdCore()` 已按平台选择库名/加载方式；本文件说明各平台如何把原生库**打进产物、
放到运行时可解析的位置**。

统一入口：`tool/build_ffi.sh [host|windows|linux|macos|android|ios|all]`
构建 `rdcore-ffi` 并把产物 stage 到各平台 runner 的约定目录（下表）。

| 平台    | 库文件                  | stage 目录（构建脚本写入）            | 运行时最终位置 / 加载方式                                   | 打包接线                              |
|---------|-------------------------|--------------------------------------|-----------------------------------------------------------|---------------------------------------|
| Windows | `rdcore_ffi.dll`        | `windows/rdcore/`                    | 可执行同级（`...\Release\`）→ `DynamicLibrary.open` 按名   | `windows/CMakeLists.txt` INSTALL ✅  |
| Linux   | `librdcore_ffi.so`      | `linux/rdcore/`                      | bundle `lib/`（rpath `$ORIGIN/lib`）→ 按名                 | `linux/CMakeLists.txt` INSTALL ✅   |
| Android | `librdcore_ffi.so`      | `android/app/src/main/jniLibs/<abi>/`| APK 内 `lib/<abi>/` → 按名（系统 loader）                  | gradle 默认打包 jniLibs ✅（无需改） |
| macOS   | `librdcore_ffi.dylib`   | `macos/rdcore/`                      | `.app/Contents/Frameworks/` → `@rpath` 按名               | `macos/Runner.xcodeproj` 已写入 ✅   |
| iOS     | `librdcore_ffi.a`（静态）| `ios/rdcore/`                        | 静态链入主可执行 → `DynamicLibrary.process()`              | `ios/Runner.xcodeproj` 已写入 ✅     |

> **`rdcore-ffi` 的 `crate-type` 为 `["cdylib", "staticlib", "rlib"]`**：
> cdylib 供 Windows/Linux/macOS 动态加载，staticlib 供 iOS 静态链入，rlib 供 Rust 单测。

Windows / Linux / Android **全自动**（脚本 stage + 构建系统自动打包）。
macOS / iOS 的 Xcode 接线已**直接写入对应 `project.pbxproj`**（见下），`flutter build macos/ios`
开箱即用，无需再手动拖文件。

## macOS（已自动接线）

1. `tool/build_ffi.sh macos` 构建 x86_64 + arm64 通用 dylib，放到 `macos/rdcore/`，
   并自动执行 `install_name_tool -id @rpath/librdcore_ffi.dylib`。
2. `macos/Runner.xcodeproj/project.pbxproj` 中：
   - 新增 `rdcore` 组（路径 `rdcore/`），内含 `librdcore_ffi.dylib` 文件引用；
   - Runner target 的 **Bundle Framework** 复制阶段（dstSubfolderSpec=10=Frameworks）
     已加入该 dylib，并带 `CodeSignOnCopy` 属性（随包签名）。
3. Runner 的 rpath 默认含 `@executable_path/../Frameworks`，故
   `DynamicLibrary.open('librdcore_ffi.dylib')` 可解析。

## iOS（已自动接线）

iOS 禁止 dlopen 随包动态库，`rdcore-ffi` 须以**静态库**链入主可执行文件：

1. `tool/build_ffi.sh ios` 构建 device（`aarch64-apple-ios`）+ simulator
   （`aarch64-apple-ios-sim`、`x86_64-apple-ios`）切片，lipo 成通用
   `librdcore_ffi.a` 放到 `ios/rdcore/`。
   （也可手动：`rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios`
   → `cargo rustc -p rdcore-ffi --target aarch64-apple-ios --crate-type staticlib`。
   **必须用 `cargo rustc --crate-type staticlib` 而非 `cargo build`**：本 crate 的
   `crate-type` 含 `cdylib`，`cargo build` 会尝试链接 cdylib 而要求 iOS SDK/链接器，
   非 Mac 环境会失败；仅编 staticlib 不需要链接器。）
2. `ios/Runner.xcodeproj/project.pbxproj` 中：
   - 新增 `rdcore` 组（路径 `rdcore/`），内含 `librdcore_ffi.a` 文件引用；
   - Runner target 的 **Link Binary With Libraries** 阶段已加入该 `.a`。
3. Dart 端在 iOS 走 `DynamicLibrary.process()`（已在 `_openRdCore()` 处理），
   直接在主可执行文件符号表里查 `rdcore_*`。
4. 为避免链接器把未直接引用的 `#[no_mangle] extern "C"` 符号剥离，
   建议在 `ios/Runner/AppDelegate.swift` 里对某个 `rdcore_*` 符号做一次哑引用，
   或给链接选项加 `-force_load $(PROJECT_DIR)/rdcore/librdcore_ffi.a`。

> **App Store 发布注意**：本脚本用 `lipo` 把 simulator 切片并入 fat `.a`，
> 仅适合开发 / TestFlight 模拟器。**提交 App Store 时必须改用 XCFramework**：
> `xcodebuild -create-xcframework -library ios/rdcore/librdcore_ffi.a -output rdcore.xcframework`，
> 并把 pbxproj 改为链接 `.xcframework`（去掉 simulator 切片）。

## 开发期（flutter run / flutter test）

`tool/build_ffi.sh` 也会把桌面产物复制到 `flutter/` 工程根，
使 `flutter test` / `flutter run -d windows` 在开发机上就能按名加载，无需先打包。

## 验证状态（截至 2026-07-19）

- ✅ **Windows**：`flutter build windows --release` 已验证，`rdcore_ffi.dll` 出现在
  `build/windows/x64/runner/Release/` 与 exe 同级；`flutter test` 4/4 绿。
- ✅ **iOS 静态库可构建**：在本机（Windows）直接
  `cargo build -p rdcore-ffi --target aarch64-apple-ios` 成功产出 `librdcore_ffi.a`
  （`rdcore-ffi` 为纯 Rust，staticlib 无链接步骤，不需 iOS SDK）。
- ⏳ **macOS / iOS 完整 `flutter build`**：本机无 macOS/Xcode 主机未实跑；
  pbxproj 改动遵循 Flutter 自身生成的工程结构（通用 PBX 对象约定），
  需在 Mac 上跑一次 `flutter build macos` / `flutter build ios` 做最终确认。
- ⏳ **Android**：无 NDK/Android SDK 未实跑；`jniLibs` 目录与 `cargo-ndk` 流程已就位。
