// 真机 GUI 端到端冒烟（integration_test，需有窗口的设备/模拟器/桌面运行）：
//   flutter test integration_test/smoke_test.dart -d <device>
// 或（Windows/Linux/macOS 桌面）：
//   flutter test integration_test
//
// 与 test/gui_e2e_smoke_test.dart 逻辑相同，但运行在真实渲染环境（而非 headless tester），
// 用于在有显示器/真机/模拟器上做最终 GUI 冒烟。rdcore_ffi.dll 可用时自动走真实 FFI 路径。
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:remote_desktop/connection/demo_session.dart';
import 'package:remote_desktop/ffi/rdcore_bindings.dart';
import 'package:remote_desktop/ui/demo_screen.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('真机 GUI 端到端冒烟：连接 → 同意 → 不可伪造加密横幅', (tester) async {
    final useNative = RdCoreLib().available;
    final session = buildDemoSession(deviceName: 'smoke', useNative: useNative);

    await tester.pumpWidget(MaterialApp(home: DemoScreen(session: session)));
    await tester.pumpAndSettle();

    await tester.tap(find.widgetWithText(FilledButton, '发起连接（Viewer 侧）'));
    await tester.pumpAndSettle();

    expect(find.text('有连接请求'), findsOneWidget,
        reason: 'Host 应收到连接请求并弹出同意弹窗');

    await tester.tap(find.widgetWithText(FilledButton, '允许'));
    await tester.pumpAndSettle();

    expect(find.text('端到端加密：已建立'), findsWidgets,
        reason: '双侧不可伪造安全横幅应显示加密已建立');

    session.dispose();
  });
}
