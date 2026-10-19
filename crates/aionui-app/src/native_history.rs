//! Native Codex history deletion, isolated from the conversation domain.
//!
//! Verified contract: codex 0.162.1 generated ClientRequest.json and
//! v2/{ThreadListParams,ThreadDeleteParams,ThreadListResponse}.json.
//! A thread deletion includes spawned descendants, so every retained local
//! resume/fork anchor is checked against the native graph before mutation.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::{AgentRegistry, session_agent::resolve_session_cli_program};
use aionui_api_types::AgentMetadata;
use aionui_conversation::ConversationError;
use aionui_conversation::service::native_history::{NativeHistoryCleaner, NativeHistoryOutcome};
use aionui_db::models::AcpSessionRow;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

pub(crate) struct CodexHistoryCleaner {
    registry: Arc<AgentRegistry>,
}

impl CodexHistoryCleaner {
    pub(crate) fn new(registry: Arc<AgentRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl NativeHistoryCleaner for CodexHistoryCleaner {
    async fn delete_codex_history(
        &self,
        user_id: &str,
        session: &AcpSessionRow,
        protected_session_ids: &[String],
    ) -> Result<NativeHistoryOutcome, ConversationError> {
        let thread_id = session
            .session_id
            .as_deref()
            .ok_or_else(|| failure("missing session anchor"))?;
        uuid::Uuid::parse_str(thread_id).map_err(|_| failure("invalid session anchor"))?;
        if protected_session_ids.iter().any(|id| id == thread_id) {
            return Ok(NativeHistoryOutcome::RetainedReference);
        }
        let metadata = self
            .registry
            .get_for_user(user_id, &session.agent_id)
            .await
            .map_err(|_| failure("agent lookup failed"))?
            .ok_or_else(|| failure("agent metadata is missing"))?;
        delete_native_thread(&metadata, thread_id, protected_session_ids).await
    }
}

fn failure(reason: &str) -> ConversationError {
    ConversationError::Internal {
        reason: format!("Codex history cleanup failed ({reason}); conversation was kept"),
    }
}

struct Rpc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Rpc {
    fn spawn(metadata: &AgentMetadata) -> Result<Self, ConversationError> {
        let program = resolve_session_cli_program("codex", metadata).unwrap_or_else(|| "codex".into());
        let mut command = aionui_runtime::Builder::clean_cli(program);
        command
            .args(["app-server", "--stdio"])
            .envs(metadata.env.iter().map(|entry| (&entry.name, &entry.value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|_| failure("could not start Codex"))?;
        let stdin = child.stdin.take().ok_or_else(|| failure("missing stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| failure("missing stdout"))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
        })
    }

    async fn send(&mut self, value: Value) -> Result<(), ConversationError> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| failure("invalid request"))?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|_| failure("request write failed"))?;
        self.stdin.flush().await.map_err(|_| failure("request flush failed"))
    }

    async fn request_frame(&mut self, method: &str, params: Value) -> Result<Value, ConversationError> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"id": id, "method": method, "params": params})).await?;
        loop {
            let mut line = String::new();
            if self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|_| failure("response read failed"))?
                == 0
            {
                return Err(failure("Codex closed the connection"));
            }
            if line.len() > 8 * 1024 * 1024 {
                return Err(failure("response exceeded the metadata limit"));
            }
            let frame: Value = serde_json::from_str(&line).map_err(|_| failure("invalid response"))?;
            if frame.get("id").and_then(Value::as_u64) != Some(id) {
                // Do not acknowledge reverse-RPC or tool approval requests. This
                // connection only reads metadata and deletes persisted history.
                continue;
            }
            return Ok(frame);
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ConversationError> {
        let frame = self.request_frame(method, params).await?;
        response_result(method, &frame)
    }

    async fn read_thread(&mut self, id: &str) -> Result<Option<Value>, ConversationError> {
        let frame = self
            .request_frame("thread/read", json!({"threadId": id, "includeTurns": false}))
            .await?;
        // Verified against native 0.162.1 in the isolated live fixture: a
        // missing persisted thread returns this exact scoped error. Do not
        // interpret unrelated invalid-request errors as successful deletion.
        if frame["error"]["code"].as_i64() == Some(-32600)
            && frame["error"]["message"].as_str() == Some(format!("thread not loaded: {id}").as_str())
        {
            return Ok(None);
        }
        let result = response_result("thread/read", &frame)?;
        let thread = result
            .get("thread")
            .filter(|thread| thread["id"].as_str() == Some(id))
            .ok_or_else(|| failure("invalid thread metadata"))?;
        Ok(Some(thread.clone()))
    }

    async fn initialize(&mut self) -> Result<(), ConversationError> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {"name": "aionui_history_cleanup", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )
        .await?;
        self.send(json!({"method": "initialized", "params": {}})).await
    }

    async fn list_threads(&mut self) -> Result<Vec<Value>, ConversationError> {
        let mut threads = Vec::new();
        for archived in [false, true] {
            let mut cursor: Option<String> = None;
            let mut cursors = HashSet::new();
            loop {
                let page = self
                    .request(
                        "thread/list",
                        json!({
                            "archived": archived,
                            "cursor": cursor,
                            "limit": 200,
                            "modelProviders": [],
                            "sourceKinds": ["cli", "vscode", "exec", "appServer", "subAgent", "subAgentReview",
                                            "subAgentCompact", "subAgentThreadSpawn", "subAgentOther", "unknown"]
                        }),
                    )
                    .await?;
                let data = page
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(|| failure("invalid thread list"))?;
                for thread in data {
                    if thread.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
                        return Err(failure("thread metadata is missing an id"));
                    }
                    threads.push(thread.clone());
                }
                match page.get("nextCursor") {
                    Some(Value::Null) => break,
                    Some(Value::String(next)) if !next.is_empty() && cursors.insert(next.clone()) => {
                        cursor = Some(next.clone());
                    }
                    _ => return Err(failure("invalid thread pagination")),
                }
            }
        }
        Ok(threads)
    }
}

