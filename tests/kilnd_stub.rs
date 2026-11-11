//! Regression tests that need kilnd's real gRPC wire: a scripted kilnd stub
//! on a Unix socket, reached through `TonicKilnd` or the CLI.
mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::http::{FakeHttp, Reply};
use kiln_harness::api::{ExpiryReason, TonicKilnd};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, MemoryJournal};
use kiln_harness::pb;
use kiln_harness::pb::kilnd_service_server::{KilndService, KilndServiceServer};
use kiln_harness::provider::{ModelResponse, ScriptedProvider, ToolCall};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// A kilnd that creates one ready sandbox, answers the baseline `find`
/// with an empty workspace, and deletes on request.
#[derive(Default)]
struct Stub {
    /// Health answers this many calls, then never answers again.
    health_answers: u32,
    /// Advertise expiry and typed reasons.
    current: bool,
    /// Report every non-baseline Exec as canceled by a sandbox deletion that
    /// left the sandbox unusable.
    deleted_during_exec: bool,
    /// Publish each non-baseline Exec's row, then lose its response.
    lose_tool_response: bool,
    /// Rows `GetSandbox` reports, newest first.
    recent: Arc<std::sync::Mutex<Vec<pb::Execution>>>,
    health_calls: Arc<AtomicU32>,
    execs: Arc<AtomicU32>,
    deleted: Arc<AtomicU32>,
}

fn sandbox(state: pb::SandboxState) -> pb::Sandbox {
    pb::Sandbox {
        id: "sb-0000000000000001".to_string(),
        state: state as i32,
        backend_id: "test-backend-1".to_string(),
        spec: Some(pb::SandboxSpec::default()),
        ..pb::Sandbox::default()
    }
}

fn settled() -> pb::Output {
    pb::Output {
        total_bytes_known: true,
        ..pb::Output::default()
    }
}

#[async_trait]
impl KilndService for Stub {
    async fn health(
        &self,
        _request: Request<pb::HealthRequest>,
    ) -> Result<Response<pb::HealthResponse>, Status> {
        if self.health_calls.fetch_add(1, Ordering::SeqCst) >= self.health_answers {
            std::future::pending::<()>().await;
        }
        let runtime_capabilities = self.current.then(|| pb::RuntimeCapabilities {
            features: vec![
                pb::Feature::SandboxExpiry as i32,
                pb::Feature::TypedReasons as i32,
            ],
            limits: Some(pb::ApiLimits {
                min_sandbox_lifetime_ms: 10_000,
                max_sandbox_lifetime_ms: 7 * 86_400_000,
                min_sandbox_idle_ms: 10_000,
                max_sandbox_idle_ms: 86_400_000,
                ..pb::ApiLimits::default()
            }),
            ..pb::RuntimeCapabilities::default()
        });
        Ok(Response::new(pb::HealthResponse {
            status: pb::health_response::Status::Serving as i32,
            runtime_capabilities,
            ..pb::HealthResponse::default()
        }))
    }

    async fn create_sandbox(
        &self,
        _request: Request<pb::CreateSandboxRequest>,
    ) -> Result<Response<pb::CreateSandboxResponse>, Status> {
        Ok(Response::new(pb::CreateSandboxResponse {
            sandbox: Some(sandbox(pb::SandboxState::Ready)),
        }))
    }

    async fn exec(
        &self,
        request: Request<pb::ExecRequest>,
    ) -> Result<Response<pb::ExecResponse>, Status> {
        let number = self.execs.fetch_add(1, Ordering::SeqCst) + 1;
        let baseline = request
            .get_ref()
            .argv
            .first()
            .is_some_and(|arg| arg == "/usr/bin/find");
        let mut execution = pb::Execution {
            id: format!("ex-{number:016x}"),
            sandbox_id: "sb-0000000000000001".to_string(),
            state: pb::ExecutionState::Exited as i32,
            exit_code: Some(0),
            stdout: Some(settled()),
            stderr: Some(settled()),
            sandbox_reusable: self.current.then_some(true),
            ..pb::Execution::default()
        };
        if self.deleted_during_exec && !baseline {
            execution.state = pb::ExecutionState::Canceled as i32;
            execution.exit_code = None;
            execution.failure_reason = "sandbox was deleted while the execution ran".to_string();
            execution.sandbox_reusable = Some(false);
            execution.cancel_reason = pb::CancelReason::SandboxDeleted as i32;
        }
        self.recent
            .lock()
            .expect("recent")
            .insert(0, execution.clone());
        if self.lose_tool_response && !baseline {
            return Err(Status::unavailable(
                "response lost after terminal execution publication",
            ));
        }
        Ok(Response::new(pb::ExecResponse {
            execution: Some(execution),
        }))
    }

