//! History-only reports use the ordinary conversation authentication and CSRF boundary.
mod common;

use axum::http::StatusCode;
use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn archived_http_target_rejects_messages_and_reports_without_starting_runtime() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let created = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({"type": "acp", "name": "Archived target", "extra": {}}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let id = body_json(created).await["data"]["id"].as_str().unwrap().to_owned();
    let archived = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/sidebar/conversation/{id}/archive"),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    for (suffix, payload) in [
        ("messages", json!({"content": "rejected input", "files": []})),
        (
            "external-reports",
            json!({"operationId": "archived-report", "content": "rejected report"}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                &format!("/api/conversations/{id}/{suffix}"),
                payload,
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GONE, "{suffix}");
        let body = body_json(response).await;
        assert_eq!(body["code"], "CONVERSATION_ARCHIVED", "{body}");
    }
    assert!(services.worker_task_manager.get_task(&id).is_none());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(&id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn external_report_http_contract_and_security() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let created = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "Report target", "extra": {} }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let id = body_json(created).await["data"]["id"].as_str().unwrap().to_owned();
    let url = format!("/api/conversations/{id}/external-reports");
    let report = json!({ "operationId": "mnp-completed-child-1", "content": "완료 전문 전체" });
    for (auth, csrf_value, expected) in [
        ("invalid-token", csrf.as_str(), StatusCode::UNAUTHORIZED),
        (token.as_str(), "", StatusCode::FORBIDDEN),
        (other_token.as_str(), other_csrf.as_str(), StatusCode::NOT_FOUND),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token("POST", &url, report.clone(), auth, csrf_value))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let missing = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations/missing/external-reports",
            report.clone(),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let invalid = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &url,
            json!({ "operationId": "bad", "content": "" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let mut message_id = String::new();
    for repeated in [false, true] {
        let response = app
            .clone()
            .oneshot(json_with_token("POST", &url, report.clone(), &token, &csrf))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let data = body_json(response).await["data"].clone();
        assert_eq!(data["executionRequested"], false);
        assert_eq!(data["repeated"], repeated);
        if repeated {
            assert_eq!(data["messageId"], message_id);
        }
        message_id = data["messageId"].as_str().unwrap().to_owned();
    }
    let conflict = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &url,
            json!({ "operationId": "mnp-completed-child-1", "content": "변경된 결과" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let response = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{id}/messages/{message_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["content"]["content"], "완료 전문 전체");
    assert_eq!(body["data"]["hidden"], false);
}
