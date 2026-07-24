import 'connection_state.dart';

/// 不可伪造安全横幅的实时数据模型，镜像 Rust 端 `SecurityIndicator` 的 JSON 形式。
///
/// 该数据全部来自已认证对端（Ed25519 验签通过），Viewer 无法伪造，UI 必须原样渲染。
class SecurityIndicator {
  final String displayName;
  final List<int> deviceId;
  final List<int> fingerprint;
  final String fingerprintSpaced;
  final ConnectionState state;
  final bool encrypted;

  const SecurityIndicator({
    required this.displayName,
    required this.deviceId,
    required this.fingerprint,
    required this.fingerprintSpaced,
    required this.state,
    required this.encrypted,
  });

  factory SecurityIndicator.fromJson(Map<String, dynamic> j) {
    return SecurityIndicator(
      displayName: j['display_name'] as String,
      deviceId: (j['device_id'] as List<dynamic>).cast<int>(),
      fingerprint: (j['fingerprint'] as List<dynamic>).cast<int>(),
      fingerprintSpaced: j['fingerprint_spaced'] as String,
      state: ConnectionState.fromJson(j['state']),
      encrypted: j['encrypted'] as bool,
    );
  }
}
