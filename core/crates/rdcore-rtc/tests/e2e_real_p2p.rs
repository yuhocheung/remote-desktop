//! 真实 WebRTC P2P 端到端联调（localhost 回环）。
//!
//! 这是「让系统第一次真正动起来」的里程碑验证：不靠进程内占位、不靠伪造 SDP，
//! 而是用 `rdcore-rtc` 的真实 `webrtc-rs` PeerConnection，经真实 WebSocket 信令服务器
//! 中继 SDP/ICE，在 localhost 上完成真实的 ICE + DTLS 握手，拿到两条 negotiated 数据通道，
//! 然后验证媒体帧（视频）与控制消息（心跳）真的跨 P2P 字节管道往返。
//!
//! 关键：两个对等端同机，靠 host 候选（含回环 127.0.0.1）即可连接，无需 STUN/TURN。
//! `RtcConfig { include_loopback: true, ice_servers: vec![] }` 即为此场景优化。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdcore_media::{DataChannel, MediaChannel};
use rdcore_proto::{
    Capabilities, ConnectionAnswer, ConnectionOffer, FrameMetadata, Heartbeat, IceCandidate,
    InputCaps, MediaFrame, Message, SessionId, VideoCodec,
};
use rdcore_rtc::{RTCIceCandidateInit, RtcConfig, WebRtcPeer};
use rdcore_signaling::SignalingClient;

const VIEWER_ID: [u8; 16] = [1u8; 16];
const HOST_ID: [u8; 16] = [2u8; 16];

fn caps() -> Capabilities {
    Capabilities {
        video_codecs: vec![VideoCodec::Raw],
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

fn frame_meta() -> FrameMetadata {
    FrameMetadata {
        width: 64,
        height: 48,
        fps: 30,
        codec: VideoCodec::Raw,
    }
}

/// 在空闲端口启动信令服务器，返回其地址。
async fn spawn_server() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = signaling_svc::serve_listener(listener).await;
    });
    addr
}

#[tokio::test]
async fn real_webrtc_p2p_over_localhost() {
    // 显式安装 rustls 默认 CryptoProvider（原因同 rdcore-ffi 真实 e2e 测试：
    // gateway 的 aws-lc-rs 与 webrtc 的 ring 并存导致 rustls 无法自动判定）。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let addr = spawn_server().await;
    let session = SessionId([7u8; 16]);
    let url = format!("ws://{addr}/{}", signaling_svc::session_hex(&session));

    // 纯 host 候选 + 回环：同机 localhost 即可连通，无需 STUN/TURN。
    let cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };
    let viewer_peer = Arc::new(WebRtcPeer::with_config(cfg.clone()).await.unwrap());
    let host_peer = Arc::new(WebRtcPeer::with_config(cfg).await.unwrap());

    let viewer_sig = Arc::new(SignalingClient::connect(&url).await.unwrap());
    let host_sig = Arc::new(SignalingClient::connect(&url).await.unwrap());

    let stop = Arc::new(AtomicBool::new(false));

    let v_task = tokio::spawn(viewer_handshake(
        viewer_peer.clone(),
        viewer_sig.clone(),
        session,
        stop.clone(),
    ));
    let h_task = tokio::spawn(host_handshake(
        host_peer.clone(),
        host_sig.clone(),
        session,
        stop.clone(),
    ));

    // 等待两条数据通道真正 open（ICE + DTLS 成功），整体 20s 超时防挂起。
    let connected = tokio::time::timeout(Duration::from_secs(20), async {
        viewer_peer.wait_data_channels_open().await;
        host_peer.wait_data_channels_open().await;
    })
    .await;
    assert!(
        connected.is_ok(),
        "真实 WebRTC 连接（数据通道 open）应在 localhost 上建立；超时说明 ICE 未连通"
    );

    // 握手/ICE 中继可以停止了。
    stop.store(true, Ordering::SeqCst);
    let _ = tokio::join!(v_task, h_task);

    // 取通道：media 走视频帧，control 走控制消息。
    let (viewer_media, viewer_dc) = viewer_peer.channels();
    let (host_media, host_dc) = host_peer.channels();

    // 媒体帧：Host 捕获 → 编码 → 真实 WebRTC → Viewer 解码。
    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0xABu8; 16 * 12 * 4],
    };
    host_media
        .send_frame(&frame)
        .await
        .expect("Host 经真实 WebRTC 媒体通道发帧");
    let got = viewer_media
        .recv_frame()
        .await
        .unwrap()
        .expect("Viewer 应收到媒体帧");
    assert_eq!(got, frame, "视频帧应经真实 WebRTC 媒体通道无损往返");

    // 控制消息：Host → Viewer 心跳。
    let hb = Message::Heartbeat(Heartbeat {
        seq: 1,
        timestamp_ms: 1_700_000_000_000,
    });
    host_dc
        .send(&hb)
        .await
        .expect("Host 经真实 WebRTC 控制通道发消息");
    let got = viewer_dc
        .recv()
        .await
        .unwrap()
        .expect("Viewer 应收到控制消息");
    assert_eq!(got, hb, "控制消息应经真实 WebRTC 数据通道无损往返");
}

