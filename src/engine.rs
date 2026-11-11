use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::time::error::Elapsed;
use tokio::time::{Instant, sleep, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::api::{
    ApiError, ApiLimits, Capabilities, Execution, ExecutionState, ExpiryPolicy, ExpiryReason,
    Feature, FinalCheck, Sandbox, SandboxApi, SandboxRequest, SandboxState, UnusableReason,
    backend_unsupported_feature, request_id_is_valid,
};
use crate::audit::{AuditError, AuditEvent, AuditSink, ContentSummary, OutputEvidence};
use crate::journal::{
    JOURNAL_SCHEMA, JournalError, JournalEvent, JournalSink, RecordedToolLimits, SessionLimits,
    request_sha256,
};
use crate::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, StopReason, ToolCall,
    ToolDefinition, ToolResult, Usage,
};
use crate::tools::{
    ToolError, ToolLimits, ToolRegistry, ToolRun, ToolSession, ToolStop, UNUSABLE_SANDBOX,
    recover_execution_evidence, sanitize_call_bounded, unusable_stop,
};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard ceilings for every configuration, including recorded ones.
pub const MAX_STEPS: u32 = 1000;
pub const MAX_TOOL_CALLS: u32 = 10_000;
pub const MAX_TOOL_CALLS_PER_TURN: u32 = 64;
pub const MAX_SESSION_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
pub const MAX_TOOL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Ceiling for each byte limit.
pub const MAX_BYTE_LIMIT: usize = 16 << 20;
pub const MAX_OUTPUT_TOKENS_PER_REQUEST: u32 = 1_000_000;
/// Ceiling for each cumulative token budget.
pub const MAX_TOTAL_TOKENS: u64 = 1_000_000_000;
/// Ceiling for the cumulative model-output byte budget.
pub const MAX_TOTAL_MODEL_OUTPUT_BYTES: u64 = 256 << 20;
pub const MAX_MANIFEST_ENTRIES: usize = 1_000_000;
/// Largest accepted system prompt or task prompt.
pub const MAX_TASK_TEXT_BYTES: usize = 64 << 10;
/// Bounds for [`HarnessConfig::sandbox_idle_timeout`].
pub const MIN_SANDBOX_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_SANDBOX_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Bounds for [`HarnessConfig::sandbox_lifetime_margin`].
pub const MIN_SANDBOX_LIFETIME_MARGIN: Duration = Duration::from_secs(1);
pub const MAX_SANDBOX_LIFETIME_MARGIN: Duration = Duration::from_secs(10 * 60);
/// Shortest interval between `RenewSandbox` heartbeats.
const MIN_RENEW_INTERVAL_MS: u64 = 1000;
/// Most `CreateSandbox` RPCs one session sends when its create is keyed.
const MAX_CREATE_ATTEMPTS: u32 = 8;
/// Longest backoff before a create retry or a Health reread.
const MAX_CREATE_BACKOFF: Duration = Duration::from_secs(2);
/// Further `release_request_id` deletes after absence is confirmed, when no
/// earlier delete reported the request ID released.
const EXTRA_RELEASE_DELETES: u32 = 2;
/// What one session is asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTask {
    pub system: String,
    pub prompt: String,
}

impl SessionTask {
    /// The task the deterministic demo script carries out.
    #[must_use]
    pub fn demo() -> Self {
        Self {
            system: "You operate one no-network kiln sandbox through typed tools. \
                     Work only under /workspace."
                .to_string(),
            prompt: "Write demo.txt, read it back, list the workspace, check the workspace \
                     diff, count the file's bytes with wc, then report what you did."
                .to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_tool_calls_per_turn: u32,
    pub session_timeout: Duration,
    pub lifecycle_timeout: Duration,
    pub lifecycle_retry_delay: Duration,
    pub max_model_arguments_bytes: usize,
    pub max_final_answer_bytes: usize,
    pub max_output_tokens_per_request: u32,
    pub max_total_input_tokens: u64,
    pub max_total_output_tokens: u64,
    /// Bytes of model output (text plus compact tool-call arguments, see
    /// [`ModelResponse::output_bytes`]) per session. Bounded locally, so
    /// a session stays bounded even when reported token usage is wrong.
    pub max_total_model_output_bytes: u64,
    pub tool_limits: ToolLimits,
    /// How long kilnd may leave the sandbox idle before reclaiming it, when
    /// the daemon supports expiry. A heartbeat renews it during provider
    /// calls; executions count as activity on their own.
    pub sandbox_idle_timeout: Duration,
    /// Added to the session and lifecycle windows to form the sandbox's
    /// maximum lifetime, so it outlives every path that still uses it.
    pub sandbox_lifetime_margin: Duration,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_tool_calls: 6,
            max_tool_calls_per_turn: 8,
            session_timeout: Duration::from_secs(30),
            lifecycle_timeout: Duration::from_secs(10),
            lifecycle_retry_delay: Duration::from_millis(100),
            max_model_arguments_bytes: 64 << 10,
            max_final_answer_bytes: 64 << 10,
            max_output_tokens_per_request: 4096,
            max_total_input_tokens: 2_000_000,
            max_total_output_tokens: 200_000,
            max_total_model_output_bytes: 8 << 20,
            tool_limits: ToolLimits::default(),
            sandbox_idle_timeout: Duration::from_secs(120),
            sandbox_lifetime_margin: Duration::from_secs(60),
        }
    }
}

impl HarnessConfig {
    /// The limits recorded in a session journal.
    #[must_use]
    pub fn limits(&self) -> SessionLimits {
        let tools = &self.tool_limits;
        SessionLimits {
            max_steps: self.max_steps,
            max_tool_calls: self.max_tool_calls,
            max_tool_calls_per_turn: self.max_tool_calls_per_turn,
            session_timeout_ms: duration_ms(self.session_timeout),
            max_output_tokens_per_request: self.max_output_tokens_per_request,
            max_total_input_tokens: self.max_total_input_tokens,
            max_total_output_tokens: self.max_total_output_tokens,
            max_total_model_output_bytes: self.max_total_model_output_bytes,
            max_model_arguments_bytes: self.max_model_arguments_bytes,
            max_final_answer_bytes: self.max_final_answer_bytes,
            tool_limits: RecordedToolLimits {
                timeout_ms: duration_ms(tools.timeout),
                max_argv_bytes: tools.max_argv_bytes,
                max_stdin_bytes: tools.max_stdin_bytes,
                max_file_bytes: tools.max_file_bytes,
                max_result_bytes: tools.max_result_bytes,
                max_manifest_bytes: tools.max_manifest_bytes,
                max_manifest_entries: tools.max_manifest_entries,
            },
        }
    }

    /// Choose the sandbox idle timeout for replaying a recording, which has
    /// no expiry flags of its own: the recorded one when the recording ran
    /// with expiry, and otherwise the smallest whole-second idle timeout of
    /// at least the default whose renew interval leaves room for the
    /// recorded tool budget (`idle >= 1.5 * budget`), at most the ceiling.
    /// kilnd's clamp and the tool-budget check still apply when the replay
    /// runs, and refuse it before create if it does not fit.
    pub fn fit_replay_idle_timeout(&mut self, recorded: Option<Duration>) {
        self.sandbox_idle_timeout = recorded.unwrap_or_else(|| {
            let budget_ms = duration_ms(tool_rpc_budget(self));
            let needed_ms = budget_ms.saturating_mul(3).div_ceil(2);
            let whole_seconds = Duration::from_secs(needed_ms.div_ceil(1000));
            whole_seconds
                .max(Self::default().sandbox_idle_timeout)
                .min(MAX_SANDBOX_IDLE_TIMEOUT)
        });
    }

    /// Check every limit against its hard ceilings, exactly as
    /// [`Harness::run`] does before any sandbox exists.
    ///
    /// # Errors
    ///
    /// Returns an error when a limit is zero, over its ceiling, or a
    /// duration is not whole milliseconds.
    pub fn validate(&self) -> Result<(), HarnessError> {
        validate_config(self)
    }

