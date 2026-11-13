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
use kiln_harness::api::{ExpiryReason, Reconnect as _, SandboxApi as _, TonicKilnd};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, MemoryJournal};
use kiln_harness::pb;
use kiln_harness::pb::kilnd_service_server::{KilndService, KilndServiceServer};
use kiln_harness::provider::{ModelResponse, ScriptedProvider, ToolCall};
use kiln_harness::tools::STREAM_PROTOCOL_VIOLATION;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// A kilnd that creates one ready sandbox, answers the baseline `find`
/// with an empty workspace, and deletes on request.
// Independent scripted behaviors read better as flags than as a state enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct Stub {
    /// Health answers this many calls, then never answers again.
    health_answers: u32,
    /// Advertise expiry, typed reasons, idempotent create and request-ID
    /// release beside the exec options, which every stub lists.
    current: bool,
    /// Record the first create, then lose its response: `UNAVAILABLE`.
    lose_first_create: bool,
    /// Every create and delete request as it arrived on the wire.
    create_requests: Arc<std::sync::Mutex<Vec<pb::CreateSandboxRequest>>>,
    delete_requests: Arc<std::sync::Mutex<Vec<pb::DeleteSandboxRequest>>>,
    /// Every Exec request as it arrived on the wire.
    exec_requests: Arc<std::sync::Mutex<Vec<pb::ExecRequest>>>,
    /// Report every non-baseline Exec as canceled by a sandbox deletion that
    /// left the sandbox unusable.
    deleted_during_exec: bool,
    /// Publish each non-baseline Exec's row, then lose its response.
    lose_tool_response: bool,
    /// Publish each non-baseline Exec's row RUNNING and lose its response;
    /// the first `GetSandbox` reports it RUNNING and then terminalizes it as
    /// canceled for a daemon shutdown, leaving the sandbox unusable.
    running_then_settles: bool,
    /// Rows `GetSandbox` reports, newest first.
    recent: Arc<std::sync::Mutex<Vec<pb::Execution>>>,
    /// Every `ExecStream` request as it arrived on the wire.
    stream_requests: Arc<std::sync::Mutex<Vec<pb::ExecRequest>>>,
    /// What a streamed execution writes to stdout, sent live in two chunks
    /// split mid-line and retained in its row.
    stream_stdout: Vec<u8>,
    /// Break `ExecStream`'s message rules this way.
    stream_violation: Option<Violation>,
    /// Send [`gapped_messages`] instead, which keeps every rule.
    stream_gapped: bool,
    health_calls: Arc<AtomicU32>,
    execs: Arc<AtomicU32>,
    deleted: Arc<AtomicU32>,
}

/// One way a stream can break kilnd's `ExecStream` ordering rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Violation {
    /// `seq` jumps from 0 to 2.
    SeqGap,
    /// The first message is a chunk, not `ExecStarted`.
    NotStartedFirst,
    /// `ExecFinished` names another execution.
    IdMismatch,
    /// A chunk follows `ExecFinished`.
    AfterFinished,
    /// A chunk names no output stream.
    UnnamedStream,
    /// The stream ends after `ExecStarted` with no `ExecFinished`.
    EndsEarly,
    /// A chunk starts again at offset 0.
    RegressingOffset,
    /// A chunk starts inside the previous one.
    OverlappingOffset,
    /// A chunk starts past the previous one's end, with no gap between.
    HoleWithoutGap,
    /// A gap starts past the previous chunk's end.
    GapAtWrongOffset,
    /// A gap's end overflows `u64`.
    OffsetOverflow,
    /// `ExecFinished` reports more bytes delivered than the chunks carried.
    WrongFinalTotals,
}

fn message(seq: u64, event: pb::exec_stream_response::Event) -> pb::ExecStreamResponse {
    pb::ExecStreamResponse {
        seq,
        event: Some(event),
    }
}

fn started(execution: &pb::Execution) -> pb::exec_stream_response::Event {
    pb::exec_stream_response::Event::Started(pb::ExecStarted {
        execution_id: execution.id.clone(),
        sandbox_id: execution.sandbox_id.clone(),
        started_at_unix_ms: 1,
        timeout_ms: 1_000,
    })
}

fn chunk(stream: pb::OutputStream, offset: u64, data: &[u8]) -> pb::exec_stream_response::Event {
    pb::exec_stream_response::Event::Chunk(pb::OutputChunk {
        stream: stream as i32,
        offset,
        data: data.to_vec(),
    })
}

fn gap(
    stream: pb::OutputStream,
    offset: u64,
    dropped: u64,
    known: bool,
) -> pb::exec_stream_response::Event {
    pb::exec_stream_response::Event::Gap(pb::OutputGap {
        stream: stream as i32,
        offset,
        dropped_bytes: dropped,
        dropped_bytes_known: known,
    })
}

fn finished(execution: pb::Execution, delivered: u64) -> pb::exec_stream_response::Event {
    pb::exec_stream_response::Event::Finished(pb::ExecFinished {
        execution: Some(execution),
        stdout: Some(pb::LiveDelivery {
            delivered_bytes: delivered,
            dropped_bytes: 0,
            dropped_bytes_known: true,
        }),
        stderr: Some(pb::LiveDelivery {
            dropped_bytes_known: true,
            ..pb::LiveDelivery::default()
        }),
    })
}

