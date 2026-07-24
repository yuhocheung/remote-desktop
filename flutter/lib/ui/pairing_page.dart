// Host：生成配对邀请并展示配对码（点击整卡即可复制，供 Viewer 输码 / 扫码），
//       支持「刷新二维码」（重新发布，旧码即失效）与「取消配对」（撤销发布）。
// Viewer：粘贴配对码 → 解析出 session_id + token → 回调上层带 token 连信令。
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:mobile_scanner/mobile_scanner.dart';
import 'package:qr_flutter/qr_flutter.dart';

import '../connection/pairing.dart';
import '../ffi/rdcore_bindings.dart';

/// 配对页：Host 展示配对码 / Viewer 输入配对码。
class PairingPage extends StatefulWidget {
  const PairingPage({super.key, required this.isHost, this.onPaired});

  /// true=Host（生成并展示配对码）；false=Viewer（输入配对码）。
  final bool isHost;

  /// Viewer 成功解析配对码后回调（携带配对邀请）。
  final void Function(PairingInvite invite)? onPaired;

  @override
  State<PairingPage> createState() => _PairingPageState();
}

class _PairingPageState extends State<PairingPage> {
  final _codeCtrl = TextEditingController();
  PairingInvite? _invite;
  String? _error;

  /// 当前展示的配对是否已发布到共享 token 库（受控端）。
  bool _published = false;

  /// 用户已选择「作为被控端监听」：页面退出后配对由 Host 会话接管，
  /// dispose 时不得撤销（会话结束才撤销，见 ConnectionManager.removeSession）。
  bool _listening = false;

  @override
  void initState() {
    super.initState();
    if (widget.isHost) _generate();
  }

  void _generate() {
    try {
      final client = PairingClient(RdCoreLib());
      final invite = client.createInvite();
      // 生成即发布（刷新二维码 = 重新发布，旧配对随即失效）；
      // 发布失败（如远程信令部署无此文件语义）不阻塞展示，仅提示。
      String? warn;
      try {
        client.publishInvite(invite);
        _published = true;
      } on Object catch (e) {
        _published = false;
        warn = '配对发布失败（远程信令部署可忽略）：$e';
      }
      setState(() {
        _invite = invite;
        _error = warn;
      });
    } on Object catch (e) {
      setState(() => _error = '生成配对码失败：$e');
    }
  }

  /// 主动取消配对：撤销发布（配对码立即失效）并退出本页。
  void _cancelPairing() {
    if (_published) {
      PairingClient(RdCoreLib()).revokeInvite();
      _published = false;
    }
    Navigator.of(context).pop();
  }

  void _submit() {
    final invite = PairingInvite.parse(_codeCtrl.text);
    if (invite == null) {
      setState(() => _error = '配对码格式不正确（应为 <32hex>:<64hex>）');
      return;
    }
    setState(() => _error = null);
    widget.onPaired?.call(invite);
  }

  /// 复制完整配对码（`<32hex>:<64hex>`）并提示。供整卡点击与「复制」按钮复用。
  void _copyCode(String code) {
    Clipboard.setData(ClipboardData(text: code));
    ScaffoldMessenger.of(context).showSnackBar(
      const SnackBar(
        content: Text('配对码已复制'),
        duration: Duration(seconds: 2),
      ),
    );
  }

  Future<void> _scan() async {
    final code = await Navigator.of(context).push<String>(
      MaterialPageRoute(builder: (_) => const _ScanPage()),
    );
    if (code == null) return;
    final invite = PairingInvite.parse(code);
    if (invite == null) {
      setState(() => _error = '扫到的二维码不是有效配对码');
      return;
    }
    setState(() => _error = null);
    widget.onPaired?.call(invite);
  }

