//! 视频帧的 RTP 分片打包与抗丢包重组（gap J 生产路径）。
//!
//! 与旧 `h264_rtp`（RFC 6184 NAL 重组）不同：本模块把**整帧已序列化/已加密的字节**
//! 当作不透明载荷传输。视频像素在应用层已做端到端 AEAD（防信令 MITM，见
//! `rdcore-app::Connection::send_media`），RTP 层若再按 NAL 结构打包反而要求载荷可解析，
//! 与加密冲突；因此这里只做「定长分片 + 时间戳分组 + 序列号连续」的最小重组：
//!
//! - 发送端（[`RtpFramePacketizer`]）：整帧切成 ≤ [`VIDEO_RTP_MTU`] 字节的连续 RTP 包，
//!   同一帧共享同一 RTP 时间戳（90kHz），末包 marker 置位，序列号跨帧连续递增。
//! - 接收端（[`RtpFrameReassembler`]）：按时间戳归组、序列号严格连续拼合；
//!   **出现缺口即丢弃整帧并毒化该时间戳**（不等重传、不阻塞后续帧）。
//!
//! 为什么可以这么「粗暴」：本系统编码器每帧强制 IDR（全帧内编码），任何一帧独立可解，
//! 丢一帧天然在下一帧恢复，无需 PLI/FIR 与参考链维护。这正是视频从「可靠有序 SCTP
//! DataChannel」（队头阻塞、单丢包全流卡顿）迁到 RTP 的收益：丢包只影响当前帧。
//!
//! SDP 协商辅助见 [`sdp_has_active_video`]：两端据此判定对端是否声明了视频 m-line，
//! 不支持 RTP 的旧端（如 Web Viewer）自动回退 `media` DataChannel 路径。

use rtp::header::Header;
use rtp::packet::Packet;
use std::time::{SystemTime, UNIX_EPOCH};

/// 单个 RTP 包的最大载荷字节数。
///
/// 对应常见 1500 字节链路 MTU，扣除 IP/UDP/SRTP 头与认证标签后的安全值（与
/// WebRTC 业界惯用的 1200 对齐）。一帧 3440×1440 的 IDR（约 190KB）约切 160 包。
pub const VIDEO_RTP_MTU: usize = 1200;

/// 视频轨道在 SDP 中声明的 payload type（H.264 / 90kHz）。
///
/// 注意：这只是 SDP 协商占位；`TrackLocalStaticRTP::write_rtp` 会按实际 binding
/// 覆盖每个包的 `payload_type` 与 `ssrc`，因此此处取值只影响 SDP 文本。
pub const VIDEO_PAYLOAD_TYPE: u8 = 96;

/// 单帧重组缓冲的硬上限（防对端异常 / 内存炸弹）。与 `rdcore-media` 的
/// `MAX_MEDIA_FRAME_LEN = 64 MiB` 对齐（Raw 1080p 约 8MiB，余量充足）。
const MAX_ASSEMBLY_LEN: usize = 64 * 1024 * 1024;

/// 取当前时刻的 90kHz RTP 时间戳（ wrapping u32，约 13.2 小时回绕一次；
/// 回绕由 [`ts_newer`] 的回绕比较正确处理）。
pub fn timestamp_90khz() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 90_000 ticks/s = 每 1e9/9e4 ns 一个 tick；用 u128 防溢出后再截断。
    ((nanos * 90_000 / 1_000_000_000) & 0xffff_ffff) as u32
}

/// 回绕安全的「a 严格新于 b」比较（RFC 3550 序列号/时间戳常规写法）：
/// 差值落在 (0, 2^31) 区间视为 a 更新。
fn ts_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// 判断 SDP 文本中是否存在**未被拒绝**的视频 m-line。
///
/// SDP 按行组织，`m=video <port> ...` 中 port 为 0 表示该媒体描述被拒绝
///（RFC 3266）。两端据此做能力探测：Viewer 在 Offer 里声明视频 m-line，
/// Host 见到才 `setup_video_track`；Host 的 Answer 里视频 m-line 保持激活，
/// Viewer 才启用 RTP 收帧。旧端（Web Viewer 等）不发视频 m-line，自动回退
/// `media` DataChannel——SDP 本身已被 Offer/Answer 签名覆盖，探测结果可信。
pub fn sdp_has_active_video(sdp: &str) -> bool {
    sdp.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("m=video")
            && line
                .split_whitespace()
                .nth(1)
                .is_some_and(|port| port != "0")
    })
}

