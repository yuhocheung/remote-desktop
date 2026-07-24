//! rdcore-consent — 同意门控 + 连接生命周期 + 不可伪造安全指示（P5）。
//!
//! 架构文档 §关键设计决策：
//! - 连接需 Host 用户**同意**（交互模式）或设备预授权 + 临时 PIN（无人值守模式）。
//! - Host 端常驻**不可伪造横幅**，由独立高权限服务 / OS 安全注意序列绘制，Viewer 无法覆盖。
//! - 可随时终止。
//!
//! 本 crate 实现这些策略的纯逻辑与数据模型；横幅的实际绘制（高权限 / OS 层）在 P6。
//! 所有状态都由 P4 验签得到的 [`rdcore_session::VerifiedPeer`] 驱动，因此横幅上展示的
//! 对端身份 / 指纹一定来自已认证的来源，无法被对端伪造。

use rdcore_crypto::Fingerprint;
use rdcore_identity::DeviceId;
use rdcore_session::VerifiedPeer;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// 连接模式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ConsentMode {
    /// 交互模式：每次连接都需 Host 用户显式批准（默认、最安全）。
    Interactive,
    /// 无人值守模式：设备已预授权，凭临时 PIN 自动放行（PIN 由带外发给授权者）。
    Unattended { pin: String },
}

/// 可被授予的权限范围（最小权限原则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConsentScope {
    /// 仅观看屏幕（不控制）。
    View,
    /// 发送鼠标 / 键盘 / 滚轮输入（特权操作）。
    Input,
    /// 剪贴板同步（应逐次再确认，见 [`ConsentGate::request_clipboard`]）。
    Clipboard,
    /// 文件传输。
    FileTransfer,
}

/// Host 对一次连接请求的决定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsentDecision {
    /// 授予：给定范围 + 可选有效期（到时自动收回）。
    Grant {
        scopes: HashSet<ConsentScope>,
        duration: Option<Duration>,
    },
    /// 拒绝：附原因（用于横幅 / 日志）。
    Deny { reason: String },
}

/// 连接被关闭的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ClosedReason {
    /// Host 主动撤销 / 终止。
    Revoked,
    /// 心跳超时（对端失联）。
    Timeout,
    /// 传输层断开（非超时）。
    Disconnected,
    /// 授权有效期到期。
    Expired,
}

/// 连接生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ConnectionState {
    /// 已发起，等待 Host 同意。
    AwaitingConsent,
    /// 已激活（含授予的范围与到期时刻）。
    Active {
        scopes: HashSet<ConsentScope>,
        /// `Instant` 不可序列化（瞬态），横幅只需展示"是否已激活/范围"，跳过它。
        #[serde(skip)]
        expires_at: Option<Instant>,
    },
    /// 被 Host 拒绝。
    Denied { reason: String },
    /// 已关闭。
    Closed(ClosedReason),
}

/// Host 侧的同意门控 + 生命周期状态机。
///
/// 由 P4 验签得到的 [`VerifiedPeer`] 构建，因此本门控里的对端身份 / 指纹是已被
/// 密码学确认的，横幅无法被对端伪造。
pub struct ConsentGate {
    peer: VerifiedPeer,
    mode: ConsentMode,
    state: ConnectionState,
    /// 距离上次心跳超过该时长即判定超时。
    heartbeat_timeout: Duration,
    last_heartbeat: Option<Instant>,
    /// 剪贴板是否已被本次传输逐次授权（一次性，用完即废）。
    clipboard_granted: bool,
}

impl ConsentGate {
    /// 构建门控。`heartbeat_timeout` 为心跳超时阈值（建议数秒~数十秒）。
    pub fn new(peer: VerifiedPeer, mode: ConsentMode, heartbeat_timeout: Duration) -> Self {
        Self {
            peer,
            mode,
            state: ConnectionState::AwaitingConsent,
            heartbeat_timeout,
            last_heartbeat: None,
            clipboard_granted: false,
        }
    }

