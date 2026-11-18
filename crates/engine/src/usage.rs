//! Subscription usage, and the reset credits that clear its limits, for each
//! provider that reports them.

use crate::auth::AuthProvider;
use smelt_provider::usage::ManagedUsageReport;

/// A provider whose subscription usage smelt can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageProvider {
    Codex,
    KimiCode,
    ClaudeCode,
}

impl UsageProvider {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "codex" => Ok(Self::Codex),
            "kimi-code" => Ok(Self::KimiCode),
            "claude-code" => Ok(Self::ClaudeCode),
            other => Err(format!("usage is not supported for {other}")),
        }
    }
}

/// An opaque identity of the signed-in account, so usage caches and
/// redemptions never cross accounts. Never contains credentials.
pub fn scope(provider: UsageProvider) -> Option<String> {
    match provider {
        UsageProvider::Codex => crate::auth::usage_scope(AuthProvider::Codex),
        UsageProvider::KimiCode => crate::auth::usage_scope(AuthProvider::KimiCode),
        UsageProvider::ClaudeCode => crate::claude_code::usage_scope(),
    }
}

pub async fn report(provider: UsageProvider) -> Result<ManagedUsageReport, String> {
    let client = smelt_provider::http_client();
    match provider {
        UsageProvider::Codex => crate::auth::managed_usage(AuthProvider::Codex, &client).await,
        UsageProvider::KimiCode => {
            crate::auth::managed_usage(AuthProvider::KimiCode, &client).await
        }
        UsageProvider::ClaudeCode => crate::claude_code::usage(&client).await,
    }
}

/// Redeems one reset credit of the account `scope` names. `credit_id` picks
/// the reset when the report lists options. Reuse `request_id` to retry an
/// uncertain outcome without spending twice.
pub async fn redeem(
    provider: UsageProvider,
    scope: &str,
    credit_id: Option<&str>,
    request_id: &str,
) -> Result<String, String> {
    let client = smelt_provider::http_client();
    match provider {
        UsageProvider::Codex => crate::auth::redeem_usage_reset(scope, request_id, &client).await,
        UsageProvider::ClaudeCode => {
            let credit_id = credit_id.ok_or("Choose which Claude reset to redeem.")?;
            crate::claude_code::redeem_reset(&client, scope, credit_id, request_id).await
        }
        UsageProvider::KimiCode => Err("Kimi Code has no usage limit resets.".into()),
    }
}
