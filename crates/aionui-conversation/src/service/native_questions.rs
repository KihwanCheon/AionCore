//! Persistence shared by native question delivery and its dedicated answer endpoint.

use aionui_api_types::WebSocketMessage;
use aionui_db::{IConversationRepository, MessageRowUpdate, models::MessageRow};
use aionui_realtime::EventBroadcaster;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ConversationError;

pub(crate) fn native_question_message_id(conversation_id: &str, request_id: &str) -> String {
    let identity = json!([conversation_id, request_id]).to_string();
    format!("ask-{:x}", Sha256::digest(identity.as_bytes()))
}

pub(crate) fn broadcast_native_question(broadcaster: &dyn EventBroadcaster, user_id: &str, row: &MessageRow) {
    if let Ok(content) = serde_json::from_str::<Value>(&row.content) {
        broadcaster.broadcast(WebSocketMessage::new(
            "message.stream",
            json!({
                "user_id": user_id, "conversation_id": row.conversation_id,
                "msg_id": row.id, "type": "ask", "data": content,
                "replace": true, "position": "left", "status": "finish",
                "hidden": false, "created_at": row.created_at, "backend_turn_id": row.backend_turn_id,
            }),
        ));
    }
}

/// Record the same answer acceptance already acknowledged by the REST endpoint.
/// Notice dismissal remains independent and never dispatches an answer.
pub(crate) async fn record_native_question_answer(
    repo: &dyn IConversationRepository,
    broadcaster: &dyn EventBroadcaster,
    user_id: &str,
    conversation_id: &str,
    request_id: &str,
    answers: Option<Vec<Value>>,
) -> Result<(), ConversationError> {
    let id = native_question_message_id(conversation_id, request_id);
    let Some(mut row) = repo.get_message(user_id, conversation_id, &id).await? else {
        // Older runtimes can still answer their transient question cards.
        return Ok(());
    };
    if row.r#type != "ask" {
        return Err(ConversationError::internal(
            "Native question anchor has an unexpected type",
        ));
    }
    let mut content: Value =
        serde_json::from_str(&row.content).map_err(|error| ConversationError::internal(error.to_string()))?;
    if content["request_id"] != request_id {
        return Err(ConversationError::internal(
            "Native question anchor has a different request id",
        ));
    }
    content["resolution"] = json!(if answers.is_some() { "answered" } else { "declined" });
    if let Some(answers) = answers {
        content["answers"] = json!(answers);
    }
    row.content = content.to_string();
    repo.update_message(
        user_id,
        conversation_id,
        &id,
        &MessageRowUpdate {
            content: Some(row.content.clone()),
            ..Default::default()
        },
    )
    .await?;
    broadcast_native_question(broadcaster, user_id, &row);
    Ok(())
}
