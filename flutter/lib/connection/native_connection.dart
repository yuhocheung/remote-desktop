import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';
import 'package:ffi/ffi.dart';
import '../ffi/rdcore_bindings.dart';
import '../models/connection_state.dart';
import '../models/security_indicator.dart';
import '../models/consent_scope.dart';
import '../models/media_frame.dart';
import '../models/remote_input.dart';
import 'connection_backend.dart';
import 'local_identity.dart';

/// 基于 dart:ffi 的真实连接后端，封装 Rust 端的 `RdLocal` / `RdSession` 不透明句柄。
///
/// 所有跨 FFI 的内存都由本类在调用后即时释放：Rust 返回的字符串用
/// `rdcore_string_free` 释放，字节缓冲区用 `rdcore_bytes_free` 释放；传入 Rust 的
/// 临时字符串 / 字节在调用后由 `malloc.free` 释放（Rust 端只读，不接管所有权）。
class NativeConnection implements ConnectionBackend {
  NativeConnection._(this._lib, this._local, this._session, this._isHost);

  final RdCoreLib _lib;
  final Pointer<Void> _local;
  final Pointer<Void> _session;
  final bool _isHost;

  /// 基于已存在的 [LocalIdentity] 开会话（复用其 `RdLocal` 句柄）。
  /// [role] 为 `'host'` 或 `'viewer'`；[sessionId] 必须 16 字节；
  /// [unattendedPin] 非空则进入无人值守模式（凭 PIN 自动放行）。
  factory NativeConnection.fromIdentity(
    LocalIdentity local, {
    required String role,
    required Uint8List sessionId,
    String? unattendedPin,
  }) {
    final lib = local.lib;
    if (sessionId.length != 16) {
      throw RdCoreException('sessionId 必须为 16 字节');
    }
    final sidPtr = malloc<Uint8>(16);
    sidPtr.asTypedList(16).setAll(0, sessionId);
    final pinPtr = unattendedPin?.toNativeUtf8() ?? nullptr;
    final session = lib.sessionNew(local.handle, role == 'host' ? 1 : 0, sidPtr, pinPtr);
    malloc.free(sidPtr);
    if (pinPtr != nullptr) malloc.free(pinPtr);
    if (session.address == 0) {
      throw RdCoreException('创建会话失败: ${lib.takeLastError() ?? "未知"}');
    }
    return NativeConnection._(lib, local.handle, session, role == 'host');
  }

  Uint8List _readBytes(Pointer<RdBytes> p) {
    if (p.address == 0) {
      throw RdCoreException('原生调用失败: ${_lib.takeLastError() ?? "未知"}');
    }
    final bytes = p.ref.data.asTypedList(p.ref.len);
    final copy = Uint8List.fromList(bytes);
    _lib.bytesFree(p);
    return copy;
  }

  /// 数据 / 状态类返回（非空 = 成功字符串，NULL = 失败）。
  String _readString(Pointer<Utf8> p) {
    if (p.address == 0) {
      throw RdCoreException('原生调用失败: ${_lib.takeLastError() ?? "未知"}');
    }
    final s = p.toDartString();
    _lib.stringFree(p);
    return s;
  }

  /// 命令类返回（NULL = 成功，非空 = 错误串）。
  void _checkCommand(Pointer<Utf8> p) {
    if (p.address != 0) {
      final s = p.toDartString();
      _lib.stringFree(p);
      throw RdCoreException(s);
    }
  }

  // 注意：serde 对 `ConnectionState` 的单元变体 `AwaitingConsent` 输出裸字符串
  // `"AwaitingConsent"`，结构/元组变体才输出对象。因此这里必须把 `jsonDecode` 的结果
  // 作为 `dynamic` 交给 [ConnectionState.fromJson]（它已分别处理 String 与 Map 两种形态），
  // 不能预先 `as Map<String, dynamic>`，否则 `AwaitingConsent` 会抛 _TypeError。
  ConnectionState _stateFrom(Pointer<Utf8> p) =>
      ConnectionState.fromJson(jsonDecode(_readString(p)));

