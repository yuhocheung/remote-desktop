import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:io' show Platform;
import 'dart:isolate';
import 'dart:typed_data';
import 'dart:ui' as ui;
import 'package:ffi/ffi.dart';

import '../ffi/rdcore_bindings.dart';
import '../ffi/rdcore_texture.dart';
import '../models/connection_state.dart';
import '../models/consent_scope.dart';
import '../models/media_frame.dart';
import '../models/remote_input.dart';
import '../models/security_indicator.dart';
import 'connection_backend.dart';
import 'local_identity.dart';

/// 基于 dart:ffi 的真实 WebRTC 连接后端（主线程代理）。
///
/// 关键修复（扫码后卡死）：Rust 端 `rdcore_connection_establish` 是**同步阻塞**调用，
/// Viewer 侧会一直阻塞等待 Host 的同意决定（见 rdcore-app `establish` 的
/// `loop { recv_app ... }`，**无超时**）。它内部用 `tokio::Runtime::block_on`，而
/// `Runtime` 是 `!Sync` 的——因此同一 `RdConnection` 的全部 FFI 调用必须留在**同一个
/// OS 线程**上。若在主（UI）线程直接 `block_on`，等同意期间整个 UI 被冻死；若把
/// establish 丢到别的线程、后续 FFI 回主线程，则跨线程用 `!Sync` Runtime 是未定义行为。
///
/// 故本类不再在主线程做任何 FFI：整条连接生命周期（建连 + `establish` + 拉帧 +
/// 发输入 + 拉音频 + `dispose`）都跑在**一个后台 isolate** 里，该 isolate 独占
/// 这条 `RdConnection`。主线程只通过该 isolate 的端口：
///   - 收到它主动推送的画面/音频字节（缓冲后供 [pullFrame]/[pullAudio] 同步取出）；
///   - 把用户输入事件发给它（[sendInputEvent]）；
/// 从而 UI 线程永不阻塞。
///
/// [selfSignaling] 为 true，[ConnectionController] 据此走自管理握手分支。
class NativeRtcConnection implements ConnectionBackend {
  NativeRtcConnection._(
    this._isHost,
    this._cmdPort,
    this._events,
    this._ready,
    this._isolate,
  );

  /// 本连接是否为 Host 角色（Host 不挂纹理，仅 Viewer 走真零拷贝）。
  final bool _isHost;

  /// 向后台 isolate 发送命令（connect / input / dispose 等）。后台 isolate 先把命令端口
  /// 发回来，本端在 [_listen] 内首次收到 [SendPort] 时赋值。
  SendPort? _cmdPort;

  /// 后台 isolate 主动推送的事件通道（ready / error / frame / audio / input）。
  final ReceivePort _events;

  /// 由后台 isolate 完成真实握手（含 Host 同意）后完成的 Future。
  final Completer<void> _ready;

  /// 持有后台 isolate 句柄，便于在卡死（如 Host 永不批准）时强制终止，避免泄漏。
  final Isolate _isolate;

  bool _established = false;
  bool _awaitingConsent = false;
  // Host 专用：当前对端已掉线（Rust 端常驻循环正等待下一个重扫的 Viewer 接入）。
  bool _peerGone = false;
  String? _cachedName;
  String? _cachedFp;
  SecurityIndicator? _cachedIndicator;

  // 缓冲：后台 isolate 主动推送，主线程同步取出（保持 ConnectionController 的 50ms 轮询模型）。
  RdMediaFrame? _lastFrame;
  String? _lastFrameError;
  RdAudioFrame? _lastAudio;
  RdInputEvent? _lastInput;
  // 最近一次实际收到远程画面帧（来自后台 isolate 推送的 'frame'）的时间戳。
  // 用于主线程断线看门狗——后台 isolate 的拉帧会阻塞在 Rust recv_rendered（对端静默掉线时
  // WebRTC 数据通道不即时关闭，recv 一直阻塞），故断线检测必须放到主线程。
  DateTime? _lastFrameAt;

  // ── 真零拷贝纹理（终极 Viewer 渲染）──
  /// 已创建的原生纹理句柄（null = 走旧 pull_frame 字节路径 / 无纹理插件）。
  RdCoreTextureHandle? _texture;
  /// Flutter `Texture` 控件使用的纹理 id（null = 未启用纹理）。
  int? _textureId;
  /// 纹理是否已真正出帧（收到首个 'tex-frame'）。RemoteScreen 据此切换到 Texture 控件。
  bool _textureActive = false;
  /// 连接是否已销毁（dispose 调用后置位）。用于拦截 `_resizeTexture` 在 `await create`
  /// 期间的竞态：若用户在等待原生纹理创建时断开，新建纹理无主，需立即释放避免泄漏。
  bool _disposed = false;
  /// 最近经纹理到达的帧尺寸（供坐标映射；字节路径下由 pullFrame 维护）。
  int? _lastFrameW;
  int? _lastFrameH;

  /// 对端（受控端 / Host）主动断开时，由后台 isolate 经 'peer-gone' 消息触发的回调。
  /// [ConnectionController] 据此在 UI 上显示「受控端已断开」浮层。
  void Function()? onPeerDisconnected;

  /// 真零拷贝纹理的 id / 激活态变化时通知（由 [ConnectionController] 接成
  /// `notifyListeners`，驱动 RemoteScreen 用新 textureId 重建 Texture 控件）。
  ///
  /// 关键：iOS 上 `FlutterTexture` 以 `copyPixelBuffer()` 模式会缓存某个 textureId 的
  /// IOSurface，resize 时**必须换新 textureId** 才能让 Flutter 绑定全新 IOSurface——
  /// 原地替换 CVPixelBuffer 会让纹理整片空白（已实测）。故 textureId 一变，必须显式通知
  /// UI 重建，否则 RemoteScreen 仍持有旧 id 的 Texture 控件 → 空白。
  void Function()? onTextureChanged;

  // Host 音频抓取配置（hostSetCaptureAudio 暂存，hostStartCaptureAudio 时一并经端口下发）。
  int _audioChannels = 2;
  int _audioSampleRate = 48000;
  int _audioSamplesPerFrame = 960;

