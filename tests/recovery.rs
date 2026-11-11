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
