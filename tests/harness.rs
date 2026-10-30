use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiln_harness::api::{
    ApiError, CapturedOutput, Execution, ExecutionState, Health, NodeReservations, Sandbox,
    SandboxApi, SandboxExec, SandboxRequest,
};
use kiln_harness::audit::{AuditError, AuditEvent, AuditSink, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError};
use kiln_harness::provider::{
    ModelAction, Provider, ProviderContext, ProviderError, ScriptedProvider, TranscriptItem,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct FakeApi {
    state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    creates: Vec<SandboxRequest>,
    execs: Vec<SandboxExec>,
    executions: Vec<Execution>,
    deleted: Vec<String>,
    files: BTreeMap<String, Vec<u8>>,
    next_execution: u64,
    create_delay: Duration,
    create_ready_at: Option<std::time::Instant>,
    suppress_create_record: bool,
    create_error_after_record: bool,
    tool_exec_delay: Duration,
    baseline_exec_delay: Duration,
    fail_baseline: bool,
    delete_failures: u32,
    delete_attempts: u32,
}

impl FakeApi {
    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake state lock")
    }
}

#[async_trait]
impl SandboxApi for FakeApi {
    async fn health(&self) -> Result<Health, ApiError> {
        let state = self.state();
        Ok(Health {
            serving: true,
            detail: String::new(),
            active_sandboxes: u32::from(state.creates.len() > state.deleted.len()),
            running_executions: 0,
            reservations: NodeReservations::default(),
        })
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        let labels = request.labels.clone();
        let (delay, fail) = {
            let mut state = self.state();
            let delay = state.create_delay;
            let fail = state.create_error_after_record;
            if !state.suppress_create_record {
                state.creates.push(request);
                state.create_ready_at = std::time::Instant::now().checked_add(delay);
            }
            (delay, fail)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if fail {
            return Err(api_error("unavailable", "create response lost"));
        }
        Ok(Sandbox {
            id: "sb-0000000000000001".to_string(),
            backend_id: "test-backend-1".to_string(),
            labels,
        })
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let (execution, delay) = {
            let mut state = self.state();
            state.next_execution += 1;
            let id = format!("ex-{:016x}", state.next_execution);
            state.execs.push(request.clone());
            let argv = &request.argv;
            let (exit_code, stdout, stderr) = match argv.first().map(String::as_str) {
                Some("/usr/bin/find") => {
                    if state.fail_baseline && state.execs.len() == 1 {
                        (1, Vec::new(), b"find failed".to_vec())
                    } else {
                        let mut output = Vec::new();
                        for path in state.files.keys() {
                            output.extend_from_slice(path.as_bytes());
                            output.push(0);
                        }
                        (0, output, Vec::new())
                    }
                }
                Some("/bin/dd") => {
                    let path = argv
                        .iter()
                        .find_map(|arg| arg.strip_prefix("of="))
                        .expect("dd output path")
                        .to_string();
                    state.files.insert(path, request.stdin.clone());
                    (0, Vec::new(), Vec::new())
                }
                Some("/bin/stat") => {
                    let path = argv.last().expect("stat path");
                    match state.files.get(path) {
                        Some(bytes) => (0, format!("{}\n", bytes.len()).into_bytes(), Vec::new()),
                        None => (1, Vec::new(), b"not found".to_vec()),
                    }
                }
                Some("/usr/bin/head") => {
                    let limit = argv[2].parse::<usize>().expect("head limit");
                    let path = argv.last().expect("head path");
                    match state.files.get(path) {
                        Some(bytes) => (0, bytes[..bytes.len().min(limit)].to_vec(), Vec::new()),
                        None => (1, Vec::new(), b"not found".to_vec()),
                    }
                }
                Some("/bin/ls") => {
                    let names: BTreeSet<_> = state
                        .files
                        .keys()
                        .filter_map(|path| path.strip_prefix("/workspace/"))
                        .collect();
                    let listing = names.into_iter().collect::<Vec<_>>().join("\n") + "\n";
                    (0, listing.into_bytes(), Vec::new())
                }
                Some("/usr/bin/wc") => {
                    let path = argv.last().expect("wc path");
                    let size = state.files.get(path).map_or(0, Vec::len);
                    (0, format!("{size} {path}\n").into_bytes(), Vec::new())
                }
                Some("/bin/big-output") => (0, vec![b'x'; 100 << 10], Vec::new()),
                _ => (127, Vec::new(), b"unsupported fake command".to_vec()),
            };
            let execution = Execution {
                id,
                state: ExecutionState::Exited,
                exit_code: Some(exit_code),
                failure_reason: String::new(),
                stdout: captured(stdout),
                stderr: captured(stderr),
            };
            state.executions.push(execution.clone());
            let delay = if argv.first().map(String::as_str) == Some("/usr/bin/find") {
                state.baseline_exec_delay
            } else {
                state.tool_exec_delay
            };
            (execution, delay)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(execution)
    }

    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError> {
        let mut state = self.state();
        state.delete_attempts += 1;
        if state
            .create_ready_at
            .is_some_and(|ready_at| std::time::Instant::now() < ready_at)
        {
            return Err(api_error(
                "failed_precondition",
                "create operation is still in progress",
            ));
        }
        if state.delete_failures > 0 {
            state.delete_failures -= 1;
            return Err(api_error("unavailable", "transient delete failure"));
        }
        state.deleted.push(sandbox_id.to_string());
        Ok(())
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        let state = self.state();
        Ok(state.executions.iter().rev().cloned().collect())
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        let state = self.state();
        Ok(if state.creates.len() > state.deleted.len() {
            vec![Sandbox {
                id: "sb-0000000000000001".to_string(),
                backend_id: "test-backend-1".to_string(),
                labels: state
                    .creates
                    .last()
                    .map_or_else(BTreeMap::new, |request| request.labels.clone()),
            }]
        } else {
            Vec::new()
        })
    }
}

fn api_error(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.to_string(),
        message: message.to_string(),
    }
}

fn captured(data: Vec<u8>) -> CapturedOutput {
    CapturedOutput {
        observed_bytes: data.len() as u64,
        data,
        total_bytes_known: true,
        truncated: false,
    }
}

fn short_lifecycle_config() -> HarnessConfig {
    HarnessConfig {
        lifecycle_timeout: Duration::from_millis(100),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    }
}

struct SlowProvider {
    delay: Duration,
}

#[async_trait]
impl Provider for SlowProvider {
    async fn next(&mut self, _context: ProviderContext<'_>) -> Result<ModelAction, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(ModelAction::FinalAnswer {
            text: "late".to_string(),
        })
    }
}