fn response_result(method: &str, frame: &Value) -> Result<Value, ConversationError> {
    if let Some(error) = frame.get("error") {
        tracing::warn!(rpc_method = method, rpc_code = ?error.get("code"), "Codex history RPC failed");
        return Err(failure("Codex rejected the history request"));
    }
    frame
        .get("result")
        .cloned()
        .ok_or_else(|| failure("missing response result"))
}

fn is_retained_fork_error(frame: &Value, thread_id: &str) -> bool {
    // Native 0.162.1 also protects copy-on-write history references that need
    // not appear as visible thread lineage. Captured in the cleanup fixture's
    // last-native-error.json; retain the source instead of bypassing this guard.
    frame["error"]["code"].as_i64() == Some(-32600)
        && frame["error"]["message"].as_str()
            == Some(format!("cannot delete thread {thread_id}: forked history still references it").as_str())
}

/// Includes fork ancestry conservatively: even a lazy local fork must keep its
/// source available until it has acquired an independent native session.
fn has_protected_descendant(threads: &[Value], root: &str, protected: &[String]) -> bool {
    let protected: HashSet<&str> = protected.iter().map(String::as_str).collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for thread in threads {
        let Some(id) = thread.get("id").and_then(Value::as_str) else {
            continue;
        };
        for field in ["parentThreadId", "forkedFromId"] {
            if let Some(parent) = thread.get(field).and_then(Value::as_str) {
                children.entry(parent).or_default().push(id);
            }
        }
    }
    let mut pending = vec![root];
    let mut visited = HashSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        if protected.contains(id) {
            return true;
        }
        if let Some(next) = children.get(id) {
            pending.extend(next.iter().copied());
        }
    }
    false
}

