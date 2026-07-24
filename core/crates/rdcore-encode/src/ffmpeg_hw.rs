//! Host 硬件 H.264 编码后端（FFmpeg，仅 `hwcodec` + Windows 构建时编译）。
//!
//! 为什么从 Media Foundation 切换到 FFmpeg（2026-07-23 决策，对齐 RustDesk 的 hwcodec）：
//! MF MFT 硬件编码路线踩满 4 个连续深坑——`MF_E_TRANSFORM_ASYNC_LOCKED` →
//! `MF_E_TRANSFORM_TYPE_NOT_SET` → `0x8000FFFF`（需注入 D3D11 设备管理器）→ **仍 0x8000FFFF**
//! （NVENC MFT 还要求输入是 D3D11 纹理且建在正确的 GPU 适配器上，系统内存 NV12 喂不出行）。
//! FFmpeg 的 `h264_nvenc`/`h264_amf`/`h264_qsv` 经 `D3D11VA`/`CUDA` **硬件设备上下文**统一封装，
//! 由 ffmpeg 内部托管「选对 GPU 适配器 + NV12→D3D11/CUDA 纹理上传」两坑，一份代码覆盖三家厂商。
//!
//! RAM 路径（远程桌面最契合）：调用方继续喂**系统内存 NV12**（由本模块的 `rgb_to_nv12` 从
//! 捕获的 RGBA 转换得到，无需改 capture→encode 边界），ffmpeg 内部完成 GPU 上传与编码。
//! 这与 RustDesk 的 hwcodec RAM 路径一致——不要求调用方产出 GPU 纹理即可拿到真硬件帧。
//!
//! 字节契约：FFmpeg 的 H.264 编码器同样产出 **AVCC**（长度前缀）样本，本后端经 `annexb` 模块
//! 转成 **Annex-B**（`00 00 00 01` 起始码）并每帧前置 SPS/PPS，使 `MediaFrame.data` 与
//! `openh264` 软编输出字节级同构——对 RTP 打包器（`h264_rtp.rs`）与 Viewer 软解端完全透明，
//! 无需改任何下游代码（见 MEMORY「Host 硬编可独立实现、对 Viewer 透明」）。
//!
//! 厂商映射（由 `capability` 探测决定优先级，构造期逐个 try_open 回退）：
//! - NVIDIA NVENC → `h264_nvenc` + `AV_HWDEVICE_TYPE_CUDA`
//! - AMD AMF      → `h264_amf`    + `AV_HWDEVICE_TYPE_D3D11VA`
//! - Intel QSV    → `h264_qsv`    + `AV_HWDEVICE_TYPE_D3D11VA`
//! （仅探到通用 MF HW 编码器而无厂商 DLL 时，按 nvenc→amf→qsv 顺序试错）
//!
//! 运行约束：本模块**编译无需 GPU**（纯类型/FFI 声明 + ffmpeg-next 绑定），但**实际编码必须
//! 在装有 GPU 的 Windows 上运行**，且宿主 ffmpeg 构建需启用对应硬件后端（RustDesk 的 vcpkg
//! `ffmpeg[amf,avcodec,...,nvcodec,qsv]`）。沙箱/CI 无 GPU、无 ffmpeg 开发库，只能保证默认
//! 构建与软编回退不变；ffmpeg 特性的编译验证交用户机器（见 MEMORY「FFmpeg 硬件编码后端落地」）。

#![cfg(all(feature = "hwcodec", windows))]

