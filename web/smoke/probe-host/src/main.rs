//! M3 smoke 探针 Host：剪贴板同步 / 文件传输 / 音频播放三项业务的**协议对端**。
//!
//! # 为什么需要它（Host 支持度结论）
//! `rdcore-desktop` agent 只接了媒体泵 + 输入注入循环，**没有** clipboard / file / audio
//! 业务循环；而 `rdcore-app::Connection` 的公开 API 只有 `send_app/recv_app`（`AppMessage`
//! 共 5 个变体：**无 FileTransfer**）与媒体/音频帧收发，不暴露裸控制通道。仓库内唯一的
//! 文件传输线格式在 rdcore-ffi Track B（headless `RdSession` 回环 seam）：
//! 内层明文 = `postcard(rdcore_proto::Message::FileTransfer(ev))`，AEAD 后由
//! `Message::Encrypted` 承载。因此探针直接建在公开库 API（rdcore-rtc / rdcore-session /
//! rdcore-signaling / rdcore-media / rdcore-crypto / rdcore-audio）之上，复刻
//! `rdcore-app::Connection::establish` 的 Host 分支编排，获得裸通道出入口。**不改
//! core/shared/cloud 任何现有文件。**
//!
//! # 探针行为（全部确定性、可被 smoke 断言）
//! - 剪贴板（`AppMessage::Clipboard` 格式，与 rdcore-app 真实控制通道一致）：
//!   收 `Request` → 回 `Data(固定已知文本)`；收 `Data` → 写 `<run-dir>/clipboard_received.txt`。
//! - 文件传输（ffi Track B 格式：`postcard(Message::FileTransfer)` 内层）：
//!   收 `Offer` → 立即 `Accept`，分片重组落盘 `<run-dir>/ft_recv_<name>`；
//!   建连后主动向 Viewer `Offer` 一个已知内容文件（`<run-dir>/ft_offer.bin`），
//!   收到 `Accept` 后按 ≤1MiB 分片发完 + `Done`（覆盖 Host→浏览器方向）。
//! - 音频：`SyntheticAudioSource`（48000Hz / 2ch / 440Hz 正弦 / 20ms 帧）经
//!   `audio` DataChannel（id=2）泵出，线格式与 `Connection::send_audio` 一致
//!   （仅 `data` 字节 AEAD，替换为 postcard(Ciphertext)）。

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rdcore_audio::{AudioSource, SyntheticAudioSource};
use rdcore_consent::{ConsentDecision, ConsentScope};
use rdcore_crypto::{
    aead_open, aead_seal, ephemeral_x25519_keypair, x25519_public_bytes, Ed25519CryptoProvider,
    SecretKey, SessionKey,
};
use rdcore_identity::{
    DeviceId, IdentityStore, PassphraseKeyProvider, PersistentIdentityStore,
};
use rdcore_media::{AudioChannel, DataChannel};
use rdcore_proto::{
    Capabilities, ClipboardAction, ClipboardEvent, ConnectionAnswer, FileTransferAction,
    FileTransferEvent, Heartbeat, IceCandidate, InputCaps, InputEvent, Message, SessionId,
    VideoCodec, MAX_FILE_CHUNK_SIZE,
};
use rdcore_rtc::{RtcConfig, WebRtcPeer};
use rdcore_session::{
    establish_session_key, sign_answer, sign_ephemeral_key, verify_offer,
};
use rdcore_signaling::SignalingClient;
use serde::{Deserialize, Serialize};

// ─────────────────────────── AppMessage 镜像（线格式锁定） ───────────────────────────
//
// 与 `rdcore-app::AppMessage` **逐字节同构**（postcard 按变体下标编码，变体顺序严禁重排；
// rdcore-web 的 pipeline.rs 也以同样方式镜像）。rdcore-app 依赖 WebRTC 抓屏等重依赖，
// 探针只需其枚举布局，故在此镜像并注明单一事实来源。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum AppMessage {
    /// 心跳存活探针。
    Heartbeat(Heartbeat),
    /// 远程输入事件（鼠标 / 键盘 / 滚轮）。
    Input(InputEvent),
    /// 剪贴板同步事件。
    Clipboard(ClipboardEvent),
    /// Host 给 Viewer 的授权决定。
    Consent(ConsentDecision),
    /// Host 撤销连接。
    Revoke,
    /// Viewer 请求 Host 下一帧输出关键帧（IDR）——P 帧流丢帧/花屏/积压恢复。
    /// 与 rdcore-app 线格式一致：只能追加在枚举末尾，严禁重排。
    RequestKeyframe,
}

