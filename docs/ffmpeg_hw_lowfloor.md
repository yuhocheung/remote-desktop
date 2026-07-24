# FFmpeg 低驱动地板硬件编码构建方案（方案 A 落地）

> 日期：2026-07-24　状态：**✅ 已完成——vcpkg 构建 + 真机 NVENC 点亮（30/30 帧）**
> 关联：`core/crates/rdcore-encode/src/ffmpeg_hw.rs`、MEMORY「FFmpeg 硬件编码后端落地」
> 落地提交：`31c8445`（NVENC 真机打通的 5 处运行期修复）

## 1. 背景与目标

现行开发库（`C:\ffmpeg-dev`，gyan full-shared **ffmpeg 8.1**）的 `h264_nvenc` 要求
**NVENC API 13.1 → NVIDIA 驱动 ≥ 610.00**。真机冒烟（2026-07-23）证实：本机驱动
API 13.0（约 570.x），NVENC 初始化失败，回退 openh264 软编。

NVENC 版本地板在 **ffmpeg 编译期**由其捆绑的 `nv-codec-headers` 版本决定，运行时无法
降级协商。本方案用钉死版本的 vcpkg 基线编译一套低地板开发库：

| 组件 | 版本 | 效果 |
|---|---|---|
| ffmpeg | **7.1.2** | 与现有代码的 ffmpeg-next API 结构兼容（两个 `Video` 类型设计两代一致） |
| ffnvcodec（nv-codec-headers） | **12.2.72.0** | NVENC API 12.2 → **最低驱动 551.76**（2024-03） |
| amd-amf / mfx-dispatch | 基线自带 | `h264_amf` / `h264_qsv` 照常构建，三家厂商覆盖不变 |

关键点：NVENC **向后兼容**——旧头文件编译的客户端在新驱动上正常工作（支持一个 API
区间而非单点）。因此这一个构建同时覆盖 551.76 以上的老驱动与最新驱动，覆盖率严格
大于 ffmpeg 8.1 构建。比 551.76 还老的极少数用户仍走既有软编兜底，功能无损。

版本地板对照（已核实）：

| NVENC API | 最低驱动 | 对应 ffmpeg 构建 |
|---|---|---|
| 12.2 | 551.76+ | **本方案（7.1.2 + headers 12.2）** |
| 13.0 | 570.00+ | 部分 8.0 构建 |
| 13.1 | 610.00+ | 现行 gyan 8.1（本机即失败于此） |

