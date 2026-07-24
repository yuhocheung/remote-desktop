//! rdcore-ffi — C ABI 桥接层（cdylib），把 Rust 核心暴露给 Flutter/Dart（dart:ffi）。
//!
//! 设计（锚定架构文档 P6）：
//! - 复杂 Rust 对象以**不透明句柄**暴露（`RdLocal` 长期身份 / `RdSession` 一次连接）；
//!   Dart 只持有裸指针，永远不碰内部字段。
//! - 信令/媒体消息（`ConnectionOffer/Answer`、`SessionKeyExchange`、`Ciphertext`）走
//!   **postcard 字节缓冲区**（`RdBytes{data,len}`），由 Dart 经信令 WebSocket 收发
//!   （云端只做中转，看不到内容）。
//! - 连接状态 / 不可伪造安全指示器序列化为 **JSON 串**返回，Dart 直接 `jsonDecode`
//!   渲染横幅（数据全部来自已认证对端，Viewer 无法伪造）。
//! - 错误统一存线程局部 `LAST_ERROR`：失败函数返回 `NULL`，Dart 调 [`rdcore_last_error`]
//!   取人类可读消息。
//!
//! 本 crate 同时是 `cdylib`（给 Dart 动态链接）与 `rlib`（本 crate 的 Rust 单测直接调用），
//! 因此下面的完整 Host↔Viewer 握手 + 端到端加密流程有 Rust 级回归测试守护。

#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::missing_safety_doc)]

use std::collections::HashSet;
use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rdcore_consent::{ConnectionState, ConsentDecision, ConsentGate, ConsentMode, ConsentScope};
use rdcore_crypto::{
    aead_open, aead_seal, ephemeral_x25519_keypair, x25519_public_bytes, Ciphertext,
    Ed25519CryptoProvider, SecretKey, SessionKey, X25519SecretKey,
};
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_proto::{
    decode, encode, AudioCodec, AudioFrame, Capabilities, ClipboardAction, ClipboardEvent,
    ConnectionAnswer, ConnectionOffer, FileTransferAction, FileTransferEvent, InputCaps,
    InputEvent, InputKind, MediaFrame, Message, MouseButton, SessionId, VideoCodec,
};
use rdcore_session::{
    establish_session_key, sign_answer, sign_ephemeral_key, sign_offer, verify_answer,
    verify_offer, VerifiedPeer,
};

// Track A 媒体面 / 输入面依赖（与 `rdcore_app::Connection` 共用同一套编解码 + E2E 加密原语）。
use rdcore_audio::{
    AudioDecoder, AudioEncoder, AudioSource, NullAudioSource, RawDecoder as AudioRawDecoder,
    RawEncoder as AudioRawEncoder,
};
use rdcore_capture::{CaptureSource, NullCaptureSource};
use rdcore_decode::{Decoder, RawDecoder};
use rdcore_encode::{Encoder, RawEncoder};
use rdcore_media::{
    audio_channel_pair, data_channel_pair, media_channel_pair, AudioChannel, DataChannel,
    InMemoryAudioChannel, InMemoryDataChannel, InMemoryMediaChannel, MediaChannel,
};
// 缺口 M：Viewer 侧真实 WebRTC Peer。复用 `rdcore_app::Connection`（与 Host 完全相同的
// webrtc-rs PeerConnection + 信令握手 + ICE + E2E 密钥 + 同意），由其内部 `SignalingClient`
// 持有信令，与 Host 一致；Flutter/iOS 经 FFI 即可建立真实 Viewer 连接。
use rdcore_app::{Connection, HostAudioPump, HostMediaPump};
use rdcore_rtc::{IceServer, RtcConfig};
use tokio::runtime::Runtime;

/// 权限位掩码（Dart 侧按位组合传入 [`rdcore_host_decide`]）。
const SCOPE_VIEW: u32 = 1;
const SCOPE_INPUT: u32 = 2;
const SCOPE_CLIPBOARD: u32 = 4;
const SCOPE_FILE: u32 = 8;

/// 不透明句柄：本设备长期身份 + 私钥（跨多个会话复用）。
pub struct RdLocal {
    store: InMemoryIdentityStore,
    secret: SecretKey,
}

/// 不透明句柄：一次连接会话（Host 或 Viewer）。
pub struct RdSession {
    provider: Ed25519CryptoProvider,
    store: InMemoryIdentityStore,
    secret: SecretKey,
    session_id: SessionId,
    role: Role,
    mode: ConsentMode,
    peer: Option<VerifiedPeer>,
    consent: Option<ConsentGate>,
    our_x: Option<X25519SecretKey>,
    session_key: Option<SessionKey>,
    // ── Track A 媒体面 / 输入面（headless 回环 seam）──
    /// Host 发送视频的媒体通道。
    media_send: Option<InMemoryMediaChannel>,
    /// Viewer 接收视频的媒体通道。
    media_recv: Option<InMemoryMediaChannel>,
    /// Viewer 发送输入的（控制）数据通道。
    input_send: Option<InMemoryDataChannel>,
    /// Host 接收输入的数据通道。
    input_recv: Option<InMemoryDataChannel>,
    /// Host 抓取源（headless 用 `NullCaptureSource`；真实抓屏由 `real` feature 提供）。
    capture: Option<Box<dyn CaptureSource + Send>>,
    /// 停止后台媒体泵的标志（与媒体泵线程共享）。
    media_stop: Option<Arc<AtomicBool>>,
    /// 媒体泵后台线程句柄（会话释放时 join，确保泵任务被取消）。
    media_thread: Option<thread::JoinHandle<()>>,
    // ── Track A 音频面（与媒体面平行、互不阻塞；C 音频管线）──
    /// Host 发送音频的音频通道。
    audio_send: Option<InMemoryAudioChannel>,
    /// Viewer 接收音频的音频通道。
    audio_recv: Option<InMemoryAudioChannel>,
    /// Host 音频抓取源（headless 用 `NullAudioSource`；真实采集由 `real` feature 提供）。
    audio_capture: Option<Box<dyn AudioSource + Send>>,
    /// 停止后台音频泵的标志（与音频泵线程共享）。
    audio_stop: Option<Arc<AtomicBool>>,
    /// 音频泵后台线程句柄（会话释放时 join，确保泵任务被取消）。
    audio_thread: Option<thread::JoinHandle<()>>,
    /// Viewer/输入侧复用的 tokio runtime（拉帧 / 收发输入都是异步操作）。
    rt: Option<Runtime>,
    // ── Track B 韧性面（kimi-k3）：文件传输接收侧状态机（B6）──
    /// 每个 `transfer_id` 一个接收会话（Host 逐次同意后重组分片）。
    file_sessions: std::collections::HashMap<u64, rdcore_app::file_transfer::TransferSession>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Host,
    Viewer,
}

/// C 端可安全读取的字节缓冲区。Dart 侧 `Pointer<RdBytes>` 取 `data`/`len` 拷出后
/// 必须调用 [`rdcore_bytes_free`] 释放。
#[repr(C)]
pub struct RdBytes {
    pub data: *mut u8,
    pub len: usize,
}

/// 上一次失败操作的错误信息（线程安全；Dart 单 isolate 调用天然串行，无竞争）。
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn set_last_error(e: impl std::fmt::Display) {
    if let Ok(mut g) = LAST_ERROR.lock() {
        *g = Some(e.to_string());
    }
}

fn clear_last_error() {
    if let Ok(mut g) = LAST_ERROR.lock() {
        *g = None;
    }
}

/// 把 Rust `String` 序列化成 C 字符串（调用方用 [`rdcore_string_free`] 释放）。
/// 含 NUL 时降级为空串，避免未定义行为。
fn to_cstr(s: String) -> *mut c_char {
    CString::new(s)
        .map(|c| c.into_raw())
        .unwrap_or_else(|_| CString::new("").unwrap().into_raw())
}

fn null_cstr() -> *mut c_char {
    ptr::null_mut()
}

/// 设置 `LAST_ERROR` 并返回**非空错误串**（命令类函数用：NULL=成功，非空串=失败）。
fn err_cstr(msg: impl std::fmt::Display) -> *mut c_char {
    let s = msg.to_string();
    set_last_error(&s);
    to_cstr(s)
}

/// 把 `Vec<u8>` 移交为 `RdBytes`（调用方用 [`rdcore_bytes_free`] 释放）。
fn vec_to_rdbytes(v: Vec<u8>) -> *mut RdBytes {
    // 手动拆解 Vec，避免 `into_raw_parts`（clippy 的 MSRV 数据库误判其需要 1.93）。
    let mut v = std::mem::ManuallyDrop::new(v);
    let data = v.as_mut_ptr();
    let len = v.len();
    Box::into_raw(Box::new(RdBytes { data, len }))
}

/// 把 `ConnectionState` 序列化为 JSON 串（或设置错误并返回 NULL）。
/// 状态类函数约定：成功返回非空 JSON 串，失败返回 NULL。
fn json_state(st: &ConnectionState) -> *mut c_char {
    match serde_json::to_string(st) {
        Ok(s) => {
            clear_last_error();
            to_cstr(s)
        }
        Err(e) => {
            set_last_error(e);
            ptr::null_mut()
        }
    }
}

/// 把顶层 `Message` 编码为 postcard 字节缓冲区（或设置错误并返回 NULL）。
fn encode_message(m: &Message) -> *mut RdBytes {
    match encode(m) {
        Ok(bytes) => vec_to_rdbytes(bytes),
        Err(e) => {
            set_last_error(e);
            ptr::null_mut()
        }
    }
}

/// 从 C 字符串取 UTF-8 副本（NULL 或非法 UTF-8 返回 None）。
unsafe fn cstr_to_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(|s| s.to_string())
}

/// 一次连接的默认能力协商（P6 后续由媒体层真实填充 SDP / 分辨率）。
/// fps 为「可提供的帧率上限」声明值，与 rdcore-app 侧一致报 60（实际发送率由
/// Host `--fps` 决定，默认 60，见 rdcore-desktop `DEFAULT_FPS`）。
fn default_caps() -> Capabilities {
    Capabilities {
        video_codecs: vec![VideoCodec::Raw],
        max_width: 1920,
        max_height: 1080,
        fps: 60,
        clipboard: true,
        input: InputCaps {
            mouse: true,
            keyboard: true,
            wheel: true,
        },
    }
}

fn build_offer(s: &RdSession) -> ConnectionOffer {
    ConnectionOffer {
        session_id: s.session_id,
        from: s.store.local_identity().id,
        sdp: String::new(),
        capabilities: default_caps(),
        frame: None,
        signature: None,
    }
}

fn build_answer(s: &RdSession) -> ConnectionAnswer {
    ConnectionAnswer {
        session_id: s.session_id,
        from: s.store.local_identity().id,
        sdp: String::new(),
        capabilities: default_caps(),
        frame: None,
        signature: None,
    }
}

fn bits_to_scopes(mask: u32) -> HashSet<ConsentScope> {
    let mut s = HashSet::new();
    if mask & SCOPE_VIEW != 0 {
        s.insert(ConsentScope::View);
    }
    if mask & SCOPE_INPUT != 0 {
        s.insert(ConsentScope::Input);
    }
    if mask & SCOPE_CLIPBOARD != 0 {
        s.insert(ConsentScope::Clipboard);
    }
    if mask & SCOPE_FILE != 0 {
        s.insert(ConsentScope::FileTransfer);
    }
    s
}

// ───────────────────────────── 版本 / 错误 ─────────────────────────────

/// 返回本桥接层版本号（语义化版本字符串）。
#[no_mangle]
pub extern "C" fn rdcore_version() -> *mut c_char {
    to_cstr(env!("CARGO_PKG_VERSION").to_string())
}

/// 取上一次失败操作的错误信息；无错误返回 NULL。读取后调用方应 [`rdcore_string_free`]。
#[no_mangle]
pub extern "C" fn rdcore_last_error() -> *mut c_char {
    match LAST_ERROR.lock() {
        Ok(g) => match g.as_ref() {
            Some(s) => to_cstr(s.clone()),
            None => ptr::null_mut(),
        },
        Err(_) => ptr::null_mut(),
    }
}

/// 释放 [`rdcore_last_error`] / 其它返回的 C 字符串。
#[no_mangle]
pub extern "C" fn rdcore_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    unsafe { drop(CString::from_raw(s)) };
}

/// 释放 [`RdBytes`] 缓冲区（同时释放内部 `data`）。
#[no_mangle]
pub extern "C" fn rdcore_bytes_free(b: *mut RdBytes) {
    if b.is_null() {
        return;
    }
    unsafe {
        let b = Box::from_raw(b);
        if !b.data.is_null() {
            drop(Vec::from_raw_parts(b.data, b.len, b.len));
        }
    }
}

// ───────────────────────────── 身份 / 存储 ─────────────────────────────

/// 用给定展示名生成本设备身份 + 内存存储，返回长期句柄。
#[no_mangle]
pub extern "C" fn rdcore_identity_new(display_name: *const c_char) -> *mut RdLocal {
    let name = unsafe { cstr_to_string(display_name) }.unwrap_or_else(|| "device".into());
    let provider = Ed25519CryptoProvider;
    let (identity, secret) = create_local_identity(&provider, &name);
    let store = InMemoryIdentityStore::new(identity);
    Box::into_raw(Box::new(RdLocal { store, secret }))
}

/// 释放长期身份句柄。
#[no_mangle]
pub extern "C" fn rdcore_identity_free(local: *mut RdLocal) {
    if local.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw(local)) };
}

/// 返回本机公钥指纹（空格分隔大写十六进制），用于带外展示给用户核对。
#[no_mangle]
pub extern "C" fn rdcore_local_fingerprint(local: *mut RdLocal) -> *mut c_char {
    if local.is_null() {
        return null_cstr();
    }
    let l = unsafe { &*local };
    to_cstr(l.store.local_identity().fingerprint.to_spaced_hex())
}

/// 返回本机设备 ID（16 字节）。
#[no_mangle]
pub extern "C" fn rdcore_local_device_id(local: *mut RdLocal) -> *mut RdBytes {
    if local.is_null() {
        return ptr::null_mut();
    }
    let l = unsafe { &*local };
    vec_to_rdbytes(l.store.local_identity().id.to_vec())
}

/// 把本机身份导出为 JSON（供对端扫码/带外配对导入）。
#[no_mangle]
pub extern "C" fn rdcore_local_peer_json(local: *mut RdLocal) -> *mut c_char {
    if local.is_null() {
        return null_cstr();
    }
    let l = unsafe { &*local };
    match serde_json::to_string(l.store.local_identity()) {
        Ok(s) => {
            clear_last_error();
            to_cstr(s)
        }
        Err(e) => {
            set_last_error(e);
            ptr::null_mut()
        }
    }
}

/// 导入对端身份 JSON（带外配对），成功返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_remember_peer_json(
    local: *mut RdLocal,
    json: *const c_char,
) -> *mut c_char {
    if local.is_null() {
        return err_cstr("null local handle");
    }
    let j = match unsafe { cstr_to_string(json) } {
        Some(j) => j,
        None => return err_cstr("null peer json"),
    };
    let peer: rdcore_identity::PeerIdentity = match serde_json::from_str(&j) {
        Ok(p) => p,
        Err(e) => return err_cstr(format!("invalid peer json: {e}")),
    };
    let l = unsafe { &mut *local };
    l.store.remember(peer);
    clear_last_error();
    ptr::null_mut()
}

// ───────────────────────────── 会话生命周期 ─────────────────────────────

/// 基于长期身份开一个连接会话。
/// `role`: 1=Host, 0=Viewer；`session_id`: 16 字节；`unattended_pin`: 非 NULL 且非空即
/// 进入无人值守模式（凭 PIN 自动放行）。
#[no_mangle]
pub extern "C" fn rdcore_session_new(
    local: *mut RdLocal,
    role: c_int,
    session_id: *const u8,
    unattended_pin: *const c_char,
) -> *mut RdSession {
    if local.is_null() {
        return ptr::null_mut();
    }
    let l = unsafe { &*local };
    let mut sid = [0u8; 16];
    if !session_id.is_null() {
        unsafe { ptr::copy_nonoverlapping(session_id, sid.as_mut_ptr(), 16) };
    }
    let role = if role == 1 { Role::Host } else { Role::Viewer };
    let mode = match unsafe { cstr_to_string(unattended_pin) } {
        Some(pin) if !pin.is_empty() => ConsentMode::Unattended { pin },
        _ => ConsentMode::Interactive,
    };
    let session = RdSession {
        provider: Ed25519CryptoProvider,
        store: l.store.clone(),
        secret: l.secret.clone(),
        session_id: SessionId(sid),
        role,
        mode,
        peer: None,
        consent: None,
        our_x: None,
        session_key: None,
        media_send: None,
        media_recv: None,
        input_send: None,
        input_recv: None,
        capture: None,
        media_stop: None,
        media_thread: None,
        audio_send: None,
        audio_recv: None,
        audio_capture: None,
        audio_stop: None,
        audio_thread: None,
        rt: None,
        file_sessions: std::collections::HashMap::new(),
    };
    Box::into_raw(Box::new(session))
}

