import 'dart:async';
import 'dart:ui' as ui;
import 'package:flutter/foundation.dart';
import '../models/connection_state.dart';
import '../models/consent_scope.dart';
import '../models/media_frame.dart';
import '../models/remote_input.dart';
import 'connection_backend.dart';
import 'native_rtc_connection.dart';
import 'signaling.dart';

/// 连接阶段（UI 驱动）。
enum ConnectionPhase {
  setup,
  connecting,
  awaitingConsent,
  active,
  denied,
  closed,
  failed,
}

/// 编排一次连接的 Dart 侧控制器：驱动 [ConnectionBackend] 完成
/// Offer/Answer 握手、端到端密钥交换、同意门控，并通过 [SignalingTransport]
/// 与对端收发信令字节。状态变化通过 [ChangeNotifier] 通知 UI。
class ConnectionController extends ChangeNotifier {
  ConnectionController({
    required ConnectionBackend backend,
    required SignalingTransport transport,
    required bool isHost,
  })  : _backend = backend,
        _transport = transport,
        _isHost = isHost {
    _transport.incoming.listen(_onIncoming);
    // 真实 WebRTC 后端：挂接「对端主动断开」回调，用于 UI 浮层提示（受控端已断开）。
    if (backend is NativeRtcConnection) {
      backend.onPeerDisconnected = _onPeerGone;
    }
  }

  final ConnectionBackend _backend;
  final SignalingTransport _transport;
  final bool _isHost;

  ConnectionPhase _phase = ConnectionPhase.setup;
  bool _encrypted = false;
  String? _error;
  String? _peerName;
  String? _peerFingerprint;
  Set<ConsentScope> _grantedScopes = {};
  ClosedReason? _closedReason;
  bool _disposed = false;
  // 已生成的本端会话密钥字节（Host 在收到 Offer 时即 prepare，但门控到 approve 才发送）。
  Uint8List? _pendingSessionKey;

  // ───────────────── 媒体 / 输入（Track A） ─────────────────
  Timer? _frameTimer;
  Timer? _inputTimer;
  // 自管理信令 Host 的对端巡检：对端掉线 → UI 回到等待态；重扫接入 → 恢复激活。
  Timer? _peerWatch;
  // 主线程断线看门狗：周期性检查「最近一次收到画面帧」的时间戳，超时即判定对端已断开。
  // 置于主线程（而非后台 isolate）是关键——isolate 的拉帧调用会阻塞在 Rust recv_rendered，
  // 对端静默掉线时 WebRTC 数据通道不即时关闭，isolate 内的看门狗无法运行。
  Timer? _peerWatchdog;
  RdMediaFrame? _lastFrame;
  RdInputEvent? _lastInput;
  // 最近帧尺寸（字节路径下由 pullFrame 维护；纹理路径下由 NativeRtcConnection 维护），
  // 供 Viewer 把局部坐标映射到 Host 绝对坐标。
  ui.Size? _frameSize;
  // Viewer 侧拉帧/解码诊断：Rust last_error 透传，或看门狗给出的「无首帧」提示。
  String? _frameError;
  // 激活后超时（默认 10s）仍未收到任何画面帧。
  bool _frameStalled = false;
  // 对端（受控端 / Host）主动断开。置位后保持 active 相位，使最后一帧继续显示，
  // 仅叠加「受控端已断开」浮层，不直接进入 closed（closed 卡片会替换整屏画面）。
  bool _peerDisconnected = false;

  /// Viewer 侧最近一次拉到的远程画面（null = 尚未收到帧）。
  RdMediaFrame? get lastFrame => _lastFrame;

  /// 最近帧尺寸（纹理路径或字节路径）；供 Viewer 把局部坐标映射到 Host 绝对坐标。
  ui.Size? get frameSize {
    if (_frameSize != null) return _frameSize;
    final b = _backend;
    if (b is NativeRtcConnection) return b.frameSize;
    return null;
  }

  /// 当前真零拷贝纹理 id（null = 未启用，走字节路径）。
  int? get textureId {
    final b = _backend;
    return b is NativeRtcConnection ? b.textureId : null;
  }

  /// 是否正在用真零拷贝纹理渲染（RemoteScreen 据此选 `Texture` 控件）。
  /// 仅当纹理已真正出帧（收到首个 'tex-frame'）才为 true；Android 在 Rust EGL 上传
  /// 未就绪时会回退字节路径，此时为 false。
  bool get usingTexture {
    final b = _backend;
    return b is NativeRtcConnection ? b.textureActive : false;
  }

