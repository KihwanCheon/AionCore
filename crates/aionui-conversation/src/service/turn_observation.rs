use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;
use tracing::warn;

use super::{ConversationAgentTurnOutcome, ConversationAgentTurnStatus, ConversationService};
use crate::turn_orchestrator::ConversationTurnStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedTurnTerminal {
    turn_id: String,
    status: ConversationTurnStatus,
    error_message: Option<String>,
}

#[derive(Debug, Default)]
struct TurnObservationState {
    terminal_turns: HashMap<String, ObservedTurnTerminal>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TurnObservationService {
    state: Arc<Mutex<TurnObservationState>>,
    notify: Arc<Notify>,
}

impl TurnObservationService {
    fn record(&self, conversation_id: &str, terminal: ObservedTurnTerminal) {
        match self.state.lock() {
            Ok(mut state) => {
                state.terminal_turns.insert(conversation_id.to_owned(), terminal);
                drop(state);
                self.notify.notify_waiters();
            }
            Err(_) => warn!(
                conversation_id,
                "conversation turn observation lock poisoned while recording terminal turn"
            ),
        }
    }

    async fn wait_after(&self, conversation_id: &str, excluded_turn_id: &str) -> ObservedTurnTerminal {
        loop {
            let notified = self.notify.notified();
            let observed = self
                .state
                .lock()
                .ok()
                .and_then(|state| state.terminal_turns.get(conversation_id).cloned());
            if let Some(terminal) = observed
                && terminal.turn_id != excluded_turn_id
            {
                return terminal;
            }
            notified.await;
        }
    }
}

impl ConversationService {
    pub(crate) fn record_agent_turn_terminal(
        &self,
        conversation_id: &str,
        turn_id: &str,
        status: ConversationTurnStatus,
        error_message: Option<String>,
    ) {
        self.turn_observation.record(
            conversation_id,
            ObservedTurnTerminal {
                turn_id: turn_id.to_owned(),
                status,
                error_message,
            },
        );
    }

    pub async fn wait_for_agent_turn_after(
        &self,
        conversation_id: &str,
        excluded_turn_id: &str,
    ) -> ConversationAgentTurnOutcome {
        let terminal = self
            .turn_observation
            .wait_after(conversation_id, excluded_turn_id)
            .await;
        ConversationAgentTurnOutcome {
            runtime: self.runtime_summary_for(conversation_id).await,
            conversation_id: conversation_id.to_owned(),
            turn_id: terminal.turn_id,
            status: if terminal.status == ConversationTurnStatus::Failed {
                ConversationAgentTurnStatus::Failed
            } else {
                ConversationAgentTurnStatus::Completed
            },
            interrupted: terminal.status == ConversationTurnStatus::Interrupted,
            error_message: terminal.error_message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_ignores_interrupted_turn_and_observes_follow_up_turn() {
        let observations = TurnObservationService::default();
        observations.record(
            "conv-1",
            ObservedTurnTerminal {
                turn_id: "turn-interrupted".to_owned(),
                status: ConversationTurnStatus::Interrupted,
                error_message: None,
            },
        );

        let waiter = {
            let observations = observations.clone();
            tokio::spawn(async move { observations.wait_after("conv-1", "turn-interrupted").await })
        };
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "the interrupted turn itself must not satisfy the follow-up wait"
        );

        observations.record(
            "conv-1",
            ObservedTurnTerminal {
                turn_id: "turn-resumed".to_owned(),
                status: ConversationTurnStatus::Completed,
                error_message: None,
            },
        );
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("follow-up turn should wake the waiter")
            .expect("waiter task should complete");
        assert_eq!(observed.turn_id, "turn-resumed");
        assert_eq!(observed.status, ConversationTurnStatus::Completed);
    }
}