/// 释放会话句柄。
#[no_mangle]
pub extern "C" fn rdcore_session_free(session: *mut RdSession) {
    if session.is_null() {
        return;
    }
    let mut s = unsafe { Box::from_raw(session) };
    // 停止后台媒体泵并等待其线程退出（确保泵任务被取消、资源释放）。
    if let Some(stop) = s.media_stop.take() {
        stop.store(true, Ordering::SeqCst);
    }
    if let Some(h) = s.media_thread.take() {
        let _ = h.join();
    }
}

// ───────────────────────────── 握手：Offer / Answer ─────────────────────────────

/// 生成（并签名）本端的 ConnectionOffer，返回 postcard 字节。失败返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_make_offer(session: *mut RdSession) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let offer = sign_offer(&s.provider, &s.secret, build_offer(s));
    encode_message(&Message::Offer(offer))
}

/// 收下对端 Offer：解码 + Ed25519 验签；成功缓存已认证 `VerifiedPeer`，返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_ingest_offer(
    session: *mut RdSession,
    bytes: *const u8,
    len: usize,
) -> *mut c_char {
    if session.is_null() || bytes.is_null() {
        return err_cstr("null session or bytes");
    }
    let buf = unsafe { std::slice::from_raw_parts(bytes, len) };
    let msg = match decode(buf) {
        Ok(m) => m,
        Err(e) => return err_cstr(format!("decode: {e}")),
    };
    let offer = match msg {
        Message::Offer(o) => o,
        _ => return err_cstr("expected Offer message"),
    };
    let s = unsafe { &mut *session };
    match verify_offer(&s.provider, &s.store, &offer) {
        Ok(peer) => {
            s.peer = Some(peer);
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => err_cstr(e),
    }
}

/// 生成（并签名）本端的 ConnectionAnswer，返回 postcard 字节。失败返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_make_answer(session: *mut RdSession) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    if s.peer.is_none() {
        set_last_error("ingest peer offer first");
        return ptr::null_mut();
    }
    let answer = sign_answer(&s.provider, &s.secret, build_answer(s));
    encode_message(&Message::Answer(answer))
}

/// 收下对端 Answer：解码 + 验签；成功缓存 `VerifiedPeer`，返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_ingest_answer(
    session: *mut RdSession,
    bytes: *const u8,
    len: usize,
) -> *mut c_char {
    if session.is_null() || bytes.is_null() {
        return err_cstr("null session or bytes");
    }
    let buf = unsafe { std::slice::from_raw_parts(bytes, len) };
    let msg = match decode(buf) {
        Ok(m) => m,
        Err(e) => return err_cstr(format!("decode: {e}")),
    };
    let answer = match msg {
        Message::Answer(a) => a,
        _ => return err_cstr("expected Answer message"),
    };
    let s = unsafe { &mut *session };
    match verify_answer(&s.provider, &s.store, &answer) {
        Ok(peer) => {
            s.peer = Some(peer);
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => err_cstr(e),
    }
}

// ───────────────────── 端到端加密握手：会话密钥交换 ─────────────────────

/// 生成临时 X25519 密钥对，用 P4 身份签名后返回 `SessionKeyExchange` 字节。
/// 内部保存本端私钥，供稍后 `rdcore_ingest_session_key_exchange` 派生会话密钥。
#[no_mangle]
pub extern "C" fn rdcore_make_session_key_exchange(session: *mut RdSession) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    if s.peer.is_none() {
        set_last_error("peer not verified yet");
        return ptr::null_mut();
    }
    let (pk, sk) = ephemeral_x25519_keypair();
    let from = s.store.local_identity().id;
    let ex = sign_ephemeral_key(
        &s.provider,
        &s.secret,
        s.session_id,
        from,
        x25519_public_bytes(&pk),
    );
    s.our_x = Some(sk);
    encode_message(&Message::SessionKey(ex))
}

/// 收下对端 `SessionKeyExchange`：会话 ID 绑定 + 验签 + X25519 ECDH 派生会话密钥。
#[no_mangle]
pub extern "C" fn rdcore_ingest_session_key_exchange(
    session: *mut RdSession,
    bytes: *const u8,
    len: usize,
) -> *mut c_char {
    if session.is_null() || bytes.is_null() {
        return err_cstr("null session or bytes");
    }
    let buf = unsafe { std::slice::from_raw_parts(bytes, len) };
    let msg = match decode(buf) {
        Ok(m) => m,
        Err(e) => return err_cstr(format!("decode: {e}")),
    };
    let ex = match msg {
        Message::SessionKey(e) => e,
        _ => return err_cstr("expected SessionKey message"),
    };
    let s = unsafe { &mut *session };
    let our = match s.our_x.as_ref() {
        Some(k) => k,
        None => return err_cstr("call rdcore_make_session_key_exchange first"),
    };
    match establish_session_key(&s.provider, &s.store, our, &ex, s.session_id) {
        Ok(key) => {
            s.session_key = Some(key);
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => err_cstr(e),
    }
}

/// 用会话密钥 AEAD 加密明文，返回 `Message::Encrypted` 字节。失败返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_encrypt(
    session: *mut RdSession,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("no session key established");
            return ptr::null_mut();
        }
    };
    let plain = unsafe { std::slice::from_raw_parts(data, len) };
    let ct = aead_seal(key, plain);
    encode_message(&Message::Encrypted(ct))
}

/// 解密 `Message::Encrypted` 字节，返回原始明文。篡改/错误密钥返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_decrypt(
    session: *mut RdSession,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let buf = unsafe { std::slice::from_raw_parts(data, len) };
    let msg = match decode(buf) {
        Ok(m) => m,
        Err(e) => {
            set_last_error(format!("decode: {e}"));
            return ptr::null_mut();
        }
    };
    let ct = match msg {
        Message::Encrypted(c) => c,
        _ => {
            set_last_error("expected Encrypted message");
            return ptr::null_mut();
        }
    };
    let s = unsafe { &*session };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("no session key established");
            return ptr::null_mut();
        }
    };
    match aead_open(key, &ct) {
        Some(plain) => vec_to_rdbytes(plain),
        None => {
            set_last_error("decrypt failed (tampered payload or wrong key)");
            ptr::null_mut()
        }
    }
}

// ───────────────────────────── 同意门控（仅 Host） ─────────────────────────────

/// Host 收到请求后调用：交互模式保持等待，无人值守模式按 PIN 即刻放行。返回状态 JSON。
#[no_mangle]
pub extern "C" fn rdcore_host_request_consent(
    session: *mut RdSession,
    pin: *const c_char,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        return err_cstr("only Host can manage consent");
    }
    let peer = match s.peer.clone() {
        Some(p) => p,
        None => return err_cstr("peer not verified yet"),
    };
    let presented = unsafe { cstr_to_string(pin) };
    let mut gate = ConsentGate::new(peer, s.mode.clone(), Duration::from_secs(30));
    let st = gate.request_consent(presented.as_deref());
    s.consent = Some(gate);
    json_state(&st)
}

/// Host 对请求做决定：`grant!=0` 授予 `scopes_mask` 范围（`duration_secs>0` 设有效期），
/// 否则拒绝。返回状态 JSON。
#[no_mangle]
pub extern "C" fn rdcore_host_decide(
    session: *mut RdSession,
    grant: c_int,
    scopes_mask: u32,
    duration_secs: i64,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        return err_cstr("only Host can manage consent");
    }
    let gate = match s.consent.as_mut() {
        Some(g) => g,
        None => return err_cstr("consent not started (call rdcore_host_request_consent)"),
    };
    let decision = if grant != 0 {
        ConsentDecision::Grant {
            scopes: bits_to_scopes(scopes_mask),
            duration: if duration_secs > 0 {
                Some(Duration::from_secs(duration_secs as u64))
            } else {
                None
            },
        }
    } else {
        ConsentDecision::Deny {
            reason: "Host 拒绝本次连接".into(),
        }
    };
    let st = gate.decide(decision);
    json_state(&st)
}

/// 推进生命周期（仅 Active 检查到期/心跳超时），返回状态 JSON。
#[no_mangle]
pub extern "C" fn rdcore_tick(session: *mut RdSession) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    match s.consent.as_mut() {
        Some(g) => json_state(&g.tick(Instant::now())),
        None => json_state(&ConnectionState::AwaitingConsent),
    }
}

/// 记录一次心跳（对端仍在线）。
#[no_mangle]
pub extern "C" fn rdcore_heartbeat(session: *mut RdSession) {
    if session.is_null() {
        return;
    }
    let s = unsafe { &mut *session };
    if let Some(g) = s.consent.as_mut() {
        g.note_heartbeat(Instant::now());
    }
}

/// Host 主动撤销/终止连接，返回状态 JSON。
#[no_mangle]
pub extern "C" fn rdcore_revoke(session: *mut RdSession) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    match s.consent.as_mut() {
        Some(g) => json_state(&g.revoke()),
        None => err_cstr("no active consent to revoke"),
    }
}

/// 传输层报告断开（非超时），返回状态 JSON。
#[no_mangle]
pub extern "C" fn rdcore_on_disconnected(session: *mut RdSession) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    match s.consent.as_mut() {
        Some(g) => json_state(&g.on_disconnected()),
        None => json_state(&ConnectionState::AwaitingConsent),
    }
}

// ───────────────────────────── 状态 / 安全指示器 ─────────────────────────────

/// 返回当前连接状态 JSON（无 consent 时为 `AwaitingConsent`）。
#[no_mangle]
pub extern "C" fn rdcore_connection_state(session: *mut RdSession) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &*session };
    let st = match &s.consent {
        Some(g) => g.state().clone(),
        None => ConnectionState::AwaitingConsent,
    };
    json_state(&st)
}

/// 返回不可伪造安全指示器 JSON（`encrypted!=0` 表示端到端加密已建立）。
#[no_mangle]
pub extern "C" fn rdcore_security_indicator(
    session: *mut RdSession,
    encrypted: c_int,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &*session };
    let gate = match s.consent.as_ref() {
        Some(g) => g,
        None => return err_cstr("consent not initialized"),
    };
    let ind = gate.security_indicator(encrypted != 0);
    match serde_json::to_string(&ind) {
        Ok(json) => {
            clear_last_error();
            to_cstr(json)
        }
        Err(e) => {
            set_last_error(e);
            ptr::null_mut()
        }
    }
}

// ───────────────────────────── 已认证对端信息 ─────────────────────────────

/// 返回已认证对端展示名（未验签返回 NULL）。
#[no_mangle]
pub extern "C" fn rdcore_peer_display_name(session: *mut RdSession) -> *mut c_char {
    if session.is_null() {
        return null_cstr();
    }
    let s = unsafe { &*session };
    match &s.peer {
        Some(p) => to_cstr(p.display_name.clone()),
        None => null_cstr(),
    }
}

/// 返回已认证对端公钥指纹（空格分隔大写十六进制）；未验签返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_peer_fingerprint(session: *mut RdSession) -> *mut c_char {
    if session.is_null() {
        return null_cstr();
    }
    let s = unsafe { &*session };
    match &s.peer {
        Some(p) => to_cstr(p.fingerprint.to_spaced_hex()),
        None => null_cstr(),
    }
}

/// 返回已认证对端设备 ID（16 字节）；未验签返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_peer_device_id(session: *mut RdSession) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    match &s.peer {
        Some(p) => vec_to_rdbytes(p.id.to_vec()),
        None => ptr::null_mut(),
    }
}

// ───────────────────────────── Track A 媒体面 / 输入面 ─────────────────────────────
//
// 与「冻结契约」一致：媒体像素与输入事件都经 E2E 会话密钥 AEAD 加密，云端（信令）只中转
// 握手 / SDP / ICE，永远看不到媒体或输入内容。下列函数把 `RdSession` 暴露的 E2E 密钥复用到
// 媒体 / 输入通道，走与 `rdcore_app::Connection::send_media` / `recv_media` / `send_input` /
// `recv_input` **完全相同**的线格式（像素 aead_seal → postcard(Ciphertext) → MediaFrame.data；
// 输入 postcard(InputEvent) → aead_seal → Message::Encrypted）。
//
// 真实部署里媒体 / 输入通道由 WebRTC 承载；此处用进程内 `InMemory*` 通道作为 headless 回环
// seam（`rdcore_session_attach_loopback_media` 把 Host/Viewer 两端接到同一对通道上），让媒体 /
// 输入面在 C-ABI 层也能被单测完整验证。

/// 一帧已可显示的画面（RGBA8888），由 Viewer 侧 [`rdcore_viewer_pull_frame`] 返回。
///
/// `data` 是长度 `width * height * 4` 的 RGBA 缓冲，调用方用 [`rdcore_media_frame_free`] 释放。
#[repr(C)]
pub struct RdMediaFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA 缓冲（长度 = `width * height * 4`）。
    pub data: *mut u8,
    /// `data` 的字节长度。
    pub len: usize,
}

/// 一帧已解码的音频（16-bit 交错 PCM 或 Opus 压缩），由 Viewer 侧 [`rdcore_viewer_pull_audio`] 返回。
///
/// `codec`：`0` = Raw（16-bit 交错 PCM），`1` = Opus。`data` 语义随 `codec`：
/// - Raw：`data` 长度 = 采样数 × `channels` × 2（每采样 2 字节）。
/// - Opus：`data` 为 Opus 压缩字节（由 `real` feature 编码）。
///
/// 调用方用 [`rdcore_audio_frame_free`] 释放内部缓冲。
#[repr(C)]
pub struct RdAudioFrame {
    /// 编解码器（0=Raw，1=Opus）。
    pub codec: c_int,
    /// 通道数（1 = 单声道，2 = 立体声）。
    pub channels: u16,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// PCM / 压缩字节缓冲（语义见上方 `codec` 说明）。
    pub data: *mut u8,
    /// `data` 的字节长度。
    pub len: usize,
}

/// Viewer → Host 的输入事件（C 侧构造后传入 [`rdcore_viewer_send_input`]）。
#[repr(C)]
pub struct RdInputEvent {
    /// 类型：0=MouseMove, 1=MouseButton, 2=MouseWheel, 3=Key。
    pub kind: c_int,
    /// MouseMove 的 x 坐标（像素）。
    pub x: i32,
    /// MouseMove 的 y 坐标（像素）。
    pub y: i32,
    /// MouseButton 的按键：0=Left, 1=Middle, 2=Right, 3=Back, 4=Forward。
    pub button: c_int,
    /// MouseButton 是否按下（非 0 = 按下）。
    pub pressed: c_int,
    /// MouseWheel 的水平增量。
    pub delta_x: i16,
    /// MouseWheel 的垂直增量。
    pub delta_y: i16,
    /// Key 的扫描码。
    pub key_code: u32,
    /// Key 的修饰键位掩码。
    pub modifiers: u32,
}

impl RdInputEvent {
    /// 从 Rust `InputEvent` 还原成 C 结构（Host 侧 [`rdcore_host_poll_input`] 用）。
    fn from_input(e: &InputEvent) -> Self {
        let mut out = RdInputEvent {
            kind: 0,
            x: 0,
            y: 0,
            button: 0,
            pressed: 0,
            delta_x: 0,
            delta_y: 0,
            key_code: 0,
            modifiers: 0,
        };
        match &e.kind {
            InputKind::MouseMove { x, y } => {
                out.kind = 0;
                out.x = *x;
                out.y = *y;
            }
            InputKind::MouseButton { button, pressed } => {
                out.kind = 1;
                out.button = match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                };
                out.pressed = if *pressed { 1 } else { 0 };
            }
            InputKind::MouseWheel { delta_x, delta_y } => {
                out.kind = 2;
                out.delta_x = *delta_x;
                out.delta_y = *delta_y;
            }
            InputKind::Key {
                key_code,
                pressed,
                modifiers,
            } => {
                out.kind = 3;
                out.key_code = *key_code;
                out.pressed = if *pressed { 1 } else { 0 };
                out.modifiers = u32::from(*modifiers);
            }
            InputKind::KeyWithChar { key_code, .. } => {
                // character 无法放回 C struct（无字段）；Host 注入走 rdcore-app
                // recv_input 不经此 FFI poll，故 character 丢失可接受。
                out.kind = 4;
                out.key_code = *key_code;
            }
        }
        out
    }
}