  /// 开一个真实 WebRTC 连接（Rust 端 `RdConnection`）。
  ///
  /// [displayName] 用于在本机生成 `RdLocal` 身份——身份创建也发生在后台 isolate 内
  /// （FFI 句柄不可跨 isolate 传递，故不在主线程建好再传下去）。
  /// [baseUrl] 为信令基址（**不含** session/token），形如 `wss://host` 或
  /// `ws://127.0.0.1:8080`；本方法按 Host 侧约定拼为 `baseUrl/<sessionHex>?token=<token>`。
  /// [sessionHex] 32 字符小写 hex；[token] 配对 token；[scopesMask] 仅 Host 用。
  ///
  /// [iceServers] 可选，显式指定 ICE 服务器（STUN/TURN）；用于移动端无法用环境变量的场景。
  static Future<NativeRtcConnection> connect(
    String displayName, {
    required bool isHost,
    required String baseUrl,
    required String sessionHex,
    required String token,
    int scopesMask = 0,
    bool includeLoopback = false,
    bool forceRelay = false,
    int heartbeatMs = 30000,
    String? iceServers,
  }) async {
    final initPort = ReceivePort();
    final params = <String, dynamic>{
      'isHost': isHost,
      'baseUrl': baseUrl,
      'sessionHex': sessionHex,
      'token': token,
      'scopesMask': scopesMask,
      'includeLoopback': includeLoopback,
      'forceRelay': forceRelay,
      'heartbeatMs': heartbeatMs,
      'iceServers': iceServers,
      'displayName': displayName,
    };
    final isolate = await Isolate.spawn(
      _connectionWorker,
      <String, dynamic>{'toMain': initPort.sendPort, 'params': params},
    );
    // 单订阅流（ReceivePort）只能有一个 listener：这里只建一个 listen，命令端口由
    // [_listen] 在收到第一条 [SendPort] 消息时取出，并随即发出 'connect' 命令。
    // 切勿先用 `initPort.first` 再 `initPort.listen`，否则会抛
    // "bad state: stream has already been listened to"。
    final ready = Completer<void>();
    final conn = NativeRtcConnection._(isHost, null, initPort, ready, isolate);
    conn._listen();
    return conn;
  }

  void _listen() {
    _events.listen((msg) {
      // 第一条消息必为后台 isolate 暴露的命令端口（SendPort）。
      if (msg is SendPort) {
        _cmdPort = msg;
        // 触发后台 isolate 开始建连 + 握手（阻塞等同意发生在后台线程，不冻 UI）。
        _cmdPort!.send(<String, dynamic>{'cmd': 'connect'});
        return;
      }
      if (msg is! Map<String, dynamic>) return;
      switch (msg['type']) {
        case 'ready':
          final ind = msg['indicator'] as String?;
          if (ind != null) {
            try {
              final j = jsonDecode(ind) as Map<String, dynamic>;
              _cachedIndicator = SecurityIndicator.fromJson(j);
              // 对端展示名 / 指纹取自已认证的安全指示器（Rust 验签后填充），
              // 切勿用本地设备名覆盖。
              _cachedName = _cachedIndicator!.displayName;
              _cachedFp = _cachedIndicator!.fingerprintSpaced;
            } on Object {
              // 指示器非关键，获取失败不影响已建立的连接。
            }
          }
          _established = true;
          // Host 重扫重连后后台 isolate 会再发一次 ready：清除对端掉线标记，
          // 使 connectionState 恢复 active（首连时该标记本就为 false，幂等）。
          _peerGone = false;
          if (!_ready.isCompleted) _ready.complete();
          // Viewer 侧在握手完成后创建真零拷贝纹理（Host 不挂纹理）。
          if (!_isHost) unawaited(_initTexture());
          break;
        case 'tex-frame':
          // 真零拷贝：Rust 已把帧写入原生纹理缓冲，仅通知 Flutter 重新合成（无像素数据）。
          if (!_textureActive) {
            print('[rdcore:tex] 纹理路径已激活 textureId=$_textureId '
                'active=$_textureActive（收到首个 tex-frame）');
          }
          _textureActive = true;
          // 首帧激活：通知 UI 从字节路径 / 占位切到 Texture 控件（RemoteScreen 重建）。
          onTextureChanged?.call();
          _lastFrameAt = DateTime.now();
          _lastFrameW = msg['w'] as int?;
          _lastFrameH = msg['h'] as int?;
          if (_textureId != null) {
            RdCoreTexture().markFrameAvailable(_textureId!);
          }
          break;
        case 'tex-resize':
          // 帧尺寸与纹理缓冲不符：主线程重新分配缓冲并回发新 ptr。
          final w = msg['w'] as int?;
          final h = msg['h'] as int?;
          if (w != null && h != null) {
            print('[rdcore:tex] resize → ${w}x$h');
            unawaited(_resizeTexture(w, h));
          }
          break;
        case 'tex-fallback':
          // 纹理路径不可用（如 Android EGL 尚未实现）：销毁纹理，回退字节路径。
          print('[rdcore:tex] 回退字节路径: ${msg['error']}');
          if (_texture != null) {
            RdCoreTexture().dispose(_texture!.textureId);
            _texture = null;
            _textureId = null;
            _textureActive = false;
            // 通知 UI 切回字节路径（RemoteScreen 重建为 RemoteFrameView）。
            onTextureChanged?.call();
          }
          break;
        case 'peer-gone':
          // 对端（受控端 / Host）主动断开，依角色分流：
          // - Host 角色：标记 _peerGone，Rust 端常驻监听循环会自动接入下一个
          //   重扫同一配对码的 Viewer，届时会再收到 'ready'。
          // - Viewer 角色：后台 isolate 检测到 WebRTC 对端状态进入 Disconnected/Failed/Closed，
          //   或帧流停滞超时，通知上层触发「受控端已断开」浮层。
          if (msg['isHost'] as bool? ?? false) {
            _peerGone = true;
          } else {
            onPeerDisconnected?.call();
          }
          break;
        case 'error':
          final e = msg['error'] as String? ?? '握手失败';
          if (!_ready.isCompleted) _ready.completeError(e);
          break;
        case 'awaiting':
          // 后台 isolate 已创建连接，正阻塞于 establish（含等待 Host 同意）。
          _awaitingConsent = true;
          break;
        case 'frame':
          final w = msg['w'] as int;
          final h = msg['h'] as int;
          final bytes = msg['bytes'] as Uint8List;
          _lastFrame = RdMediaFrame(width: w, height: h, rgba: bytes);
          _lastFrameError = null;
          _lastFrameAt = DateTime.now();
          break;
        case 'frame-error':
          // Rust 侧拉帧失败（解码失败 / 媒体通道关闭等），记录供 UI 诊断。
          _lastFrameError = msg['error'] as String?;
          break;
        case 'audio':
          _lastAudio = RdAudioFrame(
            codec: msg['codec'] as int,
            channels: msg['channels'] as int,
            sampleRate: msg['sampleRate'] as int,
            data: msg['bytes'] as Uint8List,
          );
          break;
        case 'input':
          _lastInput = RdInputEvent.fromMap(msg['event'] as Map<String, dynamic>);
          break;
      }
    });
  }

