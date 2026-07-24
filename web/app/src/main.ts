/**
 * 编排层：配对码 → 信令 → 身份 → 握手（PeerHello/签名 Offer/验签 Answer/ICE）
 * → 数据通道 open → 会话密钥交换 → 加密媒体/控制流 → 键鼠输入转发（M2-A）
 * → 断线自动重连（M2-B）。
 *
 * 镜像 rdcore-app::Connection::establish 的 Viewer 分支（浏览器实现）。
 *
 * M2-B 自动重连语义（与 Host 侧 wait_peer_gone_or_rescan / reconnect_with 对齐）：
 * - 触发源：PeerConnection failed/closed、disconnected 超 5s 宽限、信令 WS 关闭。
 * - 恢复：指数退避（1s→2s→4s→…上限 8s）后用**同一配对码**原地重连——
 *   新 WebHandshake（新 X25519 临时密钥，会话密钥指纹必然轮换）、新 PeerConnection、
 *   重走签名 Offer/consent/会话密钥交换，Host 按「重扫」原地接入，无需重启/换码。
 * - Host 主动撤销（revoke）与用户手动断开是终态：不自动重连。
 * - 每次 establish 递增 sessionId，陈旧会话的异步回调一律按 id 丢弃。
 */
import init, * as rdweb from '@rdcore/rdcore_web.js';
import wasmUrl from '@rdcore/rdcore_web_bg.wasm?url';
import { SignalingClient } from './signaling';
import { RtcPeer } from './rtc';
import { InputController, type SentInputRecord } from './input';
import { AudioPlayer } from './audio';
import { XferController } from './xfer';
import PipelineWorker from './pipeline.worker?worker';

/** 本端能力（纳入 Offer 签名；与 rdcore-app::Connection::capabilities 对齐）。 */
const CAPABILITIES_JSON = JSON.stringify({
  video_codecs: ['H264', 'Raw'],
  max_width: 1920,
  max_height: 1080,
  fps: 30,
  clipboard: true,
  input: { mouse: true, keyboard: true, wheel: true },
});
/** 控制通道帧长上限（与 rdcore-media::MAX_DATA_FRAME_LEN 一致）。 */
const CONTROL_MAX_LEN = 8 * 1024 * 1024;

/**
 * 构建时注入的 ICE 服务器（VITE_ICE_SERVERS，JSON 数组，如含 TURN 凭据）；
 * 未注入或注入非法时回退公共 STUN（跨 NAT 环境需显式注入 TURN）。
 */
function defaultIceServers(): RTCIceServer[] {
  const raw = import.meta.env.VITE_ICE_SERVERS as string | undefined;
  if (raw) {
    try {
      const parsed = JSON.parse(raw) as RTCIceServer[];
      if (Array.isArray(parsed) && parsed.length > 0) return parsed;
    } catch {
      // 注入值非法时回退默认，不阻断连接。
    }
  }
  return [{ urls: 'stun:stun.l.google.com:19302' }];
}
/** 自动重连退避：起始 1s，指数翻倍，上限 8s。 */
const BACKOFF_BASE_MS = 1000;
const BACKOFF_MAX_MS = 8000;
/** PeerConnection disconnected 宽限（短暂抖动不自愈才判定断线）。 */
const PC_GRACE_MS = 5000;
/**
 * Offer 发出后等待 Host Answer 的超时：中途被弃的握手会让 Host 的「等待 Offer」
 * 消费掉我们的 Offer 却不回 Answer（offer 在对端握手等待窗口内被吃掉），
 * 没有本超时就双向死等。超时后按断线走退避重发，直到与 Host 的等待窗口对齐。
 */
const ANSWER_TIMEOUT_MS = 15000;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const setStatus = (s: string) => ($('status').textContent = s);

type ControllerState =
  | 'idle' // 未连接 / 手动断开后（可点「连接」）
  | 'connecting' // 建连中（含自动重连的每一次尝试）
  | 'connected' // 已恢复（首帧到达）
  | 'backoff'; // 已断开，退避等待重连中