/// Viewer 握手：发 Offer，等 Answer（设置 remote description），然后跑 ICE 中继循环。
async fn viewer_handshake(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    stop: Arc<AtomicBool>,
) {
    let sdp = peer.create_offer().await.expect("create_offer");
    sig.send(&conn_offer(session, sdp))
        .await
        .expect("send offer");
    // 等 Answer；等待期间到达的 ICE 候选先喂给库内缓冲（remote desc 就绪后自动 flush）。
    let answer_sdp = loop {
        match recv_with_timeout(&sig).await {
            Some(Message::Answer(a)) => break a.sdp,
            Some(Message::Ice(i)) => feed_ice(&peer, &i).await,
            Some(_) => continue,
            None => return, // 信道关闭
        }
    };
    peer.accept_answer(answer_sdp)
        .await
        .expect("accept_answer（设置 remote description）");
    relay_loop(peer, sig, session, VIEWER_ID, stop).await;
}

/// Host 握手：等 Offer → 回 Answer，然后跑 ICE 中继循环直到被 stop。
async fn host_handshake(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    stop: Arc<AtomicBool>,
) {
    // 等对端 Offer；等待期间到达的 ICE 候选先喂给库内缓冲。
    let offer_sdp = loop {
        match recv_with_timeout(&sig).await {
            Some(Message::Offer(o)) => break o.sdp,
            Some(Message::Ice(i)) => feed_ice(&peer, &i).await,
            Some(_) => continue,
            None => return, // 信道关闭
        }
    };
    let answer_sdp = peer.accept_offer(offer_sdp).await.expect("accept_offer");
    sig.send(&conn_answer(session, answer_sdp))
        .await
        .expect("send answer");
    relay_loop(peer, sig, session, HOST_ID, stop).await;
}

/// ICE 中继循环：把本地收集的候选经信令发给对端，并接收对端候选加入连接（trickle ICE）。
async fn relay_loop(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    from: [u8; 16],
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        // drain 本地已收集候选并发出。
        for c in peer.drain_ice_candidates().await {
            sig.send(&ice_msg(session, from, &c))
                .await
                .expect("send ice");
        }
        // 收对端消息（带超时，以便周期性 drain 本地候选）。
        match recv_with_timeout(&sig).await {
            Some(Message::Ice(i)) => {
                let init: RTCIceCandidateInit = serde_json::from_str(&i.candidate)
                    .expect("ICE candidate 应为 JSON 序列化的 RTCIceCandidateInit");
                // 连接建立后到达的候选忽略错误（webrtc 拒绝重复/迟到候选）。
                let _ = peer.add_ice_candidate(init).await;
            }
            Some(_) => {}  // Offer/Answer 已由各自握手函数处理；其余忽略
            None => break, // 信道关闭
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 带超时的 recv：超时返回 None（让外层继续 drain 本地候选），信道关闭也返回 None。
async fn recv_with_timeout(sig: &SignalingClient) -> Option<Message> {
    match tokio::time::timeout(Duration::from_millis(50), sig.recv()).await {
        Ok(Ok(m)) => m,
        _ => None,
    }
}

/// 把信令上收到的 ICE 候选喂给 peer（库内会在 remote description 就绪前缓冲）。
async fn feed_ice(peer: &WebRtcPeer, i: &IceCandidate) {
    let init: RTCIceCandidateInit = serde_json::from_str(&i.candidate)
        .expect("ICE candidate 应为 JSON 序列化的 RTCIceCandidateInit");
    let _ = peer.add_ice_candidate(init).await;
}

fn conn_offer(session: SessionId, sdp: String) -> Message {
    Message::Offer(ConnectionOffer {
        session_id: session,
        from: VIEWER_ID,
        sdp,
        capabilities: caps(),
        frame: Some(frame_meta()),
        signature: None,
    })
}

fn conn_answer(session: SessionId, sdp: String) -> Message {
    Message::Answer(ConnectionAnswer {
        session_id: session,
        from: HOST_ID,
        sdp,
        capabilities: caps(),
        frame: Some(frame_meta()),
        signature: None,
    })
}

fn ice_msg(session: SessionId, from: [u8; 16], cand: &RTCIceCandidateInit) -> Message {
    Message::Ice(IceCandidate {
        session_id: session,
        from,
        candidate: serde_json::to_string(cand).expect("serialize ice candidate"),
        sdp_mid: None,
        sdp_mline_index: None,
    })
}
