#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, ConfirmExternalConversationDispatchCompletionRequest,
    ConfirmExternalConversationDispatchCompletionResponse, ExternalConversationDispatchCapabilities,
    ExternalConversationDispatchRequest, ExternalConversationDispatchResponse,
};
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use crate::external_dispatch::{ExternalConversationDispatchError, ExternalConversationDispatchService};
use crate::state::ConversationRouterState;

const EXTERNAL_DISPATCH_BODY_LIMIT: usize = 320 * 1024;

#[derive(Clone)]
struct ExternalConversationDispatchRouterState {
    service: Arc<ExternalConversationDispatchService>,
}

pub fn external_conversation_dispatch_routes(state: ConversationRouterState) -> Router {
    let state = ExternalConversationDispatchRouterState {
        service: Arc::new(ExternalConversationDispatchService::new(
            state.service,
            state.external_dispatch_repository,
        )),
    };
    Router::new()
        .route("/api/internal/external-conversation-dispatches", post(dispatch))
        .route(
            "/api/internal/external-conversation-dispatches/capabilities",
            get(dispatch_capabilities),
        )
        .route(
            "/api/internal/external-conversation-dispatches/{operation_id}",
            get(dispatch_status),
        )
        .route(
            "/api/internal/external-conversation-dispatches/{operation_id}/complete",
            post(confirm_dispatch_completion),
        )
        .layer(DefaultBodyLimit::max(EXTERNAL_DISPATCH_BODY_LIMIT))
        .with_state(state)
}

async fn dispatch_capabilities() -> Json<ApiResponse<ExternalConversationDispatchCapabilities>> {
    Json(ApiResponse::ok(ExternalConversationDispatchCapabilities {
        schema_version: 3,
        workspace_lease_version: 2,
        atomic_workspace_rebind: true,
        releases_runtime_on_terminal: true,
        persistent_recovery_state: true,
        explicit_completion_after_interruption: true,
    }))
}

async fn confirm_dispatch_completion(
    State(state): State<ExternalConversationDispatchRouterState>,
    Path(operation_id): Path<String>,
    body: Result<Json<ConfirmExternalConversationDispatchCompletionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConfirmExternalConversationDispatchCompletionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state
        .service
        .confirm_completion(&operation_id, request)
        .await
        .map_err(map_dispatch_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn dispatch(
    State(state): State<ExternalConversationDispatchRouterState>,
    body: Result<Json<ExternalConversationDispatchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ExternalConversationDispatchResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.dispatch(request).await.map_err(map_dispatch_error)?;
    let status = if response.repeated {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(ApiResponse::ok(response))))
}

async fn dispatch_status(
    State(state): State<ExternalConversationDispatchRouterState>,
    Path(operation_id): Path<String>,
) -> Result<Json<ApiResponse<ExternalConversationDispatchResponse>>, ApiError> {
    let response = state
        .service
        .status(&operation_id)
        .await
        .map_err(map_dispatch_error)?
        .ok_or_else(|| {
            ApiError::coded(
                StatusCode::NOT_FOUND,
                "EXTERNAL_DISPATCH_NOT_FOUND",
                "External conversation dispatch was not found.",
                None,
            )
        })?;
    Ok(Json(ApiResponse::ok(response)))
}

fn map_dispatch_error(error: ExternalConversationDispatchError) -> ApiError {
    match error {
        ExternalConversationDispatchError::InvalidPayload => ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_DISPATCH_INVALID",
            "External conversation dispatch payload is invalid.",
            None,
        ),
        ExternalConversationDispatchError::IdempotencyConflict => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_DISPATCH_IDEMPOTENCY_CONFLICT",
            "The operation id is already used for another dispatch.",
            None,
        ),
        ExternalConversationDispatchError::PreparationInProgress => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_DISPATCH_PREPARING",
            "The dispatch is still being prepared.",
            None,
        ),
        ExternalConversationDispatchError::ActorNotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "EXTERNAL_DISPATCH_ACTOR_NOT_FOUND",
            "The actor conversation was not found.",
            None,
        ),
        ExternalConversationDispatchError::TargetNotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "EXTERNAL_DISPATCH_TARGET_NOT_FOUND",
            "The target conversation was not found.",
            None,
        ),
        ExternalConversationDispatchError::Forbidden => ApiError::coded(
            StatusCode::FORBIDDEN,
            "EXTERNAL_DISPATCH_FORBIDDEN",
            "The target conversation belongs to another user.",
            None,
        ),
        ExternalConversationDispatchError::CapacityExhausted => ApiError::coded(
            StatusCode::TOO_MANY_REQUESTS,
            "EXTERNAL_DISPATCH_CAPACITY_EXHAUSTED",
            "Too many external conversation dispatches are tracked.",
            None,
        ),
        ExternalConversationDispatchError::NotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "EXTERNAL_DISPATCH_NOT_FOUND",
            "External conversation dispatch was not found.",
            None,
        ),
        ExternalConversationDispatchError::CompletionNotAllowed => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_DISPATCH_COMPLETION_NOT_ALLOWED",
            "The dispatch is not waiting for explicit completion.",
            None,
        ),
        ExternalConversationDispatchError::CompletionTurnNotActive => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_DISPATCH_COMPLETION_TURN_NOT_ACTIVE",
            "The target conversation has no active turn to confirm.",
            None,
        ),
        ExternalConversationDispatchError::Persistence(error) => ApiError::coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            "EXTERNAL_DISPATCH_STORAGE_FAILED",
            "External conversation dispatch state could not be stored.",
            Some(serde_json::json!({ "reason": error.to_string() })),
        ),
        ExternalConversationDispatchError::Conversation(error) => ApiError::from(error),
    }
}