    /// Replace every recorded limit with the given ones. Lifecycle windows
    /// and sandbox expiry are kept. `Harness::run` validates the result
    /// before any sandbox exists, so recorded limits cannot exceed the hard
    /// ceilings.
    pub fn apply_limits(&mut self, limits: &SessionLimits) {
        self.max_steps = limits.max_steps;
        self.max_tool_calls = limits.max_tool_calls;
        self.max_tool_calls_per_turn = limits.max_tool_calls_per_turn;
        self.session_timeout = Duration::from_millis(limits.session_timeout_ms);
        self.max_output_tokens_per_request = limits.max_output_tokens_per_request;
        self.max_total_input_tokens = limits.max_total_input_tokens;
        self.max_total_output_tokens = limits.max_total_output_tokens;
        self.max_total_model_output_bytes = limits.max_total_model_output_bytes;
        self.max_model_arguments_bytes = limits.max_model_arguments_bytes;
        self.max_final_answer_bytes = limits.max_final_answer_bytes;
        let tools = &limits.tool_limits;
        self.tool_limits = ToolLimits {
            timeout: Duration::from_millis(tools.timeout_ms),
            max_argv_bytes: tools.max_argv_bytes,
            max_stdin_bytes: tools.max_stdin_bytes,
            max_file_bytes: tools.max_file_bytes,
            max_result_bytes: tools.max_result_bytes,
            max_manifest_bytes: tools.max_manifest_bytes,
            max_manifest_entries: tools.max_manifest_entries,
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutcome {
    pub session_id: String,
    pub sandbox_id: String,
    pub final_answer: String,
    pub steps: u32,
    pub tool_calls: u32,
    pub usage: Usage,
    /// Successful `RenewSandbox` heartbeats during provider calls.
    pub renews: u32,
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Audit(#[from] AuditError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("tool setup failed: {0}")]
    ToolSetup(#[from] ToolError),
    #[error("invalid session task: {0}")]
    InvalidTask(String),
    #[error("session deadline expired")]
    Deadline,
    #[error("session canceled")]
    Canceled,
    #[error("model exhausted the {0}-step budget without a final answer")]
    StepBudget(u32),
    #[error("model exceeded the {0}-tool-call budget")]
    ToolBudget(u32),
    #[error("model requested more than {0} tool calls in one turn")]
    TurnToolBudget(u32),
    #[error("model exceeded a token budget")]
    TokenBudget,
    #[error("model output exceeded the {0}-byte session budget")]
    OutputBytesBudget(u64),
    #[error("model output was truncated at its token limit")]
    ModelTruncated,
    #[error("model refused the request")]
    ModelRefused,
    #[error("invalid model tool call: {0}")]
    InvalidModelCall(String),
    #[error("final answer exceeds the configured byte limit")]
    FinalAnswerTooLarge,
    #[error("sandbox cleanup failed after session outcome: {0}")]
    Cleanup(ApiError),
    #[error("create outcome is ambiguous after recovery window: {0}")]
    CreateUncertain(ApiError),
    /// kilnd reclaimed or deleted the sandbox before the session ended, or
    /// it could not be renewed for long enough that kilnd probably has.
    /// Cleanup still runs and verifies absence.
    #[error("sandbox is gone before the session ended: {reason}")]
    SandboxExpired { reason: ExpiryReason },
}

pub struct Harness<A, P, S, J> {
    api: A,
    provider: P,
    audit: S,
    journal: J,
    config: HarnessConfig,
    registry: ToolRegistry,
}

/// Counters that every exit path reports in `session_ended`.
#[derive(Debug, Default)]
struct Progress {
    steps: u32,
    tool_calls: u32,
    usage: Usage,
    output_bytes: u64,
    renews: u32,
    /// Transient renew failures in a row, across provider calls, and when
    /// the last one happened.
    renew_failures: u32,
    last_renew_failure: Option<Instant>,
    /// The activity generation when the last failed renew started; any
    /// later activity proves the sandbox alive after it.
    renew_failure_generation: Option<u64>,
}

impl<A, P, S, J> Harness<A, P, S, J>
where
    A: SandboxApi,
    P: Provider,
    S: AuditSink,
    J: JournalSink,
{
    #[must_use]
    pub fn new(api: A, provider: P, audit: S, journal: J, config: HarnessConfig) -> Self {
        let registry = ToolRegistry::new(config.tool_limits.clone());
        Self {
            api,
            provider,
            audit,
            journal,
            config,
            registry,
        }
    }

    #[must_use]
    pub fn audit(&self) -> &S {
        &self.audit
    }

    #[must_use]
    pub fn journal(&self) -> &J {
        &self.journal
    }

    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Run one session and wait until its sandbox has been deleted.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid tasks or budgets, provider, budget, audit,
    /// journal, kilnd, deadline, cancellation, or cleanup failures.
    #[allow(clippy::too_many_lines)]
    pub async fn run(
        &mut self,
        task: &SessionTask,
        cancel: CancellationToken,
    ) -> Result<SessionOutcome, HarnessError> {
        validate_config(&self.config)?;
        validate_task(task)?;
        let session_id = session_id()?;
        let deadline = Instant::now()
            .checked_add(self.config.session_timeout)
            .ok_or(HarnessError::Deadline)?;
        // Discovery comes before any use: an older daemon ignores fields it
        // does not know, so expiry is sent and reasons are read only when a
        // serving daemon lists them. Nothing exists yet if this fails.
        let capabilities = tokio::select! {
            () = cancel.cancelled() => return Err(HarnessError::Canceled),
            health = timeout_at(deadline, self.api.health()) => match health {
                Ok(Ok(health)) if health.serving => health.capabilities,
                Ok(Ok(health)) => {
                    return Err(HarnessError::Api(ApiError::new(
                        tonic::Code::Unavailable,
                        format!("kilnd is not serving: {}", health.detail),
                    )));
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => return Err(HarnessError::Deadline),
            },
        };
        let expiry = expiry_policy(&self.config, &capabilities)?;
        let typed_reasons = capabilities.supports(Feature::TypedReasons);
        // Keyed only when a serving daemon lists idempotent create: an older
        // daemon ignores request_id and would make a second sandbox for a
        // retry. Release is decided here, once, and used only by the cleanup
        // after a successful keyed create that sent a single attempt.
        let keyed = capabilities.supports(Feature::IdempotentCreate);
        let release = keyed && capabilities.supports(Feature::RequestIdRelease);
        let request_id = if keyed {
            let request_id = format!("kiln-harness:{session_id}");
            if !request_id_is_valid(&request_id) {
                return Err(HarnessError::Api(ApiError::new(
                    tonic::Code::InvalidArgument,
                    "session ID does not make a valid create request ID",
                )));
            }
            Some(request_id)
        } else {
            None
        };
        let heartbeat = expiry.map(|policy| Heartbeat::for_idle_ms(policy.idle_timeout_ms));
        if let Some(heartbeat) = heartbeat {
            check_tool_budget(&self.config, heartbeat)?;
        }
        self.audit.append(
            &session_id,
            None,
            AuditEvent::SessionOpened {
                session_timeout_ms: duration_ms(self.config.session_timeout),
                network: "none".to_string(),
                features: capabilities.feature_names(),
            },
        )?;
        let started = self.journal.append(
            &session_id,
            JournalEvent::SessionStarted {
                schema: JOURNAL_SCHEMA,
                harness_version: env!("CARGO_PKG_VERSION").to_string(),
                provider: self.provider.info(),
                system: task.system.clone(),
                task: task.prompt.clone(),
                tools: self.registry.definitions().to_vec(),
                limits: self.config.limits(),
            },
        );
        if let Err(error) = started {
            // No sandbox exists yet. Close the audit session best-effort and
            // report the journal failure, which is the primary error.
            let _ = self.audit.append(
                &session_id,
                None,
                AuditEvent::SessionFinished {
                    outcome: "failed".to_string(),
                    work_outcome: "failed".to_string(),
                    sandbox_deleted: true,
                    renews: 0,
                    request_id_released: None,
                },
            );
            return Err(error.into());
        }
        let mut progress = Progress::default();

        // Cancellation and the deadline both drop the create RPC. Its outcome
        // is then ambiguous, so either goes through label-based recovery.
        // No later than kilnd can have made the sandbox ready: the first
        // activity anchor, on the early side, when the first attempt was sent.
        let request = sandbox_request(&session_id, expiry, request_id);
        let create_sent = Instant::now();
        let (create, create_attempts) =
            create_with_retries(&self.api, &request, &self.config, deadline, &cancel).await;
        let sandbox = match create {
            Ok(sandbox) => sandbox,
            Err(stop) => {
                // A settled answer names a sandbox kilnd has finished with,
                // but only the listing proves it absent.
                let settled = matches!(stop, CreateStop::Settled(..));
                let (error, interrupted, work_outcome) = match stop {
                    CreateStop::Failed(error) | CreateStop::Settled(_, error) => {
                        (error, None, "create_failed")
                    }
                    CreateStop::Deadline => (
                        deadline_api("create response deadline expired"),
                        Some(HarnessError::Deadline),
                        "create_failed",
                    ),
                    CreateStop::Canceled => (
                        ApiError::new(
                            tonic::Code::Cancelled,
                            "create canceled before its response",
                        ),
                        Some(HarnessError::Canceled),
                        "canceled",
                    ),
                };
                let deadline_expired = interrupted.is_some();
                let recovery_deadline = lifecycle_deadline(self.config.lifecycle_timeout);
                let recovery = recover_session_sandboxes(
                    &self.api,
                    &session_id,
                    recovery_deadline,
                    self.config.lifecycle_retry_delay,
                )
                .await;
                let recovered_ids = recovery.as_ref().map_or_else(|_| Vec::new(), Clone::clone);
                let recovered = !recovered_ids.is_empty();
                let unambiguous = settled || unambiguous_create_error(&error, typed_reasons);
                let absence_confirmed = recovery.is_ok()
                    && (settled || recovered || (!deadline_expired && unambiguous));
                let recovery_audit = self.audit.append(
                    &session_id,
                    None,
                    AuditEvent::SandboxRecovered {
                        sandbox_ids: recovered_ids,
                        deleted: absence_confirmed,
                        create_attempts,
                    },
                );
                // As after work: without confirmed absence, the session's
                // outcome is cleanup_failed whatever the work outcome was.
                let terminal = Terminal {
                    outcome: if absence_confirmed {
                        work_outcome
                    } else {
                        "cleanup_failed"
                    },
                    work_outcome,
                    sandbox_deleted: absence_confirmed,
                    request_id_released: None,
                };
                let finish_audit =
                    self.audit
                        .append(&session_id, None, terminal.audit_event(&progress));
                let finish_journal = self
                    .journal
                    .append(&session_id, terminal.journal_event(None, &progress));
                if let Err(cleanup_error) = recovery {
                    return Err(HarnessError::Cleanup(cleanup_error));
                }
                recovery_audit?;
                finish_audit?;
                finish_journal?;
                if !recovered && (deadline_expired || !unambiguous) {
                    return Err(HarnessError::CreateUncertain(error));
                }
                return Err(interrupted.unwrap_or(HarnessError::Api(error)));
            }
        };
        let sandbox_id = sandbox.id.clone();

        let body = async {
            self.audit.append(
                &session_id,
                Some(&sandbox_id),
                AuditEvent::SandboxCreated {
                    backend_id: sandbox.backend_id,
                    expiry_lifetime_ms: expiry.map(|policy| policy.max_lifetime_ms),
                    expiry_idle_ms: expiry.map(|policy| policy.idle_timeout_ms),
                    request_id: request.request_id.clone(),
                    create_attempts,
                },
            )?;
            if cancel.is_cancelled() {
                return Err(HarnessError::Canceled);
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Deadline);
            }

            let started = tokio::select! {
                () = cancel.cancelled() => None,
                started = self
                    .registry
                    .start_session(
                        &self.api,
                        sandbox_id.clone(),
                        deadline,
                        typed_reasons,
                        create_sent,
                    ) => {
                    Some(started)
                }
            };
            let (tools, baseline) = match started {
                Some(Ok(value)) => value,
                None => {
                    // The dropped baseline RPC asks kilnd to cancel it; record
                    // any execution it already assigned, then clean up.
                    let recovery_deadline = recovery_deadline(self.config.lifecycle_timeout);
                    // Already canceled: recover on a token of its own, within
                    // its short window.
                    let execution_ids = recover_execution_evidence(
                        &self.api,
                        &sandbox_id,
                        &BTreeSet::new(),
                        recovery_deadline,
                        &CancellationToken::new(),
                    )
                    .await
                    .execution_ids;
                    self.audit.append(
                        &session_id,
                        Some(&sandbox_id),
                        AuditEvent::WorkspaceBaseline {
                            ok: false,
                            execution_ids,
                            file_count: 0,
                        },
                    )?;
                    return Err(HarnessError::Canceled);
                }
                Some(Err(error)) => {
                    let (mut execution_ids, _, _) = error.execution_evidence();
                    let mut recovered_stop = None;
                    if execution_ids.is_empty() {
                        let recovery_deadline = recovery_deadline(self.config.lifecycle_timeout);
                        let recovered = recover_execution_evidence(
                            &self.api,
                            &sandbox_id,
                            &BTreeSet::new(),
                            recovery_deadline,
                            &cancel,
                        )
                        .await;
                        recovered_stop = recovered.stop();
                        execution_ids = recovered.execution_ids;
                    }
                    self.audit.append(
                        &session_id,
                        Some(&sandbox_id),
                        AuditEvent::WorkspaceBaseline {
                            ok: false,
                            execution_ids,
                            file_count: 0,
                        },
                    )?;
                    if cancel.is_cancelled() {
                        return Err(HarnessError::Canceled);
                    }
                    // A recovered baseline row that left the sandbox unusable
                    // ends the session as a returned one would.
                    if let Some(stop) = recovered_stop {
                        return Err(stop_error(stop));
                    }
                    match &error {
                        ToolError::Api(api_error) => {
                            if let Some(reason) = ExpiryReason::of(api_error, typed_reasons) {
                                return Err(HarnessError::SandboxExpired { reason });
                            }
                            if UnusableReason::of(api_error, typed_reasons).is_some() {
                                return Err(stop_error(ToolStop::Unusable(
                                    UNUSABLE_SANDBOX.to_string(),
                                )));
                            }
                        }
                        ToolError::Unusable { stop, .. } => return Err(stop_error(stop.clone())),
                        ToolError::Deadline | ToolError::Manifest { .. } => {}
                    }
                    return Err(HarnessError::ToolSetup(error));
                }
            };
            let baseline_execution_ids = baseline.execution_ids.clone();
            self.audit.append(
                &session_id,
                Some(&sandbox_id),
                AuditEvent::WorkspaceBaseline {
                    ok: true,
                    execution_ids: baseline.execution_ids,
                    file_count: baseline.file_count,
                },
            )?;
            self.journal.append(
                &session_id,
                JournalEvent::SandboxReady {
                    sandbox_id: sandbox_id.clone(),
                    baseline_file_count: baseline.file_count,
                    expiry_lifetime_ms: expiry.map(|policy| policy.max_lifetime_ms),
                    expiry_idle_ms: expiry.map(|policy| policy.idle_timeout_ms),
                },
            )?;
            let context = LoopContext {
                api: &self.api,
                heartbeat,
                typed_reasons,
                config: &self.config,
                task,
                definitions: self.registry.definitions(),
                tools: &tools,
                session_id: &session_id,
                sandbox_id: &sandbox_id,
                deadline,
                cancel: &cancel,
            };
            run_loop(
                &mut self.provider,
                &mut self.audit,
                &mut self.journal,
                &context,
                &mut progress,
                baseline_execution_ids,
            )
            .await
        }
        .await;

        // Cleanup has a separate finite settlement window. Delete is
        // idempotently retried and independently verified after every response.
        // This is the only path that releases the request ID: the create
        // answered READY, its response is held, and no retry is to come. It
        // releases only when exactly one create was sent. After a retry, an
        // attempt the harness gave up on may still reach kilnd; keeping the
        // key lets kilnd answer it with the retained DELETED row instead of
        // creating a fresh sandbox.
        let release = release && create_attempts == 1;
        let cleanup_deadline = lifecycle_deadline(self.config.lifecycle_timeout);
        let cleanup = cleanup_sandbox(
            &self.api,
            &sandbox_id,
            cleanup_deadline,
            self.config.lifecycle_retry_delay,
            release,
        )
        .await;
        let deleted = cleanup.is_ok();
        let request_id_released = release.then(|| {
            cleanup
                .as_ref()
                .is_ok_and(|outcome| outcome.request_id_released)
        });
        let work_outcome = match &body {
            Ok(_) => "completed",
            Err(HarnessError::Canceled) => "canceled",
            Err(HarnessError::Deadline) => "deadline",
            Err(_) => "failed",
        };
        // An unconfirmed cleanup is never reported as the work's outcome.
        let terminal = Terminal {
            outcome: if deleted {
                work_outcome
            } else {
                "cleanup_failed"
            },
            work_outcome,
            sandbox_deleted: deleted,
            request_id_released,
        };
        let finish_audit = self.audit.append(
            &session_id,
            Some(&sandbox_id),
            terminal.audit_event(&progress),
        );
        let final_answer = body
            .as_ref()
            .ok()
            .map(|outcome| outcome.final_answer.clone());
        let finish_journal = self
            .journal
            .append(&session_id, terminal.journal_event(final_answer, &progress));

        if let Err(error) = cleanup {
            return Err(HarnessError::Cleanup(error));
        }
        finish_audit?;
        finish_journal?;
        body
    }
}

struct LoopContext<'a, 'r, A: SandboxApi> {
    api: &'a A,
    /// Present only when the daemon supports expiry.
    heartbeat: Option<Heartbeat>,
    typed_reasons: bool,
    config: &'a HarnessConfig,
    task: &'a SessionTask,
    definitions: &'a [ToolDefinition],
    tools: &'a ToolSession<'r, A>,
    session_id: &'a str,
    sandbox_id: &'a str,
    deadline: Instant,
    cancel: &'a CancellationToken,
}

/// A tool call after host-side sanitization, as the model will see it.
struct PreparedCall {
    visible: ToolCall,
    registered: bool,
    arguments: Option<ContentSummary>,
}

#[allow(clippy::too_many_lines)]
async fn run_loop<A, P, S, J>(
    provider: &mut P,
    audit: &mut S,
    journal: &mut J,
    context: &LoopContext<'_, '_, A>,
    progress: &mut Progress,
    initial_execution_ids: Vec<String>,
) -> Result<SessionOutcome, HarnessError>
where
    A: SandboxApi,
    P: Provider,
    S: AuditSink,
    J: JournalSink,
{
    let config = context.config;
    let session_id = context.session_id;
    let sandbox_id = context.sandbox_id;
    let deadline = context.deadline;
    let cancel = context.cancel;
    let mut messages = vec![Message::User {
        text: context.task.prompt.clone(),
    }];
    let mut known_execution_ids = initial_execution_ids.into_iter().collect::<BTreeSet<_>>();

    for turn in 1..=config.max_steps {
        if cancel.is_cancelled() {
            return Err(HarnessError::Canceled);
        }
        if Instant::now() >= deadline {
            return Err(HarnessError::Deadline);
        }
        let request = ModelRequest {
            session_id,
            turn,
            system: &context.task.system,
            tools: context.definitions,
            messages: &messages,
            max_output_tokens: config.max_output_tokens_per_request,
        };
        journal.append(
            session_id,
            JournalEvent::ModelRequested {
                turn,
                request_sha256: request_sha256(&request),
                message_count: messages.len(),
            },
        )?;
        // Progress counts only what the journal holds, so session_ended
        // always agrees with the records before it.
        progress.steps = turn;
        let requested_at = Instant::now();
        let response = complete_with_heartbeat(provider, request, context, progress).await?;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                journal.append(
                    session_id,
                    JournalEvent::ModelFailed {
                        turn,
                        kind: error.kind,
                        message: error.message.clone(),
                    },
                )?;
                return Err(HarnessError::Provider(error));
            }
        };
        let latency_ms = duration_ms(requested_at.elapsed());
        journal.append(
            session_id,
            JournalEvent::ModelResponded {
                turn,
                response: response.clone(),
                latency_ms,
            },
        )?;
        progress.usage = progress.usage + response.usage;
        progress.output_bytes = progress
            .output_bytes
            .saturating_add(response.output_bytes());
        audit.append(
            session_id,
            Some(sandbox_id),
            AuditEvent::ModelResponded {
                turn,
                stop_reason: response.stop_reason,
                tool_calls: response.tool_calls.len(),
                usage: response.usage,
                latency_ms,
                served_model: response
                    .provider_content
                    .as_ref()
                    .and_then(|content| content.model.clone()),
            },
        )?;
        if response.usage.output_tokens > u64::from(config.max_output_tokens_per_request)
            || progress.usage.total_input_tokens() > config.max_total_input_tokens
            || progress.usage.output_tokens > config.max_total_output_tokens
        {
            return Err(HarnessError::TokenBudget);
        }
        // Reported usage is trusted only for cost; size is bounded here
        // whatever the endpoint reports.
        if progress.output_bytes > config.max_total_model_output_bytes {
            return Err(HarnessError::OutputBytesBudget(
                config.max_total_model_output_bytes,
            ));
        }
        let ModelResponse {
            text,
            tool_calls,
            stop_reason,
            provider_content,
            ..
        } = response;

        if tool_calls.is_empty() {
            let outcome = final_answer(audit, context, progress, turn, stop_reason, text)?;
            final_check(audit, context, &known_execution_ids).await?;
            return Ok(outcome);
        }

        // Tool arguments may be cut off, and a refusal must not run tools.
        match stop_reason {
            StopReason::MaxTokens => return Err(HarnessError::ModelTruncated),
            StopReason::Refusal => return Err(HarnessError::ModelRefused),
            StopReason::EndTurn | StopReason::ToolUse | StopReason::Other => {}
        }
        let reject_turn = |audit: &mut S| {
            audit.append(
                session_id,
                Some(sandbox_id),
                AuditEvent::ModelTurn {
                    turn,
                    action: "tool_call_rejected".to_string(),
                    audit_call_id: None,
                    tool: None,
                    arguments: None,
                },
            )
        };
        if tool_calls.len() > config.max_tool_calls_per_turn as usize {
            reject_turn(audit)?;
            return Err(HarnessError::TurnToolBudget(config.max_tool_calls_per_turn));
        }
        // Call IDs route results back to the model. A malformed or repeated
        // ID is a provider protocol fault, so the whole turn is refused
        // before any call runs.
        let mut seen_call_ids = BTreeSet::new();
        for call in &tool_calls {
            if let Err(error) = validate_call_id(&call.call_id) {
                reject_turn(audit)?;
                return Err(error);
            }
            if !seen_call_ids.insert(call.call_id.as_str()) {
                reject_turn(audit)?;
                return Err(HarnessError::InvalidModelCall(
                    "call_id is repeated within one turn".to_string(),
                ));
            }
        }
        if text
            .as_ref()
            .is_some_and(|text| text.len() > config.max_final_answer_bytes)
        {
            reject_turn(audit)?;
            return Err(HarnessError::InvalidModelCall(
                "assistant text exceeds the configured byte limit".to_string(),
            ));
        }

        let prepared = tool_calls
            .iter()
            .map(|call| {
                let sanitized = sanitize_call_bounded(
                    call,
                    context.definitions,
                    config.max_model_arguments_bytes,
                );
                PreparedCall {
                    visible: sanitized.call,
                    registered: sanitized.registered,
                    arguments: sanitized.arguments,
                }
            })
            .collect::<Vec<_>>();
        // The neutral text and calls are the sanitized view; the provider's
        // opaque record of the turn goes back to it unmodified.
        messages.push(Message::Assistant {
            text,
            tool_calls: prepared.iter().map(|call| call.visible.clone()).collect(),
            provider_content,
        });

        let mut results = Vec::with_capacity(prepared.len());
        for (position, call) in prepared.into_iter().enumerate() {
            let index = u32::try_from(position + 1).unwrap_or(u32::MAX);
            // Provider IDs are untrusted content and stay out of the audit.
            let audit_call_id = format!("call-{turn}-{index}");
            let next_tool_calls = progress.tool_calls.saturating_add(1);
            let over_budget = next_tool_calls > config.max_tool_calls;
            let rejected = over_budget || !call.registered || call.arguments.is_none();
            audit.append(
                session_id,
                Some(sandbox_id),
                AuditEvent::ModelTurn {
                    turn,
                    action: if rejected {
                        "tool_call_rejected".to_string()
                    } else {
                        "tool_call".to_string()
                    },
                    audit_call_id: Some(audit_call_id.clone()),
                    tool: Some(call.visible.tool.clone()),
                    arguments: call.arguments.clone(),
                },
            )?;
            if over_budget {
                return Err(HarnessError::ToolBudget(config.max_tool_calls));
            }
            // The call is audited first, so a fatal renew below still leaves
            // it on record; its completion record follows the renew. A renew
            // falls due between tool calls too: a refused Exec proves no
            // activity, so the next one must not wait for the next provider
            // call, and one that could outlive the idle window renews first.
            // Host-only rejections reach no kilnd RPC.
            if !rejected
                && let Some(heartbeat) = context.heartbeat
                && context
                    .tools
                    .contacts_kilnd(&call.visible.tool, &call.visible.arguments)
            {
                renew_if_due(context, heartbeat, progress, Some(tool_rpc_budget(config))).await?;
            }
            // The tool timeout bounds the call's Execs; with response-loss
            // recovery the call ends within one tool budget of here, and
            // never after the session deadline.
            let tool_deadline = deadline.min(Instant::now() + tool_rpc_budget(config));

            let mut run = if call.arguments.is_none() {
                rejected_tool_run("model_arguments_too_large")
            } else if !call.registered {
                // Misnamed tools are model mistakes: report and continue.
                rejected_tool_run("unknown_tool")
            } else {
                tokio::select! {
                    () = cancel.cancelled() => ToolRun::interrupted(ToolStop::Canceled),
                    run = context.tools.execute(
                        &call.visible.tool,
                        call.visible.arguments.clone(),
                        deadline,
                    ) => run,
                }
            };
            if run.execution_ids.is_empty()
                && (!run.stdout.total_bytes_known || !run.stderr.total_bytes_known)
            {
                let recovery_window = recovery_window(config);
                if let Some(recovery_deadline) = Instant::now()
                    .checked_add(recovery_window)
                    .map(|end| end.min(tool_deadline))
                {
                    let recovered = context
                        .tools
                        .recover_new_execution_evidence(
                            &known_execution_ids,
                            recovery_deadline,
                            cancel,
                        )
                        .await;
                    run.attach_recovered(recovered);
                }
            }
            // Cancellation during the call or its recovery ends the session
            // as itself, whatever the recovered evidence says.
            if cancel.is_cancelled() && !matches!(run.stop, Some(ToolStop::Deadline)) {
                run.stop = Some(ToolStop::Canceled);
            }
            known_execution_ids.extend(run.execution_ids.iter().cloned());
            let result_summary = ContentSummary::json(&run.result)?;
            let stop = run.stop;
            audit.append(
                session_id,
                Some(sandbox_id),
                AuditEvent::ToolCompleted {
                    turn,
                    audit_call_id: audit_call_id.clone(),
                    tool: call.visible.tool.clone(),
                    ok: run.ok,
                    execution_ids: run.execution_ids.clone(),
                    result: result_summary,
                    stdout: run.stdout,
                    stderr: run.stderr,
                },
            )?;
            journal.append(
                session_id,
                JournalEvent::ToolExecuted {
                    turn,
                    index,
                    audit_call_id,
                    call: call.visible.clone(),
                    ok: run.ok,
                    result: run.result.clone(),
                    execution_ids: run.execution_ids,
                },
            )?;
            progress.tool_calls = next_tool_calls;
            // Every journaled tool reaches the provider, an interrupted one
            // included, so replay compares exactly what was recorded. An
            // interruption still ends the session as itself.
            let observed = provider.tool_completed(turn, index, &call.visible, run.ok, &run.result);
            if let Some(stop) = stop {
                return Err(stop_error(stop));
            }
            observed?;
            results.push(ToolResult {
                call_id: call.visible.call_id,
                tool: call.visible.tool,
                result: run.result,
            });
        }
        messages.push(Message::ToolResults { results });
    }
    Err(HarnessError::StepBudget(config.max_steps))
}

/// Before a session may be recorded completed, one `GetSandbox` confirms the
/// sandbox stayed usable. Response-loss recovery is best-effort and its
/// window finite, so a row it missed, or a sandbox lost or quarantined by
/// the end, is caught here. It is bounded by the session deadline and the
/// lifecycle window, and canceled with the session; its outcome is audited
/// as metadata only.
async fn final_check<A, S>(
    audit: &mut S,
    context: &LoopContext<'_, '_, A>,
    known: &BTreeSet<String>,
) -> Result<(), HarnessError>
where
    A: SandboxApi,
    S: AuditSink,
{
    // Cancellation and the session deadline win over any verdict: before
    // the check, while it runs, and once it has answered.
    let interrupted = || {
        if context.cancel.is_cancelled() {
            Some(HarnessError::Canceled)
        } else if Instant::now() >= context.deadline {
            Some(HarnessError::Deadline)
        } else {
            None
        }
    };
    if let Some(error) = interrupted() {
        return Err(error);
    }
    let check_deadline = context
        .deadline
        .min(Instant::now() + context.config.lifecycle_timeout);
    let checked = tokio::select! {
        biased;
        () = context.cancel.cancelled() => return Err(HarnessError::Canceled),
        checked = timeout_at(check_deadline, context.api.final_check(context.sandbox_id)) => checked,
    };
    if let Some(error) = interrupted() {
        return Err(error);
    }
    let (state, unknown_rows, verdict) = if let Ok(Ok(check)) = &checked {
        let unknown = check
            .recent
            .iter()
            .filter(|row| !known.contains(&row.id))
            .collect::<Vec<_>>();
        let state = if check.ready { "ready" } else { "not_ready" };
        (
            state,
            unknown.len(),
            judge_final_check(check, &unknown, context),
        )
    } else {
        if Instant::now() >= context.deadline {
            return Err(HarnessError::Deadline);
        }
        (
            "unknown",
            0,
            Err(HarnessError::Api(ApiError::new(
                tonic::Code::Unavailable,
                "could not confirm the sandbox at the end of the session",
            ))),
        )
    };
    audit.append(
        context.session_id,
        Some(context.sandbox_id),
        AuditEvent::FinalCheck {
            state: state.to_string(),
            unknown_executions: unknown_rows,
            usable: verdict.is_ok(),
        },
    )?;
    verdict
}

/// The final check's verdict. With a daemon that reports execution
/// usability (one that supports expiry), an execution the session never saw
/// must have finished and left the sandbox reusable; with an older one only
/// the sandbox's state is checked.
fn judge_final_check<A: SandboxApi>(
    check: &FinalCheck,
    unknown: &[&Execution],
    context: &LoopContext<'_, '_, A>,
) -> Result<(), HarnessError> {
    let not_usable = || {
        HarnessError::Api(ApiError::new(
            tonic::Code::FailedPrecondition,
            "sandbox was not usable when the session finished",
        ))
    };
    let rows_reported = context.heartbeat.is_some();
    if rows_reported
        && let Some(row) = unknown
            .iter()
            .find(|row| row.state != ExecutionState::Unknown && row.sandbox_reusable == Some(false))
    {
        return Err(stop_error(unusable_stop(row.cancel_reason)));
    }
    if !check.ready {
        return Err(check
            .deletion
            .map_or_else(not_usable, |reason| HarnessError::SandboxExpired { reason }));
    }
    if rows_reported
        && unknown
            .iter()
            .any(|row| row.state == ExecutionState::Unknown)
    {
        return Err(not_usable());
    }
    Ok(())
}

fn final_answer<A, S>(
    audit: &mut S,
    context: &LoopContext<'_, '_, A>,
    progress: &Progress,
    turn: u32,
    stop_reason: StopReason,
    text: Option<String>,
) -> Result<SessionOutcome, HarnessError>
where
    A: SandboxApi,
    S: AuditSink,
{
    match stop_reason {
        StopReason::MaxTokens => return Err(HarnessError::ModelTruncated),
        StopReason::Refusal => return Err(HarnessError::ModelRefused),
        // A provider protocol fault, never a final answer.
        StopReason::ToolUse => {
            return Err(HarnessError::InvalidModelCall(
                "tool_use stop with no tool calls".to_string(),
            ));
        }
        StopReason::EndTurn | StopReason::Other => {}
    }
    let text = text.unwrap_or_default();
    let rejected = text.is_empty() || text.len() > context.config.max_final_answer_bytes;
    audit.append(
        context.session_id,
        Some(context.sandbox_id),
        AuditEvent::ModelTurn {
            turn,
            action: if rejected {
                "final_answer_rejected".to_string()
            } else {
                "final_answer".to_string()
            },
            audit_call_id: None,
            tool: None,
            arguments: None,
        },
    )?;
    if text.is_empty() {
        return Err(HarnessError::InvalidModelCall(
            "empty final answer".to_string(),
        ));
    }
    if rejected {
        return Err(HarnessError::FinalAnswerTooLarge);
    }
    audit.append(
        context.session_id,
        Some(context.sandbox_id),
        AuditEvent::FinalAnswer {
            turn,
            answer: ContentSummary::bytes(text.as_bytes()),
        },
    )?;
    Ok(SessionOutcome {
        session_id: context.session_id.to_string(),
        sandbox_id: context.sandbox_id.to_string(),
        final_answer: text,
        steps: turn,
        tool_calls: progress.tool_calls,
        usage: progress.usage,
        renews: progress.renews,
    })
}

/// How long response-loss recovery may run after a tool call: the
/// lifecycle window, at most one second.
fn recovery_window(config: &HarnessConfig) -> Duration {
    config.lifecycle_timeout.min(Duration::from_secs(1))
}

/// The longest one kilnd-bound tool call can go without activity kilnd can
/// see: one Exec's whole wall timeout, then response-loss recovery. A tool
/// that runs several Execs (`read_file`) proves activity with each one that
/// returns, and stops at the first that does not, so one timeout bounds it.
fn tool_rpc_budget(config: &HarnessConfig) -> Duration {
    config.tool_limits.timeout + recovery_window(config)
}

/// A tool call that kilnd never admits shows it no activity, and nothing
/// renews while it runs. So the whole call, plus the renew interval before
/// it, must fit in the idle timeout actually sent, or one slow refusal could
/// outlive the sandbox. Checked before create, only when expiry is in use.
fn check_tool_budget(config: &HarnessConfig, heartbeat: Heartbeat) -> Result<(), HarnessError> {
    let budget = tool_rpc_budget(config);
    if budget + heartbeat.interval > heartbeat.idle {
        return Err(HarnessError::InvalidTask(format!(
            "the tool budget of {} ms (tool timeout plus {} ms of recovery) and the {} ms \
             renew interval exceed the sandbox idle timeout of {} ms; lower the tool \
             timeout or raise the idle timeout",
            duration_ms(budget),
            duration_ms(recovery_window(config)),
            duration_ms(heartbeat.interval),
            duration_ms(heartbeat.idle),
        )));
    }
    Ok(())
}

/// When to renew the sandbox while a provider call runs.
#[derive(Debug, Clone, Copy)]
struct Heartbeat {
    /// The idle timeout kilnd was sent, after its clamp.
    idle: Duration,
    interval: Duration,
    /// Transient renew failures in a row after which the sandbox has
    /// probably expired.
    max_failures: u32,
}

impl Heartbeat {
    /// Renew three times per idle window, at most once a second, and give up
    /// once `ceil(idle / interval) - 1` renews in a row have failed.
    fn for_idle_ms(idle_ms: u64) -> Self {
        let interval_ms = idle_ms.div_ceil(3).max(MIN_RENEW_INTERVAL_MS);
        let ticks = idle_ms.div_ceil(interval_ms);
        Self {
            idle: Duration::from_millis(idle_ms),
            interval: Duration::from_millis(interval_ms),
            max_failures: u32::try_from(ticks.saturating_sub(1))
                .unwrap_or(u32::MAX)
                .max(1),
        }
    }
}

type RenewCall<'a> =
    Pin<Box<dyn Future<Output = Result<Result<(), ApiError>, Elapsed>> + Send + 'a>>;

enum Renewal {
    Renewed,
    Transient,
    Expired(ExpiryReason),
    Fatal(ApiError),
}

fn classify_renewal(
    outcome: Result<Result<(), ApiError>, Elapsed>,
    typed_reasons: bool,
) -> Renewal {
    match outcome {
        Ok(Ok(())) => Renewal::Renewed,
        Err(_) => Renewal::Transient,
        Ok(Err(error)) => {
            if let Some(reason) = ExpiryReason::of(&error, typed_reasons) {
                Renewal::Expired(reason)
            } else if UnusableReason::of(&error, typed_reasons).is_some() {
                // Lost, quarantined, failed or no longer ready: terminal,
                // never retried.
                Renewal::Fatal(error)
            } else if matches!(
                error.code,
                // Unknown is also how local transport failures arrive, and
                // ResourceExhausted is kilnd's connection and RPC admission,
                // which clears on its own.
                tonic::Code::Unavailable
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::Unknown
                    | tonic::Code::ResourceExhausted
            ) {
                Renewal::Transient
            } else {
                Renewal::Fatal(error)
            }
        }
    }
}

/// Run one provider call under the session deadline and cancellation,
/// renewing the sandbox beside it when the daemon supports expiry: a long
/// model turn runs no execution, so nothing else tells kilnd the session is
/// alive. The schedule is session-wide: a renew is due one interval after
/// the last activity the harness can prove kilnd saw (ready, a returned or
/// recovered execution, or a renew), however many provider calls that
/// spans. An overdue renew runs
/// before the call starts. The renew RPC runs next to the provider future,
/// not in a spawned task, so dropping this future stops both before the
/// engine moves on.
async fn complete_with_heartbeat<A, P>(
    provider: &mut P,
    request: ModelRequest<'_>,
    context: &LoopContext<'_, '_, A>,
    progress: &mut Progress,
) -> Result<Result<ModelResponse, ProviderError>, HarnessError>
where
    A: SandboxApi,
    P: Provider,
{
    let deadline = context.deadline;
    let Some(heartbeat) = context.heartbeat else {
        return tokio::select! {
            () = context.cancel.cancelled() => Err(HarnessError::Canceled),
            result = timeout_at(deadline, provider.complete(request)) => {
                result.map_err(|_| HarnessError::Deadline)
            }
        };
    };
    renew_if_due(context, heartbeat, progress, None).await?;
    let call = timeout_at(deadline, provider.complete(request));
    tokio::pin!(call);
    let mut next_tick = renew_due(context, heartbeat, progress);
    let mut renew: Option<RenewCall<'_>> = None;
    let mut renew_started = (Instant::now(), context.tools.activity_generation());
    loop {
        tokio::select! {
            biased;
            () = context.cancel.cancelled() => return Err(HarnessError::Canceled),
            result = &mut call => return result.map_err(|_| HarnessError::Deadline),
            outcome = async {
                renew.as_mut().expect("polled only while a renew is in flight").await
            }, if renew.is_some() => {
                renew = None;
                settle_renewal(outcome, renew_started, context, heartbeat, progress)?;
                next_tick = next_tick.max(renew_due(context, heartbeat, progress));
            }
            () = sleep_until(next_tick), if renew.is_none() => {
                // Each renew is bounded by the interval and the session
                // deadline, so one stalled RPC cannot outlast the next tick.
                renew_started = (Instant::now(), context.tools.activity_generation());
                next_tick = renew_started.0 + heartbeat.interval;
                renew = Some(Box::pin(timeout_at(
                    next_tick.min(deadline),
                    context.api.renew(context.sandbox_id),
                )));
            }
        }
    }
}

/// Renew now when one is due, before work that needs the sandbox: a
/// provider call, or a tool call that will reach kilnd. A no-op otherwise.
/// It first forgets failed renews that later activity has outdated, then
/// runs one renew under cancellation, bounded by the interval and the
/// session deadline, with the same accounting as the heartbeat loop.
async fn renew_if_due<A: SandboxApi>(
    context: &LoopContext<'_, '_, A>,
    heartbeat: Heartbeat,
    progress: &mut Progress,
    tool_budget: Option<Duration>,
) -> Result<(), HarnessError> {
    if Instant::now() >= context.deadline {
        return Err(HarnessError::Deadline);
    }
    // Activity since the last failed renew started proves the sandbox was
    // alive after it, however close the clock readings are.
    if progress
        .renew_failure_generation
        .is_some_and(|generation| context.tools.activity_generation() > generation)
    {
        progress.renew_failures = 0;
        progress.last_renew_failure = None;
        progress.renew_failure_generation = None;
    }
    // Before a kilnd-bound tool, renew whenever the call could run past the
    // idle deadline, whatever the retry backoff after a failed renew says:
    // nothing renews while the call runs, and kilnd may never admit it.
    let now = Instant::now();
    let forced = tool_budget
        .is_some_and(|budget| now + budget >= context.tools.last_activity() + heartbeat.idle);
    if !forced && now < renew_due(context, heartbeat, progress) {
        return Ok(());
    }
    // A forced renew must succeed before the tool may start: retry a
    // transient failure until the session deadline or the estimated idle
    // deadline, whichever comes first, and fail closed after that.
    let give_up = context
        .deadline
        .min(context.tools.last_activity() + heartbeat.idle);
    loop {
        let started = (Instant::now(), context.tools.activity_generation());
        // A forced attempt also stops at the give-up instant.
        let mut attempt_deadline = (started.0 + heartbeat.interval).min(context.deadline);
        if forced {
            attempt_deadline = attempt_deadline.min(give_up);
        }
        let outcome = tokio::select! {
            () = context.cancel.cancelled() => return Err(HarnessError::Canceled),
            outcome = timeout_at(attempt_deadline, context.api.renew(context.sandbox_id)) => outcome,
        };
        let settled = settle_renewal(outcome, started, context, heartbeat, progress);
        // A deadline that passed while the renew was in flight wins, as it
        // does over every other outcome.
        if settled.is_err() && Instant::now() >= context.deadline {
            return Err(HarnessError::Deadline);
        }
        if settled? || !forced {
            return Ok(());
        }
        let retry_at = Instant::now() + context.config.lifecycle_retry_delay;
        if retry_at >= give_up {
            if Instant::now() >= context.deadline {
                return Err(HarnessError::Deadline);
            }
            return Err(HarnessError::SandboxExpired {
                reason: ExpiryReason::RenewFailed,
            });
        }
        tokio::select! {
            () = context.cancel.cancelled() => return Err(HarnessError::Canceled),
            () = sleep_until(retry_at) => {}
        }
    }
}

/// One interval after the later of the last activity and the start of the
/// last failed renew: a success is activity, and a failure is retried an
/// interval after it started, never at once.
fn renew_due<A: SandboxApi>(
    context: &LoopContext<'_, '_, A>,
    heartbeat: Heartbeat,
    progress: &Progress,
) -> Instant {
    let last = context.tools.last_activity();
    progress
        .last_renew_failure
        .map_or(last, |failed| last.max(failed))
        + heartbeat.interval
}

/// Apply one renew's outcome, and say whether it renewed: count a success as activity, count a
/// transient failure toward the budget, and end the session on proof the
/// sandbox is gone or on any other error.
fn settle_renewal<A: SandboxApi>(
    outcome: Result<Result<(), ApiError>, Elapsed>,
    started: (Instant, u64),
    context: &LoopContext<'_, '_, A>,
    heartbeat: Heartbeat,
    progress: &mut Progress,
) -> Result<bool, HarnessError> {
    match classify_renewal(outcome, context.typed_reasons) {
        Renewal::Renewed => {
            // kilnd committed it after the request was sent.
            context.tools.record_activity(started.0);
            progress.renews = progress.renews.saturating_add(1);
            progress.renew_failures = 0;
            progress.last_renew_failure = None;
            progress.renew_failure_generation = None;
            Ok(true)
        }
        Renewal::Transient => {
            progress.renew_failures = progress.renew_failures.saturating_add(1);
            progress.last_renew_failure = Some(started.0);
            progress.renew_failure_generation = Some(started.1);
            if progress.renew_failures >= heartbeat.max_failures {
                return Err(HarnessError::SandboxExpired {
                    reason: ExpiryReason::RenewFailed,
                });
            }
            Ok(false)
        }
        Renewal::Expired(reason) => Err(HarnessError::SandboxExpired { reason }),
        Renewal::Fatal(error) => Err(HarnessError::Api(error)),
    }
}

enum CreateStop {
    Failed(ApiError),
    /// Create answered `OK` with a sandbox that is not `READY`, or, when
    /// keyed, that echoes another request ID. Never retried; the error is
    /// local and names the sandbox, its state and kilnd's failure reason.
    Settled(
        #[expect(
            dead_code,
            reason = "kept for inspection; recovery finds the row by its labels"
        )]
        Box<Sandbox>,
        ApiError,
    ),
    Deadline,
    Canceled,
}

/// How a session ended, written identically to the audit and the journal.
struct Terminal {
    outcome: &'static str,
    work_outcome: &'static str,
    sandbox_deleted: bool,
    /// `None` unless cleanup was asked to release the request ID; then
    /// whether kilnd confirmed it. Audit only.
    request_id_released: Option<bool>,
}

impl Terminal {
    fn audit_event(&self, progress: &Progress) -> AuditEvent {
        AuditEvent::SessionFinished {
            outcome: self.outcome.to_string(),
            work_outcome: self.work_outcome.to_string(),
            sandbox_deleted: self.sandbox_deleted,
            renews: progress.renews,
            request_id_released: self.request_id_released,
        }
    }