  Pointer<Utf8> _callIngest(
    Pointer<Utf8> Function(Pointer<Void>, Pointer<Uint8>, int) fn,
    Uint8List bytes,
  ) {
    final p = malloc<Uint8>(bytes.length);
    p.asTypedList(bytes.length).setAll(0, bytes);
    final r = fn(_session, p, bytes.length);
    malloc.free(p);
    return r;
  }

  Pointer<RdBytes> _callBytes(
    Pointer<RdBytes> Function(Pointer<Void>, Pointer<Uint8>, int) fn,
    Uint8List bytes,
  ) {
    final p = malloc<Uint8>(bytes.length);
    p.asTypedList(bytes.length).setAll(0, bytes);
    final r = fn(_session, p, bytes.length);
    malloc.free(p);
    return r;
  }

  @override
  Uint8List makeOffer() => _readBytes(_lib.makeOffer(_session));

  @override
  void ingestOffer(Uint8List bytes) =>
      _checkCommand(_callIngest(_lib.ingestOffer, bytes));

  @override
  Uint8List makeAnswer() => _readBytes(_lib.makeAnswer(_session));

  @override
  void ingestAnswer(Uint8List bytes) =>
      _checkCommand(_callIngest(_lib.ingestAnswer, bytes));

  @override
  Uint8List makeSessionKeyExchange() => _readBytes(_lib.makeSessionKey(_session));

  @override
  void ingestSessionKeyExchange(Uint8List bytes) =>
      _checkCommand(_callIngest(_lib.ingestSessionKey, bytes));

  @override
  Uint8List encrypt(Uint8List plaintext) =>
      _readBytes(_callBytes(_lib.encrypt, plaintext));

  @override
  Uint8List decrypt(Uint8List ciphertext) =>
      _readBytes(_callBytes(_lib.decrypt, ciphertext));

  @override
  ConnectionState hostRequestConsent({String? pin}) {
    if (!_isHost) throw RdCoreException('仅 Host 可管理同意');
    final pinPtr = pin?.toNativeUtf8() ?? nullptr;
    final r = _lib.hostRequestConsent(_session, pinPtr);
    if (pinPtr != nullptr) malloc.free(pinPtr);
    return _stateFrom(r);
  }

  @override
  ConnectionState hostDecide({
    required bool grant,
    required Set<ConsentScope> scopes,
    Duration? duration,
  }) {
    if (!_isHost) throw RdCoreException('仅 Host 可管理同意');
    final mask = scopes.fold<int>(0, (m, s) => m | s.bitmask);
    final r = _lib.hostDecide(
      _session,
      grant ? 1 : 0,
      mask,
      duration?.inSeconds ?? 0,
    );
    return _stateFrom(r);
  }

  @override
  ConnectionState tick() => _stateFrom(_lib.tick(_session));

  @override
  void heartbeat() => _lib.heartbeat(_session);

  @override
  ConnectionState revoke() => _stateFrom(_lib.revoke(_session));

  @override
  ConnectionState onDisconnected() => _stateFrom(_lib.onDisconnected(_session));

  @override
  ConnectionState get connectionState =>
      _stateFrom(_lib.connectionState(_session));

  @override
  SecurityIndicator securityIndicator(bool encrypted) {
    final r = _lib.securityIndicator(_session, encrypted ? 1 : 0);
    return SecurityIndicator.fromJson(jsonDecode(_readString(r)) as Map<String, dynamic>);
  }

  @override
  String? get peerDisplayName {
    final p = _lib.peerDisplayName(_session);
    if (p.address == 0) return null;
    final s = p.toDartString();
    _lib.stringFree(p);
    return s;
  }

  @override
  String? get peerFingerprint {
    final p = _lib.peerFingerprint(_session);
    if (p.address == 0) return null;
    final s = p.toDartString();
    _lib.stringFree(p);
    return s;
  }

  @override
  bool get selfSignaling => false;
  @override
  bool get awaitingConsent => false;

