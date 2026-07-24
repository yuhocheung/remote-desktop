/**
 * 信令 WebSocket 客户端：连 `wss://<host>/signaling/<session_hex>?token=<token>`。
 *
 * 与 rdcore-signaling 对齐的线格式事实：
 * - 只传二进制帧（postcard 编码的 `Message`），云端只见 SDP/ICE，不见媒体/控制内容。
 * - 单帧上限 64 KiB（MAX_SIGNALING_MESSAGE_LEN），超长帧被对端丢弃。
 */

export type SignalingHandler = {
  /** 收到一帧 postcard(Message) 二进制数据。 */
  onMessage(bytes: Uint8Array): void;
  onOpen?(): void;
  onClose?(ev: CloseEvent): void;
  onError?(ev: Event): void;
};

export class SignalingClient {
  private ws: WebSocket;

  constructor(url: string, handler: SignalingHandler) {
    this.ws = new WebSocket(url);
    this.ws.binaryType = 'arraybuffer';
    this.ws.onopen = () => handler.onOpen?.();
    this.ws.onclose = (ev) => handler.onClose?.(ev);
    this.ws.onerror = (ev) => handler.onError?.(ev);
    this.ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) {
        handler.onMessage(new Uint8Array(ev.data));
      }
      // 文本/其它帧不属于本协议，忽略（与 Rust 端读任务行为一致）。
    };
  }

  /** 配对码 `<32hex>:<64hex>` → 信令 URL。 */
  static urlFor(host: string, pairingCode: string): string {
    const m = /^([0-9a-fA-F]{32}):([0-9a-fA-F]{64})$/.exec(pairingCode.trim());
    if (!m) throw new Error('配对码格式应为 <32hex session>:<64hex token>');
    const session = m[1].toLowerCase();
    const token = m[2].toLowerCase();
    const h = host.trim().replace(/\/+$/, '');
    // 1) 显式 ws:// / wss:// 全前缀（可带 base path）：原样拼接。
    if (/^wss?:\/\//.test(h)) return `${h}/${session}?token=${token}`;
    // 2) 本机回环：明文 ws、session 直接作首段路径——signaling-svc 握手回调取
    //    路径首段为 session_hex（`trim_start_matches('/').split('/').next()`），
    //    本地部署无网关剥离 `/signaling` 前缀，带前缀会被当非法 session 拒 400。
    if (SignalingClient.isLoopbackHost(h)) return `ws://${h}/${session}?token=${token}`;
    // 3) 远程裸主机：生产约定 wss + `/signaling` 前缀（网关剥离后交给服务）。
    return `wss://${h}/signaling/${session}?token=${token}`;
  }

  /** 是否本机回环地址（决定 ws 明文 + 无 base path）。 */
  static isLoopbackHost(host: string): boolean {
    const bare = host.trim().replace(/^wss?:\/\//, '').split('/')[0];
    return /^(localhost|127\.0\.0\.1|\[?::1\]?)(:\d+)?$/i.test(bare);
  }

  /** 发送一帧 postcard(Message) 字节。 */
  send(bytes: Uint8Array): void {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(bytes);
  }

  close(): void {
    this.ws.close();
  }
}