/// The messages of one stream for `execution`, whose retained stdout is
/// what it wrote: `ExecStarted`, that output in two chunks, `ExecFinished`;
/// or the same broken as `violation` says.
fn stream_messages(
    execution: pb::Execution,
    violation: Option<Violation>,
) -> Vec<pb::ExecStreamResponse> {
    use pb::OutputStream::{Stdout, Unspecified};
    let output = execution
        .stdout
        .as_ref()
        .map(|output| output.data.clone())
        .unwrap_or_default();
    let split = output.len() / 2;
    let (head, tail) = output.split_at(split);
    let offset = split as u64;
    let delivered = output.len() as u64;
    match violation {
        None => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, head)),
            message(2, chunk(Stdout, offset, tail)),
            message(3, finished(execution, delivered)),
        ],
        Some(Violation::SeqGap) => vec![
            message(0, started(&execution)),
            message(2, chunk(Stdout, 0, head)),
            message(3, finished(execution, delivered)),
        ],
        Some(Violation::NotStartedFirst) => vec![
            message(0, chunk(Stdout, 0, head)),
            message(1, started(&execution)),
            message(2, finished(execution, delivered)),
        ],
        Some(Violation::IdMismatch) => {
            let other = pb::Execution {
                id: "ex-00000000000000ff".to_string(),
                ..execution.clone()
            };
            vec![
                message(0, started(&execution)),
                message(1, finished(other, delivered)),
            ]
        }
        Some(Violation::AfterFinished) => vec![
            message(0, started(&execution)),
            message(1, finished(execution, delivered)),
            message(2, chunk(Stdout, 0, head)),
        ],
        Some(Violation::UnnamedStream) => vec![
            message(0, started(&execution)),
            message(1, chunk(Unspecified, 0, head)),
            message(2, finished(execution, delivered)),
        ],
        Some(Violation::EndsEarly) => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, head)),
        ],
        Some(Violation::RegressingOffset) => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, b"abc")),
            message(2, chunk(Stdout, 0, b"abc")),
            message(3, finished(execution, 6)),
        ],
        Some(Violation::OverlappingOffset) => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, b"abcd")),
            message(2, chunk(Stdout, 2, b"cdef")),
            message(3, finished(execution, 8)),
        ],
        Some(Violation::HoleWithoutGap) => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, b"abc")),
            message(2, chunk(Stdout, 5, b"fg")),
            message(3, finished(execution, 5)),
        ],
        Some(Violation::GapAtWrongOffset) => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, b"abc")),
            message(2, gap(Stdout, 5, 2, true)),
            message(3, finished(execution, 3)),
        ],
        Some(Violation::OffsetOverflow) => vec![
            message(0, started(&execution)),
            message(1, gap(Stdout, 0, u64::MAX, true)),
            message(2, gap(Stdout, u64::MAX, 1, true)),
            message(3, finished(execution, 0)),
        ],
        Some(Violation::WrongFinalTotals) => vec![
            message(0, started(&execution)),
            message(1, chunk(Stdout, 0, head)),
            message(2, chunk(Stdout, offset, tail)),
            message(3, finished(execution, delivered + 1)),
        ],
    }
}