  @override
  bool get selfSignaling => true;

  /// 后台 isolate 进入「阻塞等 Host 同意」后为 true，驱动 UI 显示「等待 Host 同意…」。
  @override
  bool get awaitingConsent => _awaitingConsent;

  /// 等待后台 isolate 完成真实握手（含 Host 同意）。超时 / 失败会抛异常，由
  /// [ConnectionController] 转为可见错误。
  @override
  Future<void> establish() => _ready.future;

  @override
  ConnectionState get connectionState {
    if (!_established) return const ConnectionState.awaiting();
    // Host 对端已掉线但会话仍存活（等待重扫接入）：向 UI 反映断开态。
    if (_peerGone) {
      return const ConnectionState.closed(ClosedReason.disconnected);
    }
    return const ConnectionState.active(null);
  }

  @override
  SecurityIndicator securityIndicator(bool encrypted) =>
      _cachedIndicator ??
      SecurityIndicator(
        displayName: _cachedName ?? '',
        deviceId: const [],
        fingerprint: const [],
        fingerprintSpaced: _cachedFp ?? '',
        state: const ConnectionState.awaiting(),
        encrypted: encrypted,
      );

  @override
  String? get peerDisplayName => _cachedName;

  @override
  String? get peerFingerprint => _cachedFp;

  @override
  void dispose() {
    _disposed = true;
    final p = _cmdPort;
    if (p != null) {
      try {
        p.send(<String, dynamic>{'cmd': 'dispose'});
      } on Object {
        // 后台 isolate 可能已自行退出（握手失败路径），忽略。
      }
    }
    // 解挂纹理（isolate 调 FFI detach），并释放原生缓冲。
    if (_texture != null) {
      p?.send(<String, dynamic>{'cmd': 'detachTexture'});
      try {
        RdCoreTexture().dispose(_texture!.textureId);
      } on Object {
        // 插件不可用或已释放，忽略。
      }
      _texture = null;
      _textureId = null;
      _textureActive = false;
      onTextureChanged?.call();
    }
    // 即便握手卡在等同意，也强制终止 isolate，避免 Dart 侧永久悬挂 + 泄漏。
    _isolate.kill(priority: Isolate.immediate);
    _events.close();
  }

  // ───────────────── 媒体 / 输入（真实 WebRTC 数据通道，全部经后台 isolate） ─────────────────

  @override
  RdMediaFrame? pullFrame() => _lastFrame;

  /// 最近一次收到远程画面帧的时间戳；用于主线程断线看门狗（null = 尚未收到任何帧）。
  DateTime? get lastFrameAt => _lastFrameAt;

  /// 是否正在用真零拷贝纹理渲染（RemoteScreen 据此选 `Texture` 控件 vs `CustomPaint`）。
  bool get textureActive => _textureActive;

  /// 当前纹理 id（null = 未启用纹理，走字节路径）。
  int? get textureId => _textureId;

  /// 最近帧尺寸（纹理路径或字节路径）；供 Viewer 把局部坐标映射到 Host 绝对坐标。
  ui.Size? get frameSize =>
      (_lastFrameW != null && _lastFrameH != null)
          ? ui.Size(_lastFrameW!.toDouble(), _lastFrameH!.toDouble())
          : null;

  /// Viewer 侧创建真零拷贝纹理并把可写缓冲地址发给后台 isolate 挂接。
  /// 失败（无原生插件 / 桌面 / headless）则保留 null，isolate 走旧 pull_frame 字节路径。
  Future<void> _initTexture() async {
    if (_texture != null) return;
    try {
      final handle = await RdCoreTexture().create(1280, 720);
      _texture = handle;
      _textureId = handle.textureId;
      // mode：0=CPU 缓冲（iOS CVPixelBuffer）；1=ANativeWindow（Android EGL 上传）。
      final mode = Platform.isAndroid ? 1 : 0;
      // submitFn：原生插件导出的 `rdcore_texture_submit` C 函数地址，随 attach 一并下发给
      // isolate，由其在 Rust 侧注册为全局提交函数（推送模型）。0 表示无原生插件/桌面。
      final submitFn = RdCoreTexture.submitFnAddress;
      _cmdPort?.send(<String, dynamic>{
        'cmd': 'attachTexture',
        'ptr': handle.ptr,
        'stride': handle.stride,
        'format': handle.format,
        'width': handle.width,
        'height': handle.height,
        'mode': mode,
        'submitFn': submitFn,
      });
    } on Object {
      // 回退字节路径：_texture 保持 null，isolate 默认走 pull_frame。
    }
  }