/// 剪贴板 `Request` 的固定应答文本（smoke 断言的已知内容；含 CJK 验证 UTF-8 透明传输）。
const KNOWN_CLIPBOARD: &str = "rdcore-probe 剪贴板应答 v1 — 固定文本 clipboard probe reply";
/// Host→Viewer 方向主动 Offer 的文件名与大小（1.5 MiB，强制 2 个分片）。
const OFFER_NAME: &str = "probe-offer.bin";
const OFFER_SIZE: usize = 1536 * 1024;
/// Host→Viewer 传输的 transfer_id（与 Viewer 侧自建 id 命名空间错开）。
const OFFER_TRANSFER_ID: u64 = 0x5EED_0002;
/// 音频参数：48kHz 立体声、20ms 帧（960 采样/声道/帧）、440Hz 正弦、振幅 0.5。
const AUDIO_CHANNELS: u16 = 2;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_SAMPLES_PER_FRAME: u32 = 960;
const AUDIO_FREQ_HZ: f32 = 440.0;
const AUDIO_AMPLITUDE: f32 = 0.5;

/// 命令行参数（手工解析，零额外依赖）。
struct Args {
    signal: String,
    identity_dir: PathBuf,
    identity_pass: String,
    run_dir: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut signal = "ws://127.0.0.1:18081".to_string();
    let mut identity_dir = PathBuf::from("probe-identity");
    let mut identity_pass = "probe-test".to_string();
    let mut run_dir = PathBuf::from(".");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--signal" => signal = it.next().context("--signal 缺参数")?,
            "--identity-dir" => identity_dir = PathBuf::from(it.next().context("--identity-dir 缺参数")?),
            "--identity-pass" => identity_pass = it.next().context("--identity-pass 缺参数")?,
            "--run-dir" => run_dir = PathBuf::from(it.next().context("--run-dir 缺参数")?),
            // 与 rdcore-desktop 调用习惯兼容的哑开关（探针语义上恒为 loopback/headless）。
            "--loopback" | "--headless" | "--no-banner" => {}
            "--fps" => {
                let _ = it.next();
            }
            other => bail!("未知参数: {other}"),
        }
    }
    Ok(Args {
        signal,
        identity_dir,
        identity_pass,
        run_dir,
    })
}

/// 生成配对邀请（镜像 `rdcore-app::Connection::create_pairing`：16B session + 32B token hex）。
fn create_pairing() -> (SessionId, String) {
    let mut sid = [0u8; 16];
    getrandom::getrandom(&mut sid).expect("系统随机数不可用");
    let mut tok = [0u8; 32];
    getrandom::getrandom(&mut tok).expect("系统随机数不可用");
    (SessionId(sid), hex::encode(tok))
}