/// [`stream_messages`] for a stream that keeps every rule but drops bytes:
/// a chunk, an exact interior gap, the next chunk right after it, and an
/// inexact empty tail gap, as kilnd's emitter sends them.
fn gapped_messages(execution: pb::Execution) -> Vec<pb::ExecStreamResponse> {
    use pb::OutputStream::{Stderr, Stdout};
    let start = started(&execution);
    let mut done = finished(execution, 5);
    if let pb::exec_stream_response::Event::Finished(finished) = &mut done {
        finished.stdout = Some(pb::LiveDelivery {
            delivered_bytes: 5,
            dropped_bytes: 2,
            dropped_bytes_known: false,
        });
    }
    vec![
        message(0, start),
        message(1, chunk(Stdout, 0, b"abc")),
        message(2, gap(Stdout, 3, 2, true)),
        message(3, chunk(Stdout, 5, b"fg")),
        message(4, gap(Stdout, 7, 0, false)),
        message(5, gap(Stderr, 0, 0, true)),
        message(6, done),
    ]
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

/// kilnd 0c79944's default exec-option bounds and kilnd 2562a53's default
/// stream timeouts, on the wire.
fn stub_exec_limits() -> pb::ApiLimits {
    pb::ApiLimits {
        max_cwd_bytes: 4096,
        max_env_entries: 64,
        max_env_name_bytes: 128,
        max_env_value_bytes: 4096,
        max_env_total_bytes: 32_768,
        default_stream_timeout_ms: 1_800_000,
        max_stream_timeout_ms: 7_200_000,
        ..pb::ApiLimits::default()
    }
}

fn settled() -> pb::Output {
    pb::Output {
        total_bytes_known: true,
        ..pb::Output::default()
    }
}

impl Stub {
    /// Record one `Exec` or `ExecStream` as the scripted kilnd would and return
    /// its row, or the status it answers with instead.
    fn record(
        &self,
        request: &pb::ExecRequest,
        mode: pb::ExecutionMode,
    ) -> Result<pb::Execution, Status> {
        let number = self.execs.fetch_add(1, Ordering::SeqCst) + 1;
        self.exec_requests
            .lock()
            .expect("exec requests")
            .push(request.clone());
        let baseline = request
            .argv
            .first()
            .is_some_and(|arg| arg == "/usr/bin/find");
        let (cwd, env_names, env_sha256) = common::pb_recorded_context(request);
        let stdout = if mode == pb::ExecutionMode::Stream {
            pb::Output {
                data: self.stream_stdout.clone(),
                total_bytes: self.stream_stdout.len() as u64,
                ..settled()
            }
        } else {
            settled()
        };
        let mut execution = pb::Execution {
            id: format!("ex-{number:016x}"),
            cwd,
            env_names,
            env_sha256,
            sandbox_id: "sb-0000000000000001".to_string(),
            state: pb::ExecutionState::Exited as i32,
            exit_code: Some(0),
            stdout: Some(stdout),
            stderr: Some(settled()),
            sandbox_reusable: self.current.then_some(true),
            mode: mode as i32,
            ..pb::Execution::default()
        };
        if self.deleted_during_exec && !baseline {
            execution.state = pb::ExecutionState::Canceled as i32;
            execution.exit_code = None;
            execution.failure_reason = "sandbox was deleted while the execution ran".to_string();
            execution.sandbox_reusable = Some(false);
            execution.cancel_reason = pb::CancelReason::SandboxDeleted as i32;
        }
        if self.running_then_settles && !baseline {
            execution.state = pb::ExecutionState::Running as i32;
            execution.exit_code = None;
            execution.sandbox_reusable = None;
            self.recent.lock().expect("recent").insert(0, execution);
            return Err(Status::unavailable(
                "connection lost while the execution was still running",
            ));
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
        Ok(execution)
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
        // Every kilnd a live session accepts lists the exec options and
        // exec streaming.
        let exec_options = [
            pb::Feature::ExecCwd as i32,
            pb::Feature::ExecEnv as i32,
            pb::Feature::ExecStream as i32,
        ];
        let exec_limits = stub_exec_limits();
        let runtime_capabilities = Some(if self.current {
            pb::RuntimeCapabilities {
                features: [
                    pb::Feature::SandboxExpiry as i32,
                    pb::Feature::TypedReasons as i32,
                    pb::Feature::IdempotentCreate as i32,
                    pb::Feature::RequestIdRelease as i32,
                ]
                .into_iter()
                .chain(exec_options)
                .collect(),
                limits: Some(pb::ApiLimits {
                    min_sandbox_lifetime_ms: 10_000,
                    max_sandbox_lifetime_ms: 7 * 86_400_000,
                    min_sandbox_idle_ms: 10_000,
                    max_sandbox_idle_ms: 86_400_000,
                    ..exec_limits
                }),
                ..pb::RuntimeCapabilities::default()
            }
        } else {
            pb::RuntimeCapabilities {
                features: exec_options.to_vec(),
                limits: Some(exec_limits),
                ..pb::RuntimeCapabilities::default()
            }
        });
        Ok(Response::new(pb::HealthResponse {
            status: pb::health_response::Status::Serving as i32,
            runtime_capabilities,
            ..pb::HealthResponse::default()
        }))
    }

    async fn create_sandbox(
        &self,
        request: Request<pb::CreateSandboxRequest>,
    ) -> Result<Response<pb::CreateSandboxResponse>, Status> {
        let request = request.into_inner();
        let mut requests = self.create_requests.lock().expect("creates");
        // kilnd's keyed lookup: the one sandbox answers every equal request
        // under its key; an unequal one is a conflict.
        let conflict = !request.request_id.is_empty()
            && requests
                .iter()
                .any(|seen| seen.request_id == request.request_id && seen != &request);
        let first = requests.is_empty();
        requests.push(request.clone());
        drop(requests);
        if conflict {
            let mut metadata = tonic::metadata::MetadataMap::new();
            metadata.insert(
                kiln_harness::api::REASON_KEY,
                tonic::metadata::MetadataValue::from_static("request_id_conflict"),
            );
            return Err(Status::with_metadata(
                tonic::Code::AlreadyExists,
                "request_id_conflict",
                metadata,
            ));
        }
        if first && self.lose_first_create {
            return Err(Status::unavailable("create response lost"));
        }
        Ok(Response::new(pb::CreateSandboxResponse {
            sandbox: Some(pb::Sandbox {
                request_id: request.request_id,
                ..sandbox(pb::SandboxState::Ready)
            }),
        }))
    }

    async fn exec(
        &self,
        request: Request<pb::ExecRequest>,
    ) -> Result<Response<pb::ExecResponse>, Status> {
        let execution = self.record(request.get_ref(), pb::ExecutionMode::Unary)?;
        Ok(Response::new(pb::ExecResponse {
            execution: Some(execution),
        }))
    }

    type ExecStreamStream = std::pin::Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<pb::ExecStreamResponse, Status>>
                + Send,
        >,
    >;

    async fn exec_stream(
        &self,
        request: Request<pb::ExecStreamRequest>,
    ) -> Result<Response<Self::ExecStreamStream>, Status> {
        let exec = request.into_inner().exec.unwrap_or_default();
        self.stream_requests
            .lock()
            .expect("stream requests")
            .push(exec.clone());
        let execution = self.record(&exec, pb::ExecutionMode::Stream)?;
        let messages = if self.stream_gapped {
            gapped_messages(execution)
        } else {
            stream_messages(execution, self.stream_violation)
        };
        Ok(Response::new(Box::pin(tonic::codegen::tokio_stream::iter(
            messages.into_iter().map(Ok),
        ))))
    }

    async fn delete_sandbox(
        &self,
        request: Request<pb::DeleteSandboxRequest>,
    ) -> Result<Response<pb::DeleteSandboxResponse>, Status> {
        self.deleted.fetch_add(1, Ordering::SeqCst);
        let request = request.into_inner();
        let request_id = self
            .create_requests
            .lock()
            .expect("creates")
            .last()
            .map(|create| create.request_id.clone())
            .unwrap_or_default();
        let released = request.release_request_id && !request_id.is_empty();
        self.delete_requests.lock().expect("deletes").push(request);
        Ok(Response::new(pb::DeleteSandboxResponse {
            sandbox: Some(pb::Sandbox {
                request_id,
                request_id_released: released,
                ..sandbox(pb::SandboxState::Deleted)
            }),
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
        let mut recent = self.recent.lock().expect("recent");
        let snapshot = recent.clone();
        if let Some(row) = recent.first_mut()
            && row.state == pb::ExecutionState::Running as i32
        {
            row.state = pb::ExecutionState::Canceled as i32;
            row.failure_reason = "the disconnect retired the sandbox".to_string();
            row.sandbox_reusable = Some(false);
            row.cancel_reason = pb::CancelReason::DaemonShutdown as i32;
        }
        let state = if self.deleted.load(Ordering::SeqCst) > 0 {
            pb::SandboxState::Deleted
        } else {
            pb::SandboxState::Ready
        };
        Ok(Response::new(pb::GetSandboxResponse {
            sandbox: Some(sandbox(state)),
            recent_executions: snapshot,
        }))
    }

    async fn get_execution(
        &self,
        request: Request<pb::GetExecutionRequest>,
    ) -> Result<Response<pb::GetExecutionResponse>, Status> {
        let id = &request.get_ref().id;
        let execution = self
            .recent
            .lock()
            .expect("recent")
            .iter()
            .find(|row| &row.id == id)
            .cloned()
            .ok_or_else(|| Status::not_found("no such execution"))?;
        Ok(Response::new(pb::GetExecutionResponse {
            execution: Some(execution),
        }))
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

/// `reconnect` dials the socket again for a connection of its own: both
/// connections answer, and once the socket is gone a reconnect fails while
/// the connections already open still work.
#[tokio::test]
async fn reconnect_opens_a_new_connection_to_the_same_socket() {
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let shutdown = serve(
        Stub {
            health_answers: u32::MAX,
            current: true,
            ..Stub::default()
        },
        &socket,
    );
    let api = TonicKilnd::connect(socket.clone()).await.expect("connect");
    let other = api.reconnect().await.expect("reconnect");
    assert!(api.health().await.expect("health").serving);
    assert!(other.health().await.expect("health on the new one").serving);

    std::fs::remove_file(&socket).expect("unlink the socket");
    let Err(error) = api.reconnect().await else {
        panic!("a reconnect without a socket succeeded");
    };
    assert!(error.to_string().contains("connect to"), "{error}");
    for open in [&api, &other] {
        assert!(
            open.health()
                .await
                .expect("an open one still answers")
                .serving
        );
    }
    let _ = shutdown.send(());
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

#[tokio::test]
async fn a_running_recovery_snapshot_is_followed_until_it_settles() {
    // R3-01: the first recovered snapshot is RUNNING; the terminal row says
    // the disconnect left the sandbox unusable, for a non-expiry cause.
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        current: true,
        running_then_settles: true,
        ..Stub::default()
    };
    let shutdown = serve(stub, &socket);
    let api = TonicKilnd::connect(socket).await.expect("connect");
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "call-1",
            "exec",
            json!({"argv": ["/bin/true"]}),
        )]),
        ModelResponse::final_text("done after a running recovery snapshot"),
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
        .expect_err("the settled row proves the sandbox unusable");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.message == "execution left the sandbox unusable"),
        "{error}"
    );
    let requests = harness
        .journal()
        .records
        .iter()
        .filter(|record| matches!(record.event, JournalEvent::ModelRequested { .. }))
        .count();
    assert_eq!(requests, 1);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn a_keyed_create_is_retried_on_the_wire_with_the_same_bytes_and_keeps_its_key() {
    use prost::Message as _;
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        current: true,
        lose_first_create: true,
        ..Stub::default()
    };
    let creates = stub.create_requests.clone();
    let deletes = stub.delete_requests.clone();
    let health_calls = stub.health_calls.clone();
    let shutdown = serve(stub, &socket);

    let api = TonicKilnd::connect(socket).await.expect("connect");
    let provider = ScriptedProvider::new([ModelResponse::final_text("done")]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("the retry finds the sandbox");

    let session_id = harness.audit().records[0].session_id.clone();
    let creates = creates.lock().expect("creates").clone();
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0].request_id, format!("kiln-harness:{session_id}"));
    assert_eq!(creates[0].encode_to_vec(), creates[1].encode_to_vec());
    // Discovery, then the reread before the retry.
    assert_eq!(health_calls.load(Ordering::SeqCst), 2);
    let deletes = deletes.lock().expect("deletes").clone();
    // After a retry, the lost first attempt could still arrive: every
    // delete keeps the key.
    assert!(!deletes.is_empty());
    assert!(deletes.iter().all(|delete| !delete.release_request_id));
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::SandboxCreated { request_id: Some(key), create_attempts: 2, .. }
            if key == &creates[0].request_id
    )));
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished {
            outcome,
            request_id_released: None,
            ..
        }) if outcome == "completed"
    ));
    let _ = shutdown.send(());
}