  @override
  Future<void> establish() async {
    // headless 后端走手动 Offer/Answer 流程（由 ConnectionController 驱动），无需自管理握手。
  }

  @override
  void dispose() {
    _lib.sessionFree(_session);
    _lib.identityFree(_local);
  }

  // ───────────────── 媒体 / 输入（Track A） ─────────────────

  @override
  RdMediaFrame? pullFrame() {
    final p = _lib.viewerPullFrame(_session);
    if (p.address == 0) return null;
    final w = p.ref.width;
    final h = p.ref.height;
    final copy = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
    _lib.mediaFrameFree(p);
    return RdMediaFrame(width: w, height: h, rgba: copy);
  }

  @override
  String? get frameError => null;

  @override
  void sendInputEvent(RdInputEvent event) {
    if (event.kind == RdInputKind.keyWithChar) {
      // IME 友好双发：character 走独立 FFI 路径（C struct 无法承载字符串）。
      final hasChar = event.character != null && event.character!.isNotEmpty;
      final charPtr = hasChar
          ? event.character!.toNativeUtf8().cast<Uint8>()
          : Pointer<Uint8>.fromAddress(0);
      try {
        _checkCommand(_lib.viewerSendInputKey(_session, event.keyCode, charPtr,
            event.pressed ? 1 : 0, event.modifiers));
      } finally {
        if (hasChar) calloc.free(charPtr);
      }
      return;
    }
    final p = calloc<NativeRdInputEvent>();
    event.writeInto(p.ref);
    try {
      _checkCommand(_lib.viewerSendInput(_session, p));
    } finally {
      _lib.inputEventFree(p);
    }
  }

  @override
  RdInputEvent? pollInput() {
    final p = _lib.hostPollInput(_session);
    if (p.address == 0) return null;
    final ev = RdInputEvent.fromNative(p.ref);
    _lib.inputEventFree(p);
    return ev;
  }

  // ───────────────── 音频（Track A） ─────────────────

  @override
  RdAudioFrame? pullAudio() {
    final p = _lib.viewerPullAudio(_session);
    if (p.address == 0) return null;
    final codec = p.ref.codec;
    final channels = p.ref.channels;
    final sampleRate = p.ref.sampleRate;
    final copy = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
    _lib.audioFrameFree(p);
    return RdAudioFrame(
      codec: codec,
      channels: channels,
      sampleRate: sampleRate,
      data: copy,
    );
  }

  @override
  void attachLoopbackAudio(ConnectionBackend viewer) {
    if (viewer is! NativeConnection) {
      throw RdCoreException('attachLoopbackAudio 需要 NativeConnection 类型的对端');
    }
    _checkCommand(_lib.attachLoopbackAudio(_session, viewer._session));
  }

  @override
  void hostSetCaptureAudio({
    required int channels,
    required int sampleRate,
    required int samplesPerFrame,
    required int frames,
    required int byte,
  }) {
    _checkCommand(_lib.hostSetCaptureAudio(
      _session,
      channels,
      sampleRate,
      samplesPerFrame,
      frames,
      byte,
    ));
  }

  @override
  void hostStartCaptureAudio(int fps) {
    _checkCommand(_lib.hostStartCaptureAudio(_session, fps));
  }

  // ───── 以下为 Host 侧媒体回环 seam（headless 单测用，真实部署走 WebRTC 数据通道） ─────

  /// 把本 Host 会话与给定 Viewer 会话接入同一对进程内媒体/输入通道。
  void attachLoopbackMedia(NativeConnection viewer) {
    _checkCommand(_lib.attachLoopbackMedia(_session, viewer._session));
  }

  /// Host 设置抓取源（headless 纯色合成帧）。
  void hostSetCapture({
    required int width,
    required int height,
    required int frames,
    required int color,
  }) {
    _checkCommand(_lib.hostSetCapture(_session, width, height, frames, color));
  }

  /// Host 以指定 fps 推送抓取帧（自有线程 + tokio runtime 驱动）。
  void hostStartCapture(int fps) {
    _checkCommand(_lib.hostStartCapture(_session, fps));
  }
}