  /// 纹理缓冲重新分配为 `w × h`。
  ///
  /// **不再原地替换 CVPixelBuffer**：iOS 上 `FlutterTexture` 以 `copyPixelBuffer()` 模式
  /// 会缓存某 textureId 的 IOSurface，原地替换底层缓冲会让该纹理整片空白（已实测）。故这里
  /// 销毁旧纹理、用新尺寸**创建带新 textureId 的全新纹理**——Flutter 干净绑定全新 IOSurface。
  ///
  /// 旧纹理延迟 300ms 释放：确保主线程 `RemoteScreen` 已切到新 id（重建 Texture 控件）、
  /// 且后台 isolate 已 reattach 到新缓冲后再 unregister，避免 (a) 合成已释放的 textureId、
  /// (b) Rust 仍在写已释放内存（use-after-free 崩溃）。这 300ms 内旧缓冲仍映射在插件里，
  /// Rust 若因竞态短暂还写旧 addr 也只是写进"将被释放但暂未释放"的缓冲，安全。
  Future<void> _resizeTexture(int w, int h) async {
    final old = _texture;
    if (old == null) return;
    final oldId = old.textureId;
    try {
      final nh = await RdCoreTexture().create(w, h);
      // 竞态守卫：若连接在 await 期间已被 dispose（用户断开），新建纹理无主，
      // 立即释放并返回，避免泄漏（dispose() 不会再跑第二次）。
      if (_disposed) {
        try {
          RdCoreTexture().dispose(nh.textureId);
        } on Object {
          // 插件不可用或已释放，忽略。
        }
        return;
      }
      print('[rdcore:tex] resize create: 新纹理 id=${nh.textureId} ptr=${nh.ptr} '
          '(旧 id=$oldId) —— 若 ptr=0 表示 getTexturePtr 失败');
      _texture = nh;
      _textureId = nh.textureId;
      final mode = Platform.isAndroid ? 1 : 0;
      // 把新缓冲挂回 Rust（全局提交函数已注册一次，无需再传 submitFn）。
      _cmdPort?.send(<String, dynamic>{
        'cmd': 'attachTexture',
        'ptr': nh.ptr,
        'stride': nh.stride,
        'format': nh.format,
        'width': w,
        'height': h,
        'mode': mode,
      });
      // 通知 UI 用新 textureId 重建 Texture 控件（iOS 必须换新 id 才能正确绑定 IOSurface）。
      print('[rdcore:tex] onTextureChanged 调用（${onTextureChanged != null ? "已接线" : "未接线!"}）');
      onTextureChanged?.call();
      // 延迟释放旧纹理，避开与 UI 切换 / Rust reattach 的竞态窗口。
      // 用 async + await 包住，确保即便插件缺该方法也不会变成未捕获异步异常。
      Future.delayed(const Duration(milliseconds: 300), () async {
        try {
          await RdCoreTexture().dispose(oldId);
        } on Object {
          // 插件不可用 / 已释放，忽略。
        }
      });
    } on Object catch (e) {
      // 重分配失败（CVPixelBuffer 创建 / getTexturePtr 异常等）：绝不能静默保留旧缓冲——
      // isolate 侧 resize 占位永不解除，会把连接锁死在「每帧返回 2」的永久空白里
      //（已实测：鼠标可动、Viewer 永远转圈）。主动回退字节路径：通知 isolate detach
      //（改走 pull_frame）+ 本地清理 + 刷新 UI；isolate 侧另有 2s 占位超时双保险。
      print('[rdcore:tex] resize create 抛异常，回退字节路径: $e');
      _cmdPort?.send(<String, dynamic>{'cmd': 'detachTexture'});
      try {
        RdCoreTexture().dispose(oldId);
      } on Object {
        // 插件不可用 / 已释放，忽略。
      }
      _texture = null;
      _textureId = null;
      _textureActive = false;
      onTextureChanged?.call();
    }
  }

  /// Viewer 侧拉帧/解码相关的最近诊断（Rust `last_error` 透传）。
  @override
  String? get frameError => _lastFrameError;

  @override
  void sendInputEvent(RdInputEvent event) {
    // 同步投递到后台 isolate（SendPort.send 不阻塞），由它调 FFI 注入 Host。
    final p = _cmdPort;
    if (p != null) p.send(<String, dynamic>{'cmd': 'input', 'event': event.toMap()});
  }

  @override
  RdInputEvent? pollInput() {
    final e = _lastInput;
    _lastInput = null;
    return e;
  }

  /// 经端口通知后台 isolate 起视频抓取（Host 用）。
  void hostStartCapture(int fps) {
    final p = _cmdPort;
    if (p != null) p.send(<String, dynamic>{'cmd': 'hostStartCapture', 'fps': fps});
  }

  @override
  RdAudioFrame? pullAudio() => _lastAudio;

  @override
  void attachLoopbackAudio(ConnectionBackend viewer) =>
      throw UnsupportedError('真实 WebRTC 不走 headless 音频回环');

  @override
  void hostSetCaptureAudio({
    required int channels,
    required int sampleRate,
    required int samplesPerFrame,
    required int frames,
    required int byte,
  }) {
    // 真实路径无 headless 回环；暂存参数并经端口下发，真正抓取在 [hostStartCaptureAudio] 时触发。
    _audioChannels = channels;
    _audioSampleRate = sampleRate;
    _audioSamplesPerFrame = samplesPerFrame;
    final p = _cmdPort;
    if (p != null) {
      p.send(<String, dynamic>{
        'cmd': 'hostSetCaptureAudio',
        'channels': channels,
        'sampleRate': sampleRate,
        'samplesPerFrame': samplesPerFrame,
        'frames': frames,
        'byte': byte,
      });
    }
  }

  @override
  void hostStartCaptureAudio(int fps) {
    final p = _cmdPort;
    if (p != null) {
      p.send(<String, dynamic>{
        'cmd': 'hostStartCaptureAudio',
        'fps': fps,
        'channels': _audioChannels,
        'sampleRate': _audioSampleRate,
        'samplesPerFrame': _audioSamplesPerFrame,
      });
    }
  }

  /// 把本 Host 会话与给定 Viewer 会话接入同一对进程内媒体/输入通道（仅 headless 用）。
  void attachLoopbackMedia(dynamic viewer) =>
      throw UnsupportedError('真实 WebRTC 不走 headless 媒体回环');

  /// Host 设置抓取源（仅 headless 用）。
  void hostSetCapture({
    required int width,
    required int height,
    required int frames,
    required int color,
  }) =>
      throw UnsupportedError('真实 WebRTC 不走 headless 抓取');

  // ───────────────── 以下为手动握手 / headless seam 方法：真实 WebRTC 不走这些路径 ─────────────────

  @override
  Uint8List makeOffer() =>
      throw UnsupportedError('RdConnection 自管理信令，无需手动 makeOffer');

  @override
  void ingestOffer(Uint8List bytes) =>
      throw UnsupportedError('RdConnection 自管理信令，无需手动 ingestOffer');

  @override
  Uint8List makeAnswer() =>
      throw UnsupportedError('RdConnection 自管理信令，无需手动 makeAnswer');

  @override
  void ingestAnswer(Uint8List bytes) =>
      throw UnsupportedError('RdConnection 自管理信令，无需手动 ingestAnswer');

  @override
  Uint8List makeSessionKeyExchange() =>
      throw UnsupportedError('RdConnection 自管理信令，无需手动 makeSessionKeyExchange');

  @override
  void ingestSessionKeyExchange(Uint8List bytes) =>
      throw UnsupportedError('RdConnection 自管理信令，无需手动 ingestSessionKeyExchange');

  @override
  Uint8List encrypt(Uint8List plaintext) =>
      throw UnsupportedError('RdConnection 经加密数据通道收发，无需手动 encrypt');