#[tokio::test]
async fn the_adapter_sends_the_key_and_reads_a_conflict_as_already_exists() {
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        current: true,
        ..Stub::default()
    };
    let deletes = stub.delete_requests.clone();
    let shutdown = serve(stub, &socket);
    let api = TonicKilnd::connect(socket).await.expect("connect");
    let request = kiln_harness::api::SandboxRequest {
        image: "alpine".to_string(),
        memory_bytes: 1 << 20,
        cpu_millis: 100,
        pids: 8,
        disk_bytes: 1 << 20,
        network_none: true,
        read_only_root: true,
        writable_paths: Vec::new(),
        labels: std::collections::BTreeMap::new(),
        expiry: None,
        request_id: Some("kiln-harness:wire".to_string()),
    };
    let sandbox = api.create(request.clone()).await.expect("create");
    assert_eq!(sandbox.request_id, "kiln-harness:wire");
    assert_eq!(sandbox.state, kiln_harness::api::SandboxState::Ready);
    let mut labeled = request;
    labeled.labels.insert("other".to_string(), "x".to_string());
    let conflict = api.create(labeled).await.expect_err("conflict");
    assert_eq!(conflict.code, tonic::Code::AlreadyExists);
    assert_eq!(conflict.reason.as_deref(), Some("request_id_conflict"));
    let released = api.delete(&sandbox.id, true).await.expect("delete");
    assert!(released.request_id_released);
    api.delete(&sandbox.id, false).await.expect("delete");
    let deletes = deletes.lock().expect("deletes").clone();
    assert_eq!(
        deletes
            .iter()
            .map(|delete| delete.release_request_id)
            .collect::<Vec<_>>(),
        [true, false]
    );
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
        self.command(dir, session_timeout_secs)
            .spawn()
            .expect("start the CLI")
    }

    fn command(&self, dir: &Path, session_timeout_secs: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kiln-harness"));
        command
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
            .stderr(Stdio::piped());
        command
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
fn the_cli_replays_a_legacy_journal_with_no_cwd_or_env() {
    // M3B-10: `kiln-harness replay` must go through `Harness::replay`, so a
    // journal recorded before the exec context runs as it ran: kilnd's `/`
    // and no overrides, journaled without `exec_context`. Routing it through
    // the public `run` would send /workspace and the overrides instead.
    let temp = common::private_tempdir().expect("tempdir");
    let stub = Stub {
        health_answers: u32::MAX,
        ..Stub::default()
    };
    let exec_requests = Arc::clone(&stub.exec_requests);
    let servers = Servers::start(temp.path(), stub, Vec::new());
    let golden =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/journal-demo-golden.jsonl");
    let journal = temp.path().join("out/replay.jsonl");
    let child = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .arg("--socket")
        .arg(&servers.socket)
        .arg("replay")
        .arg("--from")
        .arg(&golden)
        .arg("--audit")
        .arg(temp.path().join("out/replay-audit.jsonl"))
        .arg("--journal")
        .arg(&journal)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    // The stub's outputs differ from the recording, so the replay may
    // diverge; only what it sent matters here.
    let output = exit_within(child, Duration::from_secs(30)).expect("the replay ends");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let requests = exec_requests.lock().expect("exec requests").clone();
    let tools = requests
        .iter()
        .filter(|request| {
            request
                .argv
                .first()
                .is_none_or(|arg| arg != "/usr/bin/find")
        })
        .count();
    assert!(tools > 0, "no tool Exec reached kilnd: {stderr}");
    for request in &requests {
        assert_eq!(request.cwd, "", "{:?}", request.argv);
        assert!(request.env.is_empty(), "{:?}", request.argv);
    }
    let first = std::fs::read_to_string(&journal).expect("the replay journal");
    let first = first.lines().next().expect("a session start");
    assert!(first.contains("\"session_started\""), "{first}");
    assert!(!first.contains("exec_context"), "{first}");
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

/// Everything an `ExecStream` observer was told.
#[derive(Default)]
struct Seen {
    started: Vec<String>,
    stdout: Vec<u8>,
    chunks: u32,
    gaps: u32,
    /// Where stdout's next chunk or gap starts.
    next: u64,
}

impl kiln_harness::api::StreamObserver for Seen {
    fn started(&mut self, execution_id: &str) {
        self.started.push(execution_id.to_string());
    }

    fn chunk(
        &mut self,
        stream: kiln_harness::api::LiveStream,
        offset: u64,
        data: &[u8],
    ) -> std::ops::ControlFlow<()> {
        assert_eq!(stream, kiln_harness::api::LiveStream::Stdout);
        assert_eq!(offset, self.next);
        self.next += data.len() as u64;
        self.stdout.extend_from_slice(data);
        self.chunks += 1;
        std::ops::ControlFlow::Continue(())
    }

    fn gap(
        &mut self,
        stream: kiln_harness::api::LiveStream,
        offset: u64,
        dropped: u64,
        _known: bool,
    ) -> std::ops::ControlFlow<()> {
        if stream == kiln_harness::api::LiveStream::Stdout {
            assert_eq!(offset, self.next);
            self.next = self.next.saturating_add(dropped);
        }
        self.gaps += 1;
        std::ops::ControlFlow::Continue(())
    }
}

fn echo_request() -> kiln_harness::api::SandboxExec {
    kiln_harness::api::SandboxExec {
        sandbox_id: "sb-0000000000000001".to_string(),
        argv: vec!["/bin/echo".to_string(), "hello".to_string()],
        stdin: Vec::new(),
        timeout: Duration::from_millis(1_000),
        cwd: "/workspace".to_string(),
        env: common::workspace_env(),
    }
}

#[tokio::test]
async fn the_adapter_accepts_gaps_that_keep_the_offset_rules() {
    use kiln_harness::api::StreamedExec;
    // An exact interior gap, the next chunk right after it, and inexact
    // and empty tail gaps are accepted, with kilnd's totals.
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        stream_stdout: b"abcfg".to_vec(),
        stream_gapped: true,
        ..Stub::default()
    };
    let shutdown = serve(stub, &socket);
    let api = TonicKilnd::connect(socket).await.expect("connect");
    let mut seen = Seen::default();
    let streamed = api
        .exec_stream(echo_request(), &mut seen)
        .await
        .expect("admitted");
    let StreamedExec::Finished { stdout, .. } = streamed else {
        panic!("gapped: {streamed:?}");
    };
    assert_eq!((stdout.delivered_bytes, stdout.dropped_bytes), (5, 2));
    assert_eq!(
        (seen.stdout.as_slice(), seen.chunks, seen.gaps),
        (&b"abcfg"[..], 2, 3)
    );
    let _ = shutdown.send(());
}

#[tokio::test]
async fn the_adapter_reads_a_stream_and_rejects_protocol_breaks() {
    use kiln_harness::api::{ExecutionMode, StreamedExec};
    // A well-formed stream: ExecStarted, two chunks split mid-line, then
    // ExecFinished with the stored row.
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        stream_stdout: b"hello\nwor".to_vec(),
        ..Stub::default()
    };
    let requests = Arc::clone(&stub.stream_requests);
    let shutdown = serve(stub, &socket);
    let api = TonicKilnd::connect(socket).await.expect("connect");
    let mut seen = Seen::default();
    let streamed = api
        .exec_stream(echo_request(), &mut seen)
        .await
        .expect("admitted");
    let StreamedExec::Finished {
        execution, stdout, ..
    } = streamed
    else {
        panic!("{streamed:?}");
    };
    assert_eq!(execution.mode, ExecutionMode::Stream);
    assert_eq!(execution.stdout.data, b"hello\nwor");
    assert_eq!(stdout.delivered_bytes, 9);
    assert_eq!(seen.started, [execution.id.clone()]);
    assert_eq!(
        (seen.stdout.as_slice(), seen.chunks),
        (&b"hello\nwor"[..], 2)
    );
    // The ExecRequest went inside ExecStreamRequest as for a unary Exec.
    let sent = requests.lock().expect("requests")[0].clone();
    assert_eq!(sent.argv, ["/bin/echo", "hello"]);
    assert_eq!((sent.cwd.as_str(), sent.timeout_ms), ("/workspace", 1_000));
    let _ = shutdown.send(());

    // Each break, with what the observer saw before it: the offending
    // message never reaches it.
    for (violation, started, chunks, gaps) in [
        (Violation::SeqGap, true, 0, 0),
        (Violation::NotStartedFirst, false, 0, 0),
        (Violation::IdMismatch, true, 0, 0),
        (Violation::AfterFinished, true, 0, 0),
        (Violation::UnnamedStream, true, 0, 0),
        (Violation::EndsEarly, true, 1, 0),
        (Violation::RegressingOffset, true, 1, 0),
        (Violation::OverlappingOffset, true, 1, 0),
        (Violation::HoleWithoutGap, true, 1, 0),
        (Violation::GapAtWrongOffset, true, 1, 0),
        (Violation::OffsetOverflow, true, 0, 1),
        (Violation::WrongFinalTotals, true, 2, 0),
    ] {
        let temp = common::private_tempdir().expect("tempdir");
        let socket = temp.path().join("kilnd.sock");
        let stub = Stub {
            health_answers: u32::MAX,
            stream_stdout: b"hello\n".to_vec(),
            stream_violation: Some(violation),
            ..Stub::default()
        };
        let shutdown = serve(stub, &socket);
        let api = TonicKilnd::connect(socket).await.expect("connect");
        let mut seen = Seen::default();
        let streamed = api
            .exec_stream(echo_request(), &mut seen)
            .await
            .expect("admitted");
        let StreamedExec::Broken {
            execution_id,
            error,
            protocol_violation,
        } = streamed
        else {
            panic!("{violation:?}: {streamed:?}");
        };
        let expected_id = started.then(|| "ex-0000000000000001".to_string());
        assert_eq!(execution_id, expected_id, "{violation:?}");
        let code = if violation == Violation::EndsEarly {
            tonic::Code::Unknown
        } else {
            tonic::Code::Internal
        };
        assert_eq!(error.code, code, "{violation:?}: {error}");
        assert_eq!(error.reason, None, "{violation:?}");
        // A clean end without ExecFinished is a broken stream, not a
        // broken protocol.
        assert_eq!(
            protocol_violation,
            violation != Violation::EndsEarly,
            "{violation:?}"
        );
        assert_eq!((seen.chunks, seen.gaps), (chunks, gaps), "{violation:?}");
        let _ = shutdown.send(());
    }
}

