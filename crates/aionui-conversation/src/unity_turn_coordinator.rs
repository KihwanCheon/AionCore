use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use serde_json::Value;
use sha1::{Digest, Sha1};
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnityTurnResource {
    pub key: String,
    pub project_root: String,
}

#[derive(Clone, Default)]
pub(crate) struct UnityTurnCoordinator {
    locks: Arc<StdMutex<HashMap<String, Weak<TokioMutex<()>>>>>,
}

pub(crate) struct UnityTurnClaim {
    resource: UnityTurnResource,
    lock: Arc<TokioMutex<()>>,
}

pub(crate) struct UnityTurnPermit {
    #[allow(dead_code)]
    resource: UnityTurnResource,
    #[allow(dead_code)]
    guard: OwnedMutexGuard<()>,
}

impl UnityTurnCoordinator {
    pub(crate) fn claim_for_conversation_extra(&self, extra: &str) -> Option<UnityTurnClaim> {
        self.claim_for_resource(unity_turn_resource_from_extra(extra)?)
    }

    fn claim_for_resource(&self, resource: UnityTurnResource) -> Option<UnityTurnClaim> {
        let mut locks = self.locks.lock().ok()?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = locks.get(&resource.key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(TokioMutex::new(()));
            locks.insert(resource.key.clone(), Arc::downgrade(&lock));
            lock
        });
        Some(UnityTurnClaim { resource, lock })
    }
}

impl UnityTurnClaim {
    pub(crate) fn resource(&self) -> &UnityTurnResource {
        &self.resource
    }

    pub(crate) fn try_acquire(self) -> Result<UnityTurnPermit, Self> {
        match Arc::clone(&self.lock).try_lock_owned() {
            Ok(guard) => Ok(UnityTurnPermit {
                resource: self.resource,
                guard,
            }),
            Err(_) => Err(self),
        }
    }

    pub(crate) async fn wait(self) -> UnityTurnPermit {
        let guard = Arc::clone(&self.lock).lock_owned().await;
        UnityTurnPermit {
            resource: self.resource,
            guard,
        }
    }
}

fn unity_turn_resource_from_extra(extra: &str) -> Option<UnityTurnResource> {
    let extra: Value = serde_json::from_str(extra).ok()?;
    if !has_unity_mcp(&extra) {
        return None;
    }
    let workspace = extra.get("workspace")?.as_str()?.trim();
    let project_root = verified_unity_project_root(workspace)?;
    let assets_path = format!("{}/Assets", normalize_path_for_key(&project_root));
    let key = format!("unity:{:x}", Sha1::digest(assets_path.as_bytes()));
    Some(UnityTurnResource { key, project_root })
}

fn has_unity_mcp(extra: &Value) -> bool {
    let matches_name = |name: &str| {
        let normalized = name
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>();
        matches!(normalized.as_str(), "unitymcp" | "mcpforunity")
    };

    extra
        .get("mcp_servers")
        .and_then(Value::as_array)
        .is_some_and(|servers| servers.iter().filter_map(Value::as_str).any(matches_name))
        || extra
            .get("mcp_statuses")
            .and_then(Value::as_array)
            .is_some_and(|statuses| {
                statuses
                    .iter()
                    .filter_map(|status| status.get("name").and_then(Value::as_str))
                    .any(matches_name)
            })
        || extra
            .get("session_mcp_servers")
            .and_then(Value::as_array)
            .is_some_and(|servers| {
                servers
                    .iter()
                    .filter_map(|server| server.get("name").and_then(Value::as_str))
                    .any(matches_name)
            })
}

fn verified_unity_project_root(workspace: &str) -> Option<String> {
    let path = PathBuf::from(workspace);
    if !path.is_absolute()
        || !path.join("Assets").is_dir()
        || !path.join("ProjectSettings").join("ProjectVersion.txt").is_file()
    {
        return None;
    }
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    Some(display_path(&canonical))
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    value
        .strip_prefix("//?/")
        .unwrap_or(&value)
        .trim_end_matches('/')
        .to_owned()
}

fn normalize_path_for_key(path: &str) -> String {
    let normalized = path.replace('\\', "/").trim_end_matches('/').to_owned();
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn unity_project() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("Assets")).unwrap();
        fs::create_dir(directory.path().join("ProjectSettings")).unwrap();
        fs::write(
            directory.path().join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 2022.3",
        )
        .unwrap();
        directory
    }

    #[test]
    fn detects_unity_project_only_when_unity_mcp_is_selected() {
        let project = unity_project();
        let extra = json!({
            "workspace": project.path(),
            "mcp_servers": ["MindNProgress", "unityMCP"]
        });
        let resource = unity_turn_resource_from_extra(&extra.to_string()).expect("Unity resource");
        assert!(resource.key.starts_with("unity:"));
        assert!(
            resource
                .project_root
                .ends_with(project.path().file_name().unwrap().to_str().unwrap())
        );

        let without_mcp = json!({ "workspace": project.path(), "mcp_servers": ["MindNProgress"] });
        assert!(unity_turn_resource_from_extra(&without_mcp.to_string()).is_none());
    }

    #[tokio::test]
    async fn serializes_turns_for_the_same_unity_project() {
        let project = unity_project();
        let extra = json!({ "workspace": project.path(), "mcp_servers": ["unityMCP"] }).to_string();
        let coordinator = UnityTurnCoordinator::default();
        let first = coordinator
            .claim_for_conversation_extra(&extra)
            .unwrap()
            .try_acquire()
            .ok()
            .expect("first permit");
        let waiting = coordinator.claim_for_conversation_extra(&extra).unwrap();
        let waiting = waiting.try_acquire().err().expect("second turn must wait");
        let waiter = tokio::spawn(async move { waiting.wait().await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiting turn timed out")
            .unwrap();
        drop(second);
    }

    #[tokio::test]
    async fn allows_different_unity_projects_to_run_concurrently() {
        let first_project = unity_project();
        let second_project = unity_project();
        let extra = |path: &Path| json!({ "workspace": path, "mcp_servers": ["unityMCP"] }).to_string();
        let coordinator = UnityTurnCoordinator::default();
        let first = coordinator
            .claim_for_conversation_extra(&extra(first_project.path()))
            .unwrap()
            .try_acquire()
            .ok()
            .expect("first permit");
        let second = coordinator
            .claim_for_conversation_extra(&extra(second_project.path()))
            .unwrap()
            .try_acquire()
            .ok()
            .expect("different project permit");
        drop((first, second));
    }
}
