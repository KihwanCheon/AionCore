//! Fallible native-history cleanup before local conversation deletion.

use aionui_db::models::{AcpSessionRow, ConversationRow};
use std::sync::Arc;

use super::{ConversationError, ConversationService};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeHistoryOutcome {
    Deleted,
    RetainedReference,
}

#[async_trait::async_trait]
pub trait NativeHistoryCleaner: Send + Sync {
    async fn delete_codex_history(
        &self,
        user_id: &str,
        session: &AcpSessionRow,
        protected_session_ids: &[String],
    ) -> Result<NativeHistoryOutcome, ConversationError>;
}

impl ConversationService {
    pub fn with_native_history_cleaner(mut self, cleaner: Arc<dyn NativeHistoryCleaner>) -> Self {
        self.native_history_cleaner = Some(cleaner);
        self
    }

    pub(super) async fn delete_native_history(
        &self,
        user_id: &str,
        conversation: &ConversationRow,
    ) -> Result<(), ConversationError> {
        let Some(session) = self.acp_session_repo.get_for_user(user_id, &conversation.id).await? else {
            return Ok(());
        };
        if session.session_id.as_deref().is_none_or(|id| id.trim().is_empty()) {
            return Ok(());
        }
        let Some(metadata) = self
            .agent_metadata_repo
            .get_for_user(user_id, &session.agent_id)
            .await?
        else {
            return Err(ConversationError::internal(
                "Cannot verify the native session's agent; keeping history",
            ));
        };
        if aionui_db::runtime_backend_for_agent(&metadata) != "codex" {
            return Ok(());
        }
        let cleaner = self.native_history_cleaner.as_ref().ok_or_else(|| {
            ConversationError::internal("Codex history cleanup is unavailable; conversation was kept")
        })?;
        let protected = self
            .acp_session_repo
            .protected_native_session_ids(&conversation.id)
            .await?;
        let outcome = cleaner.delete_codex_history(user_id, &session, &protected).await?;
        tracing::info!(
            conversation_id = %conversation.id,
            native_thread_id = ?session.session_id,
            ?outcome,
            "Native Codex history cleanup completed"
        );
        Ok(())
    }
}
