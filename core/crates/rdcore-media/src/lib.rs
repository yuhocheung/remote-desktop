//! rdcore-media — P3 媒体 / 数据通道抽象 + 传输边界。
//!
//! 架构文档 §1/§5 把一条远程桌面连接拆成三条互相独立的通道：
//! - **信令通道**（WebSocket，已在 `rdcore-signaling` 实现）：只传 SDP/ICE（Offer/Answer/Ice），
//!   云端控制面全程看不到媒体或输入内容。
//! - **媒体通道**（`MediaChannel`）：承载屏幕视频流（`MediaFrame`），对应 WebRTC 的 RTP video。
//! - **音频通道**（`AudioChannel`）：承载设备音频流（`AudioFrame`），对应 WebRTC 的 RTP audio，
//!   与视频相互独立、互不阻塞（音频丢帧/卡顿不应拖垮视频）。
//! - **数据通道**（`DataChannel`）：承载输入 / 剪贴板 / 心跳等控制流量（对应 WebRTC 的 DataChannel）。
//!
//! 本 crate 定义这后两者的 **抽象 trait + 线格式（framing）+ 可插拔传输后端**：
//! - 线格式：`[4 字节小端长度][postcard 负载]`，可在流式 / 数据报传输上自定界；收发两侧都强制
//!   最大长度上限，防分配炸弹（复用 P0 的 F3 思路）。
//! - `ByteTransport`：一条"收发字节"的传输缝。任何满足它的后端都能驱动 `MediaChannel`/`DataChannel`：
//!   - `InMemoryTransport`：进程内 mpsc（回环 / 测试，对应 P1 假传输）。
//!   - `TcpTransport`：真实 localhost TCP 套接字（P7 起，媒体/数据真正走网络，而非进程内占位）。
//!   - 未来接 WebRTC DataChannel / RTP 时，只需另写一个 `ByteTransport` 实现，管线与上层一行都不用改。
//!
//! 设计上 `MediaChannel`/`DataChannel` 只关心"收发 `MediaFrame` / `Message`"，至于帧从哪来
//! （真实捕获还是合成）、字节走哪条线（TCP 还是 WebRTC），都由上层与 `ByteTransport` 决定。

// 故意用 `impl Future + Send` 而非 `async fn` 描述 trait 方法：
// 这样返回的 Future 是 `Send` 的，将来可在多线程 runtime 上跨线程 spawn；
// clippy 的 `manual_async_fn` 会建议写回 `async fn`（那样会失去 Send），故在此有意为之并放行。
#![allow(clippy::manual_async_fn)]

use rdcore_proto::{AudioFrame, MediaFrame, Message, ProtocolError};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::Mutex;

/// 单帧媒体允许的最大字节数（编码后）。Raw 1080p ≈ 8 MiB，留足余量防分配炸弹。
pub const MAX_MEDIA_FRAME_LEN: usize = 64 * 1024 * 1024;
/// 数据通道单条消息允许的最大字节数。需容纳剪贴板（MAX_CLIPBOARD_SIZE = 5 MiB）+ 开销。
pub const MAX_DATA_FRAME_LEN: usize = 8 * 1024 * 1024;
/// 单帧音频允许的最大字节数（编码后）。音频帧天然很小：1 秒 48kHz 立体声 16-bit PCM ≈ 192 KiB，
/// Opus 更只有几 KB；留 256 KiB 余量已足以覆盖数秒 PCM 缓冲，同时仍防分配炸弹。
pub const MAX_AUDIO_FRAME_LEN: usize = 256 * 1024;

/// 把任意可序列化值编码成"带 4 字节长度前缀的 postcard 帧"。
///
/// 长度前缀让帧在流式 / 数据报传输上能自定界；超上限直接拒绝，防分配炸弹。
fn frame_encode<T: serde::Serialize>(value: &T, max_len: usize) -> Result<Vec<u8>, ProtocolError> {
    let payload = postcard::to_stdvec(value).map_err(|_| ProtocolError::EncodeError)?;
    if payload.len() > max_len || payload.len() > u32::MAX as usize {
        return Err(ProtocolError::PayloadTooLarge);
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 反序列化带长度前缀的帧。越界 / 超长 / 长度与负载不符都返回 `Err`（防越界读 + 防分配炸弹）。
fn frame_decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    max_len: usize,
) -> Result<T, ProtocolError> {
    if bytes.len() < 4 {
        return Err(ProtocolError::DecodeError);
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    if len > max_len || bytes.len() != 4 + len {
        return Err(ProtocolError::DecodeError);
    }
    postcard::from_bytes(&bytes[4..4 + len]).map_err(|_| ProtocolError::DecodeError)
}

/// 媒体通道错误。
#[derive(Debug, Clone, PartialEq)]
pub enum MediaChannelError {
    /// 对端关闭 / 通道断开。
    Closed,
    /// 帧编解码失败。
    Frame(ProtocolError),
}

impl std::fmt::Display for MediaChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaChannelError::Closed => write!(f, "media channel closed"),
            MediaChannelError::Frame(e) => write!(f, "media frame error: {e}"),
        }
    }
}

