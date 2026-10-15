//! Retention policy HTTP contract, authentication and per-user isolation.
mod common;

use aionui_app::{AppConfig, AppServices, create_router};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{get_request, get_with_token, json_with_token, login_existing, setup_and_login};
use serde_json::json;
use tower::ServiceExt;

const URL: &str = "/api/sidebar/archive-retention";

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    common::body_json(response).await["data"].clone()
}

async fn fixture() -> (axum::Router, AppServices, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let config = AppConfig {
        data_dir: tmp.path().into(),
        work_dir: tmp.path().into(),
        ..Default::default()
    };
    let services = AppServices::from_config(aionui_db::init_database_memory().await.unwrap(), &config)
        .await
        .unwrap();
    let app = create_router(&services).await.unwrap();
    (app, services, tmp)
}

fn settings() -> serde_json::Value {
    json!({ "enabled": true, "retention_days": 7, "run_at": "04:30", "timezone": "Asia/Seoul" })
}

#[tokio::test]
async fn retention_requires_login_and_csrf_for_saving() {
    let (mut app, services, _tmp) = fixture().await;
    let response = app.clone().oneshot(get_request(URL)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(URL)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(settings().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn retention_is_shared_across_sessions_and_isolated_from_other_users() {
    let (mut app, services, _tmp) = fixture().await;
    let (owner, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let initial = body_json(app.clone().oneshot(get_with_token(URL, &owner)).await.unwrap()).await;
    assert_eq!(initial["settings"]["enabled"], false);
    let saved = app
        .clone()
        .oneshot(json_with_token("PUT", URL, settings(), &owner, &csrf))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let saved = body_json(saved).await;
    assert_eq!(saved["settings"], settings());
    assert!(saved["next_run_at"].as_i64().unwrap() > aionui_common::now_ms());
    let (device_two, _) = login_existing(&mut app, "admin", "StrongP@ss1").await;
    let read = body_json(app.clone().oneshot(get_with_token(URL, &device_two)).await.unwrap()).await;
    assert_eq!(read, saved);
    let (other, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss1").await;
    let read = body_json(app.clone().oneshot(get_with_token(URL, &other)).await.unwrap()).await;
    assert_eq!(read["settings"]["enabled"], false);
    let mut disabled = settings();
    disabled["enabled"] = json!(false);
    assert_eq!(
        app.clone()
            .oneshot(json_with_token("PUT", URL, disabled, &other, &other_csrf))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    // Rebuilding the router preserves the saved schedule, without any renderer state.
    let restarted = create_router(&services).await.unwrap();
    let read = body_json(restarted.oneshot(get_with_token(URL, &device_two)).await.unwrap()).await;
    assert_eq!(read, saved);
}

#[tokio::test]
async fn retention_rejects_invalid_input_without_overwriting_existing_policy() {
    let (mut app, services, _tmp) = fixture().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let saved = body_json(
        app.clone()
            .oneshot(json_with_token("PUT", URL, settings(), &token, &csrf))
            .await
            .unwrap(),
    )
    .await;
    for (field, value) in [
        ("retention_days", json!(0)),
        ("run_at", json!("25:00")),
        ("timezone", json!("unknown")),
    ] {
        let mut invalid = settings();
        invalid[field] = value;
        let result = app
            .clone()
            .oneshot(json_with_token("PUT", URL, invalid, &token, &csrf))
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::BAD_REQUEST);
    }
    let mut spoofed = settings();
    spoofed["user_id"] = json!("someone-else");
    let result = app
        .clone()
        .oneshot(json_with_token("PUT", URL, spoofed, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let read = body_json(app.oneshot(get_with_token(URL, &token)).await.unwrap()).await;
    assert_eq!(read, saved);
}
