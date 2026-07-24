import 'package:flutter/material.dart';

import '../models/consent_scope.dart';

/// Host 侧的「连接请求」同意弹窗。
///
/// 展示对端设备名与不可伪造指纹（来自已认证对端），列出可授予的权限范围，
/// 用户勾选后点「允许」回调 [onApprove]，点「拒绝」回调 [onDeny]。
class ConsentDialog extends StatefulWidget {
  const ConsentDialog({
    super.key,
    required this.peerName,
    this.peerFingerprint,
    required this.onApprove,
    required this.onDeny,
  });

  final String peerName;
  final String? peerFingerprint;
  final void Function(Set<ConsentScope> scopes) onApprove;
  final VoidCallback onDeny;

  @override
  State<ConsentDialog> createState() => _ConsentDialogState();
}

class _ConsentDialogState extends State<ConsentDialog> {
  // 默认只授予「观看屏幕」+「控制输入」，剪贴板/文件传输需用户显式勾选。
  final Set<ConsentScope> _scopes = {ConsentScope.view, ConsentScope.input};

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      margin: const EdgeInsets.all(12),
      child: Padding(
        padding: const EdgeInsets.all(16),
        // 内容在窄屏 / 小窗可能超出可用高度，包一层滚动避免溢出（也便于测试里
        // `ensureVisible` 把「允许」按钮滚入可视区）。真实设备小窗同样受益。
        child: SingleChildScrollView(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
            Row(
              children: [
                const Icon(Icons.security, color: Colors.orange),
                const SizedBox(width: 8),
                Expanded(
                  child: Text('有连接请求',
                      style: theme.textTheme.titleMedium),
                ),
              ],
            ),
            const SizedBox(height: 8),
            Text('对端设备：${widget.peerName}'),
            if (widget.peerFingerprint != null)
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Text('指纹：${widget.peerFingerprint}',
                    style: const TextStyle(
                        fontFamily: 'monospace', fontSize: 12)),
              ),
            const SizedBox(height: 12),
            const Text('授予权限：'),
            ...ConsentScope.values.map(
              (s) => CheckboxListTile(
                title: Text(s.label),
                value: _scopes.contains(s),
                onChanged: (v) => setState(() {
                  if (v == true) {
                    _scopes.add(s);
                  } else {
                    _scopes.remove(s);
                  }
                }),
                controlAffinity: ListTileControlAffinity.leading,
                dense: true,
              ),
            ),
            const SizedBox(height: 8),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                TextButton(
                  onPressed: widget.onDeny,
                  child: const Text('拒绝'),
                ),
                const SizedBox(width: 8),
                FilledButton(
                  onPressed: _scopes.isEmpty
                      ? null
                      : () => widget.onApprove(Set.from(_scopes)),
                  child: const Text('允许'),
                ),
              ],
            ),
          ],
        ),
        ),
      ),
    );
  }
}