/** 解码/渲染/输入/重连探针（自动化 smoke 断言用；挂在 window 上便于 Playwright 读取）。 */
declare global {
  interface Window {
    __framesDecoded: number;
    __lastFrame?: { width: number; height: number; lum: number; std?: number; diff?: number };
    __frameStats: { maxStd: number; maxDiff: number };
    /** 本端 Viewer 身份指纹（内存身份，每次页面加载重新生成）。 */
    __viewerFp?: string;
    /** 已建立会话密钥的指纹（前 4 字节 hex）：重连后必须变化（X25519 临时密钥随握手轮换）。 */
    __sessionKeyFp?: string;
    /** wasm 初始化 + 事件挂载完成（自动化等待点）。 */
    __appReady?: boolean;
    /** consent 是否授予 Input scope。 */
    __inputAllowed: boolean;
    /** 用户输入开关当前状态。 */
    __inputEnabled: boolean;
    /** 已发送输入事件的结构化日志（上限 500 条，smoke 映射断言用）。 */
    __inputSent: SentInputRecord[];
    /** smoke 测试钩子：绕过 DOM 事件直接构造输入（Host 侧 OS 回环断言用）。 */
    __inputTest?: {
      setBlackhole(on: boolean): void;
      move(x: number, y: number): void;
      button(button: number, pressed: boolean): void;
      wheel(deltaX: number, deltaY: number): void;
      key(vk: number, key: string, pressed: boolean, mods?: { ctrlKey?: boolean; altKey?: boolean; metaKey?: boolean; shiftKey?: boolean }): void;
      clearSent(): void;
    };
    /** 页面加载标记（自动重连断言「未刷新页面」用）。 */
    __loadId: string;
    /** 自动重连状态探针。 */
    __autoReconnect: {
      state: ControllerState;
      drops: number;
      attempts: number;
      lastDelayMs: number;
      lastReason: string;
    };
    /** smoke 断线注入钩子：粗暴关闭信令 + PeerConnection（模拟网络中断，不置手动标志）。 */
    __rdTest?: { simulateDrop(): void };
    /** 生产诊断快照：连接/ICE/通道 readyState（部署排障用）。 */
    __rtcDebug?: () => unknown;
  }
}
window.__framesDecoded = 0;
window.__frameStats = { maxStd: 0, maxDiff: 0 };
window.__inputAllowed = false;
window.__inputEnabled = false;
window.__inputSent = [];
window.__loadId = Math.random().toString(16).slice(2, 10);
window.__autoReconnect = { state: 'idle', drops: 0, attempts: 0, lastDelayMs: 0, lastReason: '' };
window.__clipboard = { lastSent: null, pulled: null, lastRecv: null, systemReadback: null };
window.__fileSent = null;
window.__fileRecv = null;
window.__fileLog = [];
window.__audio = { frames: 0, rms: 0, sampleRate: 0, channels: 0, playedBlocks: 0, muted: false };

/** 输入开关/指示 UI 刷新（同意门控 allowed + 用户开关 enabled）。 */
function refreshInputUi(input: InputController): void {
  const btn = $<HTMLButtonElement>('inputToggle');
  btn.disabled = !input.allowed;
  btn.textContent = input.enabled ? '输入：开' : '输入：关';
  $('inputState').textContent = !input.allowed
    ? '输入未授权'
    : input.enabled
      ? '输入已开启'
      : '输入已关闭';
  window.__inputAllowed = input.allowed;
  window.__inputEnabled = input.enabled;
}

class ViewerController {
  readonly input: InputController;
  readonly xfer = new XferController();
  readonly audio = new AudioPlayer();
  private worker: Worker | null = null;
  /** worker 管线就绪等待（首次 init / 后续 reset）。 */
  private pipelinesReady: Promise<void> | null = null;
  private pipelinesReadyResolve: (() => void) | null = null;

  // ── 会话生命周期 ──
  private state: ControllerState = 'idle';
  private sessionId = 0;
  private manual = false;
  private backoffAttempt = 0;
  private retryTimer: number | null = null;
  private countdownTimer: number | null = null;
  private graceTimer: number | null = null;

  // ── 当前会话句柄（每 establish 全换；用于 teardown 与测试钩子）──
  private sig: SignalingClient | null = null;
  private rtc: RtcPeer | null = null;

