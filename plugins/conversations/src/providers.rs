//! Provider adapter registry for rebuildup.conversations.
//!
//! Rust port of `plugins/conversations/src/providers.ts` (retired; git
//! history preserves it). v1 ships claude (JSONL), codex (rollout
//! JSONL), and opencode (SQLite `opencode.db`) adapters. An unlisted
//! provider is reported as `unknown_provider`, never silently indexed.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub status: &'static str,
    pub formats: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned_in: Option<&'static str>,
    /// Token-usage accounting the adapter surfaces on usage-carrying
    /// events (additive capability detail, consumed by #92/#28 T3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsage {
    pub cache_tokens: &'static str,
}

pub const PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "claude",
        status: "supported",
        formats: &["claude-jsonl"],
        reason: None,
        planned_in: None,
        usage: Some(ProviderUsage {
            cache_tokens: "reported",
        }),
    },
    ProviderDescriptor {
        id: "codex",
        status: "supported",
        formats: &["codex-rollout-jsonl"],
        reason: None,
        planned_in: None,
        usage: Some(ProviderUsage {
            cache_tokens: "reported",
        }),
    },
    ProviderDescriptor {
        id: "opencode",
        status: "supported",
        formats: &["opencode-sqlite"],
        reason: None,
        planned_in: None,
        usage: Some(ProviderUsage {
            cache_tokens: "reported",
        }),
    },
];

pub fn provider_descriptor(id: &str) -> Option<&'static ProviderDescriptor> {
    PROVIDERS.iter().find(|p| p.id == id)
}

pub fn is_supported(id: &str) -> bool {
    provider_descriptor(id)
        .map(|p| p.status == "supported")
        .unwrap_or(false)
}
