use super::*;

#[tokio::test]
async fn list_shares_metadata_across_hundreds_of_assistant_conversations() {
    let resolver = Arc::new(RecordingSkillResolver::new(vec!["cron".into()]));
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: Default::default(),
    });
    let (svc, _, repo, definitions, _, _) = make_service_with_assistant_support(resolver.clone(), dispatcher).await;
    upsert_test_assistant_definition(
        &definitions,
        "list-definition",
        "list-assistant",
        "codex",
        "auto",
        "auto",
    )
    .await;
    let original = create_assistant_backed_conversation(&svc, "user-1", Some("acp"), "codex", "list-assistant").await;
    let row = repo.get("user-1", &original.id).await.unwrap().unwrap();
    let snapshot = repo.assistant_snapshots.lock().unwrap()[0].clone();
    for index in 0..300 {
        let id = format!("list-{index:03}");
        let mut row = row.clone();
        row.id = id.clone();
        repo.create(&row).await.unwrap();
        let mut snapshot = snapshot.clone();
        snapshot.conversation_id = id;
        repo.assistant_snapshots.lock().unwrap().push(snapshot);
    }
    resolver.auto_inject_calls.store(0, Ordering::SeqCst);
    repo.snapshot_reads.store(0, Ordering::SeqCst);
    let response = svc
        .list(
            "user-1",
            ListConversationsQuery {
                limit: Some(10000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (response.items.len(), response.total, response.has_more),
        (301, 301, false)
    );
    assert!(
        response
            .items
            .iter()
            .all(|item| item.assistant == original.assistant && item.extra["skills"] == json!(["cron"]))
    );
    assert_eq!(
        (
            resolver.auto_inject_calls.load(Ordering::SeqCst),
            repo.snapshot_batch_reads.load(Ordering::SeqCst),
            repo.snapshot_reads.load(Ordering::SeqCst),
        ),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn list_propagates_snapshot_errors_and_can_be_retried() {
    let (svc, _, repo, _) = make_service();
    insert_conversation_with_type(&repo, "user-1", AgentType::Acp).await;
    repo.fail_snapshot_reads.store(true, Ordering::SeqCst);
    let error = svc.list("user-1", ListConversationsQuery::default()).await.unwrap_err();
    assert!(error.to_string().contains("snapshot read failed"));
    repo.fail_snapshot_reads.store(false, Ordering::SeqCst);
    assert_eq!(
        svc.list("user-1", ListConversationsQuery::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn list_uses_the_current_request_user_and_does_not_reuse_previous_metadata() {
    let resolver = Arc::new(RecordingSkillResolver::new(vec!["cron".into()]));
    let (svc, _, repo, _) = make_service_with_resolver(resolver.clone());
    insert_conversation_with_type(&repo, "user-1", AgentType::Acp).await;
    insert_conversation_with_type(&repo, "user-2", AgentType::Acp).await;
    let first = svc.list("user-1", ListConversationsQuery::default()).await.unwrap();
    let second = svc.list("user-2", ListConversationsQuery::default()).await.unwrap();
    assert_eq!((first.items.len(), second.items.len()), (1, 1));
    assert_ne!(first.items[0].id, second.items[0].id);
    assert_eq!(resolver.auto_inject_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn list_keeps_skipping_unreadable_rows_when_snapshot_storage_is_unavailable() {
    let (svc, _, repo, _) = make_service();
    let mut row = insert_conversation_with_type(&repo, "user-1", AgentType::Acp).await;
    row.extra = "invalid json".into();
    repo.rows.lock().unwrap()[0] = row;
    repo.fail_snapshot_reads.store(true, Ordering::SeqCst);
    let response = svc.list("user-1", ListConversationsQuery::default()).await.unwrap();
    assert_eq!((response.items.len(), response.total), (0, 1));
    assert_eq!(repo.snapshot_batch_reads.load(Ordering::SeqCst), 0);
}
