//! Response-loss recovery polls with backoff: one unsettled execution must
//! not turn into hundreds of RPCs.
mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use kiln_harness::api::{
    ApiError, CapturedOutput, Execution, ExecutionState, Health, Sandbox, SandboxApi, SandboxExec,
    SandboxRequest,
};
use kiln_harness::tools::recover_execution_evidence;
use tokio_util::sync::CancellationToken;
use tonic::Code;

/// `GetSandbox` shows one RUNNING row; `GetExecution` never finds it.
#[derive(Clone, Default)]
struct MissingExecutionApi {
    sandbox_reads: Arc<AtomicU32>,
    execution_reads: Arc<AtomicU32>,
}

fn running() -> Execution {
    Execution {
        id: "ex-0000000000000001".to_string(),
        state: ExecutionState::Unknown,
        exit_code: None,
        failure_reason: String::new(),
        stdout: CapturedOutput::default(),
        stderr: CapturedOutput::default(),
        sandbox_reusable: None,
        cancel_reason: None,
    }
}

#[async_trait]
impl SandboxApi for MissingExecutionApi {
    async fn health(&self) -> Result<Health, ApiError> {
        unreachable!("recovery reads no health")
    }

    async fn create(&self, _request: SandboxRequest) -> Result<Sandbox, ApiError> {
        unreachable!("recovery creates nothing")
    }

    async fn exec(&self, _request: SandboxExec) -> Result<Execution, ApiError> {
        unreachable!("recovery runs nothing")
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        self.sandbox_reads.fetch_add(1, Ordering::SeqCst);
        Ok(vec![running()])
    }

    async fn delete(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery deletes nothing")
    }

    async fn renew(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery renews nothing")
    }

    async fn get_execution(&self, _execution_id: &str) -> Result<Execution, ApiError> {
        self.execution_reads.fetch_add(1, Ordering::SeqCst);
        Err(ApiError::new(Code::NotFound, "execution not found"))
    }

    async fn sandbox_ready(&self, _sandbox_id: &str) -> Result<bool, ApiError> {
        unreachable!("recovery reads no sandbox state")
    }

