//! rdcore-audio — C 音频管线：真实音频捕获 / 播放的边界 crate。
//!
//! 架构上 Host 端需要两件事（与视频完全平行）：
//! - **捕获**：把本机音频设备变成 `AudioFrame`（对应 WebRTC 的 RTP 音频源）；
//! - **播放**：把收到的 `AudioFrame` 作用到本机扬声器（对应 WebRTC 的 RTP 音频 sink）。
//!
//! 这两条在 P1/P3 用合成后端（`NullAudioSource` / `SyntheticAudioSource` / `NullAudioSink`）
//! 跑通了管线。本 crate 把"真实后端"的**接缝**定义出来，并给出 headless 的 `Null*` 实现：
//!
//! - [`AudioSource`]：产出 `AudioFrame` 的 trait。真实后端用 `cpal` 采集麦克风 / 系统声音。
//! - [`AudioSink`]：把 `AudioFrame` 作用到本机扬声器的 trait。真实后端用 `cpal` 播放。
//! - [`NullAudioSource`] / [`NullAudioSink`]：headless / 测试用的无操作实现
//!   （返回合成 PCM / 记录但不真正播放）。
//! - [`SyntheticAudioSource`]：生成确定性正弦波 PCM，便于往返保真测试。
//! - [`AudioEncoder`] / [`AudioDecoder`]：编码抽象。`Raw` 直通（零依赖、可测）；`Opus`
//!   走 `real` feature（需 libopus）。与视频的 `rdcore_encode`/`rdcore_decode` 平行。
//! - `feature = "real"`：真实后端（cpal + opus）的接入点；**默认不启用**，以免引入原生 SDK
//!   构建依赖。启用后由 `src/real.rs` 提供 `CpalAudioSource` / `CpalAudioSink` / `OpusEncoder`
//!   / `OpusDecoder` 的真实实现——这是音频原生落地的就绪代码。

use rdcore_proto::{AudioCodec, AudioFrame};

