// Track B（kimi-k3）文件传输 UI（B6）。
//
// Viewer（发送方）：选文件 → 提议（Offer）→ 等 Host 逐次同意 → 分片发送 → 完成。
// Host（接收方）：收到 Offer → 同意/拒绝 → 收分片重组 → 得完整文件。
// 走 E2E 加密控制通道（FileTransferClient），Host 未同意前 Chunk 一律被原生侧拒收。
import 'dart:typed_data';

import 'package:flutter/material.dart';

import '../connection/file_transfer_client.dart';

/// 文件传输面板（一次传输的进度与操作）。
class FileTransferPanel extends StatefulWidget {
  const FileTransferPanel({super.key, required this.client, required this.isHost});

  final FileTransferClient client;

  /// true=Host（接收/同意）；false=Viewer（发送）。
  final bool isHost;

  @override
  State<FileTransferPanel> createState() => _FileTransferPanelState();
}

class _FileTransferPanelState extends State<FileTransferPanel> {
  static const int _chunkSize = 48 * 1024; // 每片 48KB（≤ MAX_FILE_CHUNK_SIZE）

  int _transferId = 0;
  String _fileName = '';
  Uint8List? _fileBytes;
  int _sentChunks = 0;
  int _totalChunks = 0;
  String _status = '未开始';
  bool _awaitingConsent = false;

  void _pickDemoFile() {
    // 演示：构造一个合成文件（真实产品用 file_picker 选本地文件）。
    final bytes = Uint8List.fromList(
        List<int>.generate(200 * 1024, (i) => (i * 31) % 256));
    setState(() {
      _transferId = DateTime.now().millisecondsSinceEpoch & 0x7fffffff;
      _fileName = 'demo.bin';
      _fileBytes = bytes;
      _totalChunks = (bytes.length + _chunkSize - 1) ~/ _chunkSize;
      _sentChunks = 0;
      _status = '已选择 $_fileName（${bytes.length} 字节，$_totalChunks 片）';
    });
  }

  Future<void> _sendOffer() async {
    final bytes = _fileBytes;
    if (bytes == null) return;
    try {
      // 1) 提议。真实部署把返回字节经控制通道发 Host；此处演示本端流程。
      widget.client.sendOffer(_transferId, _fileName, bytes.length);
      setState(() {
        _awaitingConsent = true;
        _status = '已发送提议，等待 Host 同意…';
      });
    } on Object catch (e) {
      setState(() => _status = '提议失败：$e');
    }
  }

  Future<void> _sendAll() async {
    final bytes = _fileBytes;
    if (bytes == null) return;
    try {
      for (var i = 0; i < _totalChunks; i++) {
        final start = i * _chunkSize;
        final end = (start + _chunkSize).clamp(0, bytes.length);
        widget.client
            .sendChunk(_transferId, i, Uint8List.sublistView(bytes, start, end));
        setState(() {
          _sentChunks = i + 1;
          _status = '发送中 $_sentChunks/$_totalChunks';
        });
        await Future<void>.delayed(const Duration(milliseconds: 1));
      }
      widget.client.sendDone(_transferId, _totalChunks);
      setState(() => _status = '传输完成（$_totalChunks 片）');
    } on Object catch (e) {
      setState(() => _status = '发送失败：$e');
    }
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
            Row(
              children: [
                const Icon(Icons.insert_drive_file, size: 18),
                const SizedBox(width: 6),
                Text(widget.isHost ? '文件传输 · 接收' : '文件传输 · 发送',
                    style: const TextStyle(fontWeight: FontWeight.bold)),
              ],
            ),
            const SizedBox(height: 8),
            if (!widget.isHost) ...[
              Wrap(
                spacing: 8,
                children: [
                  FilledButton.tonal(
                      onPressed: _pickDemoFile, child: const Text('选择文件')),
                  FilledButton(
                      onPressed:
                          _fileBytes != null && !_awaitingConsent ? _sendOffer : null,
                      child: const Text('提议传输')),
                  FilledButton(
                      onPressed: _awaitingConsent ? _sendAll : null,
                      child: const Text('开始发送')),
                ],
              ),
              const SizedBox(height: 8),
              if (_totalChunks > 0)
                LinearProgressIndicator(
                    value: _totalChunks == 0 ? 0 : _sentChunks / _totalChunks),
            ] else
              const Text('等待对端提议…（Host 逐次同意后才开始接收）'),
            const SizedBox(height: 8),
            Text(_status, style: const TextStyle(fontSize: 12)),
          ],
        ),
      ),
    );
  }
}