  private host = '';
  private pairing = '';
  private sessionHex = '';

  constructor(private readonly canvas: HTMLCanvasElement) {
    this.input = new InputController(canvas, {
      // canvas 位图尺寸 = Host 帧分辨率（worker 每帧对齐 displayWidth/Height）。
      frameSize: () =>
        canvas.width > 0 && canvas.height > 0
          ? { width: canvas.width, height: canvas.height }
          : null,
      onSent: (rec) => {
        window.__inputSent.push(rec);
        if (window.__inputSent.length > 500)
          window.__inputSent.splice(0, window.__inputSent.length - 500);
      },
    });
    window.__rdTest = {
      simulateDrop: () => {
        // 模拟网络中断：不置 manual，直接掐信令与 P2P（app 应自动重连）。
        this.sig?.close();
        this.rtc?.close();
      },
    };
  }

  /** 「连接」按钮：从 idle/手动断开进入建连；重置手动标志与退避。 */
  start(): void {
    if (this.state !== 'idle') return;
    const host = $<HTMLInputElement>('host').value.trim();
    const pairing = $<HTMLInputElement>('pairing').value.trim();
    if (!host || !pairing) {
      setStatus('失败: 请填信令主机与配对码');
      return;
    }
    // 混合内容护栏：HTTPS 页面无法发起 ws:// 明文 WebSocket（浏览器强制拦截）。
    if (location.protocol === 'https:' && /^ws:\/\//i.test(host)) {
      setStatus('失败: HTTPS 页面无法用 ws:// 明文信令，请改用裸主机（自动 wss）或 wss:// 地址');
      return;
    }
    this.host = host;
    this.pairing = pairing;
    this.sessionHex = pairing.split(':')[0].toLowerCase();
    this.manual = false;
    this.backoffAttempt = 0;
    $<HTMLButtonElement>('connect').disabled = true;
    this.establish().catch((e) => this.drop(`建连失败: ${e?.message ?? e}`, this.sessionId));
  }

  /** 「断开」按钮：手动断开（终态，不自动重连）。 */
  disconnect(): void {
    if (this.state === 'idle') return;
    this.manual = true;
    this.clearTimers();
    this.teardownSession();
    this.setState('idle');
    setStatus('已断开（手动）');
    $<HTMLButtonElement>('connect').disabled = false;
  }

  // ── 会话建立（每次调用 = 一次完整建连：新握手/新密钥/新 PeerConnection）──

