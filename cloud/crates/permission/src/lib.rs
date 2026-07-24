//! permission — 权限服务（设备可被授予的访问范围：view/input/clipboard/fileTransfer）。
//!
//! 缺口 L 的云端控制面之一。本 crate 为**纯库**：真实领域逻辑 + 内存策略存储，
//! 由网关（gateway）暴露为 HTTP 端点。
//!
//! 设计要点：
//! - 权限范围直接复用 [`auth::Scope`]（`View`/`Input`/`Clipboard`/`FileTransfer`，
//!   PascalCase 序列化名），与 Flutter 端 `ConsentScope` 及 auth 令牌 `claims.scopes`
//!   严格对齐——这是整个系统避免"范围名漂移"的关键约束。
//! - 存储为 `device_id -> 授予范围集合`；`grant` 为幂等并集，`revoke` 整体撤销。
//! - 不记录任何屏幕/键击/剪贴板内容，只记录"谁拥有什么能力"。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// 再导出 auth 的权限范围定义，作为系统唯一真源。
pub use auth::Scope;

/// 权限服务错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionError {
    /// 操作的目标设备没有任何权限记录（多半是尚未授权）。
    DeviceNotFound,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionError::DeviceNotFound => f.write_str("设备未找到/尚未授权"),
        }
    }
}

impl std::error::Error for PermissionError {}

/// 权限服务：内存策略存储，线程安全（`Arc<PermissionService>` 即可共享）。
pub struct PermissionService {
    /// device_id -> 已授予的范围集合。
    grants: Mutex<HashMap<String, HashSet<Scope>>>,
}

impl Default for PermissionService {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionService {
    pub fn new() -> Self {
        Self {
            grants: Mutex::new(HashMap::new()),
        }
    }

    /// 为设备授予范围（幂等并集：重复 grant 不会丢已有范围）。
    pub fn grant(&self, device_id: &str, scopes: &[Scope]) {
        let mut g = self.grants.lock().unwrap();
        let entry = g.entry(device_id.to_string()).or_default();
        for s in scopes {
            entry.insert(*s);
        }
    }

    /// 撤销设备的全部授权（删除记录）。
    pub fn revoke(&self, device_id: &str) {
        self.grants.lock().unwrap().remove(device_id);
    }

    /// 撤销设备的某个具体范围（其余保留）。
    pub fn revoke_scope(&self, device_id: &str, scope: Scope) {
        // 先判断是否需要清除整条记录；释放锁后再做 remove，避免持锁重入。
        let empty = {
            let mut g = self.grants.lock().unwrap();
            match g.get_mut(device_id) {
                Some(set) => {
                    set.remove(&scope);
                    set.is_empty()
                }
                None => false,
            }
        };
        if empty {
            self.grants.lock().unwrap().remove(device_id);
        }
    }

    /// 检查设备是否拥有某范围。
    pub fn check(&self, device_id: &str, scope: Scope) -> bool {
        self.grants
            .lock()
            .unwrap()
            .get(device_id)
            .map(|s| s.contains(&scope))
            .unwrap_or(false)
    }

    /// 列出设备当前所有已授予范围（无序集合，调用方如需稳定顺序自行排序）。
    pub fn list(&self, device_id: &str) -> Vec<Scope> {
        self.grants
            .lock()
            .unwrap()
            .get(device_id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// 列出所有已授权设备标识（审计/管理用）。
    pub fn devices(&self) -> Vec<String> {
        self.grants.lock().unwrap().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_then_check_true_and_unknown_false() {
        let svc = PermissionService::new();
        svc.grant("dev-1", &[Scope::View, Scope::Input]);
        assert!(svc.check("dev-1", Scope::View));
        assert!(svc.check("dev-1", Scope::Input));
        assert!(!svc.check("dev-1", Scope::Clipboard));
        // 未授权设备默认拒绝。
        assert!(!svc.check("dev-2", Scope::View));
    }

    #[test]
    fn grant_is_idempotent_union() {
        let svc = PermissionService::new();
        svc.grant("dev-1", &[Scope::View]);
        svc.grant("dev-1", &[Scope::Input]);
        svc.grant("dev-1", &[Scope::View]); // 重复不丢
        let set: HashSet<Scope> = svc.list("dev-1").into_iter().collect();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&Scope::View));
        assert!(set.contains(&Scope::Input));
    }

    #[test]
    fn revoke_scope_keeps_others_then_full_revoke_clears() {
        let svc = PermissionService::new();
        svc.grant("dev-1", &[Scope::View, Scope::Input, Scope::Clipboard]);
        svc.revoke_scope("dev-1", Scope::Input);
        assert!(svc.check("dev-1", Scope::View));
        assert!(!svc.check("dev-1", Scope::Input));
        assert!(svc.check("dev-1", Scope::Clipboard));

        svc.revoke("dev-1");
        assert!(!svc.check("dev-1", Scope::View));
        assert!(svc.list("dev-1").is_empty());
    }

    #[test]
    fn list_and_devices_reflect_grants() {
        let svc = PermissionService::new();
        svc.grant("a", &[Scope::View]);
        svc.grant("b", &[Scope::Input, Scope::Clipboard]);
        let mut devs = svc.devices();
        devs.sort();
        assert_eq!(devs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(svc.list("b").len(), 2);
    }

    #[test]
    fn scope_serializes_pascal_case() {
        // 与 auth / Flutter 对齐的范围名。
        let j = serde_json::to_string(&Scope::FileTransfer).unwrap();
        assert_eq!(j, "\"FileTransfer\"");
        let j2 = serde_json::to_string(&Scope::View).unwrap();
        assert_eq!(j2, "\"View\"");
    }
}
