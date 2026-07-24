//! 编码器吞吐压测：验证 60fps 目标下硬编（及软编对照）是否撑得住。
//!
//! 跑法（Windows，带 GPU）：
//!   cargo run --release --example enc_bench --features hwcodec          # 硬件优先
//!   BENCH_SW=1 cargo run --release --example enc_bench --features hwcodec  # 强制软编对照
//!
//! 口径：先 30 帧热身（越过 nvenc 流水线启动延迟），再计时 600 帧纯编码耗时，
//! 报告实际 fps、平均每帧字节数与码率。不含抓屏与网络，纯编码器吞吐。

use rdcore_encode::{new_encoder_forced_with_fps, new_encoder_with_fps};
use rdcore_proto::{MediaFrame, VideoCodec};
use std::time::Instant;

// 规格可用环境变量覆盖（排障时对齐真机分辨率/帧率）：
//   BENCH_W=3440 BENCH_H=1440 BENCH_FPS=60 cargo run --release --example enc_bench --features hwcodec
static W: u32 = 1280;
static H: u32 = 720;
fn dim() -> (u32, u32, u16) {
    let w = std::env::var("BENCH_W").ok().and_then(|v| v.parse().ok()).unwrap_or(W);
    let h = std::env::var("BENCH_H").ok().and_then(|v| v.parse().ok()).unwrap_or(H);
    let fps = std::env::var("BENCH_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(FPS);
    (w, h, fps)
}
const FPS: u16 = 60;
const WARMUP: u32 = 30;
const N: u32 = 600;

fn make_frame(w: u32, h: u32, t: u32) -> MediaFrame {
    let (w, h) = (w as usize, h as usize);
    let mut data = vec![0u8; w * h * 4];
    let bx = ((t as usize * 20) % w.saturating_sub(80)).max(0);
    let by = ((t as usize * 12) % h.saturating_sub(60)).max(0);
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) * 4;
            let r = ((x * 255 / w) as u8).wrapping_add((t * 3) as u8);
            let g = ((y * 255 / h) as u8).wrapping_add((t * 5) as u8);
            let b = (((x + y) * 255 / (w + h)) as u8).wrapping_add((t * 7) as u8);
            if x >= bx && x < bx + 80 && y >= by && y < by + 60 {
                data[idx] = 255;
                data[idx + 1] = 0;
                data[idx + 2] = 0;
            } else {
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
            }
            data[idx + 3] = 255;
        }
    }
    MediaFrame {
        codec: VideoCodec::Raw,
        width: w as u32,
        height: h as u32,
        data,
    }
}

fn main() {
    let (w, h, fps) = dim();
    let force_sw = std::env::var("BENCH_SW").is_ok();
    println!("=== rdcore 编码器吞吐压测：{w}x{h} 目标 {fps}fps ===");
    let enc = if force_sw {
        new_encoder_forced_with_fps(VideoCodec::H264, w, h, true, fps)
    } else {
        new_encoder_with_fps(VideoCodec::H264, w, h, fps)
    }
    .expect("构造编码器失败");
    println!("后端 kind = {}，热身 {WARMUP} 帧 ...", enc.kind());

    // 预生成 8 帧循环复用：帧合成（5M 像素 memset）不计入编码计时，
    // 多帧而非单帧是为避免「全静态画面」让码率统计失真。
    let frames: Vec<MediaFrame> = (0..8).map(|i| make_frame(w, h, i)).collect();
    for i in 0..WARMUP {
        enc.encode(&frames[(i as usize) % 8]).expect("热身帧编码失败");
    }

    let t0 = Instant::now();
    let mut bytes = 0usize;
    let mut idr_frames = 0u32;
    let mut intra_frames = 0u32;
    for i in 0..N {
        let out = enc.encode(&frames[((WARMUP + i) as usize) % 8]).expect("计时帧编码失败");
        bytes += out.data.len();
        match annexb_frame_kind(&out.data) {
            FrameKind::Idr => idr_frames += 1,
            FrameKind::Intra => intra_frames += 1,
            FrameKind::Inter => {}
        }
    }
    let dt = t0.elapsed();

    let fps_actual = N as f64 / dt.as_secs_f64();
    let avg = bytes / N as usize;
    let mbps = bytes as f64 * 8.0 / dt.as_secs_f64() / 1e6;
    let independent = idr_frames + intra_frames;
    println!("\n{N} 帧耗时 {dt:.2?}");
    println!("实测吞吐: {fps_actual:.1} fps（目标 {fps}）{}", if fps_actual >= fps as f64 { "✅ 达标" } else { "❌ 不达标" });
    println!("平均每帧 {avg} 字节，实测码率 {mbps:.2} Mbps");
    println!(
        "独立可解帧: {independent}/{N}（IDR {idr_frames} + 非 IDR I {intra_frames}）{}",
        if independent == N { "✅ 全帧可丢" } else { "⚠ 含 inter 帧，丢帧需等恢复点" }
    );
}

/// Annex-B 帧类型（与 Web Viewer pipeline.worker.ts 同口径）。
enum FrameKind {
    /// 含 NAL type=5（真 IDR）。
    Idr,
    /// 无 IDR 但含 I-slice（slice_type 2/7）——NVENC 强制 I 帧，独立可解。
    Intra,
    /// 其余（P/B，需参考链）。
    Inter,
}

fn annexb_frame_kind(data: &[u8]) -> FrameKind {
    let mut saw_intra = false;
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            let (hdr, payload) = if data[i + 2] == 1 {
                (data[i + 3], i + 4)
            } else if data[i + 2] == 0 && data[i + 3] == 1 && i + 4 < data.len() {
                (data[i + 4], i + 5)
            } else {
                i += 1;
                continue;
            };
            let nal_type = hdr & 0x1f;
            if nal_type == 5 {
                return FrameKind::Idr;
            }
            if nal_type == 1 && payload < data.len() {
                let st = read_slice_type(data, payload);
                if st == 2 || st == 7 {
                    saw_intra = true;
                }
            }
        }
        i += 1;
    }
    if saw_intra {
        FrameKind::Intra
    } else {
        FrameKind::Inter
    }
}

/// 读 slice header 的 slice_type：RBSP（剥 00 00 03）上两个 ue(v)。
fn read_slice_type(data: &[u8], start: usize) -> i64 {
    let mut rbsp = [0u8; 24];
    let mut n = 0;
    let mut i = start;
    while i < data.len().min(start + 32) && n < 24 {
        if i >= start + 2 && data[i - 2] == 0 && data[i - 1] == 0 && data[i] == 3 {
            i += 1;
            continue;
        }
        rbsp[n] = data[i];
        n += 1;
        i += 1;
    }
    if n < 2 {
        return -1;
    }
    let rbsp = &rbsp[..n];
    let mut bit = 0usize;
    let mut read_ue = || -> i64 {
        let mut zeros = 0i64;
        while bit < rbsp.len() * 8 {
            let b = (rbsp[bit >> 3] >> (7 - (bit & 7))) & 1;
            bit += 1;
            if b == 1 {
                break;
            }
            zeros += 1;
        }
        let mut val = 0i64;
        for _ in 0..zeros {
            let b = (rbsp[bit >> 3] >> (7 - (bit & 7))) & 1;
            bit += 1;
            val = (val << 1) | b as i64;
        }
        (1i64 << zeros) - 1 + val
    };
    read_ue(); // first_mb_in_slice
    read_ue() // slice_type
}