    fn journal_event(&self, final_answer: Option<String>, progress: &Progress) -> JournalEvent {
        JournalEvent::SessionEnded {
            outcome: self.outcome.to_string(),
            work_outcome: self.work_outcome.to_string(),
            final_answer,
            steps: progress.steps,
            tool_calls: progress.tool_calls,
            usage: progress.usage,
            sandbox_deleted: self.sandbox_deleted,
        }
    }
}

fn validate_task(task: &SessionTask) -> Result<(), HarnessError> {
    task_problem(&task.system, &task.prompt)
        .map_or(Ok(()), |problem| Err(HarnessError::InvalidTask(problem)))
}

/// Why the engine would refuse a task, if it would. Shared with the journal
/// grammar, so a verified journal never records a task the engine rejects.
pub(crate) fn task_problem(system: &str, prompt: &str) -> Option<String> {
    if prompt.is_empty() {
        return Some("prompt is empty".to_string());
    }
    if prompt.len() > MAX_TASK_TEXT_BYTES || system.len() > MAX_TASK_TEXT_BYTES {
        return Some(format!(
            "system and prompt are each limited to {MAX_TASK_TEXT_BYTES} bytes"
        ));
    }
    None
}

/// Whether recorded limits could have come from a valid configuration: every
/// limit positive and under its hard ceiling. Lifecycle windows are not
/// recorded, so the defaults stand in for them.
pub(crate) fn limits_are_valid(limits: &SessionLimits) -> bool {
    let mut config = HarnessConfig::default();
    config.apply_limits(limits);
    validate_config(&config).is_ok()
}

fn validate_config(config: &HarnessConfig) -> Result<(), HarnessError> {
    let tools = &config.tool_limits;
    let byte_limits = [
        config.max_model_arguments_bytes,
        config.max_final_answer_bytes,
        tools.max_argv_bytes,
        tools.max_stdin_bytes,
        tools.max_file_bytes,
        tools.max_result_bytes,
        tools.max_manifest_bytes,
    ];
    let positive = config.max_steps > 0
        && config.max_tool_calls > 0
        && config.max_tool_calls_per_turn > 0
        && config.max_output_tokens_per_request > 0
        && config.max_total_input_tokens > 0
        && config.max_total_output_tokens > 0
        && config.max_total_model_output_bytes > 0
        && !config.session_timeout.is_zero()
        && !config.lifecycle_timeout.is_zero()
        && !config.lifecycle_retry_delay.is_zero()
        && !tools.timeout.is_zero()
        && tools.max_result_bytes >= 64
        && tools.max_manifest_entries > 0
        && byte_limits.iter().all(|limit| *limit > 0)
        && config.sandbox_idle_timeout >= MIN_SANDBOX_IDLE_TIMEOUT
        && config.sandbox_lifetime_margin >= MIN_SANDBOX_LIFETIME_MARGIN;
    // Hard ceilings apply to every configuration, including limits loaded
    // from a journal, so a crafted recording cannot request absurd budgets.
    let bounded = config.max_steps <= MAX_STEPS
        && config.max_tool_calls <= MAX_TOOL_CALLS
        && config.max_tool_calls_per_turn <= MAX_TOOL_CALLS_PER_TURN
        && config.session_timeout <= MAX_SESSION_TIMEOUT
        && config.lifecycle_timeout <= MAX_LIFECYCLE_TIMEOUT
        && tools.timeout <= MAX_TOOL_TIMEOUT
        && config.max_output_tokens_per_request <= MAX_OUTPUT_TOKENS_PER_REQUEST
        && config.max_total_input_tokens <= MAX_TOTAL_TOKENS
        && config.max_total_output_tokens <= MAX_TOTAL_TOKENS
        && config.max_total_model_output_bytes <= MAX_TOTAL_MODEL_OUTPUT_BYTES
        && tools.max_manifest_entries <= MAX_MANIFEST_ENTRIES
        && byte_limits.iter().all(|limit| *limit <= MAX_BYTE_LIMIT)
        && config.sandbox_idle_timeout <= MAX_SANDBOX_IDLE_TIMEOUT
        && config.sandbox_lifetime_margin <= MAX_SANDBOX_LIFETIME_MARGIN;
    if !positive || !bounded {
        return Err(HarnessError::InvalidModelCall(
            "harness budgets must be positive and within their hard ceilings".to_string(),
        ));
    }
    // Journals record durations in whole milliseconds. Refuse anything finer
    // so a recorded limit always restores to exactly the value that ran.
    let durations = [
        config.session_timeout,
        tools.timeout,
        config.lifecycle_timeout,
        config.lifecycle_retry_delay,
        config.sandbox_idle_timeout,
        config.sandbox_lifetime_margin,
    ];
    if durations
        .iter()
        .any(|duration| duration.subsec_nanos() % 1_000_000 != 0)
    {
        return Err(HarnessError::InvalidModelCall(
            "session, tool, lifecycle, and sandbox expiry durations must be whole milliseconds"
                .to_string(),
        ));
    }
    Ok(())
}

/// The provider call-ID rule, shared with the journal grammar.
pub(crate) fn call_id_is_valid(call_id: &str) -> bool {
    !call_id.is_empty()
        && call_id.len() <= 64
        && call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_call_id(call_id: &str) -> Result<(), HarnessError> {
    if !call_id_is_valid(call_id) {
        return Err(HarnessError::InvalidModelCall(
            "call_id must be 1..=64 ASCII letters, digits, '.', '_' or '-'".to_string(),
        ));
    }
    Ok(())
}

/// The expiry to send, or `None` when the daemon does not list sandbox
/// expiry (an older daemon would ignore the field, and a crashed harness can
/// then leak the sandbox, as before expiry existed).
///
/// The lifetime covers the whole session, its cleanup window, and a margin;
/// both values are clamped into the daemon's bounds, where a maximum of 0 is
/// no cap. A lifetime the daemon caps below the session would expire the
/// sandbox mid-session, so it is refused before create.
fn expiry_policy(
    config: &HarnessConfig,
    capabilities: &Capabilities,
) -> Result<Option<ExpiryPolicy>, HarnessError> {
    if let Some(limits) = &capabilities.limits
        && !limits_are_coherent(limits)
    {
        return Err(HarnessError::Api(ApiError::new(
            tonic::Code::FailedPrecondition,
            "kilnd advertised inconsistent expiry limits",
        )));
    }
    if !capabilities.supports(Feature::SandboxExpiry) {
        return Ok(None);
    }
    let limits = capabilities.limits.unwrap_or_default();
    let session_ms =
        duration_ms(config.session_timeout).saturating_add(duration_ms(config.lifecycle_timeout));
    let max_lifetime_ms = clamp_to_daemon(
        session_ms.saturating_add(duration_ms(config.sandbox_lifetime_margin)),
        limits.min_sandbox_lifetime_ms,
        limits.max_sandbox_lifetime_ms,
    );
    if max_lifetime_ms < session_ms {
        return Err(HarnessError::InvalidTask(
            "daemon max lifetime is shorter than the session".to_string(),
        ));
    }
    let idle_timeout_ms = clamp_to_daemon(
        duration_ms(config.sandbox_idle_timeout),
        limits.min_sandbox_idle_ms,
        limits.max_sandbox_idle_ms,
    );
    // The journal grammar checks the same bounds, so the engine never sends
    // an expiry it could not record.
    if let Some(problem) = expiry_problem(
        duration_ms(config.session_timeout),
        max_lifetime_ms,
        idle_timeout_ms,
    ) {
        return Err(HarnessError::Api(ApiError::new(
            tonic::Code::FailedPrecondition,
            format!("kilnd's expiry limits leave no usable policy: {problem}"),
        )));
    }
    Ok(Some(ExpiryPolicy {
        max_lifetime_ms,
        idle_timeout_ms,
    }))
}

/// For lifetime and idle each: a nonzero maximum is at least the minimum,
/// and a nonzero default lies within `[min, max]` (no upper bound when the
/// maximum is 0). Anything else leaves no range to clamp into.
fn limits_are_coherent(limits: &ApiLimits) -> bool {
    let coherent = |min: u64, max: u64, default: u64| {
        (max == 0 || max >= min)
            && (default == 0 || (default >= min && (max == 0 || default <= max)))
    };
    coherent(
        limits.min_sandbox_lifetime_ms,
        limits.max_sandbox_lifetime_ms,
        limits.default_sandbox_lifetime_ms,
    ) && coherent(
        limits.min_sandbox_idle_ms,
        limits.max_sandbox_idle_ms,
        limits.default_sandbox_idle_ms,
    )
}

/// kilnd's code floor for both expiry fields.
const KILND_MIN_EXPIRY_MS: u64 = 10_000;

/// Why a recorded expiry could not have come from the engine, if it could
/// not. These are bounds, not exact equivalence: the margin, the cleanup
/// window, and the daemon's clamping are not recorded. Shared with the
/// journal grammar.
pub(crate) fn expiry_problem(
    session_timeout_ms: u64,
    lifetime_ms: u64,
    idle_ms: u64,
) -> Option<&'static str> {
    let lifetime_ceiling = duration_ms(MAX_SESSION_TIMEOUT)
        + duration_ms(MAX_LIFECYCLE_TIMEOUT)
        + duration_ms(MAX_SANDBOX_LIFETIME_MARGIN);
    if lifetime_ms <= session_timeout_ms {
        Some("the expiry lifetime must exceed the session timeout")
    } else if lifetime_ms > lifetime_ceiling {
        Some("the expiry lifetime exceeds its hard ceiling")
    } else if idle_ms < KILND_MIN_EXPIRY_MS {
        Some("the expiry idle timeout is below kilnd's 10-second floor")
    } else if idle_ms > duration_ms(MAX_SANDBOX_IDLE_TIMEOUT) {
        Some("the expiry idle timeout exceeds its hard ceiling")
    } else {
        None
    }
}

fn clamp_to_daemon(value: u64, min: u64, max: u64) -> u64 {
    let value = value.max(min);
    if max == 0 { value } else { value.min(max) }
}

fn sandbox_request(
    session_id: &str,
    expiry: Option<ExpiryPolicy>,
    request_id: Option<String>,
) -> SandboxRequest {
    SandboxRequest {
        image: "alpine".to_string(),
        memory_bytes: 256 << 20,
        cpu_millis: 500,
        pids: 64,
        disk_bytes: 256 << 20,
        network_none: true,
        read_only_root: true,
        writable_paths: vec!["/workspace".to_string()],
        labels: BTreeMap::from([
            ("owner".to_string(), "kiln-harness".to_string()),
            ("session".to_string(), session_id.to_string()),
        ]),
        expiry,
        request_id,
    }
}

fn session_id() -> Result<String, HarnessError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HarnessError::Deadline)?
        .as_millis();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "hs-{millis:x}-{:x}-{counter:x}",
        std::process::id()
    ))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn rejected_tool_run(kind: &str) -> ToolRun {
    ToolRun {
        ok: false,
        result: json!({"ok": false, "error": kind}),
        execution_ids: Vec::new(),
        stdout: empty_output(),
        stderr: empty_output(),
        stop: None,
    }
    .bounded(64)
}

