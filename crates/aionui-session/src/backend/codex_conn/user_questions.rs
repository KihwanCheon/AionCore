//! Non-blocking questions from Codex's request_user_input_async.
//! Verified: codex-cli 0.162.1 generated ItemStartedNotification / ItemCompletedNotification
//! schemas, and the adjacent real thread/read item (see its provenance in the test).

use crate::{UserInputQuestion, UserQuestionData};
use serde_json::Value;

pub(super) fn from_item(item: &Value) -> Option<UserQuestionData> {
    if item.get("type")?.as_str()? != "agentMessage" || item.get("delivery")?.as_str()? != "async" {
        return None;
    }
    let item_id = item.get("id")?.as_str()?.to_owned();
    if item_id.is_empty() {
        return None;
    }
    let questions: Vec<UserInputQuestion> = serde_json::from_value(item.get("questions")?.clone()).ok()?;
    if questions.is_empty() || questions.iter().any(|question| question.title.trim().is_empty()) {
        return None;
    }
    Some(UserQuestionData { item_id, questions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEvent;
    use serde_json::json;

    // Actual codex-cli 0.162.1 app-server thread/read response, captured 2026-10-15.
    // Thread 01a134cc-e443-7fe3-b8ea-4862e34708f8; rollout line 5626 also records
    // item_completed with this same item id, delivery and questions. The app-server
    // schema supplies the notification envelope; this is not a captured started frame.
    fn captured_item() -> Value {
        serde_json::from_str(include_str!("user_question_01621.json")).unwrap()
    }

    #[test]
    fn async_question_captured_item_emits_once_without_duplicate_prose() {
        let params = json!({ "item": captured_item() });
        assert!(super::super::map_item(&params, false).is_empty());
        let events = super::super::map_item(&params, true);
        let [SessionEvent::UserQuestion(data)] = events.as_slice() else {
            panic!("expected a structured question: {events:?}")
        };
        assert_eq!(data.item_id, "call_de4ec6f871324bdd9b681f9cd90d0892");
        assert_eq!(
            data.questions[0].title,
            "자동 완전 삭제 대상에 보관된 Teams와 그 하위 대화도 포함할까요?"
        );
        assert_eq!(data.questions[0].options.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn async_question_multiple_questions_and_free_text_are_preserved() {
        let item = json!({"type":"agentMessage", "delivery":"async", "id":"q", "questions":[
            {"title":"첫 질문", "options":["선택 1", "선택 2"]},
            {"title":"자유 입력", "options":null},
            {"title":"선택지 생략"}
        ]});
        let data = from_item(&item).unwrap();
        assert_eq!(data.questions.len(), 3);
        assert!(data.questions[1].options.is_none());
        assert!(data.questions[2].options.is_none());
    }

    #[test]
    fn async_question_counts_as_output_without_opening_a_blocking_request() {
        let event = SessionEvent::UserQuestion(from_item(&captured_item()).unwrap());
        let (running, _) = crate::step(&crate::SessionState::Idle, SessionEvent::TurnStarted { epoch: 1 });
        let (state, _) = crate::step(&running, event.clone());
        assert!(matches!(
            state,
            crate::SessionState::Running {
                saw_substantive_output: true,
                ..
            }
        ));
        assert!(!crate::is_requires_action(&state));
        assert_eq!(crate::classify(&event), crate::EventClass::BackendProduced);
        assert_eq!(crate::persist_tier(&event), crate::PersistTier::Display);
    }

    #[test]
    fn async_question_invalid_metadata_preserves_ordinary_text_path() {
        for questions in [
            json!(null),
            json!([]),
            json!([{"title":" "}]),
            json!([{"title":"Q", "options":[1]}]),
        ] {
            let mut item = captured_item();
            item["questions"] = questions;
            assert!(from_item(&item).is_none());
            assert!(
                super::super::map_item(&json!({"item":item}), false)
                    .iter()
                    .any(|event| matches!(event, SessionEvent::MessageDelta { .. }))
            );
        }
        let mut item = captured_item();
        item["delivery"] = Value::Null;
        assert!(from_item(&item).is_none());
        item["delivery"] = json!("async");
        item["id"] = json!("");
        assert!(from_item(&item).is_none());
    }
}