## 2. 一键构建

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build_ffmpeg_lowfloor.ps1
```

默认参数：vcpkg 检出到 `C:\dev\vcpkg-ffmpeg712`，产物汇聚到
`C:\ffmpeg-7.1.2-lowfloor-dev`（可用 `-VcpkgRoot` / `-OutDir` 覆盖）。

脚本行为：下载钉死的 vcpkg 基线压缩包（SHA
`34823ada10080ddca99b60e85f80f55e18a44eea`，2025-10-13，已核实此刻
`ports/ffmpeg=7.1.2`、`ports/ffnvcodec=12.2.72.0`）→ bootstrap →
`vcpkg install ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,amf,nvcodec,qsv]:x64-windows`
→ 把 `installed/x64-windows/{include,lib,bin}` 汇聚成 gyan 同款布局 → 校验关键文件。

前置条件与已知坑：

- **VS 2019+ Build Tools**（本机已有 2019 BuildTools）。vcpkg 要求 **English 语言包**：
  若报 `... English language pack ...`，在 VS Installer → 修改 → 语言包 勾选 English 后重跑。
- **代理（本机已踩过）**：终端不继承 IE 系统代理（本机为 `127.0.0.1:7888`），浏览器能下
  GitHub 不代表命令行能下。脚本已内置「自动读取注册表系统代理并设置
  `HTTP_PROXY`/`HTTPS_PROXY`」；若你的代理是分协议/认证形式，运行前手动
  `$env:HTTPS_PROXY = "http://..."` 即可。
- 磁盘：vcpkg 检出约 1.5 GB，构建中间产物 5-8 GB；构建全程联网，约 20-40 分钟。
- 特性面**刻意不含 gpl/nonfree**：shared + LGPL 动态链接，与双宽松许可分发兼容；
  不要追加 `gpl`、`x264`、`nonfree` 特性。
- 网络抖动时脚本内 curl 已带 6 次重试；若仍失败，重跑脚本即可（幂等，已下载会跳过）。

## 3. 接入工程

### 3.1 环境变量

```powershell
# 新终端持久生效
setx FFMPEG_DIR "C:\ffmpeg-7.1.2-lowfloor-dev"
# 当前终端立即生效
$env:FFMPEG_DIR = "C:\ffmpeg-7.1.2-lowfloor-dev"
$env:PATH = "C:\ffmpeg-7.1.2-lowfloor-dev\bin;$env:PATH"
```

### 3.2 Cargo.toml 版本回落（2026-07-24 已完成）

`core/crates/rdcore-encode/Cargo.toml` 已改为：

```toml
# ffmpeg-sys-next 的主版本必须等于 ffmpeg 主版本（7.1 → avcodec-61.dll，8.x → avcodec-62.dll）。
ffmpeg-next = { version = "7", optional = true }
```

### 3.3 bindgen 补丁（2026-07-24 已完成，**必须保留**）

`ffmpeg-sys-next` 7.1.3 锁定 bindgen **0.70**，与本机 libclang 组合会把
`AVFormatContext`/`AVOption`/`AVCodecParser`/`tm` 等结构体错误生成为占位符
（`pub _address: u8`），布局断言溢出（E0080）且安全层无法编译；8.1.0 用的 bindgen
0.72 无此问题。已在仓库内置换：

- 根 `Cargo.toml` 增加 `[patch.crates-io] ffmpeg-sys-next = { path = "vendor/ffmpeg-sys-next" }`；
- `vendor/ffmpeg-sys-next` 是 7.1.3 的完整副本，**唯一改动**是 `Cargo.toml` 中
  bindgen `version = "0.70"` → `"0.72"`。

若日后升级 ffmpeg-next 大版本（如回 8），应删除该 patch 与 vendor 目录。

### 3.4 代码适配实测结论：**编译零改动**（2026-07-24 已编译+运行验证）

对照 ffmpeg-next 7.1.0 与 8.1.0 源码，本模块用到的全部 API 两代一致：
`Context::encoder()`（均直接返回 `Encoder`）、`video::Video`/`encoder::Video` 双类型
与 `open()`、`send_frame`/`receive_packet`、`as_mut_ptr`（两代均为 unsafe fn）、
`ffi::AVHWDeviceType` 枚举变体风格（两代 bindgen 均为 `EnumVariation::Rust`）。
**编译层面 `src/ffmpeg_hw.rs` 不需要任何修改**；但真机运行期暴露 5 处问题并修复
（提交 `31c8445`，详见 §4.1 排错实录），运行期行为以现行代码为准。

验证方式：用 BtbN `ffmpeg-n7.1-latest-win64-lgpl-shared-7.1.zip`（解压于
`C:\dev\ffmpeg-7.1-lgpl-shared`，仅作编译校验，vcpkg 产物就绪后可删）执行
`cargo check --release --features hwcodec` 通过。⚠️ 附注（已闭环）：BtbN 包上 NVENC
越过版本检查后报 `OpenEncodeSessionEx failed: invalid ptr (6)`——vcpkg 12.2 构建
**复现了同一错误**，根因与修法见 §4.1 第 1 条（RAM 路径不注入 `hw_device_ctx`）。

### 3.5 强制重建绑定层

```powershell
cargo clean -p ffmpeg-sys-next -p ffmpeg-next
cargo build --release --features hwcodec
```

## 4. 真机验证

```powershell
cargo run --release --example hw_smoke --features hwcodec
```

**2026-07-24 vcpkg 7.1.2 构建实测通过**（本机驱动 API 13.0 ≥ 地板 12.2）：

```
[ffmpeg-hw] 硬件编码器初始化成功：h264_nvenc (1280x720)
    实际后端 kind = h264-hardware