/// 把 C `RdInputEvent` 转成 Rust `InputEvent`（Viewer 侧 [`rdcore_viewer_send_input`] 用）。
fn to_input_event(e: &RdInputEvent) -> InputEvent {
    let kind = match e.kind {
        0 => InputKind::MouseMove { x: e.x, y: e.y },
        1 => InputKind::MouseButton {
            button: match e.button {
                0 => MouseButton::Left,
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                3 => MouseButton::Back,
                _ => MouseButton::Forward,
            },
            pressed: e.pressed != 0,
        },
        2 => InputKind::MouseWheel {
            delta_x: e.delta_x,
            delta_y: e.delta_y,
        },
        _ => InputKind::Key {
            key_code: e.key_code,
            pressed: e.pressed != 0,
            modifiers: e.modifiers as u16,
        },
    };
    InputEvent { seq: 0, kind }
}

/// 把 Host/Viewer 两端的会话接到同一对进程内媒体/输入通道（headless 回环 seam）。
///
/// 仅用于无 WebRTC 的单测环境；真实部署里媒体/输入走 WebRTC 数据通道。调用后 Host 可发视频、
/// 收输入；Viewer 可收视频、发输入。任一指针为 NULL 或角色不匹配（host/viewer 反了）返回错误串。
#[no_mangle]
pub extern "C" fn rdcore_session_attach_loopback_media(
    host: *mut RdSession,
    viewer: *mut RdSession,
) -> *mut c_char {
    if host.is_null() || viewer.is_null() {
        return err_cstr("null session");
    }
    let h = unsafe { &mut *host };
    let v = unsafe { &mut *viewer };
    if h.role != Role::Host || v.role != Role::Viewer {
        return err_cstr("host/viewer 角色不匹配（请用 host 接 host、viewer 接 viewer）");
    }
    let (host_mc, viewer_mc) = media_channel_pair();
    let (host_dc, viewer_dc) = data_channel_pair();
    h.media_send = Some(host_mc);
    h.input_recv = Some(host_dc);
    v.media_recv = Some(viewer_mc);
    v.input_send = Some(viewer_dc);
    clear_last_error();
    ptr::null_mut()
}

/// Host 设置抓取源（headless 用纯色合成帧；真实抓屏由 `real` feature 的 `ScrapCaptureSource` 提供）。
///
/// `frames` 为发送的帧数（之后源自然结束）；`color` 为纯色字节（便于断言无损往返）。
#[no_mangle]
pub extern "C" fn rdcore_host_set_capture(
    session: *mut RdSession,
    width: u32,
    height: u32,
    frames: u32,
    color: u8,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        return err_cstr("only Host can set capture");
    }
    s.capture = Some(Box::new(NullCaptureSource::new(
        width, height, frames, color,
    )));
    clear_last_error();
    ptr::null_mut()
}

/// Host 启动后台抓取→编码→E2E 加密→媒体通道发送循环（像素走端到端 AEAD，与 `Connection` 对称）。
///
/// 内部在自己的 tokio runtime 线程上驱动循环，函数立即返回；循环随会话释放
///（`rdcore_session_free`）或源结束而停止。需先 `attach_loopback_media` + `set_capture` +
/// 完成 E2E 握手（拿到 `session_key`）。
#[no_mangle]
pub extern "C" fn rdcore_host_start_capture(session: *mut RdSession, fps: c_int) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        return err_cstr("only Host can start capture");
    }
    let media = match s.media_send.take() {
        Some(m) => m,
        None => return err_cstr("先调用 rdcore_session_attach_loopback_media"),
    };
    let key = match s.session_key.clone() {
        Some(k) => k,
        None => return err_cstr("尚未建立 E2E 会话密钥（先完成握手）"),
    };
    let mut capture = match s.capture.take() {
        Some(c) => c,
        None => return err_cstr("先调用 rdcore_host_set_capture"),
    };
    let stop = Arc::new(AtomicBool::new(false));
    s.media_stop = Some(stop.clone());
    let fps = (fps as u16).max(1);
    let handle = thread::spawn(move || {
        let rt = Runtime::new().expect("无法创建 tokio runtime");
        rt.block_on(async move {
            let encoder = RawEncoder;
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            while !stop.load(Ordering::SeqCst) {
                if let Some(frame) = capture.next_frame() {
                    if let Ok(enc) = encoder.encode(&frame) {
                        // 仅加密像素字节（宽/高/编码留明文，便于对端解码器预分配）。
                        let ct = aead_seal(&key, &enc.data);
                        if let Ok(data) = postcard::to_stdvec(&ct) {
                            let mut sealed = enc;
                            sealed.data = data;
                            let _ = media.send_frame(&sealed).await;
                        }
                    }
                } else {
                    break; // 源结束
                }
                tokio::time::sleep(interval).await;
            }
        });
    });
    s.media_thread = Some(handle);
    clear_last_error();
    ptr::null_mut()
}

/// Viewer 拉取一帧已解密/解码/渲染的画面（RGBA）。通道关闭或出错返回 NULL（详见 `rdcore_last_error`）。
#[no_mangle]
pub extern "C" fn rdcore_viewer_pull_frame(session: *mut RdSession) -> *mut RdMediaFrame {
    if session.is_null() {
        set_last_error("null session");
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Viewer {
        set_last_error("only Viewer can pull frames");
        return ptr::null_mut();
    }
    let media = match s.media_recv.as_ref() {
        Some(m) => m,
        None => {
            set_last_error("先调用 rdcore_session_attach_loopback_media");
            return ptr::null_mut();
        }
    };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("尚未建立 E2E 会话密钥");
            return ptr::null_mut();
        }
    };
    if s.rt.is_none() {
        s.rt = Some(Runtime::new().expect("无法创建 tokio runtime"));
    }
    let rt = s.rt.as_ref().unwrap();
    let sealed = match rt.block_on(media.recv_frame()) {
        Ok(Some(f)) => f,
        Ok(None) => {
            set_last_error("媒体通道已关闭（Host 已停止抓屏）");
            return ptr::null_mut();
        }
        Err(_) => {
            set_last_error("媒体接收失败");
            return ptr::null_mut();
        }
    };
    let ct: Ciphertext = match postcard::from_bytes(&sealed.data) {
        Ok(c) => c,
        Err(_) => {
            set_last_error("密文反序列化失败");
            return ptr::null_mut();
        }
    };
    let plain = match aead_open(key, &ct) {
        Some(p) => p,
        None => {
            set_last_error("帧解密失败（篡改 / 密钥不匹配）");
            return ptr::null_mut();
        }
    };
    let frame = MediaFrame {
        codec: sealed.codec,
        width: sealed.width,
        height: sealed.height,
        data: plain,
    };
    let decoded = match RawDecoder.decode(&frame) {
        Ok(d) => d,
        Err(_) => {
            set_last_error("帧解码失败");
            return ptr::null_mut();
        }
    };
    let rendered = match rdcore_render::render(&decoded) {
        Ok(r) => r,
        Err(_) => {
            set_last_error("帧渲染失败");
            return ptr::null_mut();
        }
    };
    // 把 RGBA 缓冲的所有权移交给 C（调用方用 rdcore_media_frame_free 释放）。
    let mut rgba = rendered.rgba;
    let len = rgba.len();
    let data = rgba.as_mut_ptr();
    std::mem::forget(rgba);
    let out = RdMediaFrame {
        width: rendered.width,
        height: rendered.height,
        data,
        len,
    };
    Box::into_raw(Box::new(out))
}

/// 释放 [`RdMediaFrame`]（同时释放内部 RGBA 缓冲）。
#[no_mangle]
pub extern "C" fn rdcore_media_frame_free(frame: *mut RdMediaFrame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = Box::from_raw(frame);
        if !f.data.is_null() {
            drop(Vec::from_raw_parts(f.data, f.len, f.len));
        }
    }
}

// ───────────────────────────── C 音频面（与媒体面平行、互不阻塞） ─────────────────────────────

/// 把 Host/Viewer 两端的会话接到同一对进程内音频通道（headless 回环 seam）。
///
/// 与 [`rdcore_session_attach_loopback_media`] 平行：仅用于无 WebRTC 的单测环境；真实部署里
/// 音频走独立的 WebRTC `audio` DataChannel（id=2）。调用后 Host 可发音频、收静音控制；
/// Viewer 可收音频。任一指针为 NULL 或角色不匹配返回错误串。
#[no_mangle]
pub extern "C" fn rdcore_session_attach_loopback_audio(
    host: *mut RdSession,
    viewer: *mut RdSession,
) -> *mut c_char {
    if host.is_null() || viewer.is_null() {
        return err_cstr("null session");
    }
    let h = unsafe { &mut *host };
    let v = unsafe { &mut *viewer };
    if h.role != Role::Host || v.role != Role::Viewer {
        return err_cstr("host/viewer 角色不匹配（请用 host 接 host、viewer 接 viewer）");
    }
    let (host_ac, viewer_ac) = audio_channel_pair();
    h.audio_send = Some(host_ac);
    v.audio_recv = Some(viewer_ac);
    clear_last_error();
    ptr::null_mut()
}

/// Host 设置音频抓取源（headless 用静音/固定字节合成 PCM；真实采集由 `real` feature 的
/// `CpalAudioSource` 提供）。
///
/// - `channels`：通道数（1=单声道，2=立体声）。
/// - `sample_rate`：采样率（Hz，如 48000）。
/// - `samples_per_frame`：每帧采样数/声道（如 960 = 20ms @ 48kHz）。
/// - `frames`：发送的帧数（之后源自然结束）。
/// - `byte`：填充每个 PCM 字节（默认 0 = 静音）。
#[no_mangle]
pub extern "C" fn rdcore_host_set_capture_audio(
    session: *mut RdSession,
    channels: u16,
    sample_rate: u32,
    samples_per_frame: u32,
    frames: u32,
    byte: u8,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        return err_cstr("only Host can set audio capture");
    }
    s.audio_capture = Some(Box::new(NullAudioSource::new(
        channels,
        sample_rate,
        samples_per_frame,
        frames,
        byte,
    )));
    clear_last_error();
    ptr::null_mut()
}

/// Host 启动后台采集→编码→E2E 加密→音频通道发送循环（音频字节走端到端 AEAD，与 `Connection` 对称）。
///
/// 内部在自己的 tokio runtime 线程上驱动循环，函数立即返回；循环随会话释放
///（`rdcore_session_free`）或源结束而停止。需先 `attach_loopback_audio` + `set_capture_audio` +
/// 完成 E2E 握手（拿到 `session_key`）。
#[no_mangle]
pub extern "C" fn rdcore_host_start_capture_audio(
    session: *mut RdSession,
    fps: c_int,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        return err_cstr("only Host can start audio capture");
    }
    let audio = match s.audio_send.take() {
        Some(m) => m,
        None => return err_cstr("先调用 rdcore_session_attach_loopback_audio"),
    };
    let key = match s.session_key.clone() {
        Some(k) => k,
        None => return err_cstr("尚未建立 E2E 会话密钥（先完成握手）"),
    };
    let mut capture = match s.audio_capture.take() {
        Some(c) => c,
        None => return err_cstr("先调用 rdcore_host_set_capture_audio"),
    };
    let stop = Arc::new(AtomicBool::new(false));
    s.audio_stop = Some(stop.clone());
    let fps = (fps as u16).max(1);
    let handle = thread::spawn(move || {
        let rt = Runtime::new().expect("无法创建 tokio runtime");
        rt.block_on(async move {
            let encoder = AudioRawEncoder;
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            while !stop.load(Ordering::SeqCst) {
                if let Some(frame) = capture.next_frame() {
                    if let Ok(enc) = encoder.encode(&frame) {
                        // 仅加密音频字节（channels/sample_rate 留明文，便于对端播放器预分配）。
                        let ct = aead_seal(&key, &enc.data);
                        if let Ok(data) = postcard::to_stdvec(&ct) {
                            let mut sealed = enc;
                            sealed.data = data;
                            let _ = audio.send_frame(&sealed).await;
                        }
                    }
                } else {
                    break; // 源结束
                }
                tokio::time::sleep(interval).await;
            }
        });
    });
    s.audio_thread = Some(handle);
    clear_last_error();
    ptr::null_mut()
}

/// Viewer 拉取一帧已解密/解码的音频（Raw 16-bit 交错 PCM）。通道关闭或出错返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_viewer_pull_audio(session: *mut RdSession) -> *mut RdAudioFrame {
    if session.is_null() {
        set_last_error("null session");
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Viewer {
        set_last_error("only Viewer can pull audio");
        return ptr::null_mut();
    }
    let audio = match s.audio_recv.as_ref() {
        Some(m) => m,
        None => {
            set_last_error("先调用 rdcore_session_attach_loopback_audio");
            return ptr::null_mut();
        }
    };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("尚未建立 E2E 会话密钥");
            return ptr::null_mut();
        }
    };
    if s.rt.is_none() {
        s.rt = Some(Runtime::new().expect("无法创建 tokio runtime"));
    }
    let rt = s.rt.as_ref().unwrap();
    let sealed = match rt.block_on(audio.recv_frame()) {
        Ok(Some(f)) => f,
        Ok(None) => {
            set_last_error("音频通道已关闭（Host 已停止采集）");
            return ptr::null_mut();
        }
        Err(_) => {
            set_last_error("音频接收失败");
            return ptr::null_mut();
        }
    };
    let ct: Ciphertext = match postcard::from_bytes(&sealed.data) {
        Ok(c) => c,
        Err(_) => {
            set_last_error("密文反序列化失败");
            return ptr::null_mut();
        }
    };
    let plain = match aead_open(key, &ct) {
        Some(p) => p,
        None => {
            set_last_error("音频帧解密失败（篡改 / 密钥不匹配）");
            return ptr::null_mut();
        }
    };
    let frame = AudioFrame {
        codec: sealed.codec,
        channels: sealed.channels,
        sample_rate: sealed.sample_rate,
        data: plain,
    };
    // Raw 直通解码（Opus 需 real feature 的解码器；headless 默认 Raw）。
    let decoded = match AudioRawDecoder.decode(&frame) {
        Ok(d) => d,
        Err(_) => {
            set_last_error("音频解码失败");
            return ptr::null_mut();
        }
    };
    // 把 PCM 缓冲的所有权移交给 C（调用方用 rdcore_audio_frame_free 释放）。
    let mut pcm = decoded.data;
    let len = pcm.len();
    let data = pcm.as_mut_ptr();
    std::mem::forget(pcm);
    let codec_c = match decoded.codec {
        AudioCodec::Raw => 0,
        AudioCodec::Opus => 1,
    };
    let out = RdAudioFrame {
        codec: codec_c,
        channels: decoded.channels,
        sample_rate: decoded.sample_rate,
        data,
        len,
    };
    Box::into_raw(Box::new(out))
}

/// 释放 [`RdAudioFrame`]（同时释放内部缓冲）。
#[no_mangle]
pub extern "C" fn rdcore_audio_frame_free(frame: *mut RdAudioFrame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = Box::from_raw(frame);
        if !f.data.is_null() {
            drop(Vec::from_raw_parts(f.data, f.len, f.len));
        }
    }
}

/// 释放 [`RdInputEvent`]（由 [`rdcore_host_poll_input`] 返回）。
#[no_mangle]
pub extern "C" fn rdcore_input_event_free(event: *mut RdInputEvent) {
    if event.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw(event)) };
}