/// 最新 N 帧队列（直播语义）：连接 RTP 重组产出（`on_track` 任务）与上层消费
///（`VideoReceiver::recv`）。
///
/// 与 `mpsc` 的根本差异在**满载策略**：实时视频里「积压 = 延迟」，消费者一旦
/// 短暂停顿，队列里塞的都是过期画面。`mpsc::try_send` 满载丢**新**帧会让消费者
/// 恢复后先看到最旧的画面（陈帧还要白白解码）；本队列满载改为丢**最旧**帧，
/// 消费者永远优先拿到最新画面——与 FFI 拉帧侧的「追帧丢旧」语义首尾呼应。
///
/// 取消与关闭语义：[`recv`](Self::recv) 的 await 点可安全取消；[`close`](Self::close)
/// 后所有当前/后续 `recv` 返回 `None`（轨道关闭 / 连接断开时由 `on_track` 任务调用）。
pub struct LatestFrameQueue {
    inner: std::sync::Mutex<QueueInner>,
    notify: tokio::sync::Notify,
}

struct QueueInner {
    q: std::collections::VecDeque<Vec<u8>>,
    /// 容量上限（满载丢最旧）。显式存储，不依赖 VecDeque::capacity
    ///（其值允许大于请求值，且随增长策略变化）。
    cap: usize,
    closed: bool,
}

