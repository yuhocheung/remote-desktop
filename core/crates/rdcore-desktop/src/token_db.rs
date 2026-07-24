//! A5↔B2 对接点：受控端（Host）把当前配对 session 写入「共享 token 库文件」。
//!
//! 同机部署下，受控端（本 Agent 或经 FFI 发布的 Flutter Host）在生成配对后把 session
//! 写入该文件，并以心跳周期重写保鲜；signaling-svc 在每次握手校验前调用
//! `TokenStore::reload_from_file` 以文件为事实 reconcile 内存库：
//! - 文件中的配对即当前有效配对（不焚毁，可重复扫码建连）；
//! - 主动取消配对 → [`clear_token_file`] 删除文件，下一次握手即失效；
//! - 刷新二维码 → [`register_token_file`] 覆写为新 session/token，旧配对即失效；
//! - 受控端退出/崩溃 → 心跳停更，文件 mtime 超过 signaling-svc 的
//!   `TOKEN_FILE_STALE_AFTER`（3 分钟）后配对自动失效。
//!
//! 文件格式与 `cloud/crates/signaling-svc/src/lib.rs` 的 `reload_from_file` 严格对齐：
//! 每行 `session_hex[\t token_hex]`，`session_hex` 为 32 个十六进制字符（16 字节）。

use std::path::PathBuf;

use rdcore_proto::SessionId;

/// 共享 token 库文件默认路径。
///
/// 生产部署时由环境变量 `SIGNALING_TOKEN_DB` 覆盖，且 Agent 与 signaling-svc 必须指向同一文件。
pub const DEFAULT_TOKEN_DB_PATH: &str = "signaling_token_db.txt";

/// token 库文件的心跳刷新周期（重写文件以更新 mtime）。
/// 必须明显小于 signaling-svc 的 `TOKEN_FILE_STALE_AFTER`（3 分钟），取 30 秒。
pub const TOKEN_FILE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(30);

/// 解析 token 库文件路径：优先环境变量 `SIGNALING_TOKEN_DB`，否则用默认相对路径。
pub fn token_db_path() -> PathBuf {
    std::env::var("SIGNALING_TOKEN_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TOKEN_DB_PATH))
}

/// 把一次配对的 session 写入共享 token 库文件（兼作心跳：重写即刷新 mtime）。
///
/// 本端一次只发布一个活跃配对，故采用「覆盖写入」语义：写入新配对即作废旧配对
/// （信令侧下一次握手 reconcile 时回收）。
///
/// 写入必须是**原子替换**：signaling-svc 在每次握手时 `read_to_string` 本文件，
/// 直接截断重写会让读者撞上「截断后、写入前」的空读窗口（配对条目被误回收、
/// Viewer 握手瞬时 401）。改为同目录临时文件写全 + flush + sync_all 后 rename 覆盖，
/// 读者任意时刻只能看到旧版或新版的**完整**文件。
pub fn register_token_file(session: &SessionId, token: &str) -> std::io::Result<()> {
    let path = token_db_path();
    let line = format!("{}\t{}\n", hex::encode(session.0), token);
    atomic_replace(&path, line.as_bytes())
}

/// 原子替换 `path` 的内容：同目录 `<文件名>.tmp` 完整写入并落盘后 rename 覆盖。
///
/// - 同目录保证同卷，rename 是单文件系统内的原子替换；
///   Windows 上 std rename 即 MoveFileEx(REPLACE_EXISTING) 语义，目标已存在也被覆盖；
/// - Rust std 在 Windows 以 FILE_SHARE_DELETE 打开文件，signaling 侧读句柄存续期间
///   rename 照样成功，读者读完的是被替换前的完整旧版；
/// - 失败时尽力清理临时文件，不留垃圾。
fn atomic_replace(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?; // 落盘后再替换，崩溃也不留半文件
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 主动取消配对：删除共享 token 库文件。
///
/// 信令侧下一次握手 reconcile 时回收对应条目，配对码立即失效。
/// 文件本就不存在时视为成功（幂等）。
pub fn clear_token_file() -> std::io::Result<()> {
    let path = token_db_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_proto::SessionId;

    #[test]
    fn register_writes_session_hex_and_token() {
        let dir = std::env::temp_dir();
        let path = dir.join("rdcore_test_token_db.txt");
        // 确保使用测试专用路径（避免污染真实 SIGNALING_TOKEN_DB）。
        std::env::set_var("SIGNALING_TOKEN_DB", &path);

        let sid = SessionId([0xabu8; 16]);
        let token = "deadbeef".repeat(8); // 64 hex
        register_token_file(&sid, &token).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let line = content.lines().next().unwrap();
        let (got_sid, got_tok) = line.split_once('\t').unwrap();
        assert_eq!(got_sid, &hex::encode(sid.0));
        assert_eq!(got_tok, token);

        // 格式必须与 signaling-svc 的 reload_from_file 对齐：session_hex 为 32 字符。
        assert_eq!(got_sid.len(), 32);

        // 取消配对：文件删除且幂等。
        clear_token_file().unwrap();
        assert!(!path.exists(), "clear_token_file 应删除 token 库文件");
        clear_token_file().unwrap(); // 幂等：不存在也 Ok
        std::env::remove_var("SIGNALING_TOKEN_DB");
    }

    /// 独立测试目录（按进程 + 名字隔离，避免并行测试互踩）。
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdcore_atomic_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_replace_writes_full_content_and_leaves_no_temp() {
        let dir = test_dir("basic");
        let path = dir.join("db.txt");

        let content = b"0123456789abcdef0123456789abcdef\ttok\n";
        atomic_replace(&path, content).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content, "写入-读取应一致");
        assert!(!dir.join("db.txt.tmp").exists(), "临时文件不应残留");

        // 覆盖写（心跳语义）：整体替换为更短内容，无追加、无旧内容残留。
        let shorter = b"aa\tb\n";
        atomic_replace(&path, shorter).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), shorter);
        assert!(!dir.join("db.txt.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_replace_concurrent_reads_never_see_partial() {
        let dir = test_dir("race");
        let path = dir.join("db.txt");
        // 两个合法格式但内容不同的完整行；读到第三种内容即失败。
        let line_a = format!("{}\t{}\n", "a".repeat(32), "1".repeat(64));
        let line_b = format!("{}\t{}\n", "b".repeat(32), "2".repeat(64));
        std::fs::write(&path, &line_a).unwrap();

        let writer_path = path.clone();
        let (la, lb) = (line_a.clone(), line_b.clone());
        let writer = std::thread::spawn(move || {
            for i in 0..300 {
                let content = if i % 2 == 0 { &la } else { &lb };
                atomic_replace(&writer_path, content.as_bytes()).unwrap();
            }
        });

        // 与写并发地持续读：任何时刻都必须是完整旧版或完整新版——
        // 空读 / 半行 / 混合内容即竞争复发（signaling-svc 的 reload_from_file 同款读法）。
        while !writer.is_finished() {
            let content = std::fs::read_to_string(&path).expect("rename 原子替换下读取不应失败");
            assert!(
                content == line_a || content == line_b,
                "读到空读/半行/损坏内容: {content:?}"
            );
        }
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_replace_failure_cleans_up_temp() {
        // 目录不存在 → File::create 必失败；不应留下任何临时文件。
        let dir = std::env::temp_dir().join(format!("rdcore_atomic_fail_{}", std::process::id()));
        let path = dir.join("db.txt");
        assert!(atomic_replace(&path, b"x").is_err());
        assert!(!dir.join("db.txt.tmp").exists(), "失败路径不应残留临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