/// Viewer 发送一条输入事件（经 E2E 加密控制通道 → Host）。成功返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_viewer_send_input(
    session: *mut RdSession,
    event: *const RdInputEvent,
) -> *mut c_char {
    if session.is_null() || event.is_null() {
        return err_cstr("null session or event");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Viewer {
        return err_cstr("only Viewer can send input");
    }
    let input_send = match s.input_send.as_ref() {
        Some(c) => c,
        None => return err_cstr("先调用 rdcore_session_attach_loopback_media"),
    };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => return err_cstr("尚未建立 E2E 会话密钥"),
    };
    let ev = unsafe { &*event };
    let input = to_input_event(ev);
    let bytes = match postcard::to_stdvec(&input) {
        Ok(b) => b,
        Err(_) => return err_cstr("输入事件序列化失败"),
    };
    let ct = aead_seal(key, &bytes);
    if s.rt.is_none() {
        s.rt = Some(Runtime::new().expect("无法创建 tokio runtime"));
    }
    let rt = s.rt.as_ref().unwrap();
    match rt.block_on(input_send.send(&Message::Encrypted(ct))) {
        Ok(_) => {
            clear_last_error();
            ptr::null_mut()
        }
        Err(_) => err_cstr("输入发送失败（通道已关闭）"),
    }
}

/// Viewer 发送一条带 Unicode 字符的按键事件（IME 友好，软键盘合成输入）。
///
/// `character` 为 UTF-8 null 终止 C 字符串；null 表示无字符（纯 scancode）。
/// 内部构造 `InputKind::KeyWithChar`（双发 scancode + character），Host 端注入时
/// 优先用 character 做 `enigo.text()` 文本注入，无 character 时 fallback scancode。
/// 成功返回 NULL，失败返回错误串。
#[no_mangle]
pub extern "C" fn rdcore_viewer_send_input_key(
    session: *mut RdSession,
    key_code: u32,
    character: *const c_char,
    pressed: c_int,
    modifiers: u32,
) -> *mut c_char {
    if session.is_null() {
        return err_cstr("null session");
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Viewer {
        return err_cstr("only Viewer can send input");
    }
    let input_send = match s.input_send.as_ref() {
        Some(c) => c,
        None => return err_cstr("先调用 rdcore_session_attach_loopback_media"),
    };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => return err_cstr("尚未建立 E2E 会话密钥"),
    };
    let character = if character.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(character) }.to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        }
    };
    let input = InputEvent {
        seq: 0,
        kind: InputKind::KeyWithChar {
            key_code,
            character,
            pressed: pressed != 0,
            modifiers: modifiers as u16,
        },
    };
    let bytes = match postcard::to_stdvec(&input) {
        Ok(b) => b,
        Err(_) => return err_cstr("输入事件序列化失败"),
    };
    let ct = aead_seal(key, &bytes);
    if s.rt.is_none() {
        s.rt = Some(Runtime::new().expect("无法创建 tokio runtime"));
    }
    let rt = s.rt.as_ref().unwrap();
    match rt.block_on(input_send.send(&Message::Encrypted(ct))) {
        Ok(_) => {
            clear_last_error();
            ptr::null_mut()
        }
        Err(_) => err_cstr("输入发送失败（通道已关闭）"),
    }
}

/// Host 轮询一条输入事件（Viewer → Host，E2E 加密）。收到返回 `RdInputEvent`，通道关闭或出错返回 NULL。
///
/// 非输入类加密消息（心跳 / 授权）透明跳过，继续轮询。
#[no_mangle]
pub extern "C" fn rdcore_host_poll_input(session: *mut RdSession) -> *mut RdInputEvent {
    if session.is_null() {
        set_last_error("null session");
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    if s.role != Role::Host {
        set_last_error("only Host can poll input");
        return ptr::null_mut();
    }
    let input_recv = match s.input_recv.as_ref() {
        Some(c) => c,
        None => {
            set_last_error("先调用 rdcore_session_attach_loopback_media");
            return ptr::null_mut();
        }
    };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("尚未建立 E2E 会话密钥");
            return ptr::null_mut();
        }
    };
    if s.rt.is_none() {
        s.rt = Some(Runtime::new().expect("无法创建 tokio runtime"));
    }
    let rt = s.rt.as_ref().unwrap();
    loop {
        let msg = match rt.block_on(input_recv.recv()) {
            Ok(Some(m)) => m,
            Ok(None) => {
                set_last_error("输入通道已关闭");
                return ptr::null_mut();
            }
            Err(_) => {
                set_last_error("输入接收失败");
                return ptr::null_mut();
            }
        };
        match msg {
            Message::Encrypted(ct) => {
                if let Some(plain) = aead_open(key, &ct) {
                    if let Ok(input) = postcard::from_bytes::<InputEvent>(&plain) {
                        return Box::into_raw(Box::new(RdInputEvent::from_input(&input)));
                    }
                }
                // 解密失败 / 反序列化失败：忽略，继续轮询。
            }
            _ => { /* 忽略非输入加密消息 */ }
        }
    }
}

// ───────────────────────── Track B 韧性面（kimi-k3）：文件传输 / 剪贴板（B6） ─────────────────────────
//
// 与 Track A 输入路径同构：在已建立的 E2E 会话密钥下，把 `FileTransfer` / `Clipboard` 封装成
// `Message::Encrypted` 字节交给 Dart 经控制通道收发（云端只见密文）。Track A 的媒体/输入函数
// 一字未动（§8 注 1：共享文件 append-only，双方各加各的独立函数块）。
//
// 安全：文件传输默认 opt-in，Host 逐次同意——`rdcore_file_host_decide` 之前收到的 `Chunk`
// 一律视为协议违规（`TransferSession` 返回 NotAccepted）。分片大小受 `MAX_FILE_CHUNK_SIZE`
// 约束，`rdcore-proto::Message::validate` 在接收侧再兜一层。

/// 内部：把一条 `FileTransferEvent` 加密成 `Message::Encrypted` 字节（发送侧共用）。
fn seal_file_event(s: &RdSession, ev: &FileTransferEvent) -> *mut RdBytes {
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("尚未建立 E2E 会话密钥");
            return ptr::null_mut();
        }
    };
    let msg = Message::FileTransfer(ev.clone());
    let plain = match encode(&msg) {
        Ok(b) => b,
        Err(e) => {
            set_last_error(e);
            return ptr::null_mut();
        }
    };
    let ct = aead_seal(key, &plain);
    encode_message(&Message::Encrypted(ct))
}

/// 内部：解密一条 `Message::Encrypted` 字节并还原 `FileTransferEvent`（接收侧共用）。
fn open_file_event(s: &RdSession, data: *const u8, len: usize) -> Option<FileTransferEvent> {
    let key = s.session_key.as_ref()?;
    let buf = unsafe { std::slice::from_raw_parts(data, len) };
    let msg = decode(buf).ok()?;
    let ct = match msg {
        Message::Encrypted(c) => c,
        _ => return None,
    };
    let plain = aead_open(key, &ct)?;
    match decode(&plain).ok()? {
        Message::FileTransfer(ev) => Some(ev),
        _ => None,
    }
}

/// Viewer 提议一次文件传输（Offer）。返回加密字节；对端 `rdcore_file_host_on_offer` 收。
#[no_mangle]
pub extern "C" fn rdcore_file_send_offer(
    session: *mut RdSession,
    transfer_id: u64,
    name: *const c_char,
    size: u64,
) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let name = unsafe { cstr_to_string(name) }.unwrap_or_else(|| "file".into());
    let ev = rdcore_app::file_transfer::make_offer(transfer_id, &name, size);
    seal_file_event(s, &ev)
}

/// Viewer 在收到 Host 的 `Accept` 后，发送一个数据分片。返回加密字节。
#[no_mangle]
pub extern "C" fn rdcore_file_send_chunk(
    session: *mut RdSession,
    transfer_id: u64,
    seq: u64,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let piece = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let ev = FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Chunk { seq, data: piece },
    };
    seal_file_event(s, &ev)
}

/// Viewer 发送收尾事件（`Done`，含总分片数）。返回加密字节。
#[no_mangle]
pub extern "C" fn rdcore_file_send_done(
    session: *mut RdSession,
    transfer_id: u64,
    chunks: u64,
) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let ev = rdcore_app::file_transfer::make_done(transfer_id, chunks);
    seal_file_event(s, &ev)
}

/// Host 收到一条文件加密字节并尝试解析为 `Offer`；是 Offer 则建立接收会话并返回
/// 加密字节（回显 `transfer_id` 供 Dart 关联），非 Offer 或校验失败返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_file_host_on_offer(
    session: *mut RdSession,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    let ev = match open_file_event(s, data, len) {
        Some(e) => e,
        None => {
            set_last_error("非 FileTransfer 消息或解密失败");
            return ptr::null_mut();
        }
    };
    match rdcore_app::file_transfer::TransferSession::on_offer(&ev) {
        Ok(sess) => {
            let id = sess.transfer_id();
            s.file_sessions.insert(id, sess);
            clear_last_error();
            // 回显 transfer_id（8 字节小端）供 Dart 关联本次传输。
            vec_to_rdbytes(id.to_le_bytes().to_vec())
        }
        Err(e) => {
            set_last_error(format!("建立文件接收会话失败: {e}"));
            ptr::null_mut()
        }
    }
}

/// Host 对已收到的 Offer 做决定：`accept!=0` 同意（之后可收 Chunk），否则拒绝。
/// 返回加密字节（Accept/Reject 事件）供回传 Viewer。失败返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_file_host_decide(
    session: *mut RdSession,
    transfer_id: u64,
    accept: c_int,
    reason: *const c_char,
) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    let ev = if accept != 0 {
        match s.file_sessions.get_mut(&transfer_id) {
            Some(sess) => {
                sess.accept();
                FileTransferEvent {
                    transfer_id,
                    action: FileTransferAction::Accept,
                }
            }
            None => {
                set_last_error("未找到该 transfer_id 的 Offer（先 rdcore_file_host_on_offer）");
                return ptr::null_mut();
            }
        }
    } else {
        let r = unsafe { cstr_to_string(reason) }.unwrap_or_else(|| "rejected".into());
        rdcore_app::file_transfer::TransferSession::reject_event(transfer_id, &r)
    };
    seal_file_event(s, &ev)
}

/// Host 接收一个分片或收尾事件。返回：完成时返回完整文件字节（`RdBytes`），
/// 否则（中间分片/未完成）返回 NULL 且 `rdcore_last_error` 为 NULL；出错返回 NULL 且设错误。
///
/// 区分「中间态」与「完成」靠返回值是否为 NULL + `rdcore_last_error`：完成时返回非空。
#[no_mangle]
pub extern "C" fn rdcore_file_host_on_event(
    session: *mut RdSession,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    let ev = match open_file_event(s, data, len) {
        Some(e) => e,
        None => {
            set_last_error("非 FileTransfer 消息或解密失败");
            return ptr::null_mut();
        }
    };
    let id = ev.transfer_id;
    let sess = match s.file_sessions.get_mut(&id) {
        Some(x) => x,
        None => {
            set_last_error("未找到该 transfer_id 的接收会话");
            return ptr::null_mut();
        }
    };
    match sess.on_event(&ev) {
        Ok(Some(complete)) => {
            // 传输完成：清掉会话，返回完整字节。
            s.file_sessions.remove(&id);
            clear_last_error();
            vec_to_rdbytes(complete)
        }
        Ok(None) => {
            // 中间分片已收下，未完成。
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => {
            set_last_error(format!("文件分片处理失败: {e}"));
            ptr::null_mut()
        }
    }
}

/// Viewer 接收 Host 的决定（Accept/Reject）。返回加密字节回显 `1`=Accept / `0`=Reject
/// （单字节），失败返回 NULL。Dart 据此决定是否开始发 Chunk。
#[no_mangle]
pub extern "C" fn rdcore_file_viewer_on_decision(
    session: *mut RdSession,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &mut *session };
    match open_file_event(s, data, len) {
        Some(FileTransferEvent {
            action: FileTransferAction::Accept,
            ..
        }) => {
            clear_last_error();
            vec_to_rdbytes(vec![1u8])
        }
        Some(FileTransferEvent {
            action: FileTransferAction::Reject { .. },
            ..
        }) => {
            clear_last_error();
            vec_to_rdbytes(vec![0u8])
        }
        _ => {
            set_last_error("非 Accept/Reject 决定");
            ptr::null_mut()
        }
    }
}

/// 任一端发送一条剪贴板事件（Request/Data/Clear）。返回加密字节。
/// `action`: 0=Request, 1=Data, 2=Clear；`data`/`len` 仅 Data 用。
#[no_mangle]
pub extern "C" fn rdcore_clipboard_send(
    session: *mut RdSession,
    seq: u64,
    action: c_int,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("尚未建立 E2E 会话密钥");
            return ptr::null_mut();
        }
    };
    let act = match action {
        0 => ClipboardAction::Request,
        1 => {
            if data.is_null() {
                set_last_error("clipboard Data 需要非空 data");
                return ptr::null_mut();
            }
            ClipboardAction::Data(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
        }
        _ => ClipboardAction::Clear,
    };
    let msg = Message::Clipboard(ClipboardEvent { seq, action: act });
    let plain = match encode(&msg) {
        Ok(b) => b,
        Err(e) => {
            set_last_error(e);
            return ptr::null_mut();
        }
    };
    let ct = aead_seal(key, &plain);
    encode_message(&Message::Encrypted(ct))
}

/// 接收一条剪贴板加密字节并还原为 `(action, data)`。返回 `RdBytes`：首字节为 action
/// （0=Request,1=Data,2=Clear），其后为 Data 负载（非 Data 时仅 1 字节）。失败返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_clipboard_recv(
    session: *mut RdSession,
    data: *const u8,
    len: usize,
) -> *mut RdBytes {
    if session.is_null() || data.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { &*session };
    let key = match s.session_key.as_ref() {
        Some(k) => k,
        None => {
            set_last_error("尚未建立 E2E 会话密钥");
            return ptr::null_mut();
        }
    };
    let buf = unsafe { std::slice::from_raw_parts(data, len) };
    let msg = match decode(buf) {
        Ok(m) => m,
        Err(e) => {
            set_last_error(format!("decode: {e}"));
            return ptr::null_mut();
        }
    };
    let ct = match msg {
        Message::Encrypted(c) => c,
        _ => {
            set_last_error("expected Encrypted message");
            return ptr::null_mut();
        }
    };
    let plain = match aead_open(key, &ct) {
        Some(p) => p,
        None => {
            set_last_error("剪贴板解密失败");
            return ptr::null_mut();
        }
    };
    let ev = match decode(&plain) {
        Ok(Message::Clipboard(e)) => e,
        _ => {
            set_last_error("非 Clipboard 消息");
            return ptr::null_mut();
        }
    };
    let out = match ev.action {
        ClipboardAction::Request => vec![0u8],
        ClipboardAction::Clear => vec![2u8],
        ClipboardAction::Data(d) => {
            let mut v = Vec::with_capacity(1 + d.len());
            v.push(1u8);
            v.extend_from_slice(&d);
            v
        }
    };
    clear_last_error();
    vec_to_rdbytes(out)
}

// ───────────────────────── Track B 韧性面（kimi-k3）：配对 / 发现（B3） ─────────────────────────
//
// 生成配对邀请（session_id + token），供 Host 展示配对码/二维码、Viewer 输码/扫码后
// 带 token 连信令。与 `rdcore_app::Connection::create_pairing()` 输出格式一致（§5 协调点1）：
// - session_id：16 字节 CSPRNG（hex 展示 32 字符）。
// - token：32 字节 CSPRNG（hex 展示 64 字符）。配对不焚毁：受控端在线期间可重复扫码；
//   经 `rdcore_pairing_publish` / `rdcore_pairing_revoke` 发布与撤销（A5↔B2 对接点）。
// 本函数自实现 CSPRNG 生成（不依赖 rdcore-app 编译），避免被 Track A 的 A1/A4 进行中状态阻塞。

/// 配对邀请（C 侧只读；Dart 拷贝字段后须用 [`rdcore_pairing_info_free`] 释放）。
#[repr(C)]
pub struct RdPairingInfo {
    /// 16 字节会话 ID。
    pub session_id: [u8; 16],
    /// 64 字符小写 hex token（NUL 结尾的 C 字符串）。
    pub token: *mut c_char,
}

