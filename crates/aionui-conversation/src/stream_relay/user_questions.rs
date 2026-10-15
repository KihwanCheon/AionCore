//! Fork-only projection of non-blocking questions onto existing messages and notices.

use super::StreamRelay;
use crate::runtime_persistence::RuntimeWriteKind;
use crate::service::choice_notice_row_to_response;
use aionui_ai_agent::protocol::events::{UserInputQuestion, UserQuestionData};
use aionui_api_types::{ChoiceNoticesChangedPayload, WebSocketMessage};
use aionui_common::{ErrorChain, now_ms};
use aionui_db::{ChoiceNoticeRow, DbError, MessageRowUpdate, models::MessageRow};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

impl StreamRelay {
    pub(super) async fn persist_user_questions(&self, data: &UserQuestionData) -> bool {
        if !self.adapter.allows_write(RuntimeWriteKind::AssistantTextFinalize) {
            return false;
        }
        let mut persisted = false;
        for (index, question) in data.questions.iter().enumerate() {
            if question.title.trim().is_empty() {
                continue;
            }
            match self.persist_user_question(&data.item_id, index, question).await {
                Ok(()) => persisted = true,
                Err(error) => warn!(
                    conversation_id = %self.conversation_id,
                    item_id = %data.item_id,
                    question_index = index,
                    error = %ErrorChain(&error),
                    "Failed to persist structured user question"
                ),
            }
        }
        if persisted {
            // Use the same user-scoped snapshot/dismissal path as prose markers.
            // No connected renderer is needed to create these durable notices.
            match self.repo.list_choice_notices(&self.user_id).await {
                Ok(rows) => {
                    let items = rows
                        .into_iter()
                        .map(choice_notice_row_to_response)
                        .collect::<Result<Vec<_>, _>>();
                    match items.and_then(|items| {
                        serde_json::to_value(ChoiceNoticesChangedPayload {
                            user_id: self.user_id.clone(),
                            items,
                        })
                        .map_err(|error| crate::ConversationError::internal(error.to_string()))
                    }) {
                        Ok(payload) => self
                            .broadcaster
                            .broadcast(WebSocketMessage::new("conversation.choiceNoticesChanged", payload)),
                        Err(error) => {
                            warn!(error = %ErrorChain(&error), "Failed to serialize structured question notices")
                        }
                    }
                }
                Err(error) => warn!(error = %ErrorChain(&error), "Failed to load structured question notices"),
            }
            info!(conversation_id = %self.conversation_id, item_id = %data.item_id,
                question_count = data.questions.len(), "Structured user questions synchronized");
        }
        persisted
    }

    async fn persist_user_question(
        &self,
        item_id: &str,
        index: usize,
        question: &UserInputQuestion,
    ) -> Result<(), DbError> {
        // Replay uses the same message and tombstone key, even after a new relay
        // or app restart. Hash a tuple to avoid delimiter/length collisions.
        let identity = json!([self.conversation_id, item_id]).to_string();
        let message_id = format!("question-{:x}-{index:08}", Sha256::digest(identity.as_bytes()));
        let existing = self
            .repo
            .get_message(&self.user_id, &self.conversation_id, &message_id)
            .await?;
        let created_at = existing.as_ref().map_or_else(now_ms, |message| message.created_at);
        let mut text = question.title.clone();
        for option in question.options.iter().flatten() {
            text.push_str("\n- ");
            text.push_str(option);
        }
        let content = json!({"content": text, "user_choice": question});
        if existing.is_some() {
            self.repo
                .update_message(
                    &self.user_id,
                    &self.conversation_id,
                    &message_id,
                    &MessageRowUpdate {
                        content: Some(content.to_string()),
                        status: Some(Some("finish".into())),
                        hidden: Some(false),
                    },
                )
                .await?;
        } else {
            // This insert validates the owner inside its transaction. The
            // generic upsert only scopes its conflict-update branch.
            self.repo
                .insert_message(
                    &self.user_id,
                    &MessageRow {
                        id: message_id.clone(),
                        conversation_id: self.conversation_id.clone(),
                        msg_id: Some(message_id.clone()),
                        r#type: "text".into(),
                        content: content.to_string(),
                        position: Some("left".into()),
                        status: Some("finish".into()),
                        hidden: false,
                        created_at,
                        backend_turn_id: self.adapter.current_backend_turn_id(),
                    },
                )
                .await?;
        }

        // Publish only after the anchor is queryable by a newly opened chat.
        // Replacement makes a repeated backend item idempotent in the live UI.
        self.broadcast_stream_payload(json!({
            "conversation_id": self.conversation_id, "msg_id": message_id,
            "type": "content", "data": content, "replace": true,
            "hidden": false, "position": "left", "status": "finish", "created_at": created_at,
        }));

        let preview: String = question
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(320)
            .collect();
        self.repo
            .upsert_choice_notice(&ChoiceNoticeRow {
                user_id: self.user_id.clone(),
                key: json!([self.conversation_id, message_id, 0]).to_string(),
                conversation_id: self.conversation_id.clone(),
                message_id,
                // Each structured question owns a row: index zero means its start.
                marker_index: 0,
                preview,
                status: "pending".into(),
                created_at,
                updated_at: now_ms(),
            })
            .await
    }
}

#[cfg(test)]
#[path = "user_questions_tests.rs"]
mod tests;
