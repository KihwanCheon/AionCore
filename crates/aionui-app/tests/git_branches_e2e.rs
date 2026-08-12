//! E2E coverage for the workspace Git branch batch API.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

const GIT_BRANCHES_PATH: &str = "/api/fs/git-branches";

#[tokio::test]
async fn git_branch_batch_returns_each_workspace_without_failing_the_whole_request() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let root = tempfile::tempdir().unwrap();
    let repository_path = root.path().join("repository");
    let non_repository_path = root.path().join("plain");
    std::fs::create_dir(&repository_path).unwrap();
    std::fs::create_dir(&non_repository_path).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("feature/batch-branches");
    git2::Repository::init_opts(&repository_path, &options).unwrap();

    let request = json_with_token(
        "POST",
        GIT_BRANCHES_PATH,
        json!({
            "workspaces": [
                repository_path.to_string_lossy(),
                non_repository_path.to_string_lossy()
            ]
        }),
        &token,
        &csrf,
    );
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"][0]["branch"], "feature/batch-branches");
    assert_eq!(body["data"][1]["branch"], serde_json::Value::Null);
}

#[tokio::test]
async fn git_branch_batch_rejects_unauthenticated_requests() {
    let (app, _services) = build_app().await;
    let request = Request::builder()
        .method("POST")
        .uri(GIT_BRANCHES_PATH)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"workspaces":[]}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