[3] 编码 30 帧（渐变背景 + 移动方块）...
    成功 30/30 帧，首帧 47 字节，每帧均 15519 字节，合计 465594 字节
[4] 结论
    ✅ 硬件编码真实生效（kind=h264-hardware，全部帧 Annex-B 合法）
```

说明：首帧 47 字节是纯 SPS/PPS（nvenc 约 2 帧流水线延迟，见 §4.1 第 5 条），非异常。
若 `nvidia-smi` 报 `Failed to initialize NVML` 属工具自身问题，不影响硬编判定——
NVENC 会话创建（OpenEncodeSessionEx/InitializeEncoder）本身就是驱动级硬件证明。

### 4.1 NVENC 接入排错实录（提交 31c8445，按出现顺序）

1. **`OpenEncodeSessionEx failed: invalid ptr (6)`**（BtbN 与 vcpkg 构建均复现）
   根因：RAM 路径下显式创建 CUDA `hw_device_ctx` 并注入 `AVCodecContext`，外部
   CUDA 上下文与编码器内部会话指针不兼容。
   修法：系统内存 NV12 输入**不注入 `hw_device_ctx`**，nvenc/amf/qsv 在 `open()`
   时自建并托管内部设备会话。`to_ffi` 与 `ffmpeg_next::ffi` 导入随之删除。
2. **`InitializeEncoder failed: invalid param (8): Gop Length should be greater
   than number of B frames + 1`**
   根因：NVENC 拒绝 `gop=1`（全 IDR 的惯用写法）。
   修法：GOP 取 1 秒（`NOMINAL_FPS`），并在 `encode()` 里逐帧
   `frame.set_kind(picture::Type::I)` 强制 IDR——nvenc 识别该标志照样每帧输出
   IDR，「任意帧独立可解、抗丢包」的设计不变。
3. **`硬件编码器未产出 extradata（SPS/PPS）`**
   根因：编码器默认不写 extradata。
   修法：`open()` 前 `set_flags(GLOBAL_HEADER)`。注意 nvenc 的 extradata 直接是
   **Annex-B**（`00 00 00 01` + SPS/PPS），并非 libx264 系的 AVCC——
   `extract_sps_pps` 按起始码自动识别两种格式。
4. **`index out of bounds`（rgb_to_nv12 UV 平面）**
   纯代码 bug：UV 索引 `(cy * w + cx) * 2` 把行宽双倍计入，应为 `cy * w + cx * 2`。
5. **流开头 1-2 帧「未产出任何数据包」**
   非故障：nvenc 内部约 2 帧流水线缓冲，之后每帧稳定一包。
   修法：空包视为正常缓冲，输出仅含 SPS/PPS 头的片段（合法 Annex-B，解码端忽略）。

## 5. 分发注意

- 随 Host 打包的 DLL 从 `bin\` 拷贝，ffmpeg 7.1 对应：
  `avcodec-61.dll`、`avformat-61.dll`、`avutil-59.dll`、`avdevice-61.dll`、
  `avfilter-10.dll`、`swscale-8.dll`、`swresample-5.dll`、`postproc-58.dll`
  （未启用 gpl/nonfree，无 x264 等额外 DLL）。
- NSIS 安装器脚本里的 DLL 清单需同步从 `-62` 系改为 `-61` 系（现行清单是按 8.1 写的）。
- 产品文档可写「NVIDIA 驱动 ≥ 551.76 可启用 GPU 硬编；更低驱动自动回退软编」。

## 6. 回滚

FFMPEG_DIR 指回旧库即可，两套库磁盘共存、互不干扰：

```powershell
setx FFMPEG_DIR "C:\ffmpeg-dev"        # 回到 ffmpeg 8.1
# Cargo.toml 中 ffmpeg-next 改回 "8"
cargo clean -p ffmpeg-sys-next -p ffmpeg-next
```

## 7. 后续建议

- `capability` 探测阶段直接读 `nvEncodeAPI64.dll` 的 `NvEncodeAPIGetMaxSupportedVersion`，
  把「试错撞墙」改为确定性预判，并在日志/遥测记录最终命中的后端（hw vs sw），
  用真实数据复核这个地板决策。
