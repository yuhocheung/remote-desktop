//! 线消息：信令（Offer/Answer/ICE）、输入、剪贴板、心跳。

use crate::frame::{Capabilities, FrameMetadata};
use crate::{
    DeviceId, PeerIdentity, ProtocolError, SessionId, Signature, MAX_CLIPBOARD_SIZE,
    MAX_FILE_CHUNK_SIZE,
};
use rdcore_crypto::Ciphertext;
use serde::{Deserialize, Serialize};

/// 端到端加密握手中交换的"已签名的临时公钥"。
///
/// 把 X25519 临时公钥绑定到 P4 已认证的 Ed25519 身份：签名覆盖
/// `session_id || from || ephemeral`，对端用 [`crate::canonical_ephemeral_bytes`] 验签后
/// 再做 X25519 派生会话密钥。绝不可覆盖 `signature` 自身（否则循环依赖）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKeyExchange {
    /// 会话 ID（与本次连接的 Offer/Answer 一致）。
    pub session_id: SessionId,
    /// 发送方设备 ID（用于去 store 取公钥验签）。
    pub from: DeviceId,
    /// X25519 临时公钥的原始字节。
    pub ephemeral: [u8; 32],
    /// 对上述三样的 Ed25519 签名（P4 身份）。
    pub signature: Option<Signature>,
}

/// 协议顶层消息。postcard 按变体下标编码。
///
/// **稳定性**：不要重排已有变体；新增变体只能追加到末尾。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// 连接方（Viewer/控制端）发出的 SDP Offer。
    Offer(ConnectionOffer),
    /// 接收方（Host/被控端）回的 SDP Answer。
    Answer(ConnectionAnswer),
    /// 一个 ICE 候选（NAT 穿透用的打洞地址）。
    Ice(IceCandidate),
    /// 远程输入事件（鼠标/键盘/滚轮）。
    InputEvent(InputEvent),
    /// 剪贴板同步事件。
    Clipboard(ClipboardEvent),
    /// 心跳存活探针。
    Heartbeat(Heartbeat),
    /// 端到端加密握手：携带已签名的 X25519 临时公钥（P5）。追加在末尾，下标不变。
    SessionKey(SessionKeyExchange),
    /// 端到端加密通道承载的密文（AEAD 封装的任意控制/媒体负载）。追加在末尾，下标不变。
    Encrypted(Ciphertext),
    /// 文件传输（B4 韧性面）。**追加在下标 8**，走 Control 通道且需 Host 逐次同意（防泄密）。
    FileTransfer(FileTransferEvent),
    /// 配对身份交换：连接双方在 Offer/Answer 前广播自己的公开身份（DeviceId + 公钥），
    /// 对端按 TOFU 记住首个版本（带外锚 = 一次性配对 token 保护的会话房间）。
    /// **追加在下标 9**；仅含公开信息，不含任何密钥材料。
    PeerHello(PeerIdentity),
}

/// 连接签名必须覆盖的、确定不变的字节内容。
///
/// 单独定义成这个类型（而不是对整条线 `Message` 签名），是为了让签名只覆盖
/// 握手协商里需要完整性保护的部分——**绝不可覆盖 `signature` 字段本身**，否则
/// 签名就自引用了（鸡生蛋）。这里包含 `session_id || from || sdp || capabilities || frame`：
/// 后两者（能力协商、画面元数据）若被中间人篡改（例如偷偷关闭输入/剪贴板能力），
/// 必须被签名挡住。签名时请对 [`crate::canonical_signing_bytes`] 产出的规范字节签名。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SigningPayload {
    /// 会话 ID。
    pub session_id: SessionId,
    /// 发送方设备 ID。
    pub from: DeviceId,
    /// WebRTC SDP 文本。
    pub sdp: String,
    /// 本方能力（编解码、分辨率、**输入类型、剪贴板开关**等）。必须纳入签名，
    /// 否则中间人可静默降级能力协商（例如关掉输入/剪贴板）。
    pub capabilities: Capabilities,
    /// 协商出的画面元数据（可选）。必须纳入签名，防止协商被篡改。
    pub frame: Option<FrameMetadata>,
}

/// WebRTC SDP Offer，由发起连接的一方（Viewer）发出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionOffer {
    /// 会话 ID（用于云端关联同一次连接的所有信令）。
    pub session_id: SessionId,
    /// 发送方设备 ID。
    pub from: DeviceId,
    /// SDP 描述文本（含媒体/ICE 候选等）。
    pub sdp: String,
    /// 本方能力（编解码、分辨率、输入类型等）。
    pub capabilities: Capabilities,
    /// 协商出的画面元数据（可选，Offer 阶段可能还不知道）。
    pub frame: Option<FrameMetadata>,
    /// 对 [`SigningPayload`]（session_id || from || sdp || capabilities || frame）的签名。
    /// 在 identity 接入之前（P4/P5）一直为 None。
    /// 用 `signing_payload()` 构造负载，再对 [`crate::canonical_signing_bytes`]
    /// 产出的规范字节签名。**绝不可对整条 `Message` 签名**。
    pub signature: Option<Signature>,
}

impl ConnectionOffer {
    /// 构造 `signature` 应当覆盖的规范负载。
    pub fn signing_payload(&self) -> SigningPayload {
        SigningPayload {
            session_id: self.session_id,
            from: self.from,
            sdp: self.sdp.clone(),
            capabilities: self.capabilities.clone(),
            frame: self.frame.clone(),
        }
    }
}

