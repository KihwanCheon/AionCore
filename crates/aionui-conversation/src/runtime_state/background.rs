use super::*;

/// Tracks stoppable output without taking the user-message admission claim.
pub(crate) struct BackgroundTurn {
    conversation_id: String,
    turn_id: String,
    state: Weak<ConversationRuntimeStateService>,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl ConversationRuntimeStateService {
    pub(crate) fn register_background_turn(
        self: &Arc<Self>,
        conversation_id: &str,
        turn_id: &str,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<BackgroundTurn> {
        let mut state = self.state.lock().ok()?;
        if state.shutting_down
            || state.database_maintenance_active()
            || state.deleting_conversations.contains(conversation_id)
            || state.restarting_conversations.contains(conversation_id)
            || state.cancelling_conversations.contains(conversation_id)
            || state.active_turns.contains_key(conversation_id)
        {
            return None;
        }
        state
            .background_turns
            .insert(conversation_id.to_owned(), turn_id.to_owned());
        state.revision += 1;
        drop(state);
        info!(conversation_id, turn_id, "background execution became cancellable");
        on_change();
        Some(BackgroundTurn {
            conversation_id: conversation_id.to_owned(),
            turn_id: turn_id.to_owned(),
            state: Arc::downgrade(self),
            on_change,
        })
    }

    pub fn cancel_turn_id_for(&self, conversation_id: &str) -> Option<String> {
        self.state.lock().ok().and_then(|state| {
            state
                .active_turns
                .get(conversation_id)
                .or_else(|| state.background_turns.get(conversation_id))
                .cloned()
        })
    }

    /// Validate and close admission in one lock so a stale stop cannot cancel
    /// a foreground turn that took over from a background continuation.
    pub(crate) fn try_begin_cancel(&self, conversation_id: &str, turn_id: &str) -> bool {
        let Ok(mut state) = self.state.lock() else { return false };
        let current = state
            .active_turns
            .get(conversation_id)
            .or_else(|| state.background_turns.get(conversation_id));
        if current.map(String::as_str) != Some(turn_id)
            || state.deleting_conversations.contains(conversation_id)
            || state.restarting_conversations.contains(conversation_id)
            || state.shutting_down
        {
            return false;
        }
        state.cancelling_conversations.insert(conversation_id.to_owned());
        state.revision += 1;
        info!(conversation_id, turn_id, "cancellable execution accepted stop");
        true
    }
}

impl Drop for BackgroundTurn {
    fn drop(&mut self) {
        let Some(runtime) = self.state.upgrade() else { return };
        let Ok(mut state) = runtime.state.lock() else { return };
        if state.background_turns.get(&self.conversation_id) != Some(&self.turn_id) {
            return;
        }
        state.background_turns.remove(&self.conversation_id);
        if !state.active_turns.contains_key(&self.conversation_id) {
            state.cancelling_conversations.remove(&self.conversation_id);
        }
        state.revision += 1;
        drop(state);
        runtime.release_notify.notify_waiters();
        info!(conversation_id = %self.conversation_id, turn_id = %self.turn_id,
            "background execution released cancellation target");
        (self.on_change)();
    }
}
