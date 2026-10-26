//! Claude subscription accounts maintained by the personal fork.
//!
//! A conversation selects a non-default account through
//! `extra.claude_account_id`; without it the default Claude configuration is
//! used exactly as before.

use serde::{Deserialize, Serialize};

/// Id of the default account, i.e. the default Claude configuration directory.
pub const DEFAULT_CLAUDE_ACCOUNT_ID: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeAccount {
    pub id: String,
    /// 1-based position in the account order shared by every client.
    pub number: u32,
    pub label: String,
    /// `None` for the default account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    pub is_default: bool,
    /// Whether sessions also load the default account's `settings.json`.
    pub share_default_settings: bool,
}

/// Compact entry embedded in Claude agent rows only when two or more accounts exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeAccountSummary {
    pub id: String,
    pub number: u32,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateClaudeAccountRequest {
    pub label: String,
    pub config_dir: String,
    /// Label stored for the default account when the first extra account is added.
    #[serde(default)]
    pub default_label: Option<String>,
    #[serde(default = "default_true")]
    pub share_default_settings: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateClaudeAccountRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub share_default_settings: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorderClaudeAccountsRequest {
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeAccountAuthStatus {
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

fn default_true() -> bool {
    true
}
