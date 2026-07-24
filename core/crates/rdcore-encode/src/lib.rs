//! rdcore-encode — 视频帧编码器（媒体面 / Track A）。
//!
//! 把一帧 `MediaFrame` 编码成目标 `VideoCodec` 的 `MediaFrame`。
//! - `Raw`：RGBA 直通，无压缩，用于调试 / 兜底 / headless 验证。
//! - `H264`：通过 `openh264` 进行有损压缩编码（RGBA → I420 → H.264 Annex-B NAL）。

use openh264::encoder::{Encoder as OhEncoder, EncoderConfig};
use openh264::formats::YUVBuffer;
use openh264::Timestamp;
use rdcore_proto::{MediaFrame, VideoCodec};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// 子模块：
// - `annexb`：AVCC↔Annex-B 纯转换（HW 输出与 RTP/软解字节契约对齐的核心，可单测）。
// - `capability`：硬件编码器能力探测（决定 `new_encoder` 优先级）。
// - `ffmpeg_hw`：FFmpeg（NVENC/QSV/AMF）硬件编码后端，仅 `hwcodec` + Windows 时编译。
mod annexb;
// capability 在默认构建（无 hwcodec）下其探测函数不被 new_encoder 引用，属预期死代码，允许。
// 仍暴露为 `pub mod` 以便下游/真机冒烟测试（examples/hw_smoke.rs）直接读取探测结果做可观测性。
#[allow(dead_code)]
pub mod capability;
#[cfg(all(feature = "hwcodec", windows))]
mod ffmpeg_hw;

#[cfg(all(feature = "hwcodec", windows))]
use capability::detect_hw_encoders;
#[cfg(all(feature = "hwcodec", windows))]
use ffmpeg_hw::FfmpegH264Encoder;

// 供下游/测试复用 Annex-B 工具（纯逻辑）。
pub use annexb::{avcc_sample_to_annexb, sps_pps_from_avcc_extradata};

/// 编码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// 不支持的编解码器（如未启用对应 feature 的压缩编码）。
    UnsupportedCodec(VideoCodec),
    /// 帧数据长度与 `width * height * 4` 不匹配（Raw 要求 RGBA 紧密排列）。
    InvalidFrame { expected: usize, got: usize },
    /// 编码器初始化失败（如 openh264 底层错误）。
    InitFailed(String),
    /// 编码过程失败。
    EncodeFailed(String),
    /// 内部锁被 poison。
    LockPoisoned,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::UnsupportedCodec(c) => write!(f, "不支持的编解码器: {c:?}"),
            EncodeError::InvalidFrame { expected, got } => {
                write!(f, "帧数据长度不符: 期望 {expected}, 实际 {got}")
            }
            EncodeError::InitFailed(e) => write!(f, "编码器初始化失败: {e}"),
            EncodeError::EncodeFailed(e) => write!(f, "编码失败: {e}"),
            EncodeError::LockPoisoned => write!(f, "编码器内部锁已损坏"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// 编码器：把任意（已捕获的）`MediaFrame` 编码成目标 `VideoCodec` 的 `MediaFrame`。
///
/// 约定：输入的 `MediaFrame.data` 应为 RGBA8888 紧密排列（长度 = w*h*4），
/// 无论其 `codec` 字段为何（编码阶段只看像素）。
pub trait Encoder {
    /// 编码一帧；返回同内容、但 `codec` 为目标编解码器的 `MediaFrame`。
    fn encode(&self, frame: &MediaFrame) -> Result<MediaFrame, EncodeError>;

    /// 请求下一帧为关键帧（IDR）。默认空实现供无需此能力的后端使用；
    /// H.264 软/硬后端均覆盖为「置一次性标志，下一帧强制 IDR」。
    fn request_keyframe(&self) {}

    /// 编码器种类标识，用于日志/可观测性：确认实际走了硬编还是软编回退。
    /// 默认 `"unknown"`；各后端覆盖为 `"raw"` / `"h264-software"` / `"h264-hardware"`。
    fn kind(&self) -> &'static str {
        "unknown"
    }
}

