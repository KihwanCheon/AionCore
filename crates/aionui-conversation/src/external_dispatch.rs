#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionui_api_types::{
    AssistantConversationOverridesRequest, AssistantConversationRequest, CreateConversationRequest,
    ExternalConversationDispatchCreateOptions, ExternalConversationDispatchRequest,
    ExternalConversationDispatchResource, ExternalConversationDispatchResponse, ExternalConversationDispatchState,
    ExternalConversationDispatchStrategy, ExternalConversationDispatchWorkspaceLease,
};
use aionui_db::DbError;
use aionui_db::fork_extensions::{ExternalDispatchRecord, IExternalDispatchRepository};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use uuid::Uuid;

use crate::error::ConversationError;
use crate::service::{ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationService};

const MAX_OPERATION_ID_CHARS: usize = 128;
const MAX_CONVERSATION_ID_CHARS: usize = 120;
const MAX_INSTRUCTION_BYTES: usize = 256 * 1024;
const MAX_OPTION_CHARS: usize = 512;
const MAX_TITLE_CHARS: usize = 120;
const MAX_WORKSPACE_CHARS: usize = 4096;
const MAX_LIST_ITEMS: usize = 128;
const MAX_TRACKED_OPERATIONS: usize = 256;
const COMPLETED_OPERATION_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum ExternalConversationDispatchError {
    #[error("external conversation dispatch payload is invalid")]
    InvalidPayload,
    #[error("external conversation dispatch operation id is already used for another request")]
    IdempotencyConflict,
    #[error("external conversation dispatch operation is still being prepared")]
    PreparationInProgress,
    #[error("external conversation dispatch actor was not found")]
    ActorNotFound,
    #[error("external conversation dispatch target was not found")]
    TargetNotFound,
    #[error("external conversation dispatch target belongs to another user")]
    Forbidden,
    #[error("too many external conversation dispatches are tracked")]
    CapacityExhausted,
    #[error(transparent)]
    Persistence(#[from] DbError),
    #[error(transparent)]
    Conversation(#[from] ConversationError),
}

#[derive(Clone)]
struct StoredDispatch {
    request_fingerprint: String,
    actor_conversation_id: String,
    workspace_lease_json: Option<String>,
    response: ExternalConversationDispatchResponse,
    created_at: Instant,
    created_at_ms: i64,
}

#[derive(Clone)]
pub struct ExternalConversationDispatchService {
    conversation_service: ConversationService,
    repository: Arc<dyn IExternalDispatchRepository>,
    dispatches: Arc<Mutex<HashMap<String, StoredDispatch>>>,
    boot_id: Arc<str>,
}

impl ExternalConversationDispatchService {
    pub fn new(conversation_service: ConversationService, repository: Arc<dyn IExternalDispatchRepository>) -> Self {
        Self {
            conversation_service,
            repository,
            dispatches: Arc::new(Mutex::new(HashMap::new())),
            boot_id: Uuid::now_v7().to_string().into(),
        }
    }

    pub async fn dispatch(
        &self,
        request: ExternalConversationDispatchRequest,
    ) -> Result<ExternalConversationDispatchResponse, ExternalConversationDispatchError> {
        validate_request(&request)?;
        let request_fingerprint = request_fingerprint(&request)?;

        {
            let mut dispatches = self.dispatches.lock().expect("external dispatch lock poisoned");
            dispatches.retain(|_, stored| {
                let terminal = is_terminal(stored.response.state);
                !terminal || stored.created_at.elapsed() < COMPLETED_OPERATION_TTL
            });
            if let Some(stored) = dispatches.get(&request.operation_id) {
                if stored.request_fingerprint != request_fingerprint {
                    return Err(ExternalConversationDispatchError::IdempotencyConflict);
                }
                let mut response = stored.response.clone();
                response.repeated = true;
                return Ok(response);
            }
            if dispatches.len() >= MAX_TRACKED_OPERATIONS {
                return Err(ExternalConversationDispatchError::CapacityExhausted);
            }
        }

        if let Some(record) = self.repository.get(&request.operation_id).await? {
            return repeated_persisted_response(record, &request_fingerprint, &self.repository, &self.boot_id).await;
        }

        let now = aionui_common::now_ms();
        let placeholder = ExternalConversationDispatchResponse {
            operation_id: request.operation_id.clone(),
            conversation_id: request.target_conversation_id.clone().unwrap_or_default(),
            state: ExternalConversationDispatchState::Starting,
            turn_id: None,
            error_message: None,
            resource: None,
            repeated: false,
        };
        let stored = StoredDispatch {
            request_fingerprint: request_fingerprint.clone(),
            actor_conversation_id: request.actor_conversation_id.clone(),
            workspace_lease_json: request
                .workspace_lease
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|_| ExternalConversationDispatchError::InvalidPayload)?,
            response: placeholder,
            created_at: Instant::now(),
            created_at_ms: now,
        };
        let record = persisted_record(&request.operation_id, &stored, &self.boot_id, now)?;
        if !self.repository.insert(&record).await? {
            let existing = self
                .repository
                .get(&request.operation_id)
                .await?
                .ok_or(ExternalConversationDispatchError::PreparationInProgress)?;
            return repeated_persisted_response(existing, &request_fingerprint, &self.repository, &self.boot_id).await;
        }
        self.dispatches
            .lock()
            .expect("external dispatch lock poisoned")
            .insert(request.operation_id.clone(), stored);

        let cutoff = now - COMPLETED_OPERATION_TTL.as_millis() as i64;
        if let Err(error) = self.repository.delete_terminal_before(cutoff).await {
            warn!(error = %error, "failed to prune completed external conversation dispatch records");
        }

        let prepared = self.prepare_dispatch(&request).await;
        let (user_id, conversation_id) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.dispatches
                    .lock()
                    .expect("external dispatch lock poisoned")
                    .remove(&request.operation_id);
                if let Err(delete_error) = self.repository.delete(&request.operation_id).await {
                    warn!(operation_id = %request.operation_id, error = %delete_error, "failed to remove rejected external dispatch record");
                }
                return Err(error);
            }
        };

        let response = ExternalConversationDispatchResponse {
            operation_id: request.operation_id.clone(),
            conversation_id: conversation_id.clone(),
            state: ExternalConversationDispatchState::Starting,
            turn_id: None,
            error_message: None,
            resource: None,
            repeated: false,
        };
        if let Err(error) = self.set_response(&request.operation_id, response.clone()).await {
            self.dispatches
                .lock()
                .expect("external dispatch lock poisoned")
                .remove(&request.operation_id);
            warn!(operation_id = %request.operation_id, error = %error, "external dispatch was prepared but its target could not be persisted; automatic retry is disabled");
            return Err(error);
        }

        let service = self.clone();
        let operation_id = request.operation_id.clone();
        let workspace_rebind = (request.strategy == ExternalConversationDispatchStrategy::Resume)
            .then(|| request.workspace_lease.as_ref().map(|lease| lease.project_root.clone()))
            .flatten();
        let instruction = request.instruction;
        let release_workspace_runtime = request.workspace_lease.is_some();
        tokio::spawn(async move {
            let started_service = service.clone();
            let started_operation_id = operation_id.clone();
            let waiting_service = service.clone();
            let waiting_operation_id = operation_id.clone();
            let outcome = service
                .conversation_service
                .run_agent_turn_in_workspace(
                    ConversationAgentTurnRequest {
                        user_id,
                        conversation_id: conversation_id.clone(),
                        content: instruction,
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        required_runtime_mode: None,
                        persist_user_message: true,
                        user_message_hidden: false,
                        on_resource_waiting: Some(Arc::new(move |waiting| {
                            let waiting_service = waiting_service.clone();
                            let operation_id = waiting_operation_id.clone();
                            Box::pin(async move {
                                waiting_service
                                    .update_response(&operation_id, |response| {
                                        response.state = ExternalConversationDispatchState::WaitingResource;
                                        response.turn_id = Some(waiting.turn_id);
                                        response.resource = Some(ExternalConversationDispatchResource {
                                            kind: "unity_project".to_owned(),
                                            key: waiting.resource.key,
                                            project_root: waiting.resource.project_root,
                                        });
                                    })
                                    .await;
                            })
                        })),
                        on_started: Some(Arc::new(move |started| {
                            let started_service = started_service.clone();
                            let operation_id = started_operation_id.clone();
                            Box::pin(async move {
                                started_service
                                    .update_response(&operation_id, |response| {
                                        response.state = ExternalConversationDispatchState::Running;
                                        response.turn_id = Some(started.turn_id);
                                        response.resource =
                                            started.resource.map(|resource| ExternalConversationDispatchResource {
                                                kind: "unity_project".to_owned(),
                                                key: resource.key,
                                                project_root: resource.project_root,
                                            });
                                    })
                                    .await;
                            })
                        })),
                    },
                    workspace_rebind.as_deref(),
                )
                .await;

            match outcome {
                Ok(mut outcome) => loop {
                    if outcome.interrupted {
                        let interrupted_turn_id = outcome.turn_id.clone();
                        service
                            .update_response(&operation_id, |response| {
                                response.turn_id = Some(interrupted_turn_id.clone());
                                response.state = ExternalConversationDispatchState::WaitingResume;
                                response.error_message = None;
                                response.resource = None;
                            })
                            .await;
                        info!(
                            operation_id,
                            conversation_id,
                            turn_id = %interrupted_turn_id,
                            "external conversation dispatch waiting for a resumed turn"
                        );
                        outcome = service
                            .conversation_service
                            .wait_for_agent_turn_after(&conversation_id, &interrupted_turn_id)
                            .await;
                        info!(
                            operation_id,
                            conversation_id,
                            turn_id = %outcome.turn_id,
                            interrupted = outcome.interrupted,
                            "external conversation dispatch observed a follow-up turn"
                        );
                        continue;
                    }

                    if release_workspace_runtime {
                        service
                            .conversation_service
                            .release_workspace_runtime_for_external_dispatch(&conversation_id)
                            .await;
                    }
                    service
                        .update_response(&operation_id, |response| {
                            response.turn_id = Some(outcome.turn_id);
                            response.state = match outcome.status {
                                ConversationAgentTurnStatus::Completed => ExternalConversationDispatchState::Completed,
                                ConversationAgentTurnStatus::Failed => ExternalConversationDispatchState::Failed,
                            };
                            response.error_message = outcome.error_message;
                            response.resource = None;
                        })
                        .await;
                    break;
                },
                Err(error) => {
                    if release_workspace_runtime {
                        service
                            .conversation_service
                            .release_workspace_runtime_for_external_dispatch(&conversation_id)
                            .await;
                    }
                    service
                        .update_response(&operation_id, |response| {
                            response.state = ExternalConversationDispatchState::Failed;
                            response.error_message = Some(error.to_string());
                        })
                        .await;
                }
            }
        });

        Ok(response)
    }

    pub async fn status(
        &self,
        operation_id: &str,
    ) -> Result<Option<ExternalConversationDispatchResponse>, ExternalConversationDispatchError> {
        if let Some(response) = self
            .dispatches
            .lock()
            .expect("external dispatch lock poisoned")
            .get(operation_id)
            .map(|stored| stored.response.clone())
        {
            return Ok(Some(response));
        }

        let Some(mut record) = self.repository.get(operation_id).await? else {
            return Ok(None);
        };
        let mut response = deserialize_response(&record)?;
        if !is_terminal(response.state) && response.state != ExternalConversationDispatchState::RecoveryRequired {
            response.state = ExternalConversationDispatchState::RecoveryRequired;
            response.error_message = Some("interrupted_by_restart".to_owned());
            response.resource = None;
            record.state = state_name(response.state).to_owned();
            record.response_json =
                serde_json::to_string(&response).map_err(|_| ExternalConversationDispatchError::InvalidPayload)?;
            record.boot_id = self.boot_id.to_string();
            record.updated_at = aionui_common::now_ms();
            self.repository.update(&record).await?;
            warn!(
                operation_id,
                conversation_id = %response.conversation_id,
                "external conversation dispatch requires explicit recovery after restart"
            );
        }
        Ok(Some(response))
    }

    async fn prepare_dispatch(
        &self,
        request: &ExternalConversationDispatchRequest,
    ) -> Result<(String, String), ExternalConversationDispatchError> {
        let user_id = self
            .conversation_service
            .conversation_owner_user_id(&request.actor_conversation_id)
            .await?
            .ok_or(ExternalConversationDispatchError::ActorNotFound)?;
        self.conversation_service
            .validate_external_dispatch_target(&user_id, &request.actor_conversation_id)
            .await?;

        let conversation_id = match request.strategy {
            ExternalConversationDispatchStrategy::Resume => {
                let target_id = request
                    .target_conversation_id
                    .as_deref()
                    .ok_or(ExternalConversationDispatchError::InvalidPayload)?;
                let target_owner = self
                    .conversation_service
                    .conversation_owner_user_id(target_id)
                    .await?
                    .ok_or(ExternalConversationDispatchError::TargetNotFound)?;
                if target_owner != user_id {
                    return Err(ExternalConversationDispatchError::Forbidden);
                }
                self.conversation_service
                    .validate_external_dispatch_target(&user_id, target_id)
                    .await?;
                if let Some(workspace) = request.workspace_lease.as_ref() {
                    info!(
                        operation_id = %request.operation_id,
                        conversation_id = %target_id,
                        workspace_id = %workspace.workspace_id,
                        job_id = %workspace.job_id,
                        lease_id = %workspace.lease_id,
                        project_root = %workspace.project_root,
                        "external conversation dispatch accepted target workspace lease"
                    );
                }
                target_id.to_owned()
            }
            ExternalConversationDispatchStrategy::New => {
                let create = request
                    .create
                    .as_ref()
                    .ok_or(ExternalConversationDispatchError::InvalidPayload)?;
                let conversation = self
                    .conversation_service
                    .create(&user_id, create_conversation_request(create))
                    .await?;
                if let Some(workspace) = request.workspace_lease.as_ref() {
                    info!(
                        operation_id = %request.operation_id,
                        conversation_id = %conversation.id,
                        workspace_id = %workspace.workspace_id,
                        job_id = %workspace.job_id,
                        lease_id = %workspace.lease_id,
                        project_root = %workspace.project_root,
                        "external conversation dispatch created target in leased workspace"
                    );
                }
                conversation.id
            }
        };

        Ok((user_id, conversation_id))
    }

    async fn set_response(
        &self,
        operation_id: &str,
        response: ExternalConversationDispatchResponse,
    ) -> Result<(), ExternalConversationDispatchError> {
        let record = {
            let mut dispatches = self.dispatches.lock().expect("external dispatch lock poisoned");
            let stored = dispatches
                .get_mut(operation_id)
                .ok_or(ExternalConversationDispatchError::PreparationInProgress)?;
            stored.response = response;
            persisted_record(operation_id, stored, &self.boot_id, aionui_common::now_ms())?
        };
        self.repository.update(&record).await?;
        Ok(())
    }

    async fn update_response(
        &self,
        operation_id: &str,
        update: impl FnOnce(&mut ExternalConversationDispatchResponse),
    ) {
        let record = {
            let mut dispatches = self.dispatches.lock().expect("external dispatch lock poisoned");
            dispatches.get_mut(operation_id).and_then(|stored| {
                update(&mut stored.response);
                persisted_record(operation_id, stored, &self.boot_id, aionui_common::now_ms()).ok()
            })
        };
        if let Some(record) = record
            && let Err(error) = self.repository.update(&record).await
        {
            warn!(operation_id, error = %error, "failed to persist external conversation dispatch state");
        }
    }
}

