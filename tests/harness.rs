mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{DeleteBehavior, FakeApi, short_lifecycle_config};
use kiln_harness::api::ApiError;
use kiln_harness::audit::{AuditError, AuditEvent, AuditSink, MemoryAudit};
use kiln_harness::engine::{
    Harness, HarnessConfig, HarnessError, MAX_TASK_TEXT_BYTES, SessionTask,
};
use kiln_harness::journal::{
    JournalError, JournalEvent, JournalSink, JsonlJournal, MemoryJournal, read_journal,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, ProviderInfo, ScriptedProvider,
    StopReason, ToolCall, Usage,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Status};

fn task() -> SessionTask {
    SessionTask::demo()
}

struct SlowProvider {
    delay: Duration,
}

#[async_trait]
impl Provider for SlowProvider {
    fn info(&self) -> ProviderInfo {
        test_info()
    }

    async fn complete(
        &mut self,
        _request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(ModelResponse::final_text("late"))
    }
}

fn test_info() -> ProviderInfo {
    ProviderInfo {
        name: "test".to_string(),
        model: "test".to_string(),
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
        MemoryJournal::default(),
        HarnessConfig::default(),
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("demo succeeds");

    assert_eq!(outcome.steps, 5);
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
    // A names-only listing keeps replays of the same session identical.
    assert!(
        state
            .execs
            .iter()
            .any(|request| request.argv == ["/bin/ls", "-1Ap", "--", "/workspace"])
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
        ModelResponse::tool_use([ToolCall::new(
            "bad-path",
            "write_file",
            json!({"path": "../host", "content": "no"}),
        )]),
        ModelResponse::final_text("handled"),
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );

    harness
        .run(&task(), CancellationToken::new())
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

/// Captures each turn's tool results.
struct ResultsProvider {
    script: ScriptedProvider,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl Provider for ResultsProvider {
    fn info(&self) -> ProviderInfo {
        test_info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        if let Some(Message::ToolResults { results }) = request.messages.last() {
            let mut seen = self.seen.lock().expect("seen");
            seen.extend(results.iter().map(|result| result.result.clone()));
        }
        self.script.complete(request).await
    }
}

#[tokio::test]
async fn workspace_prefixed_paths_name_the_same_files() {
    let api = FakeApi::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ResultsProvider {
        script: ScriptedProvider::new([
            ModelResponse::tool_use([ToolCall::new(
                "w",
                "write_file",
                json!({"path": "/workspace/fib.sh", "content": "echo 1\n"}),
            )]),
            ModelResponse::tool_use([
                ToolCall::new("r", "read_file", json!({"path": "fib.sh"})),
                ToolCall::new("l", "list_dir", json!({"path": "/workspace"})),
                ToolCall::new("e", "read_file", json!({"path": "/etc/passwd"})),
                ToolCall::new("x", "read_file", json!({"path": "/workspacefoo"})),
            ]),
            ModelResponse::final_text("done"),
        ]),
        seen: seen.clone(),
    };
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");

    assert_eq!(
        api.state()
            .files
            .get("/workspace/fib.sh")
            .map(Vec::as_slice),
        Some(&b"echo 1\n"[..])
    );
    let seen = seen.lock().expect("seen");
    assert_eq!(seen[0]["ok"], true, "{:?}", seen[0]);
    assert_eq!(seen[1]["content"], "echo 1\n", "{:?}", seen[1]);
    assert_eq!(seen[2]["ok"], true, "{:?}", seen[2]);
    assert!(
        seen[2]["listing"]
            .as_str()
            .is_some_and(|listing| listing.contains("fib.sh")),
        "{:?}",
        seen[2]
    );
    for refused in &seen[3..] {
        assert_eq!(refused["error"], "invalid_path", "{refused:?}");
    }
}

#[tokio::test]
async fn provider_failure_still_deletes_the_sandbox() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("empty script fails");

    assert!(matches!(error, HarnessError::Provider(_)));
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn audit_excludes_model_arguments_and_file_contents() {
    let secret = "TOPSECRET-should-not-enter-audit";
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            secret,
            "write_file",
            json!({"path": "secret.txt", "content": secret}),
        )]),
        ModelResponse::final_text("done"),
    ]);
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );

    harness
        .run(&task(), CancellationToken::new())
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
        fn info(&self) -> ProviderInfo {
            test_info()
        }

        async fn complete(
            &mut self,
            request: ModelRequest<'_>,
        ) -> Result<ModelResponse, ProviderError> {
            if request.turn > 1 {
                assert!(matches!(
                    request.messages.last(),
                    Some(Message::ToolResults { results })
                        if results.len() == 1 && results[0].call_id == PROVIDER_ID
                ));
                assert!(matches!(
                    &request.messages[request.messages.len() - 2],
                    Message::Assistant { tool_calls, .. }
                        if tool_calls.len() == 1 && tool_calls[0].call_id == PROVIDER_ID
                ));
            }
            Ok(if request.turn <= 2 {
                ModelResponse::tool_use([ToolCall::new(PROVIDER_ID, "workspace_diff", json!({}))])
            } else {
                ModelResponse::final_text("done")
            })
        }
    }

    let mut harness = Harness::new(
        FakeApi::default(),
        ReusedIdProvider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    harness
        .run(&task(), CancellationToken::new())
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
    assert_eq!(model_ids, ["call-1-1", "call-2-1"]);
    assert_eq!(completed_ids, model_ids);
    let encoded = serde_json::to_string(&harness.audit().records).expect("serialize audit");
    assert!(!encoded.contains(PROVIDER_ID));
}

