//! Scope app-server notifications to the conversation's thread and active turn.
//!
//! Verified contract: codex-cli 0.162.1 generated TurnStartedNotification,
//! TurnCompletedNotification and ThreadStatusChangedNotification schemas carry
//! threadId; thread/started carries thread.id. The regression fixtures model
//! native parent/child completion ordering observed on 2026-10-08 and 2026-10-20;
//! they are synthetic envelopes, not captured JSON-RPC traffic.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IgnoreReason {
    ForeignThread,
    StaleTurn,
}

pub(super) fn notification_thread_id<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    if method == "thread/started" {
        params.get("thread")?.get("id")?.as_str()
    } else {
        params.get("threadId")?.as_str()
    }
}

pub(super) fn ignore_reason(
    method: &str,
    params: &Value,
    bound_thread: Option<&str>,
    active_turn: Option<&str>,
) -> Option<IgnoreReason> {
    // Reverse RPC approvals can belong to a child. Their resolution must still
    // clear the matching request surfaced by handle_reverse_rpc.
    if method == "serverRequest/resolved" {
        return None;
    }
    if let (Some(bound), Some(incoming)) = (bound_thread, notification_thread_id(method, params))
        && bound != incoming
    {
        return Some(IgnoreReason::ForeignThread);
    }
    // A new turn on the owning thread is allowed to replace the previous turn.
    // Connection-wide / legacy frames without IDs keep their existing handling.
    if method != "turn/started" {
        let incoming_turn = params
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| params.get("turn")?.get("id")?.as_str());
        if let (Some(active), Some(incoming)) = (active_turn, incoming_turn)
            && active != incoming
        {
            return Some(IgnoreReason::StaleTurn);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::tests::drive_codex;
    use super::*;
    use crate::{CancelTarget, CodexSessionBackend, Command, SessionBackend, SessionEvent, testing::FakeAgentIo};
    use futures_util::StreamExt;
    use serde_json::json;

    const PARENT: &str = r#"{"method":"thread/started","params":{"thread":{"id":"parent"}}}"#;
    const START: &str =
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"parent-turn","status":"inProgress"}}}"#;
    const TEXT: &str = r#"{"method":"item/agentMessage/delta","params":{"threadId":"parent","turnId":"parent-turn","itemId":"parent-message","delta":"parent continues"}}"#;
    const DONE: &str = r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"parent-turn","status":"completed"}}}"#;
    const CHILD_DONE: &str =
        r#"{"method":"turn/completed","params":{"threadId":"child","turn":{"id":"child-turn","status":"completed"}}}"#;

    #[tokio::test]
    async fn child_completion_does_not_finish_parent_before_remaining_output() {
        let events = drive_codex(&[PARENT, START, CHILD_DONE, TEXT, DONE]).await;
        let text = events
            .iter()
            .position(|event| matches!(event, SessionEvent::MessageDelta { .. }))
            .unwrap();
        let terminals = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| matches!(event, SessionEvent::TurnResult { .. }).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert!(
            terminals[0] > text,
            "child completion must not terminate the parent: {events:?}"
        );
    }

    #[tokio::test]
    async fn parent_failure_survives_child_success() {
        let events = drive_codex(&[
            PARENT, START, CHILD_DONE, TEXT,
            r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"parent-turn","status":"failed","error":{"message":"parent failure"}}}}"#,
        ]).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnResult { is_error: true, .. })),
            "the authoritative parent failure must survive child success: {events:?}"
        );
    }

    #[tokio::test]
    async fn child_notifications_do_not_rebind_or_contaminate_parent() {
        let events = drive_codex(&[
            PARENT, START,
            r#"{"method":"thread/started","params":{"thread":{"id":"child"}}}"#,
            r#"{"method":"turn/started","params":{"threadId":"child","turn":{"id":"child-turn","status":"inProgress"}}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"threadId":"child","turnId":"child-turn","itemId":"child-message","delta":"child-only text"}}"#,
            r#"{"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"systemError"}}}"#,
            r#"{"method":"error","params":{"threadId":"child","turnId":"child-turn","willRetry":false,"error":{"message":"child-only failure"}}}"#,
            CHILD_DONE, TEXT, DONE,
        ]).await;
        let threads: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::BackendBound {
                    backend_session_id: Some(id),
                } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let turns: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::BackendTurnBound { backend_turn_id } => Some(backend_turn_id.as_str()),
                _ => None,
            })
            .collect();
        let text: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::MessageDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(threads, ["parent"]);
        assert_eq!(turns, ["parent-turn"]);
        assert_eq!(text, ["parent continues"]);
    }

    #[tokio::test]
    async fn stale_parent_completion_does_not_close_new_parent_turn() {
        let events = drive_codex(&[
            PARENT,
            r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"old-turn"}}}"#,
            START,
            r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"old-turn","status":"completed"}}}"#,
            TEXT, DONE,
        ]).await;
        let text = events
            .iter()
            .position(|event| matches!(event, SessionEvent::MessageDelta { .. }))
            .unwrap();
        let terminal = events
            .iter()
            .position(|event| matches!(event, SessionEvent::TurnResult { .. }))
            .unwrap();
        assert!(
            terminal > text,
            "a stale completion must not finish the active turn: {events:?}"
        );
    }

    #[tokio::test]
    async fn child_completion_preserves_parent_interrupt_target() {
        let frames = [
            PARENT,
            START,
            r#"{"method":"thread/started","params":{"thread":{"id":"child"}}}"#,
            r#"{"method":"turn/started","params":{"threadId":"child","turn":{"id":"child-turn"}}}"#,
            CHILD_DONE,
            TEXT,
        ];
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_segments(vec![
            format!("{}\n", frames.join("\n")).into_bytes(),
            format!("{DONE}\n").into_bytes(),
        ]);
        let release = fake.segment_releaser();
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("parent-conversation", Box::new(fake)).await;
        let mut events = backend.events();
        release();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.event, SessionEvent::MessageDelta { .. }) {
                    return;
                }
            }
            panic!("reader closed before parent output");
        })
        .await
        .expect("reader processed child completion");
        backend
            .dispatch(Command::Cancel {
                target: CancelTarget::Turn,
            })
            .await
            .expect("interrupt sent");
        let wire = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let bytes = captured.lock().await.clone();
                if bytes.ends_with(b"\n") {
                    break serde_json::from_slice::<Value>(&bytes).unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("interrupt must be written");
        assert_eq!(wire["method"], "turn/interrupt");
        assert_eq!(wire["params"], json!({"threadId":"parent","turnId":"parent-turn"}));
        release();
    }

    #[tokio::test]
    async fn successive_parent_turns_each_keep_their_completion() {
        let events = drive_codex(&[
            PARENT, START, TEXT, DONE,
            r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"next-turn"}}}"#,
            CHILD_DONE,
            r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"next-turn","status":"completed"}}}"#,
        ]).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SessionEvent::TurnResult { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn child_approval_resolution_and_unscoped_notifications_remain_available() {
        assert_eq!(
            ignore_reason(
                "serverRequest/resolved",
                &json!({"threadId":"child","requestId":7}),
                Some("parent"),
                Some("turn")
            ),
            None
        );
        assert_eq!(
            ignore_reason("account/updated", &json!({}), Some("parent"), Some("turn")),
            None
        );
    }
}
