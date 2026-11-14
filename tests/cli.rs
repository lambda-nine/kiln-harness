//! Tests that drive the `kiln-harness` binary.
mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::{DeleteBehavior, FakeApi, chained_lines, short_lifecycle_config, write_journal_lines};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionTask};
use kiln_harness::journal::{
    JOURNAL_SCHEMA, JournalEvent, JsonlJournal, read_journal, request_sha256,
};
use kiln_harness::provider::{Message, ModelRequest, ProviderInfo, ToolCall, Usage};
use kiln_harness::provider::{ModelResponse, ScriptedProvider};
use serde_json::json;
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
    let temp = common::private_tempdir().expect("tempdir");
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
        Some(AuditEvent::SessionFinished { outcome, work_outcome, sandbox_deleted: false, .. })
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
    let temp = common::private_tempdir().expect("tempdir");
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

fn started() -> JournalEvent {
    JournalEvent::SessionStarted {
        schema: JOURNAL_SCHEMA,
        harness_version: "0.1.0".to_string(),
        provider: ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        },
        system: "system".to_string(),
        task: "task".to_string(),
        tools: Vec::new(),
        limits: HarnessConfig::default().limits(),
        seed: Vec::new(),
        check: None,
        exec_context: None,
        compaction: None,
    }
}

fn completed(steps: u32) -> JournalEvent {
    JournalEvent::SessionEnded {
        outcome: "completed".to_string(),
        work_outcome: "completed".to_string(),
        final_answer: Some("done".to_string()),
        steps,
        tool_calls: 0,
        usage: Usage::default(),
        sandbox_deleted: true,
        truncation_recoveries: 0,
        compactions: 0,
    }
}

#[test]
fn verify_rejects_a_complete_journal_with_a_request_after_the_final_answer() {
    // Turn 1 is a valid request, so the refusal comes from turn 2.
    let messages = vec![Message::User {
        text: "task".to_string(),
    }];
    let request = |turn| JournalEvent::ModelRequested {
        turn,
        request_sha256: request_sha256(&ModelRequest {
            session_id: "hs-pending",
            turn,
            system: "system",
            tools: &[],
            messages: &messages,
            max_output_tokens: HarnessConfig::default().max_output_tokens_per_request,
        }),
        message_count: 1,
    };
    let lines = chained_lines(
        "hs-pending",
        vec![
            started(),
            JournalEvent::SandboxReady {
                sandbox_id: "sb-1".to_string(),
                baseline_file_count: 0,
                expiry_lifetime_ms: None,
                expiry_idle_ms: None,
            },
            request(1),
            JournalEvent::ModelResponded {
                turn: 1,
                response: ModelResponse::final_text("done"),
                latency_ms: 0,
            },
            request(2),
            completed(2),
        ],
    );
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("pending.jsonl");
    write_journal_lines(&path, &lines);

    let (ok, stdout) = verify(&path);

    assert!(!ok, "{stdout}");
    assert!(read_journal(&path).is_err());
}

#[test]
fn verify_rejects_tools_before_the_sandbox_or_their_response() {
    let ghost = JournalEvent::ToolExecuted {
        turn: 99,
        index: 7,
        audit_call_id: "call-99-7".to_string(),
        call: ToolCall::new("ghost", "workspace_diff", json!({})),
        ok: true,
        result: json!({"ok": true}),
        execution_ids: vec!["ex-ghost".to_string()],
    };
    let lines = chained_lines("hs-ghost", vec![started(), ghost, completed(0)]);
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("ghost.jsonl");
    write_journal_lines(&path, &lines);

    let (ok, stdout) = verify(&path);

    assert!(!ok, "{stdout}");
}

async fn recorded_demo(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("source.jsonl");
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
    path
}

fn replay_cli(dir: &Path, from: &Path, audit: &Path, journal: &Path) -> Output {
    cli(&[
        "--socket".as_ref(),
        dir.join("no-kilnd.sock").as_os_str(),
        "replay".as_ref(),
        "--from".as_ref(),
        from.as_os_str(),
        "--audit".as_ref(),
        audit.as_os_str(),
        "--journal".as_ref(),
        journal.as_os_str(),
    ])
}

