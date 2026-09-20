use aionui_api_types::{ExternalConversationReportRequest, ExternalConversationReportResponse};
use aionui_common::now_ms;
use aionui_db::models::MessageRow;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::ConversationService;
use crate::ConversationError;

impl ConversationService {
    /// History-only delivery deliberately does not touch the runtime, leases or agent queue.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn append_external_report(
        &self,
        user_id: &str,
        conversation_id: &str,
        request: ExternalConversationReportRequest,
    ) -> Result<ExternalConversationReportResponse, ConversationError> {
        if request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || !request
                .operation_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_:./".contains(&c))
            || request.content.trim().is_empty()
            || request.content.len() > 256 * 1024
        {
            tracing::warn!("invalid external report payload");
            return Err(ConversationError::bad_request("Invalid external report payload"));
        }
        self.validate_external_dispatch_target(user_id, conversation_id).await?;
        let message_id = format!(
            "external-report-{:x}",
            Sha256::digest(serde_json::to_vec(&(conversation_id, &request.operation_id)).expect("string tuple"))
        );
        let content_hash = format!("{:x}", Sha256::digest(request.content.as_bytes()));
        let row = MessageRow {
            id: message_id.clone(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(message_id.clone()),
            r#type: "text".into(),
            content: json!({ "content": request.content, "external_report_operation_id": request.operation_id })
                .to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: None,
        };
        let existing = self
            .conversation_repo
            .get_message(user_id, conversation_id, &message_id)
            .await?;
        let repeated = if let Some(existing) = existing {
            ensure_same_report(&existing, &row)?;
            true
        } else {
            // The DB's unique primary key arbitrates concurrent requests, including
            // retries after an HTTP response is lost. Never upsert immutable reports.
            match self.conversation_repo.insert_message(user_id, &row).await {
                Ok(()) => {
                    self.broadcast_raw_message(user_id, &row);
                    false
                }
                Err(error) if error.is_unique_violation() => {
                    let existing = self
                        .conversation_repo
                        .get_message(user_id, conversation_id, &message_id)
                        .await?
                        .ok_or_else(|| {
                            ConversationError::internal("External report disappeared after insert conflict")
                        })?;
                    ensure_same_report(&existing, &row)?;
                    true
                }
                Err(error) => {
                    tracing::error!(message_id, "external report persistence failed");
                    return Err(error.into());
                }
            }
        };
        tracing::info!(
            message_id,
            repeated,
            "external report persisted without agent execution"
        );
        Ok(ExternalConversationReportResponse {
            operation_id: request.operation_id,
            conversation_id: conversation_id.to_owned(),
            message_id,
            content_hash,
            repeated,
            execution_requested: false,
        })
    }

    /// Only a visible, immutable report with identical content can suppress the
    /// normal user-message insert. Arbitrary message IDs must never hide a prompt.
    pub(crate) async fn validate_external_report_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        instruction: &str,
    ) -> Result<(), ConversationError> {
        let row = self
            .conversation_repo
            .get_message(user_id, conversation_id, message_id)
            .await?
            .ok_or_else(|| ConversationError::MessageNotFound {
                id: message_id.to_owned(),
            })?;
        let content: serde_json::Value = serde_json::from_str(&row.content).unwrap_or_default();
        if !message_id.starts_with("external-report-")
            || row.hidden
            || row.r#type != "text"
            || row.position.as_deref() != Some("right")
            || content["external_report_operation_id"].as_str().is_none()
            || content["content"].as_str() != Some(instruction)
        {
            return Err(ConversationError::bad_request(
                "The visible external report does not match the instruction",
            ));
        }
        Ok(())
    }
}

fn ensure_same_report(existing: &MessageRow, expected: &MessageRow) -> Result<(), ConversationError> {
    if existing.content != expected.content
        || existing.hidden
        || existing.r#type != expected.r#type
        || existing.position != expected.position
        || existing.msg_id != expected.msg_id
    {
        tracing::warn!(message_id = %expected.id, "external report idempotency conflict");
        return Err(ConversationError::Busy {
            reason: "External report operation ID already has different content".into(),
        });
    }
    Ok(())
}