  /// Viewer 侧拉帧/解码相关诊断信息；无则 null。
  String? get frameError => _frameError;

  /// 连接已激活但长时间未收到任何画面帧（Host 未推流 / Viewer 解码失败）。
  bool get frameStalled => _frameStalled;

  /// 对端（受控端）已主动断开连接（由 WebRTC peer 状态轮询检测到）。
  bool get peerDisconnected => _peerDisconnected;

  /// Host 侧最近一次轮询到的 Viewer 输入（null = 暂无）。
  RdInputEvent? get lastInput => _lastInput;

  // ───────────────── 音频（Track A） ─────────────────
  Timer? _audioTimer;
  RdAudioFrame? _lastAudio;
  bool _muted = false;
  double _volume = 1.0;
  double _audioLevel = 0.0;

  /// Viewer 侧最近一次拉到的远程音频帧（null = 尚未收到）。
  RdAudioFrame? get lastAudio => _lastAudio;

  /// 是否静音（Viewer 侧本地开关，不影响远端音频流的到达）。
  bool get muted => _muted;

  /// 播放音量（0..1，Viewer 侧本地增益，UI 状态）。
  double get volume => _volume;

  /// 当前音频电平（0..1，RMS），静音时归零。
  double get audioLevel => _audioLevel;

  /// 切换静音。
  void setMute(bool muted) {
    _muted = muted;
    if (_muted) _audioLevel = 0.0;
    notifyListeners();
  }

  /// 设置播放音量（0..1）。
  void setVolume(double v) {
    _volume = v.clamp(0.0, 1.0);
    notifyListeners();
  }

  ConnectionPhase get phase => _phase;
  bool get isHost => _isHost;
  bool get encrypted => _encrypted;
  String? get error => _error;
  String? get peerName => _peerName;
  String? get peerFingerprint => _peerFingerprint;
  Set<ConsentScope> get grantedScopes => _grantedScopes;
  bool get isActive => _phase == ConnectionPhase.active;
  bool get isClosed => _phase == ConnectionPhase.closed;
  bool get isFailed => _phase == ConnectionPhase.failed;

  /// 当前不可伪造安全指示器（数据来自已认证对端，Viewer 无法伪造）。
  SecurityIndicatorSnapshot get indicator => SecurityIndicatorSnapshot(
        peerName: _peerName,
        peerFingerprint: _peerFingerprint,
        encrypted: _encrypted,
        phase: _phase,
        grantedScopes: _grantedScopes,
        closedReason: _closedReason,
      );

  /// Viewer 发起连接：生成 Offer 并发信令。
  void startAsViewer() {
    _setPhase(ConnectionPhase.connecting);
    try {
      _transport.send(_backend.makeOffer());
    } on RdCoreException catch (e) {
      _fail(e.message);
    }
  }

  /// 统一启动入口：自管理信令后端（真实 WebRTC）直接跑握手；否则走手动 Offer/Answer。
  void start() {
    if (_backend.selfSignaling) {
      // establish 在后台 isolate 异步跑（可能等 Host 同意），fire-and-forget：
      // 状态通过 notifyListeners 驱动 UI（RemoteScreen 显示「正在建立／等待同意」）。
      unawaited(_establishSelfSignaling());
    } else if (!_isHost) {
      startAsViewer();
    }
    // 非自管理 Host：等待对端 Offer（由 _onIncoming 驱动）。
  }

