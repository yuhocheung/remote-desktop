import 'dart:async';
import 'dart:io' show Platform;

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../connection/connection_controller.dart';
import '../app/connection_manager.dart';
import '../models/remote_input.dart';
import 'remote_frame_view.dart';
import 'remote_texture_view.dart';
import 'security_banner.dart';

/// Viewer 侧的远程屏幕视图。
///
/// 顶部固定 [SecurityBanner]（不可伪造安全指示器）；下方根据连接阶段渲染：
/// - 已激活且为 Viewer：真实渲染来自媒体通道的 [RemoteFrameView]，并在画面区捕获
///   输入经 [ConnectionController.sendInputEvent] 发给 Host。输入支持两类来源：
///   - **物理鼠标 / 键盘 / 滚轮**：直接映射（左/右/中键、绝对坐标移动、滚轮、HID 按键）。
///   - **触摸屏**（参考 RustDesk InputModel + 微软 AVD 手势映射）：
///     单指轻点 = 左键、单指拖动 = 鼠标绝对坐标移动、长按 = 右键、双指拖动 = 滚轮。
///     鼠标与触摸按 `PointerEvent.kind` 自动切换，无需手动切模式。
/// - 已激活且为 Host：提示「正在被对方控制」并展示最近一次收到的输入；
/// - 等待同意 / 被拒绝 / 已关闭：对应状态卡片。
class RemoteScreen extends StatefulWidget {
  const RemoteScreen({
    super.key,
    required this.controller,
    this.manager,
    this.sessionId,
  });

  final ConnectionController controller;

  /// 用于「重新连接」的会话管理器与 id；演示等无管理器场景为 null（不显示重连按钮）。
  final ConnectionManager? manager;
  final String? sessionId;

  @override
  State<RemoteScreen> createState() => _RemoteScreenState();
}

class _RemoteScreenState extends State<RemoteScreen> {
  final FocusNode _focus = FocusNode();

  // —— 触摸手势状态（参考 RustDesk InputModel 的多指状态机）——
  /// 当前按下的指针 id → 屏幕位置。
  final Map<int, Offset> _pointers = {};
  /// 长按检测定时器（500ms 触发右键）。
  Timer? _longPressTimer;
  /// 单指 down 时的起始位置（判断 tap vs 拖动）。
  Offset? _singlePointerStart;
  /// 长按是否已触发（触发后 up 发右键 up，而非左键 tap）。
  bool _isLongPress = false;
  /// 双指模式（缩放 / 滚轮）。
  bool _isScaling = false;
  /// 上一帧双指中点（计算滚轮增量）。
  Offset? _lastPanMidpoint;

  // 鼠标 / 触摸模式自动切换（参考 RustDesk isPhysicalMouse）。
  // 鼠标 hover/down → true；触摸事件 → false。两模式走不同发送路径。
  bool _isPhysicalMouse = false;

  // 显示框（Listener 区域）的像素尺寸，由 LayoutBuilder 实时写入。
  // Viewer 屏幕上的局部坐标需据此 + Host 帧分辨率换算成绝对屏幕坐标。
  double _boxW = 0;
  double _boxH = 0;

  // —— 软键盘（移动端文本输入）——
  // 远程桌面是像素流，Viewer 无法感知 Host 哪处是输入框，故仅由工具栏「键盘」按钮手动唤起
  // 一个隐藏 TextField 来接管 iOS 软键盘（不自动弹，避免点非输入区误触发）；捕获到的字符
  // 经 keyWithChar 走 connectionSendInputKey（真实 WebRTC 路径，作用于 RdConnection）注入
  // Host（支持中文 IME）。
  // 关键修复：之前用裸 TextInput.attach(TextInputClient)，在 iOS 上 updateEditingValue
  // 不可靠（键盘能弹但字符不回调，表现即「输入无法同步」）；改用隐藏 TextField + controller
  // 监听，iOS 上稳定触发，是远程桌面软键盘的标准做法。
  final TextEditingController _kbController = TextEditingController();
  final FocusNode _kbFocus = FocusNode();
  bool _kbOpen = false;
  // —— 软键盘预热（移动端）——
  // iOS 软键盘（含中文 IME 词库）是懒加载的：App 内第一次唤起时系统要即时加载键盘进程，
  // 耗时可达 1~2s，表现为「第一次点键盘按钮延迟很久」，之后再点则即时弹出。
  // 解决：进入本页后趁「正在建立连接」阶段对隐藏输入框做一次极短的 focus/unfocus，
  // 让系统提前完成键盘实例化（业界标准做法）。每个 App 进程只需预热一次。
  static bool _kbPrewarmed = false;
  // 预热窗口标记：预热期间的焦点瞬变不同步 _kbOpen，避免工具栏键盘图标闪一下。
  bool _kbPrewarming = false;

  // —— 全屏模式（移动端）——
  // 开启时隐藏系统 UI（状态栏 / Home 指示条）+ 顶部安全指示器 + 底部工具栏，画面铺满整屏，
  // 提升远程桌面的可视面积。由画面右上角浮动按钮「全屏 / 退出全屏」切换。
  bool _fullscreen = false;
  // —— 适配宽度（全屏态可用）——
  // 开启后画面按显示框宽度等比缩放（保持受控端原始宽高比），左右居中、上下居中显示；
  // 关闭则恢复默认的拉伸铺满。仅在全屏状态下提供开关（全屏开关正下方的图标按钮）。
  bool _fitWidth = false;
  // 操作指引可见性：默认隐藏，点击底部右下角问号图标展开/收起。
  bool _showHint = false;
  // 已发往 Host 的「文本镜像」：Host 侧输入框内容不可读，故在本地维护一个模型用于计算
  // 增量——正向追加发字符、删除发退格、替换先退格再发新串。每次 TextField 回写都把它
  // 对齐到 controller 文本，从而正确处理 iOS 把英文逐字 / 中文拼音都标成 composing 的特性。
  String _kbSent = '';

  @override
  void initState() {
    super.initState();
    // 隐藏 TextField 的 controller 监听：iOS 软键盘回写经此稳定触发（裸 TextInputClient
    // 在 iOS 上 updateEditingValue 不可靠，故改用 TextField）。
    _kbController.addListener(_onKbChanged);
    // 焦点变化监听：捕获 iOS 软键盘自带「收起」按钮 / 下滑手势关闭，同步 _kbOpen 与本地镜像，
    // 否则 _kbOpen 会失同步（键盘已收但状态仍 true），导致再次点开关误判为「关闭」而弹不起来。
    _kbFocus.addListener(_onKbFocusChanged);
    // 连接断开时收起软键盘，避免键盘残留覆盖状态卡片。
    widget.controller.addListener(_onConnChanged);
    _prewarmKeyboard();
  }