#[tokio::test]
async fn tool_call_budget_fails_closed_and_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new("one", "workspace_diff", json!({}))]),
        ModelResponse::tool_use([ToolCall::new("two", "workspace_diff", json!({}))]),
    ]);
    let config = HarnessConfig {
        max_tool_calls: 1,
        ..HarnessConfig::default()
    };
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
async fn every_tonic_status_is_classified_through_adapter_conversion() {
    for (code, unambiguous) in [
        (Code::Ok, false),
        (Code::Cancelled, false),
        (Code::Unknown, false),
        (Code::InvalidArgument, true),
        (Code::DeadlineExceeded, false),
        (Code::NotFound, false),
        (Code::AlreadyExists, false),
        (Code::PermissionDenied, true),
        (Code::ResourceExhausted, true),
        (Code::FailedPrecondition, true),
        (Code::Aborted, false),
        (Code::OutOfRange, false),
        (Code::Unimplemented, false),
        (Code::Internal, false),
        (Code::Unavailable, false),
        (Code::DataLoss, false),
        (Code::Unauthenticated, true),
    ] {
        let api = FakeApi::default();
        let observer = api.clone();
        // Use the exact Status -> ApiError conversion used by the tonic adapter.
        observer.state().create_rejection = Some(ApiError::from(Status::new(code, "rejected")));
        let mut config = short_lifecycle_config();
        config.lifecycle_timeout = Duration::from_millis(15);
        let mut harness = Harness::new(
            api,
            ScriptedProvider::demo(),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config,
        );

        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err("create was rejected");

        let actual_code = match error {
            HarnessError::Api(error) if unambiguous => error.code,
            HarnessError::CreateUncertain(error) if !unambiguous => error.code,
            other => panic!("incorrect classification for {code:?}: {other}"),
        };
        assert_eq!(actual_code, code);
        assert!(observer.state().creates.is_empty());
        assert!(
            observer.state().list_attempts > 0,
            "recovery listing is still required"
        );
        assert!(matches!(
            harness.audit().records.last().map(|record| &record.event),
            Some(AuditEvent::SessionFinished { sandbox_deleted, .. }) if *sandbox_deleted == unambiguous
        ));
        let journal = &harness.journal().records;
        assert!(matches!(
            journal.first().map(|record| &record.event),
            Some(JournalEvent::SessionStarted { .. })
        ));
        assert!(matches!(
            journal.last().map(|record| &record.event),
            Some(JournalEvent::SessionEnded { outcome, work_outcome, sandbox_deleted, steps: 0, .. })
                if work_outcome == "create_failed"
                    && *sandbox_deleted == unambiguous
                    && outcome == if unambiguous { "create_failed" } else { "cleanup_failed" }
        ));
        assert!(matches!(
            harness.audit().records.last().map(|record| &record.event),
            Some(AuditEvent::SessionFinished { outcome, work_outcome, .. })
                if work_outcome == "create_failed"
                    && outcome == if unambiguous { "create_failed" } else { "cleanup_failed" }
        ));
    }
}

/// The terminal records of a session whose sandbox absence was never
/// confirmed.
fn assert_unconfirmed_terminal(
    harness: &Harness<FakeApi, ScriptedProvider, MemoryAudit, MemoryJournal>,
    work: &str,
) {
    assert!(matches!(
        harness.journal().records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded { outcome, work_outcome, sandbox_deleted: false, .. })
            if outcome == "cleanup_failed" && work_outcome == work
    ));
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished { outcome, work_outcome, sandbox_deleted: false })
            if outcome == "cleanup_failed" && work_outcome == work
    ));
}

#[tokio::test]
async fn ambiguous_create_with_empty_listings_ends_cleanup_failed() {
    let api = FakeApi::default();
    api.state().create_rejection = Some(ApiError {
        code: Code::Unknown,
        message: "response is ambiguous".to_string(),
    });
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_millis(25);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("unused")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("create remains uncertain");

    assert!(matches!(error, HarnessError::CreateUncertain(_)), "{error}");
    assert_unconfirmed_terminal(&harness, "create_failed");
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
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("a timed-out create without a discoverable row is ambiguous");

    assert!(matches!(error, HarnessError::CreateUncertain(_)));
    assert!(observer.state().creates.is_empty());
    assert!(observer.state().deleted.is_empty());
    assert_unconfirmed_terminal(&harness, "create_failed");
}

