//! Deterministic replay of a recorded session.
//!
//! [`ReplayProvider`] answers each model request with the response recorded
//! in a journal, after checking that the request is byte-for-byte the one the
//! recording saw. A mismatch means the sandbox produced different tool
//! results than it did during the recording.
//!
//! Model turns and compactions are answered from one queue in recorded
//! order, so a compaction the replay asks for where the recording made a
//! model request, or the other way round, diverges. A recorded summary is
//! returned as recorded; no model is asked to write it again.

use std::collections::VecDeque;

use async_trait::async_trait;
use thiserror::Error;

use crate::api::SandboxApi;
use crate::audit::AuditSink;
use crate::compaction::CompactionPolicy;
use crate::engine::{CheckOutcome, Harness, HarnessError, SessionOutcome, SessionTask};
use crate::journal::JournalSink;
use crate::journal::{
    Grammar, JournalEvent, LoadedJournal, RecordedSeedFile, SessionLimits,
    compaction_request_sha256, request_sha256,
};
use crate::provider::{
    CompactionResponse, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind,
    ProviderInfo, ToolCall,
};
use crate::tools::ExecContext;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReplayError {
    #[error("journal does not start with session_started")]
    MissingSessionStarted,
    #[error("journal is not a valid session: {0}")]
    Grammar(String),
    #[error("journal records a seed or check that does not decode")]
    InvalidTask,
}

#[derive(Debug, Clone)]
enum RecordedOutcome {
    Responded(ModelResponse),
    Compacted(CompactionResponse),
    Failed {
        kind: ProviderErrorKind,
        message: String,
    },
    /// The recording ends while this request was outstanding, for example
    /// at a deadline during the provider call.
    Unanswered,
}

#[derive(Debug, Clone)]
struct RecordedTurn {
    turn: u32,
    /// A compaction request rather than a model request.
    compaction: bool,
    request_sha256: String,
    outcome: RecordedOutcome,
}

