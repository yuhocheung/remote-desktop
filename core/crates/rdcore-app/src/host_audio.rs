//! Track A 音频面：Host 采集→编码→音频通道发送循环。
//!
//! 与视频面 [`crate::host_media`] 物理隔离、结构对称：本文件只依赖 `rdcore-audio` /
//! `rdcore-media` / `rdcore-proto`，不碰握手、加密细节、身份、信令加固。
//!
//! 设计要点（与「冻结契约」对齐）：
//! - `AudioSource` 产出 `AudioFrame`（Raw PCM）。本循环先经 [`rdcore_audio::AudioEncoder`]
//!   编码，再交给 `AudioFrameSink` 发送。`AudioFrameSink` 有两种实现：
//!   - `SocketAudioChannel<InMemoryTransport>`：裸音频通道（headless 测试用，不加密）；
//!   - `Arc<Connection>`：经 [`crate::Connection::send_audio`] 发送，**音频字节走 E2E 加密**
//!     （生产路径，与 `recv_audio` 解密对称）。
//! - 后端无关：`NullAudioSource` / `SyntheticAudioSource`（headless）与 `CpalAudioSource`
//!   （真实采集，`real` feature）都满足同一个 `AudioSource` trait，因此本循环对两者一视同仁。
//! - 音频走独立的 SCTP `audio` 通道（id=2），与视频互不阻塞：音频抖动/丢帧不影响视频流畅度。

#![allow(clippy::manual_async_fn)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tokio::task::JoinHandle;

use rdcore_audio::{AudioEncoder, AudioSource};
use rdcore_media::{AudioChannel, AudioChannelError, InMemoryTransport, SocketAudioChannel};
use rdcore_proto::AudioFrame;

use crate::Connection;

/// 音频帧发送落点：把（已编码的）`AudioFrame` 发出去。
///
/// 抽出来是为了让 [`HostAudioPump`] 既能发到裸音频通道（测试），也能经 `Connection`
/// 走 E2E 加密（生产），两者零逻辑差异、互不依赖。
pub trait AudioFrameSink: Send + Sync + 'static {
    /// 发送一帧已编码音频。通道关闭返回 `Err`。
    fn send_encoded<'a>(
        &'a self,
        frame: &'a AudioFrame,
    ) -> impl std::future::Future<Output = Result<(), AudioChannelError>> + Send;
}

/// 裸音频通道实现（headless / 测试）：直接 `send_frame`，不做额外加密。
impl AudioFrameSink for SocketAudioChannel<InMemoryTransport> {
    fn send_encoded<'a>(
        &'a self,
        frame: &'a AudioFrame,
    ) -> impl std::future::Future<Output = Result<(), AudioChannelError>> + Send {
        async move { self.send_frame(frame).await }
    }
}

/// 生产路径：经 `Connection::send_audio` 发送，**音频字节走端到端 AEAD 加密**。
impl AudioFrameSink for Arc<Connection> {
    fn send_encoded<'a>(
        &'a self,
        frame: &'a AudioFrame,
    ) -> impl std::future::Future<Output = Result<(), AudioChannelError>> + Send {
        async move {
            self.send_audio(frame)
                .await
                .map_err(|_| AudioChannelError::Closed)
        }
    }
}

