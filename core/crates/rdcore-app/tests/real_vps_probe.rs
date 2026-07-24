//! 真实 VPS 联调探针（手动运行，不纳入 CI）：
//! 以真实配对码作为 Viewer 连接部署在 VPS 上的 Host，验证
//! 信令 → PeerHello → 验签 → ICE(STUN/TURN) → E2E → 同意 全链路。
//!
//! 用法：
//!   PROBE_SIGNALING=ws://8.138.237.243:8080 \
//!   PROBE_PAIRING=<32hex session>:<64hex token> \
//!   cargo test -p rdcore-app --test real_vps_probe -- --ignored --nocapture

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use rdcore_app::Connection;
use rdcore_consent::{ConsentDecision, ConsentScope};
use rdcore_crypto::Ed25519CryptoProvider;
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_proto::{MediaFrame, SessionId, VideoCodec};
use rdcore_rtc::{IceServer, RtcConfig};
use tokio::sync::Mutex;

fn parse_pairing(code: &str) -> (SessionId, String) {
    let (session_hex, token) = code.split_once(':').expect("配对码格式 <32hex>:<64hex>");
    let bytes = hex::decode(session_hex).expect("session 应为 32 位 hex");
    let mut id = [0u8; 16];
    id.copy_from_slice(&bytes);
    (SessionId(id), token.to_string())
}

/// 与生产一致的 STUN+TURN 配置（同 rdcore-desktop::config::default_rtc_config）。
fn prod_rtc_cfg() -> RtcConfig {
    RtcConfig {
        ice_servers: vec![
            IceServer {
                urls: vec!["stun:8.138.237.243:3478".into()],
                username: None,
                credential: None,
            },
            IceServer {
                urls: vec!["turn:8.138.237.243:3478?transport=udp".into()],
                username: Some("rdcore".into()),
                credential: Some(
                    "84d9e822b2be47739710013bfd15aec91b5cd4363c61b78c".into(),
                ),
            },
        ],
        channel_buffer: 64,
        include_loopback: false,
        force_relay: false,
    }
}

#[tokio::test]
#[ignore = "真实联调探针：需 PROBE_SIGNALING / PROBE_PAIRING 环境变量"]
async fn probe_viewer_against_deployed_host() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let signaling = std::env::var("PROBE_SIGNALING").expect("PROBE_SIGNALING");
    let pairing = std::env::var("PROBE_PAIRING").expect("PROBE_PAIRING");
    let (session, token) = parse_pairing(&pairing);
    let url = format!("{}/{}?token={token}", signaling.trim_end_matches('/'), hex::encode(session.0));
    eprintln!("[probe] url={url}");

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "probe-viewer");
    let store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer),
    ));

    eprintln!("[probe] 构造 Viewer 连接…");
    let viewer = Connection::new_viewer(
        &url,
        session,
        viewer_sk,
        store.clone(),
        prod_rtc_cfg(),
        Duration::from_secs(30),
    )
    .await
    .expect("构造 Viewer 连接失败");

    eprintln!("[probe] 开始 establish（PeerHello → 验签 → ICE → E2E → 同意）…");
    let stop = Arc::new(AtomicBool::new(false));
    let viewer = Arc::new(Mutex::new(viewer));
    let v = viewer.clone();
    let handle = tokio::spawn(async move { v.lock().await.establish(stop, None).await });
    match tokio::time::timeout(Duration::from_secs(45), handle).await {
        Ok(Ok(Ok(()))) => eprintln!("[probe] ✓ establish 成功"),
        Ok(Ok(Err(e))) => panic!("[probe] establish 失败：{e:#}"),
        Ok(Err(e)) => panic!("[probe] establish 任务 panic：{e}"),
        Err(_) => panic!("[probe] establish 超时（45s）——卡在 ICE/DTLS/E2E 阶段"),
    }

    let key = viewer
        .lock()
        .await
        .session_key()
        .expect("establish 后应有会话密钥");
    eprintln!("[probe] ✓ 会话密钥已派生（{} 字节）", key.0.len());

    // 媒体面：Host 在 establish 返回后启动抓屏泵，此处阻塞等首帧。
    eprintln!("[probe] 等待首帧（10s 超时）…");
    let v = viewer.clone();
    let frame = tokio::time::timeout(Duration::from_secs(10), async move {
        v.lock().await.recv_media().await
    })
    .await;
    match frame {
        Ok(Ok(Some(f))) => eprintln!(
            "[probe] ✓ 收到媒体帧 {}x{} len={} codec={:?}",
            f.width,
            f.height,
            f.data.len(),
            f.codec
        ),
        Ok(Ok(None)) => panic!("[probe] recv_media 返回 None（通道关闭）"),
        Ok(Err(e)) => panic!("[probe] recv_media 错误：{e:#}"),
        Err(_) => panic!("[probe] 等帧超时（10s）——媒体面不通"),
    }

    // 续帧检查（解码路径）：首帧之后继续读 8 秒，改用 recv_rendered（解密+H.264 解码+渲染），
    // 与 Flutter Viewer 的拉帧路径完全一致。若 recv_media 通路正常而此处卡死/报错，
    // 问题在解码器；若此处也流畅，问题在 Dart/UI 侧。
    eprintln!("[probe] 继续读帧 8s（recv_rendered 解码路径）…");
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut count = 0u32;
    let mut last_wh = (0u32, 0u32);
    while std::time::Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(std::time::Instant::now());
        let v = viewer.clone();
        match tokio::time::timeout(remain, async move { v.lock().await.recv_rendered().await })
            .await
        {
            Ok(Ok(Some(f))) => {
                count += 1;
                last_wh = (f.width, f.height);
            }
            Ok(Ok(None)) => {
                eprintln!("[probe] 通道在 {count} 帧后关闭");
                break;
            }
            Ok(Err(e)) => {
                eprintln!("[probe] recv_rendered 错误（已收 {count} 帧）：{e:#}");
                break;
            }
            Err(_) => break,
        }
    }
    eprintln!(
        "[probe] 8s 解码续帧：{count} 帧，末帧 {}x{}{}",
        last_wh.0,
        last_wh.1,
        if count == 0 { "（解码路径卡死！）" } else { "" }
    );
    assert!(count > 0, "首帧后解码路径无续帧");
}

