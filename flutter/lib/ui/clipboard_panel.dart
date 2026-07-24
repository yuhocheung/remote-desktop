// Track B（kimi-k3）剪贴板 UI（B6）。
//
// 任一端发送 / 接收剪贴板事件（Request/Data/Clear），走 E2E 加密控制通道（ClipboardClient）。
// 剪贴板默认 opt-in（ConsentScope.clipboard）。
import 'dart:typed_data';

import 'package:flutter/material.dart';

import '../connection/clipboard_client.dart';

/// 剪贴板面板：发送本地剪贴板文本 / 显示最近收到的对端剪贴板。
class ClipboardPanel extends StatefulWidget {
  const ClipboardPanel({super.key, required this.client});

  final ClipboardClient client;

  @override
  State<ClipboardPanel> createState() => _ClipboardPanelState();
}

class _ClipboardPanelState extends State<ClipboardPanel> {
  final _textCtrl = TextEditingController();
  int _seq = 0;
  String _status = '';

  Future<void> _sendText() async {
    final text = _textCtrl.text;
    if (text.isEmpty) return;
    try {
      // 演示本端封装（真实部署把返回字节经控制通道发对端）。
      widget.client.send(++_seq, ClipboardOp.data, Uint8List.fromList(text.codeUnits));
      setState(() => _status = '已发送 ${text.length} 字符');
    } on Object catch (e) {
      setState(() => _status = '发送失败：$e');
    }
  }

  Future<void> _requestRemote() async {
    try {
      widget.client.send(++_seq, ClipboardOp.request);
      setState(() => _status = '已请求对端剪贴板');
    } on Object catch (e) {
      setState(() => _status = '请求失败：$e');
    }
  }

  @override
  void dispose() {
    _textCtrl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            const Row(
              children: [
                Icon(Icons.content_paste, size: 18),
                SizedBox(width: 6),
                Text('剪贴板同步', style: TextStyle(fontWeight: FontWeight.bold)),
              ],
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _textCtrl,
              decoration: const InputDecoration(
                labelText: '要发送的文本',
                border: OutlineInputBorder(),
                isDense: true,
              ),
              maxLines: 2,
            ),
            const SizedBox(height: 8),
            Wrap(
              spacing: 8,
              children: [
                FilledButton.tonal(
                    onPressed: _sendText, child: const Text('发送')),
                FilledButton.tonal(
                    onPressed: _requestRemote, child: const Text('请求对端')),
              ],
            ),
            const SizedBox(height: 8),
            if (_status.isNotEmpty)
              Text(_status, style: const TextStyle(fontSize: 12)),
          ],
        ),
      ),
    );
  }
}