impl std::error::Error for MediaChannelError {}

impl From<ProtocolError> for MediaChannelError {
    fn from(e: ProtocolError) -> Self {
        MediaChannelError::Frame(e)
    }
}

/// 音频通道错误（与 `MediaChannelError` 同构，独立类型便于上层区分音频/视频故障）。
#[derive(Debug, Clone, PartialEq)]
pub enum AudioChannelError {
    /// 对端关闭 / 通道断开。
    Closed,
    /// 帧编解码失败。
    Frame(ProtocolError),
}

impl std::fmt::Display for AudioChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioChannelError::Closed => write!(f, "audio channel closed"),
            AudioChannelError::Frame(e) => write!(f, "audio frame error: {e}"),
        }
    }
}

impl std::error::Error for AudioChannelError {}

impl From<ProtocolError> for AudioChannelError {
    fn from(e: ProtocolError) -> Self {
        AudioChannelError::Frame(e)
    }
}

/// 数据通道错误。
#[derive(Debug, Clone, PartialEq)]
pub enum DataChannelError {
    /// 对端关闭 / 通道断开。
    Closed,
    /// 协议层错误（编码 / 解码 / 限长）。
    Protocol(ProtocolError),
}

impl std::fmt::Display for DataChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataChannelError::Closed => write!(f, "data channel closed"),
            DataChannelError::Protocol(e) => write!(f, "data channel protocol error: {e}"),
        }
    }
}

impl std::error::Error for DataChannelError {}

impl From<ProtocolError> for DataChannelError {
    fn from(e: ProtocolError) -> Self {
        DataChannelError::Protocol(e)
    }
}