#[derive(Debug, Clone)]
pub struct ReplayProvider {
    info: ProviderInfo,
    task: SessionTask,
    /// Where the recording's executions ran; [`Harness::replay`] runs
    /// them there again.
    exec_context: ExecContext,
    limits: SessionLimits,
    /// The recorded compaction policy, restored for the replay.
    compaction: Option<CompactionPolicy>,
    /// The idle timeout the recorded session sent kilnd, if it used expiry.
    sandbox_idle_ms: Option<u64>,
    /// Model requests and compactions, in recorded order.
    turns: VecDeque<RecordedTurn>,
    tools: VecDeque<RecordedTool>,
    final_answer: Option<String>,
    outcome: Option<String>,
    diverged_at: Option<u32>,
    /// Requests and tool calls the recording did not match.
    unexpected: u32,
    /// The recorded check's outcome, when the recording ran one.
    recorded_check: Option<CheckOutcome>,
    /// The replayed check's verdict differed from the recorded one.
    check_diverged: bool,
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
    /// kept as a turn without an answer: `exhausted()` stays false until
    /// replay reaches it, and reaching it returns a `Script` error.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal does not start a session or breaks
    /// the session grammar.
    #[allow(clippy::too_many_lines)]
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
            seed,
            check,
            exec_context,
            compaction,
            ..
        }) = records.next().map(|record| &record.event)
        else {
            return Err(ReplayError::MissingSessionStarted);
        };
        // The grammar checked that both decode; a forged record that
        // bypassed it is refused here as well.
        let seed = seed
            .iter()
            .map(RecordedSeedFile::decode)
            .collect::<Option<Vec<_>>>()
            .ok_or(ReplayError::InvalidTask)?;
        let check = check
            .as_ref()
            .map(|check| check.decode().ok_or(ReplayError::InvalidTask))
            .transpose()?;
        let mut replay = Self {
            info: provider.clone(),
            task: SessionTask {
                system: system.clone(),
                prompt: task.clone(),
                seed,
                check,
            },
            exec_context: recorded_exec_context(exec_context.as_deref())?,
            limits: *limits,
            compaction: compaction.clone(),
            sandbox_idle_ms: None,
            turns: VecDeque::new(),
            tools: VecDeque::new(),
            final_answer: None,
            outcome: None,
            diverged_at: None,
            unexpected: 0,
            recorded_check: None,
            check_diverged: false,
        };
        // The grammar has paired every request with one answer in turn order.
        let mut pending: Option<Pending> = None;
        for record in records {
            match &record.event {
                JournalEvent::ModelRequested {
                    turn,
                    request_sha256,
                    ..
                }
                | JournalEvent::CompactionRequested {
                    turn,
                    request_sha256,
                    ..
                } => {
                    pending = Some(Pending {
                        turn: *turn,
                        compaction: matches!(
                            record.event,
                            JournalEvent::CompactionRequested { .. }
                        ),
                        request_sha256: request_sha256.clone(),
                    });
                }
                JournalEvent::ModelResponded { response, .. } => {
                    replay.push_turn(&mut pending, RecordedOutcome::Responded(response.clone()));
                }
                JournalEvent::CompactionResponded { response, .. } => {
                    replay.push_turn(&mut pending, RecordedOutcome::Compacted(response.clone()));
                }
                JournalEvent::ModelFailed { kind, message, .. }
                | JournalEvent::CompactionFailed { kind, message, .. } => replay.push_turn(
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
                JournalEvent::CheckCompleted { outcome, .. } => {
                    replay.recorded_check = Some(outcome.clone());
                }
                JournalEvent::SandboxReady { expiry_idle_ms, .. } => {
                    replay.sandbox_idle_ms = *expiry_idle_ms;
                }
                JournalEvent::SessionStarted { .. } => {}
            }
        }
        // A trailing request with no answer stays a turn to account for, so
        // `exhausted()` is false until replay has reached it.
        replay.push_turn(&mut pending, RecordedOutcome::Unanswered);
        Ok(replay)
    }

    /// The recorded system prompt and task. Run it with [`Harness::replay`],
    /// which also restores the recorded execution context.
    #[must_use]
    pub fn task(&self) -> SessionTask {
        self.task.clone()
    }

    /// The recorded limits.
    #[must_use]
    pub fn limits(&self) -> SessionLimits {
        self.limits
    }

    /// The sandbox idle timeout the recorded session sent kilnd, when it
    /// ran against a daemon with expiry.
    #[must_use]
    pub fn recorded_sandbox_idle_timeout(&self) -> Option<std::time::Duration> {
        self.sandbox_idle_ms.map(std::time::Duration::from_millis)
    }

    /// The recorded compaction policy, if the session could compact.
    #[must_use]
    pub fn compaction(&self) -> Option<&CompactionPolicy> {
        self.compaction.as_ref()
    }

    /// Why this build cannot replay the recording's compactions, if it
    /// cannot: they were requested with other instructions. The journal
    /// still verifies; [`Harness::replay`] refuses it before any sandbox
    /// exists.
    #[must_use]
    pub fn compaction_problem(&self) -> Option<String> {
        self.compaction
            .as_ref()
            .filter(|policy| !policy.instructions_are_current())
            .map(|policy| {
                format!(
                    "the recording compacted with instructions {} and this build sends {}; \
                     replay it with the build that recorded it",
                    policy.instructions_sha256,
                    crate::compaction::instructions_sha256()
                )
            })
    }

    /// The configuration that replays this recording: `base` with the
    /// recorded limits and compaction policy, and the sandbox idle timeout
    /// from [`crate::engine::HarnessConfig::fit_replay_idle_timeout`].
    #[must_use]
    pub fn replay_config(
        &self,
        base: crate::engine::HarnessConfig,
    ) -> crate::engine::HarnessConfig {
        let mut config = base;
        config.apply_limits(&self.limits);
        config.compaction.clone_from(&self.compaction);
        config.fit_replay_idle_timeout(self.recorded_sandbox_idle_timeout());
        config
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

    fn push_turn(&mut self, pending: &mut Option<Pending>, outcome: RecordedOutcome) {
        if let Some(Pending {
            turn,
            compaction,
            request_sha256,
        }) = pending.take()
        {
            self.turns.push_back(RecordedTurn {
                turn,
                compaction,
                request_sha256,
                outcome,
            });
        }
    }

    /// True once every recorded model turn, compaction attempt (unanswered
    /// ones included) and tool call has been replayed.
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

    /// The recorded check's outcome, when the recording ran one.
    #[must_use]
    pub fn recorded_check(&self) -> Option<&CheckOutcome> {
        self.recorded_check.as_ref()
    }

    /// True when the replayed check's verdict differed from the recorded
    /// one (any error equals any other error), or a check ran that the
    /// recording has no outcome for.
    #[must_use]
    pub fn check_diverged(&self) -> bool {
        self.check_diverged
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

    fn check_completed(&mut self, outcome: &CheckOutcome) {
        let same = self
            .recorded_check
            .as_ref()
            .is_some_and(|recorded| recorded.same_verdict(outcome));
        self.check_diverged |= !same;
    }

    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let Some(recorded) = self.turns.pop_front() else {
            self.mark_unexpected(request.turn);
            return Err(ProviderError::new(
                ProviderErrorKind::Script,
                format!("recording has no response for turn {}", request.turn),
            ));
        };
        if recorded.compaction {
            self.mark_unexpected(request.turn);
            return Err(ProviderError::new(
                ProviderErrorKind::ReplayDiverged,
                format!(
                    "turn {} requested a model turn where the recording compacted",
                    request.turn
                ),
            ));
        }
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
            // The request matched; the recording simply has no answer for it.
            RecordedOutcome::Unanswered | RecordedOutcome::Compacted(_) => Err(ProviderError::new(
                ProviderErrorKind::Script,
                format!("recording ends before turn {} was answered", request.turn),
            )),
        }
    }

    async fn compact(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        let Some(recorded) = self.turns.pop_front() else {
            self.mark_unexpected(request.turn);
            return Err(ProviderError::new(
                ProviderErrorKind::Script,
                format!("recording has no compaction for turn {}", request.turn),
            ));
        };
        if !recorded.compaction {
            self.mark_unexpected(request.turn);
            return Err(ProviderError::new(
                ProviderErrorKind::ReplayDiverged,
                format!(
                    "turn {} compacted where the recording requested a model turn",
                    request.turn
                ),
            ));
        }
        // replay_config restores the recorded policy; without one the
        // engine never compacts, so nothing can match.
        let matches = self.compaction.as_ref().is_some_and(|policy| {
            request.turn == recorded.turn
                && compaction_request_sha256(&request, &policy.instructions_sha256)
                    == recorded.request_sha256
        });
        if !matches {
            self.mark_unexpected(request.turn);
            return Err(ProviderError::new(
                ProviderErrorKind::ReplayDiverged,
                format!(
                    "turn {} compaction request differs from the recording",
                    request.turn
                ),
            ));
        }
        match recorded.outcome {
            RecordedOutcome::Compacted(response) => Ok(response),
            RecordedOutcome::Failed { kind, message } => Err(ProviderError::new(kind, message)),
            RecordedOutcome::Unanswered | RecordedOutcome::Responded(_) => Err(ProviderError::new(
                ProviderErrorKind::Script,
                format!(
                    "recording ends before turn {}'s compaction was answered",
                    request.turn
                ),
            )),
        }
    }
}

