#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExternalLaunchError {
    #[error("External conversation launch payload is invalid")]
    InvalidPayload,

    #[error("External conversation launch callback URL is invalid")]
    InvalidCallbackUrl,

    #[error("External conversation launch capacity is exhausted")]
    CapacityExhausted,

    #[error("External conversation launch was not found or has expired")]
    NotFoundOrExpired,

    #[error("External conversation launch has already been claimed")]
    AlreadyClaimed,

    #[error("External conversation launch must be claimed before completion")]
    NotClaimed,

    #[error("External conversation launch belongs to another user")]
    Forbidden,

    #[error("External conversation launch completion is already in progress")]
    CompletionInProgress,

    #[error("External conversation launch was completed with a different conversation")]
    ConversationMismatch,

    #[error("Conversation does not exist for the current user")]
    ConversationNotFound,

    #[error("External conversation launch storage failed")]
    Storage,
}
