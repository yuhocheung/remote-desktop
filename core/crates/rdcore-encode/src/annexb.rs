//! AVCC（长度前缀）↔ Annex-B（起始码）转换与 NAL 工具（纯逻辑、无平台依赖）。
//!
//! 背景：Media Foundation / NVENC / AMF 等硬件 H.264 编码器通常产出 **AVCC** 格式
//! （每个 NALU 前 4 字节大端长度前缀，SPS/PPS 在独立的 `AVCDecoderConfigurationRecord` 里）；
//! 而本系统的 RTP 打包器（`TrackLocalStaticSample` → webrtc-rs `H264Payloader`，见
//! `rdcore-rtc/src/h264_rtp.rs`）与 Viewer 软解端（openh264）均以 **Annex-B**
//! （`00 00 00 01` 起始码）为契约。故硬件后端必须把 AVCC 转成 Annex-B，才能让
//! `MediaFrame.data` 与现有软编字节级同构、对 Viewer 与 RTP 路径完全透明。

/// Annex-B 4 字节起始码（`00 00 00 01`）。
pub const ANNEX_B_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// 把一个 AVCC 样本（含若干“4 字节长度前缀 + NALU”片段）转换为 Annex-B 字节流。
///
/// - `avcc`：编码器单次 `ProcessOutput` 产出的样本（AVCC 长度前缀布局）。
/// - `sps_pps_annexb`：可选，已转换为 Annex-B 的 SPS/PPS 头，会原样前置到输出之前
///   （远程桌面场景下每帧前置 SPS/PPS，保证任意帧独立可解，对齐软编“每帧 IDR”语义）。
///
/// 长度前缀若越界（损坏流）则停止解析，已解析部分仍返回，避免 panic。
pub fn avcc_sample_to_annexb(avcc: &[u8], sps_pps_annexb: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + sps_pps_annexb.map_or(0, |e| e.len()) + 16);
    if let Some(head) = sps_pps_annexb {
        out.extend_from_slice(head);
    }
    let mut i = 0usize;
    while i + 4 <= avcc.len() {
        let nalu_len =
            u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if nalu_len == 0 {
            continue; // 防御：跳过空 NALU，避免死循环 / 吞掉后续数据。
        }
        if i + nalu_len > avcc.len() {
            break; // 越界，流损坏，停止。
        }
        out.extend_from_slice(&ANNEX_B_START_CODE);
        out.extend_from_slice(&avcc[i..i + nalu_len]);
        i += nalu_len;
    }
    out
}

/// 从 AVCC `AVCDecoderConfigurationRecord`（extradata）抽取 SPS/PPS，拼成 Annex-B 头。
///
/// 标准布局（ISO/IEC 14496-15）：
/// ```text
/// [0] configurationVersion (=1)
/// [1] AVCProfileIndication
/// [2] profile_compatibility
/// [3] AVCLevelIndication
/// [4] 6 bits reserved(=111111) + 2 bits lengthSizeMinusOne
/// [5] 3 bits reserved(=111) + 5 bits numOfSequenceParameterSets
/// SPS: [2 bytes length][sps NALU] ... (numOfSPS 个)
/// [1] numOfPictureParameterSets
/// PPS: [2 bytes length][pps NALU] ... (numOfPPS 个)
/// ```
pub fn sps_pps_from_avcc_extradata(extradata: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // 最小长度：8 字节头 + 至少 1 个 SPS（2 字节长度）才有意义。
    if extradata.len() < 8 {
        return out;
    }
    let num_sps = (extradata[5] & 0x1f) as usize;
    let mut i = 6usize;
    for _ in 0..num_sps {
        if i + 2 > extradata.len() {
            return out;
        }
        let len = u16::from_be_bytes([extradata[i], extradata[i + 1]]) as usize;
        i += 2;
        if i + len > extradata.len() {
            return out;
        }
        out.extend_from_slice(&ANNEX_B_START_CODE);
        out.extend_from_slice(&extradata[i..i + len]);
        i += len;
    }
    if i >= extradata.len() {
        return out;
    }
    let num_pps = extradata[i] as usize;
    i += 1;
    for _ in 0..num_pps {
        if i + 2 > extradata.len() {
            return out;
        }
        let len = u16::from_be_bytes([extradata[i], extradata[i + 1]]) as usize;
        i += 2;
        if i + len > extradata.len() {
            return out;
        }
        out.extend_from_slice(&ANNEX_B_START_CODE);
        out.extend_from_slice(&extradata[i..i + len]);
        i += len;
    }
    out
}

