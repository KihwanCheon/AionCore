use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConversationDispatchStrategy {
    Resume,
    New,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationDispatchCreateOptions {
    pub agent_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub enabled_skill_ids: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_builtin_skill_ids: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_ids: Option<Vec<String>>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationDispatchRequest {
    pub operation_id: String,
    pub actor_conversation_id: String,
    pub strategy: ExternalConversationDispatchStrategy,
    #[serde(default)]
    pub target_conversation_id: Option<String>,
    pub instruction: String,
    #[serde(default)]
    pub create: Option<ExternalConversationDispatchCreateOptions>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalConversationDispatchState {
    Starting,
    WaitingResource,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationDispatchResource {
    pub kind: String,
    pub key: String,
    pub project_root: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalConversationDispatchResponse {
    pub operation_id: String,
    pub conversation_id: String,
    pub state: ExternalConversationDispatchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ExternalConversationDispatchResource>,
    pub repeated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_camel_case_wire_fields_and_snake_case_strategy() {
        let request: ExternalConversationDispatchRequest = serde_json::from_value(serde_json::json!({
            "operationId": "operation-1",
            "actorConversationId": "actor-1",
            "strategy": "resume",
            "targetConversationId": "target-1",
            "instruction": "Continue the work"
        }))
        .unwrap();

        assert_eq!(request.operation_id, "operation-1");
        assert_eq!(request.strategy, ExternalConversationDispatchStrategy::Resume);
        assert_eq!(request.target_conversation_id.as_deref(), Some("target-1"));
    }

    #[test]
    fn status_omits_absent_optional_fields() {
        let value = serde_json::to_value(ExternalConversationDispatchResponse {
            operation_id: "operation-1".to_owned(),
            conversation_id: "target-1".to_owned(),
            state: ExternalConversationDispatchState::Starting,
            turn_id: None,
            error_message: None,
            resource: None,
            repeated: false,
        })
        .unwrap();

        assert_eq!(value["operationId"], "operation-1");
        assert_eq!(value["state"], "starting");
        assert!(value.get("turnId").is_none());
        assert!(value.get("errorMessage").is_none());
    }

    #[test]
    fn waiting_resource_status_exposes_the_unity_project_claim() {
        let value = serde_json::to_value(ExternalConversationDispatchResponse {
            operation_id: "operation-2".to_owned(),
            conversation_id: "target-2".to_owned(),
            state: ExternalConversationDispatchState::WaitingResource,
            turn_id: Some("turn-2".to_owned()),
            error_message: None,
            resource: Some(ExternalConversationDispatchResource {
                kind: "unity_project".to_owned(),
                key: "unity:abc123".to_owned(),
                project_root: "C:/Git/Holdem/hdtf-client".to_owned(),
            }),
            repeated: false,
        })
        .unwrap();

        assert_eq!(value["state"], "waiting_resource");
        assert_eq!(value["resource"]["kind"], "unity_project");
        assert_eq!(value["resource"]["key"], "unity:abc123");
    }
}
