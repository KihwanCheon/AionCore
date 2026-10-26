//! Account management: numbering, validation, and login status.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    ClaudeAccount, ClaudeAccountAuthStatus, CreateClaudeAccountRequest, DEFAULT_CLAUDE_ACCOUNT_ID,
    ReorderClaudeAccountsRequest, UpdateClaudeAccountRequest,
};
use aionui_runtime::Builder;
use async_trait::async_trait;

use super::session::default_config_dir;
use super::store::{ClaudeAccountStore, StoredAccount};
use crate::error::AgentError;

const MAX_LABEL_CHARS: usize = 60;
const DEFAULT_ACCOUNT_FALLBACK_LABEL: &str = "Default";
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(20);

/// Counts conversations bound to an account (implemented over the conversation store).
#[async_trait]
pub trait IClaudeAccountUsage: Send + Sync {
    async fn conversations_using(&self, account_id: &str) -> Result<i64, AgentError>;
}

pub struct ClaudeAccountService {
    store: Arc<ClaudeAccountStore>,
    usage: Arc<dyn IClaudeAccountUsage>,
}

impl ClaudeAccountService {
    pub fn new(store: Arc<ClaudeAccountStore>, usage: Arc<dyn IClaudeAccountUsage>) -> Self {
        Self { store, usage }
    }

    pub async fn list(&self) -> Result<Vec<ClaudeAccount>, AgentError> {
        Ok(numbered(&self.store.load().await?))
    }

    pub async fn create(&self, req: CreateClaudeAccountRequest) -> Result<ClaudeAccount, AgentError> {
        let label = normalize_label(&req.label)?;
        let default_label = match req.default_label.as_deref() {
            Some(value) => normalize_label(value)?,
            None => DEFAULT_ACCOUNT_FALLBACK_LABEL.to_owned(),
        };
        let config_dir = req.config_dir.trim().to_owned();
        let requested = canonical_dir(&config_dir).await?;
        if let Some(default_dir) = default_config_dir()
            && canonical_dir_if_exists(&default_dir).await.as_ref() == Some(&requested)
        {
            return Err(AgentError::bad_request(
                "The default Claude config directory is already the default account",
            ));
        }
        let existing = self.store.load().await?;
        for account in existing.iter().filter_map(|account| account.config_dir.as_deref()) {
            if canonical_dir_if_exists(Path::new(account)).await.as_ref() == Some(&requested) {
                return Err(AgentError::conflict(
                    "This Claude config directory is already registered",
                ));
            }
        }

        let id = new_account_id(&existing);
        let share_default_settings = req.share_default_settings;
        let created_id = id.clone();
        let accounts = self
            .store
            .update(move |accounts| {
                if accounts.is_empty() {
                    accounts.push(StoredAccount {
                        id: DEFAULT_CLAUDE_ACCOUNT_ID.to_owned(),
                        label: default_label,
                        config_dir: None,
                        share_default_settings: true,
                    });
                }
                accounts.push(StoredAccount {
                    id,
                    label,
                    config_dir: Some(config_dir),
                    share_default_settings,
                });
                Ok(accounts.clone())
            })
            .await?;
        tracing::info!(account_id = %created_id, total = accounts.len(), "claude account created");
        find_numbered(&accounts, &created_id)
    }

    pub async fn update(&self, id: &str, req: UpdateClaudeAccountRequest) -> Result<ClaudeAccount, AgentError> {
        let label = req.label.as_deref().map(normalize_label).transpose()?;
        let target = id.to_owned();
        let accounts = self
            .store
            .update(move |accounts| {
                let account = accounts
                    .iter_mut()
                    .find(|account| account.id == target)
                    .ok_or_else(|| AgentError::not_found(format!("Claude account '{target}' not found")))?;
                if let Some(label) = label {
                    account.label = label;
                }
                if let Some(share) = req.share_default_settings
                    && account.id != DEFAULT_CLAUDE_ACCOUNT_ID
                {
                    account.share_default_settings = share;
                }
                Ok(accounts.clone())
            })
            .await?;
        find_numbered(&accounts, id)
    }

    pub async fn reorder(&self, req: ReorderClaudeAccountsRequest) -> Result<Vec<ClaudeAccount>, AgentError> {
        let accounts = self
            .store
            .update(move |accounts| {
                let mut requested = req.ids.clone();
                requested.sort();
                requested.dedup();
                let mut current: Vec<String> = accounts.iter().map(|account| account.id.clone()).collect();
                current.sort();
                if accounts.is_empty() || requested.len() != req.ids.len() || requested != current {
                    return Err(AgentError::bad_request(
                        "ids must list every Claude account exactly once",
                    ));
                }
                let mut reordered = Vec::with_capacity(accounts.len());
                for id in &req.ids {
                    if let Some(index) = accounts.iter().position(|account| &account.id == id) {
                        reordered.push(accounts.remove(index));
                    }
                }
                *accounts = reordered;
                Ok(accounts.clone())
            })
            .await?;
        Ok(numbered(&accounts))
    }

