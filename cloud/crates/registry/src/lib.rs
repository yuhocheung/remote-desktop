//! registry — 设备注册表（仅元数据）。
//!
//! 缺口 L 的云端控制面之一。本 crate 为**纯库**：真实领域逻辑 + 内存存储，
//! 由网关（gateway）暴露为 HTTP 端点。
//!
//! **强约束**：只保存设备元数据——`id`/`name`/`platform`/`public_key`/`last_seen`。
//! 绝不保存屏幕像素、键击、剪贴板、文件内容。公钥用于后续设备级身份校验/TOFU，
//! 不属于机密 body。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 设备平台（用于展示与路由策略，不影响安全）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum Platform {
    #[default]
    Unknown,
    Windows,
    MacOS,
    Linux,
    IOS,
    Android,
}

/// 单条设备记录（纯元数据）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub platform: Platform,
    /// Ed25519 公钥（hex 或 base64 文本形式，便于 JSON 传输）。
    pub public_key: String,
    /// 最近一次心跳/上线时间（Unix 秒）。
    pub last_seen: u64,
}

/// 设备注册表：内存存储，线程安全（`Arc<RegistryService>` 即可共享）。
pub struct RegistryService {
    devices: Mutex<HashMap<String, DeviceRecord>>,
}

impl Default for RegistryService {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryService {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
        }
    }

    /// 注册/更新设备（幂等 upsert）。返回最终写入的记录（last_seen 刷新为当前时刻）。
    pub fn register(
        &self,
        id: &str,
        name: &str,
        platform: Platform,
        public_key: &str,
    ) -> DeviceRecord {
        let ts = now_secs();
        let rec = DeviceRecord {
            id: id.to_string(),
            name: name.to_string(),
            platform,
            public_key: public_key.to_string(),
            last_seen: ts,
        };
        self.devices
            .lock()
            .unwrap()
            .insert(id.to_string(), rec.clone());
        rec
    }

    /// 取单条记录（克隆，避免锁外持有引用）。
    pub fn get(&self, id: &str) -> Option<DeviceRecord> {
        self.devices.lock().unwrap().get(id).cloned()
    }

    /// 列出全部设备（克隆快照）。
    pub fn list(&self) -> Vec<DeviceRecord> {
        self.devices.lock().unwrap().values().cloned().collect()
    }

    /// 更新最近上线时间，成功返回 true（设备存在）。
    pub fn update_last_seen(&self, id: &str) -> bool {
        let mut d = self.devices.lock().unwrap();
        if let Some(rec) = d.get_mut(id) {
            rec.last_seen = now_secs();
            true
        } else {
            false
        }
    }

    /// 注销设备，成功返回 true（设备存在）。
    pub fn deregister(&self, id: &str) -> bool {
        self.devices.lock().unwrap().remove(id).is_some()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_get_list_and_update() {
        let svc = RegistryService::new();
        let rec = svc.register("dev-1", "Alice PC", Platform::Windows, "deadbeef");
        assert_eq!(rec.id, "dev-1");
        assert_eq!(rec.platform, Platform::Windows);
        assert!(rec.last_seen > 0);

        assert!(svc.get("dev-1").is_some());
        assert!(svc.get("dev-2").is_none());

        svc.register("dev-2", "Bob Mac", Platform::MacOS, "cafe");
        let mut all = svc.list();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].platform, Platform::MacOS);

        // 更新 last_seen 应能成功，且时间戳变大。
        let before = svc.get("dev-1").unwrap().last_seen;
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(svc.update_last_seen("dev-1"));
        let after = svc.get("dev-1").unwrap().last_seen;
        assert!(after >= before);
        assert!(!svc.update_last_seen("dev-99"));
    }

    #[test]
    fn deregister_removes_device() {
        let svc = RegistryService::new();
        svc.register("dev-1", "X", Platform::Linux, "k");
        assert!(svc.deregister("dev-1"));
        assert!(svc.get("dev-1").is_none());
        assert!(!svc.deregister("dev-1")); // 二次删除应返回 false
    }

    #[test]
    fn register_is_idempotent_upsert() {
        let svc = RegistryService::new();
        svc.register("dev-1", "Old", Platform::Windows, "k1");
        svc.register("dev-1", "New", Platform::Linux, "k2");
        let rec = svc.get("dev-1").unwrap();
        assert_eq!(rec.name, "New");
        assert_eq!(rec.platform, Platform::Linux);
        assert_eq!(svc.list().len(), 1);
    }

    #[test]
    fn platform_serializes_pascal_case() {
        let j = serde_json::to_string(&Platform::IOS).unwrap();
        assert_eq!(j, "\"IOS\"");
        let rec = DeviceRecord {
            id: "d".to_string(),
            name: "n".to_string(),
            platform: Platform::Android,
            public_key: "pk".to_string(),
            last_seen: 0,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"Android\""));
    }
}
