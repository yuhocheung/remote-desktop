import 'dart:typed_data';
import '../models/connection_state.dart';
import '../models/security_indicator.dart';
import '../models/consent_scope.dart';
import '../models/media_frame.dart';
import '../models/remote_input.dart';

/// 连接后端异常（原生调用失败 / 握手被拒 / 非法参数等）。
class RdCoreException implements Exception {
  RdCoreException(this.message);
  final String message;
  @override
  String toString() => 'RdCoreException: $message';
}

/// 一次连接会话的后端抽象。
///
/// 原生实现见 [NativeConnection]（经由 dart:ffi 调用 Rust 核心）；
/// 测试 / 演示见 [MockConnection]（纯 Dart，无需原生库）。
///
/// 约定（与 Rust 端一致）：
/// - `make*` 返回 postcard 字节，由调用方经信令通道发给对端；
/// - `ingest*` 收下对端字节，失败抛 [RdCoreException]；
/// - 状态类方法（`hostDecide`/`tick`/`revoke`/`connectionState`/`securityIndicator`）
///   成功返回解析后的 Dart 对象，失败抛异常。
abstract class ConnectionBackend {
  Uint8List makeOffer();
  void ingestOffer(Uint8List bytes);
  Uint8List makeAnswer();
  void ingestAnswer(Uint8List bytes);
  Uint8List makeSessionKeyExchange();
  void ingestSessionKeyExchange(Uint8List bytes);
  Uint8List encrypt(Uint8List plaintext);
  Uint8List decrypt(Uint8List ciphertext);
  ConnectionState hostRequestConsent({String? pin});
  ConnectionState hostDecide({
    required bool grant,
    required Set<ConsentScope> scopes,
    Duration? duration,
  });
  ConnectionState tick();
  void heartbeat();
  ConnectionState revoke();
  ConnectionState onDisconnected();
  ConnectionState get connectionState;
  SecurityIndicator securityIndicator(bool encrypted);
  String? get peerDisplayName;
  String? get peerFingerprint;

  /// 后端是否自管理信令（真实 WebRTC：Rust 内部完成 Offer/Answer/ICE/E2E/同意）。
  /// 为 true 时 [ConnectionController] 不会驱动手动握手，而是调用 [establish]，
  /// 也不再需要 Flutter 侧打开 [SignalingTransport]（Rust 端已自持信令）。
  bool get selfSignaling => false;

  /// 跑完整握手（仅自管理信令后端真正实现；其余后端为 no-op，手动流程由其自身驱动）。
  ///
  /// 改为 `Future<void>`：真实后端在后台 isolate 中跑，可能等待对端同意，
  /// 调用方需 `await` 以免阻塞 UI 线程。
  Future<void> establish() async {}

  /// 是否已进入「等待对端同意」阶段（仅自管理信令后端在后台 isolate 阻塞于同意等待时为
  /// true）。供 [ConnectionController] 在 connecting → active 之间把相位推进到
  /// [ConnectionPhase.awaitingConsent]，让 UI 显示「等待 Host 同意…」而非一直
  /// 「正在建立连接…」。默认 false。
  bool get awaitingConsent => false;

  // ───────────────── 媒体 / 输入（Track A） ─────────────────
  /// Viewer 拉取一帧已解密/解码/渲染的远程画面（RGBA）。无新帧返回 null。
  RdMediaFrame? pullFrame();

  /// Viewer 侧拉帧/解码相关的最近诊断信息（Rust `last_error` 透传；无则 null）。
  /// 用于「已连接但无画面」时给出可操作的错误，而非永远 loading。
  String? get frameError => null;

  /// Viewer 经端到端加密数据通道发送一条输入事件（鼠标/键盘/滚轮）。
  void sendInputEvent(RdInputEvent event);

  /// Host 轮询 Viewer 发来的输入事件；无待处理事件返回 null。
  RdInputEvent? pollInput();

  // ───────────────── 音频（Track A） ─────────────────
  /// Viewer 拉取一帧已解密/解码的远程音频（Raw 16-bit 交错 PCM 或 Opus）。无新帧返回 null。
  RdAudioFrame? pullAudio();

  /// 把本 Host 会话与给定 Viewer 会话接入同一对进程内音频通道（headless 单测 / 演示用，
  /// 真实部署走 WebRTC `audio` DataChannel id=2）。
  void attachLoopbackAudio(ConnectionBackend viewer);

  /// Host 设置音频抓取源（headless 静音 / 固定字节合成 PCM；真实采集由 `real` feature 提供）。
  void hostSetCaptureAudio({
    required int channels,
    required int sampleRate,
    required int samplesPerFrame,
    required int frames,
    required int byte,
  });

  /// Host 以指定 fps 推送抓取音频帧（自有线程 + tokio runtime 驱动）。
  void hostStartCaptureAudio(int fps);

  void dispose();
}