/// 生成一次配对邀请（Host 侧调用）。返回 `RdPairingInfo`，用 [`rdcore_pairing_info_free`] 释放。
#[no_mangle]
pub extern "C" fn rdcore_create_pairing() -> *mut RdPairingInfo {
    let mut sid = [0u8; 16];
    if getrandom::getrandom(&mut sid).is_err() {
        set_last_error("系统随机数不可用，无法生成 session_id");
        return ptr::null_mut();
    }
    let mut tok = [0u8; 32];
    if getrandom::getrandom(&mut tok).is_err() {
        set_last_error("系统随机数不可用，无法生成 token");
        return ptr::null_mut();
    }
    // 小写 hex（64 字符），与计划 §5.1 一致。
    let mut hex = String::with_capacity(64);
    for b in tok {
        hex.push(std::char::from_digit((b >> 4) as u32, 16).unwrap());
        hex.push(std::char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    clear_last_error();
    Box::into_raw(Box::new(RdPairingInfo {
        session_id: sid,
        token: to_cstr(hex),
    }))
}

/// 释放 [`RdPairingInfo`]（含内部 token C 字符串）。
#[no_mangle]
pub extern "C" fn rdcore_pairing_info_free(info: *mut RdPairingInfo) {
    if info.is_null() {
        return;
    }
    unsafe {
        let i = Box::from_raw(info);
        if !i.token.is_null() {
            drop(CString::from_raw(i.token));
        }
    }
}

// ── 配对发布 / 撤销（受控端，A5↔B2 对接点）──
//
// 与 `rdcore-desktop` 的 `token_db` 模块语义严格对齐（文件格式 / 环境变量 / 心跳周期一致）：
// 发布 = 把 `session_hex\ttoken` 写入共享 token 库文件，并以 30s 心跳重写保鲜；
// 撤销 = 停心跳 + 删文件。信令侧（signaling-svc `reload_from_file`）以文件为事实：
// 配对不焚毁，受控端在线期间可重复扫码建连；删文件（主动取消）、覆写（刷新二维码）、
// 心跳过期（受控端退出/崩溃，3 分钟）都会使配对在下一次握手时失效。

/// token 库文件心跳周期（必须明显小于 signaling-svc 的 `TOKEN_FILE_STALE_AFTER` = 3min）。
const PAIRING_FILE_HEARTBEAT: Duration = Duration::from_secs(30);

/// 共享 token 库文件路径（与 rdcore-desktop `token_db::token_db_path` 对齐）。
fn pairing_token_db_path() -> std::path::PathBuf {
    std::env::var("SIGNALING_TOKEN_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("signaling_token_db.txt"))
}

/// 心跳停止标志（重新发布 / 撤销时置位，旧心跳线程随之退出）。
struct PairingPublishGuard {
    stop: Arc<AtomicBool>,
}

static PAIRING_PUBLISH: std::sync::OnceLock<Mutex<Option<PairingPublishGuard>>> =
    std::sync::OnceLock::new();

fn pairing_publish_slot() -> &'static Mutex<Option<PairingPublishGuard>> {
    PAIRING_PUBLISH.get_or_init(|| Mutex::new(None))
}

/// 发布配对（受控端调用）：写入 token 库文件并启动心跳线程。
/// 重复调用即「刷新二维码」：覆写文件为新 session/token 并重启心跳，旧配对随即失效。
/// 返回 1 成功 / 0 失败（原因经 `rdcore_last_error` 取）。
#[no_mangle]
pub extern "C" fn rdcore_pairing_publish(session_id: *const u8, token: *const c_char) -> c_int {
    if session_id.is_null() {
        set_last_error("null session_id");
        return 0;
    }
    let sid = unsafe { std::slice::from_raw_parts(session_id, 16) };
    let Some(tok) = (unsafe { cstr_to_string(token) }) else {
        set_last_error("null token");
        return 0;
    };
    let line = format!("{}\t{}\n", hex::encode(sid), tok);
    let path = pairing_token_db_path();
    if let Err(e) = std::fs::write(&path, &line) {
        set_last_error(format!("写入 token 库文件失败（{}）: {e}", path.display()));
        return 0;
    }
    // 停旧心跳，起新心跳（重复发布 = 刷新）。
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut g = pairing_publish_slot().lock().unwrap();
        if let Some(old) = g.take() {
            old.stop.store(true, Ordering::SeqCst);
        }
        g.replace(PairingPublishGuard {
            stop: stop.clone(),
        });
    }
    let hb_stop = stop.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(PAIRING_FILE_HEARTBEAT);
            if hb_stop.load(Ordering::SeqCst) {
                break;
            }
            // 重写即刷新 mtime；写失败不致命，下一次心跳再试。
            let _ = std::fs::write(pairing_token_db_path(), &line);
        }
    });
    clear_last_error();
    1
}

/// 撤销配对（受控端主动取消 / 退出前调用）：停心跳并删除 token 库文件（幂等）。
/// 信令侧下一次握手 reconcile 时配对码即失效。
#[no_mangle]
pub extern "C" fn rdcore_pairing_revoke() {
    {
        let mut g = pairing_publish_slot().lock().unwrap();
        if let Some(old) = g.take() {
            old.stop.store(true, Ordering::SeqCst);
        }
    }
    match std::fs::remove_file(pairing_token_db_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => set_last_error(format!("删除 token 库文件失败: {e}")),
    }
}

/// 把 16 字节 session_id 编为 32 字符小写 hex（信令 URL 路径用），供 Dart 直接展示/拼接。
/// 返回 C 字符串，用 [`rdcore_string_free`] 释放。
#[no_mangle]
pub extern "C" fn rdcore_session_id_to_hex(session_id: *const u8) -> *mut c_char {
    if session_id.is_null() {
        return null_cstr();
    }
    let bytes = unsafe { std::slice::from_raw_parts(session_id, 16) };
    let mut hex = String::with_capacity(32);
    for b in bytes {
        hex.push(std::char::from_digit((b >> 4) as u32, 16).unwrap());
        hex.push(std::char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    clear_last_error();
    to_cstr(hex)
}

// ───────────────────────────── 真实 WebRTC 连接（缺口 M：Viewer Peer） ─────────────────────────────
//
// 把 `rdcore_app::Connection` 封装成 C 句柄。Viewer 与 Host 共用同一套 webrtc-rs PeerConnection +
// 信令握手 + ICE + E2E 密钥 + 同意逻辑（与 `rdcore-desktop` 完全一致），信令由 `Connection`
// 内部的 `SignalingClient` 持有。Flutter/iOS 只需持有本句柄并调用下方函数，不再自行维护
// WebSocket 信令（仅 Mock 路径用 Dart 信令）。

/// 真零拷贝纹理目标（原生侧分配、Rust 直接写入的可写像素缓冲）。
///
/// `ptr` 为**同进程**内的原生可写地址：iOS 为 `CVPixelBuffer` 基址（CPU 缓冲）；
/// Android 为 `ANativeWindow*`（Rust 经 EGL 上传，见 `mode`）。
/// `mode`：`0` = CPU 缓冲（Rust 直接 memcpy，iOS 走此路径）；`1` = `ANativeWindow`
/// （Rust 经 EGL 上传到 `SurfaceTexture`，Android 用，GPU 直出，待 host 硬编团队接入 GLES）。
/// `format`：`0` = BGRA（iOS `kCVPixelFormatType_32BGRA`）；`1` = RGBA（Android GL 上传）。
// 注：`stride`/`mode`/`format` 在推送模型下不再由 Rust 直接解释（交由原生插件 C 函数处理），
// 仅作为 FFI 契约保留，供 Dart 侧按平台填写；允许未读以避免 dead_code 告警。
#[repr(C)]
#[allow(dead_code)]
pub struct RdTextureTarget {
    /// 同进程内可写像素缓冲地址（iOS=CVPixelBuffer 基址 / Android=ANativeWindow*）。
    pub ptr: *mut std::os::raw::c_void,
    /// 缓冲宽（像素）。
    pub width: u32,
    /// 缓冲高（像素）。
    pub height: u32,
    /// 每行字节数（允许行对齐，Rust 按 stride 逐行拷贝）。
    pub stride: u32,
    /// 写入模式（见上方说明）。
    pub mode: u32,
    /// 像素格式（见上方说明）。
    pub format: u32,
}

/// 真实连接句柄：封装 `Arc<Connection>`（可在后台任务间安全共享）+ 专用 tokio runtime。
pub struct RdConnection {
    conn: std::sync::Arc<Connection>,
    rt: Runtime,
    is_host: bool,
    /// Host 侧媒体泵（drop 时停止抓取循环）。
    pump: Option<HostMediaPump>,
    /// Host 侧音频泵（drop 时停止采集→编码→发送循环，与媒体泵平行、互不阻塞）。
    audio_pump: Option<HostAudioPump>,
    /// Host 授权范围（establish 时作为 `ConsentDecision::Grant` 下发）。
    scopes: std::collections::HashSet<ConsentScope>,
    /// 真零拷贝纹理目标（Viewer 侧）。挂上后 `render_to_texture` 经全局注册的
    /// `rdcore_texture_submit` C 函数把解码帧推给原生插件，由插件拷贝到原生 backing 并
    /// 上传 GPU，Dart 经 `Texture` 控件合成——像素绝不进入 Dart 堆。`ptr` 为原生插件返回的
    /// 纹理句柄（iOS=CVPixelBuffer 基址 / Android=DirectByteBuffer 地址）。None = 走旧
    /// pull_frame 字节路径（兼容 headless / 桌面 / 无纹理插件的平台）。
    texture_target: Option<RdTextureTarget>,
    /// 最近一次收到帧的尺寸（供纹理 resize 用）；用 Mutex 以便 `render_to_texture` 的
    /// 不可变借用也能写入。
    last_frame_size: std::sync::Mutex<Option<(u32, u32)>>,
}

/// 构造 Viewer（控制端）真实连接并连上信令服务器（握手由 [`rdcore_connection_establish`] 触发）。
///
/// `url` 为信令基址（不含 session/token），形如 `wss://host/signaling`；本函数按 Host 侧
/// 解析 FFI 传入的 ICE 服务器清单（显式覆盖环境变量，供移动端注入 TURN 凭据）。
///
/// 格式：每行一个服务器，`|` 分隔凭据；同一服务器的多个 URL 用 `,` 分隔。
/// - STUN：`stun:host:port`
/// - TURN：`turn:host:port?transport=udp|username|credential`
///
/// 空串 / 全空行 → 返回 `None`（交由 [`RtcConfig::from_env`] 用环境变量兜底）。
fn parse_ice_servers(spec: &str) -> Option<Vec<IceServer>> {
    let mut out: Vec<IceServer> = Vec::new();
    for raw in spec.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '|');
        let urls_part = match parts.next() {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => continue,
        };
        let urls: Vec<String> = urls_part
            .split(',')
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
            .collect();
        if urls.is_empty() {
            continue;
        }
        let username = parts
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let credential = parts
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        out.push(IceServer {
            urls,
            username,
            credential,
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `build_signaling_url` 约定拼接为 `wss://host/signaling/<session_hex>?token=<token>`。
#[no_mangle]
pub extern "C" fn rdcore_connection_new_viewer(
    url: *const c_char,
    session_hex: *const c_char,
    token: *const c_char,
    local: *mut RdLocal,
    include_loopback: c_int,
    force_relay: c_int,
    hb_ms: c_int,
    ice_servers: *const c_char,
) -> *mut RdConnection {
    new_connection(
        url,
        session_hex,
        token,
        local,
        include_loopback,
        force_relay,
        hb_ms,
        false,
        0,
        ice_servers,
    )
}

/// 构造 Host（被控端）真实连接（同 Viewer，仅角色与默认授权范围不同）。
#[no_mangle]
pub extern "C" fn rdcore_connection_new_host(
    url: *const c_char,
    session_hex: *const c_char,
    token: *const c_char,
    local: *mut RdLocal,
    include_loopback: c_int,
    force_relay: c_int,
    hb_ms: c_int,
    scopes_mask: c_int,
    ice_servers: *const c_char,
) -> *mut RdConnection {
    new_connection(
        url,
        session_hex,
        token,
        local,
        include_loopback,
        force_relay,
        hb_ms,
        true,
        scopes_mask,
        ice_servers,
    )
}

#[allow(clippy::too_many_arguments)]
fn new_connection(
    url: *const c_char,
    session_hex: *const c_char,
    token: *const c_char,
    local: *mut RdLocal,
    include_loopback: c_int,
    force_relay: c_int,
    hb_ms: c_int,
    is_host: bool,
    scopes_mask: c_int,
    ice_servers: *const c_char,
) -> *mut RdConnection {
    let base = match unsafe { cstr_to_string(url) } {
        Some(s) if !s.is_empty() => s,
        _ => {
            set_last_error("url 为空");
            return ptr::null_mut();
        }
    };
    let shex = match unsafe { cstr_to_string(session_hex) } {
        Some(s) if s.len() == 32 => s,
        _ => {
            set_last_error("session_hex 必须为 32 位十六进制");
            return ptr::null_mut();
        }
    };
    let tok = unsafe { cstr_to_string(token) }.unwrap_or_default();
    if local.is_null() {
        set_last_error("null local");
        return ptr::null_mut();
    }
    let l = unsafe { &*local };

    let bytes = match hex::decode(&shex) {
        Ok(b) if b.len() == 16 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&b);
            a
        }
        _ => {
            set_last_error("session_hex 解析失败");
            return ptr::null_mut();
        }
    };
    let session_id = SessionId(bytes);
    let full_url = format!("{}/{}?token={}", base.trim_end_matches('/'), shex, tok);

    let store: std::sync::Arc<std::sync::Mutex<dyn IdentityStore + Send + Sync>> =
        std::sync::Arc::new(std::sync::Mutex::new(l.store.clone()));
    let secret = l.secret.clone();

    let ice_spec = unsafe { cstr_to_string(ice_servers) }.unwrap_or_default();
    let mut cfg = RtcConfig::from_env();
    if !ice_spec.trim().is_empty() {
        if let Some(servers) = parse_ice_servers(&ice_spec) {
            cfg.ice_servers = servers;
        }
    }
    if include_loopback != 0 {
        cfg.include_loopback = true;
    }
    if force_relay != 0 {
        cfg.force_relay = true;
    }
    let hb = Duration::from_millis(hb_ms.max(1) as u64);

    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            set_last_error(format!("无法创建 runtime: {e}"));
            return ptr::null_mut();
        }
    };
    let built = if is_host {
        rt.block_on(Connection::new_host(
            &full_url, session_id, secret, store, cfg, hb,
        ))
    } else {
        rt.block_on(Connection::new_viewer(
            &full_url, session_id, secret, store, cfg, hb,
        ))
    };
    match built {
        Ok(c) => Box::into_raw(Box::new(RdConnection {
            conn: std::sync::Arc::new(c),
            rt,
            is_host,
            pump: None,
            audio_pump: None,
            scopes: bits_to_scopes(scopes_mask as u32),
            texture_target: None,
            last_frame_size: std::sync::Mutex::new(None),
        })),
        Err(e) => {
            set_last_error(format!("建连失败: {e}"));
            ptr::null_mut()
        }
    }
}

/// 跑完整条握手（offer/answer + ICE + E2E 密钥 + 同意）。Host 默认授权 `scopes` 范围；
/// Viewer 传 `None`。握手完成即代表连接可用（数据通道 open + 会话密钥就绪）。
///
/// ⚠️ 会阻塞调用线程至握手完成（通常数百毫秒~数秒）；Flutter 侧应在后台 isolate 调用。
#[no_mangle]
pub extern "C" fn rdcore_connection_establish(conn: *mut RdConnection) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &*conn };
    let conn_arc = c.conn.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let decision = if c.is_host {
        Some(ConsentDecision::Grant {
            scopes: c.scopes.clone(),
            duration: None,
        })
    } else {
        None
    };
    match c.rt.block_on(conn_arc.establish(stop, decision.clone())) {
        Ok(()) => {
            // Host 常驻监听：对端掉线或 Viewer 重扫同一配对码时，原地重连接入下一个
            // Viewer（与桌面 Agent 的「接受 → 等断开 → 重连」循环同一语义），而不是让
            // 会话随首次断开死亡、二维码变死码。会话结束由 `rdcore_connection_free`
            // drop RdConnection → drop Runtime 时一并终止本任务。
            if let (true, Some(d)) = (c.is_host, decision) {
                let conn2 = conn_arc.clone();
                c.rt.spawn(async move {
                    loop {
                        let outcome = conn2.wait_peer_gone_or_rescan().await;
                        eprintln!("[ffi-host] 对端离开/重扫（{outcome:?}），等待下一个 Viewer…");
                        loop {
                            match conn2.reconnect_with(d.clone()).await {
                                Ok(()) => {
                                    eprintln!("[ffi-host] 已接入新 Viewer");
                                    break;
                                }
                                Err(e) => {
                                    eprintln!("[ffi-host] 重连失败：{e:#}，2 秒后重试…");
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                }
                            }
                        }
                    }
                });
            }
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => err_cstr(format!("握手失败: {e}")),
    }
}