    /// 对端发来连接请求（须在 P4 验签之后调用）。
    ///
    /// - 交互模式：保持 `AwaitingConsent`，等待用户 `decide`。
    /// - 无人值守模式：PIN 匹配则直接 `Active`（全范围），否则 `Denied`。
    pub fn request_consent(&mut self, presented_pin: Option<&str>) -> ConnectionState {
        if let ConsentMode::Unattended { pin } = &self.mode {
            let ok = match presented_pin {
                Some(p) => constant_time_eq(p.as_bytes(), pin.as_bytes()),
                None => false,
            };
            self.state = if ok {
                ConnectionState::Active {
                    scopes: full_scopes(),
                    expires_at: None,
                }
            } else {
                ConnectionState::Denied {
                    reason: "临时 PIN 不匹配".into(),
                }
            };
            // 激活即把心跳基线设为"现在"，否则永不发心跳的对端会被判失联（见 tick）。
            if ok {
                self.last_heartbeat = Some(Instant::now());
            }
            return self.state.clone();
        }
        // 交互模式：等待用户决定。
        self.state = ConnectionState::AwaitingConsent;
        self.state.clone()
    }

    /// Host 用户（或上层策略）做出的决定。
    pub fn decide(&mut self, decision: ConsentDecision) -> ConnectionState {
        self.state = match decision {
            ConsentDecision::Grant { scopes, duration } => {
                // 激活即把心跳基线设为"现在"，使超时时钟从授权时刻起算；
                // 此后若对端再不发心跳，`tick` 也会在阈值后判其失联（防僵尸会话）。
                self.last_heartbeat = Some(Instant::now());
                ConnectionState::Active {
                    scopes,
                    expires_at: duration.map(|d| Instant::now() + d),
                }
            }
            ConsentDecision::Deny { reason } => ConnectionState::Denied { reason },
        };
        self.state.clone()
    }

    /// Host 随时终止连接。
    pub fn revoke(&mut self) -> ConnectionState {
        self.state = ConnectionState::Closed(ClosedReason::Revoked);
        self.state.clone()
    }

    /// 记录一次心跳（对端仍在线）。`now` 由调用方提供（测试可注入）。
    pub fn note_heartbeat(&mut self, now: Instant) {
        self.last_heartbeat = Some(now);
    }

    /// 推进时间：仅在 `Active` 态检查授权到期 / 心跳超时，返回当前状态。
    /// `now` 由调用方提供（测试可注入）。
    ///
    /// 心跳基线在 `decide(Grant)` / 无人值守批准时设为激活时刻，因此：
    ///
    /// - 对端持续发心跳 → 基线不断前移，连接保持；
    /// - 对端从未发心跳（或停发）→ 从激活时刻起算超过 `heartbeat_timeout` 即判失联。
    ///
    /// 这保证"沉默的对端"也能被及时断开，不会出现永不超时的僵尸会话。
    pub fn tick(&mut self, now: Instant) -> ConnectionState {
        if let ConnectionState::Active { expires_at, .. } = &self.state {
            // 授权到期
            if let Some(exp) = expires_at {
                if now >= *exp {
                    self.state = ConnectionState::Closed(ClosedReason::Expired);
                    return self.state.clone();
                }
            }
            // 心跳超时：基线存在且在窗口内 → 保持；否则（含基线缺失）判失联。
            let alive = match self.last_heartbeat {
                Some(last) => now.saturating_duration_since(last) <= self.heartbeat_timeout,
                None => false,
            };
            if !alive {
                self.state = ConnectionState::Closed(ClosedReason::Timeout);
                return self.state.clone();
            }
        }
        self.state.clone()
    }