fn request_fingerprint(
    request: &ExternalConversationDispatchRequest,
) -> Result<String, ExternalConversationDispatchError> {
    let serialized = serde_json::to_vec(request).map_err(|_| ExternalConversationDispatchError::InvalidPayload)?;
    Ok(format!("{:x}", Sha256::digest(serialized)))
}

fn is_terminal(state: ExternalConversationDispatchState) -> bool {
    matches!(
        state,
        ExternalConversationDispatchState::Completed | ExternalConversationDispatchState::Failed
    )
}

fn state_name(state: ExternalConversationDispatchState) -> &'static str {
    match state {
        ExternalConversationDispatchState::Starting => "starting",
        ExternalConversationDispatchState::WaitingResource => "waiting_resource",
        ExternalConversationDispatchState::Running => "running",
        ExternalConversationDispatchState::WaitingResume => "waiting_resume",
        ExternalConversationDispatchState::RecoveryRequired => "recovery_required",
        ExternalConversationDispatchState::Completed => "completed",
        ExternalConversationDispatchState::Failed => "failed",
    }
}

fn persisted_record(
    operation_id: &str,
    stored: &StoredDispatch,
    boot_id: &str,
    updated_at: i64,
) -> Result<ExternalDispatchRecord, ExternalConversationDispatchError> {
    Ok(ExternalDispatchRecord {
        operation_id: operation_id.to_owned(),
        request_fingerprint: stored.request_fingerprint.clone(),
        actor_conversation_id: stored.actor_conversation_id.clone(),
        target_conversation_id: (!stored.response.conversation_id.is_empty())
            .then(|| stored.response.conversation_id.clone()),
        state: state_name(stored.response.state).to_owned(),
        response_json: serde_json::to_string(&stored.response)
            .map_err(|_| ExternalConversationDispatchError::InvalidPayload)?,
        workspace_lease_json: stored.workspace_lease_json.clone(),
        boot_id: boot_id.to_owned(),
        created_at: stored.created_at_ms,
        updated_at,
        terminal_at: is_terminal(stored.response.state).then_some(updated_at),
    })
}