/// 媒体通道：承载屏幕视频流（对应 WebRTC 的 RTP video）。
///
/// 只关心"收发 `MediaFrame`"，至于帧从哪来（真实捕获还是合成）由上层决定。
pub trait MediaChannel {
    /// 发送一帧编码后的视频。通道关闭返回 `Err(Closed)`。
    fn send_frame(
        &self,
        frame: &MediaFrame,
    ) -> impl std::future::Future<Output = Result<(), MediaChannelError>> + Send;
    /// 接收一帧；对端关闭且无更多帧时返回 `Ok(None)`。
    fn recv_frame(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<MediaFrame>, MediaChannelError>> + Send;
}

/// 音频通道：承载设备音频流（对应 WebRTC 的 RTP audio）。
///
/// 与 `MediaChannel` 完全平行——只关心"收发 `AudioFrame`"，至于帧从哪来
/// （真实麦克风/扬声器捕获还是合成）由上层决定。二者走各自独立的 `ByteTransport`，
/// 互不阻塞：音频抖动或丢帧不会影响视频流畅度。
pub trait AudioChannel {
    /// 发送一帧编码后的音频。通道关闭返回 `Err(Closed)`。
    fn send_frame(
        &self,
        frame: &AudioFrame,
    ) -> impl std::future::Future<Output = Result<(), AudioChannelError>> + Send;
    /// 接收一帧；对端关闭且无更多帧时返回 `Ok(None)`。
    fn recv_frame(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<AudioFrame>, AudioChannelError>> + Send;
}

/// 数据通道：承载输入 / 剪贴板 / 心跳等控制流量（对应 WebRTC 的 DataChannel）。
///
/// 架构上这些流量不应走信令 WebSocket，而应走本通道。
pub trait DataChannel {
    /// 发送一条控制消息。通道关闭返回 `Err(Closed)`。
    fn send(
        &self,
        msg: &Message,
    ) -> impl std::future::Future<Output = Result<(), DataChannelError>> + Send;
    /// 接收一条控制消息；对端关闭且无更多消息时返回 `Ok(None)`。
    fn recv(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Message>, DataChannelError>> + Send;
}

// ───────────────────────────── 可插拔传输后端 ─────────────────────────────

/// 字节传输层错误（真实网络传输使用）。
#[derive(Debug)]
pub enum TransportError {
    /// 对端关闭 / 连接断开。
    Closed,
    /// I/O 错误（读 / 写 socket）。
    Io(std::io::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Closed => write!(f, "transport closed"),
            TransportError::Io(e) => write!(f, "transport io error: {e}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e)
    }
}

/// 一条"收发字节"的传输缝。
///
/// `MediaChannel` / `DataChannel` 不直接碰网络，只调用本 trait 收发"已帧化的字节"
/// （即 `frame_encode` 产出的 `[4 字节长度前缀][postcard 负载]`）。任何满足本 trait 的后端
/// （进程内 mpsc、真实 TCP、未来的 WebRTC DataChannel）都能驱动媒体/数据通道，管线零改动。
pub trait ByteTransport: Send + Sync {
    /// 发送一段已帧化的字节。对端关闭返回 `Err(Closed)`。
    fn send_bytes(
        &self,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), TransportError>> + Send;
    /// 接收一段已帧化的字节；对端关闭且无更多数据时返回 `Ok(None)`。
    fn recv_bytes(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, TransportError>> + Send;
}

/// 进程内字节传输（mpsc 通道），用于回环 / 测试，对应 P1 的假传输。
pub struct InMemoryTransport {
    tx: Sender<Vec<u8>>,
    rx: Arc<Mutex<Receiver<Vec<u8>>>>,
}

impl InMemoryTransport {
    /// 创建一对交叉互联的传输端点：`(a, b)`。a 发出的字节由 b 收到，反之亦然。
    pub fn pair() -> (InMemoryTransport, InMemoryTransport) {
        let (tx_a, rx_a) = mpsc::channel(8);
        let (tx_b, rx_b) = mpsc::channel(8);
        (
            InMemoryTransport {
                tx: tx_a,
                rx: Arc::new(Mutex::new(rx_b)),
            },
            InMemoryTransport {
                tx: tx_b,
                rx: Arc::new(Mutex::new(rx_a)),
            },
        )
    }
}

impl ByteTransport for InMemoryTransport {
    fn send_bytes(
        &self,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), TransportError>> + Send {
        async move { self.tx.send(data).await.map_err(|_| TransportError::Closed) }
    }

    fn recv_bytes(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, TransportError>> + Send {
        async move {
            let mut rx = self.rx.lock().await;
            Ok(rx.recv().await)
        }
    }
}

/// 真实 TCP 字节传输：把一条已连接的 `TcpStream` 包装成 `ByteTransport`。
///
/// - 发送侧直接写出 `frame_encode` 后的字节（含 4 字节长度前缀）。
/// - 接收侧由后台读任务按同样的 4 字节长度前缀做流定界，把完整帧投喂进 mpsc 通道，
///   供 `recv_bytes` 取出——这样 TCP 这种流式协议也能精确还原每一帧。
/// - 读任务在"对端关闭 / 读失败"时自然退出，`recv_bytes` 随后返回 `None`（通道关闭语义）。
pub struct TcpTransport {
    writer: Mutex<tokio::io::WriteHalf<TcpStream>>,
    reader: Arc<Mutex<Receiver<Vec<u8>>>>,
}

impl TcpTransport {
    /// 创建一对经真实 localhost TCP 互联的传输端点。
    ///
    /// 内部 bind 到 `127.0.0.1:0` 拿空闲端口，一端 `connect`、一端 `accept`，
    /// 得到同一条 TCP 连接的两端（双向可用）：A 发送的字节由 B 收到，反之亦然。
    pub async fn pair() -> std::io::Result<(TcpTransport, TcpTransport)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let connect_fut = TcpStream::connect(addr);
        let accept_fut = listener.accept();
        let (conn_res, accept_res) = tokio::join!(connect_fut, accept_fut);
        let stream_c = conn_res?;
        let (stream_a, _peer) = accept_res?;
        Ok((Self::from_stream(stream_a), Self::from_stream(stream_c)))
    }

