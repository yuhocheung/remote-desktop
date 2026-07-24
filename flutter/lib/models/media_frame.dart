import 'dart:typed_data';

/// 一帧已解码 / 渲染的远程画面（RGBA，每像素 4 字节）。
///
/// 与 Rust 端 `RdMediaFrame` FFI 结构对应：从原生 [`RdMediaFrame`] 拉回后，
/// 由 Dart 持有自己的 [`rgba`] 字节拷贝（原生内存立即释放，避免泄漏）。
/// UI 直接用 [`rgba`] 经 `Image.memory` 上屏。
class RdMediaFrame {
  const RdMediaFrame({
    required this.width,
    required this.height,
    required this.rgba,
  });

  final int width;
  final int height;

  /// 原始 RGBA 像素，长度应为 `width * height * 4`。
  final Uint8List rgba;

  /// 是否尺寸 / 像素字节数自洽（用于断言 / 调试）。
  bool get isValid => rgba.length == width * height * 4 && width > 0 && height > 0;

  @override
  bool operator ==(Object other) {
    if (other is! RdMediaFrame) return false;
    if (other.width != width || other.height != height) return false;
    if (other.rgba.length != rgba.length) return false;
    for (var i = 0; i < rgba.length; i++) {
      if (other.rgba[i] != rgba[i]) return false;
    }
    return true;
  }

  @override
  int get hashCode => width ^ (height << 16) ^ rgba.length;

  @override
  String toString() => 'RdMediaFrame($width x $height, ${rgba.length} bytes)';
}

/// 一帧已解码 / 渲染的远程音频。
///
/// 与 Rust 端 `RdAudioFrame` FFI 结构对应：从原生 [`RdAudioFrame`] 拉回后，
/// 由 Dart 持有自己的 [`data`] 字节拷贝（原生内存立即释放，避免泄漏）。
///
/// `codec` 取值：
/// - `0` = Raw（16-bit 交错 PCM，小端有符号），`data` 长度 = 采样数 × `channels` × 2；
/// - `1` = Opus（压缩字节，需解压后播放，真实部署由 `real` feature 编解码）。
class RdAudioFrame {
  const RdAudioFrame({
    required this.codec,
    required this.channels,
    required this.sampleRate,
    required this.data,
  });

  /// 编解码器：0 = Raw（16-bit 交错 PCM），1 = Opus。
  final int codec;

  /// 通道数（1 = 单声道，2 = 立体声）。
  final int channels;

  /// 采样率（Hz）。
  final int sampleRate;

  /// 音频字节：`codec == 0` 时为 16-bit 交错 PCM；`codec == 1` 时为 Opus 压缩字节。
  final Uint8List data;

  /// 是否为 Raw PCM（可直接计算电平 / 播放）。
  bool get isRaw => codec == 0;

  /// Raw PCM 的采样数 / 声道（仅 Raw 有意义；非 Raw 或参数非法返回 0）。
  int get pcmSampleCount =>
      (isRaw && channels > 0) ? data.length ~/ (2 * channels) : 0;

  /// 是否尺寸 / 参数自洽（用于断言 / 调试）。
  bool get isValid {
    if (channels <= 0 || sampleRate <= 0) return false;
    if (isRaw) return data.length % (2 * channels) == 0;
    return data.isNotEmpty;
  }

  /// 计算 Raw PCM 帧的 RMS 电平（0..1），用于音量指示条。
  /// 非 Raw 或空帧返回 0。
  double rmsLevel() {
    if (!isRaw || pcmSampleCount == 0) return 0.0;
    var sumSq = 0.0;
    final n = pcmSampleCount * channels;
    for (var i = 0; i < n; i++) {
      final lo = data[i * 2];
      final hi = data[i * 2 + 1];
      final s = (hi << 8 | lo).toSigned(16); // 16-bit 小端有符号
      sumSq += s * s;
    }
    final rms = sumSq / n;
    return (rms / (32768.0 * 32768.0)).clamp(0.0, 1.0);
  }

  @override
  bool operator ==(Object other) {
    if (other is! RdAudioFrame) return false;
    if (other.codec != codec ||
        other.channels != channels ||
        other.sampleRate != sampleRate) {
      return false;
    }
    if (other.data.length != data.length) return false;
    for (var i = 0; i < data.length; i++) {
      if (other.data[i] != data[i]) return false;
    }
    return true;
  }

  @override
  int get hashCode => codec ^ (channels << 8) ^ (sampleRate << 16) ^ data.length;

  @override
  String toString() =>
      'RdAudioFrame(codec=$codec, ${channels}ch, ${sampleRate}Hz, ${data.length} bytes)';
}
