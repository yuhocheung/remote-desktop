/**
 * 键鼠输入映射与发送（M2-A）：DOM 事件 → InputKind → postcard(AppMessage) 明文。
 *
 * 与 Host 注入端（rdcore-capture::EnigoInputInjector → enigo 0.5 / Windows）对齐的
 * 语义事实（以 core 源码为准，勿凭直觉改动）：
 * - **坐标**：MouseMove 的 x/y 是被控端主屏的**绝对物理像素**
 *   （enigo `move_mouse(Abs)` → 0..w-1 归一化到 0..65535 的 SendInput）。
 *   本模块按「Host 帧分辨率 ÷ canvas 显示尺寸」把 canvas 内坐标换算到帧像素。
 * - **鼠标按键**：button 编号 0=左 1=中 2=右 3=侧前(Back) 4=侧后(Forward)，
 *   与 DOM `MouseEvent.button` 的编号一致，可直接透传。
 * - **滚轮**：delta_y 符号与浏览器一致（正 = 向下滚动）——enigo
 *   `scroll(length, Vertical)` 正值即向下；单位是「滚轮格数」（15°/格），
 *   浏览器 pixel 模式每格约 120、line 模式约 3，归一化后至少 ±1。
 * - **按键 key_code**：协议注释写作「平台扫描码」，但 Host 侧 enigo
 *   `Key::Other(u32)` 在 Windows 上按 **Virtual-Key code (VK)** 解释
 *   （enigo-0.5 keycodes.rs: `Key::Other(v) → VIRTUAL_KEY(v as u16)`，
 *   内部再 VK→scancode 转换发 SendInput）。因此本表把 `KeyboardEvent.code`
 *   （物理键位）映射为 **Windows VK**。与 Flutter 端 `_sendBackspace` 用
 *   VK_BACK=0x08 的既有约定一致。
 * - **字符**：可打印字符（`key.length === 1` 且无 Ctrl/Alt/Meta）走
 *   `KeyWithChar`（Host `pressed=true` 且 character 非空时 `enigo.text()`
 *   整段 Unicode 注入，中文/IME 友好）；功能键与快捷键组合走 `Key`
 *   （纯 VK 物理键，保证 Ctrl+C 等快捷键在远端生效）。
 * - **modifiers 位掩码**：Shift=1 Ctrl=2 Alt=4 Meta=8（与 Flutter `_modifierMask` 一致；
 *   Host 当前不展开修饰键，仅作记录/前向兼容）。
 * - **连发**：与 Flutter `_onKey` 一致跳过 `e.repeat`，远端按住重复交由 Host 侧
 *   物理 keydown 状态（未来如需重复字符再放开）。
 */

import * as rdweb from '@rdcore/rdcore_web.js';

