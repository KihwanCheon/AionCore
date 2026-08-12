//! E2E coverage for the active conversation runtime snapshot API.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_request, get_with_token, json_with_token, setup_and_login};

const ACTIVE_RUNTIMES_PATH: &str = "/api/internal/conversation-runtimes/active";

#[tokio::test]
async fn active_runtime_snapshot_returns_only_the_current_users_processing_conversations() {
    let (mut app, services) = build_app().await;
    let (user_1_token, user_1_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (user_2_token, user_2_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;

    let user_1_response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "User 1", "extra": {} }),
            &user_1_token,
            &user_1_csrf,
        ))
        .await
        .unwrap();
    let user_1_conversation_id = body_json(user_1_response).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let user_2_response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "User 2", "extra": {} }),
            &user_2_token,
            &user_2_csrf,
        ))
        .await
        .unwrap();
    let user_2_conversation_id = body_json(user_2_response).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let runtime_state = services.conversation_service.runtime_state();
    let _user_1_claim = runtime_state
        .try_claim_turn(&user_1_conversation_id, "turn-user-1")
        .unwrap();
    let _user_2_claim = runtime_state
        .try_claim_turn(&user_2_conversation_id, "turn-user-2")
        .unwrap();

    let response = app
        .clone()
        .oneshot(get_with_token(ACTIVE_RUNTIMES_PATH, &user_1_token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["schema_version"], 1);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["items"][0]["conversation_id"], user_1_conversation_id);
    assert_eq!(body["data"]["items"][0]["runtime"]["state"], "starting");
}

#[tokio::test]
async fn active_runtime_snapshot_rejects_unauthenticated_requests() {
    let (app, _services) = build_app().await;

    let response = app.oneshot(get_request(ACTIVE_RUNTIMES_PATH)).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