  @override
  Uint8List decrypt(Uint8List ciphertext) =>
      throw UnsupportedError('RdConnection 经加密数据通道收发，无需手动 decrypt');

  @override
  ConnectionState hostRequestConsent({String? pin}) =>
      throw UnsupportedError('RdConnection 的同意在 establish 时由 Rust 端完成');

  @override
  ConnectionState hostDecide({
    required bool grant,
    required Set<ConsentScope> scopes,
    Duration? duration,
  }) =>
      throw UnsupportedError('RdConnection 的授权范围在连接创建时（scopesMask）已决定');

  @override
  ConnectionState tick() =>
      throw UnsupportedError('RdConnection 无手动 tick');

  @override
  void heartbeat() => throw UnsupportedError('RdConnection 无手动 heartbeat');

  @override
  ConnectionState revoke() =>
      throw UnsupportedError('RdConnection 的撤销经 Dart 侧 dispose 触发');

  @override
  ConnectionState onDisconnected() =>
      throw UnsupportedError('RdConnection 无手动 onDisconnected');
}

// ───────────────── 后台 isolate：独占这条 RdConnection 的全部 FFI ─────────────────

/// (channels, sampleRate, samplesPerFrame, frames, byte)
typedef _PendingAudio = (int, int, int, int, int);

/// Viewer 侧真零拷贝纹理的挂接状态（由主线程经端口下发，后台 isolate 读它决定渲染路径）。
class _TexState {
  /// 是否已把原生缓冲挂到 Rust 连接（开启真零拷贝）。
  bool attached = false;
  /// 当前挂接缓冲的尺寸（供 'tex-frame' 回传，供 Dart 维护帧尺寸）。
  int w = 0;
  int h = 0;
  /// 已发出 resize 请求、等待主线程真正完成「重分配缓冲 + 重挂接」的占位尺寸。
  ///
  /// 去抖关键：重挂接落地前，Rust 侧仍持旧尺寸、会持续对 `render_to_texture` 返回 `2`；
  /// 若不加此占位，媒体循环每 50ms 都会再发一次 `tex-resize` → resize 风暴 →
  /// Swift 反复创建/销毁 CVPixelBuffer → Flutter 的 IOSurface 绑定被撕裂 → 空白。
  /// 仅当「尺寸真的变了 且 尚未发出同尺寸请求」时才发一次 `tex-resize`。
  int pendingW = 0;
  int pendingH = 0;
  /// 发出 resize 请求的时间（配合 pendingW/H 占位）。
  ///
  /// 死锁兜底：占位后若主线程 `_resizeTexture` 失败（create/getTexturePtr 抛异常被吞、
  /// attach 消息丢失），占位永不解除、resize 永不重发 → Rust 每帧返回 `2` → 画面永久
  /// 空白（已实测：鼠标可动、Viewer 永远转圈）。媒体循环据此时间戳判定「resize 卡死」，
  /// 超时后放弃纹理路径、回退字节路径，把「永久空白」降级为「回退可用」。
  DateTime? pendingSince;
}