/// A recorded request waiting for its answer.
struct Pending {
    turn: u32,
    compaction: bool,
    request_sha256: String,
}

/// The recorded execution context; the grammar has already refused an
/// unknown one, and a forged record that bypassed it is refused here.
fn recorded_exec_context(name: Option<&str>) -> Result<ExecContext, ReplayError> {
    ExecContext::from_journal(name).ok_or(ReplayError::InvalidTask)
}

impl<A, S, J> Harness<A, ReplayProvider, S, J>
where
    A: SandboxApi,
    S: AuditSink,
    J: JournalSink,
{
    /// Replay the recording: its task, in the execution context it was
    /// recorded in. A journal written before `exec_context` existed runs
    /// with no cwd and no env, as it ran, and needs neither exec option;
    /// every other one runs as [`Harness::run`] does.
    ///
    /// Construct the harness with [`ReplayProvider::replay_config`]: a
    /// recording that could compact replays only under its recorded policy
    /// and with this build's instructions, and anything else is refused
    /// before a sandbox exists.
    ///
    /// # Errors
    ///
    /// `InvalidTask` when the recording compacted with other instructions,
    /// or the harness's compaction policy is not the recorded one; otherwise
    /// as [`Harness::run`].
    pub async fn replay(
        &mut self,
        cancel: CancellationToken,
    ) -> Result<SessionOutcome, HarnessError> {
        if let Some(problem) = self.provider().compaction_problem() {
            return Err(HarnessError::InvalidTask(problem));
        }
        if self.config().compaction.as_ref() != self.provider().compaction() {
            return Err(HarnessError::InvalidTask(
                "the compaction policy differs from the recording's; build the replay's \
                 configuration with ReplayProvider::replay_config"
                    .to_string(),
            ));
        }
        let task = self.provider().task();
        let exec_context = self.provider().exec_context;
        self.run_in_context(&task, exec_context, cancel).await
    }
}