  /// 自管理信令后端：Rust 端内部经自带信令完成整条握手，Dart 侧只需触发并接管媒体循环。
  ///
  /// 异步：真实后端的 [ConnectionBackend.establish] 会 `await` 后台 isolate 完成握手
  /// （含 Host 同意），期间 UI 线程不被阻塞，故可安全 `await`。
  Future<void> _establishSelfSignaling() async {
    _setPhase(ConnectionPhase.connecting);
    // 后台 isolate 进入「阻塞等 Host 同意」时把相位从 connecting 推进到 awaitingConsent，
    // 让 UI 显示「等待 Host 同意…」而非一直「正在建立连接…」。
    final watch = Timer.periodic(const Duration(milliseconds: 250), (_) {
      if (!_disposed &&
          _phase == ConnectionPhase.connecting &&
          _backend.awaitingConsent) {
        _setPhase(ConnectionPhase.awaitingConsent);
      }
    });
    try {
      await _backend.establish().timeout(
        const Duration(seconds: 120),
        onTimeout: () => throw '连接超时（120s）：未收到对端确认，或信令 / ICE 未能建立。'
            '请确认：① 被控端程序已运行并保持在线；'
            '② 双方网络可达（跨网 / 对称 NAT 需配置 TURN）；'
            '③ 信令服务器地址正确（见 App 设置页）。',
      );
      watch.cancel();
      _refreshPeer();
      _encrypted = true;
      _phase = ConnectionPhase.active;
      _startMediaLoops();
      // Host 常驻会话：Rust 端会在对端掉线 / 重扫时自动重连（会话不死亡），
      // 这里周期巡检后端状态，把「对端离开 → 等待重连 → 新对端接入」反映到 UI。
      if (_isHost) {
        _peerWatch?.cancel();
        _peerWatch =
            Timer.periodic(const Duration(seconds: 1), (_) {
          if (_disposed) return;
          final st = _backend.connectionState;
          if (st.isClosed && _phase == ConnectionPhase.active) {
            _phase = ConnectionPhase.connecting;
            _stopMediaLoops();
            notifyListeners();
          } else if (st.isActive && _phase == ConnectionPhase.connecting) {
            _refreshPeer();
            _phase = ConnectionPhase.active;
            _startMediaLoops();
            notifyListeners();
          }
        });
      }
      notifyListeners();
    } on Object catch (e) {
      // 任何异常（RdCoreException 或非 RdCoreException，如 FFI 原生不可用 / 握手失败 /
      // 超时）都必须转为可见错误，绝不能静默抛出——否则会逃出 connectViaPairing 所在的
      // 未 await 异步闭包，仅被 runZonedGuarded 打印、不上屏，表现为「无画面无提示」。
      watch.cancel();
      _stopMediaLoops();
      _fail('连接失败：$e');
    }
  }

  void _onIncoming(Uint8List bytes) {
    if (_disposed) return;
    final type = messageTypeFromBytes(bytes);
    try {
      switch (type) {
        case MessageType.offer:
          if (!_isHost) return; // Viewer 不应收到 Offer
          _backend.ingestOffer(bytes);
          _refreshPeer();
          _setPhase(ConnectionPhase.awaitingConsent);
          // 启动 Rust 端同意门控（rdcore_host_decide 的前置条件：必须先 request_consent）。
          _backend.hostRequestConsent();
          _transport.send(_backend.makeAnswer());
          // 生成 Host 端 ephemeral 密钥并缓存（Rust ECDH 要求 ingest 对端密钥前本端已生成），
          // 但暂不发送——Host 的会话密钥只在 [approve] 时才发给 Viewer，
          // 这样 Viewer 端的“激活”严格发生在 Host 同意之后（安全门控）。
          _prepareSessionKey();
          break;
        case MessageType.answer:
          if (_isHost) return;
          _backend.ingestAnswer(bytes);
          _refreshPeer();
          _prepareSessionKey();
          _sendSessionKey();
          break;
        case MessageType.sessionKey:
          _backend.ingestSessionKeyExchange(bytes);
          _encrypted = true;
          // Viewer 收到 Host 会话密钥 = E2E 建立完成 → 激活并启动媒体循环
          // （拉帧 + 拉音频）。Host 的激活经 [approve] → [_applyState] 启动
          // （轮询输入），此处仅补 Viewer 侧的激活路径。
          if (!_isHost && _phase != ConnectionPhase.active) {
            _phase = ConnectionPhase.active;
            _startMediaLoops();
          }
          notifyListeners();
          break;
        case MessageType.encrypted:
          _backend.decrypt(bytes); // 真实实现在此分发控制/输入事件
          notifyListeners();
          break;
        default:
          break;
      }
    } on RdCoreException catch (e) {
      _fail(e.message);
    }
  }

  /// 生成并缓存本端会话密钥字节（Rust 端 ECDH 要求 ingest 对端密钥前本端已生成
  /// 自己的 ephemeral key）。Host 在收到 Offer 即 prepare，但暂不发送。
  void _prepareSessionKey() {
    _pendingSessionKey = _backend.makeSessionKeyExchange();
  }

