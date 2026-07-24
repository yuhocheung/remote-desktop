import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/models/media_frame.dart';

/// [RdAudioFrame] 模型单测：Raw PCM / Opus 语义、合法性、RMS 电平、相等性。
void main() {
  group('RdAudioFrame 模型', () {
    test('Raw PCM 帧 isRaw/isValid/pcmSampleCount', () {
      final f = RdAudioFrame(
        codec: 0,
        channels: 2,
        sampleRate: 48000,
        data: Uint8List(1920), // 480 采样 × 2ch × 2B
      );
      expect(f.isRaw, isTrue);
      expect(f.isValid, isTrue);
      expect(f.pcmSampleCount, 480);
    });

    test('非 2 字节倍数的 Raw 数据 isValid=false', () {
      final f = RdAudioFrame(
          codec: 0, channels: 2, sampleRate: 48000, data: Uint8List(5));
      expect(f.isValid, isFalse);
    });

    test('Opus 帧 isRaw=false，非空即合法且电平为 0', () {
      final f = RdAudioFrame(
        codec: 1,
        channels: 1,
        sampleRate: 48000,
        data: Uint8List.fromList([1, 2, 3]),
      );
      expect(f.isRaw, isFalse);
      expect(f.isValid, isTrue);
      expect(f.pcmSampleCount, 0);
      expect(f.rmsLevel(), 0.0);
    });

    test('Raw 静音帧 rmsLevel=0', () {
      final f = RdAudioFrame(
          codec: 0, channels: 1, sampleRate: 8000, data: Uint8List(100));
      expect(f.rmsLevel(), 0.0);
    });

    test('Raw 已知单采样 rmsLevel=0.25（16384/32768 平方）', () {
      final f = RdAudioFrame(
        codec: 0,
        channels: 1,
        sampleRate: 8000,
        // 小端 16-bit：0x4000 = 16384
        data: Uint8List.fromList([0x00, 0x40]),
      );
      expect(f.pcmSampleCount, 1);
      expect(f.rmsLevel(), closeTo(0.25, 1e-9));
    });

    test('相等性按字段逐字节比较', () {
      final a = RdAudioFrame(
          codec: 0,
          channels: 2,
          sampleRate: 48000,
          data: Uint8List.fromList([1, 2, 3, 4]));
      final b = RdAudioFrame(
          codec: 0,
          channels: 2,
          sampleRate: 48000,
          data: Uint8List.fromList([1, 2, 3, 4]));
      final c = RdAudioFrame(
          codec: 0,
          channels: 2,
          sampleRate: 48000,
          data: Uint8List.fromList([1, 2, 3, 5]));
      expect(a, equals(b));
      expect(a, isNot(equals(c)));
    });
  });
}
