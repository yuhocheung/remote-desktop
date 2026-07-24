import 'package:flutter_test/flutter_test.dart';

import 'package:remote_desktop/connection/connection_controller.dart';
import 'package:remote_desktop/connection/mock_connection.dart';
import 'package:remote_desktop/connection/signaling.dart';
import 'package:remote_desktop/models/consent_scope.dart';

/// 用 [MockConnection] + [InMemorySignaling] 验证 [ConnectionController] 的状态机，
/// 不依赖原生库，可在 CI / `flutter test` 中直接运行。
void main() {
  test('完整握手：Viewer 发起 → Host 等待同意 → 允许 → 双方激活且加密', () async {
    final (hostSig, viewerSig) = InMemorySignaling.pair();
    final host = ConnectionController(
        backend: MockConnection(isHost: true), transport: hostSig, isHost: true);
    final viewer = ConnectionController(
        backend: MockConnection(isHost: false),
        transport: viewerSig,
        isHost: false);

    // Viewer 发起 Offer（异步派发：offer→answer→sessionKey 级联在 microtask 中完成）。
    viewer.startAsViewer();
    await Future(() {});

    // Host 应进入「等待同意」并已知对端名。
    expect(host.phase, ConnectionPhase.awaitingConsent);
    expect(host.peerName, isNotNull);
    expect(host.peerName, equals('peer-device'));

    // Host 允许 view + input。Host 会话密钥经信令送达 Viewer 后才算 E2E 建立。
    host.approve({ConsentScope.view, ConsentScope.input});
    await Future(() {});

    expect(host.isActive, isTrue);
    expect(viewer.isActive, isTrue, reason: 'Viewer 应在 Host 同意后激活');
    expect(viewer.encrypted, isTrue, reason: '同意后 E2E 加密应建立');

    viewer.dispose();
    host.dispose();
  });

  test('Host 拒绝后双方进入拒绝/关闭', () async {
    final (hostSig, viewerSig) = InMemorySignaling.pair();
    final host = ConnectionController(
        backend: MockConnection(isHost: true), transport: hostSig, isHost: true);
    final viewer = ConnectionController(
        backend: MockConnection(isHost: false),
        transport: viewerSig,
        isHost: false);

    viewer.startAsViewer();
    await Future(() {});
    host.deny();
    await Future(() {});

    expect(host.phase, ConnectionPhase.denied);
    // 拒绝门控生效：Viewer 不应被激活（当前协议没有「拒绝」信令，
    // 真实产品会通过信令关闭 / 专用消息通知 Viewer，此处仅验证未被放行）。
    expect(viewer.isActive, isFalse);
    expect(viewer.phase, isNot(ConnectionPhase.active));

    viewer.dispose();
    host.dispose();
  });

  test('已激活后 Viewer 撤销 → 双方关闭', () async {
    final (hostSig, viewerSig) = InMemorySignaling.pair();
    final host = ConnectionController(
        backend: MockConnection(isHost: true), transport: hostSig, isHost: true);
    final viewer = ConnectionController(
        backend: MockConnection(isHost: false),
        transport: viewerSig,
        isHost: false);

    viewer.startAsViewer();
    await Future(() {});
    host.approve({ConsentScope.view});
    await Future(() {});
    expect(viewer.isActive, isTrue);

    viewer.revoke();
    await Future(() {});
    expect(viewer.isClosed, isTrue);

    viewer.dispose();
    host.dispose();
  });

  test('同意授予范围应反映到 grantedScopes', () async {
    final (hostSig, viewerSig) = InMemorySignaling.pair();
    final host = ConnectionController(
        backend: MockConnection(isHost: true), transport: hostSig, isHost: true);
    final viewer = ConnectionController(
        backend: MockConnection(isHost: false),
        transport: viewerSig,
        isHost: false);

    viewer.startAsViewer();
    await Future(() {});
    host.approve({ConsentScope.view, ConsentScope.input, ConsentScope.clipboard});
    await Future(() {});

    expect(host.grantedScopes, containsAll({
      ConsentScope.view,
      ConsentScope.input,
      ConsentScope.clipboard,
    }));
    expect(host.grantedScopes, isNot(contains(ConsentScope.fileTransfer)));

    viewer.dispose();
    host.dispose();
  });
}
