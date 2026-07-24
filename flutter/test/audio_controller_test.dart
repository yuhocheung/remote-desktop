import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/connection_controller.dart';
import 'package:remote_desktop/connection/mock_connection.dart';
import 'package:remote_desktop/connection/signaling.dart';
import 'package:remote_desktop/models/consent_scope.dart';

/// 音频（Track A）控制器行为测试：激活后 Viewer 周期拉到音频帧，
/// 静音状态可切换、音量被钳制到 [0,1]。
void main() {
  test('已激活 Viewer 拉到音频帧；静音切换与音量钳制', () async {
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

    // 音频定时器（50ms 周期）应至少拉到一帧。
    await Future.delayed(const Duration(milliseconds: 150));
    expect(viewer.lastAudio, isNotNull, reason: '激活后 Viewer 应拉到音频帧');
    expect(viewer.lastAudio!.isRaw, isTrue);

    // 静音状态切换。
    viewer.setMute(true);
    expect(viewer.muted, isTrue);
    viewer.setMute(false);
    expect(viewer.muted, isFalse);

    // 音量钳制到 [0,1]。
    viewer.setVolume(5.0);
    expect(viewer.volume, 1.0);
    viewer.setVolume(-1.0);
    expect(viewer.volume, 0.0);

    viewer.dispose();
    host.dispose();
  });
}
