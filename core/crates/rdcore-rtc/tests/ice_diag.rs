//! 诊断：绕过信令服务器，直接在进程内交换 Offer/Answer/ICE，判断 localhost 回环
//! 下 WebRTC 自身能否连通。若本测试也超时，则问题在 WebRTC/ICE 配置（或沙箱网络）；
//! 若本测试通过而 `e2e_real_p2p` 失败，则问题在信令中继。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdcore_rtc::{RTCIceCandidateInit, RtcConfig, WebRtcPeer};
use tokio::sync::Mutex;

#[tokio::test]
async fn ice_diag_direct_loopback() {
    let cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };
    let a = Arc::new(WebRtcPeer::with_config(cfg.clone()).await.unwrap());
    let b = Arc::new(WebRtcPeer::with_config(cfg).await.unwrap());

    // Offer / Answer 直接交换（同进程，无需信令）。
    let sdp_a = a.create_offer().await.unwrap();
    let sdp_b = b.accept_offer(sdp_a).await.unwrap();
    a.accept_answer(sdp_b).await.unwrap();

    let a_drained = Arc::new(AtomicUsize::new(0));
    let b_drained = Arc::new(AtomicUsize::new(0));
    let a_sample: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let b_sample: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));

    // 直接交换 ICE 候选，直到两边都收集完成或超时。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let ca = a.drain_ice_candidates().await;
        for c in ca {
            let s = serde_json::to_string(&c).unwrap();
            let init: RTCIceCandidateInit = serde_json::from_str(&s).unwrap();
            let _ = b.add_ice_candidate(init).await;
            if a_sample.lock().await.is_empty() {
                a_sample.lock().await.push(s);
            }
        }
        a_drained.fetch_add(1, Ordering::SeqCst);

        let cb = b.drain_ice_candidates().await;
        for c in cb {
            let s = serde_json::to_string(&c).unwrap();
            let init: RTCIceCandidateInit = serde_json::from_str(&s).unwrap();
            let _ = a.add_ice_candidate(init).await;
            if b_sample.lock().await.is_empty() {
                b_sample.lock().await.push(s);
            }
        }
        b_drained.fetch_add(1, Ordering::SeqCst);

        if a.ice_gathering_complete() && b.ice_gathering_complete() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let connected = tokio::time::timeout(Duration::from_secs(10), async {
        a.wait_data_channels_open().await;
        b.wait_data_channels_open().await;
    })
    .await;

    if connected.is_err() {
        eprintln!(
            "DIAG FAILED: a_drains={} b_drains={} a_complete={} b_complete={}",
            a_drained.load(Ordering::SeqCst),
            b_drained.load(Ordering::SeqCst),
            a.ice_gathering_complete(),
            b.ice_gathering_complete()
        );
        eprintln!("DIAG a_sample: {:#?}", *a_sample.lock().await);
        eprintln!("DIAG b_sample: {:#?}", *b_sample.lock().await);
    }
    assert!(connected.is_ok(), "直接回环 ICE 诊断失败：见上方 DIAG 日志");
}
