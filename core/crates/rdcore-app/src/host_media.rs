//! Track A 媒体面：Host 抓取→编码→媒体通道发送循环。
//!
//! 与 Track B 的 `connection_lifecycle.rs` 物理隔离：本文件只依赖 `rdcore-capture` /
//! `rdcore-media` / `rdcore-encode` / `rdcore-proto`，不碰握手、加密细节、身份、信令加固。
//!
//! 设计要点（与「冻结契约」对齐）：
//! - `CaptureSource` 产出 `MediaFrame`（Raw RGBA）；本循环先经 [`rdcore_encode::RawEncoder`]
//!   编码，再交给 `FrameSink` 发送。`FrameSink` 有两种实现：
//!   - `SocketMediaChannel<InMemoryTransport>`：裸媒体通道（headless 测试用，不加密）；
//!   - `Arc<Connection>`：经 [`crate::Connection::send_media`] 发送，**像素走 E2E 加密**
//!     （生产路径，与 `recv_media` 解密对称）。
//! - 后端无关：`NullCaptureSource`（headless）与 `ScrapCaptureSource`（真实抓屏，`real` feature）
//!   都满足同一个 `CaptureSource` trait，因此本循环对两者一视同仁。

#![allow(clippy::manual_async_fn)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

use rdcore_capture::CaptureSource;
use rdcore_encode::Encoder;
use rdcore_media::{InMemoryTransport, MediaChannel, MediaChannelError, SocketMediaChannel};
use rdcore_proto::{MediaFrame, VideoCodec};

use crate::Connection;

/// 帧发送落点：把（已编码的）`MediaFrame` 发出去。
///
/// 抽出来是为了让 [`HostMediaPump`] 既能发到裸媒体通道（测试），也能经 `Connection`
/// 走 E2E 加密（生产），两者零逻辑差异、互不依赖。
pub trait FrameSink: Send + Sync + 'static {
    /// 发送一帧已编码视频。通道关闭返回 `Err`。
    fn send_encoded<'a>(
        &'a self,
        frame: &'a MediaFrame,
    ) -> impl std::future::Future<Output = Result<(), MediaChannelError>> + Send;
}

/// 裸媒体通道实现（headless / 测试）：直接 `send_frame`，不做额外加密。
impl FrameSink for SocketMediaChannel<InMemoryTransport> {
    fn send_encoded<'a>(
        &'a self,
        frame: &'a MediaFrame,
    ) -> impl std::future::Future<Output = Result<(), MediaChannelError>> + Send {
        async move { self.send_frame(frame).await }
    }
}

/// 生产路径：经 `Connection::send_media` 发送，**像素走端到端 AEAD 加密**。
impl FrameSink for Arc<Connection> {
    fn send_encoded<'a>(
        &'a self,
        frame: &'a MediaFrame,
    ) -> impl std::future::Future<Output = Result<(), MediaChannelError>> + Send {
        async move {
            // 错误逐帧上报会刷屏；发送任务侧已做节流日志，这里只映射错误类型。
            self.send_media(frame)
                .await
                .map_err(|_| MediaChannelError::Closed)
        }
    }
}

/// 自适应降帧档位：60→45→30，30 为地板（再低由既有背压丢帧机制兜底，保实时）。
fn step_down_fps(fps: u16) -> Option<u16> {
    match fps {
        f if f > 45 => Some(45),
        f if f > 30 => Some(30),
        _ => None,
    }
}