#[tokio::test]
async fn transient_delete_failures_are_retried() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().delete_failures = 2;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
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
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = tokio::time::Instant::now();

    let error = harness
        .run(&task(), CancellationToken::new())
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
async fn successful_delete_response_must_still_verify_absence() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().delete_behavior = DeleteBehavior::AcknowledgeOnly;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let started = tokio::time::Instant::now();
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("cleanup is not confirmed while the sandbox remains listed");

    assert!(matches!(error, HarnessError::Cleanup(_)));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(observer.state().delete_attempts > 1);
    {
        let state = observer.state();
        assert_eq!(state.list_attempts, state.delete_attempts);
    }
    assert_eq!(observer.state().deleted.len(), 0);
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            sandbox_deleted: false,
            ..
        })
    ));
}

#[tokio::test]
async fn acknowledged_delete_is_retried_until_independent_absence() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().delete_acknowledgements_before_removal = 2;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("absence is eventually verified");

    let state = observer.state();
    assert_eq!(state.delete_attempts, 3);
    assert_eq!(state.list_attempts, 3);
    assert_eq!(state.deleted.len(), 1);
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            sandbox_deleted: true,
            ..
        })
    ));
}

#[tokio::test]
async fn delete_acknowledgement_requires_successful_bounded_verification() {
    for stalled in [false, true] {
        let api = FakeApi::default();
        let observer = api.clone();
        {
            let mut state = observer.state();
            if stalled {
                state.list_delay = Duration::from_secs(1);
            } else {
                state.list_failures = u32::MAX;
            }
        }
        let mut harness = Harness::new(
            api,
            ScriptedProvider::new([ModelResponse::final_text("done")]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            short_lifecycle_config(),
        );
        let started = tokio::time::Instant::now();

        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err("verification is required");

        assert!(matches!(error, HarnessError::Cleanup(_)));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(
            observer.state().deleted.len(),
            1,
            "deletion itself succeeded"
        );
        assert!(observer.state().list_attempts > 0);
        assert!(matches!(
            harness.audit().records.last().map(|record| &record.event),
            Some(AuditEvent::SessionFinished {
                sandbox_deleted: false,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn transient_verification_errors_are_retried() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().list_failures = 2;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("listing eventually confirms absence");

    assert_eq!(observer.state().list_attempts, 3);
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn lost_delete_response_is_confirmed_by_independent_absence() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().delete_behavior = DeleteBehavior::FailAfterRemoving;
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("absence confirms ambiguous deletion");

    assert_eq!(observer.state().list_attempts, 1);
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn stalled_exec_hits_wall_deadline_and_then_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().tool_exec_delay = Duration::from_secs(1);
    let provider = ScriptedProvider::new([ModelResponse::tool_use([ToolCall::new(
        "slow",
        "exec",
        json!({"argv": ["/bin/slow"]}),
    )])]);
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_millis(500);
    config.tool_limits.timeout = Duration::from_millis(50);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = tokio::time::Instant::now();

    let error = harness
        .run(&task(), CancellationToken::new())
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
    let provider = ScriptedProvider::new([ModelResponse::tool_use([ToolCall::new(
        "cancel-me",
        "exec",
        json!({"argv": ["/bin/slow"]}),
    )])]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
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
        .run(&task(), cancel)
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
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("sandbox-created audit write fails");

    assert!(matches!(error, HarnessError::Audit(_)));
    assert_eq!(observer.state().deleted.len(), 1);
}

#[tokio::test]
async fn model_visible_output_truncation_is_explicit_in_result_and_audit() {
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "big",
            "exec",
            json!({"argv": ["/bin/big-output"]}),
        )]),
        ModelResponse::final_text("done"),
    ]);
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
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
        ModelResponse::tool_use([ToolCall::new(
            "large",
            "write_file",
            json!({"path": "large.txt", "content": "x".repeat(70 << 10)}),
        )]),
        ModelResponse::final_text("handled"),
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    harness
        .run(&task(), CancellationToken::new())
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
        ModelResponse::tool_use([ToolCall::new(
            "large",
            "write_file",
            json!({"path": "large.txt", "content": "x".repeat(70 << 10)}),
        )]),
        ModelResponse::final_text("handled"),
    ]);
    let mut config = short_lifecycle_config();
    config.tool_limits.max_result_bytes = 64;
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    harness
        .run(&task(), CancellationToken::new())
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
        ScriptedProvider::new([ModelResponse::final_text("x".repeat(1 << 20))]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
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
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("overflowing lifecycle timeout is invalid");

    assert!(matches!(error, HarnessError::InvalidModelCall(_)));
    assert!(observer.state().creates.is_empty());
}

/// Records the messages each request carried, then defers to a script.
struct RecordingProvider {
    inner: ScriptedProvider,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl RecordingProvider {
    fn new(
        responses: impl IntoIterator<Item = ModelResponse>,
    ) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: ScriptedProvider::new(responses),
                seen: seen.clone(),
            },
            seen,
        )
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.seen
            .lock()
            .expect("seen lock")
            .push(request.messages.to_vec());
        self.inner.complete(request).await
    }
}

