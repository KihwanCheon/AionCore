use std::sync::Arc;

use aionui_db::IConversationRepository;
use aionui_system::external_launch::{
    ExternalLaunchConversationLookup, ExternalLaunchError, ExternalLaunchRouterState, ExternalLaunchService,
    build_external_launch_callback_client,
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

pub(super) fn build_external_launch_state(services: &AppServices) -> ExternalLaunchRouterState {
    let lookup = Arc::new(RepositoryExternalLaunchConversationLookup {
        conversation_repo: services.conversation_repo.clone(),
    });
    let service = ExternalLaunchService::new(build_external_launch_callback_client(), lookup);
    ExternalLaunchRouterState {
        service: Arc::new(service),
    }
}
