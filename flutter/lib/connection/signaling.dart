import 'dart:async';
import 'dart:typed_data';

/// 信令传输抽象：把本端产出的 postcard 信令字节发往信令服务器，并接收对端字节。
///
/// 真实实现会连 WebSocket 到 `signaling-svc`（Rust 端，P2 已实现），云端只做中转，
/// 看不到媒体或控制内容。此处只定义接口，[InMemorySignaling] 提供内存回环用于演示/测试。
abstract class SignalingTransport {
  /// 发送本端信令字节。
  void send(Uint8List message);

  /// 对端发来的信令字节流。
  Stream<Uint8List> get incoming;

  Future<void> close();
}

/// 内存回环信令：把一端 `send` 的字节直接投递给对端 `incoming`，用于演示与单元测试。
class InMemorySignaling implements SignalingTransport {
  InMemorySignaling._(this._peer);

  /// 创建一对互连的端点（a 与 b 互为对端），分别交给 Host 与 Viewer 使用。
  static (InMemorySignaling, InMemorySignaling) pair() {
    final a = InMemorySignaling._(null);
    final b = InMemorySignaling._(null);
    a._peer = b;
    b._peer = a;
    return (a, b);
  }

  /// 对端端点（与 `pair()` 返回的两端之一互连）。
  InMemorySignaling? _peer;

  // 注意：此处刻意不使用 sync:true。同步广播控制器会在 `add` 时立即回调监听者，
  // 而本端监听者（ConnectionController._onIncoming）内部又会反向 `send`，
  // 导致「同一控制器正在派发事件时再次 add」的 [`Bad state: Cannot fire new event`]
  // 重入错误。改用默认（异步/microtask）派发即可天然避免重入；测试侧在触发动作后
  // `await Future(() {})` 让整条级联（offer→answer→sessionKey→激活）在 microtask 中跑完。
  final StreamController<Uint8List> _ctrl =
      StreamController<Uint8List>.broadcast();

  @override
  void send(Uint8List message) {
    final peer = _peer;
    if (peer == null || peer._ctrl.isClosed) return;
    // 拷贝一份，避免收发双方共享可变 buffer。
    peer._ctrl.add(Uint8List.fromList(message));
  }

  @override
  Stream<Uint8List> get incoming => _ctrl.stream;

  @override
  Future<void> close() async {
    await _ctrl.close();
  }
}

/// 空操作信令传输：用于自管理信令后端（真实 WebRTC，Rust 端已自持信令）。
/// 后端不需要 Flutter 侧收发信令字节，故 send/incoming 均为空实现，避免
/// 与 Rust 端同会话重复连接信令服务器、重复消费一次性 token。
class NoopSignalingTransport implements SignalingTransport {
  @override
  void send(Uint8List message) {}

  @override
  Stream<Uint8List> get incoming => const Stream<Uint8List>.empty();

  @override
  Future<void> close() async {}
}

/// `Message` 枚举变体下标（与 Rust 端 postcard 编码一致）：
/// Offer=0, Answer=1, Ice=2, InputEvent=3, Clipboard=4, Heartbeat=5, SessionKey=6, Encrypted=7,
/// FileTransfer=8。
/// 维护约定（契约 §5）：Rust `Message` 仅在**末尾追加**变体；此处也必须只在末尾同步追加，
/// 否则 postcard 首字节下标错位、消息被静默丢弃。
enum MessageType {
  offer,
  answer,
  ice,
  inputEvent,
  clipboard,
  heartbeat,
  sessionKey,
  encrypted,
  fileTransfer,
}

/// 窥探 postcard 缓冲首字节，得到顶层消息类型以做路由（postcard 把枚举变体按下标编码，
/// 小下标占据单字节）。返回 null 表示无法识别。
MessageType? messageTypeFromBytes(Uint8List bytes) {
  if (bytes.isEmpty) return null;
  final idx = bytes[0];
  if (idx >= 0 && idx < MessageType.values.length) {
    return MessageType.values[idx];
  }
  return null;
}