  @override
  void dispose() {
    // 受控端：页面被返回键/手势退出（未进入监听）时撤销已发布的配对，
    // 避免「人走了配对码还活着」。已进入监听的由会话结束路径撤销。
    if (widget.isHost && _published && !_listening) {
      PairingClient(RdCoreLib()).revokeInvite();
      _published = false;
    }
    _codeCtrl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: Text(widget.isHost ? '配对 · 被控端' : '配对 · 控制端')),
      // 内容可能较高（二维码 + 配对码 + 按钮），用可滚动容器避免小屏溢出导致“显示不全”。
      body: SingleChildScrollView(
        padding: const EdgeInsets.all(16),
        child: widget.isHost ? _buildHost() : _buildViewer(),
      ),
    );
  }

  Widget _buildHost() {
    final invite = _invite;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(
          '让控制端扫码或输入以下配对码（本机在线期间有效，可重复扫码；退出或取消后失效）：',
          style: Theme.of(context).textTheme.bodyMedium,
        ),
        const SizedBox(height: 16),
        if (invite != null) ...[
          Center(
            child: QrImageView(
              data: invite.code,
              size: 200,
              backgroundColor: Colors.white,
              padding: const EdgeInsets.all(12),
            ),
          ),
          const SizedBox(height: 16),
          _buildInviteCard(invite),
          const SizedBox(height: 12),
          Wrap(
            spacing: 8,
            runSpacing: 8,
            children: [
              FilledButton.tonalIcon(
                icon: const Icon(Icons.copy, size: 16),
                label: const Text('复制'),
                onPressed: () => _copyCode(invite.code),
              ),
              TextButton.icon(
                icon: const Icon(Icons.refresh, size: 16),
                label: const Text('刷新二维码'),
                onPressed: _generate,
              ),
              TextButton.icon(
                icon: const Icon(Icons.link_off, size: 16),
                label: const Text('取消配对'),
                onPressed: _cancelPairing,
              ),
              FilledButton.icon(
                icon: const Icon(Icons.login, size: 16),
                label: const Text('作为被控端监听'),
                onPressed: () {
                  // 进入监听后配对由 Host 会话接管；dispose 不再撤销。
                  _listening = true;
                  widget.onPaired?.call(invite);
                },
              ),
            ],
          ),
        ],
        if (_error != null)
          Padding(
            padding: const EdgeInsets.only(top: 12),
            child: Text(_error!, style: TextStyle(color: Colors.red[700])),
          ),
      ],
    );
  }

  /// 配对码卡片：点击整卡复制完整配对码。
  /// 将 32 位会话 ID 与 64 位令牌分行展示，避免一长串被截断/难辨认。
  Widget _buildInviteCard(PairingInvite invite) {
    final cs = Theme.of(context).colorScheme;
    return Material(
      color: cs.surfaceContainerHighest,
      borderRadius: BorderRadius.circular(14),
      child: InkWell(
        borderRadius: BorderRadius.circular(14),
        onTap: () => _copyCode(invite.code),
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Icon(Icons.vpn_key_outlined, size: 18, color: cs.primary),
                  const SizedBox(width: 8),
                  Text(
                    '配对码',
                    style: TextStyle(
                      fontSize: 14,
                      fontWeight: FontWeight.w600,
                      color: cs.onSurfaceVariant,
                    ),
                  ),
                  const Spacer(),
                  Icon(Icons.copy_outlined, size: 18, color: cs.onSurfaceVariant),
                ],
              ),
              const SizedBox(height: 12),
              _codeBlock('会话 ID', invite.sessionHex, cs),
              const SizedBox(height: 10),
              _codeBlock('令牌 Token', invite.token, cs),
              const SizedBox(height: 12),
              Row(
                children: [
                  Icon(Icons.touch_app_outlined, size: 14, color: cs.onSurfaceVariant),
                  const SizedBox(width: 6),
                  Text(
                    '点击任意位置复制完整配对码',
                    style: TextStyle(fontSize: 12, color: cs.onSurfaceVariant),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }

  /// 单段配对码（标签 + 等宽文本），用 JetBrains Mono 等宽对齐、自动换行完整显示。
  Widget _codeBlock(String label, String value, ColorScheme cs) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          label,
          style: TextStyle(fontSize: 12, color: cs.onSurfaceVariant),
        ),
        const SizedBox(height: 4),
        Text(
          value,
          softWrap: true,
          style: const TextStyle(
            fontFamily: 'JetBrainsMono',
            fontSize: 14,
            height: 1.5,
            letterSpacing: 0.5,
          ),
        ),
      ],
    );
  }

  Widget _buildViewer() {
    final cs = Theme.of(context).colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(
          '粘贴被控端展示的配对码：',
          style: Theme.of(context).textTheme.bodyMedium,
        ),
        const SizedBox(height: 12),
        TextField(
          controller: _codeCtrl,
          decoration: const InputDecoration(
            labelText: '配对码（<32hex>:<64hex>）',
            border: OutlineInputBorder(),
            alignLabelWithHint: true,
          ),
          style: const TextStyle(
            fontFamily: 'JetBrainsMono',
            fontSize: 14,
            letterSpacing: 0.5,
          ),
          maxLines: 3,
          cursorColor: cs.primary,
        ),
        const SizedBox(height: 12),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            FilledButton.icon(
              icon: const Icon(Icons.qr_code_scanner),
              label: const Text('扫码连接'),
              onPressed: _scan,
            ),
            FilledButton.tonalIcon(
              icon: const Icon(Icons.link),
              label: const Text('输码连接'),
              onPressed: _submit,
            ),
          ],
        ),
        if (_error != null)
          Padding(
            padding: const EdgeInsets.only(top: 12),
            child: Text(_error!, style: TextStyle(color: Colors.red[700])),
          ),
      ],
    );
  }
}

/// 扫码页：相机取景识别配对二维码，识别到即返回配对码。
class _ScanPage extends StatefulWidget {
  const _ScanPage();

  @override
  State<_ScanPage> createState() => _ScanPageState();
}

class _ScanPageState extends State<_ScanPage> {
  bool _handled = false;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('扫描配对二维码')),
      body: MobileScanner(
        onDetect: (capture) {
          if (_handled) return;
          for (final b in capture.barcodes) {
            final raw = b.rawValue;
            if (raw != null && raw.isNotEmpty) {
              _handled = true;
              Navigator.of(context).pop(raw);
              return;
            }
          }
        },
      ),
    );
  }
}
