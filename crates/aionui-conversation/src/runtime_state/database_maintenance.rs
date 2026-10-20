use super::*;

impl ConversationRuntimeState {
    pub(super) fn database_maintenance_active(&self) -> bool {
        self.database_maintenance
            .as_ref()
            .is_some_and(|(_, until)| *until > std::time::Instant::now())
    }
}

impl ConversationRuntimeStateService {
    /// Atomically rejects active work and prevents new turn admission while the
    /// desktop shuts down the backend. Expiry recovers a disconnected desktop.
    pub fn prepare_database_maintenance(&self) -> Result<String, ConversationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::internal("conversation runtime state lock poisoned"))?;
        if state.shutting_down
            || state.database_maintenance_active()
            || !state.active_turns.is_empty()
            || !state.background_turns.is_empty()
            || !state.restarting_conversations.is_empty()
            || !state.deleting_conversations.is_empty()
            || !state.cancelling_conversations.is_empty()
        {
            return Err(ConversationError::Busy {
                reason: "database maintenance requires an idle runtime".into(),
            });
        }
        let token = uuid::Uuid::new_v4().to_string();
        state.database_maintenance = Some((
            token.clone(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        ));
        info!("database maintenance admission gate acquired");
        Ok(token)
    }

    pub fn release_database_maintenance(&self, token: &str) {
        if let Ok(mut state) = self.state.lock()
            && state
                .database_maintenance
                .as_ref()
                .is_some_and(|(owner, _)| owner == token)
        {
            state.database_maintenance = None;
            info!("database maintenance admission gate released");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_turn_prevents_maintenance_without_interrupting_work() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _turn = state.try_claim_turn("conversation", "turn").unwrap();
        assert!(state.prepare_database_maintenance().is_err());
        assert_eq!(state.active_turn_id_for("conversation").as_deref(), Some("turn"));
    }

    #[test]
    fn maintenance_blocks_new_turns_until_owner_releases() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let token = state.prepare_database_maintenance().unwrap();
        assert!(state.try_claim_turn("conversation", "turn").is_err());
        state.release_database_maintenance("different-owner");
        assert!(state.try_claim_turn("conversation", "turn").is_err());
        state.release_database_maintenance(&token);
        assert!(state.try_claim_turn("conversation", "turn").is_ok());
    }

    #[test]
    fn abandoned_maintenance_lease_expires() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.prepare_database_maintenance().unwrap();
        state.state.lock().unwrap().database_maintenance.as_mut().unwrap().1 =
            std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(state.try_claim_turn("conversation", "turn").is_ok());
    }

    #[test]
    fn background_work_and_maintenance_exclude_each_other() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let background = state
            .register_background_turn("conversation", "turn", Arc::new(|| {}))
            .unwrap();
        assert!(state.prepare_database_maintenance().is_err());
        drop(background);
        state.prepare_database_maintenance().unwrap();
        assert!(
            state
                .register_background_turn("conversation", "next", Arc::new(|| {}))
                .is_none()
        );
        assert!(state.begin_restart("conversation").is_err());
    }
}
