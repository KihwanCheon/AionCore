use super::*;

fn snapshot(state: &ConversationRuntimeStateService) -> ConversationRuntimeSummary {
    state.summary_from_parts("conv", Some(ConversationStatus::Finished), true, 0, true)
}

#[test]
fn background_output_can_be_stopped_without_blocking_a_message() {
    let state = Arc::new(ConversationRuntimeStateService::default());
    let background = state
        .register_background_turn("conv", "background", Arc::new(|| {}))
        .unwrap();
    let running = snapshot(&state);
    assert!(running.can_stop && running.can_send_message && !running.is_processing);
    assert_eq!(running.cancel_turn_id.as_deref(), Some("background"));
    assert!(running.turn_id.is_none());
    drop(background);
    assert!(!snapshot(&state).can_stop);
    assert!(snapshot(&state).runtime_revision > running.runtime_revision);
}

#[test]
fn foreground_takeover_rejects_stale_background_stop_and_drop() {
    let state = Arc::new(ConversationRuntimeStateService::default());
    let background = state
        .register_background_turn("conv", "background", Arc::new(|| {}))
        .unwrap();
    let _foreground = state.try_claim_turn("conv", "foreground").unwrap();
    assert!(!state.try_begin_cancel("conv", "background"));
    assert!(state.try_begin_cancel("conv", "foreground"));
    drop(background);
    assert_eq!(snapshot(&state).cancel_turn_id.as_deref(), Some("foreground"));
    assert!(state.is_cancelling("conv"));
}

#[test]
fn background_stop_blocks_new_admission_until_termination() {
    let state = Arc::new(ConversationRuntimeStateService::default());
    let background = state
        .register_background_turn("conv", "background", Arc::new(|| {}))
        .unwrap();
    assert!(state.try_begin_cancel("conv", "background"));
    assert!(state.try_claim_turn("conv", "next").is_err());
    drop(background);
    assert!(state.try_claim_turn("conv", "next").is_ok());
}

#[test]
fn restart_clears_background_target_and_old_guard_cannot_clear_new_target() {
    let state = Arc::new(ConversationRuntimeStateService::default());
    let old = state.register_background_turn("conv", "old", Arc::new(|| {})).unwrap();
    state.begin_restart("conv").unwrap();
    assert!(!snapshot(&state).can_stop);
    state.clear_turn_state_for_restart("conv");
    state.clear_restarting("conv");
    let _new = state.register_background_turn("conv", "new", Arc::new(|| {})).unwrap();
    drop(old);
    assert!(!state.try_begin_cancel("conv", "old"));
    assert_eq!(snapshot(&state).cancel_turn_id.as_deref(), Some("new"));
}