struct FailingAudit {
    calls: usize,
    fail_at: usize,
}

impl AuditSink for FailingAudit {
    fn append(
        &mut self,
        _session_id: &str,
        _sandbox_id: Option<&str>,
        _event: AuditEvent,
    ) -> Result<(), AuditError> {
        self.calls += 1;
        if self.calls == self.fail_at {
            Err(AuditError::Io(std::io::Error::other(
                "injected audit failure",
            )))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn deterministic_vertical_uses_one_no_network_sandbox_and_cleans_it() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        HarnessConfig::default(),
    );

    let outcome = harness
        .run(CancellationToken::new())
        .await
        .expect("demo succeeds");

    assert_eq!(outcome.steps, 6);
    assert_eq!(outcome.tool_calls, 5);
    let state = observer.state();
    assert_eq!(state.creates.len(), 1);
    assert_eq!(state.deleted, ["sb-0000000000000001"]);
    assert_eq!(state.execs.len(), 7);
    let request = &state.creates[0];
    assert!(request.network_none);
    assert!(request.read_only_root);
    assert_eq!(request.writable_paths, ["/workspace"]);
    assert!(
        state
            .execs
            .iter()
            .all(|request| request.sandbox_id == "sb-0000000000000001")
    );
    drop(state);

    let records = &harness.audit().records;
    assert!(matches!(
        records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            sandbox_deleted: true,
            ..
        })
    ));
    let linked_tools = records
        .iter()
        .filter_map(|record| match &record.event {
            AuditEvent::ToolCompleted { execution_ids, .. } => Some(execution_ids),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(linked_tools.len(), 5);
    assert!(linked_tools.iter().all(|ids| !ids.is_empty()));
}

#[tokio::test]
async fn invalid_workspace_path_is_rejected_before_kilnd_exec() {
    let api = FakeApi::default();
    let observer = api.clone();
    let provider = ScriptedProvider::new([
        ModelAction::ToolCall {
            call_id: "bad-path".to_string(),
            tool: "write_file".to_string(),
            arguments: json!({"path": "../host", "content": "no"}),
        },
        ModelAction::FinalAnswer {
            text: "handled".to_string(),
        },
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        HarnessConfig::default(),
    );

    harness
        .run(CancellationToken::new())
        .await
        .expect("provider can recover from tool rejection");

    let state = observer.state();
    assert_eq!(state.execs.len(), 1, "only the fixed baseline find ran");
    assert_eq!(state.deleted.len(), 1);
    drop(state);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ToolCompleted {
            ok: false,
            execution_ids,
            ..
        } if execution_ids.is_empty()
    )));
}