    /// 把一条已连接的 `TcpStream` 包装成 `TcpTransport`（启动后台定界读任务）。
    pub fn from_stream(stream: TcpStream) -> Self {
        let (mut rd, wr) = tokio::io::split(stream);
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            loop {
                // 读 4 字节长度前缀。
                let mut len_buf = [0u8; 4];
                if rd.read_exact(&mut len_buf).await.is_err() {
                    break; // 对端关闭 / 读失败 → 结束读任务（recv 会收到 None）
                }
                let len = u32::from_le_bytes(len_buf) as usize;
                // 读取长度超过媒体通道上限的异常大帧：断连（再保险，frame_decode 也会拦）。
                if len > MAX_MEDIA_FRAME_LEN {
                    break;
                }
                // 读完整 payload，并把 [前缀][payload] 整体回传，与 frame_encode 输出一致。
                let mut buf = vec![0u8; 4 + len];
                buf[0..4].copy_from_slice(&len_buf);
                if rd.read_exact(&mut buf[4..]).await.is_err() {
                    break;
                }
                if tx.send(buf).await.is_err() {
                    break;
                }
            }
        });
        Self {
            writer: Mutex::new(wr),
            reader: Arc::new(Mutex::new(rx)),
        }
    }
}

impl ByteTransport for TcpTransport {
    fn send_bytes(
        &self,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), TransportError>> + Send {
        async move {
            let mut w = self.writer.lock().await;
            w.write_all(&data).await?;
            w.flush().await?;
            Ok(())
        }
    }

    fn recv_bytes(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, TransportError>> + Send {
        async move {
            let mut rx = self.reader.lock().await;
            Ok(rx.recv().await)
        }
    }
}

/// 通用字节传输媒体通道：任意 `ByteTransport` 后端都能驱动它。
pub struct SocketMediaChannel<S: ByteTransport> {
    transport: S,
}

impl<S: ByteTransport> SocketMediaChannel<S> {
    /// 用指定传输后端构造媒体通道。
    pub fn new(transport: S) -> Self {
        Self { transport }
    }
}

impl<S: ByteTransport> MediaChannel for SocketMediaChannel<S> {
    fn send_frame(
        &self,
        frame: &MediaFrame,
    ) -> impl std::future::Future<Output = Result<(), MediaChannelError>> + Send {
        async move {
            let bytes = frame_encode(frame, MAX_MEDIA_FRAME_LEN)?;
            self.transport
                .send_bytes(bytes)
                .await
                .map_err(|_| MediaChannelError::Closed)?;
            Ok(())
        }
    }

    fn recv_frame(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<MediaFrame>, MediaChannelError>> + Send
    {
        async move {
            match self
                .transport
                .recv_bytes()
                .await
                .map_err(|_| MediaChannelError::Closed)?
            {
                Some(bytes) => frame_decode(&bytes, MAX_MEDIA_FRAME_LEN)
                    .map(Some)
                    .map_err(MediaChannelError::Frame),
                None => Ok(None),
            }
        }
    }
}

/// 通用字节传输音频通道：任意 `ByteTransport` 后端都能驱动它（与 `SocketMediaChannel` 平行）。
pub struct SocketAudioChannel<S: ByteTransport> {
    transport: S,
}

impl<S: ByteTransport> SocketAudioChannel<S> {
    /// 用指定传输后端构造音频通道。
    pub fn new(transport: S) -> Self {
        Self { transport }
    }
}

impl<S: ByteTransport> AudioChannel for SocketAudioChannel<S> {
    fn send_frame(
        &self,
        frame: &AudioFrame,
    ) -> impl std::future::Future<Output = Result<(), AudioChannelError>> + Send {
        async move {
            let bytes = frame_encode(frame, MAX_AUDIO_FRAME_LEN)?;
            self.transport
                .send_bytes(bytes)
                .await
                .map_err(|_| AudioChannelError::Closed)?;
            Ok(())
        }
    }