/// Run one `exec` session against a stub that breaks the stream as
/// `violation` says, and return the harness and how the session ended.
async fn session_with_violation(
    violation: Violation,
) -> (
    Harness<TonicKilnd, ScriptedProvider, MemoryAudit, MemoryJournal>,
    Result<kiln_harness::engine::SessionOutcome, HarnessError>,
) {
    let temp = common::private_tempdir().expect("tempdir");
    let socket = temp.path().join("kilnd.sock");
    let stub = Stub {
        health_answers: u32::MAX,
        current: true,
        stream_stdout: b"abc".to_vec(),
        stream_violation: Some(violation),
        ..Stub::default()
    };
    let shutdown = serve(stub, &socket);
    let api = TonicKilnd::connect(socket).await.expect("connect");
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "call-1",
            "exec",
            json!({"argv": ["/bin/true"]}),
        )]),
        ModelResponse::final_text("done"),
    ]);
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );
    let ended = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await;
    let _ = shutdown.send(());
    (harness, ended)
}

#[tokio::test]
async fn a_protocol_violation_ends_the_session_and_is_never_a_result() {
    // M4-12: whatever rule kilnd broke, before or after naming the
    // execution, the call is never an ordinary result. The session ends as
    // unusable with a fixed message, the execution is evidence (from the
    // stream, or found by recovery when the stream named none), and the
    // model-visible result names no execution.
    for violation in [
        Violation::SeqGap,
        Violation::NotStartedFirst,
        Violation::IdMismatch,
        Violation::AfterFinished,
        Violation::UnnamedStream,
        Violation::RegressingOffset,
        Violation::OverlappingOffset,
        Violation::HoleWithoutGap,
        Violation::GapAtWrongOffset,
        Violation::WrongFinalTotals,
    ] {
        let (harness, ended) = session_with_violation(violation).await;
        let Err(error) = ended else {
            panic!("{violation:?}: a broken protocol ends the session");
        };
        assert!(
            matches!(&error, HarnessError::Api(error)
                if error.code == tonic::Code::FailedPrecondition
                    && error.message == STREAM_PROTOCOL_VIOLATION),
            "{violation:?}: {error}"
        );
        let executed: Vec<_> = harness
            .journal()
            .records
            .iter()
            .filter_map(|record| match &record.event {
                JournalEvent::ToolExecuted {
                    ok,
                    result,
                    execution_ids,
                    ..
                } => Some((*ok, result.clone(), execution_ids.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            executed.len(),
            1,
            "{violation:?}: the model is not asked again"
        );
        let (ok, result, ids) = &executed[0];
        assert!(!ok, "{violation:?}: {result}");
        assert_eq!(
            result["message"], STREAM_PROTOCOL_VIOLATION,
            "{violation:?}"
        );
        assert!(result.get("stdout").is_none(), "{violation:?}: {result}");
        assert!(
            !result.to_string().contains("ex-"),
            "{violation:?}: {result}"
        );
        assert_eq!(ids, &["ex-0000000000000002".to_string()], "{violation:?}");
    }

    // An offset can only overflow after a legal gap past 8 MiB, so in a
    // session the cutoff stops the stream first: the row is the result,
    // marked stopped.
    let (harness, ended) = session_with_violation(Violation::OffsetOverflow).await;
    ended.expect("the cutoff comes first");
    assert!(harness.journal().records.iter().any(|record| matches!(
        &record.event,
        JournalEvent::ToolExecuted { result, .. } if result["stopped"] == "output_limit"
    )));

    // A stream that just ends early broke no rule: the row is the result.
    let (harness, ended) = session_with_violation(Violation::EndsEarly).await;
    ended.expect("an early end is a broken stream, not a broken protocol");
    assert!(harness.journal().records.iter().any(|record| matches!(
        &record.event,
        JournalEvent::ToolExecuted { execution_ids, ok: true, result, .. }
            if execution_ids == &["ex-0000000000000002".to_string()]
                && result["stdout"] == "abc"
    )));
}

#[tokio::test]
async fn regressing_offsets_break_the_stream_before_they_count() {
    // M4-07 on the wire: a chunk that starts again at offset 0 would make
    // bytes count twice, or hide a flood from the cutoff. The adapter drops
    // the stream at it, the session ends, and the live evidence holds only
    // the bytes the stream carried by the rules.
    let (harness, ended) = session_with_violation(Violation::RegressingOffset).await;
    ended.expect_err("a broken protocol ends the session");
    let live = harness
        .audit()
        .records
        .iter()
        .find_map(|record| match &record.event {
            kiln_harness::audit::AuditEvent::ToolCompleted { live, .. } => live.clone(),
            _ => None,
        })
        .expect("live evidence");
    assert_eq!(live.stdout.delivered_bytes, 3);
    assert!(!live.stdout.dropped_bytes_known);
    assert_eq!(live.stopped, None);
}

/// A model that calls `exec` once, then answers.
fn exec_then_done() -> Vec<Reply> {
    let usage = json!({"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120});
    let call = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": {"name": "exec", "arguments": "{\"argv\":[\"/bin/cat\"]}"},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": usage,
    });
    let done = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop",
        }],
        "usage": usage,
    });
    vec![Reply::json(200, &call), Reply::json(200, &done)]
}