fn lifecycle_deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout
}

fn recovery_deadline(timeout: Duration) -> Instant {
    lifecycle_deadline(timeout.min(Duration::from_secs(1)))
}

/// How a tool's stop ends the session. An unusable sandbox, for a reason
/// that is not an expiry or deletion, is a local `FAILED_PRECONDITION`;
/// nothing is retried.
fn stop_error(stop: ToolStop) -> HarnessError {
    match stop {
        ToolStop::Deadline => HarnessError::Deadline,
        ToolStop::Canceled => HarnessError::Canceled,
        ToolStop::Expired(reason) => HarnessError::SandboxExpired { reason },
        ToolStop::Unusable(message) => {
            HarnessError::Api(ApiError::new(tonic::Code::FailedPrecondition, message))
        }
    }
}

fn deadline_api(message: &str) -> ApiError {
    ApiError::new(tonic::Code::DeadlineExceeded, message)
}

/// Whether a create error proves nothing was allocated. `UNIMPLEMENTED`
/// alone may be any failure, but with a typed `backend_unsupported:*`
/// reason kilnd refused before any row or backend call.
fn unambiguous_create_error(error: &ApiError, typed_reasons: bool) -> bool {
    matches!(
        error.code,
        tonic::Code::InvalidArgument
            | tonic::Code::ResourceExhausted
            | tonic::Code::FailedPrecondition
            | tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
    ) || backend_unsupported_feature(error, typed_reasons).is_some()
}

