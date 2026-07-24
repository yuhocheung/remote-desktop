import 'package:flutter/material.dart';

import '../connection/connection_controller.dart';
import '../connection/demo_session.dart';
import 'consent_dialog.dart';
import 'remote_screen.dart';
import 'security_banner.dart';
import '../models/consent_scope.dart';

/// 演示画面：左右并排展示 Host 与 Viewer 两侧，完整跑通握手 → 同意 → 激活流程。
///
/// 真实产品中 Host 与 Viewer 通常不在同一设备上，本画面仅用于在本机直观演示
/// P6 的连接状态机与不可伪造安全指示器。
class DemoScreen extends StatefulWidget {
  const DemoScreen({super.key, required this.session});

  final DemoSession session;

  @override
  State<DemoScreen> createState() => _DemoScreenState();
}

class _DemoScreenState extends State<DemoScreen> {
  bool _started = false;

  @override
  void dispose() {
    widget.session.dispose();
    super.dispose();
  }

  void _start() {
    setState(() => _started = true);
    widget.session.start();
  }

  @override
  Widget build(BuildContext context) {
    final host = widget.session.host;
    final viewer = widget.session.viewer;
    return Scaffold(
      appBar: AppBar(
        title: const Text('远程桌面演示'),
        actions: [
          if (_started)
            IconButton(
              icon: const Icon(Icons.close),
              tooltip: '结束演示',
              onPressed: () => Navigator.of(context).pop(),
            ),
        ],
      ),
      body: !_started
          ? Center(
              child: FilledButton.icon(
                icon: const Icon(Icons.play_arrow),
                label: const Text('发起连接（Viewer 侧）'),
                onPressed: _start,
              ),
            )
          : Row(
              children: [
                Expanded(child: _HostPanel(controller: host)),
                const VerticalDivider(width: 1),
                Expanded(child: RemoteScreen(controller: viewer)),
              ],
            ),
    );
  }
}

/// Host 侧面板：等待连接时显示 [ConsentDialog]，其余阶段显示状态文案。
class _HostPanel extends StatelessWidget {
  const _HostPanel({required this.controller});

  final ConnectionController controller;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: controller,
      builder: (ctx, _) {
        final indicator = controller.indicator;
        return Column(
          children: [
            SecurityBanner(snapshot: indicator),
            Expanded(
              child: controller.phase == ConnectionPhase.awaitingConsent
                  ? ConsentDialog(
                      peerName: indicator.peerName ?? '未知设备',
                      peerFingerprint: indicator.peerFingerprint,
                      onApprove: (scopes) => controller.approve(scopes),
                      onDeny: controller.deny,
                    )
                  : Center(child: Text(_hostStatusText(controller))),
            ),
          ],
        );
      },
    );
  }

  String _hostStatusText(ConnectionController c) {
    if (c.isActive) {
      final scopes = c.grantedScopes.map((s) => s.label).join('、');
      return '已允许连接（$scopes）';
    }
    if (c.phase == ConnectionPhase.denied) return '已拒绝';
    if (c.isClosed) {
      final reason = c.indicator.closedReason;
      return '已关闭${reason == null ? '' : '（${reason.name}）'}';
    }
    return '等待连接请求…';
  }
}
