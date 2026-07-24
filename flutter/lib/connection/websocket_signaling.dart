import 'dart:async';
import 'dart:io';
import 'dart:typed_data';

import 'signaling.dart';

/// 真实 WebSocket 信令传输：连到 `signaling-svc`（Rust 端，P2 已实现），云端只做中转，
/// 看不到媒体或控制内容。
///
/// URL 形态严格为 `ws(s)://host/<sessionHex>?token=<token>`（路径第一段即 session，
/// 与服务端 `req.uri().path().trim_start_matches('/').split('/').next()` 解析一致）。
///
/// 行为细节：
/// - 连接尚未建立时 [send] 的字节先入缓冲，连接成功后自动 flush；
/// - 底层的连接失败 / 异常 / 远端关闭都在内部消化（通过优雅关闭 [incoming] 反映，
///   而非抛出未处理异常），上层 [ConnectionController] 只需消费正常信令字节；
/// - [close] 幂等。
/// - [allowInsecure] 仅用于开发：接受自签/无效证书（等价于 Rust 端 TLS 测试的
///   `badCertificateCallback`），生产环境必须保持 false 由系统信任链校验 `wss://`。
class WebSocketSignaling implements SignalingTransport {
  WebSocketSignaling(this.url, {this.allowInsecure = false}) {
    unawaited(_connect());
  }

  final String url;
  final bool allowInsecure;

  /// 开发模式自签证书时创建的临时 HttpClient（wss 握手忽略证书校验）。
  HttpClient? _client;

  WebSocket? _ws;
  final List<Uint8List> _sendBuffer = [];
  bool _connecting = true;
  bool _closed = false;

  final StreamController<Uint8List> _incomingCtrl =
      StreamController<Uint8List>.broadcast();

  @override
  Stream<Uint8List> get incoming => _incomingCtrl.stream;

  Future<void> _connect() async {
    try {
      // 开发模式（allowInsecure）：用临时 HttpClient 忽略证书校验，便于对接自签 TLS
      // 的信令服务。生产环境保持默认（false），由系统信任链校验 wss://。
      if (allowInsecure) {
        _client = HttpClient()..badCertificateCallback = (_, __, ___) => true;
      }
      final ws = await WebSocket.connect(url, customClient: _client);
      if (_closed) {
        // 在等待连接期间已被 close：直接丢弃。
        await ws.close();
        return;
      }
      _ws = ws;
      _connecting = false;
      // 把连接建立前缓冲的信令字节 flush 出去。
      for (final m in _sendBuffer) {
        _rawSend(m);
      }
      _sendBuffer.clear();
      ws.listen(
        (data) {
          if (_incomingCtrl.isClosed) return;
          if (data is List<int>) {
            _incomingCtrl.add(Uint8List.fromList(data));
          }
        },
        onError: (_) {
          // 连接错误：内部消化，优雅关闭 incoming（不向上抛未处理异常）。
          _failClosed();
        },
        onDone: () {
          _failClosed();
        },
        cancelOnError: false,
      );
    } on Object {
      _connecting = false;
      _failClosed();
    }
  }

  void _rawSend(Uint8List message) {
    try {
      _ws?.add(message);
    } on Object {
      // 单帧发送失败（连接已断开）：忽略，上层会通过 incoming 关闭感知。
    }
  }

  void _failClosed() {
    if (!_incomingCtrl.isClosed) {
      _incomingCtrl.close();
    }
  }

  @override
  void send(Uint8List message) {
    if (_closed) return;
    if (_connecting || _ws == null) {
      // 连接未建立：先缓冲。
      _sendBuffer.add(Uint8List.fromList(message));
      return;
    }
    _rawSend(message);
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    _connecting = false;
    _sendBuffer.clear();
    final ws = _ws;
    _ws = null;
    try {
      await ws?.close();
    } on Object {
      // 忽略关闭异常。
    }
    // 开发模式下的临时 HttpClient 随连接一起释放。
    try {
      _client?.close(force: true);
    } on Object {
      // 忽略关闭异常。
    }
    _client = null;
    _failClosed();
  }
}
