//! audit — 审计服务（仅记录元数据：谁、何时、做了什么）。
//!
//! 缺口 L 的云端控制面之一。本 crate 为**纯库**：真实领域逻辑 + 内存有界环形日志，
//! 由网关（gateway）暴露为 HTTP 端点。
//!
//! **强约束**：审计日志只存元数据（事件类型、主体、动作摘要、时间戳、连接/session id）。
//! 绝不记录屏幕像素、键击内容、剪贴板正文、文件传输内容——这些属于媒体/控制面，
//! 不在云端控制面可见范围内（见架构文档"三独立通道"）。这条约束由代码注释与单元测试
//! 双重保证：`AuditEvent` 的字段类型里根本没有 `Vec<u8>`/`String content` 这类可承载
//! 明文内容的字段。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 可被审计的事件类型（全部为"控制面元数据"，与媒体/输入内容无关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AuditKind {
    /// 设备登录（认证成功）。
    Login,
    /// 建立远程连接。
    Connect,
    /// 断开远程连接。
    Disconnect,
    /// 文件传输发起/完成（仅记录动作，不记录内容）。
    Transfer,
    /// 权限变更（授权/撤销）。
    PermissionChange,
    /// 设备注册/注销。
    DeviceRegister,
}

/// 单条审计事件。**只有元数据字段**，无任何可承载屏幕/键击/剪贴板正文的字段。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEvent {
    /// 单调递增的事件 id（便于分页/去重）。
    pub id: u64,
    /// 事件类型。
    pub kind: AuditKind,
    /// 主体：设备/用户标识。
    pub subject: String,
    /// 动作摘要（短文本，例如 "grant View,Input to dev-2"）。不含机密内容。
    pub action: String,
    /// Unix 秒级时间戳。
    pub ts: u64,
}

/// 审计查询过滤条件（全部可选）。
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub kind: Option<AuditKind>,
    pub subject: Option<String>,
}

/// 审计服务：内存有界环形日志（超过容量丢弃最旧条目）。
pub struct AuditService {
    log: Mutex<VecDeque<AuditEvent>>,
    next_id: Mutex<u64>,
    cap: usize,
}

impl Default for AuditService {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl AuditService {
    /// `cap` 为环形日志最大容量；超过则丢弃最旧事件。
    pub fn new(cap: usize) -> Self {
        Self {
            log: Mutex::new(VecDeque::with_capacity(cap.min(1))),
            next_id: Mutex::new(1),
            cap: cap.max(1),
        }
    }

    /// 记录一条事件，返回其分配到的 id。
    pub fn record(&self, kind: AuditKind, subject: &str, action: &str) -> u64 {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let v = *n;
            *n += 1;
            v
        };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let event = AuditEvent {
            id,
            kind,
            subject: subject.to_string(),
            action: action.to_string(),
            ts,
        };
        let mut log = self.log.lock().unwrap();
        log.push_back(event);
        while log.len() > self.cap {
            log.pop_front();
        }
        id
    }

    /// 按过滤条件查询（无过滤则全量）。返回时间升序。
    pub fn query(&self, q: &AuditQuery) -> Vec<AuditEvent> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|e| match q.kind {
                Some(k) => e.kind == k,
                None => true,
            })
            .filter(|e| match &q.subject {
                Some(s) => &e.subject == s,
                None => true,
            })
            .cloned()
            .collect()
    }

    /// 最近 `n` 条（时间升序，最多 n 条）。
    pub fn recent(&self, n: usize) -> Vec<AuditEvent> {
        let log = self.log.lock().unwrap();
        let start = log.len().saturating_sub(n);
        log.iter().skip(start).cloned().collect()
    }

    /// 当前日志条数（受容量上限约束）。
    pub fn count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// 清空日志（测试/合规用）。
    pub fn clear(&self) {
        self.log.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_assigns_monotonic_ids_and_timestamp() {
        let svc = AuditService::new(100);
        let id1 = svc.record(AuditKind::Login, "dev-1", "login ok");
        let id2 = svc.record(AuditKind::Connect, "dev-1", "connect");
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        let events = svc.recent(10);
        assert_eq!(events.len(), 2);
        assert!(events[1].ts >= events[0].ts);
        assert_eq!(events[0].kind, AuditKind::Login);
    }

    #[test]
    fn query_filters_by_kind_and_subject() {
        let svc = AuditService::new(100);
        svc.record(AuditKind::Login, "dev-1", "login");
        svc.record(AuditKind::Connect, "dev-1", "connect");
        svc.record(AuditKind::Login, "dev-2", "login");

        let only_logins = svc.query(&AuditQuery {
            kind: Some(AuditKind::Login),
            subject: None,
        });
        assert_eq!(only_logins.len(), 2);

        let dev1_logins = svc.query(&AuditQuery {
            kind: Some(AuditKind::Login),
            subject: Some("dev-1".to_string()),
        });
        assert_eq!(dev1_logins.len(), 1);
        assert_eq!(dev1_logins[0].subject, "dev-1");

        let dev2_any = svc.query(&AuditQuery {
            kind: None,
            subject: Some("dev-2".to_string()),
        });
        assert_eq!(dev2_any.len(), 1);
    }

    #[test]
    fn ring_buffer_drops_oldest_when_over_cap() {
        let svc = AuditService::new(3);
        svc.record(AuditKind::Login, "a", "1");
        svc.record(AuditKind::Login, "a", "2");
        svc.record(AuditKind::Login, "a", "3");
        assert_eq!(svc.count(), 3);
        svc.record(AuditKind::Login, "a", "4"); // 触发丢弃最旧
        assert_eq!(svc.count(), 3);
        let events = svc.recent(10);
        // 最早那条（id=1）应已被挤出。
        assert!(!events.iter().any(|e| e.action == "1"));
        assert!(events.iter().any(|e| e.action == "4"));
    }

    #[test]
    fn audit_event_has_no_content_fields() {
        // 元数据的强约束：事件结构里没有可承载正文内容的字段。
        // 这里通过序列化往返验证字段集，确保没有意外加入 content/body 之类的字段。
        let e = AuditEvent {
            id: 7,
            kind: AuditKind::Transfer,
            subject: "dev-1".to_string(),
            action: "file push x".to_string(),
            ts: 1_700_000_000,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"action\""));
        assert!(json.contains("\"Transfer\""));
        // 不应出现任何 "content"/"body"/"data" 字段。
        assert!(!json.contains("content"));
        assert!(!json.contains("body"));
    }
}
