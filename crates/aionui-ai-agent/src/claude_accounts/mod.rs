//! Claude subscription accounts maintained by the personal fork.
//!
//! Accounts live in `<data_dir>/claude-accounts.json`, outside the database, so
//! the feature adds no migration and an older `aioncore` can still open the same
//! database. A missing file means a single default account and nothing changes.
//! A conversation selects a non-default account through
//! `extra.claude_account_id`; its direct-CLI spawn then points
//! `CLAUDE_CONFIG_DIR` at that account (see [`session`]).

mod routes;
mod service;
mod session;
mod store;

use std::sync::{Arc, OnceLock};

use aionui_api_types::{AgentManagementRow, ClaudeAccountSummary};
use aionui_session::SessionConfig;

use crate::error::AgentError;

pub use routes::{ClaudeAccountRouterState, claude_account_routes};
pub use service::{ClaudeAccountService, IClaudeAccountUsage};
pub use store::{CLAUDE_ACCOUNTS_FILE, ClaudeAccountStore};

pub(crate) use session::SessionAccount;

static STORE: OnceLock<Arc<ClaudeAccountStore>> = OnceLock::new();

/// Makes the store visible to session builds and agent listings. Called once
/// from the application composition root; later calls are ignored.
pub fn install_store(store: Arc<ClaudeAccountStore>) {
    let _ = STORE.set(store);
}

fn installed_store() -> Option<&'static ClaudeAccountStore> {
    STORE.get().map(Arc::as_ref)
}

/// Resolves the conversation's account for a Claude spawn; `None` keeps it unchanged.
pub(crate) async fn resolve_for_session(account_id: Option<&str>) -> Result<Option<SessionAccount>, AgentError> {
    session::resolve_session_account(installed_store(), account_id, session::default_settings_path()).await
}

pub(crate) fn apply_to_session(config: &mut SessionConfig, account: &SessionAccount) {
    session::apply_session_account(config, account);
}

/// Adds the shared account order to Claude rows when two or more accounts exist.
/// With a single account the rows are left exactly as before.
pub(crate) async fn attach_management_summaries(rows: &mut [AgentManagementRow]) {
    attach_summaries_from(installed_store(), rows).await;
}

async fn attach_summaries_from(store: Option<&ClaudeAccountStore>, rows: &mut [AgentManagementRow]) {
    let Some(summaries) = summaries_from(store).await else {
        return;
    };
    for row in rows.iter_mut().filter(|row| row.backend.as_deref() == Some("claude")) {
        row.claude_accounts = Some(summaries.clone());
    }
}

async fn summaries_from(store: Option<&ClaudeAccountStore>) -> Option<Vec<ClaudeAccountSummary>> {
    let store = store?;
    let stored = match store.load().await {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(error = %error, "claude accounts unavailable; agent rows keep single-account shape");
            return None;
        }
    };
    (stored.len() >= 2).then(|| {
        service::numbered(&stored)
            .into_iter()
            .map(|account| ClaudeAccountSummary {
                id: account.id,
                number: account.number,
                label: account.label,
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::store::StoredAccount;
    use super::*;
    use aionui_api_types::DEFAULT_CLAUDE_ACCOUNT_ID;

    fn row(id: &str, backend: &str) -> AgentManagementRow {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "backend": backend,
            "agent_type": "acp",
            "agent_source": "builtin",
            "enabled": true,
            "installed": true,
            "sort_order": 0,
            "status": "online",
        }))
        .unwrap()
    }

    async fn store_with(dir: &std::path::Path, extra: bool) -> ClaudeAccountStore {
        let store = ClaudeAccountStore::in_data_dir(dir);
        if extra {
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
                        config_dir: Some("C:/max".to_owned()),
                        share_default_settings: true,
                    });
                    Ok(())
                })
                .await
                .unwrap();
        }
        store
    }

    #[tokio::test]
    async fn single_account_leaves_agent_rows_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path(), false).await;
        let mut rows = vec![row("claude-row", "claude")];
        let before = serde_json::to_value(&rows).unwrap();

        attach_summaries_from(Some(&store), &mut rows).await;
        attach_summaries_from(None, &mut rows).await;

        assert_eq!(serde_json::to_value(&rows).unwrap(), before);
        assert!(before[0].get("claude_accounts").is_none());
    }

    #[tokio::test]
    async fn two_accounts_number_only_claude_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path(), true).await;
        let mut rows = vec![row("claude-row", "claude"), row("codex-row", "codex")];

        attach_summaries_from(Some(&store), &mut rows).await;

        let numbers: Vec<(u32, String)> = rows[0]
            .claude_accounts
            .as_ref()
            .unwrap()
            .iter()
            .map(|account| (account.number, account.label.clone()))
            .collect();
        assert_eq!(numbers, [(1, "Team".to_owned()), (2, "Max".to_owned())]);
        assert!(rows[1].claude_accounts.is_none());
    }
}