  /// 发送已缓存的本端会话密钥（Host 仅在 approve 后调用，以门控 Viewer 激活）。
  void _sendSessionKey() {
    if (_pendingSessionKey != null) {
      _transport.send(_pendingSessionKey!);
    }
  }

  void _refreshPeer() {
    _peerName = _backend.peerDisplayName;
    _peerFingerprint = _backend.peerFingerprint;
    notifyListeners();
  }

  /// Host 批准本次连接（授予指定范围，可选有效期）。
  void approve(Set<ConsentScope> scopes, {Duration? duration}) {
    if (!_isHost) return;
    if (_backend.selfSignaling) return; // 真实 WebRTC：授权在连接创建时（scopesMask）已决定
    final st = _backend.hostDecide(grant: true, scopes: scopes, duration: duration);
    _grantedScopes = scopes;
    _applyState(st);
    _sendSessionKey(); // 把会话密钥发给 Viewer，使其侧也激活
  }

  /// Host 拒绝本次连接。
  void deny() {
    if (!_isHost) return;
    if (_backend.selfSignaling) return; // 真实 WebRTC：授权在连接创建时（scopesMask）已决定
    final st = _backend.hostDecide(grant: false, scopes: {});
    _applyState(st);
  }

  /// 主动撤销 / 终止连接（Host 或 Viewer 均可）。
  void revoke() {
    if (_backend.selfSignaling) {
      // 真实 WebRTC 后端（RdConnection）没有「撤销授权」语义——其 `revoke()` 是未实现桩
      // （直接抛 UnsupportedError）。主动断开 = 销毁底层连接：worker isolate 释放 FFI 句柄 →
      // PeerConnection 关闭 → 对端（Host）经 `peer_gone` 感知到断开并自动等待下一次重连。
      _peerWatch?.cancel();
      _peerWatch = null;
      _stopMediaLoops();
      _backend.dispose();
      _phase = ConnectionPhase.closed;
      _closedReason = ClosedReason.disconnected;
      notifyListeners();
      return;
    }
    final st = _backend.revoke();
    _applyState(st);
  }

  /// 对端（受控端 / Host）主动断开：由 [NativeRtcConnection.onPeerDisconnected] 触发。
  /// 保持 active 相位（保留最后一帧作背景），仅置 [peerDisconnected] 让 UI 叠浮层提示。
  void _onPeerGone() {
    if (_disposed || _peerDisconnected) return;
    _peerDisconnected = true;
    _stopMediaLoops();
    notifyListeners();
  }

  /// 经端到端加密通道发送输入/控制负载（仅在已加密时有效）。
  void sendInput(Uint8List payload) {
    if (!_encrypted) return;
    try {
      _transport.send(_backend.encrypt(payload));
    } on RdCoreException catch (e) {
      _fail(e.message);
    }
  }

  void _applyState(ConnectionState st) {
    if (st.isActive) {
      _phase = ConnectionPhase.active;
      _startMediaLoops();
    } else if (st.kind == ConnectionStateKind.denied) {
      _phase = ConnectionPhase.denied;
      _stopMediaLoops();
    } else if (st.isClosed) {
      _phase = ConnectionPhase.closed;
      _closedReason = st.closedReason;
      _stopMediaLoops();
    }
    notifyListeners();
  }

