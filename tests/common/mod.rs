//! A fake kilnd shared by host-side tests.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiln_harness::api::{
    ApiError, CapturedOutput, Execution, ExecutionState, Health, NodeReservations, Sandbox,
    SandboxApi, SandboxExec, SandboxRequest,
};
use kiln_harness::engine::HarnessConfig;
use tonic::Code;

#[derive(Clone, Default)]
pub struct FakeApi {
    pub state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
pub enum DeleteBehavior {
    #[default]
    Remove,
    AcknowledgeOnly,
    FailAfterRemoving,
}

/// Test knobs; independent flags read better than a state enum here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
pub struct FakeState {
    pub creates: Vec<SandboxRequest>,
    pub execs: Vec<SandboxExec>,
    pub executions: Vec<Execution>,
    pub deleted: Vec<String>,
    pub files: BTreeMap<String, Vec<u8>>,
    pub next_execution: u64,
    pub create_delay: Duration,
    pub create_ready_at: Option<std::time::Instant>,
    pub suppress_create_record: bool,
    pub create_error_after_record: bool,
    pub create_rejection: Option<ApiError>,
    pub tool_exec_delay: Duration,
    pub baseline_exec_delay: Duration,
    pub fail_baseline: bool,
    pub delete_failures: u32,
    pub delete_attempts: u32,
    pub delete_behavior: DeleteBehavior,
    pub delete_acknowledgements_before_removal: u32,
    pub list_failures: u32,
    pub list_delay: Duration,
    pub list_attempts: u32,
    /// Bytes `head` returns instead of the stored file, to simulate a sandbox
    /// that answers differently from a recording.
    pub read_override: Option<Vec<u8>>,
    /// Let delete succeed while a create is still in flight.
    pub create_allows_early_delete: bool,
    /// Record the created row this long after the request, independently of
    /// the RPC.
    pub create_record_delay: Duration,
}

impl FakeApi {
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
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
            if let Some(error) = &state.create_rejection {
                return Err(error.clone());
            }
            let delay = state.create_delay;
            let fail = state.create_error_after_record;
            if !state.suppress_create_record {
                if state.create_record_delay.is_zero() {
                    state.creates.push(request);
                    state.create_ready_at = if state.create_allows_early_delete {
                        None
                    } else {
                        std::time::Instant::now().checked_add(delay)
                    };
                } else {
                    // The daemon records the row on its own schedule, even
                    // after the client drops the RPC.
                    let shared = self.state.clone();
                    let after = state.create_record_delay;
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        shared
                            .lock()
                            .expect("fake state lock")
                            .creates
                            .push(request);
                    });
                }
            }
            (delay, fail)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if fail {
            return Err(api_error(Code::Unavailable, "create response lost"));
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
                    let stored = state.read_override.as_ref().or(state.files.get(path));
                    match stored {
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
                Code::FailedPrecondition,
                "create operation is still in progress",
            ));
        }
        if state.delete_failures > 0 {
            state.delete_failures -= 1;
            return Err(api_error(Code::Unavailable, "transient delete failure"));
        }
        if matches!(state.delete_behavior, DeleteBehavior::AcknowledgeOnly) {
            return Ok(());
        }
        if state.delete_acknowledgements_before_removal > 0 {
            state.delete_acknowledgements_before_removal -= 1;
            return Ok(());
        }
        if !state.deleted.iter().any(|id| id == sandbox_id) {
            state.deleted.push(sandbox_id.to_string());
        }
        if matches!(state.delete_behavior, DeleteBehavior::FailAfterRemoving) {
            return Err(api_error(Code::Unavailable, "delete response lost"));
        }
        Ok(())
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        let state = self.state();
        Ok(state.executions.iter().rev().cloned().collect())
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        let delay = {
            let mut state = self.state();
            state.list_attempts += 1;
            if state.list_failures > 0 {
                state.list_failures -= 1;
                return Err(api_error(Code::Unavailable, "listing failed"));
            }
            state.list_delay
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
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

pub fn api_error(code: Code, message: &str) -> ApiError {
    ApiError {
        code,
        message: message.to_string(),
    }
}

pub fn captured(data: Vec<u8>) -> CapturedOutput {
    CapturedOutput {
        observed_bytes: data.len() as u64,
        data,
        total_bytes_known: true,
        truncated: false,
    }
}

pub fn short_lifecycle_config() -> HarnessConfig {
    HarnessConfig {
        lifecycle_timeout: Duration::from_millis(100),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    }
}