  private async establish(): Promise<void> {
    const sid = ++this.sessionId;
    this.setState('connecting');
    this.teardownSession(); // 防御：清掉可能残留的旧句柄（drop 里已清过，幂等）
    // 首次 init、后续 reset（每会话至多一次，见 ensurePipelines 的幂等设计）。
    const pipelinesReady = this.ensurePipelines();
    await pipelinesReady;

    // ── 握手状态机（WASM，主线程；每次全新 X25519 临时密钥）──
    const handshake = new rdweb.WebHandshake(this.sessionHex);
    // ── 发送侧加密管线（主线程；接收侧管线在 Worker 内，已 reset）──
    const sendPipe = new rdweb.FramePipeline();

    // ── 控制消息发送：postcard → 加密 → 4 字节长度前缀 → 16 KiB 分片 ──
    let controlSend: (bytes: Uint8Array) => void = () => {};
    const sendControlRaw = (bytes: Uint8Array) => controlSend(bytes);
    const sendEncryptedApp = (appPlaintext: Uint8Array) =>
      sendControlRaw(sendPipe.encrypt_control_message(appPlaintext));
    /** Offer→Answer 看门狗（超时按断线处理，退避后重发 Offer）。 */
    let answerTimer: number | null = null;
    /** 供嵌套 function 声明使用的 this 别名（function 声明的 this 是动态的）。 */
    const self = this;

    // 供 worker 消息分发读取的当前会话上下文（worker 全程复用，见 ensurePipelines）。
    this.current = {
      sid,
      frameSeen: false,
      onControlPlain: (bytes: Uint8Array) => {
        const info = JSON.parse(handshake.handle_message(bytes)) as { kind: string };
        if (info.kind === 'session_key') {
          handshake.handle_session_key_exchange(bytes);
          const key = handshake.session_key_bytes();
          sendPipe.set_session_key(key);
          this.worker?.postMessage({ type: 'session_key', key });
          // 会话密钥轮换探针（重连断言用）：取前 4 字节 hex 作指纹，不暴露密钥本体。
          window.__sessionKeyFp = Array.from(key.slice(0, 4))
            .map((b) => b.toString(16).padStart(2, '0'))
            .join('');
          // 会话密钥就绪 = 加密控制通道可用：接通输入发送通道与剪贴板/文件传输出口。
          this.input.attach(sendEncryptedApp);
          this.xfer.attach(sendEncryptedApp);
          setStatus('E2E 会话密钥已建立');
        }
      },
      onConsent: (detail: unknown) => this.applyConsent(detail),
      // Worker 背压/解码错误请求关键帧：构造 AppMessage::RequestKeyframe 加密发出
      //（PLI/FIR 语义；Host 收到后下一帧即 IDR，配合其 1 秒周期 IDR 兜底）。
      onNeedKey: () => {
        try {
          sendEncryptedApp(rdweb.build_request_keyframe());
        } catch (e) {
          console.error('[needkey] 关键帧请求发送失败:', e);
        }
      },
      onRevoke: () => {
        setStatus('Host 已撤销连接');
        this.input.allowed = false;
        this.input.enabled = false;
        refreshInputUi(this.input);
        // revoke = 终态：按手动断开处理，不自动重连。
        this.manual = true;
        this.drop('Host 撤销', sid);
      },
      onFrame: () => {
        // 首帧到达 = 媒体面全链路恢复：重置退避，进入 connected。
        this.backoffAttempt = 0;
        if (this.state === 'connecting') this.setState('connected');
      },
    };

    // ── 信令 ──
    const sig = new SignalingClient(SignalingClient.urlFor(this.host, this.pairing), {
      onMessage: (bytes) => void onSignaling(bytes).catch((e) => setStatus(`信令处理失败: ${e}`)),
      onOpen: () => void startHandshake().catch((e) => setStatus(`握手失败: ${e}`)),
      onClose: () => this.drop('信令已断开', sid),
    });
    this.sig = sig;

    // ── WebRTC（三条协商通道 media/0 control/1 audio/2）──
    // 本机回环时不依赖外部 STUN（离线/防火墙环境 ICE 仅靠 host 候选即可成对）。
    const iceServers: RTCIceServer[] = SignalingClient.isLoopbackHost(this.host)
      ? []
      : defaultIceServers();
    const rtc = await RtcPeer.create(
      {
        onIceCandidate: (json, mid, idx) =>
          sig.send(handshake.build_ice(json, mid ?? undefined, idx ?? undefined)),
        // 媒体/音频/控制字节零拷贝转移给 worker（transfer 后主线程侧即失效，
        // rtc 回调里每消息新建 Uint8Array，无复用方，安全）。
        onMediaMessage: (bytes) => this.worker?.postMessage({ type: 'media', bytes }, [bytes.buffer]),
        onControlMessage: (bytes) => this.worker?.postMessage({ type: 'control', bytes }, [bytes.buffer]),
        onAudioMessage: (bytes) => this.worker?.postMessage({ type: 'audio', bytes }, [bytes.buffer]),
        onControlOpen: () => {
          // 数据通道 open 后：先发明文 SessionKeyExchange（镜像 exchange_session_key）。
          sendControlRaw(handshake.build_session_key_exchange());
          setStatus('数据通道已开，交换会话密钥…');
        },
        onStateChange: (s) => this.onPcState(sid, s),
      },
      iceServers,
    );
    if (sid !== this.sessionId || this.state !== 'connecting') {
      // establish 等待期间已被 drop/重入（state 已离开 connecting）：本会话作废。
      rtc.close();
      sig.close();
      return;
    }
    this.rtc = rtc;
    this.startStats(); // 链路统计悬浮层：每秒采样 getStats + 帧统计
    window.__rtcDebug = () => rtc.debugState();
    controlSend = (bytes) => {
      const framed = rdweb.frame_wrap(bytes, CONTROL_MAX_LEN);
      const n = rdweb.sctp_chunk_count(framed.length);
      for (let i = 0; i < n; i++)
        rtc.control.send(rdweb.sctp_chunk(framed, i) as Uint8Array<ArrayBuffer>);
    };
    // 原样包一层：发送异常（如通道未 open）不再静默吞掉，而是进 console + 状态栏。
    const rawSend = controlSend;
    controlSend = (bytes) => {
      try {
        rawSend(bytes);
      } catch (e) {
        console.error('[input] control send failed (state=' + rtc.control.readyState + '):', e);
        setStatus(`控制通道发送失败：${e instanceof Error ? e.message : String(e)}`);
        throw e;
      }
    };

    async function onSignaling(bytes: Uint8Array): Promise<void> {
      const info = JSON.parse(handshake.handle_message(bytes)) as { kind: string };
      switch (info.kind) {
        case 'peer_hello':
          break; // TOFU 已由 handle_message 记住
        case 'answer': {
          if (answerTimer !== null) {
            window.clearTimeout(answerTimer);
            answerTimer = null;
          }
          const answer = JSON.parse(handshake.handle_answer(bytes)) as {
            sdp: string;
            fingerprint: string;
            display_name: string;
          };
          $('fingerprint').textContent =
            `对端 ${answer.display_name} 指纹: ${answer.fingerprint}（请带外核对）`;
          await rtc.acceptAnswer(answer.sdp);
          setStatus('Answer 验签通过，等待 P2P…');
          break;
        }
        case 'ice':
          await rtc.addIceCandidate((info as unknown as { candidate: string }).candidate);
          break;
        default:
          break; // 其它变体（SessionKey 等）不走信令
      }
    }

    /** 信令 open 后的建连序列（镜像 establish Viewer 分支）。 */
    async function startHandshake(): Promise<void> {
      await pipelinesReady;
      // PeerHello 先行（Host 按 TOFU 记住本端公钥，是验签 Offer 的前提）。
      sig.send(handshake.peer_hello());
      const sdp = await rtc.createOffer();
      sig.send(handshake.build_signed_offer(sdp, CAPABILITIES_JSON));
      // Answer 看门狗：超时不回即按断线处理（退避重发，直到与 Host 等待窗口对齐）。
      answerTimer = window.setTimeout(() => self.drop('等待 Host Answer 超时', sid), ANSWER_TIMEOUT_MS);
      setStatus(
        window.__autoReconnect.attempts > 0
          ? '重连中：Offer 已发出，等待 Host 确认…'
          : 'Offer 已发出，等待 Host 确认…',
      );
    }
  }

