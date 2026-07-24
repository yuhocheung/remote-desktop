/// 本机/对端身份（用于带外配对导入导出），镜像 Rust 端 `PeerIdentity` 的 JSON 形式。
class PeerIdentity {
  final List<int> id;
  final String displayName;
  final List<int> publicKey;
  final List<int> fingerprint;

  const PeerIdentity({
    required this.id,
    required this.displayName,
    required this.publicKey,
    required this.fingerprint,
  });

  factory PeerIdentity.fromJson(Map<String, dynamic> j) {
    return PeerIdentity(
      id: (j['id'] as List<dynamic>).cast<int>(),
      displayName: j['display_name'] as String,
      publicKey: (j['public_key'] as List<dynamic>).cast<int>(),
      fingerprint: (j['fingerprint'] as List<dynamic>).cast<int>(),
    );
  }

  Map<String, dynamic> toJson() => {
        'id': id,
        'display_name': displayName,
        'public_key': publicKey,
        'fingerprint': fingerprint,
      };
}
