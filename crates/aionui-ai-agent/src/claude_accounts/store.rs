//! File-backed Claude account list.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::error::AgentError;

pub const CLAUDE_ACCOUNTS_FILE: &str = "claude-accounts.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredAccount {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    #[serde(default = "default_true")]
    pub share_default_settings: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredFile {
    version: u32,
    accounts: Vec<StoredAccount>,
}

/// Account list persisted as JSON; the array order is the shared display order.
///
/// A list with one entry or fewer is never kept on disk: removing the file
/// restores the exact single-account state.
pub struct ClaudeAccountStore {
    path: PathBuf,
    cache: Mutex<Option<Vec<StoredAccount>>>,
}

impl ClaudeAccountStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new(None),
        }
    }

    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join(CLAUDE_ACCOUNTS_FILE))
    }

    pub(crate) async fn load(&self) -> Result<Vec<StoredAccount>, AgentError> {
        let mut cache = self.cache.lock().await;
        if let Some(accounts) = cache.as_ref() {
            return Ok(accounts.clone());
        }
        let accounts = read_file(&self.path).await?;
        *cache = Some(accounts.clone());
        Ok(accounts)
    }

    /// Applies `change` to the current list and persists the result atomically.
    pub(crate) async fn update<T>(
        &self,
        change: impl FnOnce(&mut Vec<StoredAccount>) -> Result<T, AgentError>,
    ) -> Result<T, AgentError> {
        let mut cache = self.cache.lock().await;
        let mut accounts = match cache.as_ref() {
            Some(accounts) => accounts.clone(),
            None => read_file(&self.path).await?,
        };
        let result = change(&mut accounts)?;
        if accounts.len() <= 1 {
            accounts.clear();
        }
        write_file(&self.path, &accounts).await?;
        *cache = Some(accounts);
        Ok(result)
    }
}

async fn read_file(path: &Path) -> Result<Vec<StoredAccount>, AgentError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let file: StoredFile = serde_json::from_slice(&bytes)
                .map_err(|e| AgentError::internal(format!("claude accounts file is invalid: {e}")))?;
            if file.version != FILE_VERSION {
                return Err(AgentError::internal(format!(
                    "claude accounts file version {} is not supported",
                    file.version
                )));
            }
            Ok(file.accounts)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(AgentError::internal(format!("read claude accounts file: {e}"))),
    }
}

async fn write_file(path: &Path, accounts: &[StoredAccount]) -> Result<(), AgentError> {
    if accounts.is_empty() {
        return match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AgentError::internal(format!("remove claude accounts file: {e}"))),
        };
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::internal(format!("create claude accounts directory: {e}")))?;
    }
    let body = serde_json::to_vec_pretty(&StoredFile {
        version: FILE_VERSION,
        accounts: accounts.to_vec(),
    })
    .map_err(|e| AgentError::internal(format!("encode claude accounts: {e}")))?;
    let pending = path.with_extension("json.tmp");
    tokio::fs::write(&pending, body)
        .await
        .map_err(|e| AgentError::internal(format!("write claude accounts file: {e}")))?;
    tokio::fs::rename(&pending, path)
        .await
        .map_err(|e| AgentError::internal(format!("replace claude accounts file: {e}")))
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, config_dir: Option<&str>) -> StoredAccount {
        StoredAccount {
            id: id.to_owned(),
            label: id.to_owned(),
            config_dir: config_dir.map(str::to_owned),
            share_default_settings: true,
        }
    }

    #[tokio::test]
    async fn missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClaudeAccountStore::in_data_dir(dir.path());
        assert!(store.load().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn persists_order_and_reloads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClaudeAccountStore::in_data_dir(dir.path());
        store
            .update(|accounts| {
                accounts.push(account("default", None));
                accounts.push(account("max", Some("C:/max")));
                Ok(())
            })
            .await
            .unwrap();

        let reopened = ClaudeAccountStore::in_data_dir(dir.path());
        let ids: Vec<String> = reopened.load().await.unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["default", "max"]);
    }

    #[tokio::test]
    async fn a_single_remaining_account_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClaudeAccountStore::in_data_dir(dir.path());
        store
            .update(|accounts| {
                accounts.push(account("default", None));
                accounts.push(account("max", Some("C:/max")));
                Ok(())
            })
            .await
            .unwrap();
        assert!(dir.path().join(CLAUDE_ACCOUNTS_FILE).is_file());

        store
            .update(|accounts| {
                accounts.retain(|a| a.id != "max");
                Ok(())
            })
            .await
            .unwrap();
        assert!(!dir.path().join(CLAUDE_ACCOUNTS_FILE).exists());
        assert!(store.load().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_change_leaves_the_stored_list_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClaudeAccountStore::in_data_dir(dir.path());
        let result: Result<(), AgentError> = store
            .update(|accounts| {
                accounts.push(account("default", None));
                Err(AgentError::bad_request("rejected"))
            })
            .await;
        assert!(result.is_err());
        assert!(store.load().await.unwrap().is_empty());
    }
}