    pub async fn delete(&self, id: &str) -> Result<(), AgentError> {
        if id == DEFAULT_CLAUDE_ACCOUNT_ID {
            return Err(AgentError::bad_request("The default Claude account cannot be deleted"));
        }
        if !self.store.load().await?.iter().any(|account| account.id == id) {
            return Err(AgentError::not_found(format!("Claude account '{id}' not found")));
        }
        let used = self.usage.conversations_using(id).await?;
        if used > 0 {
            return Err(AgentError::conflict(format!(
                "Claude account is used by {used} conversation(s)"
            )));
        }
        let target = id.to_owned();
        self.store
            .update(move |accounts| {
                accounts.retain(|account| account.id != target);
                Ok(())
            })
            .await?;
        tracing::info!(account_id = %id, "claude account deleted");
        Ok(())
    }

    /// Runs `claude auth status --json` against the account's config directory.
    pub async fn auth_status(&self, id: &str) -> Result<ClaudeAccountAuthStatus, AgentError> {
        let accounts = self.store.load().await?;
        let config_dir = if id == DEFAULT_CLAUDE_ACCOUNT_ID {
            None
        } else {
            let account = accounts
                .iter()
                .find(|account| account.id == id)
                .ok_or_else(|| AgentError::not_found(format!("Claude account '{id}' not found")))?;
            account.config_dir.clone()
        };
        let program = aionui_runtime::resolve_command_path("claude")
            .ok_or_else(|| AgentError::not_found("claude CLI was not found on PATH"))?;
        let mut command = Builder::clean_cli(&program);
        command.args(["auth", "status", "--json"]);
        match config_dir {
            Some(dir) => command.env("CLAUDE_CONFIG_DIR", dir),
            None => command.env_remove("CLAUDE_CONFIG_DIR"),
        };
        let output = tokio::time::timeout(AUTH_STATUS_TIMEOUT, command.output())
            .await
            .map_err(|_| AgentError::timeout("claude auth status timed out"))?
            .map_err(|e| AgentError::bad_gateway(format!("run claude auth status: {e}")))?;
        Ok(parse_auth_status(&String::from_utf8_lossy(&output.stdout)))
    }
}

/// Accounts with their shared numbers. An empty store is the implicit default account.
pub(crate) fn numbered(stored: &[StoredAccount]) -> Vec<ClaudeAccount> {
    if stored.is_empty() {
        return vec![ClaudeAccount {
            id: DEFAULT_CLAUDE_ACCOUNT_ID.to_owned(),
            number: 1,
            label: String::new(),
            config_dir: None,
            is_default: true,
            share_default_settings: true,
        }];
    }
    stored
        .iter()
        .enumerate()
        .map(|(index, account)| ClaudeAccount {
            id: account.id.clone(),
            number: u32::try_from(index + 1).unwrap_or(u32::MAX),
            label: account.label.clone(),
            config_dir: account.config_dir.clone(),
            is_default: account.id == DEFAULT_CLAUDE_ACCOUNT_ID,
            share_default_settings: account.share_default_settings,
        })
        .collect()
}

fn find_numbered(stored: &[StoredAccount], id: &str) -> Result<ClaudeAccount, AgentError> {
    numbered(stored)
        .into_iter()
        .find(|account| account.id == id)
        .ok_or_else(|| AgentError::not_found(format!("Claude account '{id}' not found")))
}

fn normalize_label(value: &str) -> Result<String, AgentError> {
    let label = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(AgentError::bad_request(format!(
            "label must be 1 to {MAX_LABEL_CHARS} characters"
        )));
    }
    Ok(label)
}

async fn canonical_dir(value: &str) -> Result<PathBuf, AgentError> {
    let path = Path::new(value);
    if value.is_empty() || !path.is_absolute() {
        return Err(AgentError::bad_request("config_dir must be an absolute path"));
    }
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {}
        _ => return Err(AgentError::bad_request("config_dir must be an existing directory")),
    }
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| AgentError::bad_request(format!("config_dir cannot be resolved: {e}")))
}

async fn canonical_dir_if_exists(path: &Path) -> Option<PathBuf> {
    tokio::fs::canonicalize(path).await.ok()
}

fn new_account_id(existing: &[StoredAccount]) -> String {
    loop {
        let id = aionui_common::generate_short_id();
        if id != DEFAULT_CLAUDE_ACCOUNT_ID && !existing.iter().any(|account| account.id == id) {
            return id;
        }
    }
}

