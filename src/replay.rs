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
use crate::journal::{Grammar, JournalEvent, LoadedJournal, SessionLimits, request_sha256};
use crate::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, ProviderInfo, ToolCall,
};
use serde_json::Value;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReplayError {
    #[error("journal does not start with session_started")]
    MissingSessionStarted,
    #[error("journal is not a valid session: {0}")]
    Grammar(String),
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
    limits: SessionLimits,
    turns: VecDeque<RecordedTurn>,
    tools: VecDeque<RecordedTool>,
    final_answer: Option<String>,
    outcome: Option<String>,
    diverged_at: Option<u32>,
    /// Requests and tool calls the recording did not match.
    unexpected: u32,
}

#[derive(Debug, Clone)]
struct RecordedTool {
    turn: u32,
    index: u32,
    call: ToolCall,
    ok: bool,
    result: Value,
}

impl ReplayProvider {
    /// Collect each recorded turn's request hash and response or failure.
    ///
    /// A trailing request without a response (an interrupted recording) is
    /// dropped; replay reaching it returns a `Script` error.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal does not start a session or breaks
    /// the session grammar.
    pub fn from_journal(journal: &LoadedJournal) -> Result<Self, ReplayError> {
        // `LoadedJournal` fields are public, so check the grammar here too
        // rather than trusting that the records came from `read_journal`.
        let mut grammar = Grammar::default();
        for record in &journal.records {
            grammar
                .feed(record.sequence, &record.event)
                .map_err(|error| ReplayError::Grammar(error.to_string()))?;
        }
        let mut records = journal.records.iter();
        let Some(JournalEvent::SessionStarted {
            provider,
            system,
            task,
            limits,
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
            limits: *limits,
            turns: VecDeque::new(),
            tools: VecDeque::new(),
            final_answer: None,
            outcome: None,
            diverged_at: None,
            unexpected: 0,
        };
        // The grammar has paired every request with one answer in turn order.
        let mut pending: Option<(u32, String)> = None;
        for record in records {
            match &record.event {
                JournalEvent::ModelRequested {
                    turn,
                    request_sha256,
                    ..
                } => pending = Some((*turn, request_sha256.clone())),
                JournalEvent::ModelResponded { response, .. } => {
                    replay.push_turn(&mut pending, RecordedOutcome::Responded(response.clone()));
                }
                JournalEvent::ModelFailed { kind, message, .. } => replay.push_turn(
                    &mut pending,
                    RecordedOutcome::Failed {
                        kind: *kind,
                        message: message.clone(),
                    },
                ),
                JournalEvent::ToolExecuted {
                    turn,
                    index,
                    call,
                    ok,
                    result,
                    ..
                } => replay.tools.push_back(RecordedTool {
                    turn: *turn,
                    index: *index,
                    call: call.clone(),
                    ok: *ok,
                    result: result.clone(),
                }),
                JournalEvent::SessionEnded {
                    outcome,
                    final_answer,
                    ..
                } => {
                    replay.outcome = Some(outcome.clone());
                    replay.final_answer.clone_from(final_answer);
                }
                JournalEvent::SessionStarted { .. } | JournalEvent::SandboxReady { .. } => {}
            }
        }
        Ok(replay)
    }

    /// The recorded system prompt and task.
    #[must_use]
    pub fn task(&self) -> SessionTask {
        self.task.clone()
    }

    /// The recorded limits.
    #[must_use]
    pub fn limits(&self) -> SessionLimits {
        self.limits
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

    fn push_turn(&mut self, pending: &mut Option<(u32, String)>, outcome: RecordedOutcome) {
        if let Some((turn, request_sha256)) = pending.take() {
            self.turns.push_back(RecordedTurn {
                turn,
                request_sha256,
                outcome,
            });
        }
    }

    /// True once every recorded model turn and tool call has been replayed.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.turns.is_empty() && self.tools.is_empty() && self.unexpected == 0
    }

    /// Record a request or tool call the recording does not match. The first
    /// one sets `diverged_at`.
    fn mark_unexpected(&mut self, turn: u32) {
        self.unexpected = self.unexpected.saturating_add(1);
        self.diverged_at.get_or_insert(turn);
    }

    /// The first turn whose request differed from the recording.
    #[must_use]
    pub fn diverged_at(&self) -> Option<u32> {
        self.diverged_at
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn tool_completed(
        &mut self,
        turn: u32,
        index: u32,
        call: &ToolCall,
        ok: bool,
        result: &Value,
    ) -> Result<(), ProviderError> {
        // A tool that answers differently diverges here, before its result
        // reaches the next request. Only a matching call consumes its
        // record; any other call, including one with no recorded
        // counterpart, counts as unexpected and keeps the replay unexhausted.
        let matches = self.tools.front().is_some_and(|recorded| {
            recorded.turn == turn
                && recorded.index == index
                && recorded.call == *call
                && recorded.ok == ok
                && recorded.result == *result
        });
        if matches {
            self.tools.pop_front();
            return Ok(());
        }
        self.mark_unexpected(turn);
        Err(ProviderError::new(
            ProviderErrorKind::ReplayDiverged,
            format!("turn {turn} tool call {index} differs from the recording"),
        ))
    }

    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let Some(recorded) = self.turns.pop_front() else {
            self.unexpected = self.unexpected.saturating_add(1);
            return Err(ProviderError::new(
                ProviderErrorKind::Script,
                format!("recording has no response for turn {}", request.turn),
            ));
        };
        if request.turn != recorded.turn || request_sha256(&request) != recorded.request_sha256 {
            self.mark_unexpected(request.turn);
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