    /// 传输层报告断开（非超时）。
    pub fn on_disconnected(&mut self) -> ConnectionState {
        if matches!(self.state, ConnectionState::Active { .. }) {
            self.state = ConnectionState::Closed(ClosedReason::Disconnected);
        }
        self.state.clone()
    }

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    pub fn peer(&self) -> &VerifiedPeer {
        &self.peer
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, ConnectionState::Active { .. })
    }

    /// 当前激活态下是否拥有某范围（非 Active 一律 false）。
    pub fn scopes_allow(&self, scope: ConsentScope) -> bool {
        match &self.state {
            ConnectionState::Active { scopes, .. } => scopes.contains(&scope),
            _ => false,
        }
    }

    /// 剪贴板逐次同意：仅当本次已授权且仍 Active 时允许"一次"，用完即废（防泄密）。
    pub fn request_clipboard(&mut self) -> bool {
        let ok = self.is_active()
            && self.scopes_allow(ConsentScope::Clipboard)
            && self.clipboard_granted;
        if ok {
            self.clipboard_granted = false; // 一次性
        }
        ok
    }

    /// Host 显式批准本次剪贴板传输（调用后方可用一次）。
    pub fn grant_clipboard_once(&mut self) {
        if self.is_active() && self.scopes_allow(ConsentScope::Clipboard) {
            self.clipboard_granted = true;
        }
    }

    /// 不可伪造横幅所需的实时数据：UI / OS 高权限层必须**按此原样**渲染，Viewer 无法覆盖。
    /// `encrypted` 表示本次会话是否已建立端到端加密（P5 会话密钥握手成功后传 true）。
    pub fn security_indicator(&self, encrypted: bool) -> SecurityIndicator {
        SecurityIndicator {
            display_name: self.peer.display_name.clone(),
            device_id: self.peer.id,
            fingerprint: self.peer.fingerprint.clone(),
            fingerprint_spaced: self.peer.fingerprint.to_spaced_hex(),
            state: self.state.clone(),
            encrypted,
        }
    }
}

/// 不可伪造安全横幅的实时数据模型（P6 / OS 高权限层渲染）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecurityIndicator {
    /// 对端展示名（来自已认证的 VerifiedPeer）。
    pub display_name: String,
    /// 对端设备 ID。
    pub device_id: DeviceId,
    /// 对端公钥指纹（应由用户带外核对）。
    pub fingerprint: Fingerprint,
    /// 空格分隔的大写十六进制指纹，便于人眼逐字节比对。
    pub fingerprint_spaced: String,
    /// 当前连接状态（横幅必须如实反映）。
    pub state: ConnectionState,
    /// 是否已建立端到端加密。
    pub encrypted: bool,
}

/// 无人值守模式授予的全部范围。
fn full_scopes() -> HashSet<ConsentScope> {
    [
        ConsentScope::View,
        ConsentScope::Input,
        ConsentScope::Clipboard,
        ConsentScope::FileTransfer,
    ]
    .into_iter()
    .collect()
}

