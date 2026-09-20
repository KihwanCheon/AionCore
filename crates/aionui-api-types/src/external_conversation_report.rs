use serde::{Deserialize, Serialize};

/// Persist a visible external report without starting or interrupting an agent.
/// The operation ID is immutable and scoped to the target conversation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalConversationReportRequest {
    pub operation_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationReportResponse {
    pub operation_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub content_hash: String,
    pub repeated: bool,
    pub execution_requested: bool,
}