/// Whether a failed create may be sent again with the same request ID and
/// an equal request. Anything else is final at once, notably the definite
/// refusals and `ALREADY_EXISTS request_id_conflict`.
fn retryable_create_error(error: &ApiError) -> bool {
    matches!(
        error.code,
        tonic::Code::Unavailable
            | tonic::Code::Unknown
            | tonic::Code::DeadlineExceeded
            | tonic::Code::Aborted
            | tonic::Code::Internal
            | tonic::Code::Cancelled
    )
}

/// The backoff before the `n`th sleep of a create's retry phase (from 1):
/// `retry_delay * 2^(n-1)`, at most [`MAX_CREATE_BACKOFF`].
fn create_backoff(retry_delay: Duration, n: u32) -> Duration {
    let factor = 1_u32 << n.saturating_sub(1).min(16);
    retry_delay
        .checked_mul(factor)
        .unwrap_or(MAX_CREATE_BACKOFF)
        .min(MAX_CREATE_BACKOFF)
}

/// Accept a create's `OK` only for a `READY` sandbox that, when keyed,
/// echoes this request ID. kilnd answers a keyed create with the row in
/// whatever state it settled, so anything else is a sandbox to recover, not
/// a session's.
fn check_settled(sandbox: Sandbox, request_id: Option<&str>) -> Result<Sandbox, CreateStop> {
    if sandbox.state != SandboxState::Ready {
        let mut message = format!(
            "create settled sandbox {} as {}",
            sandbox.id,
            sandbox.state.as_str()
        );
        if !sandbox.failure_reason.is_empty() {
            message.push_str(": ");
            message.push_str(&sandbox.failure_reason);
        }
        let error = ApiError::new(tonic::Code::FailedPrecondition, message);
        return Err(CreateStop::Settled(Box::new(sandbox), error));
    }
    if let Some(request_id) = request_id
        && sandbox.request_id != request_id
    {
        let error = ApiError::new(
            tonic::Code::Unknown,
            format!(
                "create answered sandbox {} as {} for request ID {:?}, not {request_id:?}",
                sandbox.id,
                sandbox.state.as_str(),
                sandbox.request_id
            ),
        );
        return Err(CreateStop::Settled(Box::new(sandbox), error));
    }
    Ok(sandbox)
}