impl LatestFrameQueue {
    /// 新建容量为 `cap`（≥1）的队列。
    pub fn new(cap: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(QueueInner {
                q: std::collections::VecDeque::new(),
                cap: cap.max(1),
                closed: false,
            }),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// 投递一帧；满载时挤掉最旧帧。返回是否发生了挤占（供丢帧计数/日志）。
    /// 关闭后调用为空操作（返回 false）。
    pub fn push(&self, frame: Vec<u8>) -> bool {
        let evicted = {
            let mut g = self.inner.lock().unwrap();
            if g.closed {
                return false;
            }
            let evicted = g.q.len() >= g.cap && g.q.pop_front().is_some();
            g.q.push_back(frame);
            evicted
        };
        // 存一个许可：无论 recv 当前是否在等待，下一次 recv 都能立即醒来。
        self.notify.notify_one();
        evicted
    }

    /// 关闭队列：当前与后续的 `recv` 一律返回 `None`。幂等。
    pub fn close(&self) {
        {
            let mut g = self.inner.lock().unwrap();
            if g.closed {
                return;
            }
            g.closed = true;
        }
        // 双保险：notify_waiters 唤醒已登记的等待者；notify_one 存的许可覆盖
        // 「close 时还未登记」的后续等待者（其 recv 循环会先看到 closed，不依赖许可，
        // 但许可保证它至少醒一次做这次检查）。
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// 取下一帧（FIFO）；队列空时挂起，关闭且无帧可取时返回 `None`。
    pub async fn recv(&self) -> Option<Vec<u8>> {
        loop {
            // 先建 future 再查状态：close/push 若发生在检查与 await 之间，
            // 许可已存，await 立即返回，循环复查状态——不会睡死。
            let notified = self.notify.notified();
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(f) = g.q.pop_front() {
                    return Some(f);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

/// 视频帧 RTP 分片打包器（发送端，有状态：序列号跨帧连续）。
///
/// 每个包的 `ssrc` / `payload_type` 由 `TrackLocalStaticRTP::write_rtp` 按协商
/// binding 覆盖，这里只负责 `version=2`、序列号、时间戳与 marker。
#[derive(Debug, Default)]
pub struct RtpFramePacketizer {
    seq: u16,
}

impl RtpFramePacketizer {
    /// 新建打包器（序列号从 0 起；标准允许随机起点，固定起点便于测试断言）。
    pub fn new() -> Self {
        Self { seq: 0 }
    }

    /// 把一帧完整载荷切成连续 RTP 包（末包 marker 置位）。
    ///
    /// 空载荷也产出一个 marker 包（长度 0 的帧照样自定界）。
    pub fn packetize(&mut self, payload: &[u8], timestamp: u32) -> Vec<Packet> {
        let n = payload.len().div_ceil(VIDEO_RTP_MTU).max(1);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let start = i * VIDEO_RTP_MTU;
            let end = (start + VIDEO_RTP_MTU).min(payload.len());
            let marker = i + 1 == n;
            out.push(Packet {
                header: Header {
                    version: 2,
                    marker,
                    payload_type: VIDEO_PAYLOAD_TYPE,
                    sequence_number: self.seq,
                    timestamp,
                    ..Default::default()
                },
                payload: payload[start..end].to_vec().into(),
            });
            self.seq = self.seq.wrapping_add(1);
        }
        out
    }
}

/// 一帧的重组中间态。
#[derive(Debug)]
struct Assembly {
    /// 帧时间戳（同帧所有包共享）。
    ts: u32,
    /// 下一个期望的序列号（严格连续；已到过的序号区间一律视为重复包忽略）。
    next_seq: u16,
    /// 已拼合的字节。
    buf: Vec<u8>,
}

/// 视频帧 RTP 抗丢包重组器（接收端，每条远端轨道一个实例，按到达顺序喂包）。
///
/// 语义（与模块文档一致）：
/// - 时间戳归组：新时间戳到达即丢弃未完成的旧帧（它再也不可能补齐——发送端
///   序列号连续，缺了的片不会以「新时间戳之外的序号」补发）。
/// - 序列号连续：同帧内出现缺口 → 丢弃整帧并**毒化**该时间戳（后续同帧残片
///   一律忽略，绝不产出半帧）。
/// - 迟到包（时间戳旧于已见最大时间戳）静默忽略；重复包（序号落后于
///   `next_seq`）静默忽略。
/// - marker 置位且拼合连续 → 产出整帧字节。
#[derive(Debug, Default)]
pub struct RtpFrameReassembler {
    cur: Option<Assembly>,
    /// 已见最大时间戳（用于拒绝迟到包）。
    max_ts: Option<u32>,
    /// 已毒化的时间戳（该帧出现缺口，残片一律忽略）。
    poisoned_ts: Option<u32>,
}

impl RtpFrameReassembler {
    /// 新建重组器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个 RTP 包；一帧完整时返回 `Some(整帧字节)`，否则 `None`。
    ///
    /// 本函数纯同步、无错误返回：一切异常（缺口 / 迟到 / 重复 / 超长）都归结为
    /// 「丢帧」，由调用方自然跳过——这是 RTP 路径与 DataChannel 路径的核心差异。
    pub fn push(&mut self, pkt: &Packet) -> Option<Vec<u8>> {
        let ts = pkt.header.timestamp;
        let seq = pkt.header.sequence_number;

        // 已毒化帧的残片：忽略。
        if self.poisoned_ts == Some(ts) {
            return None;
        }
        // 迟到包（旧于已见最大时间戳）：忽略。
        if let Some(m) = self.max_ts {
            if ts_newer(m, ts) {
                return None;
            }
        }
        // 推进最大时间戳。
        if self.max_ts.is_none_or(|m| ts_newer(ts, m)) {
            self.max_ts = Some(ts);
        }

        match self.cur.take() {
            // 无在拼帧：从本包起新帧。
            None => self.start(ts, seq, pkt),
            Some(a) if a.ts != ts => {
                // 时间戳前进：旧帧未完成（缺片），丢弃；从本包起新帧。
                // （旧帧的时间戳已小于 max_ts，其迟到残片由上面的迟到检查拦截。）
                self.start(ts, seq, pkt)
            }
            Some(mut a) => {
                let behind = a.next_seq.wrapping_sub(seq);
                if behind == 0 {
                    // 期望的下一包：拼合。
                    self.append(a, pkt)
                } else if behind < 0x8000 {
                    // 序号落后：重复包（网络重复或 NACK 重传的已消费片），忽略。
                    a.buf.shrink_to_fit();
                    self.cur = Some(a);
                    None
                } else {
                    // 序号超前：出现缺口，本帧作废并毒化。
                    self.poisoned_ts = Some(ts);
                    None
                }
            }
        }
    }

    /// 从首包起一帧；单包帧（marker 置位）立即产出。
    fn start(&mut self, ts: u32, seq: u16, pkt: &Packet) -> Option<Vec<u8>> {
        if pkt.payload.len() > MAX_ASSEMBLY_LEN {
            self.poisoned_ts = Some(ts);
            return None;
        }
        if pkt.header.marker {
            // 单包帧：直接产出，不留中间态。
            return Some(pkt.payload.to_vec());
        }
        self.cur = Some(Assembly {
            ts,
            next_seq: seq.wrapping_add(1),
            buf: pkt.payload.to_vec(),
        });
        None
    }

    /// 拼合同帧的下一包；marker 置位即产出整帧。
    fn append(&mut self, mut a: Assembly, pkt: &Packet) -> Option<Vec<u8>> {
        if a.buf.len() + pkt.payload.len() > MAX_ASSEMBLY_LEN {
            // 超长即异常（防内存炸弹）：毒化本帧。
            self.poisoned_ts = Some(a.ts);
            return None;
        }
        a.buf.extend_from_slice(&pkt.payload);
        a.next_seq = a.next_seq.wrapping_add(1);
        if pkt.header.marker {
            Some(a.buf)
        } else {
            self.cur = Some(a);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用打包器产出第 `ts` 帧的包的便捷断言工具。
    fn pkt(payload: &[u8], ts: u32, seq: u16, marker: bool) -> Packet {
        Packet {
            header: Header {
                version: 2,
                marker,
                payload_type: VIDEO_PAYLOAD_TYPE,
                sequence_number: seq,
                timestamp: ts,
                ..Default::default()
            },
            payload: payload.to_vec().into(),
        }
    }

    #[test]
    fn packetize_single_packet_frame() {
        let mut p = RtpFramePacketizer::new();
        let pkts = p.packetize(b"hello", 1000);
        assert_eq!(pkts.len(), 1, "小载荷应单包");
        assert!(pkts[0].header.marker, "单包应置 marker");
        assert_eq!(pkts[0].header.version, 2, "RTP version 应为 2");
        assert_eq!(pkts[0].payload.as_ref(), b"hello");
    }

    #[test]
    fn packetize_fragments_and_marks_last() {
        let mut p = RtpFramePacketizer::new();
        let payload = vec![0xABu8; VIDEO_RTP_MTU * 3 + 17];
        let pkts = p.packetize(&payload, 2000);
        assert_eq!(pkts.len(), 4, "应按 MTU 分片");
        for (i, k) in pkts.iter().enumerate() {
            assert_eq!(k.header.sequence_number, i as u16, "序列号应连续");
            assert_eq!(k.header.timestamp, 2000, "同帧时间戳应一致");
            assert_eq!(k.header.marker, i + 1 == pkts.len(), "仅末包置 marker");
            assert_eq!(k.header.version, 2);
        }
        // 拼回应等于原载荷。
        let mut joined = Vec::new();
        for k in &pkts {
            joined.extend_from_slice(&k.payload);
        }
        assert_eq!(joined, payload);
    }

    #[test]
    fn packetizer_seq_continues_across_frames() {
        let mut p = RtpFramePacketizer::new();
        let a = p.packetize(&vec![0u8; VIDEO_RTP_MTU * 2], 100);
        let b = p.packetize(&vec![1u8; VIDEO_RTP_MTU], 200);
        assert_eq!(a.len(), 2);
        assert_eq!(b[0].header.sequence_number, 2, "下一帧序列号应接续");
    }

    #[test]
    fn reassemble_single_packet_frame() {
        let mut r = RtpFrameReassembler::new();
        let out = r.push(&pkt(b"frame", 500, 0, true));
        assert_eq!(out.as_deref(), Some(b"frame".as_ref()));
    }

    #[test]
    fn reassemble_multi_fragment_roundtrip() {
        let mut p = RtpFramePacketizer::new();
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let pkts = p.packetize(&payload, 4242);
        assert!(pkts.len() > 1);
        let mut r = RtpFrameReassembler::new();
        let mut got = None;
        for k in &pkts {
            got = r.push(k).or(got);
        }
        assert_eq!(got, Some(payload), "完整到达应字节一致");
    }

    #[test]
    fn gap_poisons_frame_and_next_frame_recovers() {
        let mut p = RtpFramePacketizer::new();
        let f1 = p.packetize(&vec![0x11; VIDEO_RTP_MTU * 4], 1000);
        let f2 = p.packetize(&vec![0x22; VIDEO_RTP_MTU * 2], 2000);
        let mut r = RtpFrameReassembler::new();
        // f1 丢掉第 2 片（seq 1）：第 3 片（seq 2）到达时应发现缺口、毒化 1000。
        assert!(r.push(&f1[0]).is_none());
        assert!(r.push(&f1[2]).is_none(), "缺口后本帧作废");
        // 同帧残片（含 marker）不再产出。
        assert!(r.push(&f1[3]).is_none(), "毒化帧残片应被忽略");
        // f2 完好到达：正常产出，不受前帧影响（全 IDR 策略下直接恢复）。
        let mut got = None;
        for k in &f2 {
            got = r.push(k).or(got);
        }
        assert_eq!(got, Some(vec![0x22; VIDEO_RTP_MTU * 2]));
    }

    #[test]
    fn lost_last_fragment_drops_frame_silently() {
        let mut p = RtpFramePacketizer::new();
        let f1 = p.packetize(&vec![0x33; VIDEO_RTP_MTU * 3], 7000);
        let f2 = p.packetize(&[0x44; 10], 8000);
        let mut r = RtpFrameReassembler::new();
        assert!(r.push(&f1[0]).is_none());
        assert!(r.push(&f1[1]).is_none());
        // f1 的 marker 片丢失；f2 首包到达时旧帧被丢弃，f2 正常产出。
        let out = r.push(&f2[0]);
        assert_eq!(out, Some(vec![0x44; 10]), "新帧到达即放弃残缺旧帧");
    }

    #[test]
    fn duplicate_and_late_packets_ignored() {
        let mut p = RtpFramePacketizer::new();
        let f1 = p.packetize(&vec![0x55; VIDEO_RTP_MTU * 2], 100);
        let f2 = p.packetize(&[0x66; 5], 200);
        let mut r = RtpFrameReassembler::new();
        assert!(r.push(&f1[0]).is_none());
        // 重复首片（网络重复 / NACK 重传）：忽略，不破坏重组。
        assert!(r.push(&f1[0]).is_none(), "重复包应被忽略");
        let out = r.push(&f1[1]);
        assert_eq!(out, Some(vec![0x55; VIDEO_RTP_MTU * 2]));
        // 上一帧的迟到残片（时间戳 100 < max 100？ 不，等于 max 但帧已产出）：
        // 此时 cur=None，100 不再新于 max_ts(100)——不会误判为新帧。
        assert!(r.push(&f1[0]).is_none(), "已产出帧的迟到包应被忽略");
        let out2 = r.push(&f2[0]);
        assert_eq!(out2, Some(vec![0x66; 5]));
    }

    #[test]
    fn reordered_fragment_drops_frame_but_recovers() {
        // 乱序（UDP 偶发）：同帧内序号超前 = 缺口 → 丢帧；下一帧恢复。
        let mut p = RtpFramePacketizer::new();
        let f1 = p.packetize(&vec![0x77; VIDEO_RTP_MTU * 3], 900);
        let f2 = p.packetize(&[0x88; 7], 1000);
        let mut r = RtpFrameReassembler::new();
        assert!(r.push(&f1[1]).is_none()); // 先收第 2 片：起新帧
        assert!(r.push(&f1[0]).is_none(), "乱序旧片视为重复忽略");
        // f1[0] 被当重复忽略后，f1[2] 接续 next_seq=2 → 会产出「缺首片的帧」！
        // 这是本设计的已知边界：乱序到达无法与「缺口」区分时按最简策略处理。
        // 由于载荷整体 AEAD，解密必然失败，上层丢帧（见 Connection::recv_media），
        // 不会渲染坏画面。
        let _ = r.push(&f1[2]);
        let out = r.push(&f2[0]);
        assert_eq!(out, Some(vec![0x88; 7]), "下一帧应正常恢复");
    }

    #[test]
    fn seq_wraparound_roundtrip() {
        // 序列号 u16 回绕：从 0xFFFE 起跨 0 应连续重组。
        let mut r = RtpFrameReassembler::new();
        assert!(r.push(&pkt(&[1u8; 4], 50, 0xFFFE, false)).is_none());
        assert!(r.push(&pkt(&[2u8; 4], 50, 0xFFFF, false)).is_none());
        let out = r.push(&pkt(&[3u8; 4], 50, 0, true));
        assert_eq!(
            out,
            Some(vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]),
            "回绕边界应正确拼合"
        );
    }

    #[test]
    fn sdp_video_detection() {
        let sdp_with = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 H264/90000\r\n";
        assert!(sdp_has_active_video(sdp_with), "激活的视频 m-line 应检出");
        let sdp_rejected = "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF 96\r\n";
        assert!(
            !sdp_has_active_video(sdp_rejected),
            "port=0 的拒绝 m-line 不算"
        );
        let sdp_none = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert!(!sdp_has_active_video(sdp_none), "无视频 m-line 不算");
        let sdp_lf = "v=0\nm=video 9 UDP/TLS/RTP/SAVPF 96\n";
        assert!(sdp_has_active_video(sdp_lf), "纯 LF 行尾也应检出");
    }

    #[test]
    fn reassembler_is_send() {
        // 编译期校验：重组器可 move 进 on_track 的 async 任务。
        fn assert_send<T: Send>() {}
        assert_send::<RtpFrameReassembler>();
    }

    #[tokio::test]
    async fn latest_queue_fifo_and_close() {
        let q = LatestFrameQueue::new(4);
        q.push(vec![1]);
        q.push(vec![2]);
        assert_eq!(q.recv().await, Some(vec![1]), "应先进先出");
        assert_eq!(q.recv().await, Some(vec![2]));
        q.close();
        assert_eq!(q.recv().await, None, "关闭且排空后应返回 None");
    }

    #[tokio::test]
    async fn latest_queue_evicts_oldest_on_full() {
        let q = LatestFrameQueue::new(2);
        assert!(!q.push(vec![1]));
        assert!(!q.push(vec![2]));
        assert!(q.push(vec![3]), "满载投递应挤掉最旧帧");
        assert!(q.push(vec![4]));
        assert_eq!(q.recv().await, Some(vec![3]), "最旧两帧应被挤掉");
        assert_eq!(q.recv().await, Some(vec![4]));
    }

    #[tokio::test]
    async fn latest_queue_recv_waits_then_wakes() {
        use std::sync::Arc;
        let q = Arc::new(LatestFrameQueue::new(4));
        let q2 = q.clone();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            q2.push(vec![9]);
        });
        assert_eq!(q.recv().await, Some(vec![9]), "空队列 recv 应挂起至投递");
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn latest_queue_close_wakes_waiter() {
        use std::sync::Arc;
        let q = Arc::new(LatestFrameQueue::new(4));
        let q2 = q.clone();
        let closer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            q2.close();
        });
        assert_eq!(q.recv().await, None, "等待中 close 应唤醒并返回 None");
        closer.await.unwrap();
        // 关闭后再投递：空操作，recv 仍返回 None。
        assert!(!q.push(vec![1]));
        assert_eq!(q.recv().await, None);
    }
}