async fn delete_native_thread(
    metadata: &AgentMetadata,
    thread_id: &str,
    protected: &[String],
) -> Result<NativeHistoryOutcome, ConversationError> {
    let mut rpc = Rpc::spawn(metadata)?;
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        rpc.initialize().await?;
        let mut threads = rpc.list_threads().await?;
        let root = rpc.read_thread(thread_id).await?;
        // Lists can omit persisted empty chats. Read retained anchors absent
        // from the list and their ancestry before authorizing a recursive delete.
        let mut known: HashSet<String> = threads
            .iter()
            .filter_map(|t| t["id"].as_str().map(str::to_owned))
            .collect();
        let mut pending = protected
            .iter()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            .cloned()
            .collect::<Vec<_>>();
        if let Some(root) = root {
            if known.insert(thread_id.to_owned()) {
                threads.push(root);
            }
        } else if !known.contains(thread_id) {
            return Ok(NativeHistoryOutcome::Deleted);
        }
        let mut checked = HashSet::new();
        while let Some(id) = pending.pop() {
            if !checked.insert(id.clone()) {
                continue;
            }
            let listed = threads
                .iter()
                .find(|thread| thread["id"].as_str() == Some(&id))
                .cloned();
            let thread = match listed {
                Some(thread) => Some(thread),
                None => rpc.read_thread(&id).await?,
            };
            if let Some(thread) = thread {
                for field in ["parentThreadId", "forkedFromId"] {
                    if let Some(parent) = thread[field].as_str() {
                        pending.push(parent.to_owned());
                    }
                }
                if known.insert(id) {
                    threads.push(thread);
                }
            }
        }
        if has_protected_descendant(&threads, thread_id, protected) {
            return Ok(NativeHistoryOutcome::RetainedReference);
        }
        let reply = rpc
            .request_frame("thread/delete", json!({"threadId": thread_id}))
            .await?;
        if is_retained_fork_error(&reply, thread_id) {
            return Ok(NativeHistoryOutcome::RetainedReference);
        }
        response_result("thread/delete", &reply)?;
        if rpc.read_thread(thread_id).await?.is_some() {
            return Err(failure("thread is still present after deletion"));
        }
        Ok(NativeHistoryOutcome::Deleted)
    })
    .await
    .unwrap_or_else(|_| Err(failure("history request timed out")));
    if let Err(error) = aionui_runtime::kill_process_tree(&mut rpc.child).await {
        tracing::warn!(error_kind = ?error.kind(), "Could not reap the Codex history helper");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepts_the_captured_scoped_fork_reference_guard() {
        let frame = json!({"error":{"code":-32600,
            "message":"cannot delete thread source: forked history still references it"}});
        assert!(is_retained_fork_error(&frame, "source"));
        assert!(!is_retained_fork_error(&frame, "different-source"));
        assert!(!is_retained_fork_error(
            &json!({"error":{"code":-32600,"message":"database locked"}}),
            "source"
        ));
    }

    #[test]
    fn protects_spawned_archived_and_forked_descendants() {
        let threads = vec![
            json!({"id":"root"}),
            json!({"id":"child","parentThreadId":"root"}),
            json!({"id":"fork","forkedFromId":"child"}),
        ];
        assert!(has_protected_descendant(&threads, "root", &["fork".into()]));
        assert!(has_protected_descendant(&threads, "root", &["root".into()]));
        assert!(!has_protected_descendant(&threads, "root", &["unrelated".into()]));
    }

    #[test]
    fn graph_walk_terminates_on_duplicate_edges_and_cycles() {
        let threads = vec![
            json!({"id":"a","parentThreadId":"b"}),
            json!({"id":"b","parentThreadId":"a","forkedFromId":"a"}),
        ];
        assert!(!has_protected_descendant(&threads, "a", &[]));
    }

    #[tokio::test]
    #[ignore = "uses an isolated native Codex fixture; no model request"]
    async fn live_codex_history_cleanup() {
        let home = std::env::var("AIONUI_CODEX_CLEANUP_LIVE_HOME").expect("isolated fixture home");
        let target = std::env::var("AIONUI_CODEX_CLEANUP_LIVE_THREAD").expect("fixture thread");
        let keep = std::env::var("AIONUI_CODEX_CLEANUP_LIVE_KEEP").expect("retained fixture thread");
        assert!(std::path::Path::new(&home).join(".cleanup-fixture").is_file());
        let mut metadata: AgentMetadata = serde_json::from_value(json!({
            "id":"cleanup-fixture", "name":"Codex", "agent_type":"acp", "agent_source":"builtin",
            "enabled":true, "available":true, "sort_order":0
        }))
        .unwrap();
        metadata.env = vec![aionui_api_types::AgentEnvEntry {
            name: "CODEX_HOME".into(),
            value: home.clone(),
            description: None,
        }];
        let mut probe = Rpc::spawn(&metadata).unwrap();
        probe.initialize().await.unwrap();
        probe.list_threads().await.unwrap();
        let target_meta = probe
            .read_thread(&target)
            .await
            .unwrap()
            .expect("fixture target must exist");
        let keep_meta = probe
            .read_thread(&keep)
            .await
            .unwrap()
            .expect("retained fixture must exist");
        let target_path = dunce::canonicalize(target_meta["path"].as_str().unwrap()).unwrap();
        let keep_path = dunce::canonicalize(keep_meta["path"].as_str().unwrap()).unwrap();
        let fixture_root = dunce::canonicalize(&home).unwrap();
        assert!(target_path.starts_with(&fixture_root) && keep_path.starts_with(&fixture_root));
        aionui_runtime::kill_process_tree(&mut probe.child).await.unwrap();
        assert_eq!(
            delete_native_thread(&metadata, &target, std::slice::from_ref(&target))
                .await
                .unwrap(),
            NativeHistoryOutcome::RetainedReference
        );
        assert!(target_path.is_file(), "a retained native history must remain on disk");
        assert_eq!(
            delete_native_thread(&metadata, &target, std::slice::from_ref(&keep))
                .await
                .unwrap(),
            NativeHistoryOutcome::Deleted
        );
        assert!(
            !target_path.exists(),
            "success must remove the actual rollout, not only its UI entry"
        );
        assert!(keep_path.is_file(), "unrelated native history must survive");
        assert_eq!(
            delete_native_thread(&metadata, &target, &[keep]).await.unwrap(),
            NativeHistoryOutcome::Deleted
        );
    }
}