    async fn delete_sandbox(
        &self,
        _request: Request<pb::DeleteSandboxRequest>,
    ) -> Result<Response<pb::DeleteSandboxResponse>, Status> {
        self.deleted.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(pb::DeleteSandboxResponse {
            sandbox: Some(sandbox(pb::SandboxState::Deleted)),
        }))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<pb::ListSandboxesRequest>,
    ) -> Result<Response<pb::ListSandboxesResponse>, Status> {
        Ok(Response::new(pb::ListSandboxesResponse::default()))
    }

    async fn renew_sandbox(
        &self,
        _request: Request<pb::RenewSandboxRequest>,
    ) -> Result<Response<pb::RenewSandboxResponse>, Status> {
        Ok(Response::new(pb::RenewSandboxResponse {
            sandbox: Some(sandbox(pb::SandboxState::Ready)),
        }))
    }

    async fn get_sandbox(
        &self,
        _request: Request<pb::GetSandboxRequest>,
    ) -> Result<Response<pb::GetSandboxResponse>, Status> {
        Ok(Response::new(pb::GetSandboxResponse {
            sandbox: Some(sandbox(pb::SandboxState::Deleted)),
            recent_executions: self.recent.lock().expect("recent").clone(),
        }))
    }

    async fn get_execution(
        &self,
        _request: Request<pb::GetExecutionRequest>,
    ) -> Result<Response<pb::GetExecutionResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

/// Serve `stub` on `socket` until the returned sender fires. Call it inside
/// a runtime.
fn serve(stub: Stub, socket: &Path) -> tokio::sync::oneshot::Sender<()> {
    use tonic::codegen::tokio_stream::wrappers::UnixListenerStream;
    let listener = tokio::net::UnixListener::bind(socket).expect("bind");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .serve_with_incoming_shutdown(
                KilndServiceServer::new(stub),
                UnixListenerStream::new(listener),
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
            .expect("serve");
    });
    shutdown_tx
}

#[tokio::test]
async fn an_exec_that_left_the_sandbox_deleted_ends_the_session() {
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        current: true,
        deleted_during_exec: true,
        ..Stub::default()
    };
    let execs = stub.execs.clone();
    let deleted = stub.deleted.clone();
    let shutdown = serve(stub, &socket);

    let api = TonicKilnd::connect(socket).await.expect("connect");
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "call-1",
            "exec",
            json!({"argv": ["/bin/true"]}),
        )]),
        ModelResponse::final_text("done despite deleted sandbox"),
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("the sandbox is gone");
    assert!(
        matches!(
            error,
            HarnessError::SandboxExpired {
                reason: ExpiryReason::SandboxDeleted
            }
        ),
        "{error}"
    );
    // The baseline and the one tool call; the model was not asked again.
    assert_eq!(execs.load(Ordering::SeqCst), 2);
    let requests = harness
        .journal()
        .records
        .iter()
        .filter(|record| matches!(record.event, JournalEvent::ModelRequested { .. }))
        .count();
    assert_eq!(requests, 1);
    assert!(
        harness
            .journal()
            .records
            .iter()
            .any(|record| matches!(record.event, JournalEvent::ToolExecuted { ok: false, .. }))
    );
    assert!(deleted.load(Ordering::SeqCst) >= 1);
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            outcome,
            sandbox_deleted: true,
            ..
        }) if outcome == "failed"
    ));
    let _ = shutdown.send(());
}