/// WebRTC SDP Answer，由接收方（Host）回。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionAnswer {
    /// 会话 ID。
    pub session_id: SessionId,
    /// 发送方设备 ID。
    pub from: DeviceId,
    /// SDP 描述文本。
    pub sdp: String,
    /// 本方能力。
    pub capabilities: Capabilities,
    /// 协商出的画面元数据（可选）。
    pub frame: Option<FrameMetadata>,
    /// 对 [`SigningPayload`] 的签名。identity 接入前为 None。
    /// 用 `signing_payload()` 构造负载并对规范字节签名。
    pub signature: Option<Signature>,
}

impl ConnectionAnswer {
    /// 构造 `signature` 应当覆盖的规范负载。
    pub fn signing_payload(&self) -> SigningPayload {
        SigningPayload {
            session_id: self.session_id,
            from: self.from,
            sdp: self.sdp.clone(),
            capabilities: self.capabilities.clone(),
            frame: self.frame.clone(),
        }
    }
}

/// 单个 ICE 候选片段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceCandidate {
    /// 会话 ID。
    pub session_id: SessionId,
    /// 发送方设备 ID。
    pub from: DeviceId,
    /// ICE 候选字符串（如 "candidate:1 1 UDP ..."）。
    pub candidate: String,
    /// SDP mid（媒体流标识），可选。
    pub sdp_mid: Option<String>,
    /// SDP m-line 序号，可选。
    pub sdp_mline_index: Option<u32>,
}

/// 一条远程输入事件（鼠标/键盘/滚轮）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    /// 单调递增序号，用于去重/乱序处理。
    pub seq: u64,
    /// 输入的具体类型。
    pub kind: InputKind,
}

/// 输入的种类，嵌在 [`InputEvent`] 内。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputKind {
    /// 鼠标移动。坐标为相对被控端屏幕的像素位置（坐标系细节 P3/P5 前再定）。
    MouseMove { x: i32, y: i32 },
    /// 鼠标按键。
    MouseButton {
        button: MouseButton,
        /// true=按下，false=抬起。
        pressed: bool,
    },
    /// 鼠标滚轮。
    MouseWheel { delta_x: i16, delta_y: i16 },
    /// 原始按键事件。`key_code` 是平台扫描码，`modifiers` 是修饰键位掩码。
    Key {
        key_code: u32,
        pressed: bool,
        modifiers: u16,
    },
    /// 带字符的按键事件（IME 友好，参考 RustDesk Map 模式双发）。同时携带 USB HID
    /// 扫描码与 Unicode 字符：Host 端 `pressed=true` 且 `character` 非空时优先用
    /// `enigo.text(character)` 做文本注入（支持中文/日文等 IME 合成输入）；
    /// `character` 为 None 或空时 fallback 到 `key_code` 物理按键（快捷键/游戏）。
    KeyWithChar {
        key_code: u32,
        character: Option<String>,
        pressed: bool,
        modifiers: u16,
    },
}

/// 鼠标按键标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// 左键。
    Left,
    /// 中键。
    Middle,
    /// 右键。
    Right,
    /// 侧前键（浏览器后退）。
    Back,
    /// 侧后键（浏览器前进）。
    Forward,
}

/// 剪贴板同步消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipboardEvent {
    /// 序号。
    pub seq: u64,
    /// 动作。
    pub action: ClipboardAction,
}

/// 剪贴板动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipboardAction {
    /// 对端请求当前剪贴板内容。
    Request,
    /// 剪贴板数据（发送方已做清洗）。这是 exfil/DoS 面，受 `MAX_CLIPBOARD_SIZE` 限制。
    Data(Vec<u8>),
    /// 本地剪贴板已变更，清除镜像副本。
    Clear,
}

/// 存活探针；双方互发以检测连接是否断掉。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// 序号。
    pub seq: u64,
    /// 发送方时间戳（毫秒，通常为 Unix 毫秒）。
    pub timestamp_ms: u64,
}

/// 一次文件传输的事件（B4）。走 Control 通道（E2E 加密），Host 逐次同意后才开始发 Chunk。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTransferEvent {
    /// 同一次传输的唯一标识（区分多文件/多次传输）。
    pub transfer_id: u64,
    /// 事件动作。
    pub action: FileTransferAction,
}

/// 文件传输动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileTransferAction {
    /// 发送方提议：文件名 + 总字节数。等 Host 逐次同意（`Accept`）后才发数据。
    Offer { name: String, size: u64 },
    /// Host 接受本次传输（逐次授权）。
    Accept,
    /// Host 拒绝本次传输。
    Reject { reason: String },
    /// 一个数据分片。`seq` 单调递增，接收方按序重组。
    Chunk { seq: u64, data: Vec<u8> },
    /// 全部发送完毕（含总分片数，供校验完整性）。
    Done { chunks: u64 },
    /// 任一方中止本次传输。
    Abort,
}

impl Message {
    /// 在 [`crate::decode`] / [`crate::decode_limited`] 之后调用的语义校验。
    ///
    /// 强制 [`MAX_CLIPBOARD_SIZE`]：剪贴板 `Data` 超过上限就以
    /// [`ProtocolError::PayloadTooLarge`] 拒绝。注意：传输层仍必须在 decode 前
    /// 用 [`crate::MAX_SIGNALING_MESSAGE_LEN`] 限制原始 buffer 大小，因为 postcard
    /// 会按长度前缀预分配 `Vec`/`String`。
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Message::Clipboard(event) => {
                if let ClipboardAction::Data(data) = &event.action {
                    if data.len() > MAX_CLIPBOARD_SIZE {
                        return Err(ProtocolError::PayloadTooLarge);
                    }
                }
            }
            Message::FileTransfer(event) => {
                if let FileTransferAction::Chunk { data, .. } = &event.action {
                    if data.len() > MAX_FILE_CHUNK_SIZE {
                        return Err(ProtocolError::PayloadTooLarge);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