use crate::annexb::sps_pps_from_avcc_extradata;
use crate::capability::HwEncoderKind;
use crate::{EncodeError, Encoder};
use ffmpeg_next as ffmpeg;
// ffmpeg-next 7.x/8.x 有两个同名 `Video` 类型，务必区分：
// - `encoder::video::Video`（构建器）：`Encoder::video()` 返回，提供 `set_*` 与 `open(self)`；
// - `encoder::Video`（即 `video::Encoder`，已打开编码器）：`open()` 的返回类型，经
//   `Deref → video::Video → Encoder → Context` 透出 `send_frame`/`receive_packet`/`as_ptr`。
use ffmpeg_next::codec::encoder::video::Video as FfmpegVideoBuilder;
use ffmpeg_next::codec::encoder::Video as FfmpegVideo;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::util::format::pixel::Pixel;
use rdcore_proto::{MediaFrame, VideoCodec};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 硬件编码器内部可变状态（经 `Mutex` 暴露 `&self` 编码接口）。
struct FfmpegState {
    /// 已打开的 ffmpeg 编码器（持有底层 `AVCodecContext`）。
    encoder: FfmpegVideo,
    /// 已转成 Annex-B 的 SPS/PPS 头，每帧前置以保证任意帧独立可解。
    sps_pps: Vec<u8>,
    /// 单调递增的时间戳（以 `1/fps` 秒为单位的 time_base 计，fps 为构造时传入的目标帧率）。
    pts: i64,
    /// 启动 IDR 连发余量（前 N 帧全强制 IDR，防「首帧 IDR 被丢 = 整段 GOP 不可解」）。
    startup_idr: u8,
}

/// FFmpeg H.264 硬件编码器（实现 `Encoder` trait）。
pub struct FfmpegH264Encoder {
    inner: Mutex<FfmpegState>,
    /// 下一帧强制 IDR 的一次性请求标志（Viewer 关键帧请求 / Host 丢帧自愈 / 首帧保险）。
    need_keyframe: AtomicBool,
}

// SAFETY: 所有对 ffmpeg 状态（含底层 `*mut AVCodecContext` / `*mut AVBufferRef` 裸指针）的访问
// 都经 `Mutex` 串行化，不存在跨线程并发访问；ffmpeg 编码器本身无线程局部状态。故断言
// Send+Sync，满足 `new_encoder` 返回的 `Box<dyn Encoder + Send + Sync>` 约束。与旧 MF 后端同因。
unsafe impl Send for FfmpegH264Encoder {}
unsafe impl Sync for FfmpegH264Encoder {}

impl FfmpegH264Encoder {
    /// 构造并初始化硬件 H.264 编码器：按能力探测的优先级逐个 try_open，任一成功即用，
    /// 全部失败返回 Err（调用方 `new_h264_encoder` 在**构造期**回退软编）。
    /// `fps` 为目标帧率，用于时间基 / 码控参考帧率 / GOP（=1 秒）。
    pub fn new(width: u32, height: u32, fps: u16) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 {
            return Err(EncodeError::InvalidFrame {
                expected: 0,
                got: 0,
            });
        }

        // ffmpeg 全局初始化（幂等；多次调用安全，忽略 AlreadyInitialized 类错误）。
        let _ = ffmpeg::init();

        let backends = select_backends();
        let mut last_err = None;
        for (name, dev_type) in backends {
            match try_open(name, dev_type, width, height, fps) {
                Ok(state) => {
                    eprintln!(
                        "[ffmpeg-hw] 硬件编码器初始化成功：{name} ({}x{})",
                        width, height
                    );
                    return Ok(Self {
                        inner: Mutex::new(state),
                        // 首帧强制 IDR：保证新编码器（新 Viewer 加入 / 降档重建）的第一帧
                        // 即可作解码起点，不等 1 秒 GOP 边界。
                        need_keyframe: AtomicBool::new(true),
                    });
                }
                Err(e) => {
                    eprintln!("[ffmpeg-hw] {name} 初始化失败（{dev_type:?}）：{e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            EncodeError::InitFailed("无可用 ffmpeg 硬件后端（GPU/驱动/ffmpeg 构建缺失）".into())
        }))
    }
}