/// Host 音频泵：把 `AudioSource` 产出的帧按 `fps` 编码并循环发送到 `AudioFrameSink`。
///
/// 连接建立（E2E 加密已就绪）后由 [`crate::Connection::start_audio_capture`] 启动（产线经
/// `Arc<Connection>` 走加密）；headless 测试也可直接传裸 `AudioChannel`。采到 `None`
/// （源结束）或收到 stop 信号即退出。
///
/// 编码器由 `codec` 决定（`Raw` 直通 / `Opus` 经 `real` feature），并在首帧到达时惰性构造，
/// 因此无论 `NullAudioSource` 还是 `CpalAudioSource` 都能即插即用。
///
/// **线程模型**：采集 + 编码跑在**专用 OS 线程**，编码后的帧经 `mpsc` 交给一个 tokio 任务经
/// `AudioFrameSink` 发出。原因与视频泵一致：真实 `AudioSource`（如 `CpalAudioSource` 包裹的
/// cpal 设备句柄）可能持有非 `Send` 资源，必须在采集线程内就地构造，绝不能跨线程 move。
pub struct HostAudioPump {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HostAudioPump {
    /// 启动后台采集→编码→发送循环（factory 形式）。
    ///
    /// - `factory`：在**采集线程内**构造帧来源 `C`（`|| C`）。真实 `AudioSource`
    ///   （如 `CpalAudioSource`）必须在采集线程内就地构造，`factory` 自身必须 `Send`
    ///   （不捕获任何 `!Send` 数据），但产出的 `C` 可以 `!Send`。
    /// - `sink`：发送落点（产线传 `Arc<Connection>` 走 E2E 加密；测试传裸 `AudioChannel`）。
    /// - `fps`：目标帧率（至少 1）。
    /// - `codec`：目标音频编解码器（`Raw` / `Opus`）。
    pub fn start_with<C, F, S>(
        factory: F,
        sink: S,
        fps: u16,
        codec: rdcore_proto::AudioCodec,
    ) -> Self
    where
        C: AudioSource + 'static,
        F: FnOnce() -> C + Send + 'static,
        S: AudioFrameSink,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = mpsc::sync_channel::<AudioFrame>(4);
        let interval = Duration::from_secs_f64(1.0 / (fps.max(1) as f64));

        // 采集 + 编码在专用 OS 线程跑：线程内 `factory()` 就地构造 `C`（可能 !Send），
        // 编码后的帧经 channel 交给 async 发送端（经 AudioFrameSink 走 E2E 加密）。
        let stop_cap = stop.clone();
        let capture_thread = thread::spawn(move || {
            let mut capture = factory();
            let mut encoder: Option<Box<dyn AudioEncoder + Send + Sync>> = None;
            loop {
                if stop_cap.load(Ordering::SeqCst) {
                    break;
                }
                match capture.next_frame() {
                    Some(frame) => {
                        // 首帧到达时按目标 codec 惰性构造编码器；构造失败跳过该帧。
                        if encoder.is_none() {
                            if let Ok(e) = rdcore_audio::new_encoder(codec) {
                                encoder = Some(e);
                            }
                        }
                        // 编码（失败跳过该帧，不崩溃泵）；成功则交给发送端。
                        if let Some(enc) = encoder.as_ref() {
                            if let Ok(encoded) = enc.encode(&frame) {
                                if frame_tx.send(encoded).is_err() {
                                    break; // 接收端已关闭（连接断开 / 停止）
                                }
                            }
                        }
                    }
                    None => break, // 源结束（如流停止）
                }
                thread::sleep(interval);
                if stop_cap.load(Ordering::SeqCst) {
                    break;
                }
            }
        });

