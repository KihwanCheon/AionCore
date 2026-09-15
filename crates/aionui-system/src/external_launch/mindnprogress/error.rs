#[derive(Debug, thiserror::Error)]
pub enum MindNProgressNavigationError {
    #[error("conversation not found")]
    ConversationNotFound,
    #[error("integration storage failed")]
    Storage,
    #[error("integration is not configured")]
    NotConfigured,
    #[error("MindNProgress is unavailable")]
    Unavailable,
    #[error("MindNProgress returned an invalid response")]
    InvalidResponse,
    #[error("MindNProgress request failed")]
    Upstream {
        status: u16,
        code: Option<String>,
        message: String,
    },
}
