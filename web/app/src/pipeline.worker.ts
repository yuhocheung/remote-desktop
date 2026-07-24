/**
 * 帧管线 Worker：WASM 重组/解密 + WebCodecs 解码 + OffscreenCanvas 绘制。
 *
 * 媒体面数据流（与 Host 的 rdcore-app::send_media 严格对偶）：
 *   RTCDataChannel(media) 二进制消息
 *     → FramePipeline.push_sctp_message（1 字节分片标签重组 + 去 4 字节长度前缀）
 *     → postcard(MediaFrame)，其中 data = postcard(Ciphertext)
 *     → XChaCha20Poly1305 解密（会话密钥来自握手）
 *     → H.264 Annex-B → WebCodecs VideoDecoder → OffscreenCanvas
 *
 * 控制面数据流（与 recv_app 对偶）：
 *   RTCDataChannel(control) → 重组 → postcard(Message::Encrypted)
 *     → 解密 → postcard(AppMessage) → JSON 回主线程（Consent / Heartbeat / Revoke 等）。
 */
import init, * as rdweb from '@rdcore/rdcore_web.js';
import wasmUrl from '@rdcore/rdcore_web_bg.wasm?url';

type WorkerIn =
  | { type: 'init'; probe?: boolean }
  | { type: 'canvas'; canvas: OffscreenCanvas }
  | { type: 'reset' }
  | { type: 'session_key'; key: Uint8Array }
  | { type: 'media'; bytes: Uint8Array }
  | { type: 'control'; bytes: Uint8Array }
  | { type: 'audio'; bytes: Uint8Array };

const CONTROL_MAX_LEN = 8 * 1024 * 1024; // 与 rdcore-media::MAX_DATA_FRAME_LEN 一致
const AUDIO_MAX_LEN = 256 * 1024; // 与 rdcore-media::MAX_AUDIO_FRAME_LEN 一致

let mediaPipe: rdweb.FramePipeline | null = null;
let controlPipe: rdweb.FramePipeline | null = null;
let audioPipe: rdweb.FramePipeline | null = null;
let decoder: VideoDecoder | null = null;
/** 解码器是否已收到首个 IDR（FFmpeg 硬编首组 SPS/PPS 独立帧不含 VCL 的兼容处理）。 */
let gotKeyFrame = false;
let ctx: OffscreenCanvasRenderingContext2D | null = null;
let sessionKey: Uint8Array | null = null;
/**
 * 密钥先于 Worker 就绪窗口内的加密控制消息暂存队列（release 时序竞争修复）。
 *
 * 背景：Host 在密钥响应后数微秒内即发 consent（release 构建几乎背靠背），而本 Worker
 * 拿到密钥需走完「control_plain → 主线程握手 → postMessage session_key」一个以上的
 * 宏任务周期——窗口内到达的加密消息解密必败。此前直接按明文误投即静默丢弃，
 * 表现为「视频正常但 consent 丢失、输入未授权」（仅 release Host 复现，debug 慢速
 * 让出窗口故不现）。现改为：密钥未到时解密失败的消息除按明文投递外同时入队，
 * 密钥就绪后重放。上限 64 条防密钥永远不到时的无界堆积。
 */
let pendingPreKey: Uint8Array[] = [];
const PENDING_PRE_KEY_CAP = 64;

// ── 画面统计探针（仅 smoke 用）：把帧缩采样到 32×18 网格，算亮度均值/标准差/帧差 ──
// ⚠ 仅在页面 URL 带 ?probe=1 时启用（主线程经 init 消息传入——worker 自己的
// location 是脚本 URL，不带页面查询串）：getImageData 在部分平台（Mac Chrome 等）
// 会强制 GPU→CPU 同步读回，每帧一次足以拖垮解码管线；生产默认关闭。
let probeEnabled = false;
const SAMPLE_W = 32;
const SAMPLE_H = 18;
let sampleCtx: OffscreenCanvasRenderingContext2D | null = null;
let prevLuma: Float32Array | null = null;

