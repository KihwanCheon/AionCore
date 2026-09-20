//! MindNProgress conversation navigation relay and security tests.

mod common;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::net::SocketAddr;
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{body_json, build_app, get_request, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn navigation_relay_is_authenticated_owner_scoped_csrf_protected_and_token_safe() {
    let mock_server = MockServer::start().await;
    let integration_dir = tempfile::tempdir().unwrap();
    let entry = integration_dir.path().join("mcp").join("server.mjs");
    let token_file = integration_dir
        .path()
        .join("server")
        .join("data")
        .join("_integration-token");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::create_dir_all(token_file.parent().unwrap()).unwrap();
    std::fs::write(&entry, "// test entry").unwrap();
    std::fs::write(&token_file, "integration-secret\n").unwrap();

    let (app, services) = build_app().await;
    let mut app = app.layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43210))));
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let create_conversation = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "MnP relay", "extra": {} }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(create_conversation.status(), StatusCode::CREATED);
    let created_id = body_json(create_conversation).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    Mock::given(method("GET"))
        .and(path(format!(
            "/api/integrations/aionui/conversations/{created_id}/mindnprogress"
        )))
        .and(header("authorization", "Bearer integration-secret"))
        .and(header("x-mnp-selection-device-address", "10.77.15.55"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "conversationId": created_id,
            "exists": true,
            "target": {
                "mapId": "map-1",
                "documentTitle": "Document title",
                "cardId": "card-1",
                "cardTitle": "Card title",
                "archived": false
            },
            "selectionAvailable": false,
            "matchingViewCount": 0,
            "localSelectionAvailable": false,
            "localViewCount": 0,
            "message": "linked"
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/integrations/aionui/conversations/{created_id}/mindnprogress/select"
        )))
        .and(header("authorization", "Bearer integration-secret"))
        .and(header("x-mnp-selection-device-address", "10.77.15.55"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "selected": true,
            "conversationId": created_id,
            "target": {
                "mapId": "map-1",
                "documentTitle": "Document title",
                "cardId": "card-1",
                "cardTitle": "Card title",
                "archived": false
            },
            "deliveredClientCount": 1,
            "requestedAt": "2026-09-15T00:00:00.000Z",
            "message": "selected"
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let create_mcp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/mcp/servers",
            json!({
                "name": "MindNProgress",
                "transport": {
                    "type": "stdio",
                    "command": "node",
                    "args": [entry.to_string_lossy()],
                    "env": {
                        "MNP_API_URL": mock_server.uri(),
                        "MNP_TOKEN_FILE": token_file.to_string_lossy()
                    }
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(create_mcp.status(), StatusCode::CREATED);

    let relay_path = format!("/api/integrations/mindnprogress/conversations/{created_id}");
    let unauthenticated = app.clone().oneshot(get_request(&relay_path)).await.unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(unauthenticated).await["code"], "UNAUTHORIZED");

    let lookup = app
        .clone()
        .oneshot(with_remote_client(get_with_token(&relay_path, &token)))
        .await
        .unwrap();
    assert_eq!(lookup.status(), StatusCode::OK);
    let lookup_body = body_json(lookup).await;
    assert_eq!(lookup_body["data"]["exists"], true);
    assert_eq!(lookup_body["data"]["selectionAvailable"], false);
    assert_eq!(lookup_body["data"]["matchingViewCount"], 0);
    assert_eq!(lookup_body["data"]["localSelectionAvailable"], false);
    assert_eq!(lookup_body["data"]["target"]["documentTitle"], "Document title");
    assert!(!lookup_body.to_string().contains("integration-secret"));
    assert!(!lookup_body.to_string().contains("10.77.15.55"));

    let (other_token, _other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let cross_user = app
        .clone()
        .oneshot(get_with_token(&relay_path, &other_token))
        .await
        .unwrap();
    assert_eq!(cross_user.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(cross_user).await["code"], "MNP_AIONUI_CONVERSATION_NOT_FOUND");

    let select_path = format!("{relay_path}/select");
    let missing_csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&select_path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(missing_csrf).await["code"], "CSRF_INVALID");

    let selection = app
        .oneshot(with_remote_client(json_with_token(
            "POST",
            &select_path,
            json!({}),
            &token,
            &csrf,
        )))
        .await
        .unwrap();
    assert_eq!(selection.status(), StatusCode::OK);
    let selection_body = body_json(selection).await;
    assert_eq!(selection_body["data"]["selected"], true);
    assert_eq!(selection_body["data"]["message"], "selected");
    assert!(!selection_body.to_string().contains("integration-secret"));
    assert!(!selection_body.to_string().contains("10.77.15.55"));

    for request in mock_server.received_requests().await.unwrap() {
        assert!(request.headers.get("x-mnp-ai-editor-id").is_none());
    }
}

fn with_remote_client(mut request: Request<Body>) -> Request<Body> {
    request
        .headers_mut()
        .insert("x-aionui-client-address", "10.77.15.55".parse().unwrap());
    // Browser-provided account assertions are deliberately untrusted and must
    // never be copied to MindNProgress by the backend relay.
    request
        .headers_mut()
        .insert("x-mnp-ai-editor-id", "attacker-controlled".parse().unwrap());
    request
}