/// 音频管线错误。
#[derive(Debug, Clone, PartialEq)]
pub enum AudioError {
    /// 播放失败（设备不可用 / 写入失败），携带原因。
    Playback(String),
    /// 编码失败，携带原因。
    Encode(String),
    /// 解码失败，携带原因。
    Decode(String),
    /// 该编解码器在当前构建不可用（如 `Opus` 需启用 `real` feature）。
    UnsupportedCodec,
    /// 当前平台不支持该操作。
    UnsupportedPlatform(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::Playback(s) => write!(f, "audio playback error: {s}"),
            AudioError::Encode(s) => write!(f, "audio encode error: {s}"),
            AudioError::Decode(s) => write!(f, "audio decode error: {s}"),
            AudioError::UnsupportedCodec => write!(f, "audio codec unsupported in this build"),
            AudioError::UnsupportedPlatform(s) => write!(f, "audio platform unsupported: {s}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// 音频捕获源：产出（已编码的）`AudioFrame`。
///
/// 真实后端（feature = "real"）会调用系统音频 API（如 `cpal`、WASAPI、CoreAudio）取一段
/// PCM，打包成 `AudioFrame`；上层（音频通道）拿到后再经 [`AudioEncoder`] 编码（Raw 直通 /
/// Opus 压缩）发送。
///
/// 与 `CaptureSource` 平行：`next_frame` 返回 `None` 表示源结束 / 已停止。
pub trait AudioSource {
    /// 抓取一段音频；无更多帧（如流结束 / 已停止）返回 `None`。
    fn next_frame(&mut self) -> Option<AudioFrame>;
}

/// 音频播放落点：把收到的 `AudioFrame` 作用到本机扬声器（Viewer 端）。
///
/// 真实后端（feature = "real"）会用系统音频 API（如 `cpal`）真正播放；上层（音频通道）
/// 经 [`AudioDecoder`] 解出 Raw PCM 后调 [`AudioSink::play`]。
pub trait AudioSink {
    /// 播放一帧（已解码为 Raw PCM 的）音频。设备不可用返回 `Err`。
    fn play(&mut self, frame: &AudioFrame) -> Result<(), AudioError>;
}

/// Headless 音频源：返回固定尺寸 / 固定填充字节的纯色 PCM 帧（测试 / 无音频设备环境）。
///
/// 不调用任何系统 API，仅用于把 `AudioSource` 管线在 headless 下跑通。默认填充 `0x00`（静音）。
pub struct NullAudioSource {
    channels: u16,
    sample_rate: u32,
    samples_per_frame: u32,
    byte: u8,
    remaining: u32,
}

impl NullAudioSource {
    /// 构造：每帧 `channels` 声道、`sample_rate` Hz、`samples_per_frame` 采样/声道，
    /// 共 `frames` 帧，之后返回 `None`。`byte` 填充每个 PCM 字节（默认 0 = 静音）。
    pub fn new(channels: u16, sample_rate: u32, samples_per_frame: u32, frames: u32, byte: u8) -> Self {
        Self {
            channels,
            sample_rate,
            samples_per_frame,
            byte,
            remaining: frames,
        }
    }
}

impl AudioSource for NullAudioSource {
    fn next_frame(&mut self) -> Option<AudioFrame> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let bytes_per_sample = 2u32; // 16-bit PCM
        let len = (self.samples_per_frame * self.channels as u32 * bytes_per_sample) as usize;
        Some(AudioFrame {
            codec: AudioCodec::Raw,
            channels: self.channels,
            sample_rate: self.sample_rate,
            data: vec![self.byte; len],
        })
    }
}

/// Headless 音频播放器：记录所有播放的帧但不真正作用到扬声器（测试用）。
pub struct NullAudioSink {
    /// 已播放（记录）的帧序列。
    pub played: Vec<AudioFrame>,
}

impl NullAudioSink {
    /// 构造一个空记录器。
    pub fn new() -> Self {
        Self { played: Vec::new() }
    }
}

impl Default for NullAudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for NullAudioSink {
    fn play(&mut self, frame: &AudioFrame) -> Result<(), AudioError> {
        self.played.push(frame.clone());
        Ok(())
    }
}

/// 合成音频源：生成确定性正弦波 16-bit PCM（测试保真往返用）。
///
/// 每帧 `samples_per_frame` 采样/声道，跨帧连续（全局采样序号递增），故拼接后是一段
/// 完整连续正弦波，便于 `recv → decode → 比对` 验证无损/低损。
pub struct SyntheticAudioSource {
    channels: u16,
    sample_rate: u32,
    samples_per_frame: u32,
    freq_hz: f32,
    amplitude: f32,
    remaining: u32,
    /// 已产生的全局采样序号（跨帧连续）。
    global_sample: u32,
}

impl SyntheticAudioSource {
    /// 构造：`channels` 声道、`sample_rate` Hz、每帧 `samples_per_frame` 采样/声道、
    /// 正弦频率 `freq_hz`、振幅 `amplitude`（0..1，对应 i16 峰值）、共 `frames` 帧。
    pub fn new(
        channels: u16,
        sample_rate: u32,
        samples_per_frame: u32,
        freq_hz: f32,
        amplitude: f32,
        frames: u32,
    ) -> Self {
        Self {
            channels,
            sample_rate,
            samples_per_frame,
            freq_hz,
            amplitude,
            remaining: frames,
            global_sample: 0,
        }
    }

    /// 把单个采样序号转成 16-bit 有符号 PCM（所有声道同值，单声道/立体声一致）。
    fn sample_at(&self, n: u32) -> i16 {
        let t = n as f32 / self.sample_rate as f32;
        let v = (2.0 * std::f32::consts::PI * self.freq_hz * t).sin() * self.amplitude;
        (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    }
}

impl AudioSource for SyntheticAudioSource {
    fn next_frame(&mut self) -> Option<AudioFrame> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let ch = self.channels as usize;
        let n = self.samples_per_frame as usize;
        let mut data = Vec::with_capacity(n * ch * 2);
        for _ in 0..n {
            let s = self.sample_at(self.global_sample);
            self.global_sample += 1;
            for _ in 0..ch {
                data.extend_from_slice(&s.to_le_bytes());
            }
        }
        Some(AudioFrame {
            codec: AudioCodec::Raw,
            channels: self.channels,
            sample_rate: self.sample_rate,
            data,
        })
    }
}

/// 音频编码器：把 Raw PCM 帧编码为目标 codec。与视频 `Encoder` 平行。
pub trait AudioEncoder: Send + Sync {
    /// 编码一段 Raw PCM 帧；返回目标 codec 的 `AudioFrame`。
    fn encode(&self, pcm: &AudioFrame) -> Result<AudioFrame, AudioError>;
}

/// 音频解码器：把目标 codec 的帧解出 Raw PCM。与视频 `Decoder` 平行。
pub trait AudioDecoder: Send + Sync {
    /// 解出 Raw PCM 帧（16-bit 交错）。
    fn decode(&self, frame: &AudioFrame) -> Result<AudioFrame, AudioError>;
}

/// Raw 直通编码器：原样返回（PCM 本身就是线格式）。
pub struct RawEncoder;

impl AudioEncoder for RawEncoder {
    fn encode(&self, pcm: &AudioFrame) -> Result<AudioFrame, AudioError> {
        Ok(pcm.clone())
    }
}

/// Raw 直通解码器：原样返回。
pub struct RawDecoder;

impl AudioDecoder for RawDecoder {
    fn decode(&self, frame: &AudioFrame) -> Result<AudioFrame, AudioError> {
        Ok(frame.clone())
    }
}

/// 按目标 codec 构造编码器（Raw 直通可用；Opus 需 `real` feature）。
pub fn new_encoder(codec: AudioCodec) -> Result<Box<dyn AudioEncoder + Send + Sync>, AudioError> {
    match codec {
        AudioCodec::Raw => Ok(Box::new(RawEncoder)),
        #[cfg(feature = "real")]
        AudioCodec::Opus => Ok(Box::new(crate::real::OpusEncoder::new()?)),
        // 非 real 构建：Opus 不可用。
        #[cfg(not(feature = "real"))]
        AudioCodec::Opus => Err(AudioError::UnsupportedCodec),
    }
}

/// 按 codec 构造解码器。
pub fn new_decoder(codec: AudioCodec) -> Result<Box<dyn AudioDecoder + Send + Sync>, AudioError> {
    match codec {
        AudioCodec::Raw => Ok(Box::new(RawDecoder)),
        #[cfg(feature = "real")]
        AudioCodec::Opus => Ok(Box::new(crate::real::OpusDecoder::new()?)),
        #[cfg(not(feature = "real"))]
        AudioCodec::Opus => Err(AudioError::UnsupportedCodec),
    }
}

// 真实后端（cpal 采集/播放 + opus 压缩）：默认不编译（feature 关闭），启用 `real` 时由 `src/real.rs` 提供。
#[cfg(feature = "real")]
mod real;
#[cfg(feature = "real")]
pub use real::{CpalAudioSink, CpalAudioSource, OpusDecoder, OpusEncoder};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_audio_source_yields_frames_then_none() {
        let mut src = NullAudioSource::new(2, 48_000, 960, 3, 0x00);
        assert!(src.next_frame().is_some());
        assert!(src.next_frame().is_some());
        assert!(src.next_frame().is_some());
        assert!(src.next_frame().is_none(), "帧数耗尽后应返回 None");
    }

    #[test]
    fn null_audio_source_frame_has_expected_size() {
        let mut src = NullAudioSource::new(1, 48_000, 480, 1, 0x00);
        let f = src.next_frame().unwrap();
        assert_eq!(f.channels, 1);
        assert_eq!(f.sample_rate, 48_000);
        // 480 采样 × 1 声道 × 2 字节 = 960 字节
        assert_eq!(f.data.len(), 480 * 2);
    }

    #[test]
    fn null_audio_sink_records_frames() {
        let mut sink = NullAudioSink::new();
        let f = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 2,
            sample_rate: 48_000,
            data: vec![0u8; 100],
        };
        sink.play(&f).unwrap();
        assert_eq!(sink.played.len(), 1);
        assert_eq!(sink.played[0].sample_rate, 48_000);
    }