/** 对当前解码帧做缩采样统计：mean=平均亮度(0..255)，std=空间亮度标准差，diff=与上一帧的平均亮度差。 */
function frameStats(frame: VideoFrame): { lum: number; std: number; diff: number } {
  if (!probeEnabled) return { lum: -1, std: -1, diff: -1 };
  try {
    if (!sampleCtx) {
      sampleCtx = new OffscreenCanvas(SAMPLE_W, SAMPLE_H).getContext('2d', {
        willReadFrequently: true,
      });
    }
    if (!sampleCtx) return { lum: -1, std: -1, diff: -1 };
    sampleCtx.drawImage(frame, 0, 0, SAMPLE_W, SAMPLE_H);
    const d = sampleCtx.getImageData(0, 0, SAMPLE_W, SAMPLE_H).data;
    const n = SAMPLE_W * SAMPLE_H;
    const cur = new Float32Array(n);
    let sum = 0;
    for (let i = 0; i < n; i++) {
      const o = i * 4;
      cur[i] = 0.299 * d[o] + 0.587 * d[o + 1] + 0.114 * d[o + 2];
      sum += cur[i];
    }
    const mean = sum / n;
    let varSum = 0;
    let diffSum = 0;
    for (let i = 0; i < n; i++) {
      varSum += (cur[i] - mean) * (cur[i] - mean);
      if (prevLuma) diffSum += Math.abs(cur[i] - prevLuma[i]);
    }
    const std = Math.sqrt(varSum / n);
    const diff = prevLuma ? diffSum / n : 0;
    prevLuma = cur;
    return { lum: Math.round(mean * 10) / 10, std: Math.round(std * 10) / 10, diff: Math.round(diff * 100) / 100 };
  } catch {
    return { lum: -1, std: -1, diff: -1 }; // 统计失败不阻断渲染
  }
}

/** 处理一条完整的控制面 payload：解密 → AppMessage / FileTransfer 分发。 */
function handleControlPayload(payload: Uint8Array): void {
  // 会话密钥交换消息是明文 Message::SessionKey，未加密——先尝试按 Encrypted 解，
  // 失败则按明文信令交给主线程（握手期的 SessionKey 消息）。
  let plain: Uint8Array;
  try {
    plain = controlPipe!.decrypt_control_message(payload);
  } catch {
    postMessage({ type: 'control_plain', bytes: payload });
    return;
  }
  // 内层两种格式：AppMessage（consent/clipboard/heartbeat/…，rdcore-app 格式）
  // 或 Message::FileTransfer（文件传输，rdcore-ffi Track B 格式；其变体下标 8
  // 超出 AppMessage 的 0..=4，按 AppMessage 解析必失败，据此区分）。
  try {
    postMessage({ type: 'app', json: rdweb.app_message_to_json(plain) });
  } catch (e1) {
    try {
      postMessage({ type: 'file', json: rdweb.file_message_to_json(plain) });
    } catch (e2) {
      postMessage({ type: 'error', message: `未知控制消息: ${e1} / ${e2}` });
    }
  }
}

/** Annex-B 码流是否含 IDR（NAL type 5）。逐帧扫描，几 KB 开销可忽略。 */
function annexbHasIdr(data: Uint8Array): boolean {
  for (let i = 0; i + 4 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0) {
      let hdr = -1;
      if (data[i + 2] === 1) hdr = data[i + 3];
      else if (data[i + 2] === 0 && data[i + 3] === 1) hdr = data[i + 4];
      if (hdr >= 0 && (hdr & 0x1f) === 5) return true;
    }
  }
  return false;
}

/**
 * Annex-B 帧是否含 I-slice（slice_type 2/7）——**非 IDR 的 I 帧**检测。
 * NVENC「逐帧强制 I」输出的正是这类帧（NAL type=1 但全帧 intra）：独立可解，
 * 与 IDR 一样可作 WebCodecs 'key' 提交与丢帧恢复点，但编码开销只有 IDR 的一半
 * （IDR 需插 SPS/PPS + 重置参考缓冲，实测 82 vs 45fps）。据此 Host 无需 forced-idr。
 */