#[tokio::test]
async fn a_recovered_execution_that_left_the_sandbox_deleted_ends_the_session() {
    // R2-01: the Exec response is lost after kilnd published a terminal row
    // that left the sandbox unusable; recovery finds that row.
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        current: true,
        deleted_during_exec: true,
        lose_tool_response: true,
        ..Stub::default()
    };
    let execs = stub.execs.clone();
    let shutdown = serve(stub, &socket);

    let api = TonicKilnd::connect(socket).await.expect("connect");
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "call-1",
            "exec",
            json!({"argv": ["/bin/true"]}),
        )]),
        ModelResponse::final_text("done after recovered deletion"),
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("the recovered row proves the sandbox gone");
    assert!(
        matches!(
            error,
            HarnessError::SandboxExpired {
                reason: ExpiryReason::SandboxDeleted
            }
        ),
        "{error}"
    );
    assert_eq!(execs.load(Ordering::SeqCst), 2);
    let events = harness
        .journal()
        .records
        .iter()
        .map(|record| &record.event)
        .collect::<Vec<_>>();
    // The recovered tool result is journaled, and the model is not asked
    // again.
    assert!(events.iter().any(|event| matches!(
        event,
        JournalEvent::ToolExecuted { execution_ids, ok: false, .. }
            if execution_ids == &["ex-0000000000000002".to_string()]
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, JournalEvent::ModelRequested { .. }))
            .count(),
        1
    );
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            outcome,
            sandbox_deleted: true,
            ..
        }) if outcome == "failed"
    ));
    let _ = shutdown.send(());
}

/// Run `stub` and, when given, a scripted model server on a thread of their
/// own while the CLI runs, so the test can block on the child.
struct Servers {
    socket: PathBuf,
    base_url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Servers {
    fn start(dir: &Path, stub: Stub, model: Vec<Reply>) -> Self {
        let socket = dir.join("kilnd.sock");
        let server_socket = socket.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let kilnd = serve(stub, &server_socket);
                let http = FakeHttp::start(model).await;
                ready_tx.send(http.base_url()).expect("ready");
                let _ = stop_rx.await;
                let _ = kilnd.send(());
                http.stop();
            });
        });
        let base_url = ready_rx.recv().expect("servers ready");
        Self {
            socket,
            base_url,
            shutdown: Some(stop_tx),
            thread: Some(thread),
        }
    }

    fn run(&self, dir: &Path, session_timeout_secs: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
            .arg("--socket")
            .arg(&self.socket)
            .arg("run")
            .args(["--provider", "openai-compat", "--base-url", &self.base_url])
            .args(["--model", "test-model", "--prompt", "say done"])
            .args(["--session-timeout-secs", session_timeout_secs])
            .arg("--audit")
            .arg(dir.join("out/audit.jsonl"))
            .arg("--journal")
            .arg(dir.join("out/run.jsonl"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the CLI")
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Wait for the child to exit within `limit`, killing it if it does not.
fn exit_within(mut child: Child, limit: Duration) -> Option<std::process::Output> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if child.try_wait().expect("child state").is_some() {
            return Some(child.wait_with_output().expect("output"));
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[test]
fn discovery_that_never_answers_ends_at_the_session_deadline() {
    let temp = common::private_tempdir().expect("tempdir");
    let servers = Servers::start(temp.path(), Stub::default(), Vec::new());
    let output = exit_within(servers.run(temp.path(), "1"), Duration::from_secs(10))
        .expect("the CLI exits by its session deadline");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session deadline expired"), "{stderr}");
}

#[test]
fn a_post_session_health_that_never_answers_warns_and_keeps_success() {
    let temp = common::private_tempdir().expect("tempdir");
    let stub = Stub {
        // The session's discovery answers; the check after it never does.
        health_answers: 1,
        ..Stub::default()
    };
    let completion = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
    });
    let servers = Servers::start(temp.path(), stub, vec![Reply::json(200, &completion)]);
    // The default lifecycle window is 10 s.
    let output = exit_within(servers.run(temp.path(), "60"), Duration::from_secs(30))
        .expect("the CLI does not hang after the session");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: kilnd health after the session did not answer"),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("done"));
}