/// 后台 isolate 入口：独占一条 `RdConnection` 的全部 FFI 调用。
///
/// 所有阻塞调用（建连、[establish] 等同意、拉帧、轮询输入）都发生在此 isolate 的线程上，
/// 主（UI）线程只收发端口消息，永不冻结。
void _connectionWorker(Map<String, dynamic> init) {
  final toMain = init['toMain'] as SendPort;
  final params = init['params'] as Map<String, dynamic>;

  final lib = RdCoreLib();
  final receive = ReceivePort();
  // 第一条消息始终是命令端口（即便后续因原生库不可用直接报错，主线程也能正确建链，
  // 不会把 'error' 误当成 SendPort 触发转换异常）。
  toMain.send(receive.sendPort);

  Pointer<Void>? conn;
  LocalIdentity? local;
  Timer? frameTimer;
  Timer? audioTimer;
  Timer? inputTimer;
  Timer? peerTimer;
  _PendingAudio? pendingAudio;
  var established = false;
  // 真零拷贝纹理挂接状态（由主线程经端口下发）。
  final tex = _TexState();

  void cancelTimers() {
    frameTimer?.cancel();
    audioTimer?.cancel();
    inputTimer?.cancel();
    peerTimer?.cancel();
    frameTimer = audioTimer = inputTimer = peerTimer = null;
  }

  void freeConn() {
    if (conn != null) {
      lib.connectionFree(conn!);
      conn = null;
    }
    local?.dispose();
    local = null;
  }

  receive.listen((msg) {
    if (msg is! Map<String, dynamic>) return;
    final cmd = msg['cmd'];
    switch (cmd) {
      case 'connect':
        try {
          local = LocalIdentity.create(displayName: params['displayName'] as String);
          conn = _createConn(lib, local!, params);
          if (conn!.address == 0) {
            final e = lib.takeLastError() ?? '创建连接失败';
            toMain.send(<String, dynamic>{'type': 'error', 'error': e});
            freeConn();
            receive.close();
            return;
          }
          // 进入阻塞式 establish（含等待 Host 同意）：先告知主线程，使 UI 从
          // 「正在建立连接…」推进到「等待 Host 同意…」，避免用户误以为卡死。
          toMain.send(<String, dynamic>{'type': 'awaiting'});
          final err = lib.connectionEstablish(conn!);
          if (err.address != 0) {
            final s = err.toDartString();
            lib.stringFree(err);
            toMain.send(<String, dynamic>{'type': 'error', 'error': '握手失败: $s'});
            freeConn();
            receive.close();
            return;
          }
          established = true;
          // 缓存对端安全指示器（Ed25519 验签通过的对端身份，Viewer 不可伪造）。
          String? indicatorJson;
          try {
            final p = lib.connectionSecurityIndicator(conn!);
            if (p.address != 0) {
              indicatorJson = p.toDartString();
              lib.stringFree(p);
            }
          } on Object {
            // 指示器非关键，获取失败不影响已建立的连接。
          }
          toMain.send(<String, dynamic>{
            'type': 'ready',
            'indicator': indicatorJson,
          });
          _startMedia(lib, conn!, params, tex, toMain,
              (t) => frameTimer = t, (t) => audioTimer = t, (t) => inputTimer = t,
              (t) => peerTimer = t, cancelTimers);
        } on Object catch (e) {
          // 任何异常都必须转为可见错误上屏，绝不能静默抛出。
          toMain.send(<String, dynamic>{'type': 'error', 'error': e.toString()});
          freeConn();
          receive.close();
        }
        break;

      case 'input':
        if (established && conn != null) {
          try {
            final ev = RdInputEvent.fromMap(msg['event'] as Map<String, dynamic>);
            _sendInput(lib, conn!, ev);
          } on Object {
            // 输入注入失败忽略（不影响连接）。
          }
        }
        break;

      case 'hostStartCapture':
        if (conn != null) {
          final fps = (msg['fps'] as int?) ?? 30;
          final err = lib.connectionStartCapture(conn!, fps);
          if (err.address != 0) lib.stringFree(err);
        }
        break;

      case 'hostSetCaptureAudio':
        pendingAudio = (
          (msg['channels'] as int?) ?? 2,
          (msg['sampleRate'] as int?) ?? 48000,
          (msg['samplesPerFrame'] as int?) ?? 960,
          (msg['frames'] as int?) ?? 0,
          (msg['byte'] as int?) ?? 0,
        );
        break;

      case 'hostStartCaptureAudio':
        if (conn != null) {
          final a = pendingAudio ?? (2, 48000, 960, 0, 0);
          final fps = (msg['fps'] as int?) ?? 30;
          final err = lib.connectionStartCaptureAudio(
            conn!,
            a.$1,
            a.$2,
            a.$3,
            fps,
          );
          if (err.address != 0) lib.stringFree(err);
        }
        break;

      case 'attachTexture':
        // Viewer 侧：主线程创建原生纹理后下发的可写缓冲地址，挂到 Rust 连接开启真零拷贝。
        if (conn != null) {
          final ptr = (msg['ptr'] as int?) ?? 0;
          final w = (msg['width'] as int?) ?? 0;
          final h = (msg['height'] as int?) ?? 0;
          final stride = (msg['stride'] as int?) ?? (w * 4);
          final format = (msg['format'] as int?) ?? 0;
          final mode = (msg['mode'] as int?) ?? 0;
          // 推送模型：先把原生插件的提交函数注册进 Rust（全局一次）。无地址则回退字节路径。
          final submitFn = (msg['submitFn'] as int?) ?? 0;
          if (submitFn != 0) {
            final sfn = lib.textureSetSubmitFn;
            if (sfn != null) {
              sfn(Pointer<Void>.fromAddress(submitFn));
            }
          }
          if (ptr != 0 && w > 0 && h > 0) {
            final p = Pointer<Void>.fromAddress(ptr);
            final fn = lib.connectionAttachTexture;
            if (fn == null) {
              // 原生库不含零拷贝符号：回退字节路径，销毁纹理。
              tex.attached = false;
              print('[rdcore:tex] attach: 原生库缺 connectionAttachTexture，回退');
              toMain.send(<String, dynamic>{
                'type': 'tex-fallback',
                'error': '原生库不支持真零拷贝纹理（缺 connectionAttachTexture）',
              });
            } else {
              final err = fn(conn!, p, w, h, stride, mode, format);
              if (err.address != 0) {
                lib.stringFree(err);
                // 挂接失败：保持字节路径（tex.attached 仍 false）。
                // 清空占位尺寸，使下一帧可重试重挂接。
                tex.pendingW = 0;
                tex.pendingH = 0;
                tex.pendingSince = null;
                print('[rdcore:tex] attach 失败（保持字节路径）');
              } else {
                tex.attached = true;
                tex.w = w;
                tex.h = h;
                // 重挂接落地：清空占位，允许后续真实尺寸变化再次触发 resize。
                tex.pendingW = 0;
                tex.pendingH = 0;
                tex.pendingSince = null;
                print('[rdcore:tex] attach 成功 w=$w h=$h');
              }
            }
          } else {
            print('[rdcore:tex] attach 跳过：ptr=$ptr w=$w h=$h 非法');
          }
        }
        break;

      case 'detachTexture':
        // 解挂纹理（回退字节路径），通常在 dispose 时由主线程下发。
        if (conn != null) {
          final fn = lib.connectionDetachTexture;
          if (fn != null) {
            final err = fn(conn!);
            if (err.address != 0) lib.stringFree(err);
          }
        }
        tex.attached = false;
        tex.pendingW = 0;
        tex.pendingH = 0;
        break;

      case 'dispose':
        cancelTimers();
        freeConn();
        receive.close();
        break;
    }
  });
}

/// 按参数在后台 isolate 内创建 `RdConnection`（调用 FFI `connectionNewViewer/Host`）。
Pointer<Void> _createConn(
  RdCoreLib lib,
  LocalIdentity local,
  Map<String, dynamic> params,
) {
  final baseUrl = params['baseUrl'] as String;
  final sessionHex = params['sessionHex'] as String;
  final token = params['token'] as String;
  final isHost = params['isHost'] as bool;
  final includeLoopback = params['includeLoopback'] as bool;
  final forceRelay = params['forceRelay'] as bool;
  final heartbeatMs = params['heartbeatMs'] as int;
  final scopesMask = params['scopesMask'] as int;
  final iceServers = params['iceServers'] as String?;

  final basePtr = baseUrl.toNativeUtf8();
  final shexPtr = sessionHex.toNativeUtf8();
  final tokPtr = token.toNativeUtf8();
  final icePtr = iceServers != null ? iceServers.toNativeUtf8() : nullptr;
  final c = isHost
      ? lib.connectionNewHost(basePtr, shexPtr, tokPtr, local.handle,
          includeLoopback ? 1 : 0, forceRelay ? 1 : 0, heartbeatMs, scopesMask, icePtr)
      : lib.connectionNewViewer(basePtr, shexPtr, tokPtr, local.handle,
          includeLoopback ? 1 : 0, forceRelay ? 1 : 0, heartbeatMs, icePtr);
  malloc.free(basePtr);
  malloc.free(shexPtr);
  malloc.free(tokPtr);
  if (icePtr.address != 0) malloc.free(icePtr);
  return c;
}