  // ── 断线与恢复 ──

  /** 断线统一入口（幂等；陈旧会话直接丢弃）。 */
  private drop(reason: string, sid: number): void {
    if (sid !== this.sessionId) return;
    if (this.state !== 'connecting' && this.state !== 'connected') return;
    window.__autoReconnect.drops += 1;
    window.__autoReconnect.lastReason = reason;
    this.clearTimers();
    this.teardownSession();
    if (this.manual) {
      this.setState('idle');
      $<HTMLButtonElement>('connect').disabled = false;
      return;
    }
    // 指数退避：1s→2s→4s→…上限 8s；恢复（首帧）后归零。
    const delay = Math.min(BACKOFF_MAX_MS, BACKOFF_BASE_MS * 2 ** this.backoffAttempt);
    this.backoffAttempt += 1;
    window.__autoReconnect.attempts += 1;
    window.__autoReconnect.lastDelayMs = delay;
    this.setState('backoff');
    let remain = Math.round(delay / 1000);
    setStatus(`已断开（${reason}），${remain}s 后重连…`);
    this.countdownTimer = window.setInterval(() => {
      remain -= 1;
      if (remain > 0) setStatus(`已断开（${reason}），${remain}s 后重连…`);
    }, 1000);
    this.retryTimer = window.setTimeout(() => {
      setStatus('重连中…');
      this.establish().catch((e) => this.drop(`建连失败: ${e?.message ?? e}`, this.sessionId));
    }, delay);
  }