        // 发送端在 tokio 任务：把帧经 AudioFrameSink 发出（Connection 走 E2E 加密）。
        let handle = tokio::spawn(async move {
            while let Ok(frame) = frame_rx.recv() {
                if sink.send_encoded(&frame).await.is_err() {
                    break; // 通道关闭 / 连接断开
                }
            }
            let _ = capture_thread.join();
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// 便捷形式：直接接收已构造的 `Send` 采集源（headless 测试等）。
    ///
    /// 内部包成 `move || capture` 工厂转发给 [`HostAudioPump::start_with`]。
    pub fn start<C, S>(capture: C, sink: S, fps: u16, codec: rdcore_proto::AudioCodec) -> Self
    where
        C: AudioSource + Send + 'static,
        S: AudioFrameSink,
    {
        Self::start_with(move || capture, sink, fps, codec)
    }

    /// 停止循环并等待后台任务退出。
    pub async fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for HostAudioPump {
    fn drop(&mut self) {
        // 不能在此 await；直接置位停止标志并中止任务（发送端任务被中止会丢弃接收端，
        // 采集线程的 frame_tx.send 随即返回 Err 而自行退出，无泄漏）。
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_audio::{NullAudioSource, SyntheticAudioSource};
    use rdcore_media::{audio_channel_pair, InMemoryAudioChannel};

    #[tokio::test]
    async fn pump_sends_captured_audio_to_channel() {
        // Host 侧采 5 帧 PCM，经进程内音频通道发给 Viewer 侧。
        let (host_ac, viewer_ac): (InMemoryAudioChannel, InMemoryAudioChannel) =
            audio_channel_pair();
        let capture = NullAudioSource::new(2, 48_000, 960, 5, 0x00);
        let mut pump = HostAudioPump::start(capture, host_ac, 1000, rdcore_proto::AudioCodec::Raw);

        let mut got = 0u32;
        for _ in 0..5 {
            match viewer_ac.recv_frame().await.unwrap() {
                Some(f) => {
                    assert_eq!(f.channels, 2);
                    assert_eq!(f.sample_rate, 48_000);
                    assert_eq!(f.data.len(), 960 * 2 * 2);
                    got += 1;
                }
                None => break,
            }
        }
        pump.stop().await;
        assert_eq!(got, 5, "应经音频通道收到全部 5 帧");
    }

    #[tokio::test]
    async fn pump_stops_on_source_end() {
        // 采集源只给 3 帧后返回 None，泵应自然退出。
        let (host_ac, viewer_ac) = audio_channel_pair();
        let capture = NullAudioSource::new(1, 48_000, 480, 3, 0x00);
        let mut pump = HostAudioPump::start(capture, host_ac, 1000, rdcore_proto::AudioCodec::Raw);

        let mut got = 0u32;
        while let Some(f) = viewer_ac.recv_frame().await.unwrap() {
            assert_eq!(f.channels, 1);
            got += 1;
            if got >= 3 {
                break;
            }
        }
        pump.stop().await;
        assert_eq!(got, 3);
    }

    /// 全链路 in-process 验证：SyntheticAudioSource -> 编码(Raw) -> 音频通道 -> 解码(Raw) -> 比对。
    /// 对应音频面「采集→编码→传输→解码→播放」整条管线（headless 也能跑通保真）。
    #[tokio::test]
    async fn pipeline_synthetic_audio_end_to_end_lossless() {
        let (host_ac, viewer_ac): (InMemoryAudioChannel, InMemoryAudioChannel) =
            audio_channel_pair();
        // 生成 5 帧确定性正弦波，经 Raw 直通应无损往返（逐字节一致）。
        let capture = SyntheticAudioSource::new(2, 48_000, 960, 440.0, 0.5, 5);
        let mut pump = HostAudioPump::start(capture, host_ac, 1000, rdcore_proto::AudioCodec::Raw);

        let mut received = Vec::new();
        for _ in 0..5 {
            match viewer_ac.recv_frame().await.unwrap() {
                Some(f) => received.push(f),
                None => break,
            }
        }
        pump.stop().await;

        assert_eq!(received.len(), 5, "应经音频通道收到全部 5 帧");
        // 重新合成期望的 PCM 并逐字节比对（Raw 直通应完全无损）。
        let mut expected_src = SyntheticAudioSource::new(2, 48_000, 960, 440.0, 0.5, 5);
        for (i, got) in received.iter().enumerate() {
            let exp = expected_src.next_frame().unwrap();
            assert_eq!(got.codec, rdcore_proto::AudioCodec::Raw);
            assert_eq!(got.channels, exp.channels);
            assert_eq!(got.sample_rate, exp.sample_rate);
            assert_eq!(got.data, exp.data, "第 {i} 帧 Raw PCM 应逐字节一致（无损）");
        }
    }
}
