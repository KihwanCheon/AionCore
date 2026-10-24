use super::*;
use aionui_ai_agent::{
    AgentStreamEvent,
    protocol::events::{FinishEventData, TextEventData},
};
use aionui_db::models::ConversationRow;
use aionui_db::{
    IConversationRepository, IUserRepository, MessagePageDirection, MessagePageParams, SqliteConversationRepository,
    SqliteUserRepository, init_database_memory,
};
use aionui_realtime::BroadcastEventBus;
use std::sync::Arc;
use tokio::sync::broadcast;

struct Rig {
    user_id: String,
    other_user_id: String,
    repo: Arc<SqliteConversationRepository>,
    bus: Arc<BroadcastEventBus>,
}

impl Rig {
    async fn new() -> Self {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let user_id = users.create_user("question-owner", "hash").await.unwrap().id;
        let other_user_id = users.create_user("other-owner", "hash").await.unwrap().id;
        let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        repo.create(&ConversationRow {
            id: "conv-q".into(),
            user_id: user_id.clone(),
            name: "question test".into(),
            r#type: "codex".into(),
            extra: "{}".into(),
            model: None,
            status: Some("running".into()),
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
            project_id: None,
            folder_id: None,
            name_source: None,
        })
        .await
        .unwrap();
        Self {
            user_id,
            other_user_id,
            repo,
            bus: Arc::new(BroadcastEventBus::new(64)),
        }
    }

    fn relay(&self, user_id: &str) -> StreamRelay {
        StreamRelay::new(
            "conv-q".into(),
            "assistant-prose".into(),
            "turn-q".into(),
            user_id.into(),
            self.repo.clone(),
            self.bus.clone(),
        )
    }

