use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use thiserror::Error;
use tokio::time::{Instant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::api::{ApiError, SandboxApi, SandboxRequest};
use crate::audit::{AuditError, AuditEvent, AuditSink, ContentSummary, OutputEvidence};
use crate::provider::{ModelAction, Provider, ProviderContext, ProviderError, TranscriptItem};
use crate::tools::{
    ToolError, ToolLimits, ToolRegistry, ToolRun, ToolStop, recover_execution_evidence,
};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub session_timeout: Duration,
    pub lifecycle_timeout: Duration,
    pub lifecycle_retry_delay: Duration,
    pub max_model_arguments_bytes: usize,
    pub max_final_answer_bytes: usize,
    pub tool_limits: ToolLimits,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_tool_calls: 6,
            session_timeout: Duration::from_secs(30),
            lifecycle_timeout: Duration::from_secs(10),
            lifecycle_retry_delay: Duration::from_millis(100),
            max_model_arguments_bytes: 64 << 10,
            max_final_answer_bytes: 64 << 10,
            tool_limits: ToolLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutcome {
    pub session_id: String,
    pub sandbox_id: String,
    pub final_answer: String,
    pub steps: u32,
    pub tool_calls: u32,
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Audit(#[from] AuditError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("tool setup failed: {0}")]
    ToolSetup(#[from] ToolError),
    #[error("session deadline expired")]
    Deadline,
    #[error("session canceled")]
    Canceled,
    #[error("model exhausted the {0}-step budget without a final answer")]
    StepBudget(u32),
    #[error("model exceeded the {0}-tool-call budget")]
    ToolBudget(u32),
    #[error("invalid model tool call: {0}")]
    InvalidModelCall(String),
    #[error("final answer exceeds the configured byte limit")]
    FinalAnswerTooLarge,
    #[error("sandbox cleanup failed after session outcome: {0}")]
    Cleanup(ApiError),
    #[error("create outcome is ambiguous after recovery window: {0}")]
    CreateUncertain(ApiError),
}

pub struct Harness<A, P, S> {
    api: A,
    provider: P,
    audit: S,
    config: HarnessConfig,
    registry: ToolRegistry,
}

impl<A, P, S> Harness<A, P, S>
where
    A: SandboxApi,
    P: Provider,
    S: AuditSink,
{
    #[must_use]
    pub fn new(api: A, provider: P, audit: S, config: HarnessConfig) -> Self {
        let registry = ToolRegistry::new(config.tool_limits.clone());
        Self {
            api,
            provider,
            audit,
            config,
            registry,
        }
    }

    #[must_use]
    pub fn audit(&self) -> &S {
        &self.audit
    }

    /// Run one session and wait until its sandbox has been deleted.
    ///
    /// # Errors
    ///
    /// Returns an error for provider, budget, audit, kilnd, deadline,
    /// cancellation, or cleanup failures.
    #[allow(clippy::too_many_lines)]
    pub async fn run(&mut self, cancel: CancellationToken) -> Result<SessionOutcome, HarnessError> {
        validate_config(&self.config)?;
        let session_id = session_id()?;
        let deadline = Instant::now()
            .checked_add(self.config.session_timeout)
            .ok_or(HarnessError::Deadline)?;
        self.audit.append(
            &session_id,
            None,
            AuditEvent::SessionOpened {
                session_timeout_ms: duration_ms(self.config.session_timeout),
                network: "none".to_string(),
            },
        )?;

        let create = timeout_at(deadline, self.api.create(sandbox_request(&session_id))).await;
        let sandbox = match create {
            Ok(Ok(sandbox)) => sandbox,
            failed => {
                let (error, deadline_expired) = match failed {
                    Ok(Err(error)) => (error, false),
                    Err(_) => (deadline_api("create response deadline expired"), true),
                    Ok(Ok(_)) => unreachable!("successful create handled above"),
                };
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
                let absence_confirmed = recovery.is_ok()
                    && (recovered || (!deadline_expired && unambiguous_create_error(&error)));
                let recovery_audit = self.audit.append(
                    &session_id,
                    None,
                    AuditEvent::SandboxRecovered {
                        sandbox_ids: recovered_ids,
                        deleted: absence_confirmed,
                    },
                );
                let finish_audit = self.audit.append(
                    &session_id,
                    None,
                    AuditEvent::SessionFinished {
                        outcome: "create_failed".to_string(),
                        sandbox_deleted: absence_confirmed,
                    },
                );
                if let Err(cleanup_error) = recovery {
                    return Err(HarnessError::Cleanup(cleanup_error));
                }
                recovery_audit?;
                finish_audit?;
                if !recovered && (deadline_expired || !unambiguous_create_error(&error)) {
                    return Err(HarnessError::CreateUncertain(error));
                }
                return Err(if deadline_expired {
                    HarnessError::Deadline
                } else {
                    HarnessError::Api(error)
                });
            }
        };
        let sandbox_id = sandbox.id.clone();

        let body = async {
            self.audit.append(
                &session_id,
                Some(&sandbox_id),
                AuditEvent::SandboxCreated {
                    backend_id: sandbox.backend_id,
                },
            )?;
            if cancel.is_cancelled() {
                return Err(HarnessError::Canceled);
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Deadline);
            }

            let (tools, baseline) = match self
                .registry
                .start_session(&self.api, sandbox_id.clone(), deadline)
                .await
            {
                Ok(value) => value,
                Err(error) => {
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
            run_loop(
                &mut self.provider,
                &mut self.audit,
                &self.config,
                self.registry.definitions(),
                &tools,
                &session_id,
                &sandbox_id,
                deadline,
                &cancel,
                baseline_execution_ids,
            )
            .await
        }
        .await;

        // Cleanup has a separate finite settlement window. Delete is
        // idempotently retried and absence is checked after ambiguous RPCs.
        let cleanup_deadline = lifecycle_deadline(self.config.lifecycle_timeout);
        let cleanup = cleanup_sandbox(
            &self.api,
            &sandbox_id,
            cleanup_deadline,
            self.config.lifecycle_retry_delay,
        )
        .await;
        let deleted = cleanup.is_ok();
        let outcome_name = match &body {
            Ok(_) => "completed",
            Err(HarnessError::Canceled) => "canceled",
            Err(HarnessError::Deadline) => "deadline",
            Err(_) => "failed",
        };
        let finish_audit = self.audit.append(
            &session_id,
            Some(&sandbox_id),
            AuditEvent::SessionFinished {
                outcome: outcome_name.to_string(),
                sandbox_deleted: deleted,
            },
        );

        if let Err(error) = cleanup {
            return Err(HarnessError::Cleanup(error));
        }
        if let Err(error) = finish_audit {
            return Err(HarnessError::Audit(error));
        }
        body
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_loop<A, P, S>(
    provider: &mut P,
    audit: &mut S,
    config: &HarnessConfig,
    definitions: &[crate::provider::ToolDefinition],
    tools: &crate::tools::ToolSession<'_, A>,
    session_id: &str,
    sandbox_id: &str,
    deadline: Instant,
    cancel: &CancellationToken,
    initial_execution_ids: Vec<String>,
) -> Result<SessionOutcome, HarnessError>
where
    A: SandboxApi,
    P: Provider,
    S: AuditSink,
{
    let mut transcript = Vec::new();
    let mut tool_calls = 0u32;
    let mut known_execution_ids = initial_execution_ids.into_iter().collect::<BTreeSet<_>>();

    for turn in 1..=config.max_steps {
        if cancel.is_cancelled() {
            return Err(HarnessError::Canceled);
        }
        if Instant::now() >= deadline {
            return Err(HarnessError::Deadline);
        }
        let context = ProviderContext {
            session_id,
            turn,
            tools: definitions,
            transcript: &transcript,
        };
        let action = tokio::select! {
            () = cancel.cancelled() => return Err(HarnessError::Canceled),
            result = timeout_at(deadline, provider.next(context)) => {
                result.map_err(|_| HarnessError::Deadline)??
            }
        };

        match action {
            ModelAction::FinalAnswer { text } => {
                let rejected = text.len() > config.max_final_answer_bytes;
                audit.append(
                    session_id,
                    Some(sandbox_id),
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
                if rejected {
                    return Err(HarnessError::FinalAnswerTooLarge);
                }
                audit.append(
                    session_id,
                    Some(sandbox_id),
                    AuditEvent::FinalAnswer {
                        turn,
                        answer: ContentSummary::bytes(text.as_bytes()),
                    },
                )?;
                return Ok(SessionOutcome {
                    session_id: session_id.to_string(),
                    sandbox_id: sandbox_id.to_string(),
                    final_answer: text,
                    steps: turn,
                    tool_calls,
                });
            }
            ModelAction::ToolCall {
                call_id,
                tool,
                arguments,
            } => {
                // Provider IDs are untrusted content and stay in the transient transcript.
                let audit_call_id = format!("call-{turn}");
                let call_id_valid = validate_call_id(&call_id).is_ok();
                let tool_name_valid = tool.len() <= 64
                    && !tool.is_empty()
                    && tool.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    });
                let next_tool_calls = tool_calls.saturating_add(1);
                let audited_tool = definitions
                    .iter()
                    .find(|definition| definition.name == tool)
                    .map_or_else(
                        || "unknown".to_string(),
                        |definition| definition.name.clone(),
                    );
                let argument_summary =
                    ContentSummary::json_bounded(&arguments, config.max_model_arguments_bytes)?;
                let rejected = !call_id_valid
                    || !tool_name_valid
                    || next_tool_calls > config.max_tool_calls
                    || argument_summary.is_none();
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
                        tool: Some(audited_tool.clone()),
                        arguments: argument_summary.clone(),
                    },
                )?;
                if !call_id_valid {
                    return Err(HarnessError::InvalidModelCall(
                        "call_id must be 1..=64 ASCII letters, digits, '.', '_' or '-'".to_string(),
                    ));
                }
                if !tool_name_valid {
                    return Err(HarnessError::InvalidModelCall(
                        "tool name must be 1..=64 lowercase ASCII letters, digits or '_'"
                            .to_string(),
                    ));
                }
                if next_tool_calls > config.max_tool_calls {
                    return Err(HarnessError::ToolBudget(config.max_tool_calls));
                }
                tool_calls = next_tool_calls;
                let model_action = ModelAction::ToolCall {
                    call_id: call_id.clone(),
                    tool: audited_tool.clone(),
                    arguments: argument_summary.as_ref().map_or_else(
                        || json!({"omitted": "arguments exceeded byte limit"}),
                        |_| arguments.clone(),
                    ),
                };
                transcript.push(TranscriptItem::Model(model_action));

                let mut run = if argument_summary.is_none() {
                    rejected_tool_run(
                        "model_arguments_too_large",
                        config.max_model_arguments_bytes,
                    )
                } else {
                    tokio::select! {
                        () = cancel.cancelled() => ToolRun::interrupted(ToolStop::Canceled),
                        run = tools.execute(&audited_tool, arguments, deadline) => run,
                    }
                };
                if run.execution_ids.is_empty()
                    && (!run.stdout.total_bytes_known || !run.stderr.total_bytes_known)
                {
                    let recovery_window = config.lifecycle_timeout.min(Duration::from_secs(1));
                    if let Some(recovery_deadline) = Instant::now().checked_add(recovery_window) {
                        let recovered = tools
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
                        audit_call_id,
                        tool: audited_tool.clone(),
                        ok: run.ok,
                        execution_ids: run.execution_ids,
                        result: result_summary,
                        stdout: run.stdout,
                        stderr: run.stderr,
                    },
                )?;
                transcript.push(TranscriptItem::ToolResult {
                    call_id,
                    tool: audited_tool,
                    result: run.result,
                });
                match stop {
                    Some(ToolStop::Deadline) => return Err(HarnessError::Deadline),
                    Some(ToolStop::Canceled) => return Err(HarnessError::Canceled),
                    None => {}
                }
            }
        }
    }
    Err(HarnessError::StepBudget(config.max_steps))
}

fn validate_config(config: &HarnessConfig) -> Result<(), HarnessError> {
    if config.max_steps == 0
        || config.max_tool_calls == 0
        || config.session_timeout.is_zero()
        || config.lifecycle_timeout.is_zero()
        || config.lifecycle_timeout > MAX_LIFECYCLE_TIMEOUT
        || config.lifecycle_retry_delay.is_zero()
        || config.tool_limits.timeout.is_zero()
        || config.tool_limits.max_argv_bytes == 0
        || config.tool_limits.max_stdin_bytes == 0
        || config.tool_limits.max_file_bytes == 0
        || config.tool_limits.max_result_bytes < 64
        || config.tool_limits.max_manifest_bytes == 0
        || config.tool_limits.max_manifest_entries == 0
        || config.max_model_arguments_bytes == 0
        || config.max_final_answer_bytes == 0
    {
        return Err(HarnessError::InvalidModelCall(
            "all harness budgets must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_call_id(call_id: &str) -> Result<(), HarnessError> {
    if call_id.is_empty()
        || call_id.len() > 64
        || !call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(HarnessError::InvalidModelCall(
            "call_id must be 1..=64 ASCII letters, digits, '.', '_' or '-'".to_string(),
        ));
    }
    Ok(())
}

fn sandbox_request(session_id: &str) -> SandboxRequest {
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

fn rejected_tool_run(kind: &str, _limit: usize) -> ToolRun {
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

fn deadline_api(message: &str) -> ApiError {
    ApiError {
        code: "deadline_exceeded".to_string(),
        message: message.to_string(),
    }
}

fn unambiguous_create_error(error: &ApiError) -> bool {
    matches!(
        error.code.as_str(),
        "invalid_argument"
            | "resource_exhausted"
            | "failed_precondition"
            | "permission_denied"
            | "unauthenticated"
    )
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
            Ok(Ok(())) => return Ok(()),
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