/// 同进程 Host↔Viewer，但走**真实 VPS 信令 + STUN/TURN 配置**（逼近真机网络条件），
/// 复现「握手成功、媒体帧不到」并打印 send_media 的真实错误。
#[tokio::test]
#[ignore = "真实联调探针：需 PROBE_SIGNALING 环境变量"]
async fn probe_inprocess_media_over_vps() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let signaling = std::env::var("PROBE_SIGNALING").expect("PROBE_SIGNALING");
    let session = SessionId([0x42u8; 16]);
    let url = format!("{}/{}", signaling.trim_end_matches('/'), hex::encode(session.0));
    eprintln!("[probe] url={url}");

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "probe-viewer");
    let (host_peer, host_sk) = create_local_identity(&provider, "probe-host");
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer),
    ));
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer)));

    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            host_store,
            prod_rtc_cfg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: {
            let mut s = HashSet::new();
            s.insert(ConsentScope::View);
            s.insert(ConsentScope::Input);
            s
        },
        duration: None,
    };
    let h = host.clone();
    let s2 = stop.clone();
    let h_task = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });

    tokio::time::sleep(Duration::from_secs(1)).await;
    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            viewer_sk,
            viewer_store,
            prod_rtc_cfg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let v = viewer.clone();
    let s1 = stop.clone();
    let v_task = tokio::spawn(async move { v.lock().await.establish(s1, None).await });

    let joined = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(v_task, h_task)
    })
    .await;
    assert!(joined.is_ok(), "establish 超时");
    let (v_r, h_r) = joined.unwrap();
    v_r.unwrap().expect("Viewer establish 失败");
    h_r.unwrap().expect("Host establish 失败");
    eprintln!("[probe] ✓ establish 双方成功");

    // Host 直发一帧（绕过捕获/编码，排除采集变量），Viewer 限时收。
    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0x5Au8; 16 * 12 * 4],
    };
    for attempt in 1..=3 {
        match host.lock().await.send_media(&frame).await {
            Ok(()) => eprintln!("[probe] 第 {attempt} 次 send_media 成功"),
            Err(e) => panic!("[probe] 第 {attempt} 次 send_media 失败：{e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let v = viewer.clone();
    let got = tokio::time::timeout(Duration::from_secs(10), async move {
        v.lock().await.recv_media().await
    })
    .await;
    match got {
        Ok(Ok(Some(f))) => {
            assert_eq!(f, frame, "帧应无损往返");
            eprintln!("[probe] ✓ 收到媒体帧 {}x{}（内容一致）", f.width, f.height);
        }
        Ok(Ok(None)) => panic!("[probe] recv_media 返回 None（通道关闭）"),
        Ok(Err(e)) => panic!("[probe] recv_media 错误：{e:#}"),
        Err(_) => panic!("[probe] 等帧超时（10s）——send 成功但帧未到，通道静默丢包"),
    }
}