    async fn final_check(
        &self,
        _sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, ApiError> {
        unreachable!("recovery runs no final check")
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        unreachable!("recovery lists nothing")
    }
}

#[tokio::test(start_paused = true)]
async fn an_execution_that_never_settles_is_polled_with_backoff() {
    // R4-02: a one-second window used to send 101 reads of each RPC.
    let api = MissingExecutionApi::default();
    let started = tokio::time::Instant::now();
    let recovered = recover_execution_evidence(
        &api,
        "sb-0000000000000001",
        &BTreeSet::new(),
        started + Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await;
    let sandbox_reads = api.sandbox_reads.load(Ordering::SeqCst);
    let execution_reads = api.execution_reads.load(Ordering::SeqCst);
    // Still fails closed, within the window.
    assert_eq!(recovered.unsettled.as_deref(), Some("ex-0000000000000001"));
    assert!(started.elapsed() <= Duration::from_secs(1));
    assert!(
        (5..=15).contains(&execution_reads),
        "GetExecution reads: {execution_reads}"
    );
    assert!(
        (2..=6).contains(&sandbox_reads),
        "GetSandbox reads: {sandbox_reads}"
    );
}

/// `GetSandbox` always fails, or always answers empty.
#[derive(Clone, Default)]
struct UnreadableApi {
    empty: bool,
}

#[async_trait]
impl SandboxApi for UnreadableApi {
    async fn health(&self) -> Result<Health, ApiError> {
        unreachable!("recovery reads no health")
    }

    async fn create(&self, _request: SandboxRequest) -> Result<Sandbox, ApiError> {
        unreachable!("recovery creates nothing")
    }

    async fn exec(&self, _request: SandboxExec) -> Result<Execution, ApiError> {
        unreachable!("recovery runs nothing")
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        if self.empty {
            Ok(Vec::new())
        } else {
            Err(ApiError::new(Code::Unavailable, "daemon unreachable"))
        }
    }

    async fn delete(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery deletes nothing")
    }

    async fn renew(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery renews nothing")
    }

    async fn get_execution(&self, _execution_id: &str) -> Result<Execution, ApiError> {
        unreachable!("no row to follow")
    }

    async fn sandbox_ready(&self, _sandbox_id: &str) -> Result<bool, ApiError> {
        unreachable!("recovery reads no sandbox state")
    }

    async fn final_check(
        &self,
        _sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, ApiError> {
        unreachable!("recovery runs no final check")
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        unreachable!("recovery lists nothing")
    }
}

#[tokio::test(start_paused = true)]
async fn recovery_that_cannot_read_anything_fails_closed() {
    use kiln_harness::tools::ToolStop;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let recovered = recover_execution_evidence(
        &UnreadableApi { empty: false },
        "sb-0000000000000001",
        &BTreeSet::new(),
        deadline,
        &CancellationToken::new(),
    )
    .await;
    assert!(recovered.unreadable);
    assert_eq!(
        recovered.stop(),
        Some(ToolStop::Unusable(
            "execution evidence could not be read after its response was lost".to_string()
        ))
    );

    // A successful empty read is no evidence either way: as before, the
    // call stays an ordinary failure.
    let recovered = recover_execution_evidence(
        &UnreadableApi { empty: true },
        "sb-0000000000000001",
        &BTreeSet::new(),
        tokio::time::Instant::now() + Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await;
    assert!(!recovered.unreadable);
    assert_eq!(recovered.stop(), None);
}

/// A row that becomes observable late in the recovery window, terminal and
/// leaving the sandbox unusable.
#[derive(Clone)]
struct LateTerminalRow {
    publish_at: tokio::time::Instant,
}

impl LateTerminalRow {
    fn row() -> Execution {
        Execution {
            id: "ex-0000000000000002".to_string(),
            state: ExecutionState::Canceled,
            exit_code: None,
            failure_reason: "late cancellation retired the sandbox".to_string(),
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput::default(),
            sandbox_reusable: Some(false),
            cancel_reason: Some(kiln_harness::api::CancelReason::DaemonShutdown),
        }
    }
}

#[async_trait]
impl SandboxApi for LateTerminalRow {
    async fn health(&self) -> Result<Health, ApiError> {
        unreachable!("recovery reads no health")
    }

    async fn create(&self, _request: SandboxRequest) -> Result<Sandbox, ApiError> {
        unreachable!("recovery creates nothing")
    }

    async fn exec(&self, _request: SandboxExec) -> Result<Execution, ApiError> {
        unreachable!("recovery runs nothing")
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        Ok(if tokio::time::Instant::now() >= self.publish_at {
            vec![Self::row()]
        } else {
            Vec::new()
        })
    }

    async fn delete(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery deletes nothing")
    }

    async fn renew(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery renews nothing")
    }

    async fn get_execution(&self, _execution_id: &str) -> Result<Execution, ApiError> {
        Ok(Self::row())
    }

    async fn sandbox_ready(&self, _sandbox_id: &str) -> Result<bool, ApiError> {
        unreachable!("recovery reads no sandbox state")
    }

    async fn final_check(
        &self,
        _sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, ApiError> {
        unreachable!("recovery runs no final check")
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        unreachable!("recovery lists nothing")
    }
}

#[tokio::test(start_paused = true)]
async fn a_row_published_late_in_the_window_is_still_found() {
    // R5-03: a row that becomes observable late in the window, up to the
    // start of the last pass (100 ms before a 1 s deadline), is found. One
    // published inside that reserve is the final check's to catch (see
    // a_row_recovery_missed_still_stops_a_completed_session).
    for before in [150, 101] {
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(1);
        let api = LateTerminalRow {
            publish_at: deadline - Duration::from_millis(before),
        };
        let recovered = recover_execution_evidence(
            &api,
            "sb-0000000000000001",
            &BTreeSet::new(),
            deadline,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            recovered.execution_ids,
            ["ex-0000000000000002"],
            "{before} ms"
        );
        assert_eq!(recovered.sandbox_reusable, Some(false));
    }
}

/// `GetSandbox` answers empty until `hang_from`, then never answers.
#[derive(Clone)]
struct HangsAtTheEnd {
    hang_from: tokio::time::Instant,
}

#[async_trait]
impl SandboxApi for HangsAtTheEnd {
    async fn health(&self) -> Result<Health, ApiError> {
        unreachable!("recovery reads no health")
    }

    async fn create(&self, _request: SandboxRequest) -> Result<Sandbox, ApiError> {
        unreachable!("recovery creates nothing")
    }

    async fn exec(&self, _request: SandboxExec) -> Result<Execution, ApiError> {
        unreachable!("recovery runs nothing")
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        if tokio::time::Instant::now() >= self.hang_from {
            std::future::pending::<()>().await;
        }
        Ok(Vec::new())
    }

    async fn delete(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery deletes nothing")
    }

    async fn renew(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("recovery renews nothing")
    }

    async fn get_execution(&self, _execution_id: &str) -> Result<Execution, ApiError> {
        unreachable!("no row to follow")
    }

    async fn sandbox_ready(&self, _sandbox_id: &str) -> Result<bool, ApiError> {
        unreachable!("recovery reads no sandbox state")
    }

    async fn final_check(
        &self,
        _sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, ApiError> {
        unreachable!("recovery runs no final check")
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        unreachable!("recovery lists nothing")
    }
}

#[tokio::test(start_paused = true)]
async fn a_last_read_that_fails_after_empty_ones_is_unreadable() {
    // R6-01: the last pass starts 100 ms before the deadline and its read
    // never answers. Earlier empty reads do not stand in for it.
    let start = tokio::time::Instant::now();
    let deadline = start + Duration::from_secs(1);
    let api = HangsAtTheEnd {
        hang_from: deadline - Duration::from_millis(100),
    };
    let recovered = recover_execution_evidence(
        &api,
        "sb-0000000000000001",
        &BTreeSet::new(),
        deadline,
        &CancellationToken::new(),
    )
    .await;
    assert!(recovered.unreadable);
    assert!(recovered.stop().is_some());
    assert!(tokio::time::Instant::now() <= deadline);
}

/// A kilnd whose `GetSandbox` reads take 1 ms, and whose lost Exec
/// publishes a terminal, non-reusable row 950 ms after it was sent: inside
/// recovery's final reserve, after its last read.
#[derive(Clone)]
struct LatePublishedRow {
    inner: common::FakeApi,
    publish_at: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
}

impl LatePublishedRow {
    fn row(&self) -> Option<Execution> {
        let published = self
            .publish_at
            .lock()
            .expect("publish time")
            .is_some_and(|at| tokio::time::Instant::now() >= at);
        published.then(LateTerminalRow::row)
    }
}

#[async_trait]
impl SandboxApi for LatePublishedRow {
    async fn health(&self) -> Result<Health, ApiError> {
        self.inner.health().await
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        self.inner.create(request).await
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        if request.argv.first().map(String::as_str) == Some("/usr/bin/find") {
            return self.inner.exec(request).await;
        }
        *self.publish_at.lock().expect("publish time") =
            Some(tokio::time::Instant::now() + Duration::from_millis(950));
        Err(ApiError::new(Code::Unavailable, "response lost"))
    }

    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        tokio::time::sleep(Duration::from_millis(1)).await;
        let mut rows = self.inner.recent_executions(sandbox_id).await?;
        rows.extend(self.row());
        Ok(rows)
    }

    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError> {
        self.inner.delete(sandbox_id).await
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), ApiError> {
        self.inner.renew(sandbox_id).await
    }

    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError> {
        tokio::time::sleep(Duration::from_millis(1)).await;
        match self.row() {
            Some(row) if row.id == execution_id => Ok(row),
            _ => self.inner.get_execution(execution_id).await,
        }
    }

    async fn sandbox_ready(&self, sandbox_id: &str) -> Result<bool, ApiError> {
        self.inner.sandbox_ready(sandbox_id).await
    }

    async fn final_check(
        &self,
        sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, ApiError> {
        tokio::time::sleep(Duration::from_millis(1)).await;
        let mut check = self.inner.final_check(sandbox_id).await?;
        check.recent.extend(self.row());
        Ok(check)
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        self.inner.non_deleted_sandboxes().await
    }
}

/// Asks for one Exec, then answers 100 ms later.
struct ExecThenSlowAnswer {
    calls: u32,
}

#[async_trait]
impl kiln_harness::provider::Provider for ExecThenSlowAnswer {
    fn info(&self) -> kiln_harness::provider::ProviderInfo {
        kiln_harness::provider::ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        }
    }

    async fn complete(
        &mut self,
        _request: kiln_harness::provider::ModelRequest<'_>,
    ) -> Result<kiln_harness::provider::ModelResponse, kiln_harness::provider::ProviderError> {
        use kiln_harness::provider::{ModelResponse, ToolCall};
        self.calls += 1;
        if self.calls == 1 {
            return Ok(ModelResponse::tool_use([ToolCall::new(
                "call-1",
                "exec",
                serde_json::json!({"argv": ["/bin/true"]}),
            )]));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(ModelResponse::final_text(
            "done after recovery missed the row",
        ))
    }
}

#[tokio::test(start_paused = true)]
async fn a_row_recovery_missed_still_stops_a_completed_session() {
    // R6-01: recovery's last read, 100 ms before its deadline, cannot see a
    // row published 50 ms before it. The final check before completion does.
    use kiln_harness::audit::{AuditEvent, MemoryAudit};
    use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionTask};
    use kiln_harness::journal::MemoryJournal;
    let api = LatePublishedRow {
        inner: common::FakeApi::with_expiry(),
        publish_at: Arc::new(std::sync::Mutex::new(None)),
    };
    let config = HarnessConfig {
        lifecycle_timeout: Duration::from_secs(1),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    };
    let mut harness = Harness::new(
        api.clone(),
        ExecThenSlowAnswer { calls: 0 },
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("the final check finds the row");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition
                && error.message == "execution left the sandbox unusable"),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 2, "recovery missed the row");
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished { outcome, .. }) if outcome == "failed"
    ));
}
