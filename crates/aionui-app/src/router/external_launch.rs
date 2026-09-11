use std::sync::Arc;

use aionui_db::IConversationRepository;
use aionui_system::external_launch::{
    ExternalLaunchConversationLookup, ExternalLaunchError, ExternalLaunchRouterState, ExternalLaunchService,
    build_external_launch_callback_client, parse_allowed_callback_hosts,
};

use crate::services::AppServices;

struct RepositoryExternalLaunchConversationLookup {
    conversation_repo: Arc<dyn IConversationRepository>,
}

#[async_trait::async_trait]
impl ExternalLaunchConversationLookup for RepositoryExternalLaunchConversationLookup {
    async fn exists_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, ExternalLaunchError> {
        self.conversation_repo
            .get(user_id, conversation_id)
            .await
            .map(|conversation| conversation.is_some())
            .map_err(|_| ExternalLaunchError::Storage)
    }
}

/// Comma-separated hosts allowed as launch callback targets besides loopback.
///
/// Set by the AionUi desktop host from the MindNProgress Runner credential, so
/// a sub machine can accept the callback of the server it is actually paired
/// with. Empty or unset keeps the loopback-only default.
const CALLBACK_HOSTS_ENV: &str = "AIONUI_EXTERNAL_LAUNCH_CALLBACK_HOSTS";

pub(super) fn build_external_launch_state(services: &AppServices) -> ExternalLaunchRouterState {
    let lookup = Arc::new(RepositoryExternalLaunchConversationLookup {
        conversation_repo: services.conversation_repo.clone(),
    });
    let allowed_hosts = parse_allowed_callback_hosts(&std::env::var(CALLBACK_HOSTS_ENV).unwrap_or_default());
    if !allowed_hosts.is_empty() {
        tracing::info!(
            count = allowed_hosts.len(),
            "external launch callback hosts allowed besides loopback"
        );
    }
    let service = ExternalLaunchService::new(build_external_launch_callback_client(), lookup)
        .with_allowed_callback_hosts(allowed_hosts);
    ExternalLaunchRouterState {
        service: Arc::new(service),
    }
}
