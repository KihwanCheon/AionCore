//! Spawn adjustments for conversations bound to a non-default Claude account.

use std::path::PathBuf;

use aionui_api_types::DEFAULT_CLAUDE_ACCOUNT_ID;
use aionui_common::EnvVar;
use aionui_session::SessionConfig;

use super::store::ClaudeAccountStore;
use crate::error::AgentError;

const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Resolved spawn inputs for one non-default account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionAccount {
    pub id: String,
    pub config_dir: PathBuf,
    /// Default account `settings.json` loaded through `--settings`, when shared and present.
    pub settings_path: Option<PathBuf>,
}

/// Resolves the conversation's account. `Ok(None)` means the default account,
/// which keeps the spawn unchanged. An unknown account is an error: falling back
/// to another account would resume the session in the wrong config directory.
pub(crate) async fn resolve_session_account(
    store: Option<&ClaudeAccountStore>,
    account_id: Option<&str>,
    default_settings_path: Option<PathBuf>,
) -> Result<Option<SessionAccount>, AgentError> {
    let Some(id) = account_id
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != DEFAULT_CLAUDE_ACCOUNT_ID)
    else {
        return Ok(None);
    };
    let store = store.ok_or_else(|| AgentError::bad_request("Claude accounts are not available"))?;
    let accounts = store.load().await?;
    let account = accounts
        .iter()
        .find(|account| account.id == id)
        .ok_or_else(|| AgentError::bad_request(format!("Claude account '{id}' was not found")))?;
    let config_dir = account
        .config_dir
        .as_deref()
        .ok_or_else(|| AgentError::internal(format!("Claude account '{id}' has no config directory")))?;
    let settings_path = default_settings_path.filter(|path| account.share_default_settings && path.is_file());
    Ok(Some(SessionAccount {
        id: id.to_owned(),
        config_dir: PathBuf::from(config_dir),
        settings_path,
    }))
}

/// Points the Claude CLI at the account's config directory and, when shared,
/// loads the default account's `settings.json` (hooks, permissions) as well.
pub(crate) fn apply_session_account(config: &mut SessionConfig, account: &SessionAccount) {
    config
        .spawn_env
        .retain(|entry| !entry.name.eq_ignore_ascii_case(CONFIG_DIR_ENV));
    config.spawn_env.push(EnvVar {
        name: CONFIG_DIR_ENV.to_owned(),
        value: account.config_dir.to_string_lossy().into_owned(),
    });
    if let Some(path) = &account.settings_path {
        config
            .extra_args
            .extend(["--settings".to_owned(), path.to_string_lossy().into_owned()]);
    }
}

/// The default Claude configuration directory of this machine.
pub(crate) fn default_config_dir() -> Option<PathBuf> {
    std::env::var_os(CONFIG_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

pub(crate) fn default_settings_path() -> Option<PathBuf> {
    default_config_dir().map(|dir| dir.join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::super::store::StoredAccount;
    use super::*;

    async fn store_with_max(dir: &std::path::Path, share: bool) -> ClaudeAccountStore {
        let store = ClaudeAccountStore::in_data_dir(dir);
        store
            .update(|accounts| {
                accounts.push(StoredAccount {
                    id: DEFAULT_CLAUDE_ACCOUNT_ID.to_owned(),
                    label: "Team".to_owned(),
                    config_dir: None,
                    share_default_settings: true,
                });
                accounts.push(StoredAccount {
                    id: "max".to_owned(),
                    label: "Max".to_owned(),
                    config_dir: Some("C:/Users/me/.claude-max".to_owned()),
                    share_default_settings: share,
                });
                Ok(())
            })
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn default_or_missing_account_keeps_the_spawn_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_max(dir.path(), true).await;
        for id in [None, Some(""), Some(DEFAULT_CLAUDE_ACCOUNT_ID)] {
            assert_eq!(resolve_session_account(Some(&store), id, None).await.unwrap(), None);
        }
        // No store installed and no account requested: still the default path.
        assert_eq!(resolve_session_account(None, None, None).await.unwrap(), None);
    }

    #[tokio::test]
    async fn unknown_account_is_rejected_instead_of_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_max(dir.path(), true).await;
        assert!(resolve_session_account(Some(&store), Some("gone"), None).await.is_err());
        assert!(resolve_session_account(None, Some("max"), None).await.is_err());
    }

    #[tokio::test]
    async fn shared_settings_are_passed_only_when_enabled_and_present() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, "{}").unwrap();

        let shared = store_with_max(dir.path(), true).await;
        let account = resolve_session_account(Some(&shared), Some("max"), Some(settings.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.settings_path.as_deref(), Some(settings.as_path()));

        let missing = resolve_session_account(Some(&shared), Some("max"), Some(dir.path().join("none.json")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(missing.settings_path, None);

        let other_dir = tempfile::tempdir().unwrap();
        let unshared = store_with_max(other_dir.path(), false).await;
        let account = resolve_session_account(Some(&unshared), Some("max"), Some(settings))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.settings_path, None);
    }

    #[test]
    fn apply_sets_config_dir_last_and_appends_settings_flag() {
        let mut config = SessionConfig {
            spawn_env: vec![
                EnvVar {
                    name: "claude_config_dir".to_owned(),
                    value: "C:/agent-override".to_owned(),
                },
                EnvVar {
                    name: "HTTPS_PROXY".to_owned(),
                    value: "http://proxy".to_owned(),
                },
            ],
            extra_args: vec!["--plugin-dir".to_owned(), "C:/view".to_owned()],
            ..Default::default()
        };
        let account = SessionAccount {
            id: "max".to_owned(),
            config_dir: PathBuf::from("C:/Users/me/.claude-max"),
            settings_path: Some(PathBuf::from("C:/Users/me/.claude/settings.json")),
        };
        apply_session_account(&mut config, &account);

        let names: Vec<&str> = config.spawn_env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["HTTPS_PROXY", "CLAUDE_CONFIG_DIR"]);
        assert_eq!(config.spawn_env[1].value, "C:/Users/me/.claude-max");
        assert_eq!(
            config.extra_args,
            [
                "--plugin-dir",
                "C:/view",
                "--settings",
                "C:/Users/me/.claude/settings.json"
            ]
        );
    }
}