    fn recv_frame(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<AudioFrame>, AudioChannelError>> + Send
    {
        async move {
            match self
                .transport
                .recv_bytes()
                .await
                .map_err(|_| AudioChannelError::Closed)?
            {
                Some(bytes) => frame_decode(&bytes, MAX_AUDIO_FRAME_LEN)
                    .map(Some)
                    .map_err(AudioChannelError::Frame),
                None => Ok(None),
            }
        }
    }
}

/// 通用字节传输数据通道：任意 `ByteTransport` 后端都能驱动它。
pub struct SocketDataChannel<S: ByteTransport> {
    transport: S,
}

impl<S: ByteTransport> SocketDataChannel<S> {
    /// 用指定传输后端构造数据通道。
    pub fn new(transport: S) -> Self {
        Self { transport }
    }
}

impl<S: ByteTransport> DataChannel for SocketDataChannel<S> {
    fn send(
        &self,
        msg: &Message,
    ) -> impl std::future::Future<Output = Result<(), DataChannelError>> + Send {
        async move {
            let bytes = frame_encode(msg, MAX_DATA_FRAME_LEN)?;
            self.transport
                .send_bytes(bytes)
                .await
                .map_err(|_| DataChannelError::Closed)?;
            Ok(())
        }
    }

    fn recv(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Message>, DataChannelError>> + Send {
        async move {
            match self
                .transport
                .recv_bytes()
                .await
                .map_err(|_| DataChannelError::Closed)?
            {
                Some(bytes) => frame_decode(&bytes, MAX_DATA_FRAME_LEN)
                    .map(Some)
                    .map_err(DataChannelError::Protocol),
                None => Ok(None),
            }
        }
    }
}

/// 进程内媒体通道端点（回环 / 测试；P7 起也可换 `TcpMediaChannel` 走真实网络）。
pub type InMemoryMediaChannel = SocketMediaChannel<InMemoryTransport>;
/// 进程内数据通道端点（回环 / 测试）。
pub type InMemoryDataChannel = SocketDataChannel<InMemoryTransport>;
/// 进程内音频通道端点（回环 / 测试；与媒体通道平行，零侵入）。
pub type InMemoryAudioChannel = SocketAudioChannel<InMemoryTransport>;
/// 真实 TCP 媒体通道端点（P7：媒体流真正走网络，而非进程内占位）。
pub type TcpMediaChannel = SocketMediaChannel<TcpTransport>;
/// 真实 TCP 数据通道端点（P7：输入/剪贴板/心跳真正走网络）。
pub type TcpDataChannel = SocketDataChannel<TcpTransport>;

/// 便捷构造器：一对进程内媒体通道端点。
pub fn media_channel_pair() -> (InMemoryMediaChannel, InMemoryMediaChannel) {
    let (a, b) = InMemoryTransport::pair();
    (SocketMediaChannel::new(a), SocketMediaChannel::new(b))
}

/// 便捷构造器：一对进程内数据通道端点。
pub fn data_channel_pair() -> (InMemoryDataChannel, InMemoryDataChannel) {
    let (a, b) = InMemoryTransport::pair();
    (SocketDataChannel::new(a), SocketDataChannel::new(b))
}

/// 便捷构造器：一对进程内音频通道端点（与 `media_channel_pair` 平行，零侵入）。
pub fn audio_channel_pair() -> (InMemoryAudioChannel, InMemoryAudioChannel) {
    let (a, b) = InMemoryTransport::pair();
    (SocketAudioChannel::new(a), SocketAudioChannel::new(b))
}

/// 创建一对经真实 localhost TCP 互联的 `(媒体通道, 数据通道)` 端点：`((Host 端), (Viewer 端))`。
///
/// Host 端 `send_frame` / `send` 的内容由 Viewer 端 `recv_frame` / `recv` 收到，反之亦然。
/// 用真实套接字证明媒体/数据通道不再只是进程内占位——它们能跨网络字节管道无损往返。
pub async fn tcp_channel_pair() -> std::io::Result<(
    (TcpMediaChannel, TcpDataChannel),
    (TcpMediaChannel, TcpDataChannel),
)> {
    let (mt_a, mt_b) = TcpTransport::pair().await?;
    let (dt_a, dt_b) = TcpTransport::pair().await?;
    Ok((
        (SocketMediaChannel::new(mt_a), SocketDataChannel::new(dt_a)),
        (SocketMediaChannel::new(mt_b), SocketDataChannel::new(dt_b)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_loopback::{
        FrameDecoder, FrameEncoder, FrameSource, RawDecoder, RawEncoder, SyntheticFrameSource,
    };
    use rdcore_proto::{AudioCodec, Heartbeat, Message, VideoCodec};

    #[test]
    fn media_frame_roundtrip() {
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 32,
            height: 24,
            data: vec![0x12u8; 32 * 24 * 4],
        };
        let bytes = frame_encode(&f, MAX_MEDIA_FRAME_LEN).unwrap();
        let back = frame_decode(&bytes, MAX_MEDIA_FRAME_LEN).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn data_frame_roundtrip() {
        let m = Message::Heartbeat(Heartbeat {
            seq: 7,
            timestamp_ms: 1_700_000_000_123,
        });
        let bytes = frame_encode(&m, MAX_DATA_FRAME_LEN).unwrap();
        let back = frame_decode(&bytes, MAX_DATA_FRAME_LEN).unwrap();
        assert_eq!(m, back);
    }

    // ── 音频通道（C：与视频通道平行、零侵入）──

    #[test]
    fn audio_frame_roundtrip() {
        let f = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 2,
            sample_rate: 48_000,
            data: vec![0x34u8; 48000 * 2 * 2], // 1 秒 48k 立体声 16-bit PCM
        };
        let bytes = frame_encode(&f, MAX_AUDIO_FRAME_LEN).unwrap();
        let back = frame_decode(&bytes, MAX_AUDIO_FRAME_LEN).unwrap();
        assert_eq!(f, back);
    }

    #[tokio::test]
    async fn in_memory_audio_channel_roundtrip() {
        let (host, viewer) = audio_channel_pair();
        let f = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 1,
            sample_rate: 48_000,
            data: vec![0xCDu8; 4800], // 一小段单声道 PCM
        };
        // Host 发 → Viewer 收（pair 是交叉互联的）。
        host.send_frame(&f).await.unwrap();
        let got = viewer.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, f);
    }

    #[tokio::test]
    async fn tcp_audio_channel_roundtrip() {
        // 音频通道同样能跑在真实 localhost TCP 上（与媒体通道平行）。
        let (t_host, t_viewer) = TcpTransport::pair().await.unwrap();
        let host = SocketAudioChannel::new(t_host);
        let viewer = SocketAudioChannel::new(t_viewer);
        let f = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 2,
            sample_rate: 44_100,
            data: vec![0xABu8; 4410 * 2 * 2],
        };
        host.send_frame(&f).await.unwrap();
        let got = viewer.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, f, "音频帧应经真实 TCP 无损往返");
    }