function annexbHasIntraSlice(data: Uint8Array): boolean {
  for (let i = 0; i + 4 < data.length; i++) {
    if (data[i] !== 0 || data[i + 1] !== 0) continue;
    let hdr = -1;
    let payload = -1;
    if (data[i + 2] === 1) {
      hdr = data[i + 3];
      payload = i + 4;
    } else if (data[i + 2] === 0 && data[i + 3] === 1) {
      hdr = data[i + 4];
      payload = i + 5;
    }
    if (hdr < 0 || payload >= data.length) continue;
    const nalType = hdr & 0x1f;
    if (nalType === 5) return true; // IDR 本身即全 I-slice
    if (nalType !== 1) continue; // 只看非 IDR VCL
    // slice header：first_mb_in_slice ue(v)，slice_type ue(v)（2/7 = 全 I）
    const st = readSliceType(data, payload);
    if (st === 2 || st === 7) return true;
  }
  return false;
}

/** 读 slice header 的 slice_type：RBSP（剥 00 00 03 防竞争字节）上两个 ue(v)。 */
function readSliceType(data: Uint8Array, start: number): number {
  const rbsp: number[] = [];
  for (let i = start; i < Math.min(start + 32, data.length) && rbsp.length < 24; i++) {
    if (i >= start + 2 && data[i - 2] === 0 && data[i - 1] === 0 && data[i] === 3) continue;
    rbsp.push(data[i]);
  }
  let bit = 0;
  const readUe = (): number => {
    let zeros = 0;
    while (bit < rbsp.length * 8) {
      const b = (rbsp[bit >> 3] >> (7 - (bit & 7))) & 1;
      bit++;
      if (b === 1) break;
      zeros++;
    }
    let val = 0;
    for (let k = 0; k < zeros; k++) {
      const b = (rbsp[bit >> 3] >> (7 - (bit & 7))) & 1;
      bit++;
      val = (val << 1) | b;
    }
    return (1 << zeros) - 1 + val;
  };
  if (rbsp.length < 2) return -1;
  readUe(); // first_mb_in_slice
  return readUe(); // slice_type
}

function ensureDecoder(): VideoDecoder {
  if (decoder && decoder.state === 'configured') return decoder;
  if (!decoder || decoder.state === 'closed') {
    decoder = new VideoDecoder({
      output: (frame) => {
        const stats = frameStats(frame);
        if (ctx) {
          // 尺寸仅变化时重设：每帧赋值会重置画布状态并强制重分配。
          if (ctx.canvas.width !== frame.displayWidth) ctx.canvas.width = frame.displayWidth;
          if (ctx.canvas.height !== frame.displayHeight) ctx.canvas.height = frame.displayHeight;
          ctx.drawImage(frame, 0, 0);
          postMessage({
            type: 'frame',
            width: frame.displayWidth,
            height: frame.displayHeight,
            lum: stats.lum,
            std: stats.std,
            diff: stats.diff,
            queue: decoder?.decodeQueueSize ?? 0,
            drops: dropEvents,
          });
        }
        frame.close();
      },
      error: (e) => {
        // 解码器报错（码流损坏 / 参考链断裂等）：重建后须以 key 重新起步，
        // 并进入等关键帧模式请 Host 发 IDR 快速恢复。
        gotKeyFrame = false;
        requestKeyFromHost('decode-error');
        postMessage({ type: 'error', message: `VideoDecoder: ${e.message}` });
      },
    });
  }
  // Host 编码器输出 Annex-B：config 不带 description → Annex-B 模式。
  // Level 5.2（0x34）：真实抓屏可达 3440×1440 甚至 4K，超出 L3.1 的 720p 上限。
  // （reset 后 state 回到 unconfigured，走这里重新 configure。）
  decoder.configure({ codec: 'avc1.42e034', optimizeForLatency: true });
  return decoder;
}

/**
 * 解码积压上限：排队待解帧超过该值即判定消费跟不上（弱机 / 大分辨率 / 高码率）。
 * 继续排队只会把端到端延迟越攒越大（画面卡成幻灯片的根源）。
 */