/// 常量时间字符串比较（防时序侧信道猜 PIN）。长度不同直接 false。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_crypto::{Fingerprint, PublicKey};
    use rdcore_session::VerifiedPeer;
    use std::time::Duration;

    fn peer() -> VerifiedPeer {
        VerifiedPeer {
            id: [1u8; 16],
            display_name: "friend-phone".into(),
            public_key: PublicKey([2u8; 32]),
            fingerprint: Fingerprint([3u8; 32]),
        }
    }

    fn active_gate() -> ConsentGate {
        let mut g = ConsentGate::new(peer(), ConsentMode::Interactive, Duration::from_secs(30));
        g.request_consent(None);
        g.decide(ConsentDecision::Grant {
            scopes: [ConsentScope::View, ConsentScope::Input]
                .into_iter()
                .collect(),
            duration: None,
        });
        g
    }

    #[test]
    fn interactive_approve_grants_scopes() {
        let g = active_gate();
        assert!(g.is_active());
        assert!(g.scopes_allow(ConsentScope::View));
        assert!(g.scopes_allow(ConsentScope::Input));
        // 未授予的范围必须 false
        assert!(!g.scopes_allow(ConsentScope::Clipboard));
        assert!(!g.scopes_allow(ConsentScope::FileTransfer));
    }

    #[test]
    fn interactive_deny_blocks() {
        let mut g = ConsentGate::new(peer(), ConsentMode::Interactive, Duration::from_secs(30));
        g.request_consent(None);
        g.decide(ConsentDecision::Deny {
            reason: "不认识这台设备".into(),
        });
        assert!(!g.is_active());
        assert_eq!(
            g.state(),
            &ConnectionState::Denied {
                reason: "不认识这台设备".into()
            }
        );
    }

    #[test]
    fn revoke_closes_active() {
        let mut g = active_gate();
        g.revoke();
        assert_eq!(g.state(), &ConnectionState::Closed(ClosedReason::Revoked));
        assert!(!g.is_active());
    }

    #[test]
    fn heartbeat_timeout_closes() {
        let mut g = active_gate();
        let t0 = Instant::now();
        g.note_heartbeat(t0);
        assert!(matches!(g.tick(t0), ConnectionState::Active { .. }));
        // 超过超时阈值 → 关闭
        let late = t0 + Duration::from_secs(31);
        assert_eq!(g.tick(late), ConnectionState::Closed(ClosedReason::Timeout));
    }

    #[test]
    fn heartbeat_timeout_closes_without_explicit_heartbeat() {
        // 关键回归：激活后若对端从未发过任何心跳（last_heartbeat 仍为 None），
        // 也必须从激活时刻起算超时并断开，不能出现永不超时的僵尸会话。
        let mut g = ConsentGate::new(peer(), ConsentMode::Interactive, Duration::from_secs(30));
        g.request_consent(None);
        g.decide(ConsentDecision::Grant {
            scopes: [ConsentScope::View, ConsentScope::Input]
                .into_iter()
                .collect(),
            duration: None,
        });
        let t0 = Instant::now();
        assert!(g.is_active(), "刚授权应 Active");
        // 从未调用 note_heartbeat，仍在窗口内 → 保持
        assert!(matches!(g.tick(t0), ConnectionState::Active { .. }));
        // 超过阈值 → 判失联关闭
        let late = t0 + Duration::from_secs(31);
        assert_eq!(g.tick(late), ConnectionState::Closed(ClosedReason::Timeout));
    }

    #[test]
    fn grant_expiry_closes() {
        let mut g = ConsentGate::new(peer(), ConsentMode::Interactive, Duration::from_secs(30));
        g.request_consent(None);
        g.decide(ConsentDecision::Grant {
            scopes: [ConsentScope::View].into_iter().collect(),
            duration: Some(Duration::from_secs(10)),
        });
        let t0 = Instant::now();
        assert!(g.is_active());
        assert_eq!(
            g.tick(t0 + Duration::from_secs(11)),
            ConnectionState::Closed(ClosedReason::Expired)
        );
    }

    #[test]
    fn unattended_pin_accepts_and_rejects() {
        let mut g = ConsentGate::new(
            peer(),
            ConsentMode::Unattended { pin: "1234".into() },
            Duration::from_secs(30),
        );
        // 正确 PIN → 直接激活且全范围
        let s = g.request_consent(Some("1234"));
        assert!(matches!(s, ConnectionState::Active { .. }));
        assert!(g.scopes_allow(ConsentScope::Input));
        assert!(g.scopes_allow(ConsentScope::FileTransfer));

        // 错误 PIN → 拒绝
        let mut g2 = ConsentGate::new(
            peer(),
            ConsentMode::Unattended { pin: "1234".into() },
            Duration::from_secs(30),
        );
        let s2 = g2.request_consent(Some("0000"));
        assert_eq!(
            s2,
            ConnectionState::Denied {
                reason: "临时 PIN 不匹配".into()
            }
        );
    }

    #[test]
    fn clipboard_is_per_use() {
        let mut g = ConsentGate::new(peer(), ConsentMode::Interactive, Duration::from_secs(30));
        g.request_consent(None);
        g.decide(ConsentDecision::Grant {
            scopes: [ConsentScope::Clipboard].into_iter().collect(),
            duration: None,
        });
        // 未逐次授权前不能用
        assert!(!g.request_clipboard());
        // Host 批准一次
        g.grant_clipboard_once();
        assert!(g.request_clipboard());
        // 用完即废
        assert!(!g.request_clipboard());
    }

    #[test]
    fn security_indicator_reflects_peer_and_state() {
        let g = active_gate();
        let ind = g.security_indicator(true);
        assert_eq!(ind.display_name, "friend-phone");
        assert_eq!(ind.device_id, [1u8; 16]);
        assert!(ind.encrypted);
        assert!(!ind.fingerprint_spaced.is_empty());
        assert!(matches!(ind.state, ConnectionState::Active { .. }));
    }
}