fn deserialize_response(
    record: &ExternalDispatchRecord,
) -> Result<ExternalConversationDispatchResponse, ExternalConversationDispatchError> {
    serde_json::from_str(&record.response_json).map_err(|_| ExternalConversationDispatchError::InvalidPayload)
}

async fn repeated_persisted_response(
    mut record: ExternalDispatchRecord,
    request_fingerprint: &str,
    repository: &Arc<dyn IExternalDispatchRepository>,
    boot_id: &str,
) -> Result<ExternalConversationDispatchResponse, ExternalConversationDispatchError> {
    if record.request_fingerprint != request_fingerprint {
        return Err(ExternalConversationDispatchError::IdempotencyConflict);
    }
    let mut response = deserialize_response(&record)?;
    if !is_terminal(response.state) {
        response.state = ExternalConversationDispatchState::RecoveryRequired;
        response.error_message = Some("interrupted_by_restart".to_owned());
        response.resource = None;
        record.state = state_name(response.state).to_owned();
        record.response_json =
            serde_json::to_string(&response).map_err(|_| ExternalConversationDispatchError::InvalidPayload)?;
        record.boot_id = boot_id.to_owned();
        record.updated_at = aionui_common::now_ms();
        repository.update(&record).await?;
    }
    response.repeated = true;
    Ok(response)
}

