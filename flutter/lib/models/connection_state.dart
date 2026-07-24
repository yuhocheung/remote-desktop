import 'consent_scope.dart';

/// 连接生命周期阶段，与 Rust 端 `ConnectionState` 对齐。
enum ConnectionStateKind { awaitingConsent, active, denied, closed }

/// 连接被关闭的原因，与 Rust 端 `ClosedReason` 对齐。
enum ClosedReason { revoked, timeout, disconnected, expired }

/// 连接生命周期状态（镜像 Rust 端 `ConnectionState` 的 JSON 形式）。
///
/// Rust 端 `serde_json` 输出：
/// - `"AwaitingConsent"`
/// - `{"Active":{"scopes":["View","Input"]}}`
/// - `{"Denied":{"reason":"..."}}`
/// - `{"Closed":"Revoked"}` / `"Timeout"` / `"Disconnected"` / `"Expired"`
class ConnectionState {
  final ConnectionStateKind kind;
  final Set<ConsentScope>? scopes;
  final String? deniedReason;
  final ClosedReason? closedReason;

  const ConnectionState.awaiting()
      : kind = ConnectionStateKind.awaitingConsent,
        scopes = null,
        deniedReason = null,
        closedReason = null;

  const ConnectionState.active(this.scopes)
      : kind = ConnectionStateKind.active,
        deniedReason = null,
        closedReason = null;

  const ConnectionState.denied(this.deniedReason)
      : kind = ConnectionStateKind.denied,
        scopes = null,
        closedReason = null;

  const ConnectionState.closed(this.closedReason)
      : kind = ConnectionStateKind.closed,
        scopes = null,
        deniedReason = null;

  /// 解析 Rust 端 `serde_json` 输出的连接状态。
  factory ConnectionState.fromJson(dynamic json) {
    if (json is String) {
      if (json == 'AwaitingConsent') return const ConnectionState.awaiting();
      throw FormatException('未知连接状态: $json');
    }
    if (json is Map<String, dynamic>) {
      if (json.containsKey('Active')) {
        final a = json['Active'] as Map<String, dynamic>;
        final list = (a['scopes'] as List<dynamic>? ?? [])
            .map((e) => ConsentScopeX.fromJson(e as String))
            .toSet();
        return ConnectionState.active(list);
      } else if (json.containsKey('Denied')) {
        final d = json['Denied'] as Map<String, dynamic>;
        return ConnectionState.denied(d['reason'] as String?);
      } else if (json.containsKey('Closed')) {
        return ConnectionState.closed(_closedReason(json['Closed'] as String));
      }
    }
    throw FormatException('无法解析连接状态: $json');
  }

  static ClosedReason _closedReason(String s) {
    switch (s) {
      case 'Revoked':
        return ClosedReason.revoked;
      case 'Timeout':
        return ClosedReason.timeout;
      case 'Disconnected':
        return ClosedReason.disconnected;
      case 'Expired':
        return ClosedReason.expired;
      default:
        throw FormatException('未知关闭原因: $s');
    }
  }

  bool get isActive => kind == ConnectionStateKind.active;
  bool get isClosed => kind == ConnectionStateKind.closed;
  bool get isAwaiting => kind == ConnectionStateKind.awaitingConsent;

  @override
  String toString() => 'ConnectionState($kind)';
}