  /// 连接激活后启动媒体循环：Viewer 周期拉帧 + 拉音频，Host 周期轮询输入。
  void _startMediaLoops() {
    _stopMediaLoops();
    if (_isHost) {
      _inputTimer = Timer.periodic(const Duration(milliseconds: 50), (_) {
        if (_disposed) return;
        final ev = _backend.pollInput();
        if (ev != null) {
          _lastInput = ev;
          notifyListeners();
        }
      });
    } else {
      _frameTimer = Timer.periodic(const Duration(milliseconds: 50), (_) {
        if (_disposed) return;
        final f = _backend.pullFrame();
        if (f != null) {
          _lastFrame = f;
          _frameSize = ui.Size(f.width.toDouble(), f.height.toDouble());
          _frameStalled = false;
          _frameError = null;
          notifyListeners();
        } else {
          // 拉帧无新帧：把后端透传的 Rust 错误（解码失败 / 通道关闭）暴露出来。
          final err = _backend.frameError;
          if (err != null && err.isNotEmpty && err != _frameError) {
            _frameError = err;
            notifyListeners();
          }
        }
      });
      // 看门狗：激活后 10s 内仍未收到任何画面帧，标记卡顿并给出诊断，避免 UI 永久
      // 显示 loading 无信息。「Host 未推流」时 Rust 会一直阻塞等帧、不会返回错误，
      // 因此必须由看门狗主动兜底。
      Future.delayed(const Duration(seconds: 10), () {
        if (_disposed || _phase != ConnectionPhase.active) return;
        if (_lastFrame == null && !_frameStalled) {
          _frameStalled = true;
          _frameError ??= '连接已建立，但 10 秒内未收到任何画面帧。常见原因：\n'
              '① 被控端未真正开始抓屏/推送（检查被控端程序是否正常运行、终端是否报错）；\n'
              '② 本端 H.264 解码失败（若有具体错误会显示在此处）。';
          notifyListeners();
        }
      });
      _audioTimer = Timer.periodic(const Duration(milliseconds: 50), (_) {
        if (_disposed) return;
        final a = _backend.pullAudio();
        if (a != null) {
          _lastAudio = a;
          _audioLevel = _muted ? 0.0 : a.rmsLevel();
          notifyListeners();
        }
      });
      // 主线程断线看门狗：本端最近一次实际收到远程画面帧（由 NativeRtcConnection 在收到
      // 后台 isolate 推送的 'frame' 时更新 lastFrameAt）。后台 isolate 的拉帧调用会阻塞在
      // Rust recv_rendered——对端静默掉线 / 被杀 / 网络中断时 WebRTC 数据通道不会即时关闭，
      // recv 一直阻塞，导致 isolate 内的看门狗与 peer_state 轮询都无法运行；故断线检测必须
      // 放在主线程：主线程永不阻塞，能稳定感知「画面帧停止到达」。
      // 阈值 5s：兼顾「避免瞬时卡顿误判」与「及时提示」。
      _peerWatchdog = Timer.periodic(const Duration(seconds: 1), (_) {
        if (_disposed ||
            _peerDisconnected ||
            _isHost ||
            _phase != ConnectionPhase.active) {
          return;
        }
        final backend = _backend;
        if (backend is NativeRtcConnection) {
          final last = backend.lastFrameAt;
          if (last != null &&
              DateTime.now().difference(last) > const Duration(seconds: 5)) {
            _onPeerGone();
          }
        }
      });
    }
  }

  void _stopMediaLoops() {
    _frameTimer?.cancel();
    _inputTimer?.cancel();
    _audioTimer?.cancel();
    _peerWatchdog?.cancel();
    _frameTimer = null;
    _inputTimer = null;
    _audioTimer = null;
    _peerWatchdog = null;
  }

  /// Viewer 经端到端加密数据通道发送一条输入事件（鼠标/键盘/滚轮）。
  void sendInputEvent(RdInputEvent event) {
    if (!_encrypted) return;
    try {
      _backend.sendInputEvent(event);
    } on RdCoreException catch (e) {
      _fail(e.message);
    }
  }

  void _setPhase(ConnectionPhase p) {
    _phase = p;
    notifyListeners();
  }

  void _fail(String msg) {
    _error = msg;
    _phase = ConnectionPhase.failed;
    notifyListeners();
  }

  @override
  void dispose() {
    // 幂等：widget 可能被框架多次 dispose（例如测试里显式释放 + DemoScreen 释放），
    // 而底层 FFI 句柄（RdSession / RdLocal）只能释放一次，二次释放会 double-free。
    if (_disposed) return;
    _disposed = true;
    _peerWatch?.cancel();
    _peerWatch = null;
    _stopMediaLoops();
    _backend.dispose();
    _transport.close();
    super.dispose();
  }
}

/// 安全指示器的 UI 快照（不可伪造横幅所需数据）。
class SecurityIndicatorSnapshot {
  const SecurityIndicatorSnapshot({
    this.peerName,
    this.peerFingerprint,
    required this.encrypted,
    required this.phase,
    required this.grantedScopes,
    this.closedReason,
  });

  final String? peerName;
  final String? peerFingerprint;
  final bool encrypted;
  final ConnectionPhase phase;
  final Set<ConsentScope> grantedScopes;
  final ClosedReason? closedReason;
}
