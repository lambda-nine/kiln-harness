//! A replay output relinked to the source after the path checks must not
//! reach the source: outputs are opened, and compared by descriptor, before
//! kilnd is contacted.
mod common;

use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::MemoryAudit;
use kiln_harness::engine::{Harness, SessionTask};
use kiln_harness::journal::{JsonlJournal, read_journal};
use kiln_harness::pb;
use kiln_harness::pb::kilnd_service_server::{KilndService, KilndServiceServer};
use kiln_harness::provider::ScriptedProvider;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// Answers health only after the test releases it; everything else fails.
struct GatedKilnd {
    health_entered: mpsc::Sender<()>,
    release_health: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl KilndService for GatedKilnd {
    async fn health(
        &self,
        _request: Request<pb::HealthRequest>,
    ) -> Result<Response<pb::HealthResponse>, Status> {
        let _ = self.health_entered.send(());
        self.release_health.notified().await;
        Ok(Response::new(pb::HealthResponse {
            status: pb::health_response::Status::Serving as i32,
            ..pb::HealthResponse::default()
        }))
    }

    async fn create_sandbox(
        &self,
        _request: Request<pb::CreateSandboxRequest>,
    ) -> Result<Response<pb::CreateSandboxResponse>, Status> {
        Err(Status::unimplemented("test stub"))
    }

    async fn get_sandbox(
        &self,
        _request: Request<pb::GetSandboxRequest>,
    ) -> Result<Response<pb::GetSandboxResponse>, Status> {
        Err(Status::unimplemented("test stub"))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<pb::ListSandboxesRequest>,
    ) -> Result<Response<pb::ListSandboxesResponse>, Status> {
        Err(Status::unimplemented("test stub"))
    }

    async fn delete_sandbox(
        &self,
        _request: Request<pb::DeleteSandboxRequest>,
    ) -> Result<Response<pb::DeleteSandboxResponse>, Status> {
        Err(Status::unimplemented("test stub"))
    }

    async fn exec(
        &self,
        _request: Request<pb::ExecRequest>,
    ) -> Result<Response<pb::ExecResponse>, Status> {
        Err(Status::unimplemented("test stub"))
    }

    async fn get_execution(
        &self,
        _request: Request<pb::GetExecutionRequest>,
    ) -> Result<Response<pb::GetExecutionResponse>, Status> {
        Err(Status::unimplemented("test stub"))
    }
}

#[tokio::test]
async fn relinking_an_output_to_the_source_after_the_checks_cannot_touch_it() {
    use tonic::codegen::tokio_stream::wrappers::UnixListenerStream;

    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source.jsonl");
    let mut recording = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        JsonlJournal::open(&source).expect("journal"),
        short_lifecycle_config(),
    );
    recording
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("recording");
    drop(recording);
    let original = std::fs::read(&source).expect("read source");
    let audit = temp.path().join("audit.jsonl");
    let output_journal = temp.path().join("replay.jsonl");
    let socket = temp.path().join("kilnd.sock");

    let (ready_tx, ready_rx) = mpsc::channel();
    let (health_tx, health_rx) = mpsc::channel();
    let release_health = Arc::new(tokio::sync::Notify::new());
    let server_release = release_health.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::UnixListener::bind(&server_socket).expect("bind socket");
            ready_tx.send(()).expect("ready");
            tonic::transport::Server::builder()
                .serve_with_incoming_shutdown(
                    KilndServiceServer::new(GatedKilnd {
                        health_entered: health_tx,
                        release_health: server_release,
                    }),
                    UnixListenerStream::new(listener),
                    async {
                        let _ = shutdown_rx.await;
                    },
                )
                .await
                .expect("serve");
        });
    });
    ready_rx.recv().expect("server ready");

    let mut child = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .arg("--socket")
        .arg(&socket)
        .arg("replay")
        .arg("--from")
        .arg(&source)
        .arg("--audit")
        .arg(&audit)
        .arg("--journal")
        .arg(&output_journal)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start replay");

    // The CLI is waiting on kilnd; its outputs are already open. Relink the
    // audit path to the source, as the original race did.
    health_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the CLI reached kilnd");
    assert!(
        audit.exists(),
        "outputs are opened before kilnd is contacted"
    );
    std::fs::remove_file(&audit).expect("unlink the opened audit");
    std::fs::hard_link(&source, &audit).expect("relink the audit path to the source");
    release_health.notify_one();

    // Once the replay journal has its first record, the audit has written
    // its own first record too.
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&output_journal).map_or(0, |meta| meta.len()) == 0 {
        assert!(Instant::now() < deadline, "replay never started");
        if let Some(status) = child.try_wait().expect("child state") {
            panic!("replay exited early: {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.kill().expect("stop replay");
    let _ = child.wait();
    let _ = shutdown_tx.send(());
    server.join().expect("join server");

    assert_eq!(std::fs::read(&source).expect("read source"), original);
    read_journal(&source).expect("the source still verifies");
}
