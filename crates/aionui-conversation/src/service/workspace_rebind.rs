use std::path::PathBuf;

use aionui_common::{AgentKillReason, ErrorChain};
use tracing::{debug, info, warn};

use super::{ConversationService, normalize_workspace_path, team_id_from_extra};
use crate::error::ConversationError;

impl ConversationService {
    /// Rebind an idle conversation to another workspace before an external
    /// dispatch sends its next instruction.
    ///
    /// The persisted workspace and in-memory agent must move together. If the
    /// replacement runtime cannot be created at the requested path, restore the
    /// previous workspace and runtime before returning the original error.
    #[tracing::instrument(
        skip_all,
        fields(
            user_id = %user_id,
            conversation_id = %conversation_id,
            requested_workspace = %requested_workspace
        )
    )]
    pub async fn rebind_workspace_for_external_dispatch(
        &self,
        user_id: &str,
        conversation_id: &str,
        requested_workspace: &str,
    ) -> Result<(), ConversationError> {
        let runtime = self.runtime_summary_for(conversation_id).await;
        if runtime.is_processing || runtime.pending_confirmations > 0 || !runtime.can_send_message {
            return Err(ConversationError::BadRequest {
                reason: "Conversation must be idle before its workspace can be rebound".into(),
            });
        }
        self.rebind_workspace_after_turn_claim(user_id, conversation_id, requested_workspace)
            .await
    }

    pub(super) async fn rebind_workspace_after_turn_claim(
        &self,
        user_id: &str,
        conversation_id: &str,
        requested_workspace: &str,
    ) -> Result<(), ConversationError> {
        let row = self
            .conversation_repo
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        if let Some(team_id) = team_id_from_extra(&row.extra) {
            return Err(ConversationError::TeamRuntimeRequired {
                conversation_id: conversation_id.to_owned(),
                team_id,
            });
        }

        let normalized_workspace = normalize_workspace_path(requested_workspace)?;
        let existing_extra: serde_json::Value =
            serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        let previous_workspace = existing_extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);

        if previous_workspace
            .as_deref()
            .is_some_and(|workspace| workspace_paths_match(workspace, &normalized_workspace))
            && self
                .task_manager
                .get_task(conversation_id)
                .is_some_and(|agent| workspace_paths_match(agent.workspace(), &normalized_workspace))
        {
            debug!("Conversation runtime already uses the requested workspace");
            return Ok(());
        }

        self.task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::RuntimeCapabilityChanged))
            .await;
        self.update_extra(
            user_id,
            conversation_id,
            serde_json::json!({ "workspace": normalized_workspace }),
        )
        .await?;

        let rebound = match self
            .ensure_runtime_agent(
                user_id,
                conversation_id,
                &self.task_manager,
                "external_workspace_rebind",
            )
            .await
        {
            Ok((agent, _)) if workspace_paths_match(agent.workspace(), &normalized_workspace) => Ok(()),
            Ok((agent, _)) => Err(ConversationError::internal(format!(
                "Rebound conversation runtime opened an unexpected workspace: {}",
                agent.workspace()
            ))),
            Err(error) => Err(error),
        };

        if let Err(error) = rebound {
            self.task_manager
                .kill_and_wait(conversation_id, Some(AgentKillReason::RuntimeCapabilityChanged))
                .await;
            let previous_value = previous_workspace
                .as_ref()
                .map_or(serde_json::Value::Null, |workspace| {
                    serde_json::Value::String(workspace.clone())
                });
            if let Err(rollback_error) = self
                .update_extra(
                    user_id,
                    conversation_id,
                    serde_json::json!({ "workspace": previous_value }),
                )
                .await
            {
                warn!(
                    error = %ErrorChain(&rollback_error),
                    "Failed to restore conversation workspace after rebind failure"
                );
            } else if let Err(rollback_error) = self
                .ensure_runtime_agent(
                    user_id,
                    conversation_id,
                    &self.task_manager,
                    "external_workspace_rebind_rollback",
                )
                .await
            {
                warn!(
                    error = %ErrorChain(&rollback_error),
                    "Failed to restore conversation runtime after rebind failure"
                );
            }
            return Err(error);
        }

        info!(
            workspace = %normalized_workspace,
            "Conversation workspace rebound for external dispatch"
        );
        Ok(())
    }

    /// Tear down the idle agent process after a leased external-dispatch turn.
    /// The persisted workspace remains attached to the conversation so a later
    /// dispatch can still identify the pool before assigning a fresh lease.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn release_workspace_runtime_for_external_dispatch(&self, conversation_id: &str) {
        self.task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::RuntimeCapabilityChanged))
            .await;
        info!("Released conversation runtime after leased external dispatch");
    }
}

fn workspace_paths_match(left: &str, right: &str) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| PathBuf::from(left));
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| PathBuf::from(right));
    if cfg!(windows) {
        left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}