  /** PeerConnection 状态机：failed/closed 即断；disconnected 给 5s 宽限；connected 取消宽限。 */
  private onPcState(sid: number, s: RTCPeerConnectionState): void {
    if (sid !== this.sessionId) return;
    console.log('PeerConnection:', s);
    if (s === 'failed' || s === 'closed') {
      this.drop(`PeerConnection ${s}`, sid);
    } else if (s === 'disconnected') {
      if (this.graceTimer !== null) window.clearTimeout(this.graceTimer);
      this.graceTimer = window.setTimeout(
        () => this.drop('PeerConnection disconnected 超时', sid),
        PC_GRACE_MS,
      );
    } else if (s === 'connected' && this.graceTimer !== null) {
      window.clearTimeout(this.graceTimer);
      this.graceTimer = null;
    }
  }

  private teardownSession(): void {
    if (this.graceTimer !== null) {
      window.clearTimeout(this.graceTimer);
      this.graceTimer = null;
    }
    if (this.statsTimer !== null) {
      window.clearInterval(this.statsTimer);
      this.statsTimer = null;
    }
    const el = document.getElementById('stats');
    if (el) el.textContent = '';
    const sig = this.sig;
    this.sig = null;
    sig?.close();
    const rtc = this.rtc;
    this.rtc = null;
    rtc?.close();
    this.input.attach(null); // 断连即停发输入并补发全部抬起（防远端卡键）
    this.xfer.attach(null); // 剪贴板/文件传输出口同步失效
    this.current = null;
  }

