use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindNProgressGroup {
    pub id: String,
    pub title: String,
    pub role: MindNProgressGroupRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MindNProgressGroupRole {
    Coordinator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindNProgressTarget {
    pub map_id: String,
    pub document_title: String,
    pub card_id: String,
    pub card_title: String,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<MindNProgressGroup>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mindnprogress_title_prefix_supports_legacy_and_current_servers() {
        let legacy = serde_json::json!({
            "mapId": "map", "documentTitle": "Document", "cardId": "root",
            "cardTitle": "Root", "archived": false,
        });
        let target: MindNProgressTarget = serde_json::from_value(legacy.clone()).unwrap();
        assert!(target.title_prefix.is_none());
        assert!(serde_json::to_value(target).unwrap().get("titlePrefix").is_none());

        for prefix in ["[상위] ", "[그룹 총괄] ", ""] {
            let mut current = legacy.clone();
            current["titlePrefix"] = serde_json::json!(prefix);
            let target: MindNProgressTarget = serde_json::from_value(current).unwrap();
            assert_eq!(target.title_prefix.as_deref(), Some(prefix));
            assert_eq!(serde_json::to_value(target).unwrap()["titlePrefix"], prefix);
        }
    }
}