const DECODE_BACKLOG_MAX = 4;
/** 背压累计丢帧数（统计悬浮层 / 延迟诊断用）。 */
let dropEvents = 0;
/**
 * 等关键帧模式（P 帧流恢复核心）：背压丢帧 / 解码错误后进入——此后丢弃一切
 * delta 帧直到 key（IDR / I-slice 帧）到达；进入时经 `needkey` 消息请主线程
 * 向 Host 发 AppMessage::RequestKeyframe（PLI/FIR 语义），Host 下一帧即 IDR，
 * 配合其 1 秒周期 IDR 兜底，恢复延迟最坏 ~1s、典型 <100ms。
 */
let awaitingKey = false;
/** `needkey` 请求节流（避免背压抖动期每帧都发控制消息）。 */
let lastNeedkeyMs = 0;

/** 进入等关键帧模式并（按 300ms 节流）请求主线程向 Host 要关键帧。 */
function requestKeyFromHost(reason: string): void {
  awaitingKey = true;
  const now = performance.now();
  if (now - lastNeedkeyMs < 300) return;
  lastNeedkeyMs = now;
  postMessage({ type: 'needkey', reason });
}
self.onmessage = async (ev: MessageEvent<WorkerIn>) => {
  const msg = ev.data;
  try {
    switch (msg.type) {
      case 'init': {
        probeEnabled = msg.probe === true;
        await init({ module_or_path: wasmUrl });
        mediaPipe = new rdweb.FramePipeline();
        controlPipe = rdweb.FramePipeline.with_max_len(CONTROL_MAX_LEN);
        audioPipe = rdweb.FramePipeline.with_max_len(AUDIO_MAX_LEN);
        postMessage({ type: 'ready' });
        break;
      }
      case 'canvas': {
        ctx = msg.canvas.getContext('2d');
        break;
      }
      case 'reset': {
        // M2-B 断线重连：清空上一会话的管线/解码器/会话密钥，canvas 与 ctx 复用
        // （OffscreenCanvas 只能 transferControlToOffscreen 一次，Worker 必须复用）。
        if (decoder && decoder.state !== 'closed') decoder.close();
        decoder = null;
        mediaPipe = new rdweb.FramePipeline();
        controlPipe = rdweb.FramePipeline.with_max_len(CONTROL_MAX_LEN);
        audioPipe = rdweb.FramePipeline.with_max_len(AUDIO_MAX_LEN);
        sessionKey = null;
        prevLuma = null;
        gotKeyFrame = false; // 新会话重新等待首个 IDR
        dropEvents = 0;
        awaitingKey = false;
        lastNeedkeyMs = 0;
        pendingPreKey = []; // 上一会话的暂存消息随密钥一并作废
        postMessage({ type: 'reset_done' });
        break;
      }
      case 'session_key': {
        sessionKey = msg.key;
        mediaPipe?.set_session_key(sessionKey);
        controlPipe?.set_session_key(sessionKey);
        audioPipe?.set_session_key(sessionKey);
        postMessage({ type: 'session_key_set' });
        // 重放密钥就绪前暂存的加密消息（consent 等）：此刻能解密的才是真正排队的
        // 加密消息；仍解不开的是已被 control_plain 消费过的明文握手消息，丢弃。
        const queued = pendingPreKey;
        pendingPreKey = [];
        for (const payload of queued) {
          try {
            controlPipe?.decrypt_control_message(payload);
          } catch {
            continue; // 明文握手消息：已投递过，跳过
          }
          handleControlPayload(payload);
        }
        break;
      }
      case 'media': {
        if (!mediaPipe || !sessionKey) return;
        const payload = mediaPipe.push_sctp_message(msg.bytes);
        if (!payload) return; // 分片未凑齐
        // 二进制帧（生产路径）：[codec][w_lo][w_hi][h_lo][h_hi][data...]，
        // 一次 WASM→JS 拷贝，不再走 JSON + hex + parseInt 逐字节解码。
        const pkt = mediaPipe.decrypt_media_frame_bytes(payload);
        if (pkt[0] !== 0) return; // M2 起只接 H.264（Raw 仅回环调试）
        const data = pkt.subarray(5);
        // 关键帧判定：IDR（NAL type=5）或**非 IDR 的 I 帧**（I-slice，NVENC 逐帧
        // 强制 I 的形态）都独立可解，按 'key' 提交；仅 P/B（inter）走 delta。
        // 首帧门控同样以两者为准（FFmpeg 硬编首组 SPS/PPS 独立帧不含 VCL，
        // 贸然按 key 提交会触发 DataError: key frame required）。
        const isKey = annexbHasIdr(data) || annexbHasIntraSlice(data);
        // 解码背压：积压即丢帧保实时。P 帧流下任意丢帧（无论 I/P）都会破坏其后的
        // 参考链——故丢帧后立即进入等关键帧模式：丢后续 delta 直到 key 到达，并请
        // Host 立即发 IDR（典型恢复 <100ms；请求丢失由 Host 1 秒周期 IDR 兜底）。
        if (decoder && decoder.state === 'configured' && decoder.decodeQueueSize > DECODE_BACKLOG_MAX) {
          dropEvents++;
          requestKeyFromHost('backlog');
          return;
        }
        // 等关键帧模式：只放行 key，delta 一律丢弃（参考链已断，解出来也是花屏）。
        if (awaitingKey) {
          if (!isKey) {
            dropEvents++;
            return;
          }
          awaitingKey = false; // key 到达：参考链重建，恢复正常解码
        }
        if (!gotKeyFrame && !isKey) return;
        if (isKey) gotKeyFrame = true;
        ensureDecoder().decode(
          new EncodedVideoChunk({
            type: isKey ? 'key' : 'delta',
            timestamp: performance.now() * 1000,
            data,
          }),
        );
        break;
      }
      case 'control': {
        // ⚠ 绝不能以 sessionKey 为门槛：Host 的会话密钥响应就走这条通道，
        //    先丢包就死锁（密钥未到 → 丢控制帧 → 密钥永远到不了）。
        if (!controlPipe) return;
        const payload = controlPipe.push_sctp_message(msg.bytes);
        if (!payload) return;
        // 密钥未到且解密失败：除按明文投递（握手 SessionKey 路径）外，同时入队
        // 等待密钥就绪后重放——release Host 下 consent 会抢在 Worker 拿到密钥前
        // 到达（见 pendingPreKey 注释），不入队即静默丢失。
        if (!sessionKey) {
          try {
            controlPipe.decrypt_control_message(payload);
          } catch {
            if (pendingPreKey.length < PENDING_PRE_KEY_CAP) pendingPreKey.push(payload);
          }
        }
        handleControlPayload(payload);
        break;
      }
      case 'audio': {
        // 音频通道（M3-C）：与媒体面平行的第三条 SCTP 通道，线格式镜像
        // rdcore-app::recv_audio（postcard(AudioFrame)，data = postcard(Ciphertext)）。
        if (!audioPipe || !sessionKey) return;
        const payload = audioPipe.push_sctp_message(msg.bytes);
        if (!payload) return;
        // 二进制帧：[codec][ch_lo][ch_hi][rate u32 LE][data...]（codec 1=Raw）。
        const pkt = audioPipe.decrypt_audio_frame_bytes(payload);
        if (pkt[0] !== 1) return; // M3 只接 Raw PCM（Opus 解码不在本里程碑）
        const hdr = new DataView(pkt.buffer, pkt.byteOffset, 7);
        const channels = hdr.getUint16(1, true);
        const sampleRate = hdr.getUint32(3, true);
        const pcm = pkt.slice(7); // 独立缓冲，便于零拷贝转移给主线程
        postMessage(
          {
            type: 'audio',
            channels,
            sampleRate,
            pcm: pcm.buffer,
          },
          [pcm.buffer],
        );
        break;
      }
    }
  } catch (e) {
    postMessage({ type: 'error', message: String(e) });
  }
};

export {};