/// 判断 NALU header 的 `nal_unit_type`（低 5 位）。
#[allow(dead_code)]
pub fn nal_unit_type(annexb_nal: &[u8]) -> Option<u8> {
    // 跳过起始码。
    let mut i = 0;
    while i + 1 < annexb_nal.len() && annexb_nal[i] == 0 {
        i += 1;
    }
    if i < annexb_nal.len() && annexb_nal[i] == 1 {
        i += 1; // 越过起始码的 0x01
    }
    if annexb_nal.len() <= i {
        return None;
    }
    Some(annexb_nal[i] & 0x1f)
}

/// 统计 Annex-B 流里起始码数量（用于测试 / 校验）。
#[allow(dead_code)]
pub fn count_start_codes(annexb: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + 3 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 0 && annexb[i + 3] == 1 {
            count += 1;
            i += 4;
        } else {
            i += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_single_nalu_to_annexb() {
        // 长度前缀 4 字节（小端表示值 3）+ NALU [0x65,0x01,0x02]
        let mut avcc = vec![0, 0, 0, 3];
        avcc.extend_from_slice(&[0x65, 0x01, 0x02]);
        let out = avcc_sample_to_annexb(&avcc, None);
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 0x01, 0x02]);
    }

    #[test]
    fn avcc_multiple_nalus_to_annexb() {
        // SPS(len4) + PPS(len2) + IDR(len3)
        let mut avcc = vec![];
        avcc.extend_from_slice(&[0, 0, 0, 4]);
        avcc.extend_from_slice(&[0x67, 0x42, 0x00, 0x1e]);
        avcc.extend_from_slice(&[0, 0, 0, 2]);
        avcc.extend_from_slice(&[0x68, 0xce]);
        avcc.extend_from_slice(&[0, 0, 0, 3]);
        avcc.extend_from_slice(&[0x65, 0x09, 0x10]);
        let out = avcc_sample_to_annexb(&avcc, None);
        // 期望 3 个起始码 + 三个 NALU 体
        assert_eq!(count_start_codes(&out), 3);
        assert!(out.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(out.ends_with(&[0x65, 0x09, 0x10]));
    }

    #[test]
    fn prepend_sps_pps_head() {
        let sps_pps = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, 0, 0, 0, 1, 0x68, 0xce];
        let mut avcc = vec![0, 0, 0, 3, 0x65, 0x01, 0x02];
        let out = avcc_sample_to_annexb(&avcc, Some(&sps_pps));
        assert!(out.starts_with(&sps_pps));
        assert_eq!(count_start_codes(&out), 3); // sps + pps + slice
    }

    #[test]
    fn sps_pps_from_extradata() {
        // 构造最小 AVCDecoderConfigurationRecord：
        // ver=1, prof=0x42, compat=0x00, level=0x1e,
        // lengthSizeMinusOne=0xFF => 高6位 reserved + 低2位=3,
        // numSPS=0xE1 => 高3位 reserved + 低5位=1,
        // SPS len=4 [0x67,0x42,0x00,0x1e], numPPS=1, PPS len=2 [0x68,0xce]
        let mut ex = vec![1, 0x42, 0x00, 0x1e, 0xFF, 0xE1];
        ex.extend_from_slice(&[0, 4]);
        ex.extend_from_slice(&[0x67, 0x42, 0x00, 0x1e]);
        ex.push(1);
        ex.extend_from_slice(&[0, 2]);
        ex.extend_from_slice(&[0x68, 0xce]);
        let head = sps_pps_from_avcc_extradata(&ex);
        assert_eq!(count_start_codes(&head), 2);
        assert!(head.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(head.ends_with(&[0x68, 0xce]));
    }

    #[test]
    fn truncated_avcc_does_not_panic() {
        // 声明长度 10 但实际只有 2 字节 NALU 数据：越界 NALU 应被安全丢弃（输出为空），不越界、不 panic。
        let avcc = vec![0, 0, 0, 10, 0x65, 0x01];
        let out = avcc_sample_to_annexb(&avcc, None);
        assert!(out.is_empty(), "截断的 NALU 应被丢弃");
    }

    #[test]
    fn nal_unit_type_detection() {
        let idr = vec![0, 0, 0, 1, 0x65, 0x01]; // type 5
        let sps = vec![0, 0, 0, 1, 0x67, 0x42]; // type 7
        assert_eq!(nal_unit_type(&idr), Some(5));
        assert_eq!(nal_unit_type(&sps), Some(7));
    }
}