/// 后台 isolate 内启动媒体循环：Viewer 拉帧/拉音频，Host 轮询输入，推送回主线程。
///
/// [setPeer] 用于挂接「对端状态轮询」定时器（仅 Viewer）：周期性读取 WebRTC 对端连接状态，
/// 一旦进入 Disconnected/Failed/Closed 即向主线程上报 'peer-gone'（对端主动断开）。
void _startMedia(
  RdCoreLib lib,
  Pointer<Void> conn,
  Map<String, dynamic> params,
  _TexState tex,
  SendPort toMain,
  void Function(Timer?) setFrame,
  void Function(Timer?) setAudio,
  void Function(Timer?) setInput,
  void Function(Timer?) setPeer,
  void Function() cancel,
) {
  final isHost = params['isHost'] as bool;
  if (isHost) {
    // Host 对端状态跟踪：Rust 端有常驻监听循环（对端掉线 / 重扫自动重连），这里把
    // 状态变化同步给主线程刷新 UI。检查节流为 1s：输入轮询的 FFI 调用在对端空闲时会
    // 阻塞在通道读取上，对端掉线时通道关闭、读取立即返回，本检查随即在下一轮轮询执行。
    var peerGoneReported = false;
    var lastStateCheck = DateTime.fromMillisecondsSinceEpoch(0);
    setInput(Timer.periodic(const Duration(milliseconds: 50), (_) {
      final ev = _pollInput(lib, conn);
      if (ev != null) {
        toMain.send(<String, dynamic>{'type': 'input', 'event': ev.toMap()});
      }
      final now = DateTime.now();
      if (now.difference(lastStateCheck).inMilliseconds < 1000) return;
      lastStateCheck = now;
      final p = lib.connState(conn);
      if (p.address == 0) return;
      final s = p.toDartString();
      lib.stringFree(p);
      final gone = s.contains('"Disconnected"') ||
          s.contains('"Failed"') ||
          s.contains('"Closed"');
      if (gone && !peerGoneReported) {
        peerGoneReported = true;
        toMain.send(<String, dynamic>{'type': 'peer-gone', 'isHost': isHost});
      } else if (!gone && peerGoneReported && s.contains('"Connected"')) {
        // 新 Viewer 已重扫接入：刷新安全指示器并再发一次 ready，让主线程恢复 UI。
        peerGoneReported = false;
        String? indicatorJson;
        try {
          final ip = lib.connectionSecurityIndicator(conn);
          if (ip.address != 0) {
            indicatorJson = ip.toDartString();
            lib.stringFree(ip);
          }
        } on Object {
          // 指示器非关键，获取失败不影响已重建的连接。
        }
        toMain.send(<String, dynamic>{
          'type': 'ready',
          'indicator': indicatorJson,
        });
      }
    }));
  } else {
    // 断线看门狗（在后台 isolate 内判断，不受 UI 线程后台节流影响）：
    // 已收到过至少一帧、且连续 ~4s（120 次 ×33ms）拉不到任何新帧 → 判定受控端已断开。
    // 比轮询 WebRTC peer_state 更可靠——对端静默掉线 / 被杀 / 网络中断时，peer_state
    // 往往迟迟不跳变（依赖 ICE 保活超时），而「帧流停止」是更直接的信号。
    var hadFrame = false;
    var nullStreak = 0;
    // 拉帧节奏 33ms（≈30fps 消费上限）：配合 Host `--fps 30` 达成端到端 30fps；
    // 原 50ms（20fps 上限）曾是帧率瓶颈之一。Rust 侧追帧丢旧会兜住消费不及的积压。
    const kNullStreakLimit = 120;
    setFrame(Timer.periodic(const Duration(milliseconds: 33), (_) {
      // ── 真零拷贝纹理路径：Rust 把解码帧直接写入原生缓冲，仅回传轻量信号（无像素数据）──
    if (tex.attached && conn.address != 0) {
      final renderFn = lib.connectionRenderToTexture;
      if (renderFn == null) {
        // 原生库不含零拷贝符号：销毁纹理并回退字节路径。
        tex.attached = false;
        toMain.send(<String, dynamic>{
          'type': 'tex-fallback',
          'error': '原生库不支持真零拷贝纹理（缺 connectionRenderToTexture）',
        });
      } else {
        final r = renderFn(conn);
        if (r == 1) {
          // 已写入当前纹理：通知主线程重新合成（Flutter 重新读原生缓冲）。
          toMain.send(<String, dynamic>{
            'type': 'tex-frame',
            'w': tex.w,
            'h': tex.h,
          });
        } else if (r == 2) {
          // 帧尺寸与纹理缓冲不符：取目标尺寸，通知主线程 resize 并重挂。
          // 去抖：重挂接落地前 Rust 仍持旧尺寸、会持续返回 2；用 pendingW/H 占位，
          // 仅对「尺寸真的变了 且 尚未发出同尺寸请求」的情况发一次 tex-resize，
          // 杜绝每 50ms 触发一次的 resize 风暴（旧缓冲反复销毁→IOSurface 绑定撕裂→空白）。
          //
          // 死锁兜底：占位已发出却迟迟未落地（主线程 resize 失败被吞 / attach 丢失）时，
          // 占位永不解除 → 每帧都返回 2 → 画面永久空白。超时（2s）未落地即放弃纹理路径，
          // detach 并回退字节路径——宁可走旧路径出画面，也不能卡死在空白。
          final pend = tex.pendingSince;
          if (pend != null &&
              DateTime.now().difference(pend).inMilliseconds > 2000) {
            final fn = lib.connectionDetachTexture;
            if (fn != null) {
              final derr = fn(conn);
              if (derr.address != 0) lib.stringFree(derr);
            }
            tex.attached = false;
            tex.pendingW = 0;
            tex.pendingH = 0;
            tex.pendingSince = null;
            toMain.send(<String, dynamic>{
              'type': 'tex-fallback',
              'error': '纹理 resize 超时未落地（主线程重挂接失败），回退字节路径',
            });
            return;
          }
          final wp = calloc<Uint32>();
          final hp = calloc<Uint32>();
          final lfs = lib.connectionLastFrameSize;
          if (lfs != null) {
            final ok = lfs(conn, wp, hp);
            if (ok == 1) {
              final w = wp.value;
              final h = hp.value;
              if ((w != tex.w || h != tex.h) &&
                  (tex.pendingW != w || tex.pendingH != h)) {
                tex.pendingW = w;
                tex.pendingH = h;
                tex.pendingSince = DateTime.now();
                toMain.send(<String, dynamic>{
                  'type': 'tex-resize',
                  'w': w,
                  'h': h,
                });
              }
            }
          }
          calloc.free(wp);
          calloc.free(hp);
        } else if (r == -1) {
          // 出错（如 Android ANativeWindow EGL 尚未实现）：回退字节路径，
          // 并通知主线程销毁纹理（RemoteScreen 切回 CustomPaint）。
          tex.attached = false;
          tex.pendingW = 0;
          tex.pendingH = 0;
          final err = lib.takeLastError();
          toMain.send(<String, dynamic>{
            'type': 'tex-fallback',
            'error': err ?? '纹理渲染失败',
          });
        } else if (r == 3) {
          // 纹理已挂接但原生提交函数缺失：若不回退，媒体循环会把后续所有 0
          // 都当成"无新帧"而永远卡死。立即切回字节路径。
          tex.attached = false;
          tex.pendingW = 0;
          tex.pendingH = 0;
          final err = lib.takeLastError();
          toMain.send(<String, dynamic>{
            'type': 'tex-fallback',
            'error': err ?? '纹理已挂接但缺失原生提交函数，回退字节路径',
          });
        }
        // r == 0（无新帧）：保持上一帧，不触发重绘，等待下一轮。
        return;
      }
    }
      // ── 旧字节路径（未挂纹理 / 回退）：pull_frame 后回传 RGBA 字节 ──
      final f = _pullFrame(lib, conn);
      if (f != null) {
        hadFrame = true;
        nullStreak = 0;
        toMain.send(<String, dynamic>{
          'type': 'frame',
          'w': f.width,
          'h': f.height,
          'bytes': f.rgba,
        });
      } else {
        nullStreak++;
        if (hadFrame && nullStreak >= kNullStreakLimit) {
          // 受控端已断开：停掉全部媒体定时器并上报主线程。
          cancel();
          toMain.send(<String, dynamic>{'type': 'peer-gone', 'isHost': isHost});
          return;
        }
        // 拉帧返回 null：可能是 Rust 侧解码失败 / 媒体通道关闭而带错误返回（此时会走到这里）；
        // 也可能是「通道开着但 Host 迟迟不推帧」而阻塞在此回调之外（由 controller 看门狗兜底）。
        // 把 Rust last_error 透传给主线程，便于无画面时给出可操作的诊断。
        final err = lib.takeLastError();
        if (err != null && err.isNotEmpty) {
          toMain.send(<String, dynamic>{'type': 'frame-error', 'error': err});
        }
      }
    }));
    setAudio(Timer.periodic(const Duration(milliseconds: 50), (_) {
      final a = _pullAudio(lib, conn);
      if (a != null) {
        toMain.send(<String, dynamic>{
          'type': 'audio',
          'codec': a.codec,
          'channels': a.channels,
          'sampleRate': a.sampleRate,
          'bytes': a.data,
        });
      }
    }));
    // 对端（受控端 / Host）断开检测（快路径）：WebRTC 不会主动「推」断线事件，只能轮询。
    // 每秒读一次 peer_connection_state；一旦状态进入 Disconnected/Failed/Closed 即上报一次后停表。
    // 用子串匹配而非 JSON 解析，避免解析异常吞掉事件。
    var peerGoneSent = false;
    setPeer(Timer.periodic(const Duration(milliseconds: 1000), (_) {
      if (peerGoneSent || conn.address == 0) return;
      try {
        final p = lib.connState(conn);
        if (p.address == 0) return;
        final s = p.toDartString();
        lib.stringFree(p);
        final gone = s.contains('Disconnected') ||
            s.contains('Failed') ||
            s.contains('Closed');
        if (gone) {
          peerGoneSent = true;
          cancel();
          toMain.send(<String, dynamic>{'type': 'peer-gone', 'isHost': isHost});
        }
      } on Object {
        // 轮询失败（句柄临时不可用等）忽略，下一秒再试。
      }
    }));
  }
}