/// 创建适配目标编解码器的编码器（跨线程安全）。
///
/// H.264 优先级策略（硬件优先、软件回退，对调用方透明）：
/// - 开启 `hwcodec` 且探测到本机硬件编码器（GPU）→ 优先返回 FFmpeg 硬件后端（NVENC/QSV/AMF）；
/// - 硬件初始化失败（无 GPU / ffmpeg 构建缺 hwaccel / 沙箱）→ 自动回退 `openh264` 软编，不影响可用性；
/// - 未开 `hwcodec` / 非 Windows → 始终使用软编（默认构建路径，零 Windows 依赖）。
pub fn new_encoder(
    codec: VideoCodec,
    width: u32,
    height: u32,
) -> Result<Box<dyn Encoder + Send + Sync>, EncodeError> {
    // 默认走“硬件优先、软编回退”策略；未指定帧率时按保守 30fps 设定时间基。
    new_encoder_with_fps(codec, width, height, 30)
}

/// 与 `new_encoder` 相同，但显式指定目标帧率（影响 FFmpeg 硬编的时间基 / 码控参考 / GOP）。
///
/// 生产路径（`host_media` 泵）应使用本函数并传入泵的实际 fps，使编码器时间戳与
/// 真实帧率一致；`new_encoder` 的 30fps 默认值仅为兼容旧调用点与测试。
pub fn new_encoder_with_fps(
    codec: VideoCodec,
    width: u32,
    height: u32,
    fps: u16,
) -> Result<Box<dyn Encoder + Send + Sync>, EncodeError> {
    new_encoder_forced_with_fps(codec, width, height, false, fps)
}

/// 与 `new_encoder` 相同，但 `force_software = true` 时跳过硬件探测、直接使用 openh264 软编。
///
/// 用途：运行时兜底。硬件编码器可能出现“初始化成功、但逐帧编码失败”的情况（如本机 GPU
/// 驱动异常 / Media Foundation 编码组件缺失）。一旦确认硬件路径不可用，用本函数重建为软编，
/// 保证 Host 不会因硬件故障而整路无视频（详见 `host_media` 的连续失败计数回退）。
pub fn new_encoder_forced(
    codec: VideoCodec,
    width: u32,
    height: u32,
    force_software: bool,
) -> Result<Box<dyn Encoder + Send + Sync>, EncodeError> {
    new_encoder_forced_with_fps(codec, width, height, force_software, 30)
}

/// `new_encoder_forced` 的显式帧率版本（生产路径运行时兜底应传泵的实际 fps）。
pub fn new_encoder_forced_with_fps(
    codec: VideoCodec,
    width: u32,
    height: u32,
    force_software: bool,
    fps: u16,
) -> Result<Box<dyn Encoder + Send + Sync>, EncodeError> {
    match codec {
        VideoCodec::Raw => Ok(Box::new(RawEncoder)),
        VideoCodec::H264 => new_h264_encoder(width, height, force_software, fps),
        other => Err(EncodeError::UnsupportedCodec(other)),
    }
}

/// H.264 编码器工厂：按能力优先级选择硬件或软编。
fn new_h264_encoder(
    width: u32,
    height: u32,
    force_software: bool,
    fps: u16,
) -> Result<Box<dyn Encoder + Send + Sync>, EncodeError> {
    #[cfg(all(feature = "hwcodec", windows))]
    {
        if !force_software && !detect_hw_encoders().is_empty() {
            // FFmpeg 硬件后端（NVENC/QSV/AMF）：内部逐个 try_open 回退，任一成功即返回。
            match FfmpegH264Encoder::new(width, height, fps) {
                Ok(hw) => return Ok(Box::new(hw)),
                // 硬件初始化失败：打印真实原因后回退软编（调用方无感知，保证可用性）。
                Err(e) => {
                    eprintln!(
                        "[encode] 硬件编码器初始化失败 ({}x{})，回退 openh264 软编: {e}",
                        width, height
                    );
                }
            }
        }
    }
    Ok(Box::new(H264Encoder::with_fps(width, height, fps)?))
}

/// Raw 编码器：RGBA 直通（不压缩）。
pub struct RawEncoder;

impl Encoder for RawEncoder {
    fn kind(&self) -> &'static str {
        "raw"
    }

    fn encode(&self, frame: &MediaFrame) -> Result<MediaFrame, EncodeError> {
        let expected = (frame.width as usize) * (frame.height as usize) * 4;
        if frame.data.len() != expected {
            return Err(EncodeError::InvalidFrame {
                expected,
                got: frame.data.len(),
            });
        }
        Ok(MediaFrame {
            codec: VideoCodec::Raw,
            width: frame.width,
            height: frame.height,
            data: frame.data.clone(),
        })
    }
}

