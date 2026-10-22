//! Fork-only prose-question notification contract. This does not grant permissions,
//! pause a turn, or resolve a notification; dismissal belongs to the user.

use crate::types::SendMessageData;

const INSTRUCTIONS: &str = r#"[AionUi choice notifications]
When you need an actual choice, approval, or answer from the user, put [[AIONUI_USER_CHOICE_REQUIRED]] alone on a line immediately BEFORE the question. Follow with the question and any choices in the user's language.

Write the entire response, including the question and any choices, in the language explicitly requested by the user; otherwise follow the language of the current conversation. The language used by these instructions does not select or change the response language.

Approval requests include permission to start implementation, apply a proposed change, or perform the next action after presenting a plan, review, or progress report.
If your final response introduces a new approval request that must be answered before you can proceed, put the marker immediately before an explicit approval question, even when there are no multiple-choice options.
State exactly which action and scope need approval. Do not end only with an indirect statement such as "I will proceed once you approve" or "Let me know if I should continue".
Before ending a turn to await the user's answer to a new prose approval or decision question, check that the question has the marker. If the question was already marked earlier, do not repeat it merely as a reminder.
Do not create a new approval requirement for work the user has already authorized.

Use this marker only for a genuine new question, never in quotes, code, examples, rhetorical questions, or repeated reminders. Do not duplicate a structured question tool with this marker. Do not emit a resolution marker: only the user dismisses the notification. Do not execute actions that depend on an unanswered choice; independent already-authorized work may continue.
[/AionUi choice notifications]"#;

/// Upper bound for turns that never reach `release_user_choice_notice_for_turn`
/// (for example a dispatch whose turn failed before an outcome was returned).
const MAX_SUPPRESSED_TURNS: usize = 256;

/// Turn ids of MindNProgress child delegation turns only. Their final answer is
/// captured as the dispatch result for another AI, so the user-facing
/// approval-marker contract must not be injected into those prompts. Parent
/// wakes, integration/recovery wakes, group document instructions and Dooray
/// response resumes face the user and are never registered here.
static SUPPRESSED_TURNS: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// Exclude every outbound prompt of `turn_id` (initial send and automatic
/// continuations) from the choice-notification instruction. Call before the
/// agent receives the turn's first message.
pub fn suppress_user_choice_notice_for_turn(turn_id: &str) {
    let mut turns = SUPPRESSED_TURNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    push_bounded_turn(&mut turns, turn_id, MAX_SUPPRESSED_TURNS);
}

fn push_bounded_turn(turns: &mut std::collections::VecDeque<String>, turn_id: &str, capacity: usize) {
    if turns.iter().any(|existing| existing == turn_id) {
        return;
    }
    while turns.len() >= capacity {
        turns.pop_front();
    }
    turns.push_back(turn_id.to_owned());
}

/// Forget a suppressed turn once it has finished.
pub fn release_user_choice_notice_for_turn(turn_id: &str) {
    let mut turns = SUPPRESSED_TURNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    turns.retain(|existing| existing != turn_id);
}

fn is_user_choice_notice_suppressed(turn_id: Option<&str>) -> bool {
    let Some(turn_id) = turn_id else {
        return false;
    };
    SUPPRESSED_TURNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|existing| existing == turn_id)
}

/// Add the notification instruction only to the outbound agent prompt, never
/// to the persisted user message. Native slash commands must stay byte-identical.
pub(crate) fn with_user_choice_notice(mut data: SendMessageData) -> SendMessageData {
    if !data.content.trim_start().starts_with('/') {
        data.content.push_str("\n\n");
        data.content.push_str(INSTRUCTIONS);
    }
    data
}

