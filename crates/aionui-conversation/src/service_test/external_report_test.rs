use super::*;
use aionui_api_types::ExternalConversationReportRequest;
use aionui_db::SqliteConversationRepository;

fn report() -> ExternalConversationReportRequest {
    ExternalConversationReportRequest {
        operation_id: "mnp-report-1".into(),
        content: "# 완료 전문\n검증 결과 원문".into(),
    }
}

fn service(
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<MockBroadcaster>,
    tasks: Arc<MockTaskManager>,
) -> ConversationService {
    ConversationService::new(
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        tasks,
        repo,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
    )
}

#[tokio::test]
async fn archived_conversation_rejects_all_incoming_and_runtime_entry_points() {
    let db = init_database_memory().await.unwrap();
    seed_test_user(db.pool(), "user_1").await;
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let events = Arc::new(MockBroadcaster::new());
    let tasks = Arc::new(MockTaskManager::new());
    let svc = service(repo.clone(), events.clone(), tasks.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let before = svc.append_external_report("user_1", &conv.id, report()).await.unwrap();
    sqlx::query("UPDATE conversations SET archived_at = 1 WHERE id = ?")
        .bind(&conv.id)
        .execute(db.pool())
        .await
        .unwrap();
    events.take_events();
    let manager: Arc<dyn IWorkerTaskManager> = tasks.clone();
    let request = serde_json::from_value(json!({"content": "ordinary input", "files": []})).unwrap();
    assert!(matches!(
        svc.send_message("user_1", &conv.id, request, &manager).await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.append_external_report("user_1", &conv.id, report()).await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.warmup("user_1", &conv.id, &manager).await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.ensure_runtime("user_1", &conv.id, &manager).await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.restart_runtime("user_1", &conv.id, &manager).await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.answer_ask("user_1", &conv.id, "ask", None, &manager).await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.confirm(
            "user_1",
            &conv.id,
            "call",
            aionui_api_types::ConfirmRequest {
                msg_id: "message".into(),
                data: json!({"allow": true}),
                always_allow: false,
            },
            &manager
        )
        .await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.handle_side_question(
            "user_1",
            &conv.id,
            aionui_api_types::SideQuestionRequest {
                question: "side question".into(),
            }
        )
        .await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.set_config_option(
            "user_1",
            &conv.id,
            "mode",
            aionui_api_types::SetConfigOptionRequest { value: "auto".into() }
        )
        .await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(matches!(
        svc.run_agent_turn(ConversationAgentTurnRequest {
            user_id: "user_1".into(),
            conversation_id: conv.id.clone(),
            content: "cron input".into(),
            files: vec![],
            inject_skills: vec![],
            required_runtime_mode: None,
            persist_user_message: true,
            user_message_hidden: true,
            on_started: None,
            on_resource_waiting: None,
        })
        .await,
        Err(ConversationError::Archived { .. })
    ));
    assert!(
        events.take_events().is_empty(),
        "rejected sends must not announce successful delivery"
    );
    assert_eq!(tasks.active_count(), 0);
    assert!(
        repo.get_message("user_1", &conv.id, &before.message_id)
            .await
            .unwrap()
            .is_some()
    );
    let dispatcher = crate::external_dispatch::ExternalConversationDispatchService::new(
        svc.clone(),
        Arc::new(aionui_db::fork_extensions::SqliteExternalDispatchRepository::new(
            db.pool().clone(),
        )),
    );
    let request = serde_json::from_value(json!({
        "operationId": "archived-wake", "actorConversationId": conv.id,
        "targetConversationId": conv.id, "strategy": "resume", "instruction": report().content,
        "historyMessageId": before.message_id,
    }))
    .unwrap();
    assert!(matches!(
        dispatcher.dispatch(request).await,
        Err(
            crate::external_dispatch::ExternalConversationDispatchError::Conversation(
                ConversationError::Archived { .. }
            )
        )
    ));
    sqlx::query("UPDATE conversations SET archived_at = NULL WHERE id = ?")
        .bind(&conv.id)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        svc.append_external_report("user_1", &conv.id, report())
            .await
            .unwrap()
            .repeated
    );
}

#[tokio::test]
async fn external_report_is_visible_once_while_busy_and_survives_service_restart() {
    let db = init_database_memory().await.unwrap();
    seed_test_user(db.pool(), "user_1").await;
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let events = Arc::new(MockBroadcaster::new());
    let tasks = Arc::new(MockTaskManager::new());
    let svc = service(repo.clone(), events.clone(), tasks.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let claim = svc.runtime_state().try_claim_turn(&conv.id, "busy-turn").unwrap();
    events.take_events();
    let (first, second) = tokio::join!(
        svc.append_external_report("user_1", &conv.id, report()),
        svc.append_external_report("user_1", &conv.id, report()),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.message_id, second.message_id);
    assert_ne!(first.repeated, second.repeated);
    assert!(!first.execution_requested);
    let row = repo
        .get_message("user_1", &conv.id, &first.message_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.hidden);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.content).unwrap()["content"],
        report().content
    );
    let stream = events.take_events();
    assert_eq!(stream.len(), 1);
    assert_eq!(stream[0].name, "message.stream");
    assert_eq!(stream[0].data["user_id"], "user_1");
    assert_eq!(stream[0].data["hidden"], false);
    assert_eq!(stream[0].data["msg_id"], first.message_id);
    assert_eq!(tasks.active_count(), 0);
    assert_eq!(tasks.kill_count(), 0);
    assert!(svc.runtime_state().try_claim_turn(&conv.id, "unexpected-turn").is_err());
    drop(claim);

    let restarted = service(repo.clone(), events.clone(), tasks.clone());
    let retried = restarted
        .append_external_report("user_1", &conv.id, report())
        .await
        .unwrap();
    assert!(retried.repeated);
    assert_eq!(retried.message_id, first.message_id);
    assert!(events.take_events().is_empty());
    restarted
        .validate_external_report_message("user_1", &conv.id, &first.message_id, &report().content)
        .await
        .unwrap();
    assert!(
        restarted
            .validate_external_report_message("user_1", &conv.id, &first.message_id, "different")
            .await
            .is_err()
    );
    assert!(
        restarted
            .validate_external_report_message("other-user", &conv.id, &first.message_id, &report().content)
            .await
            .is_err()
    );
    let mut conflicting = report();
    conflicting.content = "다른 결과".into();
    assert!(matches!(
        restarted.append_external_report("user_1", &conv.id, conflicting).await,
        Err(ConversationError::Busy { .. })
    ));
    assert_eq!(
        repo.list_messages_page(
            "user_1",
            &conv.id,
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            }
        )
        .await
        .unwrap()
        .items
        .len(),
        1
    );

    // Resuming with the stored report must execute the agent but must not
    // insert a second copy of the user-visible completion transcript.
    let dispatcher = crate::external_dispatch::ExternalConversationDispatchService::new(
        restarted,
        Arc::new(aionui_db::fork_extensions::SqliteExternalDispatchRepository::new(
            db.pool().clone(),
        )),
    );
    let request: aionui_api_types::ExternalConversationDispatchRequest = serde_json::from_value(json!({
        "operationId": "wake-reuse-report", "actorConversationId": conv.id,
        "targetConversationId": conv.id, "strategy": "resume",
        "instruction": report().content, "historyMessageId": first.message_id,
    }))
    .unwrap();
    let response = dispatcher.dispatch(request.clone()).await.unwrap();
    assert_eq!(response.conversation_id, conv.id);
    let mut finished = false;
    for _ in 0..100 {
        let state = dispatcher.status("wake-reuse-report").await.unwrap().unwrap();
        if state.state == aionui_api_types::ExternalConversationDispatchState::Completed {
            finished = true;
            break;
        }
        assert_ne!(
            state.state,
            aionui_api_types::ExternalConversationDispatchState::Failed,
            "{state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(finished, "mock agent should finish the resumed report turn");
    assert!(dispatcher.dispatch(request.clone()).await.unwrap().repeated);
    let rows = repo
        .list_messages_page(
            "user_1",
            &conv.id,
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(
        rows.iter()
            .filter(|row| row.position.as_deref() == Some("right"))
            .count(),
        1
    );

    let mut invalid = request;
    invalid.operation_id = "wake-with-wrong-report".into();
    invalid.instruction = "다른 실행 지시".into();
    assert!(matches!(
        dispatcher.dispatch(invalid).await,
        Err(
            crate::external_dispatch::ExternalConversationDispatchError::Conversation(
                ConversationError::BadRequest { .. }
            )
        )
    ));
}

#[tokio::test]
async fn external_report_rejects_invalid_payload_and_foreign_or_missing_conversations() {
    let (svc, _, _, tasks) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    for invalid in [
        ExternalConversationReportRequest {
            operation_id: "".into(),
            ..report()
        },
        ExternalConversationReportRequest {
            operation_id: "a".repeat(129),
            ..report()
        },
        ExternalConversationReportRequest {
            operation_id: "bad\noperation".into(),
            ..report()
        },
        ExternalConversationReportRequest {
            content: " \n".into(),
            ..report()
        },
        ExternalConversationReportRequest {
            content: "a".repeat(256 * 1024 + 1),
            ..report()
        },
    ] {
        assert!(matches!(
            svc.append_external_report("user_1", &conv.id, invalid).await,
            Err(ConversationError::BadRequest { .. })
        ));
    }
    assert!(matches!(
        svc.append_external_report("other-user", &conv.id, report()).await,
        Err(ConversationError::NotFound { .. })
    ));
    assert!(matches!(
        svc.append_external_report("user_1", "missing", report()).await,
        Err(ConversationError::NotFound { .. })
    ));
    assert_eq!(tasks.active_count(), 0);
}

/// Records outbound prompts so the dispatch seam can be checked end to end.
struct PromptCaptureAgent {
    conversation_id: String,
    workspace: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    sent: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl IAgentTask for PromptCaptureAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        &self.workspace
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> TimestampMs {
        0
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.sent.lock().unwrap().push(data.content);
        let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

impl IMockAgent for PromptCaptureAgent {}

const CHOICE_NOTICE_HEADER: &str = "[AionUi choice notifications]";

/// Dispatches one MindNProgress-shaped request into a capture agent, then runs a
/// plain user turn in the same target conversation, and returns both prompts.
async fn dispatch_then_user_turn(
    operation_id: &str,
    actor_is_target: bool,
    body: serde_json::Value,
) -> (String, Vec<String>) {
    let db = init_database_memory().await.unwrap();
    seed_test_user(db.pool(), "user_1").await;
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let events = Arc::new(MockBroadcaster::new());
    let tasks = Arc::new(MockTaskManager::new());
    let svc = service(repo.clone(), events.clone(), tasks.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let actor_id = if actor_is_target {
        conv.id.clone()
    } else {
        svc.create("user_1", make_create_req()).await.unwrap().id
    };
    let (event_tx, _) = broadcast::channel(16);
    let capture = Arc::new(PromptCaptureAgent {
        conversation_id: conv.id.clone(),
        workspace: ensure_test_workspace_path(),
        event_tx,
        sent: Default::default(),
    });
    tasks.insert_agent(&conv.id, AgentInstance::Mock(capture.clone()));

    let dispatcher = crate::external_dispatch::ExternalConversationDispatchService::new(
        svc.clone(),
        Arc::new(aionui_db::fork_extensions::SqliteExternalDispatchRepository::new(
            db.pool().clone(),
        )),
    );
    let instruction = format!("# MindNProgress 외부 대화 요청 {operation_id}\n요청 범위를 수행한다.");
    let mut payload = json!({
        "operationId": operation_id, "actorConversationId": actor_id,
        "targetConversationId": conv.id, "strategy": "resume", "instruction": instruction,
    });
    payload
        .as_object_mut()
        .unwrap()
        .extend(body.as_object().unwrap().clone());
    let request: aionui_api_types::ExternalConversationDispatchRequest = serde_json::from_value(payload).unwrap();
    dispatcher.dispatch(request).await.unwrap();
    let mut dispatched_turn_id = None;
    for _ in 0..100 {
        let state = dispatcher.status(operation_id).await.unwrap().unwrap();
        if state.state == aionui_api_types::ExternalConversationDispatchState::Completed {
            dispatched_turn_id = state.turn_id;
            break;
        }
        assert_ne!(
            state.state,
            aionui_api_types::ExternalConversationDispatchState::Failed,
            "{state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let dispatched_turn_id = dispatched_turn_id.expect("capture agent should finish the dispatched turn");

    svc.run_agent_turn(ConversationAgentTurnRequest {
        user_id: "user_1".into(),
        conversation_id: conv.id.clone(),
        content: "일반 요청".into(),
        files: vec![],
        inject_skills: vec![],
        required_runtime_mode: None,
        persist_user_message: false,
        user_message_hidden: false,
        on_resource_waiting: None,
        on_started: None,
    })
    .await
    .unwrap();
    // A finished dispatch must have released its turn from suppression.
    AgentInstance::Mock(capture.clone())
        .send_message(SendMessageData {
            content: "같은 턴 재전송".into(),
            msg_id: "message-after-dispatch".into(),
            turn_id: Some(dispatched_turn_id),
            files: vec![],
            inject_skills: vec![],
        })
        .await
        .unwrap();

    let sent = capture.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[1].starts_with("일반 요청"));
    assert!(sent[1].contains(CHOICE_NOTICE_HEADER));
    assert!(sent[2].starts_with("같은 턴 재전송"));
    assert!(sent[2].contains(CHOICE_NOTICE_HEADER));
    (instruction, sent)
}

#[tokio::test]
async fn external_child_delegation_turn_omits_choice_notice_but_user_turn_keeps_it() {
    // MindNProgress child delegation: actor is the parent, explicit completion is requested.
    let (instruction, sent) = dispatch_then_user_turn(
        "dispatch-child-delegation",
        false,
        json!({ "explicitCompletionAfterInterruption": true }),
    )
    .await;
    assert_eq!(sent[0], instruction);
    assert!(!sent[0].contains(CHOICE_NOTICE_HEADER));
}

#[tokio::test]
async fn external_user_facing_dispatches_keep_choice_notice() {
    // Parent wake and integration/recovery wakes: the actor resumes itself.
    let (instruction, sent) = dispatch_then_user_turn("dispatch-parent-wake", true, json!({})).await;
    assert!(sent[0].starts_with(&instruction));
    assert!(sent[0].contains(CHOICE_NOTICE_HEADER));

    // Dooray response resume: the actor resumes itself without explicit completion.
    let (instruction, sent) = dispatch_then_user_turn(
        "dispatch-dooray-resume",
        true,
        json!({ "explicitCompletionAfterInterruption": false }),
    )
    .await;
    assert!(sent[0].starts_with(&instruction));
    assert!(sent[0].contains(CHOICE_NOTICE_HEADER));

    // Group document instruction: another conversation, but no explicit completion.
    let (instruction, sent) = dispatch_then_user_turn("gdi:dispatch-group-instruction", false, json!({})).await;
    assert!(sent[0].starts_with(&instruction));
    assert!(sent[0].contains(CHOICE_NOTICE_HEADER));
}

#[tokio::test]
async fn suppressed_choice_notice_turn_is_released_without_a_turn_outcome() {
    // The dispatch task releases through this handle on both Ok and Err outcomes.
    const TURN_ID: &str = "turn-dispatch-released-on-error";
    let (event_tx, _) = broadcast::channel(16);
    let capture = Arc::new(PromptCaptureAgent {
        conversation_id: "choice-release".into(),
        workspace: ensure_test_workspace_path(),
        event_tx,
        sent: Default::default(),
    });
    let agent = AgentInstance::Mock(capture.clone());
    let message = |content: &str| SendMessageData {
        content: content.into(),
        msg_id: "message-1".into(),
        turn_id: Some(TURN_ID.into()),
        files: vec![],
        inject_skills: vec![],
    };

    let suppressed = crate::external_dispatch::SuppressedChoiceNoticeTurn::default();
    suppressed.suppress(TURN_ID);
    agent.send_message(message("위임 전문")).await.unwrap();
    suppressed.release();
    agent.send_message(message("재시도 요청")).await.unwrap();
    // A second release is a no-op.
    suppressed.release();

    let sent = capture.sent.lock().unwrap().clone();
    assert_eq!(sent[0], "위임 전문");
    assert!(sent[1].starts_with("재시도 요청"));
    assert!(sent[1].contains(CHOICE_NOTICE_HEADER));
}