/// 设置本连接的视频编解码器（0=Raw 直通，1=H.264）。Host 与 Viewer 必须协商一致：
/// Host 按此编码抓屏帧，Viewer 按此解码；不一致将导致解码失败。默认 Raw。
///
/// 真实 WebRTC 媒体走 SCTP DataChannel，单条消息有上限（默认约 64 KiB）；Raw 1280×720
/// 帧 ≈ 3.7 MiB 会超过上限导致发送失败，故 1080p 级分辨率请用 H.264（编码后仅数 KiB）。
#[no_mangle]
pub extern "C" fn rdcore_connection_set_video_codec(
    conn: *mut RdConnection,
    codec: c_int,
) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &*conn };
    let vc = match codec {
        1 => VideoCodec::H264,
        _ => VideoCodec::Raw,
    };
    c.conn.set_video_codec(vc);
    clear_last_error();
    ptr::null_mut()
}

/// Viewer 拉取一帧已解密/解码/渲染的画面（RGBA8888）。通道关闭或出错返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_connection_pull_frame(conn: *mut RdConnection) -> *mut RdMediaFrame {
    if conn.is_null() {
        set_last_error("null conn");
        return ptr::null_mut();
    }
    let c = unsafe { &*conn };
    if c.is_host {
        set_last_error("only Viewer can pull frames");
        return ptr::null_mut();
    }
    match c.rt.block_on(c.conn.recv_rendered()) {
        Ok(Some(first)) => {
            // ── 追帧丢旧（与 render_to_texture 路径同一策略，治延迟累积）────────
            // macOS/桌面无原生纹理插件时走本字节路径，消费（33ms 拉帧 + UI 刷新）
            // 天然慢于 Host 60fps 生产；若严格 FIFO 逐帧渲染，mpsc 队列与 SCTP
            // 缓冲里积压的"过去"画面会形成持续累积的恒定延迟。把积压帧全部取出
            // （recv_rendered 已完成解码，参考链完整），只返回最新一帧。
            //
            // 取消安全性同纹理路径：await 点均为 cancel-safe，partial 分片状态
            // 持久于传输层，timeout 取消不会吞半帧。
            let mut rendered = first;
            loop {
                let next = c.rt.block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_millis(1),
                        c.conn.recv_rendered(),
                    )
                    .await
                });
                match next {
                    // 又取到一帧：前一帧已解码，直接丢弃（Rust 值自动释放），保留最新。
                    Ok(Ok(Some(newer))) => rendered = newer,
                    // 队列已空 / 通道关闭 / 取帧出错：停止追赶，照常返回最新帧。
                    _ => break,
                }
            }
            // 把 RGBA 缓冲所有权移交给 C（调用方用 rdcore_media_frame_free 释放）。
            let mut rgba = rendered.rgba;
            let len = rgba.len();
            let data = rgba.as_mut_ptr();
            std::mem::forget(rgba);
            clear_last_error();
            Box::into_raw(Box::new(RdMediaFrame {
                width: rendered.width,
                height: rendered.height,
                data,
                len,
            }))
        }
        Ok(None) => {
            set_last_error("媒体通道已关闭（Host 已停止抓屏）");
            ptr::null_mut()
        }
        Err(e) => {
            set_last_error(format!("拉帧失败: {e}"));
            ptr::null_mut()
        }
    }
}

// ───────────────────────── 真零拷贝纹理（终极 Viewer 渲染路径） ─────────────────────────
//
// ── 真零拷贝：原生插件提交函数（推送模型） ───────────────────────────────
// 与 RustDesk `flutter_texture_rgba_renderer` 同构：原生插件（iOS/Android）创建 Flutter
// `Texture` 并导出 C 函数 `rdcore_texture_submit`；Dart 经 FFI 把该函数指针交给本层
// （`rdcore_texture_set_submit_fn`）。每解码一帧，本层直接调用该 C 函数，把 Rust 自己的
// RGBA 缓冲推给插件；插件负责拷贝到原生 backing 并上传 GPU——像素从不进入 Dart 堆。
// 相比旧 `render_to_texture` 的「Rust memcpy 进 Dart 提供的 CVPixelBuffer 基址」，本模型
// 让插件拥有纹理生命周期与 GPU 上传，Dart 只传一个整数指针（纹理句柄），更稳健且可移植。
type TextureSubmitFn = unsafe extern "C" fn(
    texture: *mut std::os::raw::c_void,
    buffer: *const u8,
    len: c_int,
    width: c_int,
    height: c_int,
    stride: c_int,
    format: c_int,
);

static TEXTURE_SUBMIT_FN: Mutex<Option<TextureSubmitFn>> = Mutex::new(None);

/// 注册原生纹理提交函数（由 Dart 经 FFI 传入插件导出的 `rdcore_texture_submit` 指针）。
///
/// `f` 为函数指针（以 `*const c_void` 传入）；传 `NULL`/0 表示注销（回退字节路径）。
/// 该函数全局只与「一个原生纹理插件」绑定，与具体连接无关；每个连接的纹理句柄经
/// [`rdcore_connection_attach_texture`] 单独挂接。
#[no_mangle]
pub extern "C" fn rdcore_texture_set_submit_fn(f: *const std::os::raw::c_void) {
    if let Ok(mut g) = TEXTURE_SUBMIT_FN.lock() {
        *g = if f.is_null() {
            None
        } else {
            // 函数指针与数据指针同宽，可安全 transmute。
            Some(unsafe { std::mem::transmute(f) })
        };
    }
}

// 设计（推送模型，与 RustDesk `flutter_texture_rgba_renderer` 同构）：原生端（Flutter 插件）
// 经 Flutter `TextureRegistry` 创建 GPU 纹理，并把其**可写 backing 地址**（iOS=`CVPixelBuffer`
// 基址 / Android=`DirectByteBuffer` 地址）作为纹理句柄通过 `rdcore_connection_attach_texture`
// 交给本 FFI；同时 Dart 把插件导出的 `rdcore_texture_submit` C 函数指针经
// `rdcore_texture_set_submit_fn` 注册进本层。`render_to_texture` 每帧调用该 C 函数，把 Rust
// 解码出的 RGBA 缓冲推给插件，由插件拷贝到原生 backing 并上传 GPU——像素绝不进入 Dart 堆。
// 这是相对旧 `pull_frame`（RGBA 跨 FFI→isolate→Dart 多次拷贝）的终极零拷贝路径。
//
// 兼容性：未注册提交函数或未挂纹理时 `render_to_texture` 返回 0，调用方回退旧 `pull_frame`
// 字节路径（桌面 / headless / 无插件平台照常工作）。

/// Viewer 侧把原生纹理句柄挂到连接上，开启真零拷贝渲染。
///
/// `ptr` 为原生插件返回的纹理句柄（iOS=`CVPixelBuffer` 基址；Android=`DirectByteBuffer` 地址），
/// `render_to_texture` 会把它作为 `rdcore_texture_submit` 的第一个参数回传给插件。
/// `mode`/`format`/`stride` 保留为 FFI 契约（由 Dart 按平台填写），实际拷贝与格式转换由插件完成。
/// 成功返回 NULL；失败返回错误串。
#[no_mangle]
pub extern "C" fn rdcore_connection_attach_texture(
    conn: *mut RdConnection,
    ptr: *mut std::os::raw::c_void,
    width: u32,
    height: u32,
    stride: u32,
    mode: u32,
    format: u32,
) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &mut *conn };
    if c.is_host {
        return err_cstr("only Viewer can attach texture");
    }
    if ptr.is_null() {
        return err_cstr("null texture ptr");
    }
    c.texture_target = Some(RdTextureTarget {
        ptr,
        width,
        height,
        stride,
        mode,
        format,
    });
    clear_last_error();
    ptr::null_mut()
}

/// 解除纹理挂接（回退到旧 `pull_frame` 字节路径）。成功返回 NULL。
#[no_mangle]
pub extern "C" fn rdcore_connection_detach_texture(conn: *mut RdConnection) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &mut *conn };
    c.texture_target = None;
    clear_last_error();
    ptr::null_mut()
}

/// Viewer 侧把已解密/解码/渲染的画面直接写入挂接的纹理缓冲（真零拷贝）。
///
/// 返回值：
/// - `1`：已写入当前纹理（调用方应通知 Flutter 重新合成该 `textureId`）；
/// - `0`：无新帧 / 未挂接纹理（非错误，保持上一帧）；
/// - `-1`：出错（详情见 `rdcore_last_error`）；
/// - `2`：帧尺寸与纹理缓冲不符（需 resize，用 [`rdcore_connection_last_frame_size`]
///   取目标尺寸，重新 `attach_texture` 新缓冲）；
/// - `3`：纹理已挂接但无原生提交函数（缺 `rdcore_texture_submit`），调用方应
///   立即回退旧 `pull_frame` 字节路径，否则纹理会卡死不出帧。
#[no_mangle]
pub extern "C" fn rdcore_connection_render_to_texture(conn: *mut RdConnection) -> c_int {
    if conn.is_null() {
        set_last_error("null conn");
        return -1;
    }
    let c = unsafe { &*conn };
    let target = match &c.texture_target {
        Some(t) => t,
        None => return 0,
    };
    if c.is_host {
        set_last_error("only Viewer renders to texture");
        return -1;
    }
    match c.rt.block_on(c.conn.recv_rendered()) {
        Ok(Some(first)) => {
            let mut rendered = first;
            // ── 追帧丢旧（治延迟累积的核心）──────────────────────────────
            // 消费慢于生产时（大分辨率下解码+拷贝耗时 > 帧间隔），mpsc 队列与 SCTP
            // 缓冲里积压的都是"过去"的画面；若逐帧渲染，延迟会持续累积（已实测：
            // 3440×1440 下操作数秒后画面才响应）。正确策略：把队列里积压的帧全部
            // 取出并解码（H.264 P 帧依赖前帧，**必须逐帧解码维持参考链**，不能只
            // 丢字节），但只渲染/提交**最新一帧**，其余丢弃。
            //
            // 取消安全性：`recv_rendered` 内部的 await 点均在 `mpsc::Receiver::recv`
            // 与 tokio `Mutex::lock`（均为 cancel-safe），且分片重组的 partial 状态
            // 持久于传输层——timeout 取消不会吞掉半帧，下次调用可安全续传。
            loop {
                let next = c.rt.block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_millis(1),
                        c.conn.recv_rendered(),
                    )
                    .await
                });
                match next {
                    // 又取到一帧：前一帧已解码（参考链完整），丢弃，保留最新。
                    Ok(Ok(Some(newer))) => rendered = newer,
                    // 队列已空（1ms 内无新帧）/ 通道关闭 / 取帧出错：停止追赶。
                    // 关闭/出错不丢失当前已持有的最新帧，照常渲染，错误由下一轮上报。
                    _ => break,
                }
            }
            let fw = rendered.width;
            let fh = rendered.height;
            // 缓存最近帧尺寸（供 resize 取用）。
            if let Ok(mut g) = c.last_frame_size.lock() {
                *g = Some((fw, fh));
            }
            if fw != target.width || fh != target.height {
                set_last_error("纹理尺寸不符，需 resize");
                return 2;
            }
            // 推送模型真零拷贝：把 Rust 解码出的 RGBA 缓冲直接交给原生插件的
            // `rdcore_texture_submit` C 函数，由插件拷贝到原生 backing 并上传 GPU。
            // 像素从不进入 Dart 堆。无提交函数时返回 3，调用方回退旧 pull_frame 字节路径。
            let submit = *TEXTURE_SUBMIT_FN.lock().unwrap();
            match submit {
                Some(f) => {
                    // 字节序交换下沉到 Rust：两端插件的目标内存布局均为 BGRA
                    //（iOS CVPixelBuffer BGRA / Android Bitmap 小端 B,G,R,A），
                    // 这里就地把 RGBA 交换为 BGRA 后传 format=0，插件走整体
                    // memcpy 快速路径。Rust 循环可被 auto-vectorize（3440×1440
                    // 约 20MB/帧仅需数 ms），远快于插件侧逐像素交换——后者是
                    // 大分辨率下消费速度跟不上生产速度的瓶颈之一。
                    rendered
                        .rgba
                        .chunks_exact_mut(4)
                        .for_each(|px| px.swap(0, 2));
                    unsafe {
                        f(
                            target.ptr,
                            rendered.rgba.as_ptr(),
                            rendered.rgba.len() as c_int,
                            fw as c_int,
                            fh as c_int,
                            (fw as c_int) * 4, // 源缓冲紧密打包，按行拷贝
                            0,                 // 已在 Rust 侧转为 BGRA；插件直接 memcpy
                        );
                    }
                    clear_last_error();
                    1
                }
                None => {
                    // 关键健壮性修复：纹理已挂接却无提交函数，绝不能返回 0（会被
                    // 媒体循环误判为"无新帧"而永远不回退），否则纹理卡死不出帧。
                    // 返回 3 让 Dart 侧立即切回字节路径。
                    set_last_error("未注册原生纹理提交函数（缺 rdcore_texture_submit），回退字节路径");
                    3
                }
            }
        }
        Ok(None) => {
            set_last_error("媒体通道已关闭（Host 已停止抓屏）");
            0
        }
        Err(e) => {
            set_last_error(format!("渲染到纹理失败: {e}"));
            -1
        }
    }
}

/// 取最近一次收到帧的尺寸（供纹理 resize 用）。成功返回 `1` 并写入 `w`/`h`，否则 `0`。
#[no_mangle]
pub extern "C" fn rdcore_connection_last_frame_size(
    conn: *mut RdConnection,
    w: *mut u32,
    h: *mut u32,
) -> c_int {
    if conn.is_null() || w.is_null() || h.is_null() {
        return 0;
    }
    let c = unsafe { &*conn };
    if let Some((fw, fh)) = *c.last_frame_size.lock().unwrap() {
        unsafe {
            *w = fw;
            *h = fh;
        }
        clear_last_error();
        1
    } else {
        set_last_error("尚无帧尺寸缓存");
        0
    }
}

/// Viewer 拉取一帧已解密/解码的音频（Raw 16-bit 交错 PCM 或经 `real` feature 解出的 PCM），
/// 通道关闭或出错返回 NULL。音频走独立 `audio` DataChannel（id=2），与视频互不阻塞。
///
/// ⚠ 必须带超时返回：Host 不推音频流时（当前 Windows 受控端不采集音频）裸 block_on 会
/// 永久阻塞调用线程——Flutter 后台 isolate 是同一线程上跑「拉视频 + 拉音频」两个周期
/// 定时器，音频一阻塞，视频拉帧随之饿死，表现为「只显示首帧后画面冻结」。
/// 超时返回 NULL（不置错误），由上层周期重试。
#[no_mangle]
pub extern "C" fn rdcore_connection_pull_audio(conn: *mut RdConnection) -> *mut RdAudioFrame {
    if conn.is_null() {
        set_last_error("null conn");
        return ptr::null_mut();
    }
    let c = unsafe { &*conn };
    if c.is_host {
        set_last_error("only Viewer can pull audio");
        return ptr::null_mut();
    }
    // 30ms 拉取窗口：远小于 Dart 侧 50ms 轮询周期，音频流存在时基本无感；
    // 无音频流时快速让出线程，不饿死视频拉帧。
    let pulled = c.rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_millis(30), c.conn.recv_rendered_audio())
            .await
    });
    let pulled = match pulled {
        Ok(r) => r,
        Err(_) => return ptr::null_mut(), // 超时：无音频可拉（非错误，不置 last_error）
    };
    match pulled {
        Ok(Some(frame)) => {
            // 把 PCM 缓冲所有权移交给 C（调用方用 rdcore_audio_frame_free 释放）。
            let mut data = frame.data;
            let len = data.len();
            let ptr = data.as_mut_ptr();
            std::mem::forget(data);
            let codec_c = match frame.codec {
                AudioCodec::Raw => 0,
                AudioCodec::Opus => 1,
            };
            clear_last_error();
            Box::into_raw(Box::new(RdAudioFrame {
                codec: codec_c,
                channels: frame.channels,
                sample_rate: frame.sample_rate,
                data: ptr,
                len,
            }))
        }
        Ok(None) => {
            set_last_error("音频通道已关闭（Host 已停止采集）");
            ptr::null_mut()
        }
        Err(e) => {
            set_last_error(format!("拉取音频失败: {e}"));
            ptr::null_mut()
        }
    }
}