/// Send the session's create, and, when it is keyed, send it again with an
/// equal request after an ambiguous failure. Returns the result and how many
/// create RPCs were sent.
///
/// No attempt has a timeout of its own: each runs until it answers, bounded
/// only by the session deadline and cancellation, so the harness never
/// abandons one it might retry. Before every retry it backs off and rereads
/// Health, and retries only if the daemon is serving and still lists
/// idempotent create; a Health error backs off and reads again without
/// using an attempt. The retry phase ends [`HarnessConfig::lifecycle_timeout`]
/// after the first failure, or after [`MAX_CREATE_ATTEMPTS`], with the last
/// create error. Cancellation or the session deadline at any point, a
/// backoff or Health read included, is `Canceled` or `Deadline`.
/// Cancellation wins: every wait polls it first, and it is checked again
/// after each Create and Health answer, before any other early return, and
/// before an attempt is counted.
async fn create_with_retries<A: SandboxApi>(
    api: &A,
    request: &SandboxRequest,
    config: &HarnessConfig,
    deadline: Instant,
    cancel: &CancellationToken,
) -> (Result<Sandbox, CreateStop>, u32) {
    let keyed = request.request_id.is_some();
    let mut attempts = 0_u32;
    let mut sleeps = 0_u32;
    let mut window_end = None;
    loop {
        // An RPC that is never polled is not an attempt.
        if cancel.is_cancelled() {
            return (Err(CreateStop::Canceled), attempts);
        }
        attempts += 1;
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => return (Err(CreateStop::Canceled), attempts),
            result = timeout_at(deadline, api.create(request.clone())) => result,
        };
        let error = match result {
            // A held sandbox goes to the caller, whose cleanup deletes it by
            // ID whether or not the session was canceled meanwhile.
            Ok(Ok(sandbox)) => {
                return (
                    check_settled(sandbox, request.request_id.as_deref()),
                    attempts,
                );
            }
            // Cancellation that arrived while Create was failing outranks
            // the deadline, the attempt cap and the retry rules.
            _ if cancel.is_cancelled() => return (Err(CreateStop::Canceled), attempts),
            Ok(Err(error)) => error,
            Err(_) => return (Err(CreateStop::Deadline), attempts),
        };
        if !keyed || !retryable_create_error(&error) || attempts >= MAX_CREATE_ATTEMPTS {
            return (Err(CreateStop::Failed(error)), attempts);
        }
        let window_end =
            *window_end.get_or_insert_with(|| Instant::now() + config.lifecycle_timeout);
        // Back off, then read Health until it answers.
        loop {
            sleeps += 1;
            let wake = Instant::now() + create_backoff(config.lifecycle_retry_delay, sleeps);
            // Whichever ends first decides: the window with the last create
            // error, or the session deadline, during the sleep, as `Deadline`.
            if wake >= window_end && window_end <= deadline {
                return (Err(CreateStop::Failed(error)), attempts);
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return (Err(CreateStop::Canceled), attempts),
                slept = timeout_at(deadline, sleep_until(wake)) => {
                    if slept.is_err() {
                        return (Err(CreateStop::Deadline), attempts);
                    }
                }
            }
            let health = tokio::select! {
                biased;
                () = cancel.cancelled() => return (Err(CreateStop::Canceled), attempts),
                health = timeout_at(window_end.min(deadline), api.health()) => health,
            };
            if cancel.is_cancelled() {
                return (Err(CreateStop::Canceled), attempts);
            }
            match health {
                Ok(Ok(health))
                    if health.serving
                        && health.capabilities.supports(Feature::IdempotentCreate) =>
                {
                    break;
                }
                Ok(Err(_)) => {}
                Err(_) if Instant::now() >= deadline => {
                    return (Err(CreateStop::Deadline), attempts);
                }
                // Not serving, or a daemon that no longer lists the feature
                // (perhaps an older one after a restart) would ignore the
                // key; or the window closed during the read.
                Ok(Ok(_)) | Err(_) => return (Err(CreateStop::Failed(error)), attempts),
            }
        }
    }
}

