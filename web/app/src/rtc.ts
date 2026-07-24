/**
 * 浏览器原生 WebRTC 封装（Viewer 侧）。
 *
 * 与 rdcore-rtc（webrtc-rs Host）对齐的通道规划：
 * - `media`   negotiated id=0 → 屏幕视频帧（SCTP，16 KiB 分片由 WASM 管线重组）。
 * - `control` negotiated id=1 → 输入 / 剪贴板 / 心跳 / 会话密钥交换。
 * - `audio`   negotiated id=2 → 设备音频（与视频独立、互不阻塞）。
 * 三条均 ordered: true —— 分片重组依赖严格按序到达。
 *
 * 注意（来自 rdcore-app 的线格式事实）：ICE 候选的 `candidate` 字段承载
 * 整段 JSON 序列化的 RTCIceCandidateInit（含 usernameFragment），对端 serde_json
 * 还原后加入连接 —— 因此本侧 trickle 候选也按 JSON 整段发出。
 */

export type RtcHandler = {
  /** 本地 ICE 候选（trickle）：参数为 JSON 序列化后的 candidate init 字符串。 */
  onIceCandidate(candidateJson: string, sdpMid: string | null, sdpMLineIndex: number | null): void;
  onMediaMessage(bytes: Uint8Array): void;
  onControlMessage(bytes: Uint8Array): void;
  onAudioMessage?(bytes: Uint8Array): void;
  onControlOpen?(): void;
  onStateChange?(state: RTCPeerConnectionState): void;
};

export const MEDIA_CHANNEL_ID = 0;
export const CONTROL_CHANNEL_ID = 1;
export const AUDIO_CHANNEL_ID = 2;

export class RtcPeer {
  private pc: RTCPeerConnection;
  readonly media: RTCDataChannel;
  readonly control: RTCDataChannel;
  readonly audio: RTCDataChannel;

  private constructor(pc: RTCPeerConnection, media: RTCDataChannel, control: RTCDataChannel, audio: RTCDataChannel) {
    this.pc = pc;
    this.media = media;
    this.control = control;
    this.audio = audio;
  }

  static async create(handler: RtcHandler, iceServers: RTCIceServer[] = [{ urls: 'stun:stun.l.google.com:19302' }]): Promise<RtcPeer> {
    const pc = new RTCPeerConnection({ iceServers });
    const mk = (label: string, id: number): RTCDataChannel =>
      pc.createDataChannel(label, { negotiated: true, id, ordered: true });
    const media = mk('media', MEDIA_CHANNEL_ID);
    const control = mk('control', CONTROL_CHANNEL_ID);
    const audio = mk('audio', AUDIO_CHANNEL_ID);
    for (const dc of [media, control, audio]) dc.binaryType = 'arraybuffer';

    media.onmessage = (ev) => handler.onMediaMessage(new Uint8Array(ev.data));
    control.onmessage = (ev) => handler.onControlMessage(new Uint8Array(ev.data));
    audio.onmessage = (ev) => handler.onAudioMessage?.(new Uint8Array(ev.data));
    control.onopen = () => handler.onControlOpen?.();
    // 通道状态留痕：生产排障（open/close 时序 + 未开即发）在 console 可见。
    for (const dc of [media, control, audio]) {
      dc.onclose = () => console.log(`[rtc] ${dc.label}(${dc.id}) closed`);
      dc.onerror = (e) => console.error(`[rtc] ${dc.label}(${dc.id}) error:`, e);
    }
    media.onopen = () => console.log('[rtc] media(0) open');
    control.onopen = () => {
      console.log('[rtc] control(1) open');
      handler.onControlOpen?.();
    };
    audio.onopen = () => console.log('[rtc] audio(2) open');
    pc.onconnectionstatechange = () => handler.onStateChange?.(pc.connectionState);
    pc.onicecandidate = (ev) => {
      if (ev.candidate) {
        handler.onIceCandidate(
          JSON.stringify(ev.candidate.toJSON()),
          ev.candidate.sdpMid,
          ev.candidate.sdpMLineIndex,
        );
      }
    };
    return new RtcPeer(pc, media, control, audio);
  }

  /** Viewer 侧：生成本地 Offer SDP（签名后经信令发出）。 */
  async createOffer(): Promise<string> {
    const offer = await this.pc.createOffer();
    await this.pc.setLocalDescription(offer);
    return offer.sdp ?? '';
  }

  /** Viewer 侧：接受 Host 的 Answer SDP。 */
  async acceptAnswer(sdp: string): Promise<void> {
    await this.pc.setRemoteDescription({ type: 'answer', sdp });
  }

  /** 加入对端 trickle 来的 ICE 候选（candidate 为 JSON 字符串，与线格式一致）。 */
  async addIceCandidate(candidateJson: string): Promise<void> {
    try {
      await this.pc.addIceCandidate(JSON.parse(candidateJson));
    } catch {
      // remote description 未就绪或候选损坏：Host 侧同样缓冲/忽略，这里忽略。
    }
  }

  close(): void {
    this.pc.close();
  }

  /** 连接统计（链路 RTT / 吞吐 / 候选类型诊断用）。 */
  getStats(): Promise<RTCStatsReport> {
    return this.pc.getStats();
  }

  /** 诊断快照：连接/ICE/信令状态 + 三条通道 readyState（生产排障用）。 */
  debugState(): {
    connection: RTCPeerConnectionState;
    iceConnection: RTCIceConnectionState;
    iceGathering: RTCIceGatheringState;
    signaling: RTCSignalingState;
    channels: Record<string, RTCDataChannelState>;
  } {
    return {
      connection: this.pc.connectionState,
      iceConnection: this.pc.iceConnectionState,
      iceGathering: this.pc.iceGatheringState,
      signaling: this.pc.signalingState,
      channels: {
        media: this.media.readyState,
        control: this.control.readyState,
        audio: this.audio.readyState,
      },
    };
  }
}
