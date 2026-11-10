//! Tests that drive the `kiln-harness` binary.
mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::{DeleteBehavior, FakeApi, short_lifecycle_config};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, read_journal};
use kiln_harness::provider::{ModelResponse, ScriptedProvider};
use tokio_util::sync::CancellationToken;

fn cli(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(args)
        .output()
        .expect("run kiln-harness")
}

fn verify(path: &Path) -> (bool, String) {
    let output = cli(&["journal".as_ref(), "verify".as_ref(), path.as_os_str()]);
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[tokio::test]
async fn unconfirmed_cleanup_is_recorded_and_printed_as_cleanup_failed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("cleanup-failed.jsonl");
    let api = FakeApi::default();
    api.state().delete_behavior = DeleteBehavior::AcknowledgeOnly;
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_millis(25);
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        config,
    );

    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("sandbox cleanup is unconfirmed");

    assert!(matches!(error, HarnessError::Cleanup(_)));
    assert!(matches!(
        harness.audit().records.last().map(|record| &record.event),
        Some(AuditEvent::SessionFinished { outcome, work_outcome, sandbox_deleted: false })
            if outcome == "cleanup_failed" && work_outcome == "completed"
    ));
    let journal = read_journal(&path).expect("journal verifies");
    assert!(matches!(
        journal.records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded {
            outcome,
            work_outcome,
            sandbox_deleted: false,
            ..
        }) if outcome == "cleanup_failed" && work_outcome == "completed"
    ));
    let (ok, stdout) = verify(&path);
    assert!(ok, "{stdout}");
    assert!(
        stdout.contains("status: complete (cleanup_failed)"),
        "{stdout}"
    );
    assert!(stdout.contains("sandbox deleted: no"), "{stdout}");
    assert!(!stdout.contains("completed"), "{stdout}");
}

#[tokio::test]
async fn verify_prints_confirmed_deletion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("demo.jsonl");
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("demo succeeds");

    let (ok, stdout) = verify(&path);

    assert!(ok, "{stdout}");
    assert!(stdout.contains("status: complete (completed)"), "{stdout}");
    assert!(stdout.contains("sandbox deleted: yes"), "{stdout}");
}