/// H.264 编码器（通过 openh264）。
///
/// 内部持有 `openh264::encoder::Encoder`（经 `Mutex` 提供 `&self` 可变编码）。
/// **按需 IDR + P 帧流**：仅首帧 / 被请求（[`Encoder::request_keyframe`]）/ 周期兜底
/// （约 2 秒）时输出 IDR，其余帧输出 P 帧。P 帧体积与编解码开销都比 I 帧低一个数量级，
/// 是 Viewer 软解端（openh264 解 5MP 全 I 流会严重过载）流畅度的关键；丢帧/花屏恢复
/// 由「Host 丢帧即请求下一帧 IDR + Viewer 解码错误主动请求」保证（见 rdcore-app）。
pub struct H264Encoder {
    inner: Mutex<SwEncState>,
    /// 下一帧强制 IDR 的一次性请求标志。
    need_keyframe: AtomicBool,
}

/// 软编可变状态（同一 Mutex 串行化）。
struct SwEncState {
    enc: OhEncoder,
    /// 距上一个强制 IDR 的帧数。
    since_key: u32,
    /// 周期性 IDR 间隔（帧）≈ 2 秒（openh264 自身只有场景切换检测，无固定 GOP 兜底）。
    gop_len: u32,
    /// 启动 IDR 连发余量：编码器（新建 / 降档重建 / 软硬回退）后的前 N 帧全部强制 IDR。
    /// P 帧流下「首帧 IDR 被丢 = 整段 GOP 不可解」，连发 3 帧换启动期抗丢性，代价仅
    /// ~50ms 的编码开销。
    startup_idr: u8,
}

/// 启动 IDR 连发帧数（软/硬编一致；见 `SwEncState::startup_idr`）。
const STARTUP_IDR_BURST: u8 = 3;

impl H264Encoder {
    /// 以目标分辨率构造 H.264 编码器（默认按 30fps 计算周期 IDR 间隔）。
    pub fn new(width: u32, height: u32) -> Result<Self, EncodeError> {
        Self::with_fps(width, height, 30)
    }

    /// 显式指定目标帧率的构造（影响周期 IDR 间隔：2×fps 帧 ≈ 2 秒）。
    pub fn with_fps(width: u32, height: u32, fps: u16) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 {
            return Err(EncodeError::InvalidFrame {
                expected: 0,
                got: 0,
            });
        }
        // 关闭 skip-frame：保证每一帧都产出可解码画面（远程桌面静态屏也要稳定出帧）。
        // 码控用默认 Quality 模式以获得最佳保真度；注：skip-frame 关闭时 openh264 无法按
        // 目标码率精确控带，带宽适配（BWE→码率）留给 gap J 的 RTP 路径处理。
        let cfg = EncoderConfig::new(width, height).enable_skip_frame(false);
        let enc =
            OhEncoder::with_config(cfg).map_err(|e| EncodeError::InitFailed(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(SwEncState {
                enc,
                since_key: 0,
                gop_len: (fps as u32).max(1) * 2,
                startup_idr: STARTUP_IDR_BURST,
            }),
            // 首帧强制 IDR：保证解码起点。
            need_keyframe: AtomicBool::new(true),
        })
    }
}