struct FailingJournal {
    calls: usize,
    fail_at: usize,
}

impl JournalSink for FailingJournal {
    fn append(&mut self, _session_id: &str, _event: JournalEvent) -> Result<(), JournalError> {
        self.calls += 1;
        if self.calls == self.fail_at {
            Err(JournalError::Io(std::io::Error::other(
                "injected journal failure",
            )))
        } else {
            Ok(())
        }
    }
}

fn audit_call_ids(audit: &MemoryAudit) -> (Vec<String>, Vec<String>) {
    let mut model = Vec::new();
    let mut completed = Vec::new();
    for record in &audit.records {
        match &record.event {
            AuditEvent::ModelTurn {
                audit_call_id: Some(id),
                ..
            } => model.push(id.clone()),
            AuditEvent::ToolCompleted { audit_call_id, .. } => {
                completed.push(audit_call_id.clone());
            }
            _ => {}
        }
    }
    (model, completed)
}

#[tokio::test]
async fn multi_call_turn_runs_in_order_and_routes_results_by_provider_id() {
    let api = FakeApi::default();
    let observer = api.clone();
    let (provider, seen) = RecordingProvider::new([
        ModelResponse::tool_use([
            ToolCall::new(
                "write-a",
                "write_file",
                json!({"path": "a.txt", "content": "abc"}),
            ),
            ToolCall::new("read-a", "read_file", json!({"path": "a.txt"})),
        ]),
        ModelResponse::final_text("done"),
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("multi-call turn succeeds");

    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.tool_calls, 2);
    let programs = observer
        .state()
        .execs
        .iter()
        .map(|exec| exec.argv[0].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        programs,
        ["/usr/bin/find", "/bin/dd", "/bin/stat", "/usr/bin/head"],
        "the write ran before the read"
    );
    assert_eq!(observer.state().deleted.len(), 1);

    let seen = seen.lock().expect("seen lock");
    assert_eq!(seen.len(), 2);
    let second = &seen[1];
    assert_eq!(second.len(), 3, "user, assistant, one tool-results message");
    assert!(matches!(&second[0], Message::User { text } if *text == task().prompt));
    assert!(matches!(
        &second[1],
        Message::Assistant { tool_calls, .. }
            if tool_calls.iter().map(|call| call.call_id.as_str()).eq(["write-a", "read-a"])
    ));
    let Message::ToolResults { results } = &second[2] else {
        panic!("expected one tool-results message, got {:?}", second[2]);
    };
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].call_id, "write-a");
    assert_eq!(results[0].tool, "write_file");
    assert_eq!(results[0].result["bytes_written"], 3);
    assert_eq!(results[1].call_id, "read-a");
    assert_eq!(results[1].tool, "read_file");
    assert_eq!(results[1].result["content"], "abc");
    drop(seen);

    let (model_ids, completed_ids) = audit_call_ids(harness.audit());
    assert_eq!(model_ids, ["call-1-1", "call-1-2"]);
    assert_eq!(completed_ids, model_ids);
}

#[tokio::test]
async fn per_turn_call_budget_fails_closed_before_any_exec() {
    let api = FakeApi::default();
    let observer = api.clone();
    let provider = ScriptedProvider::new([ModelResponse::tool_use([
        ToolCall::new("one", "workspace_diff", json!({})),
        ToolCall::new("two", "workspace_diff", json!({})),
        ToolCall::new("three", "workspace_diff", json!({})),
    ])]);
    let config = HarnessConfig {
        max_tool_calls_per_turn: 2,
        ..short_lifecycle_config()
    };
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("three calls exceed the per-turn budget");

    assert!(matches!(error, HarnessError::TurnToolBudget(2)));
    assert_eq!(observer.state().execs.len(), 1, "only the baseline ran");
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ModelTurn {
            turn: 1,
            action,
            audit_call_id: None,
            ..
        } if action == "tool_call_rejected"
    )));
    assert!(
        !harness
            .audit()
            .records
            .iter()
            .any(|record| matches!(&record.event, AuditEvent::ToolCompleted { .. }))
    );
}