fn create_conversation_request(options: &ExternalConversationDispatchCreateOptions) -> CreateConversationRequest {
    let mcp_ids = options.mcp_ids.clone();
    let mut extra = json!({
        "custom_workspace": options.workspace.is_some(),
        "selected_mcp_server_ids": mcp_ids.clone().unwrap_or_default(),
    });
    if let Some(workspace) = options.workspace.as_ref() {
        extra["workspace"] = Value::String(workspace.clone());
    }

    CreateConversationRequest {
        r#type: None,
        name: options.title.clone(),
        model: None,
        assistant: Some(AssistantConversationRequest {
            id: format!("bare:{}", options.agent_id),
            locale: None,
            conversation_overrides: Some(AssistantConversationOverridesRequest {
                model: options.model_id.clone(),
                permission: options.mode.clone(),
                thought_level: options.thought_level.clone(),
                skill_ids: options.enabled_skill_ids.clone(),
                disabled_builtin_skill_ids: options.disabled_builtin_skill_ids.clone(),
                mcp_ids,
            }),
        }),
        source: None,
        channel_chat_id: None,
        extra,
    }
}

fn validate_request(request: &ExternalConversationDispatchRequest) -> Result<(), ExternalConversationDispatchError> {
    if !valid_operation_id(&request.operation_id)
        || !valid_identifier(&request.actor_conversation_id, MAX_CONVERSATION_ID_CHARS)
        || request.instruction.trim().is_empty()
        || request.instruction.len() > MAX_INSTRUCTION_BYTES
    {
        return Err(ExternalConversationDispatchError::InvalidPayload);
    }

    match request.strategy {
        ExternalConversationDispatchStrategy::Resume => {
            if request.create.is_some()
                || !request
                    .target_conversation_id
                    .as_deref()
                    .is_some_and(|value| valid_identifier(value, MAX_CONVERSATION_ID_CHARS))
            {
                return Err(ExternalConversationDispatchError::InvalidPayload);
            }
            if let Some(workspace) = request.workspace_lease.as_ref() {
                validate_workspace_lease(workspace)?;
            }
        }
        ExternalConversationDispatchStrategy::New => {
            if request.target_conversation_id.is_some() {
                return Err(ExternalConversationDispatchError::InvalidPayload);
            }
            let create = request
                .create
                .as_ref()
                .ok_or(ExternalConversationDispatchError::InvalidPayload)?;
            validate_create_options(create)?;
            if let Some(workspace) = request.workspace_lease.as_ref() {
                validate_workspace_lease(workspace)?;
                if create.workspace.as_deref() != Some(workspace.project_root.as_str()) {
                    return Err(ExternalConversationDispatchError::InvalidPayload);
                }
            }
        }
    }
    Ok(())
}