/// Viewer → Host 发送一个输入事件（鼠标/键盘/滚轮）。
#[no_mangle]
pub extern "C" fn rdcore_connection_send_input(
    conn: *mut RdConnection,
    ev: *const RdInputEvent,
) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    if ev.is_null() {
        return err_cstr("null event");
    }
    let c = unsafe { &*conn };
    let event = to_input_event(unsafe { &*ev });
    match c.rt.block_on(c.conn.send_input(&event)) {
        Ok(()) => {
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => err_cstr(format!("发送输入失败: {e}")),
    }
}

/// Viewer 经真实 WebRTC 连接（`RdConnection`）发送带字符的按键事件（`KeyWithChar`）。
///
/// 与 [`rdcore_connection_send_input`]（仅承载 C struct、`NativeRdInputEvent` 无字符串字段、
/// 无法带 `character`）不同，本函数直接构造 `InputKind::KeyWithChar`（双发 scancode +
/// character），经 `Connection` 的加密输入通道发给 Host；Host 端 `Connection::recv_input`
/// 反序列化后由 `enigo.text()` 注入文本（支持中文 / IME 合成输入）。
///
/// `character` 为 UTF-8 null 终止串；null / 空串表示纯 scancode（快捷键 / 游戏）。
/// 成功返回 NULL，失败返回错误串。
///
/// 关键修复：原先的 [`rdcore_viewer_send_input_key`] 作用于 headless 回环的 `RdSession`，
/// 而真实 WebRTC 路径的 Viewer 句柄是 `RdConnection`——二者是不同类型。过去 Dart 把
/// `RdConnection` 指针传给 `rdcore_viewer_send_input_key`，Rust 按 `RdSession` 解读，读到
/// `input_send == None` 直接返回「先调用 rdcore_session_attach_loopback_media」错误、被 Dart
/// 侧静默吞掉，表现为「键盘字符永不达 Host、但鼠标正常」（鼠标走的是本文件的
/// `rdcore_connection_send_input`，作用于 `RdConnection`）。本函数即修复该句柄类型错配。
#[no_mangle]
pub extern "C" fn rdcore_connection_send_input_key(
    conn: *mut RdConnection,
    key_code: u32,
    character: *const c_char,
    pressed: c_int,
    modifiers: u32,
) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &*conn };
    if c.is_host {
        return err_cstr("only Viewer can send input");
    }
    let character = if character.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(character) }.to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        }
    };
    let event = InputEvent {
        seq: 0,
        kind: InputKind::KeyWithChar {
            key_code,
            character,
            pressed: pressed != 0,
            modifiers: modifiers as u16,
        },
    };
    match c.rt.block_on(c.conn.send_input(&event)) {
        Ok(()) => {
            clear_last_error();
            ptr::null_mut()
        }
        Err(e) => err_cstr(format!("发送输入失败: {e}")),
    }
}

/// Host 接收 Viewer 发来的输入事件（通道关闭返回 NULL）。
#[no_mangle]
pub extern "C" fn rdcore_connection_recv_input(conn: *mut RdConnection) -> *mut RdInputEvent {
    // 用「先算 Option<ptr> 再 unwrap_or(null)」规避 early-return 的 clippy::needless_return，
    // 行为与原本一致：null conn / 非 Host / 通道关闭 / 出错 都返回 NULL 并写入 last_error。
    let out = if conn.is_null() {
        set_last_error("null conn");
        None
    } else {
        let c = unsafe { &*conn };
        if !c.is_host {
            set_last_error("only Host can recv input");
            None
        } else {
            match c.rt.block_on(c.conn.recv_input()) {
                Ok(Some(e)) => {
                    clear_last_error();
                    Some(Box::into_raw(Box::new(RdInputEvent::from_input(&e))))
                }
                Ok(None) => {
                    set_last_error("输入通道已关闭");
                    None
                }
                Err(e) => {
                    set_last_error(format!("接收输入失败: {e}"));
                    None
                }
            }
        }
    };
    out.unwrap_or(ptr::null_mut())
}

/// Host 启动后台抓取→编码→E2E 加密→媒体通道发送循环（headless 用 `NullCaptureSource`；
/// 真实抓屏由 `rdcore-desktop` 的 `real` 后端负责，不经此 FFI）。
#[no_mangle]
pub extern "C" fn rdcore_connection_start_capture(
    conn: *mut RdConnection,
    fps: c_int,
) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &mut *conn };
    if !c.is_host {
        return err_cstr("only Host can start capture");
    }
    let fps = (fps as u16).max(1);
    // `HostMediaPump::start_with` 内部用 `tokio::spawn` 起后台发送任务，必须在当前 tokio
    // runtime 上下文里调用；`start_capture` 本身同步返回，故用 `c.rt.block_on` 进入运行时
    // 上下文（与 `rdcore_connection_establish` 一致），让 `tokio::spawn` 能拿到调度器。
    let pump = c.rt.block_on(async {
        std::sync::Arc::clone(&c.conn)
            .start_capture(|| NullCaptureSource::new(1280, 720, 30, 0), fps)
    });
    c.pump = Some(pump);
    clear_last_error();
    ptr::null_mut()
}

/// Host 启动后台音频采集→编码→E2E 加密→音频通道（id=2）发送循环（headless 用 `NullAudioSource`
/// 合成静音 PCM；真实采集由 `rdcore-desktop` 的 `real` 后端负责，不经此 FFI，与
/// [`rdcore_connection_start_capture`] 对称）。
///
/// 音频参数在此一并传入（channels/sample_rate/samples_per_frame），由 FFI 就地构造
/// `NullAudioSource` 工厂交 [`rdcore_app::Connection::start_audio_capture`]。函数立即返回；
/// 循环随连接释放（`rdcore_connection_free`）或源结束而停止。需先 `establish`（拿到 E2E
/// 会话密钥）才能发送加密音频。
#[no_mangle]
pub extern "C" fn rdcore_connection_start_capture_audio(
    conn: *mut RdConnection,
    channels: u16,
    sample_rate: u32,
    samples_per_frame: u32,
    fps: c_int,
) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &mut *conn };
    if !c.is_host {
        return err_cstr("only Host can start audio capture");
    }
    let fps = (fps as u16).max(1);
    let ch = channels;
    let sr = sample_rate;
    let spf = samples_per_frame;
    let pump = std::sync::Arc::clone(&c.conn)
        .start_audio_capture(move || NullAudioSource::new(ch, sr, spf, u32::MAX, 0), fps);
    c.audio_pump = Some(pump);
    clear_last_error();
    ptr::null_mut()
}

/// 释放连接句柄（drop 媒体泵、关闭 PeerConnection）。
#[no_mangle]
pub extern "C" fn rdcore_connection_free(conn: *mut RdConnection) {
    if conn.is_null() {
        return;
    }
    let mut c = unsafe { Box::from_raw(conn) };
    // drop 媒体泵 → 停止后台抓取循环（HostMediaPump 的 Drop 会停）。
    c.pump = None;
    // drop 音频泵 → 停止后台采集→发送循环（HostAudioPump 的 Drop 会停）。
    c.audio_pump = None;
}

/// 连接状态（JSON：`{"peer_state":"...","role":"host"|"viewer"}`）。
#[no_mangle]
pub extern "C" fn rdcore_conn_state(conn: *mut RdConnection) -> *mut c_char {
    if conn.is_null() {
        return err_cstr("null conn");
    }
    let c = unsafe { &*conn };
    let ps = c.conn.connection_state();
    let s = format!(
        "{{\"peer_state\":\"{:?}\",\"role\":\"{}\"}}",
        ps,
        if c.is_host { "host" } else { "viewer" }
    );
    to_cstr(s)
}