/// What a confirmed cleanup reports beside absence.
#[derive(Debug, Clone, Copy, Default)]
struct CleanupOutcome {
    /// kilnd confirmed the create's request ID released. Always false when
    /// release was not asked for.
    request_id_released: bool,
}

/// Delete the sandbox and confirm its absence by listing, within `deadline`.
/// With `release`, every delete asks kilnd to release the create's request
/// ID, and, if none reported it released, up to [`EXTRA_RELEASE_DELETES`]
/// more are sent once absence is confirmed (a delete that left the row
/// `DELETING` releases nothing). Release never makes cleanup fail.
async fn cleanup_sandbox<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    deadline: Instant,
    retry_delay: Duration,
    release: bool,
) -> Result<CleanupOutcome, ApiError> {
    let mut last_error = deadline_api("sandbox cleanup deadline expired");
    let mut released = false;
    loop {
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        match timeout_at(deadline, api.delete(sandbox_id, release)).await {
            Ok(Ok(deleted)) => {
                released |= release && deleted.request_id_released;
                last_error = deadline_api("delete acknowledged but sandbox absence is unconfirmed");
            }
            Ok(Err(error)) => last_error = error,
            Err(_) => {
                last_error = deadline_api("delete RPC did not settle before cleanup deadline");
            }
        }
        match timeout_at(deadline, api.non_deleted_sandboxes()).await {
            Ok(Ok(sandboxes)) if sandboxes.iter().all(|sandbox| sandbox.id != sandbox_id) => {
                if release && !released {
                    released = release_after_absence(api, sandbox_id, deadline, retry_delay).await;
                }
                return Ok(CleanupOutcome {
                    request_id_released: released,
                });
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => last_error = error,
            Err(_) => {
                last_error = deadline_api("cleanup verification deadline expired");
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last_error);
        }
        sleep(retry_delay.min(remaining)).await;
    }
}

/// Repeat `delete(id, true)` on a sandbox whose absence is confirmed, at
/// most [`EXTRA_RELEASE_DELETES`] times within `deadline`, until one reports
/// the request ID released.
async fn release_after_absence<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    deadline: Instant,
    retry_delay: Duration,
) -> bool {
    for attempt in 0..EXTRA_RELEASE_DELETES {
        if attempt > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining <= retry_delay {
                return false;
            }
            sleep(retry_delay).await;
        }
        if Instant::now() >= deadline {
            return false;
        }
        if let Ok(Ok(deleted)) = timeout_at(deadline, api.delete(sandbox_id, true)).await
            && deleted.request_id_released
        {
            return true;
        }
    }
    false
}