fn parse_auth_status(stdout: &str) -> ClaudeAccountAuthStatus {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
    let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
    ClaudeAccountAuthStatus {
        logged_in: value
            .get("loggedIn")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        subscription_type: text("subscriptionType"),
        email: text("email"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct FixedUsage(AtomicI64);

    #[async_trait]
    impl IClaudeAccountUsage for FixedUsage {
        async fn conversations_using(&self, _account_id: &str) -> Result<i64, AgentError> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    fn service(dir: &Path, used: i64) -> ClaudeAccountService {
        ClaudeAccountService::new(
            Arc::new(ClaudeAccountStore::in_data_dir(dir)),
            Arc::new(FixedUsage(AtomicI64::new(used))),
        )
    }

    fn create_request(label: &str, config_dir: &Path) -> CreateClaudeAccountRequest {
        CreateClaudeAccountRequest {
            label: label.to_owned(),
            config_dir: config_dir.to_string_lossy().into_owned(),
            default_label: Some("Team".to_owned()),
            share_default_settings: true,
        }
    }

    #[tokio::test]
    async fn single_account_lists_only_the_implicit_default() {
        let data = tempfile::tempdir().unwrap();
        let accounts = service(data.path(), 0).list().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].is_default);
        assert_eq!(accounts[0].number, 1);
    }

    #[tokio::test]
    async fn first_extra_account_also_stores_the_default_account_first() {
        let data = tempfile::tempdir().unwrap();
        let max_dir = tempfile::tempdir().unwrap();
        let service = service(data.path(), 0);

        let created = service
            .create(create_request(" Personal  Max ", max_dir.path()))
            .await
            .unwrap();
        assert_eq!(created.number, 2);
        assert_eq!(created.label, "Personal Max");

        let accounts = service.list().await.unwrap();
        let summary: Vec<(u32, &str, bool)> = accounts
            .iter()
            .map(|account| (account.number, account.label.as_str(), account.is_default))
            .collect();
        assert_eq!(summary, [(1, "Team", true), (2, "Personal Max", false)]);
    }

    #[tokio::test]
    async fn rejects_relative_missing_and_duplicate_directories() {
        let data = tempfile::tempdir().unwrap();
        let max_dir = tempfile::tempdir().unwrap();
        let service = service(data.path(), 0);

        let mut relative = create_request("Max", max_dir.path());
        relative.config_dir = "relative/dir".to_owned();
        assert!(service.create(relative).await.is_err());
        assert!(
            service
                .create(create_request("Max", &max_dir.path().join("missing")))
                .await
                .is_err()
        );

        service.create(create_request("Max", max_dir.path())).await.unwrap();
        assert!(service.create(create_request("Again", max_dir.path())).await.is_err());
    }

    #[tokio::test]
    async fn reorder_changes_shared_numbers_and_requires_every_id() {
        let data = tempfile::tempdir().unwrap();
        let max_dir = tempfile::tempdir().unwrap();
        let service = service(data.path(), 0);
        let max = service.create(create_request("Max", max_dir.path())).await.unwrap();

        assert!(
            service
                .reorder(ReorderClaudeAccountsRequest {
                    ids: vec![max.id.clone()]
                })
                .await
                .is_err()
        );
        let reordered = service
            .reorder(ReorderClaudeAccountsRequest {
                ids: vec![max.id.clone(), DEFAULT_CLAUDE_ACCOUNT_ID.to_owned()],
            })
            .await
            .unwrap();
        assert_eq!(reordered[0].id, max.id);
        assert_eq!(reordered[0].number, 1);
        assert_eq!(reordered[1].number, 2);
    }

    #[tokio::test]
    async fn delete_is_blocked_for_default_and_in_use_accounts() {
        let data = tempfile::tempdir().unwrap();
        let max_dir = tempfile::tempdir().unwrap();
        let in_use = service(data.path(), 2);
        let max = in_use.create(create_request("Max", max_dir.path())).await.unwrap();

        assert!(in_use.delete(DEFAULT_CLAUDE_ACCOUNT_ID).await.is_err());
        assert!(in_use.delete(&max.id).await.is_err());

        let unused = service(data.path(), 0);
        unused.delete(&max.id).await.unwrap();
        assert_eq!(unused.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_renames_and_keeps_default_settings_sharing() {
        let data = tempfile::tempdir().unwrap();
        let max_dir = tempfile::tempdir().unwrap();
        let service = service(data.path(), 0);
        let max = service.create(create_request("Max", max_dir.path())).await.unwrap();

        let renamed = service
            .update(
                &max.id,
                UpdateClaudeAccountRequest {
                    label: Some("Personal".to_owned()),
                    share_default_settings: Some(false),
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.label, "Personal");
        assert!(!renamed.share_default_settings);

        let default = service
            .update(
                DEFAULT_CLAUDE_ACCOUNT_ID,
                UpdateClaudeAccountRequest {
                    label: None,
                    share_default_settings: Some(false),
                },
            )
            .await
            .unwrap();
        assert!(default.share_default_settings);
        assert!(
            service
                .update("missing", UpdateClaudeAccountRequest::default())
                .await
                .is_err()
        );
    }

    #[test]
    fn parses_auth_status_json_and_treats_garbage_as_logged_out() {
        let status = parse_auth_status(r#"{"loggedIn":true,"subscriptionType":"max","email":"a@b.c"}"#);
        assert!(status.logged_in);
        assert_eq!(status.subscription_type.as_deref(), Some("max"));
        assert!(!parse_auth_status("Not logged in").logged_in);
    }
}