    #[tokio::test]
    async fn audio_channel_closed_returns_none() {
        let (host, viewer) = audio_channel_pair();
        drop(host);
        assert!(viewer.recv_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_media_channel_roundtrip() {
        let (host, viewer) = media_channel_pair();
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 16,
            height: 12,
            data: vec![0xABu8; 16 * 12 * 4],
        };
        // Host 发 → Viewer 收（pair 是交叉互联的）。
        host.send_frame(&f).await.unwrap();
        let got = viewer.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, f);
    }

    #[tokio::test]
    async fn in_memory_data_channel_roundtrip() {
        let (host, viewer) = data_channel_pair();
        let m = Message::Heartbeat(Heartbeat {
            seq: 1,
            timestamp_ms: 1,
        });
        host.send(&m).await.unwrap();
        let got = viewer.recv().await.unwrap().unwrap();
        assert_eq!(got, m);
    }

    #[tokio::test]
    async fn media_channel_carries_real_pipeline_lossless() {
        // 用真实 Raw 编码器 + 媒体通道，证明 capture→encode→channel→decode→render 无损往返。
        let (host, viewer) = media_channel_pair();
        let width = 32u32;
        let height = 24u32;
        let frames = 5u32;

        let expected: Vec<_> = {
            let mut s = SyntheticFrameSource::new(width, height, frames);
            std::iter::from_fn(|| s.next_frame()).collect()
        };
        let mut source = SyntheticFrameSource::new(width, height, frames);
        let encoder = RawEncoder;
        let decoder = RawDecoder;
        let mut actual = Vec::with_capacity(frames as usize);
        while let Some(frame) = source.next_frame() {
            let media = encoder.encode(&frame).unwrap();
            host.send_frame(&media).await.unwrap();
            let media_in = viewer.recv_frame().await.unwrap().unwrap();
            actual.push(decoder.decode(&media_in).unwrap());
        }
        assert_eq!(actual, expected, "每帧应经媒体通道无损往返");
    }

