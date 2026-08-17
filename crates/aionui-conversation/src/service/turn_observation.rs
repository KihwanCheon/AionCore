use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use aionui_api_types::AgentErrorCode;
use tokio::sync::Notify;
use tracing::warn;

use super::{ConversationAgentTurnOutcome, ConversationAgentTurnStatus, ConversationService};
use crate::stream_relay::RelayOutcome;
use crate::turn_orchestrator::ConversationTurnStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedTurnTerminal {
    turn_id: String,
    status: ConversationTurnStatus,
    error_message: Option<String>,
    resume_required: bool,
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
        self.record_agent_turn_terminal_with_resume(conversation_id, turn_id, status, error_message, false);
    }

    pub(crate) fn record_agent_turn_terminal_with_resume(
        &self,
        conversation_id: &str,
        turn_id: &str,
        status: ConversationTurnStatus,
        error_message: Option<String>,
        resume_required: bool,
    ) {
        self.turn_observation.record(
            conversation_id,
            ObservedTurnTerminal {
                turn_id: turn_id.to_owned(),
                status,
                error_message,
                resume_required,
            },
        );
    }

    pub(crate) fn external_dispatch_requires_manual_resume(outcome: &RelayOutcome) -> bool {
        outcome.terminal.code() == Some(AgentErrorCode::UserAgentDisconnected)
            && outcome.terminal.retryable() == Some(true)
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
            resume_required: terminal.resume_required,
            error_message: terminal.error_message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_relay::{RelayTerminal, TurnAttemptSummary};

    #[test]
    fn only_retryable_agent_disconnect_requires_a_manual_resume() {
        let retryable_disconnect = RelayOutcome {
            system_responses: vec![],
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UserAgentDisconnected),
                retryable: Some(true),
            },
            attempt: TurnAttemptSummary::default(),
        };
        assert!(ConversationService::external_dispatch_requires_manual_resume(
            &retryable_disconnect
        ));

        let non_retryable_disconnect = RelayOutcome {
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UserAgentDisconnected),
                retryable: Some(false),
            },
            ..retryable_disconnect.clone()
        };
        assert!(!ConversationService::external_dispatch_requires_manual_resume(
            &non_retryable_disconnect
        ));

        let provider_timeout = RelayOutcome {
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UserLlmProviderTimeout),
                retryable: Some(true),
            },
            ..retryable_disconnect
        };
        assert!(!ConversationService::external_dispatch_requires_manual_resume(
            &provider_timeout
        ));
    }

    #[tokio::test]
    async fn wait_ignores_interrupted_turn_and_observes_follow_up_turn() {
        let observations = TurnObservationService::default();
        observations.record(
            "conv-1",
            ObservedTurnTerminal {
                turn_id: "turn-interrupted".to_owned(),
                status: ConversationTurnStatus::Interrupted,
                error_message: None,
                resume_required: false,
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
                resume_required: false,
            },
        );
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("follow-up turn should wake the waiter")
            .expect("waiter task should complete");
        assert_eq!(observed.turn_id, "turn-resumed");
        assert_eq!(observed.status, ConversationTurnStatus::Completed);
    }

    #[tokio::test]
    async fn wait_preserves_retryable_disconnect_as_resume_required() {
        let observations = TurnObservationService::default();
        observations.record(
            "conv-1",
            ObservedTurnTerminal {
                turn_id: "turn-disconnected".to_owned(),
                status: ConversationTurnStatus::Failed,
                error_message: Some("Agent process disconnected".to_owned()),
                resume_required: true,
            },
        );

        let terminal = observations.wait_after("conv-1", "turn-before-disconnect").await;
        assert_eq!(terminal.status, ConversationTurnStatus::Failed);
        assert!(terminal.resume_required);
        assert_eq!(terminal.error_message.as_deref(), Some("Agent process disconnected"));

        let waiter = {
            let observations = observations.clone();
            tokio::spawn(async move { observations.wait_after("conv-1", "turn-disconnected").await })
        };
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "the disconnected turn itself must not close the resumed dispatch"
        );

        observations.record(
            "conv-1",
            ObservedTurnTerminal {
                turn_id: "turn-manual-retry".to_owned(),
                status: ConversationTurnStatus::Completed,
                error_message: None,
                resume_required: false,
            },
        );
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("the manual retry should wake the external dispatch")
            .expect("waiter task should complete");
        assert_eq!(resumed.turn_id, "turn-manual-retry");
        assert_eq!(resumed.status, ConversationTurnStatus::Completed);
        assert!(!resumed.resume_required);
    }
}
