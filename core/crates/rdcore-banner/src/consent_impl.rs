//! `consent` feature：把 `rdcore_consent::SecurityIndicator` 一键映射为 `BannerState`，
//! 保证"横幅展示什么"与"核心产出的安全结论"是同一份事实来源，无二次手工转换。
//!
//! 映射规则：
//! - `SecurityIndicator` 的 `display_name` / `fingerprint_spaced` / `encrypted` 直接搬运；
//! - `device_id`（`[u8;16]`）与 `fingerprint` 转十六进制字符串；
//! - `ConnectionState` 映射到 `BannerPhase`，并把授予范围 / 关闭原因 / 拒绝说明带入。

#![cfg(feature = "consent")]

use crate::{BannerPhase, BannerState};
use rdcore_consent::{ClosedReason, ConnectionState, ConsentScope, SecurityIndicator};

impl From<SecurityIndicator> for BannerState {
    fn from(i: SecurityIndicator) -> Self {
        let (phase, granted_scopes, closed_reason, message) = match i.state {
            ConnectionState::AwaitingConsent => (BannerPhase::AwaitingConsent, vec![], None, None),
            ConnectionState::Active { scopes, .. } => {
                let scopes: Vec<String> = scopes.iter().map(scope_to_str).collect();
                (BannerPhase::Active, scopes, None, None)
            }
            ConnectionState::Denied { reason } => (BannerPhase::Denied, vec![], None, Some(reason)),
            ConnectionState::Closed(r) => {
                (BannerPhase::Closed, vec![], Some(reason_to_str(&r)), None)
            }
        };

        BannerState {
            peer_name: i.display_name,
            peer_device_id: i.device_id.iter().map(|b| format!("{b:02x}")).collect(),
            peer_fingerprint: i.fingerprint_spaced.replace(' ', ""),
            peer_fingerprint_spaced: i.fingerprint_spaced,
            encrypted: i.encrypted,
            phase,
            granted_scopes,
            closed_reason,
            message,
        }
    }
}

fn scope_to_str(s: &ConsentScope) -> String {
    match s {
        ConsentScope::View => "view".to_string(),
        ConsentScope::Input => "input".to_string(),
        ConsentScope::Clipboard => "clipboard".to_string(),
        ConsentScope::FileTransfer => "fileTransfer".to_string(),
    }
}

fn reason_to_str(r: &ClosedReason) -> String {
    match r {
        ClosedReason::Revoked => "revoked".to_string(),
        ClosedReason::Timeout => "timeout".to_string(),
        ClosedReason::Disconnected => "disconnected".to_string(),
        ClosedReason::Expired => "expired".to_string(),
    }
}
