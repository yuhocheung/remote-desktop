import 'package:flutter/material.dart';

import '../app/connection_manager.dart';
import '../connection/connection_controller.dart';
import '../connection/pairing.dart';
import 'pairing_page.dart';
import 'remote_screen.dart';
import 'settings_screen.dart';

/// App 主壳：会话列表 + 多会话管理（归 Track A 的 app 主壳，H 生产 UI）。
///
/// 提供：
/// - 会话卡片列表（对端名 / 角色 / 阶段芯片 / 进入 / 删除）；
/// - 居中「新建连接」主按钮 → 复用 B3 的 `PairingPage` 输码 → `connectViaPairing` 真实握手；
/// - AppBar 设置入口。
class HomeScreen extends StatelessWidget {
  const HomeScreen({super.key, required this.manager});

  final ConnectionManager manager;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: manager,
      builder: (context, _) {
        final sessions = manager.sessions;
        return Scaffold(
          appBar: AppBar(
            title: const Text('远程桌面'),
            actions: [
              IconButton(
                icon: const Icon(Icons.settings),
                tooltip: '设置',
                onPressed: () {
                  Navigator.of(context).push(
                    MaterialPageRoute(
                      builder: (_) => SettingsScreen(manager: manager),
                    ),
                  );
                },
              ),
            ],
          ),
          body: sessions.isEmpty
              ? _emptyState(context)
              : Column(
                  children: [
                    Padding(
                      padding: const EdgeInsets.symmetric(vertical: 12),
                      child: _newConnectionButton(context),
                    ),
                    Expanded(
                      child: ListView.separated(
                        itemCount: sessions.length,
                        separatorBuilder: (_, __) => const Divider(height: 1),
                        itemBuilder: (ctx, i) => _sessionTile(ctx, sessions[i]),
                      ),
                    ),
                  ],
                ),
        );
      },
    );
  }

  Widget _emptyState(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Icon(Icons.devices, size: 64, color: Colors.grey),
            const SizedBox(height: 16),
            const Text('还没有连接', style: TextStyle(fontSize: 16)),
            const SizedBox(height: 8),
            const Text(
              '点击下方「新建连接」用配对码连接。',
              textAlign: TextAlign.center,
              style: TextStyle(color: Colors.grey),
            ),
            const SizedBox(height: 24),
            FilledButton.icon(
              icon: const Icon(Icons.add),
              label: const Text('新建连接'),
              onPressed: () => _startPairing(context),
            ),
          ],
        ),
      ),
    );
  }

  /// 居中的主操作区：仅控制端「新建连接」。
  /// Flutter 端在所有平台均为纯 Viewer；受控端由 `rdcore-desktop` Host Agent
  /// 承担（2026-07-24 决议，见 docs/双轨对齐 §3.2），「作为被控端」入口已移除。
  Widget _newConnectionButton(BuildContext context) {
    return Center(
      child: Wrap(
        spacing: 12,
        runSpacing: 8,
        alignment: WrapAlignment.center,
        children: [
          FilledButton.icon(
            icon: const Icon(Icons.add),
            label: const Text('新建连接'),
            onPressed: () => _startPairing(context),
          ),
        ],
      ),
    );
  }

  Widget _sessionTile(BuildContext context, SessionEntry entry) {
    final c = entry.controller;
    final phaseLabel = _phaseLabel(c);
    final color = _phaseColor(c);
    return ListTile(
      leading: Icon(c.isHost ? Icons.computer : Icons.phone_android),
      title: Text(entry.peerName ?? (c.isHost ? '被控端' : '控制端')),
      subtitle: Text('${c.isHost ? "被控" : "控制"} · $phaseLabel'),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          _statusPill(c, color, phaseLabel),
          IconButton(
            icon: const Icon(Icons.delete_outline),
            tooltip: '删除',
            onPressed: () => manager.removeSession(entry.id),
          ),
        ],
      ),
      onTap: () {
        // 已关闭（或失败）的真实配对会话：点击直接重连，而非仅打开「已关闭」卡片。
        if ((c.isClosed || c.phase == ConnectionPhase.failed) && entry.invite != null) {
          _reconnectSession(context, entry);
        } else {
          _openSession(context, entry.id);
        }
      },
    );
  }

  /// 已关闭 / 失败的真实配对会话：复用原邀请重新建连，并导航到新会话。
  Future<void> _reconnectSession(BuildContext context, SessionEntry entry) async {
    final scaffold = ScaffoldMessenger.of(context);
    try {
      final newId = await manager.reconnect(entry.id);
      if (!context.mounted) return;
      _openSession(context, newId);
    } catch (e) {
      if (!context.mounted) return;
      scaffold.showSnackBar(
        SnackBar(content: Text('重连失败：$e'), duration: const Duration(seconds: 6)),
      );
    }
  }

  /// 无边框状态胶囊：同色系圆点 + 同色文字 + 同色淡背景，状态一目了然。
  /// 失败时包 Tooltip 透出具体错误。
  Widget _statusPill(ConnectionController c, Color color, String label) {
    final pill = Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 5),
      decoration: BoxDecoration(
        color: color.withValues(alpha: 0.14),
        borderRadius: BorderRadius.circular(999),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: 7,
            height: 7,
            decoration: BoxDecoration(color: color, shape: BoxShape.circle),
          ),
          const SizedBox(width: 6),
          Text(
            label,
            style: TextStyle(
              color: color,
              fontSize: 12,
              fontWeight: FontWeight.w600,
            ),
          ),
        ],
      ),
    );
    if (c.error != null) return Tooltip(message: c.error!, child: pill);
    return pill;
  }

  void _openSession(BuildContext context, String id) {
    final entry = manager.sessionById(id);
    if (entry == null) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('连接已创建，但会话未找到')),
      );
      return;
    }
    Navigator.of(context).push(
      MaterialPageRoute(
        builder: (_) => RemoteScreen(
          manager: manager,
          sessionId: entry.id,
          controller: entry.controller,
        ),
      ),
    );
  }

  Future<void> _startPairing(BuildContext context) async {
    final invite = await Navigator.of(context).push<PairingInvite>(
      MaterialPageRoute(
        builder: (_) => PairingPage(
          isHost: false,
          onPaired: (invite) => Navigator.of(context).pop(invite),
        ),
      ),
    );
    if (invite == null) return;
    if (!context.mounted) return;
    try {
      final id = await manager.connectViaPairing(invite);
      if (!context.mounted) return;
      _openSession(context, id);
    } catch (e) {
      if (!context.mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('连接失败：$e'), duration: const Duration(seconds: 6)),
      );
    }
  }

  String _phaseLabel(ConnectionController c) {
    switch (c.phase) {
      case ConnectionPhase.setup:
        return '未开始';
      case ConnectionPhase.connecting:
        return '连接中';
      case ConnectionPhase.awaitingConsent:
        return '等待确认';
      case ConnectionPhase.active:
        return '已连接';
      case ConnectionPhase.denied:
        return '已拒绝';
      case ConnectionPhase.closed:
        return '已关闭';
      case ConnectionPhase.failed:
        return '连接失败';
    }
  }

  Color _phaseColor(ConnectionController c) {
    switch (c.phase) {
      case ConnectionPhase.active:
        return Colors.green;
      case ConnectionPhase.connecting:
      case ConnectionPhase.awaitingConsent:
        return Colors.orange;
      case ConnectionPhase.denied:
        return Colors.red;
      case ConnectionPhase.closed:
        return Colors.grey;
      case ConnectionPhase.setup:
        return Colors.blue;
      case ConnectionPhase.failed:
        return Colors.red;
    }
  }
}
