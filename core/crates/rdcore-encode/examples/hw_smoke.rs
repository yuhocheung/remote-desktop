//! 真机冒烟测试：验证 Host 硬件 H.264 编码是否「真正落地」。
//!
//! 跑法（Windows，带 GPU）：
//!   cargo run --release --example hw_smoke --features hwcodec
//! 跑法（默认/无 GPU，仅验证回退链路）：
//!   cargo run --release --example hw_smoke
//!
//! 退出码：0 = 编码链路正常（HW 或 SW）；1 = 编码异常；2 = 构造失败。
//!
//! 本测试不依赖网络/显示器：用合成渐变 + 移动方块帧喂编码器，校验
//! 每帧输出是否以 Annex-B 起始码 `00 00 00 01` 开头（与 RTP/Viewer 字节契约一致）。

use rdcore_encode::capability::detect_hw_encoders;
use rdcore_encode::new_encoder_with_fps;
use rdcore_proto::{MediaFrame, VideoCodec};

const W: u32 = 1280;
const H: u32 = 720;
const NFRAMES: u32 = 30;

fn main() {
    println!("=== rdcore Host 硬件编码 真机冒烟测试 ===");
    println!("目标分辨率: {}x{}，编码 {} 帧合成画面\n", W, H, NFRAMES);

    // [1] 探测本机硬件编码器（LoadLibrary 探厂商 DLL + MFTEnum2 枚举硬件 MFT，用于选 ffmpeg 编码器名）
    println!("[1] 探测本机硬件编码器 ...");
    let hw = detect_hw_encoders();
    if hw.is_empty() {
        println!("    未发现硬件编码器 → 将走 openh264 软编回退");
    } else {
        for k in &hw {
            println!("    发现: {}", k.as_str());
        }
    }

    // [2] 经工厂 new_encoder_with_fps(H264, 60fps) 构造编码器（硬件优先、软编回退）
    println!("\n[2] 经工厂 new_encoder_with_fps(H264, {}x{}, 60fps) 构造编码器 ...", W, H);
    let enc = match new_encoder_with_fps(VideoCodec::H264, W, H, 60) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("    构造失败: {err}");
            std::process::exit(2);
        }
    };
    let kind = enc.kind();
    println!("    实际后端 kind = {kind}");
    let is_hw = kind == "h264-hardware";

    // [3] 编码合成帧并校验 Annex-B 契约
    println!("\n[3] 编码 {NFRAMES} 帧（渐变背景 + 移动方块）...");
    let mut total_bytes = 0usize;
    let mut ok_frames = 0u32;
    let mut first_frame_len = 0usize;
    for i in 0..NFRAMES {
        let frame = make_frame(i);
        match enc.encode(&frame) {
            Ok(out) => {
                if out.data.starts_with(&[0, 0, 0, 1]) {
                    ok_frames += 1;
                    if i == 0 {
                        first_frame_len = out.data.len();
                    }
                } else {
                    eprintln!("    帧 {i} 缺少 Annex-B 起始码（00 00 00 01）！");
                }
                total_bytes += out.data.len();
            }
            Err(e) => {
                eprintln!("    帧 {i} 编码失败: {e}");
            }
        }
    }
    let avg = if ok_frames > 0 {
        total_bytes / ok_frames as usize
    } else {
        0
    };
    println!(
        "    成功 {ok_frames}/{NFRAMES} 帧，首帧 {first_frame_len} 字节，每帧均 {avg} 字节，合计 {total_bytes} 字节"
    );

    // [4] 结论
    println!("\n[4] 结论");
    if is_hw && ok_frames == NFRAMES {
        println!("    ✅ 硬件编码真实生效（kind=h264-hardware，全部帧 Annex-B 合法）");
        println!("       说明：Host 已在生产路径使用 GPU 硬件编码器（NVENC/QSV/AMF 之一）。");
    } else if !is_hw && ok_frames == NFRAMES {
        println!("    ⚠️ 回退到软编（kind={kind}）。编码链路正常，但本机未走 GPU 加速。");
        if hw.is_empty() {
            println!("       原因：未探测到硬件编码器。若期望硬编，请在带 NVENC/QSV/AMF 的");
            println!("             Windows 上运行（且开启 --features hwcodec）。");
        } else {
            println!("       已探到硬件编码器但 FFmpeg 后端初始化失败（GPU/驱动/ffmpeg 构建），已安全回退。");
        }
    } else {
        println!("    ❌ 编码异常：{ok_frames}/{NFRAMES} 帧合法，请检查上方错误日志。");
        std::process::exit(1);
    }
}

/// 生成一帧合成 RGBA 画面：渐变背景 + 随时间移动的红色方块（制造帧间变化，逼出运动编码）。
fn make_frame(t: u32) -> MediaFrame {
    let npix = (W as usize) * (H as usize);
    let mut data = vec![0u8; npix * 4];
    let bx = ((t as usize * 20) % (W as usize).saturating_sub(80)).max(0);
    let by = ((t as usize * 12) % (H as usize).saturating_sub(60)).max(0);
    for y in 0..H as usize {
        for x in 0..W as usize {
            let idx = (y * W as usize + x) * 4;
            let r = ((x as u32 * 255 / W) as u8).wrapping_add((t * 3) as u8);
            let g = ((y as u32 * 255 / H) as u8).wrapping_add((t * 5) as u8);
            let b = (((x + y) as u32 * 255 / (W + H)) as u8).wrapping_add((t * 7) as u8);
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
        width: W,
        height: H,
        data,
    }
}