/** `KeyboardEvent.code`（物理键位）→ Windows Virtual-Key code。 */
const VK_BY_CODE: Record<string, number> = {
  Backquote: 0xc0,
  Digit1: 0x31, Digit2: 0x32, Digit3: 0x33, Digit4: 0x34, Digit5: 0x35,
  Digit6: 0x36, Digit7: 0x37, Digit8: 0x38, Digit9: 0x39, Digit0: 0x30,
  Minus: 0xbd, Equal: 0xbb, Backspace: 0x08, Tab: 0x09,
  KeyQ: 0x51, KeyW: 0x57, KeyE: 0x45, KeyR: 0x52, KeyT: 0x54, KeyY: 0x59,
  KeyU: 0x55, KeyI: 0x49, KeyO: 0x4f, KeyP: 0x50,
  BracketLeft: 0xdb, BracketRight: 0xdd, Backslash: 0xdc,
  CapsLock: 0x14,
  KeyA: 0x41, KeyS: 0x53, KeyD: 0x44, KeyF: 0x46, KeyG: 0x47, KeyH: 0x48,
  KeyJ: 0x4a, KeyK: 0x4b, KeyL: 0x4c,
  Semicolon: 0xba, Quote: 0xde, Enter: 0x0d,
  ShiftLeft: 0xa0,
  KeyZ: 0x5a, KeyX: 0x58, KeyC: 0x43, KeyV: 0x56, KeyB: 0x42, KeyN: 0x4e,
  KeyM: 0x4d, Comma: 0xbc, Period: 0xbe, Slash: 0xbf,
  ShiftRight: 0xa1,
  ControlLeft: 0xa2, MetaLeft: 0x5b, AltLeft: 0xa4, Space: 0x20,
  AltRight: 0xa5, MetaRight: 0x5c, ContextMenu: 0x5d, ControlRight: 0xa3,
  ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28,
  Escape: 0x1b,
  F1: 0x70, F2: 0x71, F3: 0x72, F4: 0x73, F5: 0x74, F6: 0x75,
  F7: 0x76, F8: 0x77, F9: 0x78, F10: 0x79, F11: 0x7a, F12: 0x7b,
  Insert: 0x2d, Delete: 0x2e, Home: 0x24, End: 0x23,
  PageUp: 0x21, PageDown: 0x22,
  PrintScreen: 0x2c, ScrollLock: 0x91, Pause: 0x13,
  NumLock: 0x90,
  Numpad0: 0x60, Numpad1: 0x61, Numpad2: 0x62, Numpad3: 0x63, Numpad4: 0x64,
  Numpad5: 0x65, Numpad6: 0x66, Numpad7: 0x67, Numpad8: 0x68, Numpad9: 0x69,
  NumpadMultiply: 0x6a, NumpadAdd: 0x6b, NumpadSubtract: 0x6d,
  NumpadDecimal: 0x6e, NumpadDivide: 0x6f, NumpadEnter: 0x0d, NumpadComma: 0x6c,
};

/** 修饰键位掩码（与 Flutter `_modifierMask` 一致）。 */
export function modifierMask(e: KeyboardEvent | MouseEvent): number {
  let m = 0;
  if (e.shiftKey) m |= 1;
  if (e.ctrlKey) m |= 2;
  if (e.altKey) m |= 4;
  if (e.metaKey) m |= 8;
  return m;
}

/** 发送侧回调：拿到 postcard(AppMessage) 明文，由外层负责加密 + 分片发送。 */
export type InputSink = (appPlaintext: Uint8Array) => void;

/** 已发送事件的结构化记录（smoke 断言用，与线上加密负载一一对应）。 */
export type SentInputRecord =
  | { seq: string; kind: 'MouseMove'; x: number; y: number }
  | { seq: string; kind: 'MouseButton'; button: number; pressed: boolean }
  | { seq: string; kind: 'MouseWheel'; deltaX: number; deltaY: number }
  | { seq: string; kind: 'Key'; keyCode: number; pressed: boolean; modifiers: number }
  | {
      seq: string;
      kind: 'KeyWithChar';
      keyCode: number;
      character: string | null;
      pressed: boolean;
      modifiers: number;
    };

/** 分布在联合各成员上的 Omit（保留可辨识联合特性）。 */
type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

export type InputControllerOptions = {
  /** 取当前 Host 帧分辨率（canvas 位图尺寸即帧尺寸）。 */
  frameSize: () => { width: number; height: number } | null;
  /** 事件发送遥测（smoke 用）：每真实构造一条事件即回调。 */
  onSent?: (rec: SentInputRecord) => void;
};

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/**
 * 键鼠输入控制器：挂到 canvas + window，受「同意门控 + 用户开关」双重控制。
 * 所有事件经单调递增 seq 编号（u64，JS 侧 BigInt）。
 */
export class InputController {
  /** 同意门控：consent scopes 含 Input 才为 true（由外层设置）。 */
  allowed = false;
  /** 用户开关（UI 切换）；与 allowed 同时为 true 才真正发送。 */
  enabled = false;

  private seq = 0n;
  private sink: InputSink | null = null;
  /** 测试模式：构造并记录事件但不真正发送（smoke 的纯映射断言用，杜绝回环副作用）。 */
  blackhole = false;
  /** 当前按下的鼠标按键位掩码（用于 pointerleave/blur 时补发抬起，防远端卡键）。 */
  private buttonsDown = new Set<number>();
  /** 当前按下的 VK（blur 时补发 keyup）。 */
  private keysDown = new Set<number>();
  /** MouseMove 节流：只发最新坐标。 */
  private pendingMove: { x: number; y: number } | null = null;
  private moveTimer: number | null = null;