#[tokio::test]
async fn provider_failure_still_deletes_the_sandbox() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([]),
        MemoryAudit::default(),
        HarnessConfig::default(),
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("empty script fails");

    assert!(matches!(error, HarnessError::Provider(_)));
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn audit_excludes_model_arguments_and_file_contents() {
    let secret = "TOPSECRET-should-not-enter-audit";
    let provider = ScriptedProvider::new([
        ModelAction::ToolCall {
            call_id: secret.to_string(),
            tool: "write_file".to_string(),
            arguments: json!({"path": "secret.txt", "content": secret}),
        },
        ModelAction::FinalAnswer {
            text: "done".to_string(),
        },
    ]);
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        HarnessConfig::default(),
    );

    harness
        .run(CancellationToken::new())
        .await
        .expect("session succeeds");

    let encoded = serde_json::to_string(&harness.audit().records).expect("serialize audit");
    assert!(!encoded.contains(secret));
    assert!(!encoded.contains("secret.txt"));
    assert!(encoded.contains("sha256"));
}

#[tokio::test]
async fn audit_correlation_is_host_generated_and_preserves_provider_result_routing() {
    const PROVIDER_ID: &str = "TOPSECRET-provider-reuses-this-id";
    struct ReusedIdProvider;

    #[async_trait]
    impl Provider for ReusedIdProvider {
        async fn next(
            &mut self,
            context: ProviderContext<'_>,
        ) -> Result<ModelAction, ProviderError> {
            if context.turn > 1 {
                assert!(matches!(
                    context.transcript.last(),
                    Some(TranscriptItem::ToolResult { call_id, .. }) if call_id == PROVIDER_ID
                ));
                assert!(matches!(
                    &context.transcript[context.transcript.len() - 2],
                    TranscriptItem::Model(ModelAction::ToolCall { call_id, .. })
                        if call_id == PROVIDER_ID
                ));
            }
            Ok(if context.turn <= 2 {
                ModelAction::ToolCall {
                    call_id: PROVIDER_ID.to_string(),
                    tool: "workspace_diff".to_string(),
                    arguments: json!({}),
                }
            } else {
                ModelAction::FinalAnswer {
                    text: "done".to_string(),
                }
            })
        }
    }

    let mut harness = Harness::new(
        FakeApi::default(),
        ReusedIdProvider,
        MemoryAudit::default(),
        short_lifecycle_config(),
    );
    harness
        .run(CancellationToken::new())
        .await
        .expect("session succeeds");

    let mut model_ids = Vec::new();
    let mut completed_ids = Vec::new();
    for record in &harness.audit().records {
        match &record.event {
            AuditEvent::ModelTurn {
                audit_call_id: Some(id),
                ..
            } => model_ids.push(id.as_str()),
            AuditEvent::ToolCompleted { audit_call_id, .. } => {
                completed_ids.push(audit_call_id.as_str());
            }
            _ => {}
        }
    }
    assert_eq!(model_ids, ["call-1", "call-2"]);
    assert_eq!(completed_ids, model_ids);
    let encoded = serde_json::to_string(&harness.audit().records).expect("serialize audit");
    assert!(!encoded.contains(PROVIDER_ID));
}

#[tokio::test]
async fn tool_call_budget_fails_closed_and_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    let provider = ScriptedProvider::new([
        ModelAction::ToolCall {
            call_id: "one".to_string(),
            tool: "workspace_diff".to_string(),
            arguments: json!({}),
        },
        ModelAction::ToolCall {
            call_id: "two".to_string(),
            tool: "workspace_diff".to_string(),
            arguments: json!({}),
        },
    ]);
    let config = HarnessConfig {
        max_tool_calls: 1,
        ..HarnessConfig::default()
    };
    let mut harness = Harness::new(api, provider, MemoryAudit::default(), config);

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("second tool call exceeds budget");

    assert!(matches!(error, HarnessError::ToolBudget(1)));
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn lost_create_response_is_recovered_by_session_label() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().create_error_after_record = true;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        short_lifecycle_config(),
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("lost create response remains an API error");

    assert!(matches!(error, HarnessError::Api(_)));
    assert_eq!(observer.state().deleted, ["sb-0000000000000001"]);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::SandboxRecovered {
            sandbox_ids,
            deleted: true,
        } if sandbox_ids == &["sb-0000000000000001"]
    )));
}