#[test]
fn the_cli_prints_live_output_sanitized_only_with_live() {
    // Escape sequences, a carriage return, a bell, NUL, invalid UTF-8 and
    // a line the stub splits across two chunks.
    let hostile = b"plain\n\x1b]0;owned\x07\x1b[2J\rover\0\xff\xc3\xa9 tail\nlast".to_vec();
    for live in [true, false] {
        let temp = common::private_tempdir().expect("tempdir");
        let stub = Stub {
            health_answers: u32::MAX,
            stream_stdout: hostile.clone(),
            ..Stub::default()
        };
        let servers = Servers::start(temp.path(), stub, exec_then_done());
        let mut command = servers.command(temp.path(), "60");
        if live {
            command.arg("--live");
        }
        let child = command.spawn().expect("start the CLI");
        let output = exit_within(child, Duration::from_secs(30)).expect("the CLI exits");
        let stderr = String::from_utf8(output.stderr).expect("stderr is ASCII");
        assert!(output.status.success(), "{stderr}");
        let lines: Vec<&str> = stderr
            .lines()
            .filter(|line| line.starts_with("[call-1-1"))
            .collect();
        if live {
            assert_eq!(
                lines,
                [
                    "[call-1-1 out] plain",
                    "[call-1-1 out] \\x1b]0;owned\\x07\\x1b[2J\\x0dover\\x00\\xff\\xc3\\xa9 tail",
                    "[call-1-1 out] last",
                ],
                "{stderr}"
            );
        } else {
            assert!(lines.is_empty(), "{stderr}");
        }
        // No raw control byte reaches the terminal either way.
        assert!(
            !stderr
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\n' && byte != b'\t'),
            "{stderr:?}"
        );
        // Live output is host-only: the journal holds the same result.
        let journal = std::fs::read_to_string(temp.path().join("out/run.jsonl")).expect("journal");
        assert!(journal.contains("\"exec_context\":\"workspace_stream_v1\""));
    }
}