    #[test]
    fn synthetic_source_is_continuous_and_deterministic() {
        // 两帧拼接后应是一段连续正弦：逐采样比对两次独立构造的结果一致。
        let mut src = SyntheticAudioSource::new(1, 48_000, 10, 440.0, 0.5, 2);
        let f1 = src.next_frame().unwrap();
        let f2 = src.next_frame().unwrap();
        assert_eq!(f1.data.len(), 10 * 2);
        assert_eq!(f2.data.len(), 10 * 2);
        // 全局采样 0..10 在 f1，10..20 在 f2；分别用 sample_at 重建验证连续性。
        let check = |f: &AudioFrame, offset: u32| {
            for (i, chunk) in f.data.chunks_exact(2).enumerate() {
                let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                let g = offset + i as u32;
                let expected = src_fixture(1, 48_000, 440.0, 0.5, g);
                assert_eq!(s, expected, "采样 {g} 应等于确定性正弦值");
            }
        };
        check(&f1, 0);
        check(&f2, 10);
    }

    // 复刻 SyntheticAudioSource::sample_at 的逻辑，独立验证（测试隔离）。
    fn src_fixture(channels: u16, sample_rate: u32, freq: f32, amplitude: f32, n: u32) -> i16 {
        let _ = channels;
        let t = n as f32 / sample_rate as f32;
        let v = (2.0 * std::f32::consts::PI * freq * t).sin() * amplitude;
        (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    }

    #[test]
    fn raw_encoder_decoder_are_identity() {
        let f = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 2,
            sample_rate: 48_000,
            data: vec![0xABu8; 3840],
        };
        let enc = RawEncoder.encode(&f).unwrap();
        assert_eq!(enc, f);
        let dec = RawDecoder.decode(&enc).unwrap();
        assert_eq!(dec, f);
    }