#[tokio::test]
async fn replay_refuses_outputs_that_alias_its_source_before_contacting_kilnd() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let source = recorded_demo(dir).await;
    let original = std::fs::read(&source).expect("read source");
    let hard_link = dir.join("hard-link.jsonl");
    std::fs::hard_link(&source, &hard_link).expect("hard link");
    let symlink = dir.join("symlink.jsonl");
    std::os::unix::fs::symlink(&source, &symlink).expect("symlink");
    common::dir_all_with_mode(&dir.join("sub"), 0o700);
    let dotted = dir.join("sub/../source.jsonl");
    let fresh_journal = dir.join("replay.jsonl");
    let fresh_audit = dir.join("replay-audit.jsonl");

    for (name, audit, journal) in [
        ("audit is the source", source.clone(), fresh_journal.clone()),
        (
            "audit is a hard link",
            hard_link.clone(),
            fresh_journal.clone(),
        ),
        ("audit is a symlink", symlink.clone(), fresh_journal.clone()),
        ("journal is the source", fresh_audit.clone(), dotted.clone()),
        (
            "audit and journal are one new file",
            dir.join("new/out.jsonl"),
            dir.join("new/./x/../out.jsonl"),
        ),
    ] {
        let output = replay_cli(dir, &source, &audit, &journal);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success(), "{name}: {stderr}");
        assert!(
            stderr.contains("refer to the same file"),
            "{name}: {stderr}"
        );
        assert_eq!(
            std::fs::read(&source).expect("read source"),
            original,
            "{name}: source changed"
        );
        assert!(read_journal(&source).is_ok(), "{name}");
        assert!(!dir.join("new").exists(), "{name}: created an output");
    }

    // Distinct paths pass the check and fail only on the missing socket.
    let output = replay_cli(dir, &source, &fresh_audit, &fresh_journal);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(!stderr.contains("same file"), "{stderr}");
    assert!(stderr.contains("connect"), "{stderr}");
    assert_eq!(std::fs::read(&source).expect("read source"), original);
}

#[test]
fn demo_refuses_an_audit_that_is_its_journal() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let output = cli(&[
        "--socket".as_ref(),
        dir.join("no-kilnd.sock").as_os_str(),
        "demo".as_ref(),
        "--audit".as_ref(),
        dir.join("out.jsonl").as_os_str(),
        "--journal".as_ref(),
        dir.join("./out.jsonl").as_os_str(),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("--audit and --journal refer to the same file"),
        "{stderr}"
    );
    assert!(!dir.join("out.jsonl").exists());
}

#[tokio::test]
async fn verify_rejects_a_tool_record_that_differs_from_the_model_call() {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("recorded.jsonl");
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::new([
            ModelResponse::tool_use([ToolCall::new("call", "workspace_diff", json!({}))]),
            ModelResponse::final_text("done"),
        ]),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("recording");
    let recorded = read_journal(&path).expect("recording verifies");
    let (ok, _) = verify(&path);
    assert!(ok);

    // Same call ID, a different call and result, rechained.
    let events = recorded
        .records
        .iter()
        .map(|record| {
            let mut event = record.event.clone();
            if let JournalEvent::ToolExecuted { call, result, .. } = &mut event {
                *call = ToolCall::new("call", "exec", json!({"argv": ["id"]}));
                *result = json!({"ok": true, "stdout": "fabricated"});
            }
            event
        })
        .collect();
    let forged = temp.path().join("forged.jsonl");
    write_journal_lines(&forged, &chained_lines(recorded.session_id(), events));

    let (ok, stdout) = verify(&forged);

    assert!(!ok, "{stdout}");
}

#[test]
fn a_permissive_umask_cannot_widen_what_the_harness_creates() {
    use std::os::unix::fs::PermissionsExt;

    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let journal = dir.join("new/deeper/journal.jsonl");
    let audit = dir.join("audit-dir/nested/audit.jsonl");
    // demo opens both outputs before contacting kilnd, then fails to connect.
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 0000; exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_kiln-harness"))
        .arg("--socket")
        .arg(dir.join("no-kilnd.sock"))
        .arg("demo")
        .arg("--audit")
        .arg(&audit)
        .arg("--journal")
        .arg(&journal)
        .output()
        .expect("run demo under umask 0000");
    assert!(!output.status.success(), "there is no kilnd to connect to");

    let mode = |path: &Path| {
        std::fs::metadata(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(&journal), 0o600);
    assert_eq!(mode(&dir.join("new")), 0o700);
    assert_eq!(mode(&dir.join("new/deeper")), 0o700);
    assert_eq!(mode(&audit), 0o600);
    assert_eq!(mode(&dir.join("audit-dir")), 0o700);
    assert_eq!(mode(&dir.join("audit-dir/nested")), 0o700);
}

#[tokio::test]
async fn verify_rejects_completion_beyond_the_recorded_step_limit() {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("recorded.jsonl");
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::new([
            ModelResponse::tool_use([ToolCall::new("call", "workspace_diff", json!({}))]),
            ModelResponse::final_text("done"),
        ]),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("two-turn recording");
    let recorded = read_journal(&path).expect("recording verifies");

    // Only max_steps changes; no request hash depends on it.
    let mut events = recorded
        .records
        .iter()
        .map(|record| record.event.clone())
        .collect::<Vec<_>>();
    let JournalEvent::SessionStarted { limits, .. } = &mut events[0] else {
        panic!("session start");
    };
    limits.max_steps = 1;
    let forged = temp.path().join("forged.jsonl");
    write_journal_lines(&forged, &chained_lines(recorded.session_id(), events));

    let (ok, stdout) = verify(&forged);

    assert!(!ok, "{stdout}");
}
