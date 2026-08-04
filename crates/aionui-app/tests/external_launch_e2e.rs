//! External WebUI conversation launch routing and security tests.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

#[tokio::test]
async fn internal_issue_is_csrf_exempt_but_browser_claim_requires_authentication() {
    let (mut app, services) = build_app().await;
    let issue_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/internal/external-conversation-launches")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "agentId": "codex",
                        "prompt": "Review this card",
                        "autoSend": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(issue_response.status(), StatusCode::CREATED);
    let launch_id = body_json(issue_response).await["data"]["launchId"]
        .as_str()
        .unwrap()
        .to_owned();

    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let unauthenticated_claim = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/external-conversation-launches/claim",
            json!({ "launchId": launch_id }),
            "invalid-token",
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(unauthenticated_claim.status(), StatusCode::UNAUTHORIZED);

    let authenticated_claim = app
        .oneshot(json_with_token(
            "POST",
            "/api/external-conversation-launches/claim",
            json!({ "launchId": launch_id }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(authenticated_claim.status(), StatusCode::OK);
    let claimed = body_json(authenticated_claim).await;
    assert_eq!(claimed["data"]["launch"]["prompt"], "Review this card");
    assert!(claimed["data"]["launch"].get("completionUrl").is_none());
}
