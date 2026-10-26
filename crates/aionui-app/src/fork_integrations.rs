//! Stable composition seam for integrations maintained by the personal fork.
//!
//! Upstream-facing composition files should depend only on the bundles and
//! builders in this module. Add future fork-only services, states, and routes
//! here so recurring upstream rebases do not expand the shared-file diff.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::claude_accounts::{
    ClaudeAccountRouterState, ClaudeAccountService, ClaudeAccountStore, IClaudeAccountUsage, claude_account_routes,
    install_store,
};
use aionui_ai_agent::error::AgentError;
use aionui_auth::{AuthState, auth_middleware};
use aionui_db::fork_extensions::claude_account_usage::count_conversations_using_claude_account;
use aionui_db::{IConversationRepository, IMcpServerRepository, SqlitePool};
use aionui_system::external_launch::{
    MindNProgressNavigationRouterState, MindNProgressNavigationService, mindnprogress_navigation_routes,
};
use async_trait::async_trait;
use axum::Router;
use axum::middleware::from_fn_with_state;

pub(crate) struct ForkIntegrationServices {
    mindnprogress_navigation: Arc<MindNProgressNavigationService>,
    claude_accounts: Arc<ClaudeAccountService>,
}

/// Builds fork-owned services from repositories constructed by [`crate::services::AppServices`].
pub(crate) fn build_fork_integration_services(
    conversation_repo: Arc<dyn IConversationRepository>,
    mcp_server_repo: Arc<dyn IMcpServerRepository>,
    pool: SqlitePool,
    data_dir: &Path,
) -> anyhow::Result<ForkIntegrationServices> {
    let http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;

    let claude_account_store = Arc::new(ClaudeAccountStore::in_data_dir(data_dir));
    install_store(claude_account_store.clone());

    Ok(ForkIntegrationServices {
        mindnprogress_navigation: Arc::new(MindNProgressNavigationService::new(
            conversation_repo,
            mcp_server_repo,
            http_client,
        )),
        claude_accounts: Arc::new(ClaudeAccountService::new(
            claude_account_store,
            Arc::new(ConversationClaudeAccountUsage { pool }),
        )),
    })
}

#[derive(Clone)]
pub(crate) struct ForkIntegrationStates {
    mindnprogress_navigation: MindNProgressNavigationRouterState,
    claude_accounts: ClaudeAccountRouterState,
}

/// Builds all fork-owned router state inside the application composition layer.
pub(crate) fn build_fork_integration_states(services: &ForkIntegrationServices) -> ForkIntegrationStates {
    ForkIntegrationStates {
        mindnprogress_navigation: MindNProgressNavigationRouterState {
            service: services.mindnprogress_navigation.clone(),
        },
        claude_accounts: ClaudeAccountRouterState {
            service: services.claude_accounts.clone(),
        },
    }
}

/// Returns the authenticated route tree maintained by the personal fork.
pub(crate) fn fork_integration_authenticated_routes(states: ForkIntegrationStates, auth_state: AuthState) -> Router {
    mindnprogress_navigation_routes(states.mindnprogress_navigation)
        .merge(claude_account_routes(states.claude_accounts))
        .route_layer(from_fn_with_state(auth_state, auth_middleware))
}

struct ConversationClaudeAccountUsage {
    pool: SqlitePool,
}

#[async_trait]
impl IClaudeAccountUsage for ConversationClaudeAccountUsage {
    async fn conversations_using(&self, account_id: &str) -> Result<i64, AgentError> {
        count_conversations_using_claude_account(&self.pool, account_id)
            .await
            .map_err(|e| AgentError::internal(format!("count claude account conversations: {e}")))
    }
}
