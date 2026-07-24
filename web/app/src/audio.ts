/**
 * 音频播放（M3-C）：Worker 解出的 Raw PCM（16-bit 小端交错）→ AudioWorklet 环形缓冲播放。
 *
 * 与 `rdcore-app::recv_rendered_audio`（Raw 直通）对偶：Host 侧 `AudioFrame.codec = Raw`
 * 时 data 即 16-bit 交错 PCM，此处按 channels/sample_rate 去交错为 Float32 平面喂给
 * AudioWorklet。AudioContext 采样率与流采样率不一致时由 WebAudio 重采样（探针恒发
 * 48kHz，Chrome 默认输出多为此值，直通）。
 *
 * 自动播放策略：AudioContext 需用户手势激活；`resume()` 在「连接」按钮点击时调用，
 * smoke 则以 `--autoplay-policy=no-user-gesture-required` 免手势。
 *
 * 探针（smoke 断言用，挂在 window.__audio）：
 * - frames：已收到的音频帧数；rms：已收信号的最大 RMS（440Hz 正弦 × 0.5 振幅 ≈ 0.35）；
 * - sampleRate/channels：首帧参数；playedBlocks：Worklet 实际输出过非静音的块数
 *   （证明播放链路而非仅接收链路工作）。
 */

/** AudioWorklet 处理器源码（Blob URL 加载，避免额外静态资源路径）。 */
const WORKLET_SOURCE = `
class PcmPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.capacity = 48000 * 2; // 每声道 1 秒环形缓冲
    this.buf = null;           // Float32Array[]（按声道而建，首帧到达时分配）
    this.readPos = 0;
    this.writePos = 0;
    this.port.onmessage = (ev) => {
      const { channels, pcm } = ev.data; // pcm: ArrayBuffer[]（每声道 Float32）
      if (!this.buf || this.buf.length !== channels) {
        this.buf = Array.from({ length: channels }, () => new Float32Array(this.capacity));
        this.readPos = this.writePos = 0;
      }
      const n = pcm[0].length;
      for (let i = 0; i < n; i++) {
        for (let c = 0; c < channels; c++) this.buf[c][this.writePos] = pcm[c][i];
        this.writePos = (this.writePos + 1) % this.capacity;
        // 写追上读（缓冲溢出）：丢弃最旧采样（保延迟优先于保连续）。
        if (this.writePos === this.readPos) this.readPos = (this.readPos + 1) % this.capacity;
      }
    };
  }
  process(_inputs, outputs) {
    const out = outputs[0];
    const n = out[0].length;
    let nonSilent = false;
    for (let i = 0; i < n; i++) {
      let any = false;
      let v0 = 0;
      if (this.buf && this.readPos !== this.writePos) {
        any = true;
        for (let c = 0; c < out.length; c++) {
          const ch = Math.min(c, this.buf.length - 1);
          const v = this.buf[ch][this.readPos];
          out[c][i] = v;
          if (c === 0) v0 = v;
        }
        this.readPos = (this.readPos + 1) % this.capacity;
      }
      if (!any) for (let c = 0; c < out.length; c++) out[c][i] = 0;
      if (any && Math.abs(v0) > 1e-4) nonSilent = true;
    }
    if (nonSilent) this.port.postMessage({ played: 1 });
    return true;
  }
}
registerProcessor('pcm-player', PcmPlayer);
`;

export type AudioFrameMsg = { channels: number; sampleRate: number; pcm: ArrayBuffer };

export class AudioPlayer {
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private gain: GainNode | null = null;
  private ready = false;
  private pending: AudioFrameMsg[] = [];
  /** 静音开关状态（UI 展示；默认不静音）。 */
  muted = false;

  /** 惰性初始化（首次音频帧或用户手势触发）；重复调用幂等。 */
  private async ensure(): Promise<void> {
    if (this.ready) return;
    if (!this.ctx) {
      this.ctx = new AudioContext({ sampleRate: 48000 });
      const url = URL.createObjectURL(new Blob([WORKLET_SOURCE], { type: 'text/javascript' }));
      try {
        await this.ctx.audioWorklet.addModule(url);
      } finally {
        URL.revokeObjectURL(url);
      }
      this.node = new AudioWorkletNode(this.ctx, 'pcm-player', {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [2],
      });
      this.gain = this.ctx.createGain();
      this.node.connect(this.gain).connect(this.ctx.destination);
      this.node.port.onmessage = () => {
        window.__audio.playedBlocks += 1;
      };
    }
    this.ready = true;
    // 排空初始化前到达的帧。
    for (const m of this.pending.splice(0)) this.feed(m);
  }

  /** 用户手势时机调用（连接按钮 / 音频开关点击）：解除自动播放限制。 */
  resume(): void {
    void this.ensure().then(() => this.ctx?.resume());
  }

  /** 静音切换（增益 0/1；返回新状态）。 */
  toggleMute(): boolean {
    this.muted = !this.muted;
    if (this.gain) this.gain.gain.value = this.muted ? 0 : 1;
    window.__audio.muted = this.muted;
    return this.muted;
  }

  /** 喂入一帧 Worker 解出的 Raw PCM（16-bit LE 交错）。 */
  onFrame(msg: AudioFrameMsg): void {
    const view = new Int16Array(msg.pcm);
    const ch = Math.max(1, msg.channels);
    const samples = Math.floor(view.length / ch);
    // 接收侧探针：RMS 用原始 int16 计算（与播放链路独立，证明 E2E 解密正确）。
    let sum = 0;
    for (let i = 0; i < samples * ch; i++) {
      const v = view[i] / 32768;
      sum += v * v;
    }
    const rms = samples > 0 ? Math.sqrt(sum / (samples * ch)) : 0;
    const probe = window.__audio;
    probe.frames += 1;
    if (rms > probe.rms) probe.rms = Math.round(rms * 1000) / 1000;
    probe.sampleRate = msg.sampleRate;
    probe.channels = msg.channels;

    if (!this.ready) {
      // AudioContext 尚未建好（等首次手势）：缓冲少量帧，超出丢弃（保延迟）。
      if (this.pending.length < 100) this.pending.push(msg);
      void this.ensure().catch(() => {});
      return;
    }
    this.feed(msg);
  }

  /** 去交错 → Float32 平面 → 投递给 Worklet（转移所有权，零拷贝）。 */
  private feed(msg: AudioFrameMsg): void {
    if (!this.node || !this.ctx) return;
    const view = new Int16Array(msg.pcm);
    const ch = Math.max(1, msg.channels);
    const samples = Math.floor(view.length / ch);
    if (samples === 0) return;
    const planar: Float32Array[] = [];
    for (let c = 0; c < ch; c++) {
      const buf = new Float32Array(samples);
      for (let i = 0; i < samples; i++) buf[i] = view[i * ch + c] / 32768;
      planar.push(buf);
    }
    this.node.port.postMessage(
      { channels: ch, pcm: planar.map((b) => b.buffer) },
      planar.map((b) => b.buffer),
    );
  }

  /** 会话结束：释放 AudioContext（下个会话重建）。 */
  dispose(): void {
    this.ready = false;
    this.pending.length = 0;
    const ctx = this.ctx;
    this.ctx = null;
    this.node = null;
    this.gain = null;
    void ctx?.close().catch(() => {});
  }
}

/** 音频探针（smoke 断言读取；main.ts 初始化）。 */
declare global {
  interface Window {
    __audio: {
      frames: number;
      rms: number;
      sampleRate: number;
      channels: number;
      playedBlocks: number;
      muted: boolean;
    };
  }
}
