use std::collections::HashMap;
use std::time::{Duration, Instant};

use aionui_api_types::{
    ConversationAssistantIdentityResponse, ConversationListResponse, ListConversationsQuery,
    assistant_avatar_response_value_with_version,
};
use aionui_common::{ErrorChain, PaginatedResult};
use aionui_db::models::{AssistantDefinitionRow, ConversationAssistantSnapshotRow};
use aionui_db::{ConversationFilters, resolve_agent_binding_from_rows};
use tracing::{info, warn};

use super::ConversationService;
use crate::convert::row_to_response_with_extra;
use crate::error::ConversationError;

impl ConversationService {
    pub(super) async fn list_with_shared_metadata(
        &self,
        user_id: &str,
        query: ListConversationsQuery,
    ) -> Result<ConversationListResponse, ConversationError> {
        let started = Instant::now();
        let filters = ConversationFilters {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(0),
            source: query.source,
            cron_job_id: query.cron_job_id,
            pinned: query.pinned,
        };
        let result = self.conversation_repo.list_paginated(user_id, &filters).await?;
        let rows_elapsed = started.elapsed();
        let auto_inject = if result.items.is_empty() {
            Vec::new()
        } else {
            self.skill_resolver.auto_inject_names().await
        };

        // Share metadata only within this request so later edits and other
        // users always get a fresh view. Preserve the legacy skip-and-log policy.
        let mut items = Vec::with_capacity(result.items.len());
        for row in result.items {
            let row_id = row.id.clone();
            let mut extra = match serde_json::from_str(&row.extra) {
                Ok(value) => value,
                Err(error) => {
                    warn!(conversation_id = %row_id, error = %ErrorChain(&error), "Skipping unreadable conversation row in list");
                    continue;
                }
            };
            self.backfill_extra_with_skills(user_id, &row_id, &mut extra, &auto_inject)
                .await;
            match row_to_response_with_extra(row, extra, &self.workspace_root) {
                Ok(response) => items.push(response),
                Err(error) => {
                    warn!(conversation_id = %row_id, error = %ErrorChain(&error), "Skipping unreadable conversation row in list");
                }
            }
        }
        let conversion_elapsed = started.elapsed() - rows_elapsed;
        let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
        let snapshots: HashMap<_, _> = if ids.is_empty() {
            HashMap::new()
        } else {
            self.conversation_repo
                .get_assistant_snapshots(user_id, &ids)
                .await?
                .into_iter()
                .map(|snapshot| (snapshot.conversation_id.clone(), snapshot))
                .collect()
        };
        let snapshots_elapsed = started.elapsed() - rows_elapsed - conversion_elapsed;
        let mut definitions = HashMap::new();
        if !snapshots.is_empty() {
            let agents = self
                .agent_metadata_repo
                .list_all_for_user(user_id)
                .await
                .map_err(|error| ConversationError::internal(format!("agent_metadata lookup failed: {error}")))?;
            for response in &mut items {
                if let Some(snapshot) = snapshots.get(&response.id) {
                    if !definitions.contains_key(&snapshot.assistant_id) {
                        definitions.insert(
                            snapshot.assistant_id.clone(),
                            self.current_assistant_definition(user_id, &snapshot.assistant_id)
                                .await?,
                        );
                    }
                    let backend = resolve_agent_binding_from_rows(&agents, &snapshot.agent_id)
                        .map(|binding| binding.runtime_backend)
                        .unwrap_or_else(|| snapshot.agent_id.clone());
                    response.assistant = Some(assistant_identity(
                        snapshot,
                        backend,
                        definitions.get(&snapshot.assistant_id).and_then(Option::as_ref),
                    ));
                }
            }
        }
        if started.elapsed() >= Duration::from_millis(500) {
            info!(
                items = items.len(),
                snapshots = snapshots.len(),
                assistant_definitions = definitions.len(),
                rows_ms = rows_elapsed.as_millis() as u64,
                conversion_ms = conversion_elapsed.as_millis() as u64,
                snapshots_ms = snapshots_elapsed.as_millis() as u64,
                identity_ms =
                    (started.elapsed() - rows_elapsed - conversion_elapsed - snapshots_elapsed).as_millis() as u64,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "slow conversation list"
            );
        }
        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }
}

pub(super) fn assistant_identity(
    snapshot: &ConversationAssistantSnapshotRow,
    backend: String,
    definition: Option<&AssistantDefinitionRow>,
) -> ConversationAssistantIdentityResponse {
    let (source, name, avatar) = match definition {
        Some(definition) => (
            definition.source.clone(),
            definition.name.clone(),
            assistant_avatar_response_value_with_version(
                &definition.avatar_type,
                definition.avatar_value.as_deref(),
                &definition.assistant_id,
                definition.updated_at,
            )
            .unwrap_or_default(),
        ),
        None => (
            snapshot.assistant_source.clone(),
            snapshot.assistant_id.clone(),
            String::new(),
        ),
    };
    ConversationAssistantIdentityResponse {
        id: snapshot.assistant_id.clone(),
        source,
        name,
        avatar,
        backend,
    }
}