/// 把配对写进共享 token 库文件（与 `rdcore-desktop::token_db` 严格同格式：
/// `session_hex\ttoken_hex\n`，临时文件写全 + rename 原子替换）。
fn register_token_file(session: &SessionId, token: &str) -> std::io::Result<()> {
    let path = std::env::var("SIGNALING_TOKEN_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("signaling_token_db.txt"));
    let line = format!("{}\t{}\n", hex::encode(session.0), token);
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    let result = (|| {
        std::fs::write(&tmp, line.as_bytes())?;
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 本端能力（纳入 Answer 签名；镜像 `rdcore-app::Connection::capabilities`）。
fn capabilities() -> Capabilities {
    Capabilities {
        video_codecs: vec![VideoCodec::H264, VideoCodec::Raw],
        max_width: 1920,
        max_height: 1080,
        fps: 30,
        clipboard: true,
        input: InputCaps {
            mouse: true,
            keyboard: true,
            wheel: true,
        },
    }
}

/// 经 E2E 密钥加密后发送一条 `AppMessage`（镜像 `rdcore-app::Connection::send_app`）。
async fn send_app<D: DataChannel>(
    dc: &D,
    key: &SessionKey,
    msg: &AppMessage,
) -> Result<()> {
    let plain = postcard::to_stdvec(msg).map_err(|e| anyhow!("AppMessage 编码失败: {e}"))?;
    let ct = aead_seal(key, &plain);
    dc.send(&Message::Encrypted(ct))
        .await
        .map_err(|e| anyhow!("控制通道发送失败: {e}"))?;
    Ok(())
}

/// 经 E2E 密钥加密后发送一条文件传输事件（镜像 rdcore-ffi `seal_file_event`：
/// 内层明文 = `postcard(Message::FileTransfer(ev))`，AEAD 后以 `Message::Encrypted` 承载）。
async fn send_file_event<D: DataChannel>(
    dc: &D,
    key: &SessionKey,
    ev: &FileTransferEvent,
) -> Result<()> {
    let plain =
        rdcore_proto::encode(&Message::FileTransfer(ev.clone())).map_err(|e| anyhow!("{e}"))?;
    let ct = aead_seal(key, &plain);
    dc.send(&Message::Encrypted(ct))
        .await
        .map_err(|e| anyhow!("控制通道发送失败: {e}"))?;
    Ok(())
}

/// 把对端经信令发来的 ICE 候选加入连接（JSON 整段还原，镜像 rdcore-app `add_remote_ice`）。
async fn add_remote_ice(peer: &Arc<WebRtcPeer>, i: &IceCandidate) {
    if let Ok(init) = serde_json::from_str(&i.candidate) {
        let _ = peer.add_ice_candidate(init).await;
    }
}

/// ICE 中继循环（镜像 rdcore-app `relay_ice`）：drain 本地候选经信令发出，收对端候选加入。
async fn relay_ice(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    from: DeviceId,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        for c in peer.drain_ice_candidates().await {
            let msg = Message::Ice(IceCandidate {
                session_id: session,
                from,
                candidate: serde_json::to_string(&c).expect("序列化 ICE 候选"),
                sdp_mid: None,
                sdp_mline_index: None,
            });
            let _ = sig.send(&msg).await;
        }
        if let Ok(Ok(Some(Message::Ice(i)))) =
            tokio::time::timeout(Duration::from_millis(50), sig.recv()).await
        {
            add_remote_ice(&peer, &i).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Viewer→Host 方向单次传输的接收状态（语义镜像 `rdcore-app::file_transfer::TransferSession`：
/// Offer 建立 → Accept 后才收 Chunk → Done 校验无缺口 + 总尺寸）。
struct RecvSession {
    name: String,
    expected_size: u64,
    chunks: BTreeMap<u64, Vec<u8>>,
    next_seq: u64,
}

impl RecvSession {
    fn new(name: &str, size: u64) -> Self {
        Self {
            name: name.to_string(),
            expected_size: size,
            chunks: BTreeMap::new(),
            next_seq: 0,
        }
    }

    fn on_chunk(&mut self, seq: u64, data: Vec<u8>) {
        self.chunks.insert(seq, data);
        while self.chunks.contains_key(&self.next_seq) {
            self.next_seq += 1;
        }
    }

    /// Done 收尾：收齐 0..chunks 且无缺口、总尺寸吻合 → 返回（文件名, 完整字节）。
    fn finish(self, chunks: u64) -> Result<(String, Vec<u8>)> {
        if self.next_seq != chunks {
            bail!("分片缺口：连续序号 {} != 声明总数 {}", self.next_seq, chunks);
        }
        let mut out = Vec::new();
        for i in 0..chunks {
            match self.chunks.get(&i) {
                Some(part) => out.extend_from_slice(part),
                None => bail!("分片缺口：缺第 {i} 片"),
            }
        }
        if out.len() as u64 != self.expected_size {
            bail!("尺寸不符：重组 {} != Offer 声明 {}", out.len(), self.expected_size);
        }
        Ok((self.name, out))
    }
}

/// 文件名消毒：只留末尾一段、白名单字符，防目录穿越。
fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file.bin");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "file.bin".to_string()
    } else {
        cleaned
    }
}

/// 生成 Host→Viewer 方向 Offer 用的确定性已知内容（头部魔数 + 伪随机体）。
fn offer_payload() -> Vec<u8> {
    let mut v = Vec::with_capacity(OFFER_SIZE);
    let header = b"rdcore-probe-offer-v1\n";
    v.extend_from_slice(header);
    while v.len() < OFFER_SIZE {
        let i = v.len() as u32;
        v.push((i.wrapping_mul(31).wrapping_add(7) % 251) as u8);
    }
    v
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.run_dir).context("创建 run-dir 失败")?;

    // ── 1) 持久化身份（与 rdcore-desktop 同一套落盘格式；TOFU 对端表随目录存续）──
    let provider = Ed25519CryptoProvider;
    let (store, secret): (PersistentIdentityStore, SecretKey) =
        PersistentIdentityStore::load_or_create(
            &args.identity_dir,
            &provider,
            "rdcore-probe-host",
            &PassphraseKeyProvider::new(args.identity_pass.clone()),
        )
        .map_err(|e| anyhow!("加载/创建探针身份失败: {e}"))?;
    let store = Arc::new(Mutex::new(store));
    let local_id: DeviceId = store.lock().unwrap().local_identity().id;

    // ── 2) 配对 + token 库注册 + 配对码打印（smoke 抓取 `[0-9a-f]{32}:[0-9a-f]{64}`）──
    let (session, token) = create_pairing();
    register_token_file(&session, &token).context("写入信令 token 库失败（SIGNALING_TOKEN_DB）")?;
    println!("配对码 : {}:{}", hex::encode(session.0), token);
    // token 库心跳（30s 重写保鲜，镜像 agent 语义；smoke 短跑其实用不上，保持协议一致）。
    {
        let hb_stop = Arc::new(AtomicBool::new(false));
        let hb_stop2 = hb_stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if hb_stop2.load(Ordering::SeqCst) {
                    break;
                }
                let _ = register_token_file(&session, &token);
            }
        });
    }

    // ── 3) 信令 + PeerConnection（loopback：空 ICE 服务器 + 回环候选）──
    let sig_url = format!(
        "{}/{}",
        args.signal.trim_end_matches('/'),
        hex::encode(session.0)
    );
    let sig = Arc::new(
        SignalingClient::connect(&sig_url)
            .await
            .map_err(|e| anyhow!("信令连接失败 {sig_url}: {e}"))?,
    );
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        include_loopback: true,
        ..Default::default()
    };
    let peer = Arc::new(
        WebRtcPeer::with_config(rtc_cfg)
            .await
            .map_err(|e| anyhow!("创建 PeerConnection 失败: {e}"))?,
    );

    // ── 4) Host 分支握手（镜像 establish：PeerHello → 收/验 Offer → 签名 Answer）──
    let hello = Message::PeerHello(store.lock().unwrap().local_identity().clone());
    sig.send(&hello)
        .await
        .map_err(|e| anyhow!("发送 PeerHello 失败: {e}"))?;
    println!("● 等待 Viewer 连接…");
    let offer = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match sig.recv().await.map_err(|e| anyhow!("信令接收失败: {e}"))? {
                Some(Message::Offer(o)) => break Ok(o),
                Some(Message::Ice(i)) => add_remote_ice(&peer, &i).await,
                Some(Message::PeerHello(p)) => {
                    // TOFU：仅首次见到该 DeviceId 时记住（防会话内公钥替换）。
                    let mut g = store.lock().unwrap();
                    if g.lookup(&p.id).is_none() {
                        g.remember(p);
                    }
                }
                Some(_) => continue,
                None => return Err(anyhow!("信令通道关闭，未收到 Offer")),
            }
        }
    })
    .await
    .context("等待 Viewer Offer 超时")??;
    {
        let g = store.lock().unwrap();
        verify_offer(&provider, &*g, &offer).map_err(|e| anyhow!("Offer 验签失败: {e}"))?;
    }
    let sdp = peer
        .accept_offer(offer.sdp)
        .await
        .map_err(|e| anyhow!("接受 Offer 失败: {e}"))?;
    // 重发一次身份（首条 PeerHello 可能发在 Viewer 进房之前；镜像 establish）。
    sig.send(&hello)
        .await
        .map_err(|e| anyhow!("重发 PeerHello 失败: {e}"))?;
    let answer = sign_answer(
        &provider,
        &secret,
        ConnectionAnswer {
            session_id: session,
            from: local_id,
            sdp,
            capabilities: capabilities(),
            frame: None,
            signature: None,
        },
    );
    sig.send(&Message::Answer(answer))
        .await
        .map_err(|e| anyhow!("发送 Answer 失败: {e}"))?;

    // ── 5) ICE 中继（后台）+ 等数据通道 open ──
    let ice_stop = Arc::new(AtomicBool::new(false));
    tokio::spawn(relay_ice(
        peer.clone(),
        sig.clone(),
        session,
        local_id,
        ice_stop.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(30), peer.wait_data_channels_open())
        .await
        .context("等待数据通道 open 超时")?;
    println!("● 数据通道已 open，交换 E2E 会话密钥…");

    // ── 6) X25519 会话密钥交换（控制通道明文，镜像 exchange_session_key）──
    let (_media, dc) = peer.channels();
    let (pub_k, sec_k) = ephemeral_x25519_keypair();
    let ex = sign_ephemeral_key(
        &provider,
        &secret,
        session,
        local_id,
        x25519_public_bytes(&pub_k),
    );
    dc.send(&Message::SessionKey(ex))
        .await
        .map_err(|e| anyhow!("发送会话密钥交换失败: {e}"))?;
    let their = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match dc.recv().await.map_err(|e| anyhow!("控制通道接收失败: {e}"))? {
                Some(Message::SessionKey(e)) => break Ok(e),
                Some(_) => continue,
                None => return Err(anyhow!("控制通道关闭，未收到对端会话密钥")),
            }
        }
    })
    .await
    .context("等待对端会话密钥超时")??;
    let key = {
        let g = store.lock().unwrap();
        establish_session_key(&provider, &*g, &sec_k, &their, session)
            .map_err(|e| anyhow!("会话密钥派生失败: {e}"))?
    };

    // ── 7) 下发授权决定（全量 scopes：View/Input/Clipboard/FileTransfer）──
    let decision = ConsentDecision::Grant {
        scopes: HashSet::from([
            ConsentScope::View,
            ConsentScope::Input,
            ConsentScope::Clipboard,
            ConsentScope::FileTransfer,
        ]),
        duration: None,
    };
    send_app(&dc, &key, &AppMessage::Consent(decision)).await?;
    println!("✓ 探针连接已建立：E2E 密钥就绪，已下发全量授权（View/Input/Clipboard/FileTransfer）");

    // ── 8) 音频泵（SyntheticAudioSource → AEAD → audio 通道；镜像 send_audio 线格式）──
    {
        let audio = peer.audio_channel();
        let key = key.clone();
        tokio::spawn(async move {
            let mut src = SyntheticAudioSource::new(
                AUDIO_CHANNELS,
                AUDIO_SAMPLE_RATE,
                AUDIO_SAMPLES_PER_FRAME,
                AUDIO_FREQ_HZ,
                AUDIO_AMPLITUDE,
                u32::MAX,
            );
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(frame) = src.next_frame() else { break };
                let ct = aead_seal(&key, &frame.data);
                let sealed_data = match postcard::to_stdvec(&ct) {
                    Ok(d) => d,
                    Err(_) => break,
                };
                let mut sealed = frame;
                sealed.data = sealed_data;
                if audio.send_frame(&sealed).await.is_err() {
                    break;
                }
            }
        });
        println!(
            "♪ audio: 合成音频泵已启动（{}Hz {}ch {}Hz 正弦，20ms/帧）",
            AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, AUDIO_FREQ_HZ as u32
        );
    }

    // ── 9) Host→Viewer 文件传输：先落盘已知内容，再发 Offer 等 Accept ──
    let offer_bytes = offer_payload();
    std::fs::write(args.run_dir.join("ft_offer.bin"), &offer_bytes).context("写 ft_offer.bin 失败")?;
    send_file_event(
        &dc,
        &key,
        &FileTransferEvent {
            transfer_id: OFFER_TRANSFER_ID,
            action: FileTransferAction::Offer {
                name: OFFER_NAME.to_string(),
                size: offer_bytes.len() as u64,
            },
        },
    )
    .await?;
    println!(
        "⇄ file: 已发 Offer {} ({}B)，等待 Viewer Accept…",
        OFFER_NAME,
        offer_bytes.len()
    );

    // ── 10) 业务主循环：剪贴板 / 文件传输（心跳与输入忽略）──
    let mut recv_session: Option<(u64, RecvSession)> = None;
    let mut pending_send: Option<Vec<u8>> = Some(offer_bytes);
    loop {
        let msg = match dc.recv().await {
            Ok(Some(m)) => m,
            Ok(None) => {
                println!("● 控制通道已关闭，探针退出");
                break;
            }
            Err(e) => {
                eprintln!("⚠ 控制通道接收错误: {e}");
                break;
            }
        };
        let Message::Encrypted(ct) = msg else {
            continue; // 握手期遗留的非加密消息：忽略
        };
        let Some(plain) = aead_open(&key, &ct) else {
            eprintln!("⚠ 控制消息解密失败（篡改/密钥不匹配），忽略");
            continue;
        };
        // 先试 AppMessage（剪贴板/心跳/输入走这条），再试 proto Message（FileTransfer 走这条）。
        // 顺序安全：FileTransfer 的 proto 变体下标 8 超出 AppMessage 的 0..=4，必解析失败。
        if let Ok(app) = postcard::from_bytes::<AppMessage>(&plain) {
            match app {
                AppMessage::Clipboard(ev) => match ev.action {
                    ClipboardAction::Request => {
                        send_app(
                            &dc,
                            &key,
                            &AppMessage::Clipboard(ClipboardEvent {
                                seq: ev.seq,
                                action: ClipboardAction::Data(
                                    KNOWN_CLIPBOARD.as_bytes().to_vec(),
                                ),
                            }),
                        )
                        .await?;
                        println!("⇄ clipboard: 收到 Request（seq={}），已回固定文本", ev.seq);
                    }
                    ClipboardAction::Data(bytes) => {
                        let path = args.run_dir.join("clipboard_received.txt");
                        std::fs::write(&path, &bytes).context("写 clipboard_received.txt 失败")?;
                        println!(
                            "⇄ clipboard: 收到 Data {} 字节 → clipboard_received.txt",
                            bytes.len()
                        );
                    }
                    ClipboardAction::Clear => {
                        println!("⇄ clipboard: 收到 Clear（忽略，探针无镜像副本）");
                    }
                },
                AppMessage::Heartbeat(_) | AppMessage::Input(_) => {}
                AppMessage::Consent(_) | AppMessage::Revoke => {}
                // 探针 Host 逐帧自带 SPS/PPS 且按自身节奏发帧，收到关键帧请求仅记录。
                AppMessage::RequestKeyframe => {
                    println!("⇄ control: 收到 RequestKeyframe（探针忽略，仅记录）");
                }
            }
            continue;
        }
        match rdcore_proto::decode(&plain) {
            Ok(Message::FileTransfer(ev)) => match ev.action {
                FileTransferAction::Offer { name, size } => {
                    recv_session = Some((ev.transfer_id, RecvSession::new(&name, size)));
                    send_file_event(
                        &dc,
                        &key,
                        &FileTransferEvent {
                            transfer_id: ev.transfer_id,
                            action: FileTransferAction::Accept,
                        },
                    )
                    .await?;
                    println!("⇄ file: 收到 Offer {name} ({size}B)，已自动 Accept");
                }
                FileTransferAction::Chunk { seq, data } => {
                    match recv_session.as_mut() {
                        Some((id, s)) if *id == ev.transfer_id => s.on_chunk(seq, data),
                        _ => eprintln!("⚠ file: 未 Accept 的 Chunk（协议违规），忽略 seq={seq}"),
                    }
                }
                FileTransferAction::Done { chunks } => {
                    match recv_session.take() {
                        Some((id, s)) if id == ev.transfer_id => match s.finish(chunks) {
                            Ok((name, bytes)) => {
                                let fname = sanitize_name(&name);
                                let path = args.run_dir.join(format!("ft_recv_{fname}"));
                                std::fs::write(&path, &bytes).context("写接收文件失败")?;
                                println!(
                                    "⇄ file: 接收完成 → ft_recv_{fname} ({}B)",
                                    bytes.len()
                                );
                            }
                            Err(e) => eprintln!("⚠ file: 重组失败: {e}"),
                        },
                        _ => eprintln!("⚠ file: 无对应会话的 Done，忽略"),
                    }
                }
                FileTransferAction::Accept => {
                    if ev.transfer_id == OFFER_TRANSFER_ID {
                        if let Some(bytes) = pending_send.take() {
                            let mut n: u64 = 0;
                            for (i, piece) in bytes.chunks(MAX_FILE_CHUNK_SIZE).enumerate() {
                                send_file_event(
                                    &dc,
                                    &key,
                                    &FileTransferEvent {
                                        transfer_id: OFFER_TRANSFER_ID,
                                        action: FileTransferAction::Chunk {
                                            seq: i as u64,
                                            data: piece.to_vec(),
                                        },
                                    },
                                )
                                .await?;
                                n += 1;
                            }
                            send_file_event(
                                &dc,
                                &key,
                                &FileTransferEvent {
                                    transfer_id: OFFER_TRANSFER_ID,
                                    action: FileTransferAction::Done { chunks: n },
                                },
                            )
                            .await?;
                            println!("⇄ file: Viewer 已 Accept，{n} 个分片 + Done 发送完毕");
                        }
                    }
                }
                FileTransferAction::Reject { reason } => {
                    println!("⇄ file: Viewer 拒绝了本次传输: {reason}");
                    pending_send = None;
                }
                FileTransferAction::Abort => {
                    println!("⇄ file: 收到 Abort");
                    recv_session = None;
                }
            },
            _ => {} // 非 FileTransfer 的 proto Message：忽略
        }
    }

    ice_stop.store(true, Ordering::SeqCst);
    Ok(())
}