    async fn messages(&self) -> Vec<MessageRow> {
        self.repo
            .list_messages_page(
                &self.user_id,
                "conv-q",
                &MessagePageParams {
                    limit: 100,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap()
            .items
    }
}

fn captured_question() -> UserQuestionData {
    let item: serde_json::Value = serde_json::from_str(include_str!(
        "../../../aionui-session/src/backend/codex_conn/user_question_01621.json"
    ))
    .unwrap();
    UserQuestionData {
        item_id: item["id"].as_str().unwrap().into(),
        questions: serde_json::from_value(item["questions"].clone()).unwrap(),
    }
}

#[tokio::test]
async fn async_question_relay_persists_anchor_before_notice_and_keeps_following_prose_separate() {
    let rig = Rig::new().await;
    let mut ws = rig.bus.subscribe();
    let (tx, rx) = broadcast::channel(16);
    tx.send(AgentStreamEvent::BackendTurnBound("codex-turn".into()))
        .unwrap();
    tx.send(AgentStreamEvent::Text(TextEventData {
        content: "설명입니다.".into(),
    }))
    .unwrap();
    tx.send(AgentStreamEvent::UserQuestion(captured_question())).unwrap();
    tx.send(AgentStreamEvent::Text(TextEventData {
        content: "독립 작업을 계속합니다.".into(),
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
    let result = rig.relay(&rig.user_id).consume(rx).await;
    assert!(result.attempt.saw_visible_output);

    let notices = rig.repo.list_choice_notices(&rig.user_id).await.unwrap();
    assert_eq!(notices.len(), 1);
    let rows = rig.messages().await;
    assert_eq!(rows.len(), 3, "question must not merge with either prose segment");
    let anchor = rows.iter().find(|row| row.id == notices[0].message_id).unwrap();
    assert_eq!(anchor.backend_turn_id.as_deref(), Some("codex-turn"));
    let content: serde_json::Value = serde_json::from_str(&anchor.content).unwrap();
    assert_eq!(
        content["content"],
        "자동 완전 삭제 대상에 보관된 Teams와 그 하위 대화도 포함할까요?\n- 보관된 일반 대화만 포함 (권장)\n- 보관된 대화와 Teams 모두 포함"
    );
    assert_eq!(notices[0].key, json!(["conv-q", anchor.id, 0]).to_string());
    assert!(
        rig.repo
            .list_choice_notices(&rig.other_user_id)
            .await
            .unwrap()
            .is_empty()
    );

    let mut saw_anchor = false;
    let mut saw_notice = false;
    while let Ok(event) = ws.try_recv() {
        assert_eq!(
            event.data["user_id"], rig.user_id,
            "events must carry the owner's routing identity"
        );
        if event.name == "message.stream" && event.data["msg_id"] == anchor.id {
            assert_eq!(event.data["data"], content);
            assert_eq!(event.data["replace"], true);
            saw_anchor = true;
        }
        if event.name == "conversation.choiceNoticesChanged" {
            assert!(saw_anchor, "publish the anchor before its clickable notice");
            assert_eq!(event.data["items"][0]["message_id"], anchor.id);
            saw_notice = true;
        }
    }
    assert!(saw_notice);
}

#[tokio::test]
async fn async_question_replay_and_user_reply_preserve_manual_dismissal() {
    let rig = Rig::new().await;
    let mut data = captured_question();
    data.questions.push(UserInputQuestion {
        title: "두 번째 질문입니다.".into(),
        options: None,
    });
    assert!(rig.relay(&rig.user_id).persist_user_questions(&data).await);
    let original = rig.repo.list_choice_notices(&rig.user_id).await.unwrap();
    assert_eq!(original.len(), 2);
    let user_reply = MessageRow {
        id: "reply".into(),
        conversation_id: "conv-q".into(),
        msg_id: Some("reply".into()),
        r#type: "text".into(),
        content: json!({"content":"선택 1"}).to_string(),
        position: Some("right".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: now_ms(),
        backend_turn_id: None,
    };
    rig.repo.insert_message(&rig.user_id, &user_reply).await.unwrap();
    assert_eq!(rig.repo.list_choice_notices(&rig.user_id).await.unwrap().len(), 2);
    rig.repo.dismiss_choice_notice(&original[0]).await.unwrap();
    // A fresh relay simulates reconnection/replay; no in-memory dedup is relied on.
    assert!(rig.relay(&rig.user_id).persist_user_questions(&data).await);
    let pending = rig.repo.list_choice_notices(&rig.user_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].key, original[1].key);
    assert_eq!(
        rig.messages().await.len(),
        3,
        "two questions plus the reply, never duplicate bubbles"
    );
}

#[tokio::test]
async fn async_question_cross_user_write_is_rejected_without_event_or_notice() {
    let rig = Rig::new().await;
    let mut ws = rig.bus.subscribe();
    let data = captured_question();
    let error = rig
        .relay(&rig.other_user_id)
        .persist_user_question(&data.item_id, 0, &data.questions[0])
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NotFound(_)), "{error:?}");
    assert!(rig.messages().await.is_empty());
    assert!(rig.repo.list_choice_notices(&rig.user_id).await.unwrap().is_empty());
    assert!(matches!(ws.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
}

#[tokio::test]
async fn async_question_empty_payload_does_not_create_a_notice() {
    let rig = Rig::new().await;
    let data = UserQuestionData {
        item_id: "empty".into(),
        questions: vec![UserInputQuestion {
            title: " \n ".into(),
            options: None,
        }],
    };
    assert!(!rig.relay(&rig.user_id).persist_user_questions(&data).await);
    assert!(rig.messages().await.is_empty());
    assert!(rig.repo.list_choice_notices(&rig.user_id).await.unwrap().is_empty());
}

fn native_question() -> serde_json::Value {
    json!({
        "session_id": "conv-q", "request_id": "native-request",
        "questions": [
            {"question": "배포 범위를 선택해 주세요.", "header": "배포", "multiSelect": false,
             "options": [{"label": "검증 환경", "description": "사전 검증"}, {"label": "운영 환경"}]},
            {"question": "대상 기능을 선택해 주세요.", "multiSelect": true,
             "options": [{"label": "질문 알림"}, {"label": "질문 이동"}]}
        ]
    })
}

#[tokio::test]
async fn native_question_persists_interactive_anchor_before_each_notice_without_a_renderer() {
    let rig = Rig::new().await;
    let mut ws = rig.bus.subscribe();
    let (tx, rx) = broadcast::channel(8);
    tx.send(AgentStreamEvent::Ask(native_question())).unwrap();
    tx.send(AgentStreamEvent::Text(TextEventData {
        content: "다음 설명".into(),
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
    rig.relay(&rig.user_id).consume(rx).await;
    let rows = rig.messages().await;
    let notices = rig.repo.list_choice_notices(&rig.user_id).await.unwrap();
    assert_eq!(notices.len(), 2);
    let anchor = rows.iter().find(|row| row.r#type == "ask").unwrap();
    assert_eq!(rows.len(), 2, "interactive question must not merge into terminal prose");
    let response = crate::convert::row_to_message_response(anchor.clone()).unwrap();
    assert_eq!(response.r#type, aionui_common::MessageType::Ask);
    assert_eq!(response.content["questions"], native_question()["questions"]);
    assert_eq!(response.content["persisted"], true);
    assert!(notices.iter().all(|notice| notice.message_id == anchor.id));
    assert_eq!(
        notices.iter().map(|notice| notice.marker_index).collect::<Vec<_>>(),
        vec![0, 1]
    );
    let mut anchors = 0;
    while let Ok(event) = ws.try_recv() {
        assert_eq!(event.data["user_id"], rig.user_id);
        if event.name == "message.stream" && event.data["type"] == "ask" {
            anchors += 1;
            assert_eq!(event.data["msg_id"], anchor.id);
        }
        if event.name == "conversation.choiceNoticesChanged" {
            assert_eq!(anchors, 1, "a notice must point at an already queryable card");
        }
    }
    assert_eq!(anchors, 1, "do not also forward a second transient question card");
}

#[tokio::test]
async fn native_question_replay_preserves_dismissal_and_answered_or_declined_card() {
    use crate::service::native_questions::record_native_question_answer;
    for answers in [
        Some(vec![
            json!({"question": "배포 범위를 선택해 주세요.", "labels": ["검증 환경"]}),
        ]),
        None,
    ] {
        let rig = Rig::new().await;
        assert!(
            rig.relay(&rig.user_id)
                .persist_native_question(&native_question())
                .await
        );
        let before = rig.repo.list_choice_notices(&rig.user_id).await.unwrap();
        rig.repo.dismiss_choice_notice(&before[0]).await.unwrap();
        let resolution = if answers.is_some() { "answered" } else { "declined" };
        record_native_question_answer(
            rig.repo.as_ref(),
            rig.bus.as_ref(),
            &rig.user_id,
            "conv-q",
            "native-request",
            answers.clone(),
        )
        .await
        .unwrap();
        assert!(
            rig.relay(&rig.user_id)
                .persist_native_question(&native_question())
                .await
        );
        let rows = rig.messages().await;
        assert_eq!(rows.len(), 1, "reopening and replay must retain one card");
        let content: serde_json::Value = serde_json::from_str(&rows[0].content).unwrap();
        assert_eq!(content["resolution"], resolution);
        if let Some(answers) = answers {
            assert_eq!(content["answers"], json!(answers));
        }
        let pending = rig.repo.list_choice_notices(&rig.user_id).await.unwrap();
        assert_eq!(pending.len(), 1, "answering must not dismiss the independent notice");
        assert_eq!(pending[0].key, before[1].key);
    }
}

#[tokio::test]
async fn native_question_rejects_malformed_or_cross_conversation_metadata() {
    let rig = Rig::new().await;
    for payload in [
        json!({}),
        json!({"request_id": "", "questions": []}),
        json!({"request_id": "req", "questions": [{"question": " ", "options": []}]}),
        json!({"request_id": "req", "questions": [{"question": "질문", "options": ["잘못된 선택지"]}]}),
        json!({"session_id": "other-conversation", "request_id": "req", "questions": native_question()["questions"]}),
    ] {
        assert!(!rig.relay(&rig.user_id).persist_native_question(&payload).await);
    }
    assert!(rig.messages().await.is_empty());
    assert!(rig.repo.list_choice_notices(&rig.user_id).await.unwrap().is_empty());
}

#[tokio::test]
async fn native_question_cross_user_cannot_create_or_resolve_another_users_card() {
    use crate::service::native_questions::record_native_question_answer;
    let rig = Rig::new().await;
    let mut ws = rig.bus.subscribe();
    assert!(
        !rig.relay(&rig.other_user_id)
            .persist_native_question(&native_question())
            .await
    );
    assert!(rig.messages().await.is_empty());
    assert!(matches!(ws.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    assert!(
        rig.relay(&rig.user_id)
            .persist_native_question(&native_question())
            .await
    );
    record_native_question_answer(
        rig.repo.as_ref(),
        rig.bus.as_ref(),
        &rig.other_user_id,
        "conv-q",
        "native-request",
        None,
    )
    .await
    .unwrap();
    let rows = rig.messages().await;
    let content: serde_json::Value = serde_json::from_str(&rows[0].content).unwrap();
    assert!(content.get("resolution").is_none());
    assert!(
        rig.repo
            .list_choice_notices(&rig.other_user_id)
            .await
            .unwrap()
            .is_empty()
    );
}