#[tokio::test]
async fn invalid_or_repeated_call_ids_reject_the_whole_turn() {
    for calls in [
        vec![
            ToolCall::new("same", "workspace_diff", json!({})),
            ToolCall::new("same", "workspace_diff", json!({})),
        ],
        vec![
            ToolCall::new("fine", "workspace_diff", json!({})),
            ToolCall::new("not valid!", "workspace_diff", json!({})),
        ],
    ] {
        let api = FakeApi::default();
        let observer = api.clone();
        let mut harness = Harness::new(
            api,
            ScriptedProvider::new([ModelResponse::tool_use(calls)]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            short_lifecycle_config(),
        );

        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err("call IDs are a provider protocol contract");

        assert!(matches!(error, HarnessError::InvalidModelCall(_)));
        assert_eq!(observer.state().execs.len(), 1, "no call in the turn ran");
        assert_eq!(observer.state().deleted.len(), 1);
    }
}

#[tokio::test]
async fn misnamed_tool_is_a_model_visible_result_and_the_loop_continues() {
    const BAD_NAME: &str = "Bad Name! rm -rf";
    let api = FakeApi::default();
    let observer = api.clone();
    let (provider, seen) = RecordingProvider::new([
        ModelResponse::tool_use([ToolCall::new("oops", BAD_NAME, json!({"x": 1}))]),
        ModelResponse::final_text("recovered"),
    ]);
    let config = HarnessConfig {
        max_tool_calls: 1,
        ..short_lifecycle_config()
    };
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("the model can recover from a misnamed tool");

    assert_eq!(outcome.final_answer, "recovered");
    assert_eq!(outcome.tool_calls, 1, "the call counts against the budget");
    assert_eq!(observer.state().execs.len(), 1, "only the baseline ran");
    assert_eq!(observer.state().deleted.len(), 1);
    let seen = seen.lock().expect("seen lock");
    assert!(matches!(
        &seen[1][1],
        Message::Assistant { tool_calls, .. } if tool_calls[0].tool == "unknown"
    ));
    assert!(matches!(
        &seen[1][2],
        Message::ToolResults { results }
            if results.len() == 1
                && results[0].call_id == "oops"
                && results[0].tool == "unknown"
                && results[0].result == json!({"ok": false, "error": "unknown_tool"})
    ));
    drop(seen);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ToolCompleted { tool, ok: false, execution_ids, .. }
            if tool == "unknown" && execution_ids.is_empty()
    )));
    let encoded = serde_json::to_string(&harness.audit().records).expect("serialize audit");
    assert!(!encoded.contains(BAD_NAME));
}

