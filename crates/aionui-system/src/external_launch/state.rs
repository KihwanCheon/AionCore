use std::sync::Arc;

use super::service::ExternalLaunchService;

#[derive(Clone)]
pub struct ExternalLaunchRouterState {
    pub service: Arc<ExternalLaunchService>,
}
