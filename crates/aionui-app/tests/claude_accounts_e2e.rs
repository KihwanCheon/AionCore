//! Claude account routes (personal fork): single-account shape, numbering, and guards.

mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

fn claude_rows(agents: &Value) -> Vec<&Value> {
    agents["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["backend"] == "claude")
        .collect()
}

#[tokio::test]
async fn claude_accounts_stay_invisible_until_a_second_account_exists() {
    let (app, services) = build_app().await;
    let mut app = app;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let unauthenticated = app.clone().oneshot(get_request("/api/claude-accounts")).await.unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    // Single account: only the implicit default, and agent rows keep their shape.
    let listed = body_json(
        app.clone()
            .oneshot(get_with_token("/api/claude-accounts", &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], "default");
    let agents = body_json(
        app.clone()
            .oneshot(get_with_token("/api/agents/management", &token))
            .await
            .unwrap(),
    )
    .await;
    let claude = claude_rows(&agents);
    assert!(!claude.is_empty(), "the builtin Claude row is seeded");
    assert!(claude.iter().all(|row| row.get("claude_accounts").is_none()));

    let max_dir = tempfile::tempdir().unwrap();
    let created = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/claude-accounts",
            json!({
                "label": "Personal Max",
                "config_dir": max_dir.path().to_string_lossy(),
                "default_label": "Company Team",
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let max = body_json(created).await["data"].clone();
    assert_eq!(max["number"], 2);
    let max_id = max["id"].as_str().unwrap().to_owned();

    // Two accounts: every Claude row carries the shared order.
    let agents = body_json(
        app.clone()
            .oneshot(get_with_token("/api/agents/management", &token))
            .await
            .unwrap(),
    )
    .await;
    for row in claude_rows(&agents) {
        let accounts = row["claude_accounts"]
            .as_array()
            .expect("claude_accounts on Claude rows");
        let order: Vec<(i64, &str)> = accounts
            .iter()
            .map(|a| (a["number"].as_i64().unwrap(), a["label"].as_str().unwrap()))
            .collect();
        assert_eq!(order, [(1, "Company Team"), (2, "Personal Max")]);
    }

    // A conversation keeps the account; the account cannot be deleted while used.
    let conversation = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "max chat", "extra": { "backend": "claude", "claude_account_id": max_id } }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(conversation.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(conversation).await["data"]["extra"]["claude_account_id"],
        max_id.as_str()
    );

    let blocked = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/claude-accounts/{max_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::CONFLICT);

    let reordered = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/claude-accounts/order",
            json!({ "ids": [max_id, "default"] }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(reordered.status(), StatusCode::OK);
    let reordered = body_json(reordered).await;
    assert_eq!(reordered["data"][0]["id"], max_id.as_str());
    assert_eq!(reordered["data"][0]["number"], 1);
}