#[tokio::test]
async fn truncated_refused_and_empty_responses_fail_closed_and_clean_up() {
    type Expected = fn(&HarnessError) -> bool;
    let write_call = || ToolCall::new("w", "write_file", json!({"path": "a.txt", "content": "a"}));
    let cases: Vec<(&str, ModelResponse, Expected)> = vec![
        (
            "max tokens with tool calls",
            ModelResponse {
                stop_reason: StopReason::MaxTokens,
                ..ModelResponse::tool_use([write_call()])
            },
            |error| matches!(error, HarnessError::ModelTruncated),
        ),
        (
            "max tokens without tool calls",
            ModelResponse {
                stop_reason: StopReason::MaxTokens,
                ..ModelResponse::final_text("half an ans")
            },
            |error| matches!(error, HarnessError::ModelTruncated),
        ),
        (
            "refusal",
            ModelResponse {
                stop_reason: StopReason::Refusal,
                ..ModelResponse::final_text("no")
            },
            |error| matches!(error, HarnessError::ModelRefused),
        ),
        (
            "refusal with tool calls",
            ModelResponse {
                stop_reason: StopReason::Refusal,
                ..ModelResponse::tool_use([write_call()])
            },
            |error| matches!(error, HarnessError::ModelRefused),
        ),
        ("empty final text", ModelResponse::final_text(""), |error| {
            matches!(error, HarnessError::InvalidModelCall(_))
        }),
        (
            "missing final text",
            ModelResponse {
                text: None,
                ..ModelResponse::final_text("")
            },
            |error| matches!(error, HarnessError::InvalidModelCall(_)),
        ),
    ];
    for (name, response, expected) in cases {
        let api = FakeApi::default();
        let observer = api.clone();
        let mut harness = Harness::new(
            api,
            ScriptedProvider::new([response]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            short_lifecycle_config(),
        );

        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err(name);

        assert!(expected(&error), "{name}: unexpected {error}");
        assert_eq!(observer.state().execs.len(), 1, "{name}: no tool ran");
        assert_eq!(observer.state().deleted.len(), 1, "{name}: cleanup");
        assert!(matches!(
            harness.journal().records.last().map(|record| &record.event),
            Some(JournalEvent::SessionEnded {
                outcome,
                sandbox_deleted: true,
                final_answer: None,
                ..
            }) if outcome == "failed"
        ));
    }
}

#[tokio::test]
async fn cumulative_and_per_request_token_budgets_fail_closed() {
    let usage = |input_tokens, output_tokens| Usage {
        input_tokens,
        output_tokens,
        ..Usage::default()
    };
    let with_usage = |response: ModelResponse, usage: Usage| ModelResponse { usage, ..response };
    let diff = || ModelResponse::tool_use([ToolCall::new("d", "workspace_diff", json!({}))]);
    let cases = [
        // Cumulative input: 10 + 10 > 15.
        (
            vec![
                with_usage(diff(), usage(10, 1)),
                with_usage(ModelResponse::final_text("done"), usage(10, 1)),
            ],
            2,
        ),
        // Cumulative output: 6 + 6 > 10.
        (
            vec![
                with_usage(diff(), usage(1, 6)),
                with_usage(ModelResponse::final_text("done"), usage(1, 6)),
            ],
            2,
        ),
        // One response above the per-request output cap of 8.
        (vec![with_usage(diff(), usage(1, 9))], 1),
    ];
    for (responses, steps) in cases {
        let api = FakeApi::default();
        let observer = api.clone();
        let config = HarnessConfig {
            max_total_input_tokens: 15,
            max_total_output_tokens: 10,
            max_output_tokens_per_request: 8,
            ..short_lifecycle_config()
        };
        let mut harness = Harness::new(
            api,
            ScriptedProvider::new(responses),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config,
        );

        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err("usage exceeds a token budget");

        assert!(matches!(error, HarnessError::TokenBudget), "{error}");
        assert_eq!(observer.state().deleted.len(), 1);
        assert!(matches!(
            harness.journal().records.last().map(|record| &record.event),
            Some(JournalEvent::SessionEnded { steps: recorded, sandbox_deleted: true, .. })
                if *recorded == steps
        ));
        assert!(harness.audit().records.iter().any(|record| matches!(
            &record.event,
            AuditEvent::ModelResponded { turn: 1, usage, .. } if usage.input_tokens > 0
        )));
    }
}

#[tokio::test]
async fn usage_is_summed_into_the_outcome() {
    let response = |text: &str, input_tokens| ModelResponse {
        usage: Usage {
            input_tokens,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
        },
        ..ModelResponse::final_text(text)
    };
    let first = ModelResponse {
        tool_calls: vec![ToolCall::new("d", "workspace_diff", json!({}))],
        stop_reason: StopReason::ToolUse,
        ..response("thinking", 100)
    };
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::new([first, response("done", 200)]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session succeeds");

    assert_eq!(
        outcome.usage,
        Usage {
            input_tokens: 300,
            output_tokens: 4,
            cache_read_input_tokens: 6,
            cache_creation_input_tokens: 8,
        }
    );
    assert_eq!(
        Usage {
            input_tokens: u64::MAX,
            ..Usage::default()
        } + Usage {
            input_tokens: 1,
            ..Usage::default()
        },
        Usage {
            input_tokens: u64::MAX,
            ..Usage::default()
        },
        "usage addition saturates"
    );
}

#[tokio::test]
async fn journal_failure_after_create_still_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        FailingJournal {
            calls: 0,
            // 1: session_started, 2: sandbox_ready.
            fail_at: 2,
        },
        short_lifecycle_config(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("sandbox-ready journal write fails");

    assert!(matches!(error, HarnessError::Journal(_)));
    assert_eq!(observer.state().creates.len(), 1);
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            sandbox_deleted: true,
            ..
        })
    ));
}

#[tokio::test]
async fn journal_failure_before_create_never_creates_a_sandbox() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        FailingJournal {
            calls: 0,
            fail_at: 1,
        },
        short_lifecycle_config(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("session-started journal write fails");

    assert!(matches!(error, HarnessError::Journal(_)));
    assert!(observer.state().creates.is_empty());
}

#[tokio::test]
async fn invalid_tasks_are_rejected_before_create() {
    let oversized = "x".repeat(MAX_TASK_TEXT_BYTES + 1);
    for task in [
        SessionTask {
            system: String::new(),
            prompt: String::new(),
        },
        SessionTask {
            system: oversized.clone(),
            prompt: "ok".to_string(),
        },
        SessionTask {
            system: String::new(),
            prompt: oversized.clone(),
        },
    ] {
        let api = FakeApi::default();
        let observer = api.clone();
        let mut harness = Harness::new(
            api,
            ScriptedProvider::demo(),
            MemoryAudit::default(),
            MemoryJournal::default(),
            short_lifecycle_config(),
        );

        let error = harness
            .run(&task, CancellationToken::new())
            .await
            .expect_err("task is invalid");

        assert!(matches!(error, HarnessError::InvalidTask(_)));
        assert!(observer.state().creates.is_empty());
        assert!(harness.audit().records.is_empty());
        assert!(harness.journal().records.is_empty());
    }
}

#[tokio::test]
async fn zero_turn_and_token_budgets_are_rejected_before_create() {
    let configs = [
        HarnessConfig {
            max_tool_calls_per_turn: 0,
            ..HarnessConfig::default()
        },
        HarnessConfig {
            max_output_tokens_per_request: 0,
            ..HarnessConfig::default()
        },
        HarnessConfig {
            max_total_input_tokens: 0,
            ..HarnessConfig::default()
        },
        HarnessConfig {
            max_total_output_tokens: 0,
            ..HarnessConfig::default()
        },
    ];
    for config in configs {
        let api = FakeApi::default();
        let observer = api.clone();
        let mut harness = Harness::new(
            api,
            ScriptedProvider::demo(),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config,
        );

        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err("zero budget is invalid");

        assert!(matches!(error, HarnessError::InvalidModelCall(_)));
        assert!(observer.state().creates.is_empty());
    }
}

