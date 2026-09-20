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