fn validate_workspace_lease(
    workspace: &ExternalConversationDispatchWorkspaceLease,
) -> Result<(), ExternalConversationDispatchError> {
    if !valid_identifier(&workspace.workspace_id, MAX_OPTION_CHARS)
        || !valid_identifier(&workspace.job_id, MAX_OPTION_CHARS)
        || !valid_identifier(&workspace.lease_id, MAX_OPTION_CHARS)
        || !valid_option(&workspace.project_root, MAX_WORKSPACE_CHARS)
    {
        return Err(ExternalConversationDispatchError::InvalidPayload);
    }
    Ok(())
}

fn validate_create_options(
    options: &ExternalConversationDispatchCreateOptions,
) -> Result<(), ExternalConversationDispatchError> {
    if !valid_option(&options.agent_id, MAX_OPTION_CHARS)
        || !valid_optional(&options.title, MAX_TITLE_CHARS)
        || !valid_optional(&options.model_id, MAX_OPTION_CHARS)
        || !valid_optional(&options.mode, MAX_OPTION_CHARS)
        || !valid_optional(&options.thought_level, MAX_OPTION_CHARS)
        || !valid_optional(&options.workspace, MAX_WORKSPACE_CHARS)
        || !valid_list(&options.enabled_skill_ids)
        || !valid_list(&options.disabled_builtin_skill_ids)
        || !valid_list(&options.mcp_ids)
    {
        return Err(ExternalConversationDispatchError::InvalidPayload);
    }
    Ok(())
}