/// 返回不可伪造安全指示器 JSON（作用于 `RdConnection`，取已验签对端的 Ed25519 身份）。
/// 连接尚未完成验签时 `security_indicator()` 返回 `None`，此处返回 NULL（Dart 调用方已
/// 对 NULL 降级处理，不影响已建立的连接）。与 `rdcore_security_indicator` 同级，但作用于
/// 真实 WebRTC 连接句柄而非握手期 `RdSession`。
#[no_mangle]
pub extern "C" fn rdcore_connection_security_indicator(
    conn: *mut RdConnection,
) -> *mut c_char {
    if conn.is_null() {
        return null_cstr();
    }
    let c = unsafe { &*conn };
    match c.conn.security_indicator() {
        Some(ind) => match serde_json::to_string(&ind) {
            Ok(json) => {
                clear_last_error();
                to_cstr(json)
            }
            Err(e) => {
                set_last_error(e);
                ptr::null_mut()
            }
        },
        None => null_cstr(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> *mut c_char {
        CString::new(s).unwrap().into_raw()
    }

    #[test]
    fn ffi_full_host_viewer_handshake_and_e2e() {
        // 两个设备的长期身份
        let host_local = rdcore_identity_new(c("host-laptop"));
        let viewer_local = rdcore_identity_new(c("viewer-phone"));
        assert!(!host_local.is_null() && !viewer_local.is_null());

        // 带外配对：互相扫码导入对方身份（真实场景是二维码/当面核对指纹）
        let host_peer_json = rdcore_local_peer_json(host_local);
        let viewer_peer_json = rdcore_local_peer_json(viewer_local);
        assert!(!host_peer_json.is_null() && !viewer_peer_json.is_null());
        assert!(rdcore_remember_peer_json(viewer_local, host_peer_json).is_null());
        assert!(rdcore_remember_peer_json(host_local, viewer_peer_json).is_null());
        rdcore_string_free(host_peer_json);
        rdcore_string_free(viewer_peer_json);

        let sid = [1u8; 16];
        let host = rdcore_session_new(host_local, 1, sid.as_ptr(), ptr::null());
        let viewer = rdcore_session_new(viewer_local, 0, sid.as_ptr(), ptr::null());
        assert!(!host.is_null() && !viewer.is_null());

        // 1) Viewer 发 Offer → Host 收
        let offer = rdcore_make_offer(viewer);
        assert!(!offer.is_null(), "offer 应生成");
        let ob = unsafe { std::slice::from_raw_parts((*offer).data, (*offer).len) };
        assert!(
            rdcore_ingest_offer(host, ob.as_ptr(), ob.len()).is_null(),
            "host 验签 viewer offer 应通过"
        );
        rdcore_bytes_free(offer);

        // 2) Host 回 Answer → Viewer 收
        let answer = rdcore_make_answer(host);
        assert!(!answer.is_null(), "answer 应生成");
        let ab = unsafe { std::slice::from_raw_parts((*answer).data, (*answer).len) };
        assert!(
            rdcore_ingest_answer(viewer, ab.as_ptr(), ab.len()).is_null(),
            "viewer 验签 host answer 应通过"
        );
        rdcore_bytes_free(answer);

        // 3) Host 同意（View + Input）
        let st = rdcore_host_request_consent(host, ptr::null());
        assert!(!st.is_null());
        rdcore_string_free(st);
        let st = rdcore_host_decide(host, 1, SCOPE_VIEW | SCOPE_INPUT, 0);
        assert!(!st.is_null());
        rdcore_string_free(st);

        // 4) 端到端密钥交换
        let v_ex = rdcore_make_session_key_exchange(viewer);
        let h_ex = rdcore_make_session_key_exchange(host);
        assert!(!v_ex.is_null() && !h_ex.is_null());
        let vb = unsafe { std::slice::from_raw_parts((*v_ex).data, (*v_ex).len) };
        assert!(
            rdcore_ingest_session_key_exchange(host, vb.as_ptr(), vb.len()).is_null(),
            "host 应接受 viewer 的密钥交换"
        );
        let hb = unsafe { std::slice::from_raw_parts((*h_ex).data, (*h_ex).len) };
        assert!(
            rdcore_ingest_session_key_exchange(viewer, hb.as_ptr(), hb.len()).is_null(),
            "viewer 应接受 host 的密钥交换"
        );
        rdcore_bytes_free(v_ex);
        rdcore_bytes_free(h_ex);

        // 5) E2E 加密：Viewer 加密 → Host 解密
        let plain = b"hello remote desktop";
        let ct = rdcore_encrypt(viewer, plain.as_ptr(), plain.len());
        assert!(!ct.is_null(), "应成功加密");
        let ctb = unsafe { std::slice::from_raw_parts((*ct).data, (*ct).len) };
        let dec = rdcore_decrypt(host, ctb.as_ptr(), ctb.len());
        assert!(!dec.is_null(), "host 应能解密");
        let decb = unsafe { std::slice::from_raw_parts((*dec).data, (*dec).len) };
        assert_eq!(decb, plain, "解密内容应与原明文一致");
        rdcore_bytes_free(ct);
        rdcore_bytes_free(dec);

        // 6) 安全指示器 & 撤销
        let ind = rdcore_security_indicator(host, 1);
        assert!(!ind.is_null(), "应返回安全指示器");
        rdcore_string_free(ind);
        let st = rdcore_revoke(host);
        assert!(!st.is_null());
        rdcore_string_free(st);

        rdcore_session_free(host);
        rdcore_session_free(viewer);
        rdcore_identity_free(host_local);
        rdcore_identity_free(viewer_local);
    }

    #[test]
    fn ffi_rejects_unpaired_peer() {
        let a = rdcore_identity_new(c("A"));
        let b = rdcore_identity_new(c("B"));
        // A 不认识 B：B 发 Offer，A 验签应失败
        let sid = [2u8; 16];
        let sa = rdcore_session_new(a, 1, sid.as_ptr(), ptr::null());
        let sb = rdcore_session_new(b, 0, sid.as_ptr(), ptr::null());
        let offer = rdcore_make_offer(sb);
        let ob = unsafe { std::slice::from_raw_parts((*offer).data, (*offer).len) };
        let err = rdcore_ingest_offer(sa, ob.as_ptr(), ob.len());
        assert!(!err.is_null(), "未配对对端应被拒绝");
        rdcore_string_free(err);
        rdcore_bytes_free(offer);
        rdcore_session_free(sa);
        rdcore_session_free(sb);
        rdcore_identity_free(a);
        rdcore_identity_free(b);
    }

    #[test]
    fn ffi_media_and_input_loopback() {
        // 两个设备的长期身份 + 带外配对
        let host_local = rdcore_identity_new(c("host-laptop"));
        let viewer_local = rdcore_identity_new(c("viewer-phone"));
        let host_peer_json = rdcore_local_peer_json(host_local);
        let viewer_peer_json = rdcore_local_peer_json(viewer_local);
        assert!(rdcore_remember_peer_json(viewer_local, host_peer_json).is_null());
        assert!(rdcore_remember_peer_json(host_local, viewer_peer_json).is_null());
        rdcore_string_free(host_peer_json);
        rdcore_string_free(viewer_peer_json);

        let sid = [3u8; 16];
        let host = rdcore_session_new(host_local, 1, sid.as_ptr(), ptr::null());
        let viewer = rdcore_session_new(viewer_local, 0, sid.as_ptr(), ptr::null());
        assert!(!host.is_null() && !viewer.is_null());

        // 握手：Offer/Answer → 同意 → E2E 密钥交换
        let offer = rdcore_make_offer(viewer);
        let ob = unsafe { std::slice::from_raw_parts((*offer).data, (*offer).len) };
        assert!(rdcore_ingest_offer(host, ob.as_ptr(), ob.len()).is_null());
        rdcore_bytes_free(offer);
        let answer = rdcore_make_answer(host);
        let ab = unsafe { std::slice::from_raw_parts((*answer).data, (*answer).len) };
        assert!(rdcore_ingest_answer(viewer, ab.as_ptr(), ab.len()).is_null());
        rdcore_bytes_free(answer);
        let st = rdcore_host_request_consent(host, ptr::null());
        rdcore_string_free(st);
        let st = rdcore_host_decide(host, 1, SCOPE_VIEW | SCOPE_INPUT, 0);
        rdcore_string_free(st);
        let v_ex = rdcore_make_session_key_exchange(viewer);
        let h_ex = rdcore_make_session_key_exchange(host);
        let vb = unsafe { std::slice::from_raw_parts((*v_ex).data, (*v_ex).len) };
        assert!(rdcore_ingest_session_key_exchange(host, vb.as_ptr(), vb.len()).is_null());
        let hb = unsafe { std::slice::from_raw_parts((*h_ex).data, (*h_ex).len) };
        assert!(rdcore_ingest_session_key_exchange(viewer, hb.as_ptr(), hb.len()).is_null());
        rdcore_bytes_free(v_ex);
        rdcore_bytes_free(h_ex);

        // 媒体面：接回环通道 → 设抓取源 → 启动后台泵
        assert!(rdcore_session_attach_loopback_media(host, viewer).is_null());
        assert!(rdcore_host_set_capture(host, 64, 48, 10, 0xAB).is_null());
        assert!(rdcore_host_start_capture(host, 30).is_null());

        // Viewer 拉 10 帧，断言 RGBA 全为 0xAB（E2E 加密往返无损、解码/渲染一致）
        let mut got = 0u32;
        for _ in 0..10 {
            let f = rdcore_viewer_pull_frame(viewer);
            assert!(!f.is_null(), "Viewer 应拉到一帧");
            let f_ref = unsafe { &*f };
            assert_eq!(f_ref.width, 64);
            assert_eq!(f_ref.height, 48);
            assert_eq!(f_ref.len, 64 * 48 * 4);
            let rgba = unsafe { std::slice::from_raw_parts(f_ref.data, f_ref.len) };
            assert!(
                rgba.iter().all(|&b| b == 0xAB),
                "渲染像素应全部为捕获的纯色 0xAB（无损往返）"
            );
            rdcore_media_frame_free(f);
            got += 1;
        }
        assert_eq!(got, 10, "应经媒体面拉到全部 10 帧");

        // 输入面：Viewer 发 MouseButton(Left, pressed) → Host 收（E2E 加密往返）
        let ev = RdInputEvent {
            kind: 1,
            x: 0,
            y: 0,
            button: 0,
            pressed: 1,
            delta_x: 0,
            delta_y: 0,
            key_code: 0,
            modifiers: 0,
        };
        assert!(rdcore_viewer_send_input(viewer, &ev as *const RdInputEvent).is_null());
        let got_ev = rdcore_host_poll_input(host);
        assert!(!got_ev.is_null(), "Host 应收到输入事件");
        let got_ref = unsafe { &*got_ev };
        assert_eq!(got_ref.kind, 1, "应为 MouseButton");
        assert_eq!(got_ref.button, 0, "应为 Left");
        assert_eq!(got_ref.pressed, 1, "应为按下");
        rdcore_input_event_free(got_ev);

        rdcore_session_free(host);
        rdcore_session_free(viewer);
        rdcore_identity_free(host_local);
        rdcore_identity_free(viewer_local);
    }

    #[test]
    fn ffi_audio_loopback() {
        // 两个设备的长期身份 + 带外配对
        let host_local = rdcore_identity_new(c("host-audio"));
        let viewer_local = rdcore_identity_new(c("viewer-audio"));
        let host_peer_json = rdcore_local_peer_json(host_local);
        let viewer_peer_json = rdcore_local_peer_json(viewer_local);
        assert!(rdcore_remember_peer_json(viewer_local, host_peer_json).is_null());
        assert!(rdcore_remember_peer_json(host_local, viewer_peer_json).is_null());
        rdcore_string_free(host_peer_json);
        rdcore_string_free(viewer_peer_json);

        let sid = [5u8; 16];
        let host = rdcore_session_new(host_local, 1, sid.as_ptr(), ptr::null());
        let viewer = rdcore_session_new(viewer_local, 0, sid.as_ptr(), ptr::null());
        assert!(!host.is_null() && !viewer.is_null());

        // 握手：Offer/Answer → 同意 → E2E 密钥交换
        let offer = rdcore_make_offer(viewer);
        let ob = unsafe { std::slice::from_raw_parts((*offer).data, (*offer).len) };
        assert!(rdcore_ingest_offer(host, ob.as_ptr(), ob.len()).is_null());
        rdcore_bytes_free(offer);
        let answer = rdcore_make_answer(host);
        let ab = unsafe { std::slice::from_raw_parts((*answer).data, (*answer).len) };
        assert!(rdcore_ingest_answer(viewer, ab.as_ptr(), ab.len()).is_null());
        rdcore_bytes_free(answer);
        let st = rdcore_host_request_consent(host, ptr::null());
        rdcore_string_free(st);
        let st = rdcore_host_decide(host, 1, SCOPE_VIEW | SCOPE_INPUT, 0);
        rdcore_string_free(st);
        let v_ex = rdcore_make_session_key_exchange(viewer);
        let h_ex = rdcore_make_session_key_exchange(host);
        let vb = unsafe { std::slice::from_raw_parts((*v_ex).data, (*v_ex).len) };
        assert!(rdcore_ingest_session_key_exchange(host, vb.as_ptr(), vb.len()).is_null());
        let hb = unsafe { std::slice::from_raw_parts((*h_ex).data, (*h_ex).len) };
        assert!(rdcore_ingest_session_key_exchange(viewer, hb.as_ptr(), hb.len()).is_null());
        rdcore_bytes_free(v_ex);
        rdcore_bytes_free(h_ex);

        // 音频面：接回环通道 → 设抓取源（2 声道 48k，每帧 960 采样，10 帧，填充 0xAB）→ 启动后台泵
        assert!(rdcore_session_attach_loopback_audio(host, viewer).is_null());
        assert!(rdcore_host_set_capture_audio(host, 2, 48_000, 960, 10, 0xAB).is_null());
        assert!(rdcore_host_start_capture_audio(host, 30).is_null());

        // Viewer 拉 10 帧，断言 PCM 全为 0xAB（E2E 加密往返无损、Raw 解码一致）
        let mut got = 0u32;
        for _ in 0..10 {
            let f = rdcore_viewer_pull_audio(viewer);
            assert!(!f.is_null(), "Viewer 应拉到一帧音频");
            let f_ref = unsafe { &*f };
            assert_eq!(f_ref.codec, 0, "默认 Raw 直通，codec 应为 0");
            assert_eq!(f_ref.channels, 2);
            assert_eq!(f_ref.sample_rate, 48_000);
            assert_eq!(
                f_ref.len,
                960 * 2 * 2,
                "每帧应为 960 采样 × 2 声道 × 2 字节"
            );
            let pcm = unsafe { std::slice::from_raw_parts(f_ref.data, f_ref.len) };
            assert!(
                pcm.iter().all(|&b| b == 0xAB),
                "音频 PCM 应全部为捕获的固定字节 0xAB（无损往返）"
            );
            rdcore_audio_frame_free(f);
            got += 1;
        }
        assert_eq!(got, 10, "应经音频面拉到全部 10 帧");

        rdcore_session_free(host);
        rdcore_session_free(viewer);
        rdcore_identity_free(host_local);
        rdcore_identity_free(viewer_local);
    }

    /// B6 辅助：建一对已配对、已握手、已建立 E2E 会话密钥的 host/viewer 会话。
    fn paired_e2e_sessions(
        sid: [u8; 16],
    ) -> (*mut RdLocal, *mut RdLocal, *mut RdSession, *mut RdSession) {
        let host_local = rdcore_identity_new(c("host"));
        let viewer_local = rdcore_identity_new(c("viewer"));
        let hp = rdcore_local_peer_json(host_local);
        let vp = rdcore_local_peer_json(viewer_local);
        assert!(rdcore_remember_peer_json(viewer_local, hp).is_null());
        assert!(rdcore_remember_peer_json(host_local, vp).is_null());
        rdcore_string_free(hp);
        rdcore_string_free(vp);
        let host = rdcore_session_new(host_local, 1, sid.as_ptr(), ptr::null());
        let viewer = rdcore_session_new(viewer_local, 0, sid.as_ptr(), ptr::null());
        // Offer/Answer
        let offer = rdcore_make_offer(viewer);
        let ob = unsafe { std::slice::from_raw_parts((*offer).data, (*offer).len) };
        assert!(rdcore_ingest_offer(host, ob.as_ptr(), ob.len()).is_null());
        rdcore_bytes_free(offer);
        let answer = rdcore_make_answer(host);
        let ab = unsafe { std::slice::from_raw_parts((*answer).data, (*answer).len) };
        assert!(rdcore_ingest_answer(viewer, ab.as_ptr(), ab.len()).is_null());
        rdcore_bytes_free(answer);
        // consent
        rdcore_string_free(rdcore_host_request_consent(host, ptr::null()));
        rdcore_string_free(rdcore_host_decide(
            host,
            1,
            SCOPE_VIEW | SCOPE_INPUT | SCOPE_FILE | SCOPE_CLIPBOARD,
            0,
        ));
        // E2E key exchange
        let v_ex = rdcore_make_session_key_exchange(viewer);
        let h_ex = rdcore_make_session_key_exchange(host);
        let vb = unsafe { std::slice::from_raw_parts((*v_ex).data, (*v_ex).len) };
        assert!(rdcore_ingest_session_key_exchange(host, vb.as_ptr(), vb.len()).is_null());
        let hb = unsafe { std::slice::from_raw_parts((*h_ex).data, (*h_ex).len) };
        assert!(rdcore_ingest_session_key_exchange(viewer, hb.as_ptr(), hb.len()).is_null());
        rdcore_bytes_free(v_ex);
        rdcore_bytes_free(h_ex);
        (host_local, viewer_local, host, viewer)
    }

    #[test]
    fn ffi_file_transfer_roundtrip() {
        let (hl, vl, host, viewer) = paired_e2e_sessions([7u8; 16]);
        let tid = 42u64;
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();

        // 1) Viewer 发 Offer → Host 收并建会话。
        let offer = rdcore_file_send_offer(viewer, tid, c("a.bin"), payload.len() as u64);
        assert!(!offer.is_null());
        let ob = unsafe { std::slice::from_raw_parts((*offer).data, (*offer).len) };
        let echoed = rdcore_file_host_on_offer(host, ob.as_ptr(), ob.len());
        assert!(!echoed.is_null(), "Host 应接受 Offer 并回显 transfer_id");
        let eb = unsafe { std::slice::from_raw_parts((*echoed).data, (*echoed).len) };
        assert_eq!(u64::from_le_bytes(eb.try_into().unwrap()), tid);
        rdcore_bytes_free(offer);
        rdcore_bytes_free(echoed);

        // 2) 未同意前发 Chunk 应被拒（NotAccepted）。
        let chunk0 = rdcore_file_send_chunk(viewer, tid, 0, payload[..100].as_ptr(), 100);
        let cb = unsafe { std::slice::from_raw_parts((*chunk0).data, (*chunk0).len) };
        let premature = rdcore_file_host_on_event(host, cb.as_ptr(), cb.len());
        assert!(premature.is_null(), "未同意前收 Chunk 应失败");
        assert!(!rdcore_last_error().is_null());
        rdcore_string_free(rdcore_last_error());
        rdcore_bytes_free(chunk0);

        // 3) Host 同意 → Viewer 收到 Accept。
        let accept = rdcore_file_host_decide(host, tid, 1, ptr::null());
        assert!(!accept.is_null());
        let ab = unsafe { std::slice::from_raw_parts((*accept).data, (*accept).len) };
        let decision = rdcore_file_viewer_on_decision(viewer, ab.as_ptr(), ab.len());
        assert!(!decision.is_null());
        let db = unsafe { std::slice::from_raw_parts((*decision).data, (*decision).len) };
        assert_eq!(db, &[1u8], "应为 Accept");
        rdcore_bytes_free(accept);
        rdcore_bytes_free(decision);

        // 4) 逐片发送 + Done → Host 重组出完整字节。
        let chunk_size = rdcore_proto::MAX_FILE_CHUNK_SIZE;
        let n_chunks = payload.len().div_ceil(chunk_size);
        for (i, piece) in payload.chunks(chunk_size).enumerate() {
            let c = rdcore_file_send_chunk(viewer, tid, i as u64, piece.as_ptr(), piece.len());
            let cb = unsafe { std::slice::from_raw_parts((*c).data, (*c).len) };
            let mid = rdcore_file_host_on_event(host, cb.as_ptr(), cb.len());
            assert!(mid.is_null(), "中间分片应返回 NULL（未完成）");
            assert!(rdcore_last_error().is_null(), "中间分片不应报错");
            rdcore_bytes_free(c);
        }
        let done = rdcore_file_send_done(viewer, tid, n_chunks as u64);
        let db2 = unsafe { std::slice::from_raw_parts((*done).data, (*done).len) };
        let complete = rdcore_file_host_on_event(host, db2.as_ptr(), db2.len());
        assert!(!complete.is_null(), "Done 后应返回完整文件");
        let comp = unsafe { std::slice::from_raw_parts((*complete).data, (*complete).len) };
        assert_eq!(comp, payload.as_slice(), "重组字节应与原文件一致");
        rdcore_bytes_free(done);
        rdcore_bytes_free(complete);

        rdcore_session_free(host);
        rdcore_session_free(viewer);
        rdcore_identity_free(hl);
        rdcore_identity_free(vl);
    }

    #[test]
    fn ffi_clipboard_roundtrip() {
        let (hl, vl, host, viewer) = paired_e2e_sessions([8u8; 16]);
        // Viewer 发剪贴板 Data → Host 收。
        let text = b"hello clipboard";
        let sent = rdcore_clipboard_send(viewer, 1, 1, text.as_ptr(), text.len());
        assert!(!sent.is_null());
        let sb = unsafe { std::slice::from_raw_parts((*sent).data, (*sent).len) };
        let recv = rdcore_clipboard_recv(host, sb.as_ptr(), sb.len());
        assert!(!recv.is_null());
        let rb = unsafe { std::slice::from_raw_parts((*recv).data, (*recv).len) };
        assert_eq!(rb[0], 1u8, "应为 Data action");
        assert_eq!(&rb[1..], text, "剪贴板内容应一致");
        rdcore_bytes_free(sent);
        rdcore_bytes_free(recv);

        rdcore_session_free(host);
        rdcore_session_free(viewer);
        rdcore_identity_free(hl);
        rdcore_identity_free(vl);
    }

    #[test]
    fn ffi_pairing_info_format() {
        let info = rdcore_create_pairing();
        assert!(!info.is_null());
        let i = unsafe { &*info };
        // token 应为 64 字符小写 hex。
        let tok = unsafe { CStr::from_ptr(i.token) }.to_str().unwrap();
        assert_eq!(tok.len(), 64, "token 应为 64 字符 hex");
        assert!(tok
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // session_id 转 hex 应为 32 字符。
        let hex = rdcore_session_id_to_hex(i.session_id.as_ptr());
        let s = unsafe { CStr::from_ptr(hex) }.to_str().unwrap();
        assert_eq!(s.len(), 32, "session_id hex 应为 32 字符");
        rdcore_string_free(hex);
        // 两次生成应不同（CSPRNG）。
        let info2 = rdcore_create_pairing();
        let i2 = unsafe { &*info2 };
        assert_ne!(i.session_id, i2.session_id);
        rdcore_pairing_info_free(info);
        rdcore_pairing_info_free(info2);
    }

    #[test]
    fn ffi_pairing_publish_and_revoke() {
        let dir =
            std::env::temp_dir().join(format!("rdcore_ffi_pairing_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token_db.txt");
        std::env::set_var("SIGNALING_TOKEN_DB", &path);

        // 发布：文件出现 `session_hex\ttoken` 一行。
        let sid = [0x5au8; 16];
        let token = CString::new("deadbeef".repeat(8)).unwrap();
        assert_eq!(rdcore_pairing_publish(sid.as_ptr(), token.as_ptr()), 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, format!("{}\t{}\n", hex::encode(sid), "deadbeef".repeat(8)));

        // 重复发布 = 刷新：文件被覆写为新配对。
        let sid2 = [0x6bu8; 16];
        assert_eq!(rdcore_pairing_publish(sid2.as_ptr(), token.as_ptr()), 1);
        let content2 = std::fs::read_to_string(&path).unwrap();
        assert!(content2.starts_with(&hex::encode(sid2)), "刷新应覆写文件");

        // 撤销：文件删除且幂等。
        rdcore_pairing_revoke();
        assert!(!path.exists(), "撤销后 token 库文件应被删除");
        rdcore_pairing_revoke();

        std::env::remove_var("SIGNALING_TOKEN_DB");
        std::fs::remove_dir_all(&dir).ok();
    }
}