    #[test]
    fn new_encoder_opus_unsupported_without_real() {
        // 默认构建没有启用 real feature，Opus 应报 UnsupportedCodec。
        match new_encoder(AudioCodec::Opus) {
            Err(e) => assert_eq!(e, AudioError::UnsupportedCodec),
            Ok(_) => panic!("Opus encoder should be unsupported without real feature"),
        }
        match new_decoder(AudioCodec::Opus) {
            Err(e) => assert_eq!(e, AudioError::UnsupportedCodec),
            Ok(_) => panic!("Opus decoder should be unsupported without real feature"),
        }
    }

    // ── 端到端：Source → 音频通道 → Sink 无损往返（验证 AudioSource/AudioSink 与通道缝兼容）──
    use rdcore_media::{audio_channel_pair, AudioChannel, InMemoryAudioChannel};

    #[tokio::test]
    async fn source_to_channel_to_sink_lossless() {
        let (host, viewer): (InMemoryAudioChannel, InMemoryAudioChannel) = audio_channel_pair();
        // Host 侧：用 SyntheticAudioSource 产 5 帧，经 Raw 编码直接发到通道。
        let mut src = SyntheticAudioSource::new(2, 48_000, 960, 440.0, 0.5, 5);
        let enc = RawEncoder;
        let mut sent = Vec::new();
        while let Some(f) = src.next_frame() {
            let frame = enc.encode(&f).unwrap();
            host.send_frame(&frame).await.unwrap();
            sent.push(frame);
        }
        drop(host);

        // Viewer 侧：从通道收，经 Raw 解码，交给 NullAudioSink "播放"。
        let mut sink = NullAudioSink::new();
        let dec = RawDecoder;
        while let Some(frame) = viewer.recv_frame().await.unwrap() {
            let pcm = dec.decode(&frame).unwrap();
            sink.play(&pcm).unwrap();
        }
        assert_eq!(sink.played.len(), 5, "应播放全部 5 帧");
        assert_eq!(sink.played, sent, "经音频通道应无损往返");
    }
}