/// `eval run` with a stdout nobody reads: the first progress line fails,
/// the suite stops, and the runs still in flight are canceled and clean up,
/// so every run that started has its line, and nothing panics.
#[test]
fn eval_run_with_a_closed_stdout_stops_and_cleans_up_every_run() {
    let temp = common::private_tempdir().expect("tempdir");
    let task = temp.path().join("tasks/closed-stdout");
    std::fs::create_dir_all(&task).expect("task directory");
    for (path, contents) in [
        (
            "task.json",
            r#"{"schema": 1, "id": "closed-stdout", "description": "a closed stdout", "check_timeout_secs": 10}"#,
        ),
        ("prompt.md", "Say done.\n"),
        ("check.sh", "exit 0\n"),
        ("solution.sh", "true\n"),
    ] {
        std::fs::write(task.join(path), contents).expect("task file");
    }
    let stub = Stub {
        health_answers: u32::MAX,
        ..Stub::default()
    };
    let creates = Arc::clone(&stub.create_requests);
    let deleted = Arc::clone(&stub.deleted);
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
    // One session answers; the other two wait for a model that never does.
    let servers = Servers::start(
        temp.path(),
        stub,
        vec![Reply::json(200, &completion), Reply::Hang, Reply::Hang],
    );
    let out = temp.path().join("out/eval");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .arg("--socket")
        .arg(&servers.socket)
        .args(["eval", "run", "--provider", "openai-compat"])
        .args(["--base-url", &servers.base_url, "--model", "test-model"])
        .arg("--tasks")
        .arg(temp.path().join("tasks"))
        .args(["--trials", "3", "--concurrency", "3", "--out"])
        .arg(&out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    // Nobody reads stdout: its first write fails.
    drop(child.stdout.take());
    let output = exit_within(child, Duration::from_secs(60)).expect("the CLI exits");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        stderr.contains("stopped early: progress output: "),
        "{stderr}"
    );

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("report.json")).expect("report"))
            .expect("report JSON");
    // openai-compat has no effort or thinking clearing; the output limit
    // is the default.
    assert!(report.get("effort").is_none(), "{report}");
    assert!(report.get("clear_thinking").is_none(), "{report}");
    assert_eq!(report["max_output_tokens"], 16_384, "{report}");
    let stopped = report["stopped"].as_str().expect("stopped");
    assert!(stopped.starts_with("progress output: "), "{stopped}");
    let lines = std::fs::read_to_string(out.join("results.jsonl")).expect("results");
    let outcomes = lines
        .lines()
        .map(|line| {
            let line: serde_json::Value = serde_json::from_str(line).expect("result line");
            line["outcome"].as_str().expect("outcome").to_string()
        })
        .collect::<Vec<_>>();
    // Every run that started has its line; the two that waited were
    // canceled and none was left without cleanup.
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| *outcome == "canceled")
            .count(),
        2,
        "{outcomes:?}"
    );
    assert!(!outcomes.iter().any(|outcome| outcome == "cleanup_failed"));
    assert_eq!(creates.lock().expect("creates").len(), 3);
    assert!(deleted.load(Ordering::SeqCst) >= 3);
}