RdMediaFrame? _pullFrame(RdCoreLib lib, Pointer<Void> conn) {
  final p = lib.connectionPullFrame(conn);
  if (p.address == 0) return null;
  final w = p.ref.width;
  final h = p.ref.height;
  final copy = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
  lib.mediaFrameFree(p);
  return RdMediaFrame(width: w, height: h, rgba: copy);
}

RdAudioFrame? _pullAudio(RdCoreLib lib, Pointer<Void> conn) {
  final p = lib.connectionPullAudio(conn);
  if (p.address == 0) return null;
  final codec = p.ref.codec;
  final channels = p.ref.channels;
  final sampleRate = p.ref.sampleRate;
  final copy = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
  lib.audioFrameFree(p);
  return RdAudioFrame(
    codec: codec,
    channels: channels,
    sampleRate: sampleRate,
    data: copy,
  );
}

RdInputEvent? _pollInput(RdCoreLib lib, Pointer<Void> conn) {
  final p = lib.connectionRecvInput(conn);
  if (p.address == 0) return null;
  final ev = RdInputEvent.fromNative(p.ref);
  lib.inputEventFree(p);
  return ev;
}

void _sendInput(RdCoreLib lib, Pointer<Void> conn, RdInputEvent ev) {
  // keyWithChar 的 character 字符串无法塞进 NativeRdInputEvent struct（C struct 不承载
  // 字符串），必须走独立的「带字符键盘」FFI 路径：真实 WebRTC 路径用 connectionSendInputKey
  // （作用于 RdConnection）；headless 回环旧 backend（native_connection）用 viewerSendInputKey
  // （作用于 RdSession）。否则 character 丢失，Host 只收到无字符事件，inject 退化为
  // enigo.key(Other(0))，什么都不会输入。
  if (ev.kind == RdInputKind.keyWithChar) {
    final hasChar = ev.character != null && ev.character!.isNotEmpty;
    final charPtr = hasChar
        ? ev.character!.toNativeUtf8().cast<Uint8>()
        : Pointer<Uint8>.fromAddress(0);
    try {
      // 真实 WebRTC 路径：Viewer 句柄是 RdConnection，必须用 connectionSendInputKey
      // （作用于 RdConnection）。早期误用 viewerSendInputKey（作用于 headless 回环的
      // RdSession）会导致指针类型错配、输入被静默丢弃——键盘字符永不达 Host。
      final err = lib.connectionSendInputKey(
          conn, ev.keyCode, charPtr, ev.pressed ? 1 : 0, ev.modifiers);
      if (err.address != 0) lib.stringFree(err);
    } finally {
      if (hasChar) calloc.free(charPtr);
    }
    return;
  }
  final p = calloc<NativeRdInputEvent>();
  ev.writeInto(p.ref);
  try {
    final err = lib.connectionSendInput(conn, p);
    if (err.address != 0) lib.stringFree(err);
  } finally {
    lib.inputEventFree(p);
  }
}
