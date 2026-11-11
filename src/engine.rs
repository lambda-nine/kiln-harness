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
    ApiError, ApiLimits, Capabilities, ExpiryPolicy, ExpiryReason, Feature, SandboxApi,
    SandboxRequest,
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
    ToolError, ToolLimits, ToolRegistry, ToolRun, ToolSession, ToolStop,
    recover_execution_evidence, sanitize_call_bounded,
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
        let heartbeat = expiry.map(|policy| Heartbeat::for_idle_ms(policy.idle_timeout_ms));
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
                },
            );
            return Err(error.into());
        }
        let mut progress = Progress::default();

        // Cancellation and the deadline both drop the create RPC. Its outcome
        // is then ambiguous, so either goes through label-based recovery.
        let create = tokio::select! {
            () = cancel.cancelled() => Err(CreateStop::Canceled),
            result = timeout_at(
                deadline,
                self.api.create(sandbox_request(&session_id, expiry)),
            ) => {
                match result {
                    Ok(Ok(sandbox)) => Ok(sandbox),
                    Ok(Err(error)) => Err(CreateStop::Failed(error)),
                    Err(_) => Err(CreateStop::Deadline),
                }
            }
        };
        let sandbox = match create {
            Ok(sandbox) => sandbox,
            Err(stop) => {
                let (error, interrupted, work_outcome) = match stop {
                    CreateStop::Failed(error) => (error, None, "create_failed"),
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
                let unambiguous = unambiguous_create_error(&error, typed_reasons);
                let absence_confirmed =
                    recovery.is_ok() && (recovered || (!deadline_expired && unambiguous));
                let recovery_audit = self.audit.append(
                    &session_id,
                    None,
                    AuditEvent::SandboxRecovered {
                        sandbox_ids: recovered_ids,
                        deleted: absence_confirmed,
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
                    .start_session(&self.api, sandbox_id.clone(), deadline, typed_reasons) => {
                    Some(started)
                }
            };
            let (tools, baseline) = match started {
                Some(Ok(value)) => value,
                None => {
                    // The dropped baseline RPC asks kilnd to cancel it; record
                    // any execution it already assigned, then clean up.
                    let recovery_deadline = recovery_deadline(self.config.lifecycle_timeout);
                    let execution_ids = recover_execution_evidence(
                        &self.api,
                        &sandbox_id,
                        &BTreeSet::new(),
                        recovery_deadline,
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
                    if execution_ids.is_empty() {
                        let recovery_deadline = recovery_deadline(self.config.lifecycle_timeout);
                        execution_ids = recover_execution_evidence(
                            &self.api,
                            &sandbox_id,
                            &BTreeSet::new(),
                            recovery_deadline,
                        )
                        .await
                        .execution_ids;
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
                    match &error {
                        ToolError::Api(api_error) => {
                            if let Some(reason) = ExpiryReason::of(api_error, typed_reasons) {
                                return Err(HarnessError::SandboxExpired { reason });
                            }
                        }
                        ToolError::Unusable {
                            expiry: Some(reason),
                            ..
                        } => return Err(HarnessError::SandboxExpired { reason: *reason }),
                        ToolError::Unusable { expiry: None, .. } => {
                            return Err(HarnessError::Api(unusable_sandbox()));
                        }
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
        let cleanup_deadline = lifecycle_deadline(self.config.lifecycle_timeout);
        let cleanup = cleanup_sandbox(
            &self.api,
            &sandbox_id,
            cleanup_deadline,
            self.config.lifecycle_retry_delay,
        )
        .await;
        let deleted = cleanup.is_ok();
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
            return final_answer(audit, context, progress, turn, stop_reason, text);
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
                let recovery_window = config.lifecycle_timeout.min(Duration::from_secs(1));
                if let Some(recovery_deadline) = Instant::now().checked_add(recovery_window) {
                    let recovered = context
                        .tools
                        .recover_new_execution_evidence(&known_execution_ids, recovery_deadline)
                        .await;
                    run.attach_recovered(recovered);
                }
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
            match stop {
                Some(ToolStop::Deadline) => return Err(HarnessError::Deadline),
                Some(ToolStop::Canceled) => return Err(HarnessError::Canceled),
                Some(ToolStop::Expired(reason)) => {
                    return Err(HarnessError::SandboxExpired { reason });
                }
                Some(ToolStop::Unusable) => return Err(HarnessError::Api(unusable_sandbox())),
                None => {}
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

/// When to renew the sandbox while a provider call runs.
#[derive(Debug, Clone, Copy)]
struct Heartbeat {
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
/// the last activity kilnd saw (ready, an execution's start or finish, or a
/// renew), however many provider calls that spans. An overdue renew runs
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
    // Activity since the last failed renew proves the sandbox was alive
    // after it, so earlier failures no longer count.
    if progress
        .last_renew_failure
        .is_some_and(|failed| context.tools.last_activity() > failed)
    {
        progress.renew_failures = 0;
    }
    if Instant::now() >= renew_due(context, heartbeat, progress) {
        let started = Instant::now();
        let outcome = tokio::select! {
            () = context.cancel.cancelled() => return Err(HarnessError::Canceled),
            outcome = timeout_at(
                (started + heartbeat.interval).min(deadline),
                context.api.renew(context.sandbox_id),
            ) => outcome,
        };
        settle_renewal(outcome, started, context, heartbeat, progress)?;
    }
    let call = timeout_at(deadline, provider.complete(request));
    tokio::pin!(call);
    let mut next_tick = renew_due(context, heartbeat, progress);
    let mut renew: Option<RenewCall<'_>> = None;
    let mut renew_started = Instant::now();
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
                renew_started = Instant::now();
                next_tick = renew_started + heartbeat.interval;
                renew = Some(Box::pin(timeout_at(
                    next_tick.min(deadline),
                    context.api.renew(context.sandbox_id),
                )));
            }
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

/// Apply one renew's outcome: count a success as activity, count a
/// transient failure toward the budget, and end the session on proof the
/// sandbox is gone or on any other error.
fn settle_renewal<A: SandboxApi>(
    outcome: Result<Result<(), ApiError>, Elapsed>,
    started: Instant,
    context: &LoopContext<'_, '_, A>,
    heartbeat: Heartbeat,
    progress: &mut Progress,
) -> Result<(), HarnessError> {
    match classify_renewal(outcome, context.typed_reasons) {
        Renewal::Renewed => {
            context.tools.record_activity();
            progress.renews = progress.renews.saturating_add(1);
            progress.renew_failures = 0;
            progress.last_renew_failure = None;
            Ok(())
        }
        Renewal::Transient => {
            progress.renew_failures = progress.renew_failures.saturating_add(1);
            progress.last_renew_failure = Some(started);
            if progress.renew_failures >= heartbeat.max_failures {
                return Err(HarnessError::SandboxExpired {
                    reason: ExpiryReason::RenewFailed,
                });
            }
            Ok(())
        }
        Renewal::Expired(reason) => Err(HarnessError::SandboxExpired { reason }),
        Renewal::Fatal(error) => Err(HarnessError::Api(error)),
    }
}

enum CreateStop {
    Failed(ApiError),
    Deadline,
    Canceled,
}

/// How a session ended, written identically to the audit and the journal.
struct Terminal {
    outcome: &'static str,
    work_outcome: &'static str,
    sandbox_deleted: bool,
}

impl Terminal {
    fn audit_event(&self, progress: &Progress) -> AuditEvent {
        AuditEvent::SessionFinished {
            outcome: self.outcome.to_string(),
            work_outcome: self.work_outcome.to_string(),
            sandbox_deleted: self.sandbox_deleted,
            renews: progress.renews,
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

fn sandbox_request(session_id: &str, expiry: Option<ExpiryPolicy>) -> SandboxRequest {
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

/// An execution reported that the sandbox cannot take another call, for a
/// reason that is not an expiry or deletion. Nothing is retried.
fn unusable_sandbox() -> ApiError {
    ApiError::new(
        tonic::Code::FailedPrecondition,
        "execution left the sandbox unusable",
    )
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
    ) || (error.code == tonic::Code::Unimplemented
        && error
            .typed_reason(typed_reasons)
            .and_then(|reason| reason.strip_prefix("backend_unsupported:"))
            .is_some_and(|feature| !feature.is_empty()))
}

async fn cleanup_sandbox<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    deadline: Instant,
    retry_delay: Duration,
) -> Result<(), ApiError> {
    let mut last_error = deadline_api("sandbox cleanup deadline expired");
    loop {
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        match timeout_at(deadline, api.delete(sandbox_id)).await {
            Ok(Ok(())) => {
                last_error = deadline_api("delete acknowledged but sandbox absence is unconfirmed");
            }
            Ok(Err(error)) => last_error = error,
            Err(_) => {
                last_error = deadline_api("delete RPC did not settle before cleanup deadline");
            }
        }
        match timeout_at(deadline, api.non_deleted_sandboxes()).await {
            Ok(Ok(sandboxes)) if sandboxes.iter().all(|sandbox| sandbox.id != sandbox_id) => {
                return Ok(());
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
                    cleanup_sandbox(api, &sandbox.id, deadline, retry_delay).await?;
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
