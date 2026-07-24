# Android 原生库放置（jniLibs）

Gradle 默认会把 `src/main/jniLibs/<abi>/` 下的 `.so` 打进 APK/AAB，运行时
`System.loadLibrary` 与 Dart `DynamicLibrary.open('librdcore_ffi.so')` 均可按名解析，
**无需修改 build.gradle**。

## 需要放置的文件

为每个目标 ABI 交叉编译 `rdcore-ffi`（`crate-type = ["cdylib"]`），产物重命名/放置为：

| ABI            | 路径                                             |
|----------------|--------------------------------------------------|
| arm64-v8a      | `jniLibs/arm64-v8a/librdcore_ffi.so`             |
| armeabi-v7a    | `jniLibs/armeabi-v7a/librdcore_ffi.so`           |
| x86_64（模拟器）| `jniLibs/x86_64/librdcore_ffi.so`                |

## 交叉编译（需 Android NDK + cargo-ndk）

```bash
# 一次性安装
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk

# 在仓库根执行（tool/build_ffi.sh android 已封装以下步骤）
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
  -o flutter/android/app/src/main/jniLibs \
  build -p rdcore-ffi --release
```

`cargo-ndk` 会自动按 ABI 建子目录并放入 `librdcore_ffi.so`。
