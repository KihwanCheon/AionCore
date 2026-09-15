use std::sync::Arc;

use super::service::MindNProgressNavigationService;

#[derive(Clone)]
pub struct MindNProgressNavigationRouterState {
    pub service: Arc<MindNProgressNavigationService>,
}