    // ── P7：真实 TCP 传输端到端 ──
    #[tokio::test]
    async fn tcp_media_channel_roundtrip() {
        let ((host, _host_dc), (viewer, _viewer_dc)) = tcp_channel_pair().await.unwrap();
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 16,
            height: 12,
            data: vec![0xCDu8; 16 * 12 * 4],
        };
        host.send_frame(&f).await.unwrap();
        let got = viewer.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, f, "媒体帧应经真实 TCP 无损往返");
    }

    #[tokio::test]
    async fn tcp_data_channel_roundtrip() {
        let ((_host_m, host_dc), (_viewer_m, viewer_dc)) = tcp_channel_pair().await.unwrap();
        let m = Message::Heartbeat(Heartbeat {
            seq: 42,
            timestamp_ms: 1,
        });
        host_dc.send(&m).await.unwrap();
        let got = viewer_dc.recv().await.unwrap().unwrap();
        assert_eq!(got, m, "控制消息应经真实 TCP 无损往返");
    }

    #[tokio::test]
    async fn tcp_media_carries_real_pipeline_lossless() {
        // 与 in_memory 同款真实管线，但这次媒体通道跑在真实 localhost TCP 上。
        let ((host, _), (viewer, _)) = tcp_channel_pair().await.unwrap();
        let width = 32u32;
        let height = 24u32;
        let frames = 5u32;

        let expected: Vec<_> = {
            let mut s = SyntheticFrameSource::new(width, height, frames);
            std::iter::from_fn(|| s.next_frame()).collect()
        };
        let mut source = SyntheticFrameSource::new(width, height, frames);
        let encoder = RawEncoder;
        let decoder = RawDecoder;
        let mut actual = Vec::with_capacity(frames as usize);
        while let Some(frame) = source.next_frame() {
            let media = encoder.encode(&frame).unwrap();
            host.send_frame(&media).await.unwrap();
            let media_in = viewer.recv_frame().await.unwrap().unwrap();
            actual.push(decoder.decode(&media_in).unwrap());
        }
        assert_eq!(actual, expected, "每帧应经真实 TCP 媒体通道无损往返");
    }

    // ── 健壮性 / 抗压测试（F3 护栏、丢帧、大帧、保序、关闭语义）──

    #[test]
    fn frame_decode_rejects_truncated() {
        // 长度前缀声称 100 字节，但实际只有 14 字节 → 长度与负载不符，必须拒绝（防越界读）。
        let mut b = vec![0u8; 14];
        b[0] = 100; // 长度前缀 = 100，但负载仅 10 字节
        assert!(frame_decode::<MediaFrame>(&b, MAX_MEDIA_FRAME_LEN).is_err());
    }

    #[test]
    fn frame_decode_rejects_oversized_len_prefix() {
        // 长度前缀超过上限 → 拒绝（防分配炸弹）。
        let b = (u32::MAX).to_le_bytes().to_vec();
        assert!(frame_decode::<MediaFrame>(&b, MAX_MEDIA_FRAME_LEN).is_err());
    }

    #[test]
    fn frame_decode_rejects_zero_len_corrupt_payload() {
        // 长度前缀为 0 但后续无有效 postcard → 解码失败（而非越界）。
        let b = [0u8, 0, 0, 0];
        assert!(frame_decode::<MediaFrame>(&b, MAX_MEDIA_FRAME_LEN).is_err());
    }

    #[test]
    fn frame_decode_rejects_corrupt_payload() {
        // 长度前缀合法但负载不是合法 postcard → 解码失败。
        let b = vec![3u8, 0, 0, 0, 1, 2, 3];
        assert!(frame_decode::<Message>(&b, MAX_DATA_FRAME_LEN).is_err());
    }

    #[test]
    fn frame_encode_rejects_over_max() {
        // 序列化后超过给定上限 → PayloadTooLarge。
        let m = Message::Heartbeat(Heartbeat {
            seq: u64::MAX,
            timestamp_ms: u64::MAX,
        });
        assert!(
            matches!(frame_encode(&m, 4), Err(ProtocolError::PayloadTooLarge)),
            "超上限的编码应被拒绝"
        );
    }