/// Host 媒体泵：把 `CaptureSource` 产出的帧按 `fps` 编码并循环发送到 `FrameSink`。
///
/// 连接建立（E2E 加密已就绪）后由 [`crate::Connection::start_capture`] 启动（产线经 `Arc<Connection>`
/// 走加密）；headless 测试也可直接传裸 `MediaChannel`。抓到 `None`（源结束）或收到 stop 信号即退出。
///
/// 编码器由 `codec` 决定（`Raw` 直通 / `H264` 经 openh264），并在首帧到达时按其实际尺寸惰性构造，
/// 因此无论 `NullCaptureSource` 还是 `ScrapCaptureSource` 都能即插即用。
///
/// **线程模型**：捕获 + 编码跑在**专用 OS 线程**，编码后的帧经 `mpsc` 交给一个 tokio 任务经
/// `FrameSink` 发出。原因：真实 `CaptureSource`（如 `scrap` 的 DXGI `Capturer`）持有 COM 裸指针，
/// 是 `!Send` 的，**不能**跨 tokio 任务的线程边界移动；用 channel 把帧（纯数据，`Send`）交给
/// 发送端即可两全。这一改动让 `C: CaptureSource` 不再要求 `Send`。
pub struct HostMediaPump {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HostMediaPump {
    /// 启动后台抓取→编码→发送循环（factory 形式）。
    ///
    /// - `factory`：在**捕获线程内**构造帧来源 `C`（`|| C`）。关键：真实 `CaptureSource`
    ///   （如 `ScrapCaptureSource`，包裹 scrap 的 DXGI `Capturer`，持有 COM 裸指针，`!Send`）
    ///   必须在捕获线程内就地构造，绝不能从其它线程 move 过来。`factory` 自身必须 `Send`
    ///   （即它不应捕获任何 `!Send` 数据），但产出的 `C` 可以 `!Send`。
    /// - `sink`：发送落点（产线传 `Arc<Connection>` 走 E2E 加密；测试传裸 `MediaChannel`）。
    /// - `fps`：目标帧率（至少 1）。
    /// - `codec`：目标视频编解码器（`Raw` / `H264`）。
    /// - `keyframe_flag`：「下一帧必须编码为 IDR」的共享一次性标志（P 帧流的恢复抓手）。
    ///   泵在每帧编码前消费它并调 `Encoder::request_keyframe`；泵内背压丢帧也会自行
    ///   置位（丢帧破坏 P 帧参考链，下一帧 IDR 让 Viewer 最坏滞后一帧自愈）。
    pub fn start_with<C, F, S>(
        factory: F,
        sink: S,
        fps: u16,
        codec: VideoCodec,
        keyframe_flag: Arc<AtomicBool>,
    ) -> Self
    where
        C: CaptureSource + 'static,
        F: FnOnce() -> C + Send + 'static,
        S: FrameSink,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = mpsc::sync_channel::<MediaFrame>(4);
        // fps / interval 可变：自适应降帧会下调（见循环内 EMA 评估）。
        let mut fps = fps.max(1);
        let mut interval = Duration::from_secs_f64(1.0 / (fps as f64));