#[tokio::test]
async fn cached_input_tokens_count_against_the_input_budget() {
    let usage = |input_tokens, cache_read_input_tokens, cache_creation_input_tokens| Usage {
        input_tokens,
        output_tokens: 1,
        cache_read_input_tokens,
        cache_creation_input_tokens,
    };
    // (usage, budget, accepted)
    for (usage, budget, accepted) in [
        (usage(1, 50, 50), 1, false),
        (usage(1, 50, 50), 100, false),
        (usage(1, 50, 50), 101, true),
        // No double counting when the cache counters are zero.
        (usage(10, 0, 0), 10, true),
        (usage(10, 0, 0), 9, false),
    ] {
        let api = FakeApi::default();
        let observer = api.clone();
        let config = HarnessConfig {
            max_total_input_tokens: budget,
            ..short_lifecycle_config()
        };
        let mut harness = Harness::new(
            api,
            ScriptedProvider::new([ModelResponse {
                usage,
                ..ModelResponse::final_text("done")
            }]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config,
        );

        let result = harness.run(&task(), CancellationToken::new()).await;

        if accepted {
            assert_eq!(result.expect("within budget").usage, usage);
        } else {
            assert!(
                matches!(result, Err(HarnessError::TokenBudget)),
                "{usage:?} with budget {budget}"
            );
        }
        assert_eq!(observer.state().deleted.len(), 1);
    }
    assert_eq!(usage(u64::MAX, 1, 1).total_input_tokens(), u64::MAX);
}

/// Run `harness` and cancel once `ready` holds; return the result and how
/// long the run took after cancellation.
async fn run_and_cancel_when<P: Provider>(
    harness: &mut Harness<FakeApi, P, MemoryAudit, MemoryJournal>,
    ready: impl Fn() -> bool,
) -> (
    Result<kiln_harness::engine::SessionOutcome, HarnessError>,
    Duration,
) {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let task = task();
    let cancel_when_ready = async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let canceled_at = tokio::time::Instant::now();
        trigger.cancel();
        canceled_at
    };
    let (result, canceled_at) = tokio::join!(harness.run(&task, cancel), cancel_when_ready);
    (result, canceled_at.elapsed())
}

#[tokio::test]
async fn cancellation_interrupts_an_active_create_and_recovers_its_sandbox() {
    let api = FakeApi::default();
    let observer = api.clone();
    {
        let mut state = observer.state();
        state.create_delay = Duration::from_secs(5);
        state.create_allows_early_delete = true;
    }
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("unused")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let (result, after_cancel) =
        run_and_cancel_when(&mut harness, || !observer.state().creates.is_empty()).await;

    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    assert!(
        after_cancel < Duration::from_millis(100),
        "{after_cancel:?}"
    );
    assert_eq!(observer.state().deleted, ["sb-0000000000000001"]);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::SandboxRecovered { sandbox_ids, deleted: true } if sandbox_ids.len() == 1
    )));
    assert!(matches!(
        harness.journal().records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded { outcome, sandbox_deleted: true, .. })
            if outcome == "canceled"
    ));
}

#[tokio::test]
async fn cancellation_during_create_recovers_a_row_recorded_late() {
    let api = FakeApi::default();
    let observer = api.clone();
    {
        let mut state = observer.state();
        state.create_delay = Duration::from_secs(5);
        state.create_record_delay = Duration::from_millis(60);
    }
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_millis(500);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("unused")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = tokio::time::Instant::now();

    let (result, _) = run_and_cancel_when(&mut harness, || {
        started.elapsed() >= Duration::from_millis(10)
    })
    .await;

    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_millis(400));
    assert_eq!(observer.state().creates.len(), 1, "the row appeared late");
    assert_eq!(observer.state().deleted, ["sb-0000000000000001"]);
}

#[tokio::test]
async fn cancellation_during_create_without_a_row_is_uncertain() {
    let api = FakeApi::default();
    let observer = api.clone();
    {
        let mut state = observer.state();
        state.create_delay = Duration::from_secs(5);
        state.suppress_create_record = true;
    }
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_millis(40);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("unused")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = tokio::time::Instant::now();

    let (result, _) = run_and_cancel_when(&mut harness, || {
        started.elapsed() >= Duration::from_millis(10)
    })
    .await;

    assert!(
        matches!(result, Err(HarnessError::CreateUncertain(_))),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_unconfirmed_terminal(&harness, "canceled");
}

#[tokio::test]
async fn cancellation_interrupts_an_active_baseline_and_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().baseline_exec_delay = Duration::from_secs(5);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("unused")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let (result, after_cancel) =
        run_and_cancel_when(&mut harness, || !observer.state().execs.is_empty()).await;

    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    assert!(
        after_cancel < Duration::from_millis(100),
        "{after_cancel:?}"
    );
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::WorkspaceBaseline { ok: false, execution_ids, .. }
            if execution_ids == &["ex-0000000000000001"]
    )));
    assert!(matches!(
        harness.journal().records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded { outcome, sandbox_deleted: true, .. })
            if outcome == "canceled"
    ));
}

