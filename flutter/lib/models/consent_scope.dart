/// 可被授予的权限范围，与 Rust 端 `ConsentScope` 对齐。
enum ConsentScope { view, input, clipboard, fileTransfer }

/// 与 Rust 端 `ConsentScope` 的 serde 名称（View/Input/Clipboard/FileTransfer）对齐。
extension ConsentScopeX on ConsentScope {
  static ConsentScope fromJson(String s) {
    switch (s) {
      case 'View':
        return ConsentScope.view;
      case 'Input':
        return ConsentScope.input;
      case 'Clipboard':
        return ConsentScope.clipboard;
      case 'FileTransfer':
        return ConsentScope.fileTransfer;
      default:
        throw FormatException('未知权限范围: $s');
    }
  }

  /// Rust 端 `serde_json` 输出的名称。
  String get jsonName {
    switch (this) {
      case ConsentScope.view:
        return 'View';
      case ConsentScope.input:
        return 'Input';
      case ConsentScope.clipboard:
        return 'Clipboard';
      case ConsentScope.fileTransfer:
        return 'FileTransfer';
    }
  }

  /// 对应 Rust FFI `rdcore_host_decide` 的位掩码（View=1, Input=2, Clipboard=4, File=8）。
  int get bitmask {
    switch (this) {
      case ConsentScope.view:
        return 1;
      case ConsentScope.input:
        return 2;
      case ConsentScope.clipboard:
        return 4;
      case ConsentScope.fileTransfer:
        return 8;
    }
  }

  String get label {
    switch (this) {
      case ConsentScope.view:
        return '观看屏幕';
      case ConsentScope.input:
        return '控制输入';
      case ConsentScope.clipboard:
        return '剪贴板';
      case ConsentScope.fileTransfer:
        return '文件传输';
    }
  }
}