  constructor(
    private canvas: HTMLCanvasElement,
    private opts: InputControllerOptions,
  ) {
    canvas.addEventListener('pointermove', (e) => this.onPointerMove(e));
    canvas.addEventListener('pointerdown', (e) => this.onPointerDown(e));
    canvas.addEventListener('pointerup', (e) => this.onPointerUp(e));
    canvas.addEventListener('pointercancel', (e) => this.onPointerUp(e));
    canvas.addEventListener('pointerleave', () => this.releaseAllButtons());
    canvas.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    window.addEventListener('contextmenu', (e) => {
      // 右键属于远程输入：canvas 上始终屏蔽本地菜单；输入激活期间全窗口屏蔽
      // （防止远程右键误开本页菜单，也避免 smoke 回环注入时菜单截获后续事件）。
      const onCanvas = (e.target as HTMLElement | null)?.closest('canvas') !== null;
      if (this.active || onCanvas) e.preventDefault();
    });
    window.addEventListener('keydown', (e) => this.onKey(e, true));
    window.addEventListener('keyup', (e) => this.onKey(e, false));
    window.addEventListener('blur', () => this.releaseAll());
  }

  /** 连接建立后绑定发送通道；断连时置 null。 */
  attach(sink: InputSink | null): void {
    this.sink = sink;
    if (!sink) this.releaseAll();
  }

  /** 是否处于「可发输入」状态（同意门控 + 用户开关 + 通道就绪）。 */
  get active(): boolean {
    return this.allowed && this.enabled && this.sink !== null;
  }

  // ── 鼠标 ────────────────────────────────────────────────

  /** DOM 坐标 → Host 帧像素（按帧分辨率 ÷ canvas 显示尺寸换算，clamp 到帧内）。 */
  private toFrame(e: PointerEvent): { x: number; y: number } | null {
    const frame = this.opts.frameSize();
    if (!frame || frame.width <= 0 || frame.height <= 0) return null;
    const rect = this.canvas.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    const x = Math.round(((e.clientX - rect.left) * frame.width) / rect.width);
    const y = Math.round(((e.clientY - rect.top) * frame.height) / rect.height);
    return { x: clamp(x, 0, frame.width - 1), y: clamp(y, 0, frame.height - 1) };
  }

  private onPointerMove(e: PointerEvent): void {
    if (!this.active) return;
    const p = this.toFrame(e);
    if (!p) return;
    this.pendingMove = p;
    if (this.moveTimer === null) {
      this.sendMouseMove(p.x, p.y);
      this.pendingMove = null;
      // ~80/s 上限：足够流畅又不刷爆 SCTP 控制通道。
      this.moveTimer = window.setTimeout(() => {
        this.moveTimer = null;
        if (this.pendingMove) {
          this.sendMouseMove(this.pendingMove.x, this.pendingMove.y);
          this.pendingMove = null;
        }
      }, 12);
    }
  }

  private onPointerDown(e: PointerEvent): void {
    if (!this.active) return;
    this.canvas.focus();
    e.preventDefault();
    const button = clamp(e.button, 0, 4);
    this.buttonsDown.add(button);
    // 按下前先对齐一次坐标，保证「在哪里按下」与「按下」一致。
    const p = this.toFrame(e);
    if (p) this.sendMouseMove(p.x, p.y);
    this.sendMouseButton(button, true);
  }

  private onPointerUp(e: PointerEvent): void {
    const button = clamp(e.button, 0, 4);
    this.buttonsDown.delete(button);
    if (!this.active) return;
    e.preventDefault();
    this.sendMouseButton(button, false);
  }