#[tokio::test]
async fn create_wall_deadline_recovers_already_recorded_sandbox() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().create_delay = Duration::from_millis(100);
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_millis(250);
    config.session_timeout = Duration::from_millis(20);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("create response exceeds session wall deadline");

    assert!(matches!(error, HarnessError::Deadline));
    assert_eq!(observer.state().deleted, ["sb-0000000000000001"]);
    assert!(
        observer.state().delete_attempts > 1,
        "delete was retried while the daemon-owned create settled"
    );
}

#[tokio::test]
async fn create_deadline_without_observable_row_is_reported_uncertain() {
    let api = FakeApi::default();
    let observer = api.clone();
    {
        let mut state = observer.state();
        state.create_delay = Duration::from_millis(100);
        state.suppress_create_record = true;
    }
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_millis(20);
    config.lifecycle_timeout = Duration::from_millis(40);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("a timed-out create without a discoverable row is ambiguous");

    assert!(matches!(error, HarnessError::CreateUncertain(_)));
    assert!(observer.state().creates.is_empty());
    assert!(observer.state().deleted.is_empty());
}

#[tokio::test]
async fn transient_delete_failures_are_retried() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().delete_failures = 2;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelAction::FinalAnswer {
            text: "done".to_string(),
        }]),
        MemoryAudit::default(),
        short_lifecycle_config(),
    );

    harness
        .run(CancellationToken::new())
        .await
        .expect("delete eventually succeeds");

    assert_eq!(observer.state().deleted.len(), 1);
    assert_eq!(observer.state().delete_failures, 0);
}

#[tokio::test]
async fn permanent_delete_failure_is_bounded_and_reported() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().delete_failures = u32::MAX;
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_millis(25);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelAction::FinalAnswer {
            text: "done".to_string(),
        }]),
        MemoryAudit::default(),
        config,
    );
    let started = tokio::time::Instant::now();

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("cleanup cannot be confirmed");

    assert!(matches!(error, HarnessError::Cleanup(_)));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert!(observer.state().deleted.is_empty());
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            sandbox_deleted: false,
            ..
        })
    ));
}

#[tokio::test]
async fn stalled_exec_hits_wall_deadline_and_then_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().tool_exec_delay = Duration::from_secs(1);
    let provider = ScriptedProvider::new([ModelAction::ToolCall {
        call_id: "slow".to_string(),
        tool: "exec".to_string(),
        arguments: json!({"argv": ["/bin/slow"]}),
    }]);
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_millis(500);
    config.tool_limits.timeout = Duration::from_millis(50);
    let mut harness = Harness::new(api, provider, MemoryAudit::default(), config);
    let started = tokio::time::Instant::now();

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("stalled exec reaches its wall deadline");

    assert!(matches!(error, HarnessError::Deadline));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ToolCompleted {
            execution_ids,
            stderr,
            ..
        } if execution_ids == &["ex-0000000000000002"]
            && stderr.observed_bytes > 0
            && stderr.total_bytes_known
    )));
}

#[tokio::test]
async fn cancellation_during_exec_is_prompt_and_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().tool_exec_delay = Duration::from_secs(1);
    let provider = ScriptedProvider::new([ModelAction::ToolCall {
        call_id: "cancel-me".to_string(),
        tool: "exec".to_string(),
        arguments: json!({"argv": ["/bin/slow"]}),
    }]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        short_lifecycle_config(),
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        trigger.cancel();
    });
    let started = tokio::time::Instant::now();

    let error = harness
        .run(cancel)
        .await
        .expect_err("cancellation wins the tool wait");

    assert!(matches!(error, HarnessError::Canceled));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ToolCompleted {
            execution_ids,
            stderr,
            ..
        } if execution_ids == &["ex-0000000000000002"]
            && stderr.observed_bytes > 0
            && stderr.total_bytes_known
    )));
}

#[tokio::test]
async fn provider_deadline_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_millis(30);
    let mut harness = Harness::new(
        api,
        SlowProvider {
            delay: Duration::from_secs(1),
        },
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("provider exceeds session deadline");

    assert!(matches!(error, HarnessError::Deadline));
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn baseline_failure_is_audited_with_execution_id_and_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().fail_baseline = true;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        short_lifecycle_config(),
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("baseline find fails");

    assert!(matches!(error, HarnessError::ToolSetup(_)));
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::WorkspaceBaseline {
            ok: false,
            execution_ids,
            ..
        } if execution_ids == &["ex-0000000000000001"]
    )));
}

