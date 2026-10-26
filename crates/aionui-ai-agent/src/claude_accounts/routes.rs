#![allow(clippy::disallowed_types)]

//! Claude account routes (personal fork).
//!
//! - `GET    /api/claude-accounts` — accounts in shared order
//! - `POST   /api/claude-accounts` — register an additional account
//! - `PUT    /api/claude-accounts/order` — reorder every account
//! - `PATCH  /api/claude-accounts/{id}` — rename / toggle settings sharing
//! - `DELETE /api/claude-accounts/{id}` — remove an unused account
//! - `GET    /api/claude-accounts/{id}/auth-status` — `claude auth status` for the account

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, ClaudeAccount, ClaudeAccountAuthStatus, CreateClaudeAccountRequest, ReorderClaudeAccountsRequest,
    UpdateClaudeAccountRequest,
};
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::routing::{get, patch, put};

use super::service::ClaudeAccountService;
use crate::routes::error_mapping::agent_error_to_api_error;

#[derive(Clone)]
pub struct ClaudeAccountRouterState {
    pub service: Arc<ClaudeAccountService>,
}

pub fn claude_account_routes(state: ClaudeAccountRouterState) -> Router {
    Router::new()
        .route("/api/claude-accounts", get(list_accounts).post(create_account))
        .route("/api/claude-accounts/order", put(reorder_accounts))
        .route(
            "/api/claude-accounts/{id}",
            patch(update_account).delete(delete_account),
        )
        .route("/api/claude-accounts/{id}/auth-status", get(auth_status))
        .with_state(state)
}

async fn list_accounts(
    State(state): State<ClaudeAccountRouterState>,
) -> Result<Json<ApiResponse<Vec<ClaudeAccount>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list().await.map_err(agent_error_to_api_error)?,
    )))
}

async fn create_account(
    State(state): State<ClaudeAccountRouterState>,
    body: Result<Json<CreateClaudeAccountRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ClaudeAccount>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.create(req).await.map_err(agent_error_to_api_error)?,
    )))
}

async fn reorder_accounts(
    State(state): State<ClaudeAccountRouterState>,
    body: Result<Json<ReorderClaudeAccountsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<ClaudeAccount>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.reorder(req).await.map_err(agent_error_to_api_error)?,
    )))
}

async fn update_account(
    State(state): State<ClaudeAccountRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateClaudeAccountRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ClaudeAccount>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.update(&id, req).await.map_err(agent_error_to_api_error)?,
    )))
}

async fn delete_account(
    State(state): State<ClaudeAccountRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete(&id).await.map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn auth_status(
    State(state): State<ClaudeAccountRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ClaudeAccountAuthStatus>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.auth_status(&id).await.map_err(agent_error_to_api_error)?,
    )))
}