        // 捕获 + 编码在专用 OS 线程跑：线程内 `factory()` 就地构造 `C`（可能 !Send），
        // 编码后的帧经 channel 交给 async 发送端（经 FrameSink 走 E2E 加密）。
        let stop_cap = stop.clone();
        let capture_thread = thread::spawn(move || {
            let mut capture = factory();
            let mut encoder: Option<Box<dyn Encoder + Send + Sync>> = None;
            let mut seq = 0u64;
            let mut dropped = 0u64;
            // 硬件编码器“初始化成功但逐帧编码失败”的连续失败计数；达阈值后重建为软编兜底。
            let mut hw_fail = 0u32;
            // 编码耗时 EMA（秒）：自适应降帧依据。软编机器跑 60fps 时编码耗时会顶满
            // 帧间隔，EMA 超帧间隔 80% 即降一档（只降不升防抖动）。
            let mut enc_cost_ema: Option<f64> = None;
            eprintln!("[host-media] pump thread started (codec={codec:?}, fps={fps})");
            loop {
                if stop_cap.load(Ordering::SeqCst) {
                    eprintln!("[host-media] stop flag set, exiting after {seq} frames");
                    break;
                }
                // 本轮起始时刻：底部节流以此为基准对齐帧间隔（见 loop 尾注释）。
                let frame_start = Instant::now();
                match capture.next_frame() {
                    Some(frame) => {
                        // 首帧到达时按实际尺寸惰性构造编码器；构造失败跳过该帧。
                        if encoder.is_none() {
                            match rdcore_encode::new_encoder_with_fps(
                                codec,
                                frame.width,
                                frame.height,
                                fps,
                            ) {
                                Ok(e) => {
                                    eprintln!(
                                        "[host-media] encoder ready: {}x{} {codec:?} via {}",
                                        frame.width,
                                        frame.height,
                                        e.kind()
                                    );
                                    encoder = Some(e);
                                }
                                Err(e) => {
                                    if seq == 0 {
                                        eprintln!(
                                            "[host-media] encoder init failed ({}x{} {codec:?}): {e}",
                                            frame.width, frame.height
                                        );
                                    }
                                }
                            }
                        }
                        // 编码（失败跳过该帧，不崩溃泵）；成功则交给发送端。
                        if let Some(enc) = encoder.as_ref() {
                            // 消费「下一帧 IDR」请求（Viewer 关键帧请求 / DC 缓冲丢帧 /
                            // 下方背压丢帧自愈）。编码器新建 / 重建的首帧自带 IDR，无需请求。
                            if keyframe_flag.swap(false, Ordering::SeqCst) {
                                enc.request_keyframe();
                            }
                            let t_enc = Instant::now();
                            let enc_result = enc.encode(&frame);
                            if enc_result.is_ok() {
                                let cost = t_enc.elapsed().as_secs_f64();
                                enc_cost_ema = Some(match enc_cost_ema {
                                    None => cost,
                                    Some(e) => 0.9 * e + 0.1 * cost,
                                });
                            }
                            match enc_result {
                                Ok(encoded) => {
                                    // 发送端跟不上（拥塞 / 对端死透）时最多重试 ~50ms，仍满则
                                    // 丢帧保实时。绝不能无限阻塞：捕获线程一旦楔死，stop 标志
                                    // 无人检查、join 永不返回，整个泵连同重连路径一起卡死。
                                    // （std 的 SyncSender::send_timeout 尚不稳定，手写重试。）
                                    let mut pending = Some(encoded);
                                    let mut attempts = 0u32;
                                    let mut disconnected = false;
                                    while let Some(m) = pending.take() {
                                        match frame_tx.try_send(m) {
                                            Ok(()) => {}
                                            Err(mpsc::TrySendError::Full(m)) => {
                                                attempts += 1;
                                                if attempts >= 50 {
                                                    dropped += 1;
                                                    // 丢帧破坏 P 帧参考链：请求下一帧
                                                    // IDR，Viewer 最坏滞后一帧自愈。
                                                    keyframe_flag.store(true, Ordering::SeqCst);
                                                    if dropped == 1 || dropped % 100 == 0 {
                                                        eprintln!(
                                                            "[host-media] backpressure, dropped {dropped} frames so far"
                                                        );
                                                    }
                                                } else {
                                                    pending = Some(m);
                                                    thread::sleep(Duration::from_millis(1));
                                                }
                                            }
                                            Err(mpsc::TrySendError::Disconnected(_)) => {
                                                disconnected = true;
                                            }
                                        }
                                    }
                                    if disconnected {
                                        eprintln!("[host-media] frame channel closed at seq {seq}");
                                        break; // 接收端已关闭（连接断开 / 停止）
                                    }
                                }
                                Err(e) => {
                                    if seq < 3 {
                                        eprintln!("[host-media] encode error at seq {seq}: {e}");
                                    }
                                    // 运行时兜底：硬件编码器初始化成功却逐帧编码失败时，连续失败
                                    // 达阈值即重建为软编，避免整路无视频（如本机 GPU/MF 编码异常）。
                                    if encoder.as_ref().map(|e| e.kind()) == Some("h264-hardware") {
                                        hw_fail += 1;
                                        if hw_fail == 30 {
                                            eprintln!(
                                                "[host-media] HW encode failed 30 frames, falling back to software encoder"
                                            );
                                            match rdcore_encode::new_encoder_forced_with_fps(
                                                codec,
                                                frame.width,
                                                frame.height,
                                                true,
                                                fps,
                                            ) {
                                                Ok(sw) => {
                                                    encoder = Some(sw);
                                                    hw_fail = 0;
                                                }
                                                Err(err) => eprintln!(
                                                    "[host-media] software fallback failed: {err}"
                                                ),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // 每 30 帧评估一次自适应降帧：编码耗时 EMA 超帧间隔 80% 时
                        // 降一档（60→45→30，地板 30，只降不升）并重建编码器，
                        // 使其时间基 / GOP / 码控参考跟随新帧率。实测 openh264 软编
                        // 720p 仅 ~47fps（见 rdcore-encode/examples/enc_bench.rs），
                        // 无 GPU 机器跑 60fps 全靠此机制兜底。
                        if codec == VideoCodec::H264 && seq > 0 && seq % 30 == 0 {
                            if let Some(ema) = enc_cost_ema {
                                if ema > interval.as_secs_f64() * 0.8 {
                                    if let Some(new_fps) = step_down_fps(fps) {
                                        eprintln!(
                                            "[host-media] 编码耗时 EMA {:.1}ms 超帧间隔 80%（{:.1}ms），{fps}→{new_fps}fps 自适应降档",
                                            ema * 1000.0,
                                            interval.as_secs_f64() * 1000.0
                                        );
                                        fps = new_fps;
                                        interval = Duration::from_secs_f64(1.0 / fps as f64);
                                        // 保留当前硬/软编选择（硬编失败已回退过的不再试硬编）。
                                        let force_sw = encoder.as_ref().map(|e| e.kind())
                                            != Some("h264-hardware");
                                        match rdcore_encode::new_encoder_forced_with_fps(
                                            codec,
                                            frame.width,
                                            frame.height,
                                            force_sw,
                                            fps,
                                        ) {
                                            Ok(e2) => {
                                                eprintln!(
                                                    "[host-media] encoder rebuilt at {fps}fps via {}",
                                                    e2.kind()
                                                );
                                                encoder = Some(e2);
                                            }
                                            Err(err) => eprintln!(
                                                "[host-media] encoder rebuild at {fps}fps failed: {err}（沿用旧编码器）"
                                            ),
                                        }
                                        enc_cost_ema = None; // 新编码器重新累积
                                    }
                                }
                            }
                        }
                        seq += 1;
                    }
                    None => {
                        eprintln!("[host-media] capture source ended after {seq} frames");
                        break; // 源结束（如流停止）
                    }
                }
                // 按绝对截止时间节流：本轮 抓帧+编码 耗时计入帧间隔，只补睡剩余部分。
                // 旧实现 sleep(interval) 在抓帧+编码之后再睡满整帧间隔，实际周期 =
                // 抓帧耗时 + 编码耗时 + interval，目标 60fps 时往往只有 15~25fps，
                // 且帧到达稀疏不均，Viewer 侧表现为卡顿与操作响应滞后。
                let elapsed = frame_start.elapsed();
                if elapsed < interval {
                    thread::sleep(interval - elapsed);
                }
                if stop_cap.load(Ordering::SeqCst) {
                    eprintln!("[host-media] stop flag set, exiting after {seq} frames");
                    break;
                }
            }
        });

        // 发送端在 tokio 任务：把帧经 FrameSink 发出（Connection 走 E2E 加密）。
        // 发送失败只丢帧、不退出：瞬时拥塞可自动恢复；对端死透由上层 stop 收束
        // （`send_encoded` 自身有 5s 上限，见 WebRtcDataChannelTransport::SEND_TIMEOUT）。
        let stop_tx = stop.clone();
        let handle = tokio::spawn(async move {
            let mut sent = 0u64;
            let mut send_errs = 0u64;
            loop {
                if stop_tx.load(Ordering::SeqCst) {
                    break;
                }
                match frame_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(frame) => {
                        match sink.send_encoded(&frame).await {
                            Ok(()) => {
                                if sent == 0 {
                                    eprintln!("[host-media] first frame sent OK");
                                }
                                sent += 1;
                            }
                            Err(e) => {
                                send_errs += 1;
                                if send_errs == 1 || send_errs % 50 == 0 {
                                    eprintln!("[host-media] send_encoded error x{send_errs}: {e}");
                                }
                                // 丢帧继续。
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break, // 捕获线程已退出
                }
            }
            eprintln!(
                "[host-media] sender task exiting after {sent} frames ({send_errs} send errors)"
            );
            let _ = capture_thread.join();
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// 便捷形式：直接接收已构造的 `Send` 捕获源（headless 测试等）。
    ///
    /// 内部包成 `move || capture` 工厂转发给 [`HostMediaPump::start_with`]；关键帧请求
    /// 标志用全新的私有 `Arc`（测试场景无外部置位方）。
    pub fn start<C, S>(capture: C, sink: S, fps: u16, codec: VideoCodec) -> Self
    where
        C: CaptureSource + Send + 'static,
        S: FrameSink,
    {
        Self::start_with(
            move || capture,
            sink,
            fps,
            codec,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// 停止循环并等待后台任务退出。
    pub async fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for HostMediaPump {
    fn drop(&mut self) {
        // 不能在此 await；直接置位停止标志并中止任务（发送端任务被中止会丢弃接收端，
        // 捕获线程的 frame_tx.send 随即返回 Err 而自行退出，无泄漏）。
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_capture::NullCaptureSource;
    use rdcore_decode::{Decoder, H264Decoder};
    use rdcore_media::{media_channel_pair, InMemoryMediaChannel};

    #[test]
    fn step_down_fps_ladder() {
        assert_eq!(step_down_fps(60), Some(45));
        assert_eq!(step_down_fps(50), Some(45));
        assert_eq!(step_down_fps(45), Some(30));
        assert_eq!(step_down_fps(31), Some(30));
        assert_eq!(step_down_fps(30), None); // 地板，不再降
        assert_eq!(step_down_fps(24), None);
    }

    #[tokio::test]
    async fn pump_sends_captured_frames_to_channel() {
        // Host 侧抓 5 帧纯色帧，经进程内媒体通道发给 Viewer 侧。
        let (host_mc, viewer_mc): (InMemoryMediaChannel, InMemoryMediaChannel) =
            media_channel_pair();
        let capture = NullCaptureSource::new(16, 12, 5, 0x33);
        let mut pump = HostMediaPump::start(capture, host_mc, 1000, VideoCodec::Raw);

        let mut got = 0u32;
        for _ in 0..5 {
            match viewer_mc.recv_frame().await.unwrap() {
                Some(f) => {
                    assert_eq!(f.width, 16);
                    assert_eq!(f.height, 12);
                    assert_eq!(f.data.len(), 16 * 12 * 4);
                    got += 1;
                }
                None => break,
            }
        }
        pump.stop().await;
        assert_eq!(got, 5, "应经媒体通道收到全部 5 帧");
    }

    #[tokio::test]
    async fn pump_stops_on_source_end() {
        // 捕获源只给 3 帧后返回 None，泵应自然退出（handle 可 join）。
        let (host_mc, viewer_mc) = media_channel_pair();
        let capture = NullCaptureSource::new(8, 8, 3, 0x01);
        let mut pump = HostMediaPump::start(capture, host_mc, 1000, VideoCodec::Raw);

        let mut got = 0u32;
        while let Some(f) = viewer_mc.recv_frame().await.unwrap() {
            assert_eq!(f.width, 8);
            got += 1;
            if got >= 3 {
                break;
            }
        }
        // 等泵自然退出（源结束后循环 break）。
        pump.stop().await;
        assert_eq!(got, 3);
    }

    /// 全链路 in-process 验证：NullCaptureSource -> H.264 编码 -> 媒体通道 -> H.264 解码 -> RGBA。
    /// 对应 Track A 媒体面「抓屏→编码→传输→解码→渲染」整条管线（headless 也能跑通）。
    #[tokio::test]
    async fn pipeline_null_capture_h264_end_to_end() {
        let (host_mc, viewer_mc): (InMemoryMediaChannel, InMemoryMediaChannel) =
            media_channel_pair();
        // 纯色帧经 H.264 有损压缩后，解码像素应在容差内一致。
        // 注：openh264 对极小分辨率（< 宏块对齐）可能拒绝，故用 64x48（与单测一致）。
        let capture = NullCaptureSource::new(64, 48, 5, 0x55);
        let mut pump = HostMediaPump::start(capture, host_mc, 1000, VideoCodec::H264);

        let dec = H264Decoder::new().expect("init decoder");
        let mut got = 0u32;
        for _ in 0..5 {
            match viewer_mc.recv_frame().await.unwrap() {
                Some(f) => {
                    assert_eq!(f.codec, VideoCodec::H264, "H.264 管线应产出 H264 帧");
                    let decoded = dec.decode(&f).expect("decode");
                    assert_eq!(decoded.width, 64);
                    assert_eq!(decoded.height, 48);
                    // 纯色 0x55：RGB 通道应接近原色（容差内）；alpha 由解码器固定为 255。
                    // 注意 NullCaptureSource 把 4 个通道都填成 0x55，但 H.264 只编码 RGB，
                    // 故 alpha 通道不应参与误差比较（否则会恒定差 170）。
                    let mut max_diff = 0i32;
                    for px in decoded.rgba.chunks(4) {
                        for &channel in &px[0..3] {
                            let d = (channel as i32 - 0x55i32).abs();
                            if d > max_diff {
                                max_diff = d;
                            }
                        }
                        assert_eq!(px[3], 255, "解码后 alpha 应为不透明");
                    }
                    assert!(max_diff <= 48, "纯色帧 H.264 误差 {max_diff} 过大");
                    got += 1;
                }
                None => break,
            }
        }
        pump.stop().await;
        assert_eq!(got, 5, "应经媒体通道收到并解码全部 5 帧");
    }
}