  /// 软键盘预热：趁「正在建立连接」的等待阶段让 iOS/Android 提前实例化软键盘，
  /// 消除用户首次点键盘按钮时 1~2s 的系统级加载延迟（见字段注释）。
  /// 预热会伴随一次极短的键盘闪现，因此特意放在本页仍是状态卡片（连接中）时执行。
  void _prewarmKeyboard() {
    if (_kbPrewarmed) return;
    if (!(Platform.isIOS || Platform.isAndroid)) return; // 桌面端无软键盘，无需预热
    WidgetsBinding.instance.addPostFrameCallback((_) async {
      if (!mounted || _kbPrewarmed) return;
      _kbPrewarmed = true; // 确认真正执行后再置位（极端：进页一帧内退出则不消耗机会）
      _kbPrewarming = true;
      _kbFocus.requestFocus();
      // 持留一小段时间确保系统真正完成键盘实例化再收起；过短可能只触发回调而未加载。
      await Future<void>.delayed(const Duration(milliseconds: 150));
      _kbPrewarming = false;
      if (mounted) _kbFocus.unfocus();
    });
  }

  @override
  void dispose() {
    // 退出全屏：若当前处于全屏，离开页面时恢复系统 UI 与竖屏方向，避免影响后续页面。
    if (_fullscreen) {
      SystemChrome.setEnabledSystemUIMode(SystemUiMode.edgeToEdge);
      SystemChrome.setPreferredOrientations([DeviceOrientation.portraitUp]);
    }
    _cancelLongPress();
    _kbController.removeListener(_onKbChanged);
    _kbFocus.removeListener(_onKbFocusChanged);
    widget.controller.removeListener(_onConnChanged);
    _kbController.dispose();
    _kbFocus.dispose();
    _focus.dispose();
    super.dispose();
  }

  /// 连接断开 / 关闭时主动收起软键盘，并退出全屏（恢复系统 UI + 竖屏）。
  /// 否则连接断开后画面切到状态卡片、全屏按钮随之消失，会卡在沉浸 / 横屏且无入口退出。
  void _onConnChanged() {
    if (!widget.controller.isActive) {
      if (_kbOpen) {
        _kbFocus.unfocus();
        _kbSent = '';
        if (mounted) setState(() => _kbOpen = false);
      }
      if (_fullscreen) {
        if (mounted) setState(() => _fullscreen = false);
        SystemChrome.setEnabledSystemUIMode(SystemUiMode.edgeToEdge);
        SystemChrome.setPreferredOrientations([DeviceOrientation.portraitUp]);
      }
    }
  }