/// Turn-start variant: externally dispatched turns stay unchanged. Mid-turn
/// deliveries come from the user and keep using [`with_user_choice_notice`].
pub(crate) fn with_turn_user_choice_notice(data: SendMessageData) -> SendMessageData {
    if is_user_choice_notice_suppressed(data.turn_id.as_deref()) {
        return data;
    }
    with_user_choice_notice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(content: &str) -> SendMessageData {
        SendMessageData {
            content: content.to_owned(),
            msg_id: "message-1".into(),
            turn_id: Some("turn-1".into()),
            files: vec!["attachment.png".into()],
            inject_skills: vec!["skill-1".into()],
        }
    }

    #[test]
    fn ordinary_prompt_receives_registration_only_instructions() {
        let original = input("검증을 진행해줘.");
        let result = with_user_choice_notice(original.clone());
        assert!(result.content.starts_with(&original.content));
        assert!(result.content.contains("[[AIONUI_USER_CHOICE_REQUIRED]]"));
        assert!(!result.content.contains("AIONUI_USER_CHOICE_RESOLVED"));
        assert_eq!(original.content, "검증을 진행해줘.");
    }

    #[test]
    fn slash_commands_remain_byte_identical() {
        for command in ["/usage", " /compact", "/model codex", "/clear"] {
            assert_eq!(with_user_choice_notice(input(command)).content, command);
        }
    }

    #[test]
    fn correlation_and_attachments_are_unchanged() {
        let original = input("");
        let result = with_user_choice_notice(original.clone());
        assert_eq!(result.msg_id, original.msg_id);
        assert_eq!(result.turn_id, original.turn_id);
        assert_eq!(result.files, original.files);
        assert_eq!(result.inject_skills, original.inject_skills);
        assert!(result.content.contains("only the user dismisses"));
    }

    // Verify the real dispatch seam, not just this helper's self-consistency.
    struct CaptureAgent {
        sent: std::sync::Mutex<Vec<SendMessageData>>,
        events: tokio::sync::broadcast::Sender<crate::protocol::events::AgentStreamEvent>,
    }

    #[async_trait::async_trait]
    impl crate::agent_task::IAgentTask for CaptureAgent {
        fn agent_type(&self) -> aionui_common::AgentType {
            aionui_common::AgentType::Acp
        }
        fn conversation_id(&self) -> &str {
            "choice-test"
        }
        fn workspace(&self) -> &str {
            "/choice-test"
        }
        fn status(&self) -> Option<aionui_common::ConversationStatus> {
            None
        }
        fn last_activity_at(&self) -> aionui_common::TimestampMs {
            0
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::protocol::events::AgentStreamEvent> {
            self.events.subscribe()
        }
        async fn send_message(&self, data: SendMessageData) -> Result<(), crate::protocol::send_error::AgentSendError> {
            self.sent.lock().unwrap().push(data);
            Ok(())
        }
        async fn cancel(&self) -> Result<(), crate::error::AgentError> {
            Ok(())
        }
        fn kill(&self, _reason: Option<aionui_common::AgentKillReason>) -> Result<(), crate::error::AgentError> {
            Ok(())
        }
    }

    impl crate::agent_task::IMockAgent for CaptureAgent {}

    #[test]
    fn externally_dispatched_turn_is_left_unchanged_until_released() {
        let mut dispatched = input("위임 작업을 수행해줘.");
        dispatched.turn_id = Some("turn-dispatch-suppressed".into());
        suppress_user_choice_notice_for_turn("turn-dispatch-suppressed");
        let result = with_turn_user_choice_notice(dispatched.clone());
        assert_eq!(result.content, dispatched.content);

        // Mid-turn deliveries are user messages even inside a dispatched turn.
        assert!(
            with_user_choice_notice(dispatched.clone())
                .content
                .contains(INSTRUCTIONS)
        );

        release_user_choice_notice_for_turn("turn-dispatch-suppressed");
        assert!(with_turn_user_choice_notice(dispatched).content.contains(INSTRUCTIONS));
    }

    #[test]
    fn ordinary_turns_are_not_affected_by_other_suppressed_turns() {
        suppress_user_choice_notice_for_turn("turn-dispatch-other");
        let mut ordinary = input("검증을 진행해줘.");
        ordinary.turn_id = Some("turn-ordinary".into());
        assert!(with_turn_user_choice_notice(ordinary).content.contains(INSTRUCTIONS));
        let mut without_turn = input("검증을 진행해줘.");
        without_turn.turn_id = None;
        assert!(
            with_turn_user_choice_notice(without_turn)
                .content
                .contains(INSTRUCTIONS)
        );
        release_user_choice_notice_for_turn("turn-dispatch-other");
    }

    #[test]
    fn suppressed_turn_registry_is_bounded_and_deduplicated() {
        // Uses a local queue so parallel tests sharing the global registry are unaffected.
        let mut turns = std::collections::VecDeque::new();
        for index in 0..5 {
            push_bounded_turn(&mut turns, &format!("turn-{index}"), 3);
        }
        push_bounded_turn(&mut turns, "turn-4", 3);
        assert_eq!(turns, ["turn-2", "turn-3", "turn-4"]);
    }

    #[tokio::test]
    async fn agent_instance_dispatches_guidance_on_normal_and_midturn_paths() {
        let (events, _) = tokio::sync::broadcast::channel(4);
        let capture = std::sync::Arc::new(CaptureAgent {
            sent: Default::default(),
            events,
        });
        let agent = crate::agent_task::AgentInstance::Mock(capture.clone());
        agent.send_message(input("첫 요청")).await.unwrap();
        agent.deliver_midturn(input("추가 요청")).await.unwrap();
        agent.send_message(input("/usage")).await.unwrap();
        let sent = capture.sent.lock().unwrap();
        assert!(sent[0].content.contains(INSTRUCTIONS));
        assert!(sent[1].content.contains(INSTRUCTIONS));
        assert_eq!(sent[2].content, "/usage");
    }

    #[tokio::test]
    async fn agent_instance_skips_guidance_for_dispatched_turn_but_not_midturn() {
        let (events, _) = tokio::sync::broadcast::channel(4);
        let capture = std::sync::Arc::new(CaptureAgent {
            sent: Default::default(),
            events,
        });
        let agent = crate::agent_task::AgentInstance::Mock(capture.clone());
        let mut dispatched = input("위임 전문");
        dispatched.turn_id = Some("turn-dispatch-instance".into());
        suppress_user_choice_notice_for_turn("turn-dispatch-instance");
        agent.send_message(dispatched.clone()).await.unwrap();
        agent.deliver_midturn(dispatched).await.unwrap();
        release_user_choice_notice_for_turn("turn-dispatch-instance");
        let sent = capture.sent.lock().unwrap();
        assert_eq!(sent[0].content, "위임 전문");
        assert!(sent[1].content.contains(INSTRUCTIONS));
    }
}