fn valid_identifier(value: &str, max_chars: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_OPERATION_ID_CHARS
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ':'))
}

fn valid_option(value: &str, max_chars: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max_chars
}

fn valid_optional(value: &Option<String>, max_chars: usize) -> bool {
    value.as_ref().is_none_or(|value| valid_option(value, max_chars))
}

fn valid_list(values: &Option<Vec<String>>) -> bool {
    values.as_ref().is_none_or(|values| {
        values.len() <= MAX_LIST_ITEMS && values.iter().all(|value| valid_option(value, MAX_OPTION_CHARS))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::fork_extensions::{IExternalDispatchRepository, SqliteExternalDispatchRepository};
    use aionui_db::init_database_memory;

    fn request(strategy: ExternalConversationDispatchStrategy) -> ExternalConversationDispatchRequest {
        ExternalConversationDispatchRequest {
            operation_id: "operation-1".to_owned(),
            actor_conversation_id: "actor-1".to_owned(),
            strategy,
            target_conversation_id: Some("target-1".to_owned()),
            instruction: "Continue the card work".to_owned(),
            create: None,
            workspace_lease: None,
        }
    }

    #[test]
    fn resume_requires_target_without_create_options() {
        assert!(validate_request(&request(ExternalConversationDispatchStrategy::Resume)).is_ok());
        let mut invalid = request(ExternalConversationDispatchStrategy::Resume);
        invalid.create = Some(ExternalConversationDispatchCreateOptions {
            agent_id: "agent-codex".to_owned(),
            title: None,
            model_id: None,
            mode: None,
            thought_level: None,
            enabled_skill_ids: None,
            disabled_builtin_skill_ids: None,
            mcp_ids: None,
            workspace: None,
        });
        assert!(matches!(
            validate_request(&invalid),
            Err(ExternalConversationDispatchError::InvalidPayload)
        ));
    }

    #[test]
    fn new_requires_bounded_creation_options() {
        let mut valid = request(ExternalConversationDispatchStrategy::New);
        valid.target_conversation_id = None;
        valid.create = Some(ExternalConversationDispatchCreateOptions {
            agent_id: "agent-codex".to_owned(),
            title: Some("Child card".to_owned()),
            model_id: Some("gpt-5.6-sol".to_owned()),
            mode: Some("default".to_owned()),
            thought_level: Some("high".to_owned()),
            enabled_skill_ids: Some(vec!["skill-a".to_owned()]),
            disabled_builtin_skill_ids: Some(Vec::new()),
            mcp_ids: Some(vec!["mcp-a".to_owned()]),
            workspace: Some("D:/workspace".to_owned()),
        });
        assert!(validate_request(&valid).is_ok());
    }

    #[test]
    fn resume_accepts_a_bounded_workspace_lease() {
        let mut valid = request(ExternalConversationDispatchStrategy::Resume);
        valid.workspace_lease = Some(ExternalConversationDispatchWorkspaceLease {
            workspace_id: "fork2".to_owned(),
            job_id: "job-20260815-0012".to_owned(),
            lease_id: "lease-opaque-id".to_owned(),
            project_root: "C:/Git/Holdem_Fork2/hdtf-client".to_owned(),
        });
        assert!(validate_request(&valid).is_ok());

        valid.workspace_lease.as_mut().unwrap().lease_id = "lease:invalid".to_owned();
        assert!(matches!(
            validate_request(&valid),
            Err(ExternalConversationDispatchError::InvalidPayload)
        ));
    }

    #[test]
    fn new_accepts_matching_workspace_lease() {
        let mut valid = request(ExternalConversationDispatchStrategy::New);
        valid.target_conversation_id = None;
        valid.create = Some(ExternalConversationDispatchCreateOptions {
            agent_id: "agent-codex".to_owned(),
            title: None,
            model_id: None,
            mode: None,
            thought_level: None,
            enabled_skill_ids: None,
            disabled_builtin_skill_ids: None,
            mcp_ids: None,
            workspace: Some("D:/workspace".to_owned()),
        });
        valid.workspace_lease = Some(ExternalConversationDispatchWorkspaceLease {
            workspace_id: "fork2".to_owned(),
            job_id: "job-12".to_owned(),
            lease_id: "lease-12".to_owned(),
            project_root: "D:/workspace".to_owned(),
        });
        assert!(validate_request(&valid).is_ok());

        valid.create.as_mut().unwrap().workspace = Some("D:/other".to_owned());
        assert!(matches!(
            validate_request(&valid),
            Err(ExternalConversationDispatchError::InvalidPayload)
        ));
    }

    #[test]
    fn create_request_uses_bare_assistant_and_overrides() {
        let options = ExternalConversationDispatchCreateOptions {
            agent_id: "agent-codex".to_owned(),
            title: Some("Child card".to_owned()),
            model_id: Some("gpt-5.6-sol".to_owned()),
            mode: Some("default".to_owned()),
            thought_level: Some("high".to_owned()),
            enabled_skill_ids: Some(vec!["skill-a".to_owned()]),
            disabled_builtin_skill_ids: Some(Vec::new()),
            mcp_ids: Some(vec!["mcp-a".to_owned()]),
            workspace: Some("D:/workspace".to_owned()),
        };
        let request = create_conversation_request(&options);
        let assistant = request.assistant.unwrap();
        assert_eq!(assistant.id, "bare:agent-codex");
        assert_eq!(
            assistant.conversation_overrides.unwrap().model.as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(request.extra["workspace"], "D:/workspace");
    }

    #[test]
    fn operation_id_accepts_colon_but_conversation_ids_remain_strict() {
        let mut valid = request(ExternalConversationDispatchStrategy::Resume);
        valid.operation_id = "mnp:map-1:card-1:v4".to_owned();
        assert!(validate_request(&valid).is_ok());

        valid.operation_id = "mnp/map-1/card-1/v4".to_owned();
        assert!(matches!(
            validate_request(&valid),
            Err(ExternalConversationDispatchError::InvalidPayload)
        ));

        valid.operation_id = "mnp:map-1:card-1:v4".to_owned();
        valid.actor_conversation_id = "actor:1".to_owned();
        assert!(matches!(
            validate_request(&valid),
            Err(ExternalConversationDispatchError::InvalidPayload)
        ));
    }

    #[tokio::test]
    async fn persisted_nonterminal_dispatch_requires_explicit_recovery_after_restart() {
        let db = init_database_memory().await.unwrap();
        let repository: Arc<dyn IExternalDispatchRepository> =
            Arc::new(SqliteExternalDispatchRepository::new(db.pool().clone()));
        let request = request(ExternalConversationDispatchStrategy::Resume);
        let fingerprint = request_fingerprint(&request).unwrap();
        let response = ExternalConversationDispatchResponse {
            operation_id: request.operation_id.clone(),
            conversation_id: "target-1".to_owned(),
            state: ExternalConversationDispatchState::Running,
            turn_id: Some("turn-before-restart".to_owned()),
            error_message: None,
            resource: None,
            repeated: false,
        };
        let record = ExternalDispatchRecord {
            operation_id: request.operation_id.clone(),
            request_fingerprint: fingerprint.clone(),
            actor_conversation_id: request.actor_conversation_id.clone(),
            target_conversation_id: Some("target-1".to_owned()),
            state: "running".to_owned(),
            response_json: serde_json::to_string(&response).unwrap(),
            workspace_lease_json: None,
            boot_id: "boot-before-restart".to_owned(),
            created_at: 1,
            updated_at: 1,
            terminal_at: None,
        };
        assert!(repository.insert(&record).await.unwrap());

        let recovered = repeated_persisted_response(
            repository.get(&request.operation_id).await.unwrap().unwrap(),
            &fingerprint,
            &repository,
            "boot-after-restart",
        )
        .await
        .unwrap();

        assert_eq!(recovered.state, ExternalConversationDispatchState::RecoveryRequired);
        assert_eq!(recovered.error_message.as_deref(), Some("interrupted_by_restart"));
        assert!(recovered.repeated);
        let persisted = repository.get(&request.operation_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, "recovery_required");
        assert_eq!(persisted.boot_id, "boot-after-restart");
    }
}