    #[tokio::test]
    async fn media_channel_many_frames_ordered() {
        // 发送 200 帧，验证全部收到且保序（mpsc 保序）。
        // 注意：InMemoryTransport 的 mpsc 容量为 8，必须先发后收地并发排空，否则会背压死锁。
        let (host, viewer) = media_channel_pair();
        let n = 200u32;
        let encoder = RawEncoder;
        let decoder = RawDecoder;
        let mut expected = Vec::new();
        {
            let mut src = SyntheticFrameSource::new(8, 8, n);
            while let Some(f) = src.next_frame() {
                expected.push(f);
            }
        }
        // 并发接收任务：边收边存。
        let recv_task = tokio::spawn(async move {
            let mut got = Vec::new();
            while let Some(f) = viewer.recv_frame().await.unwrap() {
                got.push(f);
            }
            got
        });
        // 主任务边编码边发。
        let mut src = SyntheticFrameSource::new(8, 8, n);
        while let Some(f) = src.next_frame() {
            host.send_frame(&encoder.encode(&f).unwrap()).await.unwrap();
        }
        drop(host); // 关闭发送端 → 接收任务终退出
        let got = recv_task.await.unwrap();
        assert_eq!(got.len(), n as usize, "200 帧应全部收到");
        for (i, g) in got.iter().enumerate() {
            assert_eq!(decoder.decode(g).unwrap(), expected[i], "帧顺序应保序");
        }
    }

    #[tokio::test]
    async fn media_channel_large_frame_roundtrip() {
        // ~3 MiB 单帧（1024x768 RGBA）在限值内应无损往返（InMemory）。
        let (host, viewer) = media_channel_pair();
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 1024,
            height: 768,
            data: vec![0x5Au8; 1024 * 768 * 4],
        };
        host.send_frame(&f).await.unwrap();
        let got = viewer.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, f, "大帧应无损往返");
    }

    #[tokio::test]
    async fn tcp_media_channel_large_frame_roundtrip() {
        // 大帧经真实 localhost TCP 无损往返。
        let ((host, _), (viewer, _)) = tcp_channel_pair().await.unwrap();
        let f = MediaFrame {
            codec: VideoCodec::Raw,
            width: 1024,
            height: 768,
            data: vec![0x7Eu8; 1024 * 768 * 4],
        };
        host.send_frame(&f).await.unwrap();
        let got = viewer.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, f, "大帧应经真实 TCP 无损往返");
    }

    #[tokio::test]
    async fn media_channel_closed_returns_none() {
        // 发送端关闭后，接收端应返回 Ok(None) 而非挂起或报错。
        let (host, viewer) = media_channel_pair();
        drop(host);
        assert!(viewer.recv_frame().await.unwrap().is_none());
    }

    // 丢帧容忍：传输层（此处确定性每 3 帧丢 1）吞掉整帧，媒体通道不应崩溃，
    // 且能收完剩余帧——视频流允许丢帧，不允许卡死。
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropEveryTransport {
        inner: InMemoryTransport,
        drop_every: usize,
        sent: AtomicUsize,
    }

    impl ByteTransport for DropEveryTransport {
        fn send_bytes(
            &self,
            data: Vec<u8>,
        ) -> impl std::future::Future<Output = Result<(), TransportError>> + Send {
            let n = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if self.drop_every != 0 && n % self.drop_every == 0 {
                    return Ok(()); // 模拟丢帧：直接丢弃，不转发给 inner
                }
                self.inner.send_bytes(data).await
            }
        }

        fn recv_bytes(
            &self,
        ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, TransportError>> + Send
        {
            self.inner.recv_bytes()
        }
    }

    #[tokio::test]
    async fn media_channel_tolerates_frame_loss() {
        let (t_a, t_b) = InMemoryTransport::pair();
        let host = SocketMediaChannel::new(DropEveryTransport {
            inner: t_a,
            drop_every: 3,
            sent: AtomicUsize::new(0),
        });
        let viewer = SocketMediaChannel::new(DropEveryTransport {
            inner: t_b,
            drop_every: 3,
            sent: AtomicUsize::new(0),
        });
        let total = 9u32;
        let mut source = SyntheticFrameSource::new(16, 12, total);
        let encoder = RawEncoder;
        let mut sent = 0u32;
        while let Some(f) = source.next_frame() {
            host.send_frame(&encoder.encode(&f).unwrap()).await.unwrap();
            sent += 1;
        }
        drop(host); // 关闭发送端，使对端 recv 终返回 None
        let expected = sent - sent / 3; // 每 3 帧丢 1
        let mut got = 0u32;
        while viewer.recv_frame().await.unwrap().is_some() {
            got += 1;
        }
        assert_eq!(got, expected, "丢帧后通道应仍可用且收到剩余帧");
    }
}
