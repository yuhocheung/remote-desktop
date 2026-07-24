import 'package:flutter/material.dart';

import '../app/connection_manager.dart';
import '../models/consent_scope.dart';

/// 设置页（归 Track A 的 app 主壳，H 生产 UI）。
///
/// 编辑本机显示名、信令服务器基址、TURN/STUN 中继凭据（对称 NAT / 蜂窝网络必需）、
/// 以及新建连接时默认请求的权限范围；存回 [ConnectionManager.settings]，供配对与演示会话复用。
class SettingsScreen extends StatefulWidget {
  const SettingsScreen({super.key, required this.manager});

  final ConnectionManager manager;

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  late final TextEditingController _nameCtrl;
  late final TextEditingController _urlCtrl;
  late final TextEditingController _stunCtrl;
  late final TextEditingController _turnCtrl;
  late final TextEditingController _turnUserCtrl;
  late final TextEditingController _turnPassCtrl;
  late Set<ConsentScope> _scopes;
  late bool _allowInsecure;

  @override
  void initState() {
    super.initState();
    final s = widget.manager.settings;
    _nameCtrl = TextEditingController(text: s.deviceName);
    _urlCtrl = TextEditingController(text: s.signalingBaseUrl);
    _stunCtrl = TextEditingController(text: s.stunUrl ?? '');
    _turnCtrl = TextEditingController(text: s.turnUrl ?? '');
    _turnUserCtrl = TextEditingController(text: s.turnUser ?? '');
    _turnPassCtrl = TextEditingController(text: s.turnPass ?? '');
    _scopes = Set.of(s.defaultScopes);
    _allowInsecure = s.allowInsecureSignaling;
  }

  @override
  void dispose() {
    _nameCtrl.dispose();
    _urlCtrl.dispose();
    _stunCtrl.dispose();
    _turnCtrl.dispose();
    _turnUserCtrl.dispose();
    _turnPassCtrl.dispose();
    super.dispose();
  }

  void _save() {
    widget.manager.settings = widget.manager.settings.copyWith(
      deviceName: _nameCtrl.text.isEmpty ? '我的设备' : _nameCtrl.text,
      signalingBaseUrl: _urlCtrl.text.trim(),
      defaultScopes: _scopes,
      allowInsecureSignaling: _allowInsecure,
      stunUrl: _stunCtrl.text.trim().isEmpty ? null : _stunCtrl.text.trim(),
      turnUrl: _turnCtrl.text.trim().isEmpty ? null : _turnCtrl.text.trim(),
      turnUser: _turnUserCtrl.text.trim().isEmpty ? null : _turnUserCtrl.text.trim(),
      turnPass: _turnPassCtrl.text.trim().isEmpty ? null : _turnPassCtrl.text.trim(),
    );
    Navigator.of(context).pop();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('设置')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          TextField(
            controller: _nameCtrl,
            decoration: const InputDecoration(
              labelText: '本机显示名称',
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 16),
          TextField(
            controller: _urlCtrl,
            decoration: const InputDecoration(
              labelText: '信令服务器基址',
              hintText: 'ws://host:port',
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 16),
          const Divider(),
          const Text('中继服务器（对称 NAT / 蜂窝网络必需）',
              style: TextStyle(fontWeight: FontWeight.bold)),
          const SizedBox(height: 8),
          TextField(
            controller: _stunCtrl,
            decoration: const InputDecoration(
              labelText: 'STUN 服务器（可选）',
              hintText: 'stun:example.com:3478',
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _turnCtrl,
            decoration: const InputDecoration(
              labelText: 'TURN 中继服务器',
              hintText: 'turn:example.com:3478?transport=udp',
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _turnUserCtrl,
            decoration: const InputDecoration(
              labelText: 'TURN 用户名',
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _turnPassCtrl,
            obscureText: true,
            decoration: const InputDecoration(
              labelText: 'TURN 凭据',
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 8),
          const Text('媒体经 TURN 转发，但仍由端到端密钥加密，中继服务器只见密文。',
              style: TextStyle(fontSize: 12, color: Colors.grey)),
          const SizedBox(height: 16),
          const Divider(),
          const Text('新建连接默认请求的权限',
              style: TextStyle(fontWeight: FontWeight.bold)),
          ...ConsentScope.values.map(
            (s) => CheckboxListTile(
              title: Text(s.label),
              value: _scopes.contains(s),
              onChanged: (v) {
                setState(() {
                  if (v == true) {
                    _scopes.add(s);
                  } else {
                    _scopes.remove(s);
                  }
                });
              },
            ),
          ),
          const SizedBox(height: 16),
          SwitchListTile(
            title: const Text('开发模式：接受自签 TLS 证书'),
            subtitle: const Text('仅用于本地自签 wss:// 信令服务；生产环境请关闭'),
            value: _allowInsecure,
            onChanged: (v) => setState(() => _allowInsecure = v),
          ),
          const SizedBox(height: 16),
          FilledButton.icon(
            icon: const Icon(Icons.save),
            label: const Text('保存'),
            onPressed: _save,
          ),
        ],
      ),
    );
  }
}