/// `eval run --provider anthropic` records the effort it sent, its thinking
/// clearing, and the output limit per response in the manifest and both
/// reports.
#[test]
fn eval_run_records_the_effort_clearing_and_output_limit() {
    for (clearing, recorded) in [(&[][..], "off"), (&["--clear-thinking", "2"][..], "keep 2")] {
        let temp = common::private_tempdir().expect("tempdir");
        let task = temp.path().join("tasks/say-done");
        std::fs::create_dir_all(&task).expect("task directory");
        for (path, contents) in [
            (
                "task.json",
                r#"{"schema": 1, "id": "say-done", "description": "say done", "check_timeout_secs": 10}"#,
            ),
            ("prompt.md", "Say done.\n"),
            ("check.sh", "exit 0\n"),
            ("solution.sh", "true\n"),
        ] {
            std::fs::write(task.join(path), contents).expect("task file");
        }
        let stub = Stub {
            health_answers: u32::MAX,
            ..Stub::default()
        };
        let message = json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-5-5",
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn",
            "stop_details": null,
            "usage": common::anthropic::live_usage(100, 20, 0, 0),
        });
        let servers = Servers::start(temp.path(), stub, vec![Reply::json(200, &message)]);
        let out = temp.path().join("out/eval");
        let child = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
            .arg("--socket")
            .arg(&servers.socket)
            .args(["eval", "run", "--provider", "anthropic"])
            .args([
                "--base-url",
                &servers.base_url,
                "--model",
                "claude-haiku-5-5",
            ])
            .args(["--effort", "low", "--max-output-tokens", "2048"])
            .args(clearing)
            .arg("--tasks")
            .arg(temp.path().join("tasks"))
            .arg("--out")
            .arg(&out)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the CLI");
        let output = exit_within(child, Duration::from_secs(60)).expect("the CLI exits");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");

        for file in ["manifest.json", "report.json"] {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(out.join(file)).expect(file)).expect(file);
            assert_eq!(value["effort"], "low", "{file}: {value}");
            assert_eq!(value["clear_thinking"], recorded, "{file}: {value}");
            assert_eq!(value["max_output_tokens"], 2048, "{file}: {value}");
        }
        let markdown = std::fs::read_to_string(out.join("report.md")).expect("report.md");
        assert!(
            markdown.contains(&format!(
                "- Effort: low\n- Clear thinking: {recorded}\n- Output limit per response: 2048 \
                 tokens\n"
            )),
            "{markdown}"
        );
    }
}