async fn recover_session_sandboxes<A: SandboxApi>(
    api: &A,
    session_id: &str,
    deadline: Instant,
    retry_delay: Duration,
) -> Result<Vec<String>, ApiError> {
    let mut recovered = BTreeSet::<String>::new();
    let mut successful_list = false;
    let mut last_error = deadline_api("create recovery deadline expired");
    loop {
        if Instant::now() >= deadline {
            return if successful_list {
                Ok(recovered.into_iter().collect())
            } else {
                Err(last_error)
            };
        }
        match timeout_at(deadline, api.non_deleted_sandboxes()).await {
            Ok(Ok(sandboxes)) => {
                successful_list = true;
                let matches = sandboxes
                    .into_iter()
                    .filter(|sandbox| {
                        sandbox.labels.get("owner").map(String::as_str) == Some("kiln-harness")
                            && sandbox.labels.get("session").map(String::as_str) == Some(session_id)
                    })
                    .collect::<Vec<_>>();
                if matches.is_empty() && !recovered.is_empty() {
                    return Ok(recovered.into_iter().collect());
                }
                for sandbox in matches {
                    recovered.insert(sandbox.id.clone());
                    // Never release here: the create's outcome was not held.
                    cleanup_sandbox(api, &sandbox.id, deadline, retry_delay, false).await?;
                }
            }
            Ok(Err(error)) => last_error = error,
            Err(_) => last_error = deadline_api("create recovery listing deadline expired"),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        sleep(retry_delay.min(remaining)).await;
    }
}

fn empty_output() -> OutputEvidence {
    OutputEvidence {
        observed_bytes: 0,
        total_bytes_known: true,
        truncated: false,
    }
}

#[cfg(test)]
mod heartbeat_test {
    use super::Heartbeat;
    use std::time::Duration;

    #[test]
    fn three_renews_per_idle_window_and_two_failures_allowed() {
        for (idle_ms, interval_ms) in [(120_000, 40_000), (10_000, 3_334), (3_600_000, 1_200_000)] {
            let heartbeat = Heartbeat::for_idle_ms(idle_ms);
            assert_eq!(heartbeat.interval, Duration::from_millis(interval_ms));
            assert_eq!(heartbeat.max_failures, 2, "{idle_ms}");
        }
        // At most one renew a second, however short the idle timeout.
        let heartbeat = Heartbeat::for_idle_ms(1_500);
        assert_eq!(heartbeat.interval, Duration::from_secs(1));
        assert_eq!(heartbeat.max_failures, 1);
    }
}