  private clearTimers(): void {
    if (this.retryTimer !== null) {
      window.clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
    if (this.countdownTimer !== null) {
      window.clearInterval(this.countdownTimer);
      this.countdownTimer = null;
    }
  }

  private setState(s: ControllerState): void {
    this.state = s;
    window.__autoReconnect.state = s;
  }

  // ── 链路统计悬浮层（延迟/卡顿诊断：每秒采样 getStats + worker 帧统计）──
  private statsTimer: number | null = null;
  private statFrames = 0;
  private statQueue = 0;
  private statDrops = 0;
  private statLastBytes = 0;

  /** 每秒刷新 #stats：fps · 吞吐 · RTT · 候选对类型 · 解码队列 · 背压丢帧。 */
  private startStats(): void {
    if (this.statsTimer !== null) window.clearInterval(this.statsTimer);
    this.statFrames = 0;
    this.statQueue = 0;
    this.statDrops = 0;
    this.statLastBytes = 0;
    this.statsTimer = window.setInterval(() => {
      void (async () => {
        const el = document.getElementById('stats');
        if (!el) return;
        const rtc = this.rtc;
        if (!rtc) {
          el.textContent = '';
          return;
        }
        const fps = this.statFrames;
        this.statFrames = 0;
        let rttMs = -1;
        let kbps = 0;
        let pairDesc = '?';
        try {
          const report = await rtc.getStats();
          type AnyStats = Record<string, any> & { id: string; type: string };
          const byId = new Map<string, AnyStats>();
          let pair: AnyStats | null = null;
          report.forEach((s) => {
            const st = s as unknown as AnyStats;
            byId.set(st.id, st);
            if (st.type === 'candidate-pair' && st['nominated'] && st['state'] === 'succeeded')
              pair = st;
          });
          if (pair) {
            const p: AnyStats = pair;
            rttMs = Math.round(((p['currentRoundTripTime'] as number) ?? 0) * 1000);
            const bytes = (p['bytesReceived'] as number) ?? 0;
            if (this.statLastBytes > 0)
              kbps = Math.round(((bytes - this.statLastBytes) * 8) / 1000);
            this.statLastBytes = bytes;
            const loc = byId.get(p['localCandidateId'] as string);
            const rem = byId.get(p['remoteCandidateId'] as string);
            pairDesc = `${loc?.['candidateType'] ?? '?'}↔${rem?.['candidateType'] ?? '?'}`;
          }
        } catch {
          // getStats 失败不阻断渲染
        }
        el.textContent =
          `${fps} fps · ${kbps} kb/s · RTT ${rttMs >= 0 ? `${rttMs}ms` : '?'} · ${pairDesc}` +
          ` · 解码队列 ${this.statQueue} · 丢帧 ${this.statDrops}`;
      })();
    }, 1000);
  }

  // ── worker（全程复用：OffscreenCanvas 只能转移一次；会话间靠 reset 清管线）──

  /** 当前会话上下文（worker 消息分发目标）。 */
  private current: {
    sid: number;
    frameSeen: boolean;
    onControlPlain(bytes: Uint8Array): void;
    onConsent(detail: unknown): void;
    onRevoke(): void;
    onFrame(): void;
    onNeedKey(): void;
  } | null = null;

  private ensureWorker(): void {
    if (this.worker) return;
    const worker = new PipelineWorker();
    this.worker = worker;
    worker.onmessage = (ev) => {
      const m = ev.data;
      const cur = this.current;
      switch (m.type) {
        case 'ready':
        case 'reset_done':
          this.pipelinesReadyResolve?.();
          this.pipelinesReadyResolve = null;
          break;
        case 'app': {
          if (!cur) break;
          const app = JSON.parse(m.json);
          console.log('控制消息:', app.kind, app.detail);
          if (app.kind === 'consent') {
            setStatus('已获 Host 授权，接收画面…');
            cur.onConsent(app.detail);
          }
          if (app.kind === 'revoke') cur.onRevoke();
          if (app.kind === 'clipboard') this.xfer.onClipboard(app.detail);
          break;
        }
        case 'file': {
          // 文件传输事件（Message::FileTransfer，rdcore-ffi Track B 格式）。
          if (!cur) break;
          const ev = JSON.parse(m.json) as { transfer_id: number; detail: Record<string, unknown> };
          console.log('文件事件:', ev.detail.action, ev.transfer_id);
          void this.xfer.onFileMessage(ev);
          break;
        }
        case 'audio': {
          // 音频帧（Worker 已解密为 Raw PCM）：喂给 WebAudio 播放。
          if (!cur) break;
          this.audio.onFrame({ channels: m.channels, sampleRate: m.sampleRate, pcm: m.pcm });
          if (window.__audio.frames % 25 === 1) {
            const a = window.__audio;
            $('audioState').textContent =
              `${a.sampleRate}Hz ${a.channels}ch · 已收 ${a.frames} 帧 · RMS ${a.rms} · 播放块 ${a.playedBlocks}`;
          }
          break;
        }
        case 'control_plain':
          // 握手期的明文控制消息（SessionKeyExchange 未加密）。
          cur?.onControlPlain(m.bytes as Uint8Array);
          break;
        case 'needkey':
          // Worker 背压/解码错误：向 Host 请求关键帧（P 帧流恢复）。
          cur?.onNeedKey();
          break;
        case 'error':
          console.error('pipeline worker:', m.message);
          break;
        case 'frame':
          // 每解码渲染一帧 +1；首帧到达即证明 E2E 解密 + WebCodecs + 绘制全链路通。
          window.__framesDecoded += 1;
          this.statFrames += 1;
          if (typeof m.queue === 'number') this.statQueue = m.queue;
          if (typeof m.drops === 'number') this.statDrops = m.drops;
          window.__lastFrame = { width: m.width, height: m.height, lum: m.lum, std: m.std, diff: m.diff };
          if (typeof m.std === 'number' && m.std > window.__frameStats.maxStd)
            window.__frameStats.maxStd = m.std;
          if (typeof m.diff === 'number' && m.diff > window.__frameStats.maxDiff)
            window.__frameStats.maxDiff = m.diff;
          if (cur && !cur.frameSeen) {
            cur.frameSeen = true;
            cur.onFrame();
            setStatus('首帧已渲染，投屏中…');
          }
          break;
      }
    };
    worker.postMessage({
      type: 'init',
      // 画面统计探针仅 smoke 开启（getImageData 每帧 GPU→CPU 读回，生产关闭）
      probe: new URLSearchParams(location.search).get('probe') === '1',
    });
    this.pipelinesReady = new Promise<void>((resolve) => {
      this.pipelinesReadyResolve = resolve;
    });
    const offscreen = this.canvas.transferControlToOffscreen();
    worker.postMessage({ type: 'canvas', canvas: offscreen }, [offscreen]);
  }

  /** 已按哪个 sessionId 重置过管线（保证每会话至多 reset 一次，幂等）。 */
  private pipelinesResetForSid = 0;

  /**
   * 首次 init、后续 reset：保证 worker 内管线/解码器/会话密钥干净。
   * 幂等：同一会话内重复调用返回同一个 Promise（不会中途二次 reset 洗掉会话密钥）。
   */
  private ensurePipelines(): Promise<void> {
    this.ensureWorker();
    if (this.pipelinesResetForSid !== this.sessionId) {
      this.pipelinesResetForSid = this.sessionId;
      if (this.sessionId > 1) {
        // 非首次建连：要求 worker 重置上一会话的管线（canvas/ctx 复用）。
        this.pipelinesReady = new Promise<void>((resolve) => {
          this.pipelinesReadyResolve = resolve;
        });
        this.worker!.postMessage({ type: 'reset' });
      }
    }
    return this.pipelinesReady!;
  }

  /** consent 下发的 scopes 门控：仅当含 Input 时默认开启输入；Clipboard/FileTransfer 门控 xfer UI。 */
  private applyConsent(detail: unknown): void {
    // detail = serde 展开的 AppMessage 负载：{ Consent: { Grant: { scopes: [...] } | { Deny: ... } } }
    const d = detail as { Consent?: { Grant?: { scopes?: string[] } } };
    const scopes = d?.Consent?.Grant?.scopes ?? [];
    this.input.allowed = scopes.includes('Input');
    this.input.enabled = this.input.allowed; // 授权含 Input 时默认开启，用户可手动关闭
    refreshInputUi(this.input);
    this.xfer.setScopes(scopes);
    console.log('consent scopes:', scopes, '→ input allowed =', this.input.allowed);
  }
}

async function main() {
  await init({ module_or_path: wasmUrl });

  // 构建时注入的信令默认值（web/deploy/build-dist.sh 的 VITE_SIGNALING_DEFAULT；未注入则留空手输）。
  const defaultSig = import.meta.env.VITE_SIGNALING_DEFAULT as string | undefined;
  if (defaultSig) ($('host') as HTMLInputElement).value = defaultSig;

  // 内存身份（M1 后续：identity_export/identity_import + IndexedDB 持久化）。
  const me = JSON.parse(rdweb.generate_identity('web-viewer')) as { fingerprint: string };
  console.log('本机身份指纹:', me.fingerprint);
  window.__viewerFp = me.fingerprint;

  const controller = new ViewerController($('screen') as unknown as HTMLCanvasElement);
  const input = controller.input;

  $('connect').addEventListener('click', () => {
    controller.audio.resume(); // 用户手势时机：解除 AudioContext 自动播放限制
    controller.start();
  });
  $('disconnect').addEventListener('click', () => controller.disconnect());
  refreshInputUi(input);
  $('inputToggle').addEventListener('click', () => {
    if (!input.allowed) return;
    input.enabled = !input.enabled;
    refreshInputUi(input);
  });
  // 音频开关：静音/取消静音（M3-C）。
  $('audioToggle').addEventListener('click', () => {
    controller.audio.resume();
    const muted = controller.audio.toggleMute();
    $<HTMLButtonElement>('audioToggle').textContent = muted ? '音频：静音' : '音频：开';
  });
  // 连接存续期间防误关页（Ctrl+W 不可 preventDefault，只能靠 beforeunload 提示拦截）。
  window.addEventListener('beforeunload', (e) => {
    if (input.active) e.preventDefault();
  });

  // smoke 测试钩子：blackhole（只构造记录不发送，纯映射断言）+ 直接构造 API
  // （OS 回环断言：真实经加密通道发到 Host 注入，验证 Host 侧接收/处理）。
  window.__inputTest = {
    setBlackhole: (on) => {
      input.blackhole = on;
    },
    move: (x, y) => input.sendMouseMove(x, y),
    button: (b, p) => input.sendMouseButton(b, p),
    wheel: (dx, dy) => input.sendMouseWheel(dx, dy),
    key: (vk, key, pressed, mods) =>
      input.sendKey(vk, key, pressed, mods as KeyboardEvent | undefined),
    clearSent: () => {
      window.__inputSent.length = 0;
    },
  };

  window.__appReady = true;
}

main();