#[tokio::test]
async fn baseline_response_deadline_recovers_accepted_execution_and_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().baseline_exec_delay = Duration::from_secs(1);
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_millis(500);
    config.tool_limits.timeout = Duration::from_millis(20);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("baseline response exceeds its client wall deadline");

    assert!(matches!(error, HarnessError::ToolSetup(_)));
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::WorkspaceBaseline {
            ok: false,
            execution_ids,
            ..
        } if execution_ids == &["ex-0000000000000001"]
    )));
}

#[tokio::test]
async fn audit_failure_after_create_still_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        FailingAudit {
            calls: 0,
            fail_at: 2,
        },
        short_lifecycle_config(),
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("sandbox-created audit write fails");

    assert!(matches!(error, HarnessError::Audit(_)));
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn model_visible_output_truncation_is_explicit_in_result_and_audit() {
    let provider = ScriptedProvider::new([
        ModelAction::ToolCall {
            call_id: "big".to_string(),
            tool: "exec".to_string(),
            arguments: json!({"argv": ["/bin/big-output"]}),
        },
        ModelAction::FinalAnswer {
            text: "done".to_string(),
        },
    ]);
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        short_lifecycle_config(),
    );

    harness
        .run(CancellationToken::new())
        .await
        .expect("provider accepts bounded result");

    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ToolCompleted {
            stdout,
            ok: true,
            ..
        } if stdout.truncated
    )));
}

#[tokio::test]
async fn oversized_arguments_are_rejected_before_transcript_or_exec_retention() {
    let api = FakeApi::default();
    let observer = api.clone();
    let provider = ScriptedProvider::new([
        ModelAction::ToolCall {
            call_id: "large".to_string(),
            tool: "write_file".to_string(),
            arguments: json!({"path": "large.txt", "content": "x".repeat(70 << 10)}),
        },
        ModelAction::FinalAnswer {
            text: "handled".to_string(),
        },
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        short_lifecycle_config(),
    );

    harness
        .run(CancellationToken::new())
        .await
        .expect("provider can continue after bounded rejection");

    assert_eq!(observer.state().execs.len(), 1, "only baseline find ran");
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ModelTurn {
            action,
            arguments: None,
            ..
        } if action == "tool_call_rejected"
    )));
}

#[tokio::test]
async fn minimum_result_budget_contains_oversized_argument_rejection() {
    let provider = ScriptedProvider::new([
        ModelAction::ToolCall {
            call_id: "large".to_string(),
            tool: "write_file".to_string(),
            arguments: json!({"path": "large.txt", "content": "x".repeat(70 << 10)}),
        },
        ModelAction::FinalAnswer {
            text: "handled".to_string(),
        },
    ]);
    let mut config = short_lifecycle_config();
    config.tool_limits.max_result_bytes = 64;
    let mut harness = Harness::new(FakeApi::default(), provider, MemoryAudit::default(), config);

    harness
        .run(CancellationToken::new())
        .await
        .expect("the fixed rejection fits the accepted minimum");

    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ToolCompleted { result, .. } if result.bytes <= 64
    )));
}

#[tokio::test]
async fn oversized_final_answer_is_rejected_without_hashing_or_acceptance_event() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut config = short_lifecycle_config();
    config.max_final_answer_bytes = 8;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelAction::FinalAnswer {
            text: "x".repeat(1 << 20),
        }]),
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("oversized final answer is rejected");

    assert!(matches!(error, HarnessError::FinalAnswerTooLarge));
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ModelTurn { action, .. } if action == "final_answer_rejected"
    )));
    assert!(
        !harness
            .audit()
            .records
            .iter()
            .any(|record| matches!(&record.event, AuditEvent::FinalAnswer { .. }))
    );
}

#[tokio::test]
async fn tiny_result_limit_is_rejected_before_create() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut config = short_lifecycle_config();
    config.tool_limits.max_result_bytes = 1;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("tiny result budget is invalid");

    assert!(matches!(error, HarnessError::InvalidModelCall(_)));
    assert!(observer.state().creates.is_empty());
}

#[tokio::test]
async fn excessive_lifecycle_timeout_is_rejected_before_create() {
    let api = FakeApi::default();
    let observer = api.clone();
    let config = HarnessConfig {
        lifecycle_timeout: Duration::MAX,
        ..HarnessConfig::default()
    };
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        config,
    );

    let error = harness
        .run(CancellationToken::new())
        .await
        .expect_err("overflowing lifecycle timeout is invalid");

    assert!(matches!(error, HarnessError::InvalidModelCall(_)));
    assert!(observer.state().creates.is_empty());
}
