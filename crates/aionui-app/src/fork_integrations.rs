//! Stable composition seam for integrations maintained by the personal fork.
//!
//! Upstream-facing composition files should depend only on the bundles and
//! builders in this module. Add future fork-only services, states, and routes
//! here so recurring upstream rebases do not expand the shared-file diff.

use std::sync::Arc;
use std::time::Duration;

use aionui_auth::{AuthState, auth_middleware};
use aionui_db::{IConversationRepository, IMcpServerRepository};
use aionui_system::external_launch::{
    MindNProgressNavigationRouterState, MindNProgressNavigationService, mindnprogress_navigation_routes,
};
use axum::Router;
use axum::middleware::from_fn_with_state;

pub(crate) struct ForkIntegrationServices {
    mindnprogress_navigation: Arc<MindNProgressNavigationService>,
}

/// Builds fork-owned services from repositories constructed by [`crate::services::AppServices`].
pub(crate) fn build_fork_integration_services(
    conversation_repo: Arc<dyn IConversationRepository>,
    mcp_server_repo: Arc<dyn IMcpServerRepository>,
) -> anyhow::Result<ForkIntegrationServices> {
    let http_client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;

    Ok(ForkIntegrationServices {
        mindnprogress_navigation: Arc::new(MindNProgressNavigationService::new(
            conversation_repo,
            mcp_server_repo,
            http_client,
        )),
    })
}

#[derive(Clone)]
pub(crate) struct ForkIntegrationStates {
    mindnprogress_navigation: MindNProgressNavigationRouterState,
}

/// Builds all fork-owned router state inside the application composition layer.
pub(crate) fn build_fork_integration_states(services: &ForkIntegrationServices) -> ForkIntegrationStates {
    ForkIntegrationStates {
        mindnprogress_navigation: MindNProgressNavigationRouterState {
            service: services.mindnprogress_navigation.clone(),
        },
    }
}

/// Returns the authenticated route tree maintained by the personal fork.
pub(crate) fn fork_integration_authenticated_routes(states: ForkIntegrationStates, auth_state: AuthState) -> Router {
    mindnprogress_navigation_routes(states.mindnprogress_navigation)
        .route_layer(from_fn_with_state(auth_state, auth_middleware))
}
