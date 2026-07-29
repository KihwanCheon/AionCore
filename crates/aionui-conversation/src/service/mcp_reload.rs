use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{ConversationResponse, ReloadConversationMcpServersRequest};
use aionui_common::{AgentKillReason, AgentType, ConversationStatus, now_ms};
use aionui_db::ConversationRowUpdate;
use tracing::info;

use super::{
    ConversationService, classify_repo_mcp_status, classify_session_mcp_status, upsert_conversation_mcp_status,
};
use crate::convert::{row_to_response, string_to_enum};
use crate::error::ConversationError;

impl ConversationService {
    /// Recycle an existing conversation's in-memory runtime so the backend
    /// re-reads its MCP configuration. The existing snapshot is preserved by
    /// default, which keeps ambient/global MCP behavior intact. An explicit
    /// catalog sync replaces the snapshot with AionUi-managed servers.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn reload_mcp_servers(
        &self,
        user_id: &str,
        id: &str,
        req: ReloadConversationMcpServersRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConversationResponse, ConversationError> {
        let existing = self
            .conversation_repo
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;

        // Reserve the conversation lifecycle before checking the task. This
        // closes the race where a new turn could start after the idle check but
        // before the runtime is recycled.
        let _reload_claim = self.runtime_state.try_claim_turn(id, "mcp-reload")?;

        if task_manager
            .get_task(id)
            .and_then(|task| task.status())
            .is_some_and(|status| status == ConversationStatus::Running)
        {
            return Err(ConversationError::Busy {
                reason: "MCP servers cannot be reloaded while the conversation is processing".into(),
            });
        }

        if req.sync_aionui_catalog {
            let agent_type: AgentType = string_to_enum(&existing.r#type)?;
            let mut extra: serde_json::Value = serde_json::from_str(&existing.extra)
                .map_err(|e| ConversationError::internal(format!("Failed to parse conversation extra: {e}")))?;
            let mcp_support = self.resolve_mcp_support_policy(user_id, &agent_type, &extra).await?;
            let repo = self
                .mcp_server_repo
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
                .ok_or_else(|| ConversationError::internal("MCP server repository is unavailable"))?;
            let available_rows = repo
                .list(user_id)
                .await
                .map_err(|e| ConversationError::internal(format!("Failed to list MCP servers: {e}")))?
                .into_iter()
                .filter(|row| row.enabled && !row.builtin)
                .collect::<Vec<_>>();
            let requested_row_ids = req
                .mcp_server_ids
                .as_ref()
                .map(|ids| ids.iter().collect::<HashSet<_>>());
            let selected_rows = available_rows
                .into_iter()
                .filter(|row| {
                    requested_row_ids
                        .as_ref()
                        .is_none_or(|requested_ids| requested_ids.contains(&row.id))
                })
                .collect::<Vec<_>>();

            let selected_row_ids = selected_rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
            let mut selected_mcp_names = Vec::new();
            let mut selected_mcp_statuses = Vec::new();
            let mut seen_mcp_names = HashSet::new();
            let mut status_index_by_name = HashMap::new();

            for row in &selected_rows {
                if seen_mcp_names.insert(row.name.clone()) {
                    selected_mcp_names.push(row.name.clone());
                }
                upsert_conversation_mcp_status(
                    &mut selected_mcp_statuses,
                    &mut status_index_by_name,
                    classify_repo_mcp_status(row, mcp_support),
                );
            }

            for server in &req.session_mcp_servers {
                if seen_mcp_names.insert(server.name.clone()) {
                    selected_mcp_names.push(server.name.clone());
                }
                upsert_conversation_mcp_status(
                    &mut selected_mcp_statuses,
                    &mut status_index_by_name,
                    classify_session_mcp_status(server, mcp_support),
                );
            }

            let obj = extra.as_object_mut().ok_or_else(|| ConversationError::BadRequest {
                reason: "Conversation extra must be a JSON object".into(),
            })?;
            obj.insert(
                "mcp_server_ids".to_owned(),
                serde_json::Value::Array(
                    selected_row_ids
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            obj.insert(
                "mcp_servers".to_owned(),
                serde_json::Value::Array(
                    selected_mcp_names
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            obj.insert(
                "mcp_statuses".to_owned(),
                serde_json::to_value(&selected_mcp_statuses)
                    .map_err(|e| ConversationError::internal(format!("Failed to serialize MCP statuses: {e}")))?,
            );
            obj.insert(
                "session_mcp_servers".to_owned(),
                serde_json::to_value(&req.session_mcp_servers).map_err(|e| {
                    ConversationError::internal(format!("Failed to serialize session MCP servers: {e}"))
                })?,
            );

            self.conversation_repo
                .update(
                    user_id,
                    id,
                    &ConversationRowUpdate {
                        extra: Some(serde_json::to_string(&extra).map_err(|e| {
                            ConversationError::internal(format!("Failed to serialize conversation extra: {e}"))
                        })?),
                        updated_at: Some(now_ms()),
                        ..Default::default()
                    },
                )
                .await?;

            info!(
                user_mcp_count = selected_row_ids.len(),
                session_mcp_count = req.session_mcp_servers.len(),
                explicit_selection = req.mcp_server_ids.is_some(),
                "Conversation MCP snapshot synced from AionUi catalog"
            );
        }

        task_manager
            .kill_and_wait(id, Some(AgentKillReason::RuntimeCapabilityChanged))
            .await;

        let updated = self
            .conversation_repo
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::internal("Conversation vanished after MCP reload"))?;
        let response = row_to_response(updated, &self.workspace_root)?;

        info!(
            synced_aionui_catalog = req.sync_aionui_catalog,
            "Conversation runtime recycled to reload MCP configuration"
        );
        if req.sync_aionui_catalog {
            self.broadcast_list_changed(user_id, id, "updated", response.source.as_ref());
        }
        Ok(response)
    }
}
