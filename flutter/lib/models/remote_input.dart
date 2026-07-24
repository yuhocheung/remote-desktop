import '../ffi/rdcore_bindings.dart';

/// 远程输入事件种类，镜像 Rust 端 `InputKind` 经 FFI 编码的 `kind` 字段：
/// 0=MouseMove, 1=MouseButton, 2=MouseWheel, 3=Key, 4=KeyWithChar。
enum RdInputKind { mouseMove, mouseButton, mouseWheel, key, keyWithChar }

/// 一条远程输入事件（鼠标 / 键盘 / 滚轮）。
///
/// UI 在被控端（Viewer）捕获指针、键盘、滚轮后构建本对象，发给 Host 注入。
/// `keyWithChar` 携带 Unicode [character]（IME 友好，参考 RustDesk Map 模式双发），
/// 由 `ConnectionController.sendInputEvent` 走独立的 `rdcore_viewer_send_input_key`
/// FFI 路径（C struct 无法承载字符串，故 character 不写入 [NativeRdInputEvent]）。
class RdInputEvent {
  const RdInputEvent({
    required this.kind,
    this.x = 0,
    this.y = 0,
    this.button = 0,
    this.pressed = false,
    this.deltaX = 0,
    this.deltaY = 0,
    this.keyCode = 0,
    this.modifiers = 0,
    this.character,
  });

  final RdInputKind kind;

  /// MouseMove 的像素坐标（相对被控端屏幕）。
  final int x;
  final int y;

  /// MouseButton 的按键编号：0=Left 1=Middle 2=Right 3=Back 4=Forward。
  final int button;

  /// MouseButton / Key 的按下状态（MouseMove / MouseWheel 忽略）。
  final bool pressed;

  /// MouseWheel 的滚动增量（像素或行，符号依平台约定）。
  final int deltaX;
  final int deltaY;

  /// Key 的扫描码（平台相关）。
  final int keyCode;

  /// Key 的修饰键位掩码（Shift/Ctrl/Alt/Meta 等）。
  final int modifiers;

  /// KeyWithChar 的 Unicode 字符（IME 合成输入，如中文/日文）；其他 kind 为 null。
  final String? character;

  factory RdInputEvent.mouseMove(int x, int y) =>
      RdInputEvent(kind: RdInputKind.mouseMove, x: x, y: y);

  factory RdInputEvent.mouseButton(int button, bool pressed) =>
      RdInputEvent(kind: RdInputKind.mouseButton, button: button, pressed: pressed);

  factory RdInputEvent.mouseWheel(int deltaX, int deltaY) =>
      RdInputEvent(kind: RdInputKind.mouseWheel, deltaX: deltaX, deltaY: deltaY);

  factory RdInputEvent.key(int keyCode, bool pressed, {int modifiers = 0}) =>
      RdInputEvent(
        kind: RdInputKind.key,
        keyCode: keyCode,
        pressed: pressed,
        modifiers: modifiers,
      );

  /// 带字符的按键事件（IME 友好双发）。[character] 为合成文本（中文/日文），
  /// null 时退化为纯 scancode（快捷键/游戏）。
  factory RdInputEvent.keyWithChar(
    int keyCode,
    String? character,
    bool pressed, {
    int modifiers = 0,
  }) =>
      RdInputEvent(
        kind: RdInputKind.keyWithChar,
        keyCode: keyCode,
        character: character,
        pressed: pressed,
        modifiers: modifiers,
      );

  /// 把本事件写入原生 `RdInputEvent` struct（调用方负责分配 / 释放该 struct）。
  /// 注意：keyWithChar 的 character 不写入（走独立 FFI 路径），仅写数值字段。
  void writeInto(NativeRdInputEvent s) {
    s.kind = kind.index;
    s.x = x;
    s.y = y;
    s.button = button;
    s.pressed = pressed ? 1 : 0;
    s.deltaX = deltaX;
    s.deltaY = deltaY;
    s.keyCode = keyCode;
    s.modifiers = modifiers;
  }

  /// 从原生 `RdInputEvent` struct 读出（用于 Host 端轮询到的输入）。
  factory RdInputEvent.fromNative(NativeRdInputEvent s) {
    return RdInputEvent(
      kind: RdInputKind
          .values[s.kind.clamp(0, RdInputKind.keyWithChar.index).toInt()],
      x: s.x,
      y: s.y,
      button: s.button,
      pressed: s.pressed != 0,
      deltaX: s.deltaX,
      deltaY: s.deltaY,
      keyCode: s.keyCode,
      modifiers: s.modifiers,
    );
  }

  /// 序列化为可跨 isolate 端口传递的 Map（[character] 可能为 null）。
  Map<String, dynamic> toMap() => <String, dynamic>{
        'kind': kind.index,
        'x': x,
        'y': y,
        'button': button,
        'pressed': pressed,
        'deltaX': deltaX,
        'deltaY': deltaY,
        'keyCode': keyCode,
        'modifiers': modifiers,
        'character': character,
      };

  /// 从 [toMap] 反序列化（与后台 isolate 收发输入事件用）。
  factory RdInputEvent.fromMap(Map<String, dynamic> m) {
    final kindIdx = (m['kind'] as int?)?.clamp(0, RdInputKind.keyWithChar.index) ?? 0;
    return RdInputEvent(
      kind: RdInputKind.values[kindIdx],
      x: (m['x'] as int?) ?? 0,
      y: (m['y'] as int?) ?? 0,
      button: (m['button'] as int?) ?? 0,
      pressed: (m['pressed'] as bool?) ?? false,
      deltaX: (m['deltaX'] as int?) ?? 0,
      deltaY: (m['deltaY'] as int?) ?? 0,
      keyCode: (m['keyCode'] as int?) ?? 0,
      modifiers: (m['modifiers'] as int?) ?? 0,
      character: m['character'] as String?,
    );
  }

  @override
  String toString() =>
      'RdInputEvent(${kind.name}, x=$x, y=$y, button=$button, pressed=$pressed, '
      'dx=$deltaX, dy=$deltaY, key=$keyCode, mods=$modifiers, char=$character)';
}
