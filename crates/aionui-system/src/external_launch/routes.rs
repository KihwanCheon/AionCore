#![allow(clippy::disallowed_types)]

use aionui_api_types::{
    ApiResponse, ClaimExternalConversationLaunchRequest, ClaimExternalConversationLaunchResponse,
    CompleteExternalConversationLaunchRequest, CompleteExternalConversationLaunchResponse,
    CreateExternalConversationLaunchResponse, ExternalConversationLaunchRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Extension, Json, State};
use axum::http::StatusCode;
use axum::routing::post;

use super::error::ExternalLaunchError;
use super::state::ExternalLaunchRouterState;

const EXTERNAL_LAUNCH_BODY_LIMIT: usize = 320 * 1024;

pub fn external_launch_routes(state: ExternalLaunchRouterState) -> Router {
    Router::new()
        .route("/api/external-conversation-launches/claim", post(claim_launch))
        .route("/api/external-conversation-launches/complete", post(complete_launch))
        .layer(DefaultBodyLimit::max(EXTERNAL_LAUNCH_BODY_LIMIT))
        .with_state(state)
}

/// Internal issuance route. The WebUI reverse proxy blocks this path, so only
/// clients that can reach the loopback-bound AionCore listener can call it.
pub fn external_launch_internal_routes(state: ExternalLaunchRouterState) -> Router {
    Router::new()
        .route("/api/internal/external-conversation-launches", post(issue_launch))
        .layer(DefaultBodyLimit::max(EXTERNAL_LAUNCH_BODY_LIMIT))
        .with_state(state)
}

async fn issue_launch(
    State(state): State<ExternalLaunchRouterState>,
    body: Result<Json<ExternalConversationLaunchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<CreateExternalConversationLaunchResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.issue(request).map_err(map_external_launch_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn claim_launch(
    State(state): State<ExternalLaunchRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ClaimExternalConversationLaunchRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ClaimExternalConversationLaunchResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state
        .service
        .claim(&user.id, &request.launch_id)
        .map_err(map_external_launch_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn complete_launch(
    State(state): State<ExternalLaunchRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CompleteExternalConversationLaunchRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CompleteExternalConversationLaunchResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state
        .service
        .complete(&user.id, &request.launch_id, &request.conversation_id)
        .await
        .map_err(map_external_launch_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

fn map_external_launch_error(error: ExternalLaunchError) -> ApiError {
    match error {
        ExternalLaunchError::InvalidPayload => ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_LAUNCH_INVALID",
            "External conversation launch payload is invalid.",
            None,
        ),
        ExternalLaunchError::InvalidCallbackUrl => ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_LAUNCH_CALLBACK_INVALID",
            "External conversation launch callback is invalid.",
            None,
        ),
        ExternalLaunchError::CapacityExhausted => ApiError::coded(
            StatusCode::TOO_MANY_REQUESTS,
            "EXTERNAL_LAUNCH_CAPACITY_EXHAUSTED",
            "Too many external conversation launches are pending.",
            None,
        ),
        ExternalLaunchError::NotFoundOrExpired => ApiError::coded(
            StatusCode::NOT_FOUND,
            "EXTERNAL_LAUNCH_NOT_FOUND_OR_EXPIRED",
            "External conversation launch was not found or has expired.",
            None,
        ),
        ExternalLaunchError::AlreadyClaimed => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_LAUNCH_ALREADY_CLAIMED",
            "External conversation launch has already been used.",
            None,
        ),
        ExternalLaunchError::NotClaimed => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_LAUNCH_NOT_CLAIMED",
            "External conversation launch has not been claimed.",
            None,
        ),
        ExternalLaunchError::Forbidden => ApiError::coded(
            StatusCode::FORBIDDEN,
            "EXTERNAL_LAUNCH_FORBIDDEN",
            "External conversation launch belongs to another user.",
            None,
        ),
        ExternalLaunchError::CompletionInProgress => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_LAUNCH_COMPLETION_IN_PROGRESS",
            "External conversation launch completion is already in progress.",
            None,
        ),
        ExternalLaunchError::ConversationMismatch => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_LAUNCH_CONVERSATION_MISMATCH",
            "External conversation launch was completed with another conversation.",
            None,
        ),
        ExternalLaunchError::ConversationNotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "EXTERNAL_LAUNCH_CONVERSATION_NOT_FOUND",
            "Created conversation was not found for the current user.",
            None,
        ),
        ExternalLaunchError::Storage => ApiError::coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            "EXTERNAL_LAUNCH_STORAGE_FAILED",
            "External conversation launch storage failed.",
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::external_launch::{
        ExternalLaunchConversationLookup, ExternalLaunchService, build_external_launch_callback_client,
    };

    struct ExistingConversation;

    #[async_trait]
    impl ExternalLaunchConversationLookup for ExistingConversation {
        async fn exists_for_user(&self, _user_id: &str, _conversation_id: &str) -> Result<bool, ExternalLaunchError> {
            Ok(true)
        }
    }

    fn state() -> ExternalLaunchRouterState {
        ExternalLaunchRouterState {
            service: Arc::new(ExternalLaunchService::new(
                build_external_launch_callback_client(),
                Arc::new(ExistingConversation),
            )),
        }
    }

    async fn response_json(response: axum::response::Response) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }

    #[tokio::test]
    async fn issue_claim_and_complete_use_body_tokens_without_exposing_callback() {
        let state = state();
        let issue_response = external_launch_internal_routes(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/internal/external-conversation-launches")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
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
        let issue_json = response_json(issue_response).await;
        let launch_id = issue_json["data"]["launchId"].as_str().unwrap();

        let claim_response = external_launch_routes(state.clone())
            .layer(Extension(CurrentUser::local_default()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/external-conversation-launches/claim")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({ "launchId": launch_id }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(claim_response.status(), StatusCode::OK);
        let claim_json = response_json(claim_response).await;
        assert_eq!(claim_json["data"]["launch"]["prompt"], "Review this card");
        assert!(claim_json["data"]["launch"].get("completionUrl").is_none());

        let complete_response = external_launch_routes(state)
            .layer(Extension(CurrentUser::local_default()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/external-conversation-launches/complete")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "launchId": launch_id, "conversationId": "conv-1" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(complete_response.status(), StatusCode::OK);
    }
}
