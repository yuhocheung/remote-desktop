//! 帧元数据与能力协商（协商出"传什么画面、支持什么输入"）。

use serde::{Deserialize, Serialize};

/// 编码后的媒体帧（视频负载）。
///
/// 这是"线上契约"类型（与 `Message` 同级），所以从 P3 起由 `rdcore-media` 的
/// `MediaChannel` 承载，并经 postcard 编解码在媒体通道上传输。
///
/// 与 `Frame`（解码后的 RGBA 画面）不同，`MediaFrame` 是"已编码、可传输"的形态；
/// `Raw` 编解码时其 `data` 等价于 RGBA 像素，真实 H.264/H.265 则是压缩字节。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFrame {
    /// 编解码器（决定 `data` 的语义）。
    pub codec: VideoCodec,
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 编码后的字节。
    pub data: Vec<u8>,
}

/// 编码后的音频帧（音频负载）。
///
/// 与 `MediaFrame` 平行，是音频通道（`AudioChannel`）的"线上契约"类型：已编码、可传输。
///
/// - `Raw`：`data` 为 16-bit 有符号 PCM（小端、通道交错），单帧采样数
///   = `data.len() / (channels * 2)`。
/// - `Opus`：`data` 为 Opus 压缩字节流（由 `rdcore-audio` 在 `real` feature 下编解码）。
///
/// 与视频不同，音频帧不携带宽高，而携带 `channels` / `sample_rate`，供 Viewer 正确重放。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFrame {
    /// 编解码器（决定 `data` 的语义）。
    pub codec: AudioCodec,
    /// 通道数（1 = 单声道，2 = 立体声）。
    pub channels: u16,
    /// 采样率（Hz，如 48000）。
    pub sample_rate: u32,
    /// 编码后的字节。
    pub data: Vec<u8>,
}

/// 屏幕流可协商的视频编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    /// H.264（兼容性最好，硬件加速普遍）。
    H264,
    /// H.265 / HEVC（更省带宽）。
    H265,
    /// VP8（WebRTC 传统选择）。
    Vp8,
    /// VP9。
    Vp9,
    /// AV1（最新、最省带宽，但算力要求高）。
    Av1,
    /// 未压缩（仅调试/兜底用，带宽极大）。
    Raw,
}

/// 设备音频可协商的音频编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    /// Opus（远程桌面音频事实标准，低延迟、高压缩；`rdcore-audio` 的 `real` feature 下可用）。
    Opus,
    /// 未压缩 16-bit PCM（仅调试/兜底、或零依赖的回环测试用，带宽极大）。
    Raw,
}

/// 协商后或实际观测到的屏幕流格式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameMetadata {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 帧率（fps）。
    pub fps: u16,
    /// 编解码器。
    pub codec: VideoCodec,
}

/// 端点支持哪些输入类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCaps {
    /// 是否支持鼠标。
    pub mouse: bool,
    /// 是否支持键盘。
    pub keyboard: bool,
    /// 是否支持滚轮。
    pub wheel: bool,
}

/// 一次连接中，某一方所能提供的端到端能力。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// 支持的视频编解码器列表。
    pub video_codecs: Vec<VideoCodec>,
    /// 最大宽。
    pub max_width: u32,
    /// 最大高。
    pub max_height: u32,
    /// 可提供的帧率上限。
    pub fps: u16,
    /// 是否支持剪贴板同步。
    pub clipboard: bool,
    /// 支持的输入类别。
    pub input: InputCaps,
}
