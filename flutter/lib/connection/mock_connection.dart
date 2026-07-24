import 'dart:typed_data';
import '../models/connection_state.dart';
import '../models/security_indicator.dart';
import '../models/consent_scope.dart';
import '../models/media_frame.dart';
import '../models/remote_input.dart';
import 'connection_backend.dart';

/// 纯 Dart 的连接后端模拟，用于单元测试 / 演示，无需原生库。
///
/// 它复刻 Rust 端的关键状态机行为（同意门控、撤销、关闭），但不做任何真实密码学，
/// 仅用于验证 [ConnectionController] 的编排逻辑。
class MockConnection implements ConnectionBackend {
  MockConnection({
    required this.isHost,
    this.pin,
    this.unattendedScopes,
  });

  final bool isHost;
  final String? pin;
  final Set<ConsentScope>? unattendedScopes;

  String? _peerName;
  ConnectionState _state = const ConnectionState.awaiting();

  @override
  Uint8List makeOffer() => Uint8List.fromList([0]); // 首字节 = MessageType.offer

  @override
  void ingestOffer(Uint8List bytes) {
    _peerName = 'peer-device';
  }

  @override
  Uint8List makeAnswer() => Uint8List.fromList([1]); // 首字节 = MessageType.answer

  @override
  void ingestAnswer(Uint8List bytes) {
    _peerName = 'peer-device';
  }

  @override
  Uint8List makeSessionKeyExchange() => Uint8List.fromList([6]); // 首字节 = MessageType.sessionKey

  @override
  void ingestSessionKeyExchange(Uint8List bytes) {
    // 模拟实现只记录已收到对端会话密钥（真实实现见 NativeConnection）。
  }

  @override
  Uint8List encrypt(Uint8List plaintext) => Uint8List.fromList(plaintext);

  @override
  Uint8List decrypt(Uint8List ciphertext) => Uint8List.fromList(ciphertext);

  @override
  ConnectionState hostRequestConsent({String? pin}) {
    if (!isHost) throw RdCoreException('仅 Host 可管理同意');
    if (pin != null) {
      _state = (pin == this.pin)
          ? ConnectionState.active(unattendedScopes ??
              {
                ConsentScope.view,
                ConsentScope.input,
                ConsentScope.clipboard,
                ConsentScope.fileTransfer
              })
          : const ConnectionState.denied('临时 PIN 不匹配');
    } else {
      _state = const ConnectionState.awaiting();
    }
    return _state;
  }

  @override
  ConnectionState hostDecide({
    required bool grant,
    required Set<ConsentScope> scopes,
    Duration? duration,
  }) {
    if (!isHost) throw RdCoreException('仅 Host 可管理同意');
    _state = grant
        ? ConnectionState.active(scopes)
        : const ConnectionState.denied('Host 拒绝');
    return _state;
  }

  @override
  ConnectionState tick() => _state;

  @override
  void heartbeat() {}

  @override
  ConnectionState revoke() {
    _state = const ConnectionState.closed(ClosedReason.revoked);
    return _state;
  }

  @override
  ConnectionState onDisconnected() {
    if (_state.isActive) _state = const ConnectionState.closed(ClosedReason.disconnected);
    return _state;
  }

  @override
  ConnectionState get connectionState => _state;

  @override
  SecurityIndicator securityIndicator(bool encrypted) {
    return SecurityIndicator(
      displayName: _peerName ?? 'unknown',
      deviceId: List.filled(16, 0),
      fingerprint: List.filled(32, 0),
      fingerprintSpaced: '00 00 00 00',
      state: _state,
      encrypted: encrypted,
    );
  }

  @override
  String? get peerDisplayName => _peerName;

  @override
  String? get peerFingerprint => '00 11 22 33';

  @override
  bool get selfSignaling => false;
  @override
  bool get awaitingConsent => false;

  @override
  Future<void> establish() async {
    // Mock 后端由 DemoSession 驱动内存回环握手，无需自管理握手。
  }

  // ───────────────── 媒体 / 输入（Track A mock） ─────────────────
  // 合成一帧 2x2 纯灰绿相间的 RGBA 画面，便于 widget / 后端测试断言。
  static final RdMediaFrame _sampleFrame = RdMediaFrame(
    width: 2,
    height: 2,
    rgba: Uint8List.fromList(<int>[
      128, 128, 128, 255, //
      128, 128, 128, 255,
      0, 200, 0, 255,
      0, 200, 0, 255,
    ]),
  );

  RdInputEvent? _lastSentInput;

  /// 最近一次 [sendInputEvent] 发送的输入（测试断言用）。
  RdInputEvent? get lastSentInput => _lastSentInput;

  @override
  RdMediaFrame? pullFrame() => _sampleFrame;

  @override
  String? get frameError => null;

  @override
  void sendInputEvent(RdInputEvent event) {
    _lastSentInput = event;
  }

  @override
  RdInputEvent? pollInput() => null;

  // ───────────────── 音频（Track A mock） ─────────────────
  // 合成一帧 10ms @ 48kHz 立体声 16-bit 静音 PCM，便于 widget / 后端测试断言。
  // `data` 长度 = 480 采样 × 2 声道 × 2 字节 = 1920 字节（全 0 = 静音）。
  static final RdAudioFrame _sampleAudio = RdAudioFrame(
    codec: 0,
    channels: 2,
    sampleRate: 48000,
    data: Uint8List(480 * 2 * 2),
  );

  @override
  RdAudioFrame? pullAudio() => _sampleAudio;

  @override
  void attachLoopbackAudio(ConnectionBackend viewer) {
    // Mock 无真实通道；pullAudio 始终返回合成帧，故此处为 no-op。
  }

  @override
  void hostSetCaptureAudio({
    required int channels,
    required int sampleRate,
    required int samplesPerFrame,
    required int frames,
    required int byte,
  }) {
    // Mock 无真实采集；真实抓取仅在 NativeConnection + 原生库可用时生效。
  }

  @override
  void hostStartCaptureAudio(int fps) {
    // Mock 无真实采集线程。
  }

  @override
  void dispose() {}
}