  private onWheel(e: WheelEvent): void {
    if (!this.active) return;
    e.preventDefault();
    const unit = e.deltaMode === 1 ? 3 : 120; // 1=line（每格约 3），0=pixel（每格约 120）
    let dx = Math.round(e.deltaX / unit);
    let dy = Math.round(e.deltaY / unit);
    // 非零滚动至少发 1 格（高精度触摸板小步长不致被吞）。
    if (dx === 0 && e.deltaX !== 0) dx = Math.sign(e.deltaX);
    if (dy === 0 && e.deltaY !== 0) dy = Math.sign(e.deltaY);
    if (dx === 0 && dy === 0) return;
    this.sendMouseWheel(clamp(dx, -32768, 32767), clamp(dy, -32768, 32767));
  }

  // ── 键盘 ────────────────────────────────────────────────

  private onKey(e: KeyboardEvent, pressed: boolean): void {
    if (!this.active) return;
    // 表单控件里的按键（配对码输入框等）不转发。
    const t = e.target as HTMLElement | null;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
    if (e.repeat) return; // 与 Flutter 一致：跳过系统自动重复
    const vk = VK_BY_CODE[e.code];
    if (vk === undefined) return; // 未映射键位忽略
    e.preventDefault();
    this.sendKey(vk, e.key, pressed, e);
  }

  /**
   * 构造并发送键盘事件：可打印字符（无 Ctrl/Alt/Meta）走 KeyWithChar（IME 友好，
   * Host `enigo.text()` Unicode 注入）；功能键 / 快捷键组合走 Key（纯 VK）。
   */
  sendKey(vk: number, key: string, pressed: boolean, e?: KeyboardEvent): void {
    const modifiers = e ? modifierMask(e) : 0;
    if (pressed) this.keysDown.add(vk);
    else this.keysDown.delete(vk);
    const printable = key.length === 1;
    const shortcut = e ? e.ctrlKey || e.altKey || e.metaKey : false;
    if (printable && !shortcut) {
      this.emit({ kind: 'KeyWithChar', keyCode: vk, character: key, pressed, modifiers }, (seq) =>
        rdweb.build_input_key_with_char(seq, vk, key, pressed, modifiers),
      );
    } else {
      this.emit({ kind: 'Key', keyCode: vk, pressed, modifiers }, (seq) =>
        rdweb.build_input_key(seq, vk, pressed, modifiers),
      );
    }
  }

  // ── 直接构造 API（smoke 的 Host 侧回环断言用；同样受 blackhole 控制） ──

  sendMouseMove(x: number, y: number): void {
    this.emit({ kind: 'MouseMove', x, y }, (seq) => rdweb.build_input_mouse_move(seq, x, y));
  }

  sendMouseButton(button: number, pressed: boolean): void {
    this.emit({ kind: 'MouseButton', button, pressed }, (seq) =>
      rdweb.build_input_mouse_button(seq, button, pressed),
    );
  }

  sendMouseWheel(deltaX: number, deltaY: number): void {
    this.emit({ kind: 'MouseWheel', deltaX, deltaY }, (seq) =>
      rdweb.build_input_mouse_wheel(seq, deltaX, deltaY),
    );
  }

  /** 补发全部按下的鼠标按键抬起（pointerleave / blur / 断连防卡键）。 */
  releaseAllButtons(): void {
    for (const b of this.buttonsDown) this.sendMouseButton(b, false);
    this.buttonsDown.clear();
  }

  /** 补发全部按键抬起（鼠标 + 键盘）。 */
  releaseAll(): void {
    this.releaseAllButtons();
    for (const vk of this.keysDown) {
      this.emit({ kind: 'Key', keyCode: vk, pressed: false, modifiers: 0 }, (seq) =>
        rdweb.build_input_key(seq, vk, false, 0),
      );
    }
    this.keysDown.clear();
  }

  // ── 内部 ────────────────────────────────────────────────

  /**
   * 统一出口：分配 seq（blackhole 下也步进，保证遥测序号与真实发送一致）→
   * 记录遥测（`onSent`）→ blackhole 时丢弃，否则构造并交给 sink。
   */
  private emit(
    rec: DistributiveOmit<SentInputRecord, 'seq'>,
    build: (seq: bigint) => Uint8Array,
  ): void {
    const seq = this.seq;
    this.seq += 1n;
    this.opts.onSent?.({ ...rec, seq: seq.toString() } as SentInputRecord);
    if (this.blackhole) return;
    this.sink?.(build(seq));
  }
}