impl Encoder for FfmpegH264Encoder {
    fn kind(&self) -> &'static str {
        "h264-hardware"
    }

    fn encode(&self, frame: &MediaFrame) -> Result<MediaFrame, EncodeError> {
        let expected = (frame.width as usize) * (frame.height as usize) * 4;
        if frame.data.len() != expected {
            return Err(EncodeError::InvalidFrame {
                expected,
                got: frame.data.len(),
            });
        }
        let w = frame.width as usize;
        let h = frame.height as usize;

        // 捕获的 RGBA → 系统内存 NV12（RAM 路径：ffmpeg 内部再上传 GPU）。
        let nv12 = rgb_to_nv12(&frame.data, w, h);

        let mut state = self.inner.lock().map_err(|_| EncodeError::LockPoisoned)?;

        // 构造软件 NV12 帧并填充（按 ffmpeg 自行选择的 stride 逐行拷贝，避免越界）。
        let mut ff_frame = ffmpeg::frame::Video::new(Pixel::NV12, frame.width, frame.height);
        fill_nv12_frame(&mut ff_frame, &nv12, w, h);
        ff_frame.set_pts(Some(state.pts));
        state.pts += 1;
        // 按需 IDR：仅在被请求（首帧 / Viewer 关键帧请求 / Host 丢帧自愈）或启动连发
        // 窗口内把本帧标记为关键帧，其余帧输出 P 帧——P 帧体积小一个数量级、编解码都
        // 便宜得多，是全链路流畅度（尤其 Viewer 软解端）的关键。周期性 IDR 兜底由
        // gop（=1 秒）负责。
        let force_key = self.need_keyframe.swap(false, Ordering::SeqCst) || state.startup_idr > 0;
        if state.startup_idr > 0 {
            state.startup_idr -= 1;
        }
        if force_key {
            ff_frame.set_kind(ffmpeg::util::picture::Type::I);
        }

        // 1) 送入一帧。
        state
            .encoder
            .send_frame(&ff_frame)
            .map_err(|e| EncodeError::EncodeFailed(format!("send_frame: {e}")))?;

        // 2) 排空已编码数据包（无 B 帧，通常每帧恰好一包；循环以防缓冲）。
        let mut out = Vec::with_capacity(state.sps_pps.len() + 4096);
        out.extend_from_slice(&state.sps_pps);
        let mut pkt = Packet::empty();
        while state.encoder.receive_packet(&mut pkt).is_ok() {
            if let Some(d) = pkt.data() {
                out.extend_from_slice(d);
            }
            pkt = Packet::empty();
        }

        // nvenc 有约 2 帧的流水线延迟：流开头几帧 send 后可能暂无 packet 产出，
        // 属正常缓冲而非失败（后续每帧稳定一包）。此时仅输出前置的 SPS/PPS 头，
        // 仍是合法 Annex-B 片段，解码端收到裸参数集会安全忽略。
        Ok(MediaFrame {
            codec: VideoCodec::H264,
            width: frame.width,
            height: frame.height,
            data: out,
        })
    }

    /// 请求下一帧输出 IDR（P 帧流下 Viewer 丢帧/花屏恢复的唯一快速手段）。
    fn request_keyframe(&self) {
        self.need_keyframe.store(true, Ordering::SeqCst);
    }
}

/// 硬件设备类型——仅用于后端优先级列表的标识与日志（Debug）。RAM 路径不再创建
/// `hw_device_ctx`（见 `try_open` 注释），故无需映射到 `ffi::AVHWDeviceType`。
#[derive(Debug, Clone, Copy)]
enum HwDeviceType {
    Cuda,
    D3D11Va,
}

