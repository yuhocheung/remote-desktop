//! 真实音频后端（feature = "real"）：cpal 采集/播放 + opus 压缩。
//!
//! 注意：本模块仅在 `real` feature 启用时编译，需要系统音频库与 libopus 开发包。
//! 默认构建（CI）不启用，故此处代码不参与默认构建验证，仅作为原生落地的就绪骨架保留。
//! 真实采集/播放需要后台音频流 + 环形缓冲，下面 `CpalAudioSource`/`CpalAudioSink` 给出
//! 设备枚举与配置装配的起手式，`OpusEncoder`/`OpusDecoder` 为可直接使用的同步编解码封装。

#![allow(dead_code)]

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleRate, StreamConfig};
use opus::{Application, Channels, Decoder as OpusLibDecoder, Encoder as OpusLibEncoder};
use rdcore_proto::{AudioCodec, AudioFrame};

use crate::{AudioDecoder, AudioEncoder, AudioError, AudioSink, AudioSource};

/// 每帧目标采样数（20ms @ 48kHz = 960 采样/声道）。
const SAMPLES_PER_FRAME: u32 = 960;
/// 目标采样率（Hz）。
const SAMPLE_RATE: u32 = 48_000;
/// 目标声道数（立体声）。
const CHANNELS: u16 = 2;

/// 从 16-bit 小端交错的 `Vec<u8>` 取出 `&[i16]` 视图（复制，安全）。
fn i16_from_le_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// 16-bit 小端交错打包。
fn i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// ───────────────────────────── Opus 编解码（可直接使用） ─────────────────────────────

/// Opus 编码器：把 Raw 16-bit 交错 PCM 压缩为 `AudioCodec::Opus` 帧。
pub struct OpusEncoder {
    enc: std::sync::Mutex<OpusLibEncoder>,
}

impl OpusEncoder {
    /// 构造立体声、VoIP 用途的 Opus 编码器（48kHz）。
    pub fn new() -> Result<Self, AudioError> {
        let enc = OpusLibEncoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip)
            .map_err(|e| AudioError::Encode(e.to_string()))?;
        Ok(Self {
            enc: std::sync::Mutex::new(enc),
        })
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&self, pcm: &AudioFrame) -> Result<AudioFrame, AudioError> {
        if pcm.codec != AudioCodec::Raw {
            return Err(AudioError::Encode("opus encoder expects Raw PCM input".into()));
        }
        let samples = i16_from_le_bytes(&pcm.data);
        let mut out = vec![0u8; 4000];
        let n = self
            .enc
            .lock()
            .unwrap()
            .encode(&samples, &mut out)
            .map_err(|e| AudioError::Encode(e.to_string()))?;
        out.truncate(n);
        Ok(AudioFrame {
            codec: AudioCodec::Opus,
            channels: pcm.channels,
            sample_rate: pcm.sample_rate,
            data: out,
        })
    }
}

/// Opus 解码器：把 `AudioCodec::Opus` 帧解出 Raw 16-bit 交错 PCM。
pub struct OpusDecoder {
    dec: std::sync::Mutex<OpusLibDecoder>,
}

impl OpusDecoder {
    /// 构造立体声 Opus 解码器（48kHz）。
    pub fn new() -> Result<Self, AudioError> {
        let dec = OpusLibDecoder::new(SAMPLE_RATE, Channels::Stereo)
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        Ok(Self {
            dec: std::sync::Mutex::new(dec),
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&self, frame: &AudioFrame) -> Result<AudioFrame, AudioError> {
        if frame.codec != AudioCodec::Opus {
            return Err(AudioError::Decode("opus decoder expects Opus input".into()));
        }
        let out_samples = SAMPLES_PER_FRAME as usize * CHANNELS as usize;
        let mut pcm = vec![0i16; out_samples];
        let n = self
            .dec
            .lock()
            .unwrap()
            .decode(Some(&frame.data), &mut pcm, false)
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        Ok(AudioFrame {
            codec: AudioCodec::Raw,
            channels: frame.channels,
            sample_rate: frame.sample_rate,
            data: i16_to_le_bytes(&pcm[..n * CHANNELS as usize]),
        })
    }
}

// ───────────────────────────── cpal 采集 / 播放（就绪骨架） ─────────────────────────────

/// cpal 采集源：装配默认输入设备的 48k 立体声配置。
///
/// 完整实时采集需要后台音频流把采样填入环形缓冲、再由 `next_frame` 取出——此处保留
/// 设备枚举 + 配置装配的起手代码，`next_frame` 暂返回 `None`（未接入实时流）。
pub struct CpalAudioSource {
    #[allow(dead_code)]
    host: cpal::Host,
    #[allow(dead_code)]
    device: cpal::Device,
    config: StreamConfig,
}

impl CpalAudioSource {
    /// 装配默认输入设备（无设备/不支持则报错）。
    pub fn new() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| AudioError::UnsupportedPlatform("no default input device".into()))?;
        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: SampleRate(SAMPLE_RATE),
            buffer_size: BufferSize::Fixed(SAMPLES_PER_FRAME),
        };
        Ok(Self {
            host,
            device,
            config,
        })
    }
}

impl AudioSource for CpalAudioSource {
    fn next_frame(&mut self) -> Option<AudioFrame> {
        // 后台音频流 + 环形缓冲的实时采集为后续工作；当前骨架返回 None。
        let _ = &self.config;
        None
    }
}

/// cpal 播放落点：装配默认输出设备的 48k 立体声配置。
///
/// 完整实时播放需要把 `play` 收到的 PCM 推入输出流环形缓冲——此处保留设备枚举 +
/// 配置装配的起手代码，`play` 暂为 no-op（不真正出声）。
pub struct CpalAudioSink {
    #[allow(dead_code)]
    host: cpal::Host,
    #[allow(dead_code)]
    device: cpal::Device,
    config: StreamConfig,
}

impl CpalAudioSink {
    /// 装配默认输出设备。
    pub fn new() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::UnsupportedPlatform("no default output device".into()))?;
        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: SampleRate(SAMPLE_RATE),
            buffer_size: BufferSize::Fixed(SAMPLES_PER_FRAME),
        };
        Ok(Self {
            host,
            device,
            config,
        })
    }
}

impl AudioSink for CpalAudioSink {
    fn play(&mut self, frame: &AudioFrame) -> Result<(), AudioError> {
        // 实时播放（把 PCM 推入输出流）为后续工作；当前骨架记录配置后 no-op。
        let _ = (&self.config, frame);
        Ok(())
    }
}
