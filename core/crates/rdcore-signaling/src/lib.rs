//! rdcore-signaling — WebSocket 信令客户端（连接 cloud 控制平面的 Signaling Service）。
//!
//! 客户端把 P0 的 `Message` 用 postcard 编码为二进制帧，经 WebSocket 发送；
//! 接收时 `decode_limited` 还原为 `Message`（复用 F3 限长护栏）。
//!
//! 设计上客户端对通道中立：它只负责"收发 `Message`"，至于某条 `Message` 是 Offer 还是
//! InputEvent，由上层决定。按架构 §1，信令 WebSocket 只承载 SDP/ICE（Offer/Answer/Ice）；
//! InputEvent/Clipboard/Heartbeat 在 P3 改走 WebRTC DataChannel。

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rdcore_proto::{decode_limited, encode, Message, MAX_SIGNALING_MESSAGE_LEN};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 信令客户端错误。
#[derive(Debug)]
pub enum SignalingError {
    /// 协议层（编码/解码/限长）。
    Protocol(rdcore_proto::ProtocolError),
    /// WebSocket / 连接错误。
    WebSocket(String),
    /// 通道已关闭（对端掉线或后台任务结束）。
    Closed,
}

impl std::fmt::Display for SignalingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignalingError::Protocol(e) => write!(f, "protocol error: {e}"),
            SignalingError::WebSocket(s) => write!(f, "websocket error: {s}"),
            SignalingError::Closed => write!(f, "signaling channel closed"),
        }
    }
}

impl std::error::Error for SignalingError {}

impl From<rdcore_proto::ProtocolError> for SignalingError {
    fn from(e: rdcore_proto::ProtocolError) -> Self {
        SignalingError::Protocol(e)
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for SignalingError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        SignalingError::WebSocket(e.to_string())
    }
}

/// 一个已连接信令服务器的客户端句柄（可并发 `send` / `recv`）。
pub struct SignalingClient {
    /// 发往后台写任务的字节通道。
    out_tx: UnboundedSender<Vec<u8>>,
    /// 后台读任务投喂进来的 `Message` 队列（用异步 Mutex 共享以便多次 `recv`）。
    in_rx: Arc<Mutex<UnboundedReceiver<Message>>>,
}

impl SignalingClient {
    /// 连接到信令服务器（`ws://` 或 `wss://`）。
    pub async fn connect(url: &str) -> Result<Self, SignalingError> {
        let (ws, _resp) = tokio_tungstenite::connect_async(url).await?;
        let (mut write, mut read) = ws.split();

        // 后台写任务：把 out_tx 收到的字节帧写回 WebSocket。
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(bytes) = out_rx.recv().await {
                if write.send(WsMessage::Binary(bytes)).await.is_err() {
                    break;
                }
            }
        });

        // 后台读任务：把收到的二进制帧 decode 成 Message 投喂进 in_tx。
        // 任务一旦退出（对端关闭 / 协议错误 / EOF），recv 将永远收不到消息，
        // 因此退出时务必留痕，便于区分「没收到」与「收到了但没处理」。
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            loop {
                match read.next().await {
                    Some(Ok(msg)) => match msg {
                        WsMessage::Binary(b) => {
                            // 非法/超长帧：忽略（连接仍可用，等待后续合法帧）。
                            if let Ok(m) = decode_limited(&b, MAX_SIGNALING_MESSAGE_LEN) {
                                if in_tx.send(m).is_err() {
                                    eprintln!("[sig] 读任务退出：无人接收（in_rx 已关闭）");
                                    break;
                                }
                            }
                        }
                        WsMessage::Close(_) => {
                            eprintln!("[sig] 读任务退出：收到 Close 帧");
                            break;
                        }
                        _ => {}
                    },
                    Some(Err(e)) => {
                        eprintln!("[sig] 读任务退出：WebSocket 错误：{e}");
                        break;
                    }
                    None => {
                        eprintln!("[sig] 读任务退出：连接 EOF");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            out_tx,
            in_rx: Arc::new(Mutex::new(in_rx)),
        })
    }

    /// 发送一条控制消息（postcard 编码为二进制帧）。
    pub async fn send(&self, msg: &Message) -> Result<(), SignalingError> {
        let bytes = encode(msg)?;
        self.out_tx.send(bytes).map_err(|_| SignalingError::Closed)
    }

    /// 接收一条控制消息；对端关闭时返回 `None`。
    pub async fn recv(&self) -> Result<Option<Message>, SignalingError> {
        let mut rx = self.in_rx.lock().await;
        Ok(rx.recv().await)
    }
}