/// Run one session into a file journal and require that the reader accepts
/// it as complete, whatever the session's outcome.
async fn assert_engine_journal_is_grammatical<P: Provider>(
    name: &str,
    api: FakeApi,
    provider: P,
    config: HarnessConfig,
    cancel_after: Option<Duration>,
) {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("session.jsonl");
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        config,
    );
    let cancel = CancellationToken::new();
    if let Some(after) = cancel_after {
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            trigger.cancel();
        });
    }
    let _ = harness.run(&task(), cancel).await;
    let journal = read_journal(&path).unwrap_or_else(|error| panic!("{name}: {error}"));
    assert!(journal.complete, "{name}: journal is incomplete");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn every_engine_exit_path_writes_a_grammatical_journal() {
    let diff = |id: &str| ToolCall::new(id, "workspace_diff", json!({}));
    assert_engine_journal_is_grammatical(
        "demo",
        FakeApi::default(),
        ScriptedProvider::demo(),
        short_lifecycle_config(),
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "provider failure",
        FakeApi::default(),
        ScriptedProvider::new([]),
        short_lifecycle_config(),
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "session call budget mid-turn",
        FakeApi::default(),
        ScriptedProvider::new([ModelResponse::tool_use([diff("a"), diff("b")])]),
        HarnessConfig {
            max_tool_calls: 1,
            ..short_lifecycle_config()
        },
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "per-turn budget",
        FakeApi::default(),
        ScriptedProvider::new([ModelResponse::tool_use([diff("a"), diff("b")])]),
        HarnessConfig {
            max_tool_calls_per_turn: 1,
            ..short_lifecycle_config()
        },
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "step budget",
        FakeApi::default(),
        ScriptedProvider::new([
            ModelResponse::tool_use([diff("a")]),
            ModelResponse::tool_use([diff("b")]),
        ]),
        HarnessConfig {
            max_steps: 2,
            ..short_lifecycle_config()
        },
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "token budget",
        FakeApi::default(),
        ScriptedProvider::new([ModelResponse {
            usage: Usage {
                input_tokens: 10,
                ..Usage::default()
            },
            ..ModelResponse::tool_use([diff("a")])
        }]),
        HarnessConfig {
            max_total_input_tokens: 5,
            ..short_lifecycle_config()
        },
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "provider deadline",
        FakeApi::default(),
        SlowProvider {
            delay: Duration::from_secs(1),
        },
        HarnessConfig {
            session_timeout: Duration::from_millis(30),
            ..short_lifecycle_config()
        },
        None,
    )
    .await;
    let slow_exec = FakeApi::default();
    slow_exec.state().tool_exec_delay = Duration::from_secs(1);
    assert_engine_journal_is_grammatical(
        "cancel during exec",
        slow_exec,
        ScriptedProvider::new([ModelResponse::tool_use([ToolCall::new(
            "slow",
            "exec",
            json!({"argv": ["/bin/slow"]}),
        )])]),
        short_lifecycle_config(),
        Some(Duration::from_millis(20)),
    )
    .await;
    let stuck = FakeApi::default();
    stuck.state().delete_behavior = DeleteBehavior::AcknowledgeOnly;
    assert_engine_journal_is_grammatical(
        "cleanup failure",
        stuck,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        HarnessConfig {
            lifecycle_timeout: Duration::from_millis(25),
            ..short_lifecycle_config()
        },
        None,
    )
    .await;
    let rejected = FakeApi::default();
    rejected.state().create_rejection = Some(ApiError::from(Status::new(
        Code::InvalidArgument,
        "rejected",
    )));
    assert_engine_journal_is_grammatical(
        "create rejection",
        rejected,
        ScriptedProvider::demo(),
        short_lifecycle_config(),
        None,
    )
    .await;
    let failing_baseline = FakeApi::default();
    failing_baseline.state().fail_baseline = true;
    assert_engine_journal_is_grammatical(
        "baseline failure",
        failing_baseline,
        ScriptedProvider::demo(),
        short_lifecycle_config(),
        None,
    )
    .await;
    assert_engine_journal_is_grammatical(
        "truncated tool turn",
        FakeApi::default(),
        ScriptedProvider::new([ModelResponse {
            stop_reason: StopReason::MaxTokens,
            ..ModelResponse::tool_use([diff("a")])
        }]),
        short_lifecycle_config(),
        None,
    )
    .await;
}
