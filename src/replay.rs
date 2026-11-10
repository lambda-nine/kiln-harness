//! Deterministic replay of a recorded session.
//!
//! [`ReplayProvider`] answers each model request with the response recorded
//! in a journal, after checking that the request is byte-for-byte the one the
//! recording saw. A mismatch means the sandbox produced different tool
//! results than it did during the recording.

use std::collections::VecDeque;

use async_trait::async_trait;
use thiserror::Error;

use crate::engine::SessionTask;
use crate::journal::{BudgetSnapshot, JournalEvent, LoadedJournal, request_sha256};
use crate::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, ProviderInfo,
};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReplayError {
    #[error("journal does not start with session_started")]
    MissingSessionStarted,
    #[error(
        "journal record {sequence} answers turn {found} without a matching request ({expected:?})"
    )]
    UnpairedResponse {
        sequence: u64,
        found: u32,
        expected: Option<u32>,
    },
    #[error("journal record {sequence} requests turn {found}, expected turn {expected}")]
    TurnOrder {
        sequence: u64,
        found: u32,
        expected: u32,
    },
}

#[derive(Debug, Clone)]
enum RecordedOutcome {
    Responded(ModelResponse),
    Failed {
        kind: ProviderErrorKind,
        message: String,
    },
}

#[derive(Debug, Clone)]
struct RecordedTurn {
    turn: u32,
    request_sha256: String,
    outcome: RecordedOutcome,
}

#[derive(Debug, Clone)]
pub struct ReplayProvider {
    info: ProviderInfo,
    task: SessionTask,
    budgets: BudgetSnapshot,
    turns: VecDeque<RecordedTurn>,
    final_answer: Option<String>,
    outcome: Option<String>,
    diverged_at: Option<u32>,
}

impl ReplayProvider {
    /// Collect each recorded turn's request hash and response or failure.
    ///
    /// A trailing request without a response (an interrupted recording) is
    /// dropped; replay reaching it returns a `Script` error.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal does not start a session or its model
    /// turns are out of order or unpaired.
    pub fn from_journal(journal: &LoadedJournal) -> Result<Self, ReplayError> {
        let mut records = journal.records.iter();
        let Some(JournalEvent::SessionStarted {
            provider,
            system,
            task,
            budgets,
            ..
        }) = records.next().map(|record| &record.event)
        else {
            return Err(ReplayError::MissingSessionStarted);
        };
        let mut replay = Self {
            info: provider.clone(),
            task: SessionTask {
                system: system.clone(),
                prompt: task.clone(),
            },
            budgets: *budgets,
            turns: VecDeque::new(),
            final_answer: None,
            outcome: None,
            diverged_at: None,
        };
        let mut pending: Option<(u32, String)> = None;
        let mut next_turn = 1u32;
        for record in records {
            let sequence = record.sequence;
            let outcome = match &record.event {
                JournalEvent::ModelRequested {
                    turn,
                    request_sha256,
                    ..
                } => {
                    if pending.is_some() || *turn != next_turn {
                        return Err(ReplayError::TurnOrder {
                            sequence,
                            found: *turn,
                            expected: next_turn,
                        });
                    }
                    pending = Some((*turn, request_sha256.clone()));
                    next_turn = next_turn.saturating_add(1);
                    continue;
                }
                JournalEvent::ModelResponded { turn, response, .. } => {
                    (*turn, RecordedOutcome::Responded(response.clone()))
                }
                JournalEvent::ModelFailed {
                    turn,
                    kind,
                    message,
                } => (
                    *turn,
                    RecordedOutcome::Failed {
                        kind: *kind,
                        message: message.clone(),
                    },
                ),
                JournalEvent::SessionEnded {
                    outcome,
                    final_answer,
                    ..
                } => {
                    replay.outcome = Some(outcome.clone());
                    replay.final_answer.clone_from(final_answer);
                    continue;
                }
                JournalEvent::SessionStarted { .. }
                | JournalEvent::SandboxReady { .. }
                | JournalEvent::ToolExecuted { .. } => continue,
            };
            let (turn, outcome) = outcome;
            match pending.take() {
                Some((requested, request_sha256)) if requested == turn => {
                    replay.turns.push_back(RecordedTurn {
                        turn,
                        request_sha256,
                        outcome,
                    });
                }
                other => {
                    return Err(ReplayError::UnpairedResponse {
                        sequence,
                        found: turn,
                        expected: other.map(|(requested, _)| requested),
                    });
                }
            }
        }
        Ok(replay)
    }

    /// The recorded system prompt and task.
    #[must_use]
    pub fn task(&self) -> SessionTask {
        self.task.clone()
    }

    /// The recorded budgets.
    #[must_use]
    pub fn budgets(&self) -> BudgetSnapshot {
        self.budgets
    }

    /// The recorded final answer, if the recording completed with one.
    #[must_use]
    pub fn recorded_final_answer(&self) -> Option<&str> {
        self.final_answer.as_deref()
    }

    /// The recorded session outcome, if the recording is complete.
    #[must_use]
    pub fn recorded_outcome(&self) -> Option<&str> {
        self.outcome.as_deref()
    }

    /// The first turn whose request differed from the recording.
    #[must_use]
    pub fn diverged_at(&self) -> Option<u32> {
        self.diverged_at
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let Some(recorded) = self.turns.pop_front() else {
            return Err(ProviderError::new(
                ProviderErrorKind::Script,
                format!("recording has no response for turn {}", request.turn),
            ));
        };
        if request.turn != recorded.turn || request_sha256(&request) != recorded.request_sha256 {
            self.diverged_at = Some(request.turn);
            return Err(ProviderError::new(
                ProviderErrorKind::ReplayDiverged,
                format!("turn {} request differs from the recording", request.turn),
            ));
        }
        match recorded.outcome {
            RecordedOutcome::Responded(response) => Ok(response),
            RecordedOutcome::Failed { kind, message } => Err(ProviderError::new(kind, message)),
        }
    }
}