/// 按能力探测结果排出硬件后端优先级（编码器名 + 对应硬件设备类型）。
///
/// 仅探到通用 MF HW 编码器（`MediaFoundation`）而无厂商 DLL 时，按 nvenc→amf→qsv 顺序试错，
/// 覆盖「有 GPU 但厂商探测 DLL 未注册」的情况。
fn select_backends() -> Vec<(&'static str, HwDeviceType)> {
    let kinds = crate::capability::detect_hw_encoders();
    let mut list: Vec<(&'static str, HwDeviceType)> = Vec::new();
    if kinds.contains(&HwEncoderKind::Nvenc) {
        list.push(("h264_nvenc", HwDeviceType::Cuda));
    }
    if kinds.contains(&HwEncoderKind::Amf) {
        list.push(("h264_amf", HwDeviceType::D3D11Va));
    }
    if kinds.contains(&HwEncoderKind::Qsv) {
        list.push(("h264_qsv", HwDeviceType::D3D11Va));
    }
    if list.is_empty() {
        // 兜底：无厂商专用信号时，三种都试一遍（实际会按 ffmpeg 能否 open 决定）。
        list.push(("h264_nvenc", HwDeviceType::Cuda));
        list.push(("h264_amf", HwDeviceType::D3D11Va));
        list.push(("h264_qsv", HwDeviceType::D3D11Va));
    }
    list
}

/// 尝试用指定编码器名 + 硬件设备类型打开一个 H.264 硬件编码器。
///
/// - 找到 ffmpeg 编码器 → 建上下文 → 设视频参数（NV12 / 码率 / 时间基 / gop=1秒 / 无 B 帧）
///   → `open()`（真正初始化 GPU 编码会话，失败即 Err，由调用方回退）
///   → 从 `extradata`（AVCC）抽取 SPS/PPS 转 Annex-B 缓存。
fn try_open(
    name: &str,
    _dev_type: HwDeviceType,
    width: u32,
    height: u32,
    fps: u16,
) -> Result<FfmpegState, EncodeError> {
    let codec = ffmpeg::codec::encoder::find_by_name(name).ok_or_else(|| {
        EncodeError::InitFailed(format!(
            "ffmpeg 未找到编码器 {name}（宿主 ffmpeg 构建可能未启用该硬件后端）"
        ))
    })?;

    let context = ffmpeg::codec::context::Context::new_with_codec(codec);

    // ffmpeg-next 7.x/8.x：`Context::encoder()` 直接返回 `Encoder`（非 `Result`）。
    let enc = context.encoder();
    let mut video: FfmpegVideoBuilder = enc
        .video()
        .map_err(|e| EncodeError::InitFailed(format!("获取视频编码器失败: {e}")))?;

    video.set_width(width);
    video.set_height(height);
    // RAM 路径：输入是系统内存 NV12，ffmpeg 经 `hw_device_ctx` 自动上传到 GPU。
    video.set_format(Pixel::NV12);
    // 码率 = 分辨率基线 × 帧率缩放：全 IDR 没有帧间冗余可省，恒定画质下码率必须与
    // fps 成正比。基线 w*h*4 bits 对应 30fps（约 0.133 bit/像素/帧）；不随 fps 缩放时
    // 60fps 每帧码率预算减半、画面明显变糊（旧 DataChannel 路径的发送背压会把泵压到
    // 低 fps，单帧反而分到更多码率，因此该缺陷直到 RTP 跑满 60fps 才暴露）。
    // 上限钳到 10 Mbps：DataChannel 回退路径（Web Viewer / 旧端）实测吞吐天花板
    // ~9.7 Mbps（webrtc-rs SCTP/DTLS 发送 CPU 受限，两台浏览器实测一致 9663 kb/s），
    // 超过只会把「丢帧保实时」打成常驻、有效帧率掉到 ~14fps 且缓冲延迟顶满。
    // RTP 路径如需更高码率（LAN 原生 Viewer），用 `RDCORE_VIDEO_BITRATE_KBPS`
    // 显式覆盖（弱上行 / 经 TURN 时反向调低以保帧率）。
    let bit_rate = std::env::var("RDCORE_VIDEO_BITRATE_KBPS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&kbps| kbps > 0)
        .map(|kbps| kbps * 1000)
        .unwrap_or_else(|| {
            let base_30fps = width as usize * height as usize * 4;
            (base_30fps * (fps as usize).max(1) / 30)
                .clamp(1_000_000, 10_000_000)
                .max(1)
        });
    video.set_bit_rate(bit_rate);
    let fps = (fps as i32).max(1);
    video.set_time_base((1, fps));
    video.set_frame_rate(Some((fps, 1)));
    // GOP 取 1 秒（= fps）：周期性 IDR 兜底——按需 IDR（`request_keyframe` / 首帧，
    // 见 encode() 的 need_keyframe 标志）之外，即使恢复请求全部失效，Viewer 最坏也只在
    // 1 秒 GOP 边界处自愈。无 B 帧以降低延迟。
    video.set_gop(fps as u32);
    video.set_max_b_frames(0);
    // GLOBAL_HEADER：让编码器把 SPS/PPS 写入 extradata（AVCC），否则 nvenc 不产出
    // extradata，无法为 Annex-B 码流前置参数集头。
    video.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);

    // 不注入 `hw_device_ctx`：RAM 路径下输入是系统内存 NV12，nvenc/amf/qsv 编码器在
    // open() 时会自行创建并托管内部设备会话。实测（BtbN 与 vcpkg 构建均复现）显式创建
    // CUDA `hw_device_ctx` 注入后，h264_nvenc 在 open() 报
    // `OpenEncodeSessionEx failed: invalid ptr (6)`——外部 CUDA 上下文与编码器内部
    // 会话指针不兼容。交给编码器自建会话即可，纹理上传由 ffmpeg 内部完成。
    // `video.open()` 在 ffmpeg-next 7.x/8.x 返回 `Result<encoder::Video, Error>`——即文件头部别名
    // 的 `FfmpegVideo`（已打开编码器，非构建器），经 `Deref` 透出
    // `send_frame`/`receive_packet`/`as_ptr`）。用 `opened` 变量接收。
    // 注意**不要**开 NVENC `forced-idr`：强制 I 帧输出为真 IDR 需插 SPS/PPS + 重置
    // 参考缓冲，编码吞吐实测 82→45fps（1440p 超宽），还会触发泵的自适应降档
    // 60→30，拖累所有 Viewer。非 IDR 的 I 帧（OpenGOP intra）同样独立可解，
    // Web Viewer 自 2026-07-24 起按 I-slice 识别其为恢复点，效果等同且编码更快；
    // 本系统 IDR 本就只在请求/周期兜底时偶发，无需再为它付 forced-idr 的固定开销。
    let opened: FfmpegVideo = video
        .open()
        .map_err(|e| EncodeError::InitFailed(format!("打开硬件编码器 {name} 失败: {e}")))?;

    let sps_pps = extract_sps_pps(&opened)?;

    Ok(FfmpegState {
        encoder: opened,
        sps_pps,
        pts: 0,
        // 启动 IDR 连发 3 帧（与软编 STARTUP_IDR_BURST 对齐）：启动期 RTP/DC 竞态最易
        // 丢帧，连发保证 Viewer 至少拿到一个解码起点，代价仅 ~50ms 编码开销。
        startup_idr: 3,
    })
}

/// 从已打开编码器的 `extradata` 抽取 SPS/PPS 缓存为 Annex-B 头。
/// extradata 可能是 Annex-B（nvenc）或 AVCC（libx264 系），按起始码自动识别。
fn extract_sps_pps(encoder: &FfmpegVideo) -> Result<Vec<u8>, EncodeError> {
    // SAFETY: `encoder` 持有底层 `AVCodecContext`；`extradata`/`extradata_size` 在 `open()`
    // 后为有效只读切片（AVCC 布局），open() 失败不会走到这里。此处只读，用 `as_ptr`（`&self`）。
    let ctx = unsafe { encoder.as_ptr() };
    unsafe {
        let extradata = (*ctx).extradata;
        let extradata_size = (*ctx).extradata_size;
        if extradata.is_null() || extradata_size <= 0 {
            return Err(EncodeError::InitFailed(
                "硬件编码器未产出 extradata（SPS/PPS），无法构造 Annex-B 头".into(),
            ));
        }
        let raw = std::slice::from_raw_parts(extradata, extradata_size as usize);
        // nvenc 的 extradata 直接是 Annex-B（00 00 00 01 起始码 + SPS/PPS），
        // 而 libx264 系是 AVCC（avcC 记录）。按起始码探测格式分别处理。
        if raw.starts_with(&[0, 0, 0, 1]) || raw.starts_with(&[0, 0, 1]) {
            Ok(raw.to_vec())
        } else {
            Ok(sps_pps_from_avcc_extradata(raw))
        }
    }
}

/// 把 RGBA8888 紧密排列的帧转成 NV12（Y 平面 + 交错的 UV 半分辨率平面）。
///
/// 采用 BT.601 limited-range 系数（与绝大多数 H.264 编码器默认一致）。色彩转换在 CPU 完成，
/// 属 RAM 路径的额外开销。性能：1440p 超宽（5M 像素）逐像素 f32 标量实现实测 ~30ms/帧，
/// 直接把 60fps 管线拖垮；现改为**整数系数 + 按行分段的 scope 多线程**（线程数取
/// available_parallelism 上限 8），同规格实测 ~3ms。后续进一步优化可改由 capture
/// 直接产出 NV12（DXGI `DXGI_FORMAT_NV12`）或经 ffmpeg `swscale` SIMD 卸载。
fn rgb_to_nv12(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    let y_size = w * h;
    let mut nv12 = vec![0u8; y_size + y_size / 2];
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    // 先一次性切成 Y / UV 两个独立切片：scope 内两族线程各持其一，
    //  borrow checker 才能看出两者不相交。
    let (y_plane, uv_plane) = nv12.split_at_mut(y_size);

    std::thread::scope(|s| {
        // Y 平面：按行分段并行。整数 BT.601：Y = (66R + 129G + 25B) / 256 + 16。
        {
            let rows_per = h.div_ceil(threads);
            let mut rest: &mut [u8] = y_plane;
            for t in 0..threads {
                let y0 = t * rows_per;
                if y0 >= h {
                    break;
                }
                let rows = rows_per.min(h - y0);
                let r = std::mem::take(&mut rest);
                let (chunk, tail) = r.split_at_mut(rows * w);
                rest = tail;
                s.spawn(move || {
                    for (ry, row) in chunk.chunks_exact_mut(w).enumerate() {
                        let src = &rgba[(y0 + ry) * w * 4..(y0 + ry + 1) * w * 4];
                        for (x, px) in row.iter_mut().enumerate() {
                            let i = x * 4;
                            let r = src[i] as i32;
                            let g = src[i + 1] as i32;
                            let b = src[i + 2] as i32;
                            *px = ((((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255))
                                as u8;
                        }
                    }
                });
            }
        }
        // UV 平面：2×2 块求和后按整数系数（分母 1024 = 256 系数 × 4 像素）：
        // U = (-38R - 74G + 112B) / 1024 + 128；V = (112R - 94G - 18B) / 1024 + 128。
        {
            let ch = h / 2;
            let rows_per = ch.div_ceil(threads);
            let mut rest: &mut [u8] = uv_plane;
            for t in 0..threads {
                let c0 = t * rows_per;
                if c0 >= ch {
                    break;
                }
                let rows = rows_per.min(ch - c0);
                let r = std::mem::take(&mut rest);
                let (chunk, tail) = r.split_at_mut(rows * w);
                rest = tail;
                s.spawn(move || {
                    for (ry, row) in chunk.chunks_exact_mut(w).enumerate() {
                        let cy = c0 + ry;
                        for cx in 0..w / 2 {
                            let mut r = 0i32;
                            let mut g = 0i32;
                            let mut b = 0i32;
                            for dy in 0..2 {
                                for dx in 0..2 {
                                    let x = (cx * 2 + dx).min(w - 1);
                                    let y = (cy * 2 + dy).min(h - 1);
                                    let i = (y * w + x) * 4;
                                    r += rgba[i] as i32;
                                    g += rgba[i + 1] as i32;
                                    b += rgba[i + 2] as i32;
                                }
                            }
                            row[cx * 2] =
                                (((-38 * r - 74 * g + 112 * b + 512) >> 10) + 128).clamp(0, 255)
                                    as u8;
                            row[cx * 2 + 1] =
                                (((112 * r - 94 * g - 18 * b + 512) >> 10) + 128).clamp(0, 255)
                                    as u8;
                        }
                    }
                });
            }
        }
    });
    nv12
}

/// 把连续 NV12 缓冲按 ffmpeg 帧的 stride 逐行拷贝进 Y / UV 平面（避免 stride 对齐越界）。
fn fill_nv12_frame(frame: &mut ffmpeg::frame::Video, nv12: &[u8], w: usize, h: usize) {
    let y_size = w * h;
    let y_src = &nv12[..y_size];
    let uv_src = &nv12[y_size..y_size + (w * h / 2)];

    // Y 平面（每行 w 字节，stride 可能 > w）。
    let y_stride = frame.stride(0) as usize;
    let y_dst = frame.data_mut(0);
    for y in 0..h {
        let dst = &mut y_dst[y * y_stride..y * y_stride + w];
        let src = &y_src[y * w..y * w + w];
        dst.copy_from_slice(src);
    }

    // UV 平面（交错的 U/V，每行 w 字节，半分辨率 h/2 行）。
    let uv_stride = frame.stride(1) as usize;
    let uv_dst = frame.data_mut(1);
    for y in 0..h / 2 {
        let dst = &mut uv_dst[y * uv_stride..y * uv_stride + w];
        let src = &uv_src[y * w..y * w + w];
        dst.copy_from_slice(src);
    }
}
