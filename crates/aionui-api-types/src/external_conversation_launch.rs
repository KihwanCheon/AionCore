use serde::{Deserialize, Serialize};

/// External request used to prepare a new conversation in AionUi.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationLaunchRequest {
    pub agent_id: String,
    pub completion_url: Option<String>,
    pub title: Option<String>,
    pub prompt: String,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    pub mode: Option<String>,
    pub thought_level: Option<String>,
    pub enabled_skill_ids: Option<Vec<String>>,
    pub disabled_builtin_skill_ids: Option<Vec<String>>,
    pub mcp_ids: Option<Vec<String>>,
    pub workspace: Option<String>,
    #[serde(default)]
    pub auto_send: bool,
}

/// Browser-safe launch payload. The server-only completion URL is omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationLaunchPayload {
    pub agent_id: String,
    pub title: Option<String>,
    pub prompt: String,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    pub mode: Option<String>,
    pub thought_level: Option<String>,
    pub enabled_skill_ids: Option<Vec<String>>,
    pub disabled_builtin_skill_ids: Option<Vec<String>>,
    pub mcp_ids: Option<Vec<String>>,
    pub workspace: Option<String>,
    pub auto_send: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateExternalConversationLaunchResponse {
    pub launch_id: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaimExternalConversationLaunchRequest {
    pub launch_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaimExternalConversationLaunchResponse {
    pub launch: ExternalConversationLaunchPayload,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompleteExternalConversationLaunchRequest {
    pub launch_id: String,
    pub conversation_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConversationLaunchCallbackStatus {
    NotRequired,
    Delivered,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompleteExternalConversationLaunchResponse {
    pub callback_status: ExternalConversationLaunchCallbackStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_contract_uses_existing_camel_case_payload() {
        let request: ExternalConversationLaunchRequest = serde_json::from_value(serde_json::json!({
            "agentId": "codex",
            "completionUrl": "http://127.0.0.1:4176/api/integrations/aionui/launch/token/conversation",
            "prompt": "Review this card",
            "thoughtLevel": "high",
            "mcpIds": ["mind-mcp"],
            "autoSend": true
        }))
        .unwrap();

        assert_eq!(request.agent_id, "codex");
        assert_eq!(request.thought_level.as_deref(), Some("high"));
        assert_eq!(request.mcp_ids, Some(vec!["mind-mcp".to_owned()]));
        assert!(request.auto_send);
    }

    #[test]
    fn browser_payload_has_no_completion_url_field() {
        let payload = ExternalConversationLaunchPayload {
            agent_id: "codex".to_owned(),
            title: None,
            prompt: "Review this card".to_owned(),
            model_id: None,
            provider_id: None,
            mode: None,
            thought_level: None,
            enabled_skill_ids: None,
            disabled_builtin_skill_ids: None,
            mcp_ids: None,
            workspace: None,
            auto_send: true,
        };

        let json = serde_json::to_value(payload).unwrap();
        assert!(json.get("completionUrl").is_none());
    }
}
