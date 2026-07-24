/**
 * 剪贴板同步（M3-A）+ 文件传输（M3-B）控制器。
 *
 * 线格式（与 Host 侧严格对偶，见 rdcore-web::pipeline 的构造器文档）：
 * - 剪贴板：`AppMessage::Clipboard(ClipboardEvent{seq, action})`，Request/Data/Clear
 *   三动作；Data ≤ 5 MiB（MAX_CLIPBOARD_SIZE）。
 * - 文件传输：`postcard(Message::FileTransfer(FileTransferEvent{transfer_id, action}))`，
 *   Offer/Accept/Reject/Chunk/Done/Abort；Chunk ≤ 1 MiB（MAX_FILE_CHUNK_SIZE），
 *   seq 单调递增按序重组；接收方未 Accept 前收到 Chunk 属协议违规。
 *
 * 两者共用同一条 E2E 加密控制通道（`encrypt_control_message` 对任意内层明文 AEAD）。
 * consent scope 门控：Clipboard / FileTransfer 未授予时对应 UI 禁用。
 */
import * as rdweb from '@rdcore/rdcore_web.js';

/** 加密控制消息发送出口（main.ts 注入；断连时为 null）。 */
export type SendApp = (bytes: Uint8Array) => void;

const CLIP_MAX = 5 * 1024 * 1024;
const CHUNK = 1024 * 1024; // MAX_FILE_CHUNK_SIZE

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/** crypto.subtle 计算 SHA-256（hex）。 */
async function sha256Hex(buf: Uint8Array): Promise<string> {
  const d = await crypto.subtle.digest('SHA-256', buf as Uint8Array<ArrayBuffer>);
  return Array.from(new Uint8Array(d))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

type RecvSession = {
  name: string;
  size: number;
  chunks: Map<number, Uint8Array>;
};

/** 剪贴板/文件传输探针（smoke 断言读取；window.__clipboard / __fileSent / __fileRecv / __fileLog）。 */
declare global {
  interface Window {
    __clipboard: {
      lastSent: string | null;
      pulled: string | null;
      lastRecv: string | null;
      systemReadback: string | null;
    };
    __fileSent: {
      id: number;
      name: string;
      size: number;
      sha256: string;
      state: string;
      chunks: number;
    } | null;
    __fileRecv: {
      id: number;
      name: string;
      size: number;
      sha256: string;
      chunks: number;
      sizeOk: boolean;
    } | null;
    __fileLog: string[];
  }
}

export class XferController {
  private sendApp: SendApp | null = null;
  private clipSeq = 0;
  private nextTransferId = 1;
  /** 「拉取 Host 剪贴板」的等待句柄（一次一个）。 */
  private pendingPull: {
    resolve: (text: string) => void;
    timer: number;
  } | null = null;
  /** Viewer→Host：已 Offer、等 Accept 的发送会话。 */
  private sends = new Map<number, { name: string; buf: Uint8Array }>();
  /** Host→Viewer：接收中的会话（Accept 后收 Chunk）。 */
  private recvs = new Map<number, RecvSession>();
  /** 当前展示在确认框里的入站 Offer id。 */
  private incomingOfferId: number | null = null;

  constructor() {
    $('clipSend').addEventListener('click', () => void this.sendClipboard());
    $('clipPull').addEventListener('click', () => void this.pullClipboard());
    $('fileSend').addEventListener('click', () => void this.sendFile());
    $('fileAccept').addEventListener('click', () => this.acceptIncoming());
    $('fileReject').addEventListener('click', () => this.rejectIncoming());
  }

  /** 会话建立后由 main.ts 注入发送出口；断连时传 null。 */
  attach(send: SendApp | null): void {
    this.sendApp = send;
    if (!send) {
      this.sends.clear();
      this.recvs.clear();
      this.incomingOfferId = null;
      $('fileOffer').hidden = true;
    }
  }

  /** consent scopes 门控：未授予时禁用对应按钮并如实展示。 */
  setScopes(scopes: string[]): void {
    const clip = scopes.includes('Clipboard');
    const file = scopes.includes('FileTransfer');
    $<HTMLButtonElement>('clipSend').disabled = !clip;
    $<HTMLButtonElement>('clipPull').disabled = !clip;
    $<HTMLButtonElement>('fileSend').disabled = !file;
    $('clipState').textContent = clip ? '剪贴板已授权' : '剪贴板未授权';
    $('fileSendState').textContent = file ? '文件传输已授权' : '文件传输未授权';
  }

  private log(line: string): void {
    window.__fileLog.push(line);
    if (window.__fileLog.length > 200)
      window.__fileLog.splice(0, window.__fileLog.length - 200);
  }

  // ─────────────────────────── 剪贴板（M3-A） ───────────────────────────

  /** 「发送到 Host」：面板文本 → UTF-8 → Clipboard Data。 */
  private async sendClipboard(): Promise<void> {
    if (!this.sendApp) return;
    const text = $<HTMLTextAreaElement>('clipText').value;
    const bytes = new TextEncoder().encode(text);
    if (bytes.length > CLIP_MAX) {
      $('clipState').textContent = `失败：${bytes.length} 字节超过 5 MiB 上限`;
      return;
    }
    this.sendApp(rdweb.build_clipboard_data(BigInt(++this.clipSeq), bytes));
    window.__clipboard.lastSent = text;
    $('clipState').textContent = `已发送 ${bytes.length} 字节到 Host`;
  }

  /** 「拉取 Host 剪贴板」：发 Request，等 Data 应答后写入系统剪贴板并读回核对。 */
  private async pullClipboard(): Promise<void> {
    if (!this.sendApp || this.pendingPull) return;
    this.sendApp(rdweb.build_clipboard_request(BigInt(++this.clipSeq)));
    $('clipState').textContent = '已请求 Host 剪贴板…';
    const text = await new Promise<string>((resolve) => {
      const timer = window.setTimeout(() => {
        this.pendingPull = null;
        resolve('');
      }, 10000);
      this.pendingPull = { resolve, timer };
    });
    if (!text) {
      $('clipState').textContent = '拉取超时（10s 无应答）';
      return;
    }
    $('clipState').textContent = `已拉取 ${text.length} 字符，写入系统剪贴板…`;
    // 按钮点击 = 用户手势，clipboard-write 在 secure context（localhost）可用。
    try {
      await navigator.clipboard.writeText(text);
      window.__clipboard.systemReadback = await navigator.clipboard.readText();
    } catch (e) {
      console.warn('系统剪贴板写入/读回失败（权限？）:', e);
      window.__clipboard.systemReadback = null;
    }
  }

  /** 处理一条 AppMessage::Clipboard（worker 转来的 detail = serde 展开）。 */
  onClipboard(detail: unknown): void {
    const d = detail as { Clipboard?: { action?: unknown } };
    const action = d?.Clipboard?.action;
    if (action && typeof action === 'object' && 'Data' in action) {
      const bytes = new Uint8Array((action as { Data: number[] }).Data);
      const text = new TextDecoder().decode(bytes);
      if (this.pendingPull) {
        window.clearTimeout(this.pendingPull.timer);
        const p = this.pendingPull;
        this.pendingPull = null;
        window.__clipboard.pulled = text;
        $<HTMLTextAreaElement>('clipRecv').value = text;
        p.resolve(text);
      } else {
        // Host 主动推送（ unsolicited Data）：展示进面板。
        window.__clipboard.lastRecv = text;
        $<HTMLTextAreaElement>('clipRecv').value = text;
        $('clipState').textContent = `收到 Host 剪贴板 ${bytes.length} 字节`;
      }
      return;
    }
    if (action === 'Clear') {
      window.__clipboard.lastRecv = null;
      $<HTMLTextAreaElement>('clipRecv').value = '';
      $('clipState').textContent = 'Host 已清除剪贴板镜像';
    }
  }

  // ─────────────────────────── 文件传输（M3-B） ───────────────────────────

  /** 「发送文件到 Host」：读文件 → Offer →（等 Accept 后）分片 → Done。 */
  private async sendFile(): Promise<void> {
    if (!this.sendApp) return;
    const input = $<HTMLInputElement>('fileInput');
    const file = input.files?.[0];
    if (!file) {
      $('fileSendState').textContent = '请先选择文件';
      return;
    }
    const buf = new Uint8Array(await file.arrayBuffer());
    const id = this.nextTransferId++;
    const sha256 = await sha256Hex(buf);
    this.sends.set(id, { name: file.name, buf });
    window.__fileSent = { id, name: file.name, size: buf.length, sha256, state: 'offered', chunks: 0 };
    this.sendApp(rdweb.build_file_offer(BigInt(id), file.name, BigInt(buf.length)));
    this.log(`→ Offer #${id} ${file.name} (${buf.length}B, sha256=${sha256.slice(0, 16)}…)`);
    $('fileSendState').textContent = `已提议 ${file.name}（${buf.length}B），等待 Host 同意…`;
  }

  /** 入站 Offer 的「接受」按钮。 */
  private acceptIncoming(): void {
    if (!this.sendApp || this.incomingOfferId === null) return;
    const id = this.incomingOfferId;
    this.incomingOfferId = null;
    $('fileOffer').hidden = true;
    this.sendApp(rdweb.build_file_accept(BigInt(id)));
    this.log(`→ Accept #${id}`);
    $('fileRecvState').textContent = '已接受，接收分片中…';
  }

  /** 入站 Offer 的「拒绝」按钮。 */
  private rejectIncoming(): void {
    if (!this.sendApp || this.incomingOfferId === null) return;
    const id = this.incomingOfferId;
    this.incomingOfferId = null;
    $('fileOffer').hidden = true;
    this.recvs.delete(id);
    this.sendApp(rdweb.build_file_reject(BigInt(id), 'viewer 用户拒绝'));
    this.log(`→ Reject #${id}`);
    $('fileRecvState').textContent = '已拒绝该文件';
  }

  /** 处理一条 Message::FileTransfer（worker 解析为 {transfer_id, detail}）。 */
  async onFileMessage(msg: { transfer_id: number; detail: Record<string, unknown> }): Promise<void> {
    const id = msg.transfer_id;
    const a = msg.detail;
    switch (a.action) {
      case 'Offer': {
        const name = String(a.name);
        const size = Number(a.size);
        this.recvs.set(id, { name, size, chunks: new Map() });
        this.incomingOfferId = id;
        $('fileOfferName').textContent = `Host 提议发送：${name}（${size} 字节）`;
        $('fileOffer').hidden = false;
        this.log(`← Offer #${id} ${name} (${size}B)`);
        break;
      }
      case 'Accept': {
        const entry = this.sends.get(id);
        if (!entry) break;
        this.log(`← Accept #${id}，开始分片发送`);
        const { buf } = entry;
        const n = Math.max(1, Math.ceil(buf.length / CHUNK));
        for (let seq = 0; seq < n; seq++) {
          const piece = buf.subarray(seq * CHUNK, Math.min(buf.length, (seq + 1) * CHUNK));
          this.sendApp!(rdweb.build_file_chunk(BigInt(id), BigInt(seq), piece));
          if (window.__fileSent) window.__fileSent.chunks = seq + 1;
          $('fileSendState').textContent = `分片发送中 ${seq + 1}/${n}…`;
          // 让出主线程，避免 2.5MiB 分片发送阻塞 UI/探针读取。
          await new Promise((r) => setTimeout(r, 0));
        }
        this.sendApp!(rdweb.build_file_done(BigInt(id), BigInt(n)));
        this.sends.delete(id);
        if (window.__fileSent) window.__fileSent.state = 'done';
        this.log(`→ Done #${id}（${n} 片）`);
        $('fileSendState').textContent = `发送完成（${n} 个分片）`;
        break;
      }
      case 'Reject': {
        this.sends.delete(id);
        if (window.__fileSent) window.__fileSent.state = `rejected: ${String(a.reason)}`;
        this.log(`← Reject #${id}: ${String(a.reason)}`);
        $('fileSendState').textContent = `Host 拒绝：${String(a.reason)}`;
        break;
      }
      case 'Chunk': {
        const sess = this.recvs.get(id);
        if (!sess) {
          console.warn(`未 Accept 的 Chunk（协议违规），忽略 #${id} seq=${Number(a.seq)}`);
          break;
        }
        const hex = String(a.data_hex);
        const bytes = new Uint8Array(hex.length / 2);
        for (let i = 0; i < bytes.length; i++)
          bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
        sess.chunks.set(Number(a.seq), bytes);
        $('fileRecvState').textContent = `接收中 ${sess.chunks.size} 片…`;
        break;
      }
      case 'Done': {
        const sess = this.recvs.get(id);
        if (!sess) break;
        this.recvs.delete(id);
        const n = Number(a.chunks);
        const parts: Uint8Array[] = [];
        let gap = false;
        for (let i = 0; i < n; i++) {
          const p = sess.chunks.get(i);
          if (!p) {
            gap = true;
            break;
          }
          parts.push(p);
        }
        if (gap) {
          $('fileRecvState').textContent = '失败：分片缺口';
          this.log(`✗ Done #${id}：分片缺口`);
          break;
        }
        const total = parts.reduce((s, p) => s + p.length, 0);
        const buf = new Uint8Array(total);
        let off = 0;
        for (const p of parts) {
          buf.set(p, off);
          off += p.length;
        }
        const sha256 = await sha256Hex(buf);
        const sizeOk = total === sess.size;
        window.__fileRecv = { id, name: sess.name, size: total, sha256, chunks: n, sizeOk };
        this.log(`← Done #${id} ${sess.name}（${n} 片, ${total}B, sha256=${sha256.slice(0, 16)}…）`);
        $('fileRecvState').textContent = sizeOk
          ? `接收完成 ${sess.name}（${total}B）sha256=${sha256.slice(0, 16)}…`
          : `警告：尺寸不符（${total} != ${sess.size}）`;
        // 下载入口（Blob；smoke 不必点击，页面内 sha256 已自证完整）。
        const link = $<HTMLAnchorElement>('fileDownload');
        link.href = URL.createObjectURL(new Blob([buf]));
        link.download = sess.name;
        link.hidden = false;
        link.textContent = `下载 ${sess.name}`;
        break;
      }
      case 'Abort': {
        this.recvs.delete(id);
        this.sends.delete(id);
        this.log(`← Abort #${id}`);
        break;
      }
    }
  }
}
