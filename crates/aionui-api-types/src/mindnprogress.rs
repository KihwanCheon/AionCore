use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindNProgressTarget {
    pub map_id: String,
    pub document_title: String,
    pub card_id: String,
    pub card_title: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindNProgressConversationLinkResponse {
    pub conversation_id: String,
    pub exists: bool,
    pub target: Option<MindNProgressTarget>,
    #[serde(default)]
    pub selection_available: bool,
    #[serde(default)]
    pub matching_view_count: u64,
    #[serde(default)]
    pub local_selection_available: bool,
    #[serde(default)]
    pub local_view_count: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindNProgressConversationSelectionResponse {
    pub selected: bool,
    pub conversation_id: String,
    pub target: MindNProgressTarget,
    pub delivered_client_count: u64,
    pub requested_at: String,
    pub message: String,
}