impl Encoder for H264Encoder {
    fn kind(&self) -> &'static str {
        "h264-software"
    }

    /// 请求下一帧输出 IDR（P 帧流下 Viewer 丢帧/花屏恢复的快速手段）。
    fn request_keyframe(&self) {
        self.need_keyframe.store(true, Ordering::SeqCst);
    }

    fn encode(&self, frame: &MediaFrame) -> Result<MediaFrame, EncodeError> {
        let expected = (frame.width as usize) * (frame.height as usize) * 4;
        if frame.data.len() != expected {
            return Err(EncodeError::InvalidFrame {
                expected,
                got: frame.data.len(),
            });
        }
        let npix = (frame.width as usize) * (frame.height as usize);
        // RGBA -> RGB（剥掉 alpha 通道）
        let mut rgb = Vec::with_capacity(npix * 3);
        for i in 0..npix {
            rgb.push(frame.data[i * 4]);
            rgb.push(frame.data[i * 4 + 1]);
            rgb.push(frame.data[i * 4 + 2]);
        }
        let yuv = YUVBuffer::with_rgb(frame.width as usize, frame.height as usize, &rgb);
        let data = {
            let mut st = self.inner.lock().map_err(|_| EncodeError::LockPoisoned)?;
            // 按需 IDR：被请求（首帧 / 关键帧请求）、启动连发（前 N 帧）或超周期
            // （~2 秒无 IDR）时强制本帧 intra，其余帧输出 P 帧。`force_intra_frame`
            // 为一次性语义（消费于下一帧），逐帧显式传 bool 对「粘性」实现同样安全。
            let force = self.need_keyframe.swap(false, Ordering::SeqCst)
                || st.startup_idr > 0
                || st.since_key >= st.gop_len;
            if st.startup_idr > 0 {
                st.startup_idr -= 1;
            }
            unsafe {
                st.enc.raw_api().force_intra_frame(force);
            }
            let bs = st
                .enc
                .encode_at(&yuv, Timestamp::ZERO)
                .map_err(|e| EncodeError::EncodeFailed(e.to_string()))?;
            let data = bs.to_vec();
            st.since_key = if force { 0 } else { st.since_key + 1 };
            data
        };
        Ok(MediaFrame {
            codec: VideoCodec::H264,
            width: frame.width,
            height: frame.height,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_decode::Decoder;
    use rdcore_proto::VideoCodec;

    #[test]
    fn raw_encoder_passthrough() {
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 4,
            height: 2,
            data: vec![0x10; 4 * 2 * 4],
        };
        let out = RawEncoder.encode(&f).unwrap();
        assert_eq!(out.codec, VideoCodec::Raw);
        assert_eq!(out.width, 4);
        assert_eq!(out.data.len(), 4 * 2 * 4);
    }

    #[test]
    fn raw_encoder_rejects_bad_length() {
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 4,
            height: 2,
            data: vec![0; 3],
        };
        assert!(matches!(
            RawEncoder.encode(&f),
            Err(EncodeError::InvalidFrame { .. })
        ));
    }

    /// 经工厂 `new_encoder(H264)` 编码（默认/未探测到硬件时走软编），产出 H.264 帧。
    /// 即便在 `--features hwcodec` 下无 GPU，也应回退软编成功——验证“硬件优先、软编回退”不破坏可用性。
    #[test]
    fn new_encoder_h264_via_factory() {
        let w = 32u32;
        let h = 24u32;
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: w,
            height: h,
            data: vec![0x7F; (w * h * 4) as usize],
        };
        let enc = new_encoder(VideoCodec::H264, w, h).expect("factory build encoder");
        let out = enc.encode(&f).expect("encode via factory");
        assert_eq!(out.codec, VideoCodec::H264);
        assert!(!out.data.is_empty());
    }

    #[test]
    fn h264_encoder_produces_h264_frame() {
        let w = 64u32;
        let h = 48u32;
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: w,
            height: h,
            data: vec![0x7F; (w * h * 4) as usize],
        };
        let enc = H264Encoder::new(w, h).expect("init encoder");
        let out = enc.encode(&f).expect("encode");
        assert_eq!(out.codec, VideoCodec::H264);
        assert_eq!(out.width, w);
        assert_eq!(out.height, h);
        assert!(!out.data.is_empty(), "H.264 码流不应为空");
    }

    /// 编码后再解码，像素应在容差内一致（H.264 有损，但高码率下偏差很小）。
    #[test]
    fn h264_roundtrip_pixels_within_tolerance() {
        // 复用 rdcore-decode 的解码器做闭环验证。
        use rdcore_decode::H264Decoder;

        let w = 64u32;
        let h = 48u32;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = (x * 4) as u8;
                rgba[i + 1] = (y * 4) as u8;
                rgba[i + 2] = ((x + y) * 2) as u8;
                rgba[i + 3] = 255;
            }
        }
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: w,
            height: h,
            data: rgba.clone(),
        };
        let enc = H264Encoder::new(w, h).expect("init encoder");
        let enc_frame = enc.encode(&f).expect("encode");

        let dec = H264Decoder::new().expect("init decoder");
        let decoded = dec.decode(&enc_frame).expect("decode");

        assert_eq!(decoded.width, w);
        assert_eq!(decoded.height, h);
        assert_eq!(decoded.rgba.len(), rgba.len());

        let mut max_diff = 0i32;
        let mut sum = 0i64;
        for (a, b) in rgba.iter().zip(decoded.rgba.iter()) {
            let d = (*a as i32 - *b as i32).abs();
            if d > max_diff {
                max_diff = d;
            }
            sum += d as i64;
        }
        let mae = sum as f64 / rgba.len() as f64;
        println!("h264 roundtrip max_diff={max_diff} mae={mae:.4}");
        assert!(max_diff <= 48, "最大通道误差 {max_diff} 过大");
        assert!(mae < 12.0, "平均绝对误差 {mae} 过大");
    }

    /// Annex-B 码流是否含 IDR（NAL type 5）。测试辅助：逐字节扫起始码。
    fn annexb_has_idr(data: &[u8]) -> bool {
        for i in 0..data.len().saturating_sub(4) {
            if data[i] == 0 && data[i + 1] == 0 {
                let hdr = if data[i + 2] == 1 {
                    Some(data[i + 3])
                } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    Some(data[i + 4])
                } else {
                    None
                };
                if let Some(h) = hdr {
                    if h & 0x1f == 5 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 按需 IDR + P 帧流：启动连发 3 帧 IDR → 静态画面后续为 P 帧 → request_keyframe 精确生效一帧。
    #[test]
    fn h264_on_demand_idr_p_frame_stream() {
        let w = 64u32;
        let h = 48u32;
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: w,
            height: h,
            data: vec![0x80; (w * h * 4) as usize],
        };
        let enc = H264Encoder::with_fps(w, h, 30).expect("init encoder");

        // 启动连发：前 3 帧全部 IDR（启动期抗丢，见 STARTUP_IDR_BURST）。
        let f1 = enc.encode(&f).expect("encode f1");
        let f2 = enc.encode(&f).expect("encode f2");
        let f3 = enc.encode(&f).expect("encode f3");
        assert!(
            annexb_has_idr(&f1.data) && annexb_has_idr(&f2.data) && annexb_has_idr(&f3.data),
            "启动期前 3 帧必须全是 IDR"
        );

        let f4 = enc.encode(&f).expect("encode f4");
        let f5 = enc.encode(&f).expect("encode f5");
        assert!(
            !annexb_has_idr(&f4.data) && !annexb_has_idr(&f5.data),
            "启动连发结束后静态画面应为 P 帧（全 I 流已废）"
        );
        assert!(
            f5.data.len() < f1.data.len(),
            "静态画面 P 帧（{}B）应显著小于 IDR（{}B）",
            f5.data.len(),
            f1.data.len()
        );

        enc.request_keyframe();
        let f6 = enc.encode(&f).expect("encode f6");
        assert!(annexb_has_idr(&f6.data), "request_keyframe 后下一帧必须是 IDR");

        let f7 = enc.encode(&f).expect("encode f7");
        assert!(!annexb_has_idr(&f7.data), "一次性请求只影响紧随的一帧");
    }

    /// 周期 IDR 兜底：gop_len（2×fps）帧无请求时也应自动出 IDR（启动连发 3 帧计入序列）。
    #[test]
    fn h264_periodic_idr_backstop() {
        let w = 64u32;
        let h = 48u32;
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: w,
            height: h,
            data: vec![0x55; (w * h * 4) as usize],
        };
        // fps=2 → gop_len=4：启动连发 3 帧（0/1/2）后，since_key 逐帧累积，
        // 第 7 帧（since_key 达 4）应再次自动 IDR。
        let enc = H264Encoder::with_fps(w, h, 2).expect("init encoder");
        let mut idr_at = Vec::new();
        for i in 0..8 {
            let out = enc.encode(&f).expect("encode");
            if annexb_has_idr(&out.data) {
                idr_at.push(i);
            }
        }
        assert_eq!(
            idr_at,
            vec![0, 1, 2, 7],
            "IDR 应出现在启动连发 3 帧与 2×fps 周期处: {idr_at:?}"
        );
    }
}