  /// 从「已关闭 / 连接失败」状态一键重连：复用本会话保留的配对邀请，
  /// 以同一 id 开一条全新连接并替换当前页面（pushReplacement），避免旧 controller
  /// 残留、也避免重复 id 命中陈旧会话。无管理器 / 无配对信息时退化为提示。
  Future<void> _reconnect() async {
    final mgr = widget.manager;
    final id = widget.sessionId;
    if (mgr == null || id == null) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(content: Text('该会话无法重连，请返回主页重新配对')),
        );
      }
      return;
    }
    try {
      final newId = await mgr.reconnect(id);
      if (!mounted) return;
      final entry = mgr.sessionById(newId);
      if (entry != null) {
        // 用新 controller 替换当前页面，旧页面 dispose 时其（已释放的）controller
        // 因幂等保护不会二次释放。
        Navigator.of(context).pushReplacement(
          MaterialPageRoute(
            builder: (_) => RemoteScreen(
              manager: mgr,
              sessionId: newId,
              controller: entry.controller,
            ),
          ),
        );
      }
    } on Object catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('重新连接失败：$e'), duration: const Duration(seconds: 6)),
      );
    }
  }

  /// 鼠标 buttons 位掩码 → 0=左 / 2=右 / 1=中。
  int _mapMouseButton(int buttons) {
    if ((buttons & kPrimaryMouseButton) != 0) return 0;
    if ((buttons & kSecondaryMouseButton) != 0) return 2;
    if ((buttons & kMiddleMouseButton) != 0) return 1;
    return 0;
  }

  /// 把 Viewer 显示框内的局部坐标（Listener 的 localPosition）换算成 Host 绝对屏幕坐标。
  ///
  /// 默认：远端画面以 [RemoteFrameView] 拉伸铺满显示框（无 letterbox），故为线性直接缩放：
  ///   hostX = localX / boxW * frameW，hostY = localY / boxH * frameH。
  /// 适配宽度（_fitWidth）：画面按宽度等比缩放、上下居中，需先减去垂直居中偏移再换算：
  ///   drawH = boxW * frameH / frameW，offsetY = (boxH - drawH) / 2，
  ///   hostX = localX / boxW * frameW，hostY = (localY - offsetY) / drawH * frameH。
  /// 若尚未拿到帧或显示框尺寸为 0，退化为原始坐标（至少能让事件发出去，便于排查）。
  Offset _mapToHost(Offset local) {
    final f = widget.controller.frameSize;
    if (f == null || f.width <= 0 || f.height <= 0 || _boxW <= 0 || _boxH <= 0) {
      return local;
    }
    if (_fitWidth) {
      final drawH = _boxW * f.height / f.width;
      final offsetY = (_boxH - drawH) / 2;
      final x = (local.dx / _boxW * f.width).clamp(0.0, f.width.toDouble());
      final y = ((local.dy - offsetY) / drawH * f.height)
          .clamp(0.0, f.height.toDouble());
      return Offset(x, y);
    }
    final x = (local.dx / _boxW * f.width).clamp(0.0, f.width.toDouble());
    final y = (local.dy / _boxH * f.height).clamp(0.0, f.height.toDouble());
    return Offset(x, y);
  }

  void _onPointerDown(PointerDownEvent e) {
    // 先把光标移到触点（绝对坐标）。触摸 tap 没有独立的 move 事件，若不在 down 时
    // 先 move，按钮事件会落在「上次光标位置」（初次为 0,0），点击就打不到用户点的地方。
    final host = _mapToHost(e.localPosition);
    if (e.kind == PointerDeviceKind.mouse) {
      _isPhysicalMouse = true;
      _cancelLongPress();
      _pointers.clear();
      widget.controller.sendInputEvent(
          RdInputEvent.mouseMove(host.dx.round(), host.dy.round()));
      widget.controller.sendInputEvent(
          RdInputEvent.mouseButton(_mapMouseButton(e.buttons), true));
      return;
    }
    // 触摸：记录指针。单指启动长按计时；双指进入滚轮模式。
    _isPhysicalMouse = false;
    _pointers[e.pointer] = e.position;
    widget.controller.sendInputEvent(
        RdInputEvent.mouseMove(host.dx.round(), host.dy.round()));
    if (_pointers.length == 1) {
      _singlePointerStart = e.position;
      _isLongPress = false;
      _startLongPressTimer();
    } else if (_pointers.length == 2) {
      _cancelLongPress();
      if (_isLongPress) {
        // 长按进行中来了第二指：先抬起右键，再进双指模式。
        widget.controller.sendInputEvent(RdInputEvent.mouseButton(2, false));
        _isLongPress = false;
      }
      _isScaling = true;
      _lastPanMidpoint = _currentMidpoint();
    }
  }

  void _onPointerMove(PointerMoveEvent e) {
    if (e.kind == PointerDeviceKind.mouse) {
      if (_isPhysicalMouse) {
        final h = _mapToHost(e.localPosition);
        widget.controller.sendInputEvent(
            RdInputEvent.mouseMove(h.dx.round(), h.dy.round()));
      }
      return;
    }
    _pointers[e.pointer] = e.position;
    if (_isScaling && _pointers.length >= 2) {
      _handleScaleUpdate();
    } else if (_pointers.length == 1 && !_isLongPress) {
      // 单指拖动：超出 tap 阈值则取消长按，发绝对坐标移动。
      if (_singlePointerStart != null &&
          (e.position - _singlePointerStart!).distance > kTouchSlop) {
        _cancelLongPress();
      }
      final h = _mapToHost(e.localPosition);
      widget.controller.sendInputEvent(
          RdInputEvent.mouseMove(h.dx.round(), h.dy.round()));
    }
  }

  void _onPointerUp(PointerUpEvent e) {
    if (e.kind == PointerDeviceKind.mouse) {
      if (_isPhysicalMouse) {
        widget.controller.sendInputEvent(
            RdInputEvent.mouseButton(_mapMouseButton(e.buttons), false));
      }
      return;
    }
    _pointers.remove(e.pointer);
    if (_isScaling) {
      if (_pointers.length < 2) {
        _isScaling = false;
        _lastPanMidpoint = null;
      }
      // 双指模式抬起不发点击。
      return;
    }
    // 单指抬起：判断是 tap、长按、还是拖动。
    final moved = _singlePointerStart != null &&
        (e.position - _singlePointerStart!).distance > kTouchSlop;
    if (_isLongPress) {
      // 长按结束 → 右键 up。
      widget.controller.sendInputEvent(RdInputEvent.mouseButton(2, false));
    } else if (!moved) {
      // 未长按且未大移动 → 左键 tap（down + up）。
      widget.controller.sendInputEvent(RdInputEvent.mouseButton(0, true));
      widget.controller.sendInputEvent(RdInputEvent.mouseButton(0, false));
    }
    _cancelLongPress();
    _singlePointerStart = null;
    _isLongPress = false;
  }

  /// 鼠标滚轮（触摸屏不产生 PointerScrollEvent，双指走 [_handleScaleUpdate]）。
  void _onPointerSignal(PointerSignalEvent e) {
    if (e is PointerScrollEvent) {
      widget.controller.sendInputEvent(RdInputEvent.mouseWheel(
        e.scrollDelta.dx.round(),
        e.scrollDelta.dy.round(),
      ));
    }
  }

  void _onKey(KeyEvent e) {
    if (e is KeyRepeatEvent) return;
    widget.controller.sendInputEvent(RdInputEvent.keyWithChar(
      e.physicalKey.usbHidUsage,
      e.character,
      e is KeyDownEvent,
      modifiers: _modifierMask(e),
    ));
  }

  int _modifierMask(KeyEvent e) {
    var m = 0;
    if (HardwareKeyboard.instance.isShiftPressed) m |= 1;
    if (HardwareKeyboard.instance.isControlPressed) m |= 2;
    if (HardwareKeyboard.instance.isAltPressed) m |= 4;
    if (HardwareKeyboard.instance.isMetaPressed) m |= 8;
    return m;
  }

  // —— 全屏：切换沉浸模式（隐藏系统 UI + 工具栏/安全指示器，画面铺满）+ 强制横屏 ——
  void _toggleFullscreen() {
    final next = !_fullscreen;
    // 退出全屏时一并复位「适配宽度」，恢复默认拉伸铺满（开关入口仅在全屏态可见）。
    if (mounted) setState(() {
      _fullscreen = next;
      if (!next) {
        _fitWidth = false;
      }
    });
    if (next) {
      // 进入全屏：隐藏状态栏与导航栏（iOS sticky 沉浸，用户上滑可临时唤出）；
      // 远程桌面以横屏展示更贴合 Host 桌面宽屏比例。
      SystemChrome.setEnabledSystemUIMode(SystemUiMode.immersiveSticky);
      SystemChrome.setPreferredOrientations([
        DeviceOrientation.landscapeLeft,
        DeviceOrientation.landscapeRight,
      ]);
    } else {
      // 退出全屏：恢复边缘到边缘（透明状态栏 / Home 指示条）+ 竖屏。
      SystemChrome.setEnabledSystemUIMode(SystemUiMode.edgeToEdge);
      SystemChrome.setPreferredOrientations([DeviceOrientation.portraitUp]);
    }
  }

  // —— 软键盘：唤起 iOS 软键盘并捕获文本输入注入 Host ——
  /// 软键盘焦点实际变化（含系统自带「收起」按钮 / 下滑手势关闭）的回调：
  /// 把 _kbOpen 与真实焦点同步，并清空本地文本镜像，避免状态与键盘可见性失同步。
  void _onKbFocusChanged() {
    if (_kbPrewarming) return; // 预热期间的焦点瞬变不同步 UI 状态，避免键盘图标闪一下
    final has = _kbFocus.hasFocus;
    if (!has) _kbSent = '';
    if (mounted && _kbOpen != has) setState(() => _kbOpen = has);
  }

  void _toggleKeyboard() {
    // 以真实焦点状态为准（而非缓存的 _kbOpen），确保系统已收起键盘后再次点按能正确唤起。
    if (_kbFocus.hasFocus) {
      _kbFocus.unfocus();
      return;
    }
    // 重新唤起时清空本地镜像 + 隐藏输入框内容，避免与 Host 实际文本错位。
    _kbSent = '';
    _kbController.value = TextEditingValue.empty;
    _kbFocus.requestFocus();
    if (mounted) setState(() => _kbOpen = true);
  }

  /// 从本机（iPhone）剪贴板读取文本，整段经已打通的 keyWithChar 通道发给 Host。
  /// Host 的 EnigoInputInjector 用 enigo.text() 整段注入（原子、不依赖软键盘 IME），
  /// 稳传大段文字，避免逐字符经 IME 合成导致的错位/丢失。等价于「剪贴板同步」的稳传效果。
  Future<void> _pasteFromClipboard() async {
    final data = await Clipboard.getData(Clipboard.kTextPlain);
    if (!mounted) return;
    final text = data?.text;
    if (text == null || text.isEmpty) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('剪贴板为空，无可粘贴内容')),
      );
      return;
    }
    widget.controller
        .sendInputEvent(RdInputEvent.keyWithChar(0, text, true));
    // 同步本地镜像：Host 已收到这段文本，后续键盘输入增量才能正确对齐。
    _kbSent += text;
    if (_kbOpen) {
      _kbController.value = TextEditingValue(
        text: _kbController.text + text,
        selection:
            TextSelection.collapsed(offset: _kbController.text.length + text.length),
      );
    }
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(content: Text('已粘贴 ${text.length} 个字符到受控端')),
    );
  }

  // —— 软键盘文本同步（由隐藏 TextField 的 controller 监听驱动）——
  /// 隐藏 TextField 的 controller 监听：每次软键盘回写都对齐本地 Host 文本镜像 [_kbSent]。
  /// 逻辑与旧 TextInputClient.updateEditingValue 一致（composing 阶段处理 iOS IME 特性）。
  void _onKbChanged() {
    final value = _kbController.value;
    final text = value.text;
    final comp = value.composing;
    final composingActive = comp.isValid && !comp.isCollapsed;
    if (composingActive) {
      final composingStr = text.substring(comp.start, comp.end);
      // 合成文本为纯 ASCII（英文 / 拼音）：实时同步整段（含合成区）。
      // 若合成区变短（退格删拼音字母 / 取消候选），text 同步缩短 → _syncTo 检测为删除发退格。
      final target = _isAscii(composingStr)
          ? text
          : text.replaceRange(comp.start, comp.end, '');
      _syncTo(target);
      return;
    }
    _syncTo(text);
  }

  /// 把 Host 侧文本镜像对齐到 [target]：与本地模型 [_kbSent] 比较，计算最小输入动作。
  void _syncTo(String target) {
    if (_kbSent.isEmpty) {
      if (target.isNotEmpty) _sendText(target);
      return;
    }
    if (target.startsWith(_kbSent)) {
      // 正常正向输入 / 中文选词落定：发送相对 _kbSent 的增量。
      final added = target.substring(_kbSent.length);
      if (added.isNotEmpty) _sendText(added);
      return;
    }
    if (_kbSent.startsWith(target)) {
      // 删除（英文退格 / 拼音取消）：逐字符发退格，Host 侧同步删。
      final n = _kbSent.length - target.length;
      for (var i = 0; i < n; i++) {
        _sendBackspace();
      }
      return;
    }
    // 替换（自动纠错 / 中文选词覆盖拼音）：先退格删旧串，再发新串。
    final common = _commonPrefix(_kbSent, target);
    for (var i = 0; i < _kbSent.length - common; i++) {
      _sendBackspace();
    }
    final added = target.substring(common);
    if (added.isNotEmpty) _sendText(added);
  }

  /// 经 keyWithChar 整段注入 Host（enigo.text 原子注入，支持中文 / 英文）。
  void _sendText(String s) {
    widget.controller.sendInputEvent(RdInputEvent.keyWithChar(0, s, true));
    _kbSent += s;
  }

  /// 发一次退格（Windows Host：VK_BACK=0x08 经 enigo 注入；其它 Host OS 键码映射不同）。
  void _sendBackspace() {
    widget.controller.sendInputEvent(RdInputEvent.keyWithChar(0x08, '', true));
    widget.controller.sendInputEvent(RdInputEvent.keyWithChar(0x08, '', false));
    if (_kbSent.isNotEmpty) _kbSent = _kbSent.substring(0, _kbSent.length - 1);
  }

  bool _isAscii(String s) => s.runes.every((r) => r < 0x80);

  int _commonPrefix(String a, String b) {
    final n = a.length < b.length ? a.length : b.length;
    var i = 0;
    for (; i < n; i++) {
      if (a.codeUnitAt(i) != b.codeUnitAt(i)) break;
    }
    return i;
  }

  // —— 触摸手势辅助 ——

  void _startLongPressTimer() {
    _longPressTimer = Timer(const Duration(milliseconds: 500), () {
      _isLongPress = true;
      // 长按 → 右键按下（抬起时在 _onPointerUp 发右键 up）。
      widget.controller.sendInputEvent(RdInputEvent.mouseButton(2, true));
    });
  }

  void _cancelLongPress() {
    _longPressTimer?.cancel();
    _longPressTimer = null;
  }

  void _handleScaleUpdate() {
    final mid = _currentMidpoint();
    if (_lastPanMidpoint != null) {
      final delta = mid - _lastPanMidpoint!;
      // 双指拖动 → 滚轮（垂直为主，水平也发）。
      if (delta.dx.abs() > 1 || delta.dy.abs() > 1) {
        widget.controller.sendInputEvent(RdInputEvent.mouseWheel(
            delta.dx.round(), delta.dy.round()));
      }
    }
    _lastPanMidpoint = mid;
  }

  Offset _currentMidpoint() {
    if (_pointers.isEmpty) return Offset.zero;
    var sum = Offset.zero;
    for (final p in _pointers.values) {
      sum += p;
    }
    return sum / _pointers.length.toDouble();
  }

  /// 画面右上角控制簇：常驻全屏开关；其正下方统一叠加「软键盘」与「粘贴（剪贴板）」按钮，
  /// 全屏态（底部工具栏被隐藏）与非全屏态共用同一位置，避免两态布局不一致、入口分裂。
  /// 全屏态下额外在全屏开关正下方显示「适配宽度」开关（保持受控端原始宽高比）。
  Widget _fullscreenButton(ColorScheme cs) {
    final btnStyle = IconButton.styleFrom(
      backgroundColor: cs.surface.withValues(alpha: 0.3),
      foregroundColor: cs.onSurface,
    );
    return Positioned(
      top: 8,
      right: 8,
      child: Material(
        type: MaterialType.transparency,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            IconButton(
              icon: Icon(
                _fullscreen ? Icons.fullscreen_exit_rounded : Icons.fullscreen_rounded,
              ),
              tooltip: _fullscreen ? '退出全屏' : '全屏',
              visualDensity: VisualDensity.compact,
              style: btnStyle,
              onPressed: _toggleFullscreen,
            ),
            // 适配宽度按钮：仅全屏态显示，位于全屏开关正下方。开启后画面按显示框
            // 宽度等比缩放（保持受控端原始宽高比），左右居中；再次点击恢复拉伸铺满。
            if (_fullscreen)
              Padding(
                padding: const EdgeInsets.only(top: 10),
                child: IconButton(
                  icon: Icon(
                    _fitWidth
                        ? Icons.fit_screen_rounded
                        : Icons.fit_screen_outlined,
                  ),
                  tooltip: _fitWidth ? '恢复铺满' : '适配宽度（原始比例）',
                  visualDensity: VisualDensity.compact,
                  style: btnStyle,
                  onPressed: () => setState(() => _fitWidth = !_fitWidth),
                ),
              ),
            // 软键盘按钮：始终位于全屏开关正下方（间隔 10px），全屏/非全屏共用同一入口。
            Padding(
              padding: const EdgeInsets.only(top: 10),
              child: IconButton(
                icon: Icon(
                  _kbOpen ? Icons.keyboard_alt_rounded : Icons.keyboard_rounded,
                ),
                tooltip: '软键盘',
                visualDensity: VisualDensity.compact,
                style: btnStyle,
                onPressed: _toggleKeyboard,
              ),
            ),
            // 剪贴板（粘贴）按钮：同样位于全屏开关下方，与软键盘按钮统一排布。
            Padding(
              padding: const EdgeInsets.only(top: 10),
              child: IconButton(
                icon: const Icon(Icons.content_paste_rounded),
                tooltip: '粘贴（从本机剪贴板）',
                visualDensity: VisualDensity.compact,
                style: btnStyle,
                onPressed: _pasteFromClipboard,
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// Viewer 侧音频控件：静音切换 + 音量滑块 + 实时电平条。
  /// 数据来自 [ConnectionController]（拉到的远端音频帧 + 本地静音/音量状态）。
  /// 已连接但尚无画面的占位：等待首帧时显示转圈；超时/出错时显示可诊断提示，
  /// 避免「连上了却永远 loading」且无从排查。
  Widget _noVideoPlaceholder(ConnectionController c, ColorScheme cs) {
    final err = c.frameError;
    final stalled = c.frameStalled;
    return Center(
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 28),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (err == null && !stalled)
              const CircularProgressIndicator()
            else
              Icon(
                Icons.desktop_access_disabled_outlined,
                size: 44,
                color: cs.error,
              ),
            const SizedBox(height: 16),
            Text(
              err == null && !stalled ? '等待首帧…' : '已连接，但未显示画面',
              style: Theme.of(context).textTheme.titleMedium?.copyWith(
                    color: err == null && !stalled ? cs.onSurface : cs.error,
                  ),
              textAlign: TextAlign.center,
            ),
            if (err != null) ...[
              const SizedBox(height: 12),
              SelectableText(
                err,
                style: Theme.of(context).textTheme.bodySmall?.copyWith(
                      color: cs.onSurfaceVariant,
                      height: 1.5,
                    ),
                textAlign: TextAlign.center,
              ),
            ],
            const SizedBox(height: 10),
            Text(
              '连接已建立。若长时间无画面，请确认被控端程序 '
              '正在抓屏并推送视频流。',
              style: Theme.of(context).textTheme.bodySmall?.copyWith(
                    color: cs.onSurfaceVariant,
                  ),
              textAlign: TextAlign.center,
            ),
          ],
        ),
      ),
    );
  }

  // ───────────────── 统一设计令牌 ─────────────────
  static const double _kRadius = 22.0;
  static const double _kPad = 24.0;

  /// 阶段背景：柔和垂直渐变 + 安全内边距，杜绝「黑屏空旷」感。
  /// 顶部/底部安全边距由外层 [build] 的 SafeArea 统一处理，此处不再重复（避免双重上边距）。
  Widget _stage({required Widget child}) {
    final cs = Theme.of(context).colorScheme;
    return Container(
      width: double.infinity,
      height: double.infinity,
      decoration: BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.topCenter,
          end: Alignment.bottomCenter,
          colors: [
            cs.surface,
            cs.surfaceContainerLowest,
          ],
        ),
      ),
      child: Padding(
        padding: const EdgeInsets.all(_kPad),
        child: child,
      ),
    );
  }

  /// 居中状态卡片（连接中 / 等待 / 错误 / 拒绝 / 关闭 共用）。
  Widget _centerCard({
    required Widget icon,
    required String title,
    required String subtitle,
    required Color accent,
    Widget? detail,
    List<Widget>? actions,
  }) {
    final cs = Theme.of(context).colorScheme;
    return _stage(
      child: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 420),
          child: Card(
            elevation: 0,
            color: cs.surfaceContainerHigh,
            shape: RoundedRectangleBorder(
              borderRadius: BorderRadius.circular(_kRadius),
              side: BorderSide(color: cs.outlineVariant.withValues(alpha: 0.6)),
            ),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 28, vertical: 32),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  icon,
                  const SizedBox(height: 20),
                  Text(
                    title,
                    textAlign: TextAlign.center,
                    style: Theme.of(context).textTheme.titleLarge?.copyWith(
                          fontWeight: FontWeight.w700,
                          letterSpacing: 0.2,
                        ),
                  ),
                  const SizedBox(height: 10),
                  Text(
                    subtitle,
                    textAlign: TextAlign.center,
                    style: Theme.of(context).textTheme.bodyMedium?.copyWith(
                          color: cs.onSurfaceVariant,
                          height: 1.5,
                        ),
                  ),
                  if (detail != null) ...[
                    const SizedBox(height: 22),
                    detail,
                  ],
                  if (actions != null && actions.isNotEmpty) ...[
                    const SizedBox(height: 26),
                    ...actions,
                  ],
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }

  /// 连接进度步骤条（连接中 / 等待对端确认 共用）。
  Widget _progressSteps({required int activeStep}) {
    final cs = Theme.of(context).colorScheme;
    final labels = ['发起连接', '协商加密通道', '等待对端确认'];
    return Column(
      children: [
        Row(
          children: [
            for (var i = 0; i < labels.length; i++) ...[
              _stepNode(index: i, active: activeStep, current: i == activeStep),
              if (i < labels.length - 1)
                Expanded(
                  child: Container(
                    height: 2,
                    margin: const EdgeInsets.symmetric(horizontal: 6),
                    decoration: BoxDecoration(
                      color: i < activeStep
                          ? cs.primary
                          : cs.outlineVariant,
                      borderRadius: BorderRadius.circular(2),
                    ),
                  ),
                ),
            ],
          ],
        ),
        const SizedBox(height: 8),
        Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            for (final l in labels)
              Expanded(
                child: Text(
                  l,
                  textAlign: TextAlign.center,
                  style: Theme.of(context).textTheme.labelSmall?.copyWith(
                        color: cs.onSurfaceVariant,
                      ),
                ),
              ),
          ],
        ),
      ],
    );
  }

  Widget _stepNode(
      {required int index, required int active, required bool current}) {
    final cs = Theme.of(context).colorScheme;
    final done = index < active;
    final color = done || current ? cs.primary : cs.outlineVariant;
    return Container(
      width: 26,
      height: 26,
      decoration: BoxDecoration(
        color: done ? cs.primary : Colors.transparent,
        border: Border.all(color: color, width: 2),
        shape: BoxShape.circle,
      ),
      child: done
          ? Icon(Icons.check, size: 16, color: cs.onPrimary)
          : current
              ? SizedBox(
                  width: 12,
                  height: 12,
                  child: CircularProgressIndicator(
                    strokeWidth: 2.5,
                    color: cs.primary,
                  ),
                )
              : Center(
                  child: Text(
                    '${index + 1}',
                    style: TextStyle(
                      fontSize: 11,
                      color: cs.onSurfaceVariant,
                      fontWeight: FontWeight.w600,
                    ),
                  ),
                ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return ListenableBuilder(
      listenable: widget.controller,
      builder: (context, _) {
        // 整页统一 SafeArea：顶部让出刘海 / 状态栏，底部让出 Home Indicator，
        // 使 SecurityBanner 不被遮挡（RemoteScreen 作为独立路由，无外层 Scaffold/AppBar）。
        // 外层 Container 铺满全屏并用主题 surface 色打底：SafeArea 上推后，状态栏区域
        // 透出的是本 App 背景色而非 iOS 原生黑色（否则顶部会有一条黑边）。
        // 用 Scaffold 包一层：提供本页的 ScaffoldMessenger，使「连接没反应？」的
        // SnackBar 能在本页底部直接弹出，而不是被排到上层 home_screen 的队列（退出后才显示）。
        return Scaffold(
          backgroundColor: cs.surface,
          resizeToAvoidBottomInset: false,
          body: Container(
            // 全屏沉浸态：背景用纯黑，左右两侧硬件安全区（刘海 / 圆角）会以黑条呈现
            // （而非 surface 色）；非全屏保持 surface 底色。
            color: _fullscreen ? Colors.black : cs.surface,
            child: SafeArea(
              // 全屏沉浸态：顶部 / 底部 inset 关闭（画面铺到边缘，沉浸无底部空白）。
              // 左 / 右 inset 始终保留——刘海屏横屏 notch、屏幕圆角均需安全间隔，
              // 由上方黑色背景承载；底部署 Home 指示条区仍铺满（沉浸体验）。
              top: !_fullscreen,
              bottom: !_fullscreen,
              left: true,
              right: true,
              // 关键：软键盘弹出时 MediaQuery.padding.bottom 会被键盘「吃掉」（键盘覆盖
              // Home 指示条区 → padding.bottom 从 ~34 塌缩为 0），SafeArea 底部衬垫随之
              // 消失 → 画面框变高 ~34px、Texture 按框拉伸 → 画面抖动/被拉长，收起键盘才
              // 恢复。改为按 viewPadding（不受键盘影响）维持底部衬垫，键盘开合零位移。
              maintainBottomViewPadding: true,
              child: Stack(
                children: [
                  Column(
                    children: [
                      // 全屏模式下隐藏顶部安全指示器，画面铺满。
                      if (!_fullscreen)
                        SecurityBanner(snapshot: widget.controller.indicator),
                      Expanded(child: _body()),
                    ],
                  ),
                  // 隐藏的软键盘输入框：移出屏幕外但仍在 widget 树、可聚焦，用于稳定接管 iOS
                  // 软键盘。其 controller 监听驱动字符注入 Host。IgnorePointer + Opacity(0)
                  // 确保不挡视线、不抢触摸。裸 TextInputClient 在 iOS 上 updateEditingValue
                  // 不可靠，故改用隐藏 TextField（业界标准做法）。
                  Positioned(
                    left: -10000,
                    child: Opacity(
                      opacity: 0.0,
                      child: IgnorePointer(
                        child: SizedBox(
                          width: 240,
                          child: TextField(
                            controller: _kbController,
                            focusNode: _kbFocus,
                            keyboardType: TextInputType.multiline,
                            maxLines: null,
                            // 关键：关闭自动纠错 / 建议。否则 iOS 会把整词标成 composing（灰色候选），
                            // 此时按退格键只是「取消候选标记」而非删除字符 —— _kbController 文本长度不变，
                            // delta 算出无增量，退格事件根本不发出（表现就是「退格键没反应」）。
                            // 关闭后英文 / 已确认文本即时落定，退格直接删字符、被 _syncTo 检测到并发往 Host。
                            // 关 autocorrect 不影响中文 IME：拼音合成是独立机制，仍走 composing 分支。
                            autocorrect: false,
                            enableSuggestions: false,
                          ),
                        ),
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        );
      },
    );
  }

  Widget _body() {
    final c = widget.controller;
    final cs = Theme.of(context).colorScheme;

    // 连接错误优先展示（握手失败 / FFI 不可用等原因），否则失败会被「正在建立连接…」掩盖。
    if (c.error != null) {
      return _centerCard(
        icon: Container(
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: cs.errorContainer,
            shape: BoxShape.circle,
          ),
          child: Icon(Icons.error_outline_rounded, color: cs.error, size: 30),
        ),
        title: '连接失败',
        subtitle: '无法与主机建立端到端连接，请检查下方原因后重试。',
        accent: cs.error,
        detail: ConstrainedBox(
          constraints: const BoxConstraints(maxHeight: 180),
          child: SingleChildScrollView(
            child: Container(
              width: double.infinity,
              padding: const EdgeInsets.all(14),
              decoration: BoxDecoration(
                color: cs.errorContainer.withValues(alpha: 0.5),
                borderRadius: BorderRadius.circular(14),
              ),
              child: SelectableText(
                c.error!,
                style: Theme.of(context).textTheme.bodySmall?.copyWith(
                      color: cs.onErrorContainer,
                      height: 1.5,
                    ),
              ),
            ),
          ),
        ),
        actions: [
          FilledButton.icon(
            icon: const Icon(Icons.arrow_back_rounded),
            label: const Text('返回'),
            onPressed: () => Navigator.of(context).pop(),
          ),
        ],
      );
    }

    if (c.isActive && !c.isHost) {
      return Stack(
        children: [
          // 主布局流：画面铺满 + 底部工具栏（断开居中）。浮层另叠其上，不影响布局。
          Column(
            children: [
              Expanded(
                child: Container(
              // 全屏：画面铺满；非全屏：留 12px 边距 + 圆角边框。
              margin: _fullscreen ? EdgeInsets.zero : const EdgeInsets.all(12),
              decoration: _fullscreen
                  ? null
                  : BoxDecoration(
                      border: Border.all(color: cs.outlineVariant),
                      borderRadius: BorderRadius.circular(16),
                    ),
              child: ClipRRect(
                borderRadius:
                    _fullscreen ? BorderRadius.zero : BorderRadius.circular(16),
                // 画面区域叠加层：全屏开关常驻于画面右上角（而非顶部栏），
                // 非全屏 / 全屏两种态都贴着画面右上角，便于随时切换。
                child: Stack(
                  children: [
                    KeyboardListener(
                      focusNode: _focus,
                      onKeyEvent: _onKey,
                      child: LayoutBuilder(
                        builder: (ctx, constraints) {
                          // 记录显示框尺寸，供 _mapToHost 把局部坐标换算成 Host 绝对坐标。
                          _boxW = constraints.maxWidth;
                          _boxH = constraints.maxHeight;
                          // 画面内容：纹理 / 帧视图默认拉伸铺满父约束；适配宽度时
                          // 在外层给定按受控端原始宽高比算好的约束，自然得到保比例显示。
                          final Widget video =
                              c.usingTexture && c.textureId != null
                                  ? RemoteTextureView(textureId: c.textureId!)
                                  : (c.lastFrame != null
                                      ? RemoteFrameView(frame: c.lastFrame)
                                      : _noVideoPlaceholder(c, cs));
                          final f = c.frameSize;
                          final Widget child = _fitWidth &&
                                  f != null &&
                                  f.width > 0 &&
                                  f.height > 0
                              ? Center(
                                  child: SizedBox(
                                    width: constraints.maxWidth,
                                    height: constraints.maxWidth *
                                        f.height /
                                        f.width,
                                    child: video,
                                  ),
                                )
                              : video;
                          return Listener(
                            onPointerDown: (e) {
                              // 软键盘打开时不要抢走焦点（否则会令 iOS 收起键盘）；
                              // 硬件键盘焦点仅在键盘关闭时请求。
                              if (!_kbOpen) _focus.requestFocus();
                              _onPointerDown(e);
                            },
                            onPointerMove: _onPointerMove,
                            onPointerUp: _onPointerUp,
                            onPointerSignal: _onPointerSignal,
                            child: child,
                          );
                        },
                      ),
                    ),
                // 画面右上角全屏开关（叠在画面上）。
                _fullscreenButton(cs),
              ],
                ),
              ),
            ),
          ),
          // 全屏模式下隐藏底部工具栏（断开/问号），画面铺满；键盘/粘贴已统一到画面右上角控制簇。
          if (!_fullscreen)
            Padding(
              padding: const EdgeInsets.fromLTRB(14, 10, 14, 14),
              child: Stack(
                children: [
                  Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Row(
                        mainAxisAlignment: MainAxisAlignment.center,
                        children: [
                          FilledButton.icon(
                            icon: const Icon(Icons.link_off_rounded),
                            label: const Text('断开'),
                            style: FilledButton.styleFrom(
                              backgroundColor: cs.errorContainer,
                              foregroundColor: cs.onErrorContainer,
                            ),
                            onPressed: () {
                              c.revoke();
                              Navigator.of(context).pop();
                            },
                          ),
                        ],
                      ),
                    ],
                  ),
                  // 右下角问号：点击展开/收起操作指引（默认隐藏，避免常驻占用空间）。
                  Positioned(
                    right: 0,
                    bottom: 0,
                    child: IconButton(
                      icon: const Icon(Icons.help_outline_rounded),
                      tooltip: '操作指引',
                      visualDensity: VisualDensity.compact,
                      onPressed: () => setState(() => _showHint = !_showHint),
                    ),
                  ),
                ],
              ),
            ),
          // 主布局流结束（画面 + 底部工具栏），以下为浮层。
          ],
        ),
          // 操作指引浮层：点击右下角问号展开，绝对定位浮于画面之上，不影响远程画面与工具栏布局。
          if (_showHint && !_fullscreen)
            Positioned(
              left: 14,
              right: 14,
              bottom: 72,
              child: Material(
                type: MaterialType.transparency,
                child: Container(
                  padding:
                      const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
                  decoration: BoxDecoration(
                    color: cs.surfaceContainerHighest.withValues(alpha: 0.92),
                    borderRadius: BorderRadius.circular(14),
                    border: Border.all(
                      color: cs.outlineVariant.withValues(alpha: 0.6),
                    ),
                  ),
                  child: Text(
                    '鼠标：点按 / 拖动 / 滚轮　·　触摸：单指点按=左键、'
                    '长按=右键、双指拖动=滚轮　·　点「键盘」按钮唤起输入法（中文也可）',
                    textAlign: TextAlign.center,
                    style: Theme.of(context).textTheme.labelSmall?.copyWith(
                          color: cs.onSurfaceVariant,
                          height: 1.4,
                        ),
                  ),
                ),
              ),
            ),
          // 对端（受控端）主动断开：居中浮层提示，保留最后一帧作背景，半透黑遮罩铺满全屏。
          if (c.peerDisconnected)
            Positioned.fill(
              child: Container(
                color: Colors.black54,
                child: Center(
                  child: Container(
                    padding: const EdgeInsets.all(24),
                    decoration: BoxDecoration(
                      color: cs.surfaceContainerHighest,
                      borderRadius: BorderRadius.circular(16),
                      boxShadow: const [
                        BoxShadow(
                          color: Colors.black38,
                          blurRadius: 16,
                          offset: Offset(0, 4),
                        ),
                      ],
                    ),
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Icon(Icons.link_off_rounded,
                            size: 44, color: cs.onSurfaceVariant),
                        const SizedBox(height: 12),
                        Text('受控端已断开',
                            style: Theme.of(context).textTheme.titleMedium),
                        const SizedBox(height: 16),
                        FilledButton.icon(
                          icon: const Icon(Icons.arrow_back_rounded),
                          label: const Text('返回'),
                          onPressed: () {
                            c.revoke();
                            Navigator.of(context).pop();
                          },
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
        ],
      );
    }

    if (c.isActive && c.isHost) {
      final last = c.lastInput;
      return _centerCard(
        icon: Container(
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: cs.primaryContainer,
            shape: BoxShape.circle,
          ),
          child: Icon(Icons.desktop_mac_rounded, color: cs.onPrimaryContainer, size: 30),
        ),
        title: '正在控制对方设备',
        subtitle: '连接已激活，输入由本机经加密通道送达对方。',
        accent: cs.primary,
        detail: Container(
          width: double.infinity,
          padding: const EdgeInsets.all(14),
          decoration: BoxDecoration(
            color: cs.surfaceContainerHighest,
            borderRadius: BorderRadius.circular(14),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text('最近一次输入',
                  style: Theme.of(context).textTheme.labelSmall?.copyWith(
                        color: cs.onSurfaceVariant,
                      )),
              const SizedBox(height: 4),
              Text(
                last == null ? '尚未收到输入' : '$last',
                style: Theme.of(context).textTheme.bodyMedium,
                overflow: TextOverflow.ellipsis,
                maxLines: 3,
              ),
              const SizedBox(height: 8),
              Text('音频抓取由本机后台线程推送，对方侧播放',
                  style: Theme.of(context).textTheme.labelSmall?.copyWith(
                        color: cs.onSurfaceVariant,
                      )),
            ],
          ),
        ),
        actions: [
          FilledButton.icon(
            icon: const Icon(Icons.link_off_rounded),
            label: const Text('断开'),
            style: FilledButton.styleFrom(
              backgroundColor: cs.errorContainer,
              foregroundColor: cs.onErrorContainer,
            ),
            onPressed: () {
            c.revoke();
            Navigator.of(context).pop();
          },
          ),
        ],
      );
    }

    if (c.phase == ConnectionPhase.denied) {
      return _centerCard(
        icon: Container(
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: cs.errorContainer,
            shape: BoxShape.circle,
          ),
          child: Icon(Icons.block_rounded, color: cs.error, size: 30),
        ),
        title: '连接已被拒绝',
        subtitle: '主机方未批准本次连接请求。请确认授权范围后重新发起配对。',
        accent: cs.error,
        actions: [
          FilledButton.icon(
            icon: const Icon(Icons.arrow_back_rounded),
            label: const Text('返回'),
            onPressed: () => Navigator.of(context).pop(),
          ),
        ],
      );
    }

    if (c.isClosed) {
      final reason = c.indicator.closedReason;
      return _centerCard(
        icon: Container(
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: cs.surfaceContainerHighest,
            shape: BoxShape.circle,
          ),
          child: Icon(Icons.power_settings_new_rounded,
              color: cs.onSurfaceVariant, size: 30),
        ),
        title: '连接已关闭',
        subtitle: reason == null
            ? '本次远程会话已正常结束。'
            : '本次远程会话已结束（${reason.name}）。',
        accent: cs.outlineVariant,
        actions: [
          if (widget.manager != null && widget.sessionId != null)
            FilledButton.icon(
              icon: const Icon(Icons.refresh_rounded),
              label: const Text('重新连接'),
              onPressed: _reconnect,
            ),
          FilledButton.icon(
            icon: const Icon(Icons.arrow_back_rounded),
            label: const Text('返回'),
            onPressed: () => Navigator.of(context).pop(),
          ),
        ],
      );
    }

    if (c.isFailed) {
      return _centerCard(
        icon: Container(
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: cs.errorContainer,
            shape: BoxShape.circle,
          ),
          child: Icon(Icons.error_outline_rounded, color: cs.error, size: 30),
        ),
        title: '连接失败',
        subtitle: c.error ?? '连接过程中发生未知错误。',
        accent: cs.error,
        actions: [
          if (widget.manager != null && widget.sessionId != null)
            FilledButton.icon(
              icon: const Icon(Icons.refresh_rounded),
              label: const Text('重新连接'),
              onPressed: _reconnect,
            ),
          FilledButton.icon(
            icon: const Icon(Icons.arrow_back_rounded),
            label: const Text('返回'),
            onPressed: () => Navigator.of(context).pop(),
          ),
        ],
      );
    }

    if (c.phase == ConnectionPhase.connecting ||
        c.phase == ConnectionPhase.awaitingConsent) {
      final awaiting = c.phase == ConnectionPhase.awaitingConsent;
      final activeStep = awaiting ? 3 : 2;
      final cs2 = Theme.of(context).colorScheme;
      return _centerCard(
        icon: Container(
          padding: const EdgeInsets.all(16),
          decoration: BoxDecoration(
            color: cs2.primaryContainer,
            shape: BoxShape.circle,
          ),
          child: SizedBox(
            width: 30,
            height: 30,
            child: CircularProgressIndicator(
              strokeWidth: 3,
              color: cs2.onPrimaryContainer,
            ),
          ),
        ),
        title: awaiting ? '等待对端确认' : '正在建立安全连接',
        subtitle: awaiting
            ? '连接请求已送达对端，正在等待安全连接建立完成。'
            : '正在与主机协商端到端加密通道，请稍候…',
        accent: cs2.primary,
        detail: _progressSteps(activeStep: activeStep),
        actions: [
          if (awaiting)
            OutlinedButton.icon(
              icon: const Icon(Icons.help_outline_rounded),
              label: const Text('连接没反应？'),
              onPressed: () {
                ScaffoldMessenger.of(context).showSnackBar(
                  const SnackBar(
                    content: Text('请确认 Windows 主机已运行 rdcore-desktop，'
                        '且 iPhone 与主机网络可达（跨网需配置 TURN）。'),
                    behavior: SnackBarBehavior.floating,
                  ),
                );
              },
            ),
          TextButton.icon(
            icon: const Icon(Icons.close_rounded),
            label: const Text('取消'),
            onPressed: () => Navigator.of(context).pop(),
          ),
        ],
      );
    }

    // 默认（setup 等未知阶段）：保持安全背景，不露黑屏。
    return _stage(
      child: const Center(
        child: CircularProgressIndicator(),
      ),
    );
  }
}
