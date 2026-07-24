//! rdcore-decode — 视频帧解码器（媒体面 / Track A，Viewer 侧）。
//!
//! 把线上 `MediaFrame` 还原成 `DecodedFrame`（RGBA8888），交给渲染层（`rdcore-render`）
//! 或 GUI（`RemoteScreen`）显示。`Raw` 为直通；`H264` 通过 `openh264` 解码
//! （带 SPS/PPS 缓存，容忍中途帧缺失参数集导致的解码失败）。

use openh264::decoder::Decoder as OhDecoder;
use rdcore_proto::{MediaFrame, VideoCodec};
use std::fmt;
use std::sync::Mutex;

/// 解码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// 不支持的编解码器（如未启用对应 feature 的压缩编码）。
    UnsupportedCodec(VideoCodec),
    /// 帧数据长度与 `width * height * 4` 不匹配（Raw 要求 RGBA 紧密排列）。
    InvalidFrame { expected: usize, got: usize },
    /// 解码器初始化失败。
    InitFailed(String),
    /// 解码过程失败（码流损坏等）。
    DecodeFailed(String),
    /// 解码未产出画面（需要更多数据）。
    NoFrame,
    /// 内部锁被 poison。
    LockPoisoned,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnsupportedCodec(c) => write!(f, "不支持的编解码器: {c:?}"),
            DecodeError::InvalidFrame { expected, got } => {
                write!(f, "帧数据长度不符: 期望 {expected}, 实际 {got}")
            }
            DecodeError::InitFailed(e) => write!(f, "解码器初始化失败: {e}"),
            DecodeError::DecodeFailed(e) => write!(f, "解码失败: {e}"),
            DecodeError::NoFrame => write!(f, "解码未产出画面，需要更多数据"),
            DecodeError::LockPoisoned => write!(f, "解码器内部锁已损坏"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 一帧解码后的 RGBA 画面（与渲染层 / GUI 的契约）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA8888 像素，长度应为 `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl DecodedFrame {
    /// 缓冲区应有的字节数（宽 × 高 × 4）。
    pub fn byte_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// 解码器：把 `MediaFrame` 还原成 `DecodedFrame`（RGBA）。
pub trait Decoder {
    fn decode(&self, media: &MediaFrame) -> Result<DecodedFrame, DecodeError>;
}

/// 创建适配目标编解码器的解码器（跨线程安全）。
pub fn new_decoder(codec: VideoCodec) -> Result<Box<dyn Decoder + Send + Sync>, DecodeError> {
    match codec {
        VideoCodec::Raw => Ok(Box::new(RawDecoder)),
        VideoCodec::H264 => Ok(Box::new(H264Decoder::new()?)),
        other => Err(DecodeError::UnsupportedCodec(other)),
    }
}

/// Raw 解码器：把 Raw `MediaFrame.data` 直接当作 RGBA。
pub struct RawDecoder;

impl Decoder for RawDecoder {
    fn decode(&self, media: &MediaFrame) -> Result<DecodedFrame, DecodeError> {
        if media.codec != VideoCodec::Raw {
            return Err(DecodeError::UnsupportedCodec(media.codec));
        }
        let expected = (media.width as usize) * (media.height as usize) * 4;
        if media.data.len() != expected {
            return Err(DecodeError::InvalidFrame {
                expected,
                got: media.data.len(),
            });
        }
        Ok(DecodedFrame {
            width: media.width,
            height: media.height,
            rgba: media.data.clone(),
        })
    }
}

/// H.264 解码器（通过 openh264）。
///
/// 持有持久 `Decoder`（可跨多帧保留 SPS/PPS 状态），并缓存最近一次见到的
/// SPS/PPS NAL；当某帧不包含参数集时自动补齐，提升丢帧后的恢复能力。
pub struct H264Decoder {
    inner: Mutex<OhDecoder>,
    sps_pps: Mutex<Vec<u8>>,
}

impl H264Decoder {
    /// 构造 H.264 解码器。
    pub fn new() -> Result<Self, DecodeError> {
        let dec = OhDecoder::new().map_err(|e| DecodeError::InitFailed(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(dec),
            sps_pps: Mutex::new(Vec::new()),
        })
    }
}

impl Decoder for H264Decoder {
    fn decode(&self, media: &MediaFrame) -> Result<DecodedFrame, DecodeError> {
        if media.codec != VideoCodec::H264 {
            return Err(DecodeError::UnsupportedCodec(media.codec));
        }
        let expected = (media.width as usize) * (media.height as usize) * 4;
        if media.data.len() != expected && media.codec == VideoCodec::Raw {
            return Err(DecodeError::InvalidFrame {
                expected,
                got: media.data.len(),
            });
        }

        // 维护 SPS/PPS 缓存，并对缺参数集的 IDR 帧补齐（P 帧不应补 SPS/PPS，否则会破坏参考关系）。
        let mut cache = self.sps_pps.lock().map_err(|_| DecodeError::LockPoisoned)?;
        cache_sps_pps(&media.data, &mut cache);
        let mut packet = media.data.clone();
        if !packet_has_sps_pps(&packet) && packet_has_idr(&packet) && !cache.is_empty() {
            let mut prepended = cache.clone();
            prepended.extend_from_slice(&packet);
            packet = prepended;
        }
        drop(cache);

        let mut dec = self.inner.lock().map_err(|_| DecodeError::LockPoisoned)?;
        let dec_yuv = dec
            .decode(&packet)
            .map_err(|e| DecodeError::DecodeFailed(e.to_string()))?
            .ok_or(DecodeError::NoFrame)?;

        let npix = (media.width as usize) * (media.height as usize);
        let mut rgb = vec![0u8; npix * 3];
        dec_yuv.write_rgb8(&mut rgb);

        // RGB -> RGBA（补齐不透明 alpha）。
        let mut rgba = vec![0u8; npix * 4];
        for i in 0..npix {
            rgba[i * 4] = rgb[i * 3];
            rgba[i * 4 + 1] = rgb[i * 3 + 1];
            rgba[i * 4 + 2] = rgb[i * 3 + 2];
            rgba[i * 4 + 3] = 255;
        }
        Ok(DecodedFrame {
            width: media.width,
            height: media.height,
            rgba,
        })
    }
}

/// 扫描 Annex-B 码流，返回每个 NAL 单元（含起始码）的字节范围 `[start, end)`。
fn nal_ranges(packet: &[u8]) -> Vec<(usize, usize)> {
    let n = packet.len();
    let mut ranges = Vec::new();
    let mut i = 0usize;
    while i + 2 < n {
        let sc_len = if i + 3 < n
            && packet[i] == 0
            && packet[i + 1] == 0
            && packet[i + 2] == 0
            && packet[i + 3] == 1
        {
            4
        } else if packet[i] == 0 && packet[i + 1] == 0 && packet[i + 2] == 1 {
            3
        } else {
            i += 1;
            continue;
        };
        let nal_start = i + sc_len;
        // 找到下一个起始码作为本 NAL 的结束。
        let mut j = nal_start;
        let mut end = n;
        while j + 2 < n {
            let is_4byte = j + 3 < n
                && packet[j] == 0
                && packet[j + 1] == 0
                && packet[j + 2] == 0
                && packet[j + 3] == 1;
            let is_3byte = packet[j] == 0 && packet[j + 1] == 0 && packet[j + 2] == 1;
            if is_4byte || is_3byte {
                end = j;
                break;
            }
            j += 1;
        }
        ranges.push((i, end));
        if end == n {
            break;
        }
        i = end;
    }
    ranges
}

/// 在 `cache` 中追加 `packet` 里出现的 SPS(7)/PPS(8) NAL 字节。
fn cache_sps_pps(packet: &[u8], cache: &mut Vec<u8>) {
    for (s, e) in nal_ranges(packet) {
        if e <= s || e > packet.len() {
            continue;
        }
        let sc_len = if s + 3 < packet.len()
            && packet[s] == 0
            && packet[s + 1] == 0
            && packet[s + 2] == 0
            && packet[s + 3] == 1
        {
            4
        } else {
            3
        };
        let nal_byte = s + sc_len;
        if nal_byte < packet.len() {
            let t = packet[nal_byte] & 0x1F;
            if t == 7 || t == 8 {
                cache.extend_from_slice(&packet[s..e]);
            }
        }
    }
}

/// `packet` 是否包含 SPS/PPS NAL。
fn packet_has_sps_pps(packet: &[u8]) -> bool {
    for (s, _e) in nal_ranges(packet) {
        let sc_len = if s + 3 < packet.len()
            && packet[s] == 0
            && packet[s + 1] == 0
            && packet[s + 2] == 0
            && packet[s + 3] == 1
        {
            4
        } else {
            3
        };
        let nal_byte = s + sc_len;
        if nal_byte < packet.len() {
            let t = packet[nal_byte] & 0x1F;
            if t == 7 || t == 8 {
                return true;
            }
        }
    }
    false
}

/// `packet` 是否包含 IDR 帧（NAL type 5）。仅 IDR 帧需要在缺参数集时补齐 SPS/PPS。
fn packet_has_idr(packet: &[u8]) -> bool {
    for (s, _e) in nal_ranges(packet) {
        let sc_len = if s + 3 < packet.len()
            && packet[s] == 0
            && packet[s + 1] == 0
            && packet[s + 2] == 0
            && packet[s + 3] == 1
        {
            4
        } else {
            3
        };
        let nal_byte = s + sc_len;
        if nal_byte < packet.len() {
            let t = packet[nal_byte] & 0x1F;
            if t == 5 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_encode::Encoder;
    use rdcore_proto::VideoCodec;

    #[test]
    fn raw_decoder_produces_rgba() {
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 8,
            height: 4,
            data: vec![0xAB; 8 * 4 * 4],
        };
        let out = RawDecoder.decode(&f).unwrap();
        assert_eq!(out.width, 8);
        assert_eq!(out.height, 4);
        assert_eq!(out.rgba.len(), 8 * 4 * 4);
        assert_eq!(out.rgba[0], 0xAB);
    }

    #[test]
    fn raw_decoder_rejects_bad_codec() {
        let f = MediaFrame {
            codec: VideoCodec::H264,
            width: 2,
            height: 2,
            data: vec![0; 4],
        };
        assert!(matches!(
            RawDecoder.decode(&f),
            Err(DecodeError::UnsupportedCodec(VideoCodec::H264))
        ));
    }

    #[test]
    fn raw_decoder_rejects_bad_length() {
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 2,
            height: 2,
            data: vec![0; 3],
        };
        assert!(matches!(
            RawDecoder.decode(&f),
            Err(DecodeError::InvalidFrame { .. })
        ));
    }

    #[test]
    fn h264_decoder_rejects_bad_codec() {
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 2,
            height: 2,
            data: vec![0; 4],
        };
        let dec = H264Decoder::new().expect("init decoder");
        assert!(matches!(
            dec.decode(&f),
            Err(DecodeError::UnsupportedCodec(VideoCodec::Raw))
        ));
    }

    /// 多帧连续解码：每帧均为独立 IDR，像素应在容差内一致（抗丢帧 / 任意帧独立可解）。
    #[test]
    fn h264_multi_frame_decode_keeps_sps_pps() {
        use rdcore_encode::H264Encoder;

        let w = 32u32;
        let h = 24u32;
        let enc = H264Encoder::new(w, h).expect("init encoder");
        let dec = H264Decoder::new().expect("init decoder");

        for k in 0..4u8 {
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for i in 0..(w * h) as usize {
                rgba[i * 4] = k.wrapping_mul(40);
                rgba[i * 4 + 1] = 200u8.wrapping_sub(k.wrapping_mul(10));
                rgba[i * 4 + 2] = k.wrapping_add(50);
                rgba[i * 4 + 3] = 255;
            }
            let f = MediaFrame {
                codec: VideoCodec::Raw,
                width: w,
                height: h,
                data: rgba.clone(),
            };
            let enc_frame = enc.encode(&f).expect("encode");
            let out = dec.decode(&enc_frame).expect("decode");
            assert_eq!(out.width, w);
            assert_eq!(out.height, h);
            assert_eq!(out.rgba.len(), (w * h * 4) as usize);

            // 每帧独立 IDR，像素应当可还原（H.264 有损，高码率下容差宽松）。
            let mut max_diff = 0i32;
            for (a, b) in rgba.iter().zip(out.rgba.iter()) {
                let d = (*a as i32 - *b as i32).abs();
                if d > max_diff {
                    max_diff = d;
                }
            }
            println!("multi-frame frame#{k} max_diff={max_diff}");
            assert!(max_diff <= 48, "第 {k} 帧最大通道误差 {max_diff} 过大");
        }
    }
}
