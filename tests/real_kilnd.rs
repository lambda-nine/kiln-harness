use std::path::PathBuf;

use kiln_harness::api::{SandboxApi, TonicKilnd};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::engine::{Harness, HarnessConfig, SessionTask};
use kiln_harness::journal::{JsonlJournal, read_journal};
use kiln_harness::provider::ScriptedProvider;
use kiln_harness::replay::ReplayProvider;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn scripted_provider_through_installed_kilnd_leaves_no_api_residue() {
    if std::env::var_os("KILND_INTEGRATION").is_none() {
        return;
    }
    let socket = std::env::var_os("KILND_SOCKET")
        .map_or_else(|| PathBuf::from("/run/kilnd/kilnd.sock"), PathBuf::from);
    let api = TonicKilnd::connect(socket)
        .await
        .expect("connect to installed kilnd");
    let before = api.health().await.expect("health before");
    assert!(before.serving, "kilnd unhealthy: {}", before.detail);
    assert_eq!(
        before.active_sandboxes, 0,
        "integration baseline is not empty"
    );
    assert_eq!(before.running_executions, 0);
    assert_eq!(before.reservations.sandboxes, 0);
    assert!(
        api.non_deleted_sandboxes()
            .await
            .expect("list baseline")
            .is_empty()
    );

    let temp = tempfile::tempdir().expect("temporary audit directory");
    let audit_path = temp.path().join("integration.jsonl");
    let audit = JsonlAudit::open(&audit_path).expect("open audit");
    let journal_path = temp.path().join("integration-journal.jsonl");
    let journal = JsonlJournal::open(&journal_path).expect("open journal");
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        audit,
        journal,
        HarnessConfig::default(),
    );
    let outcome = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("real kilnd scenario succeeds");
    assert_eq!(outcome.tool_calls, 5);
    let records = std::fs::read_to_string(&audit_path).expect("read flushed audit");
    let completed = records
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid audit JSON"))
        .filter(|record| record.get("event").and_then(Value::as_str) == Some("tool_completed"))
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 5);
    assert!(completed.iter().all(|record| {
        record.get("ok").and_then(Value::as_bool) == Some(true)
            && record
                .get("execution_ids")
                .and_then(Value::as_array)
                .is_some_and(|ids| !ids.is_empty())
    }));

    // The recording replays against a fresh sandbox with identical requests.
    let recorded = read_journal(&journal_path).expect("journal verifies");
    assert!(recorded.complete);
    let provider = ReplayProvider::from_journal(&recorded).expect("replayable journal");
    let task = provider.task();
    let mut config = HarnessConfig::default();
    config.apply_limits(&provider.limits());
    let mut replay = Harness::new(
        api.clone(),
        provider,
        JsonlAudit::open(&temp.path().join("replay.jsonl")).expect("open replay audit"),
        JsonlJournal::open(&temp.path().join("replay-journal.jsonl")).expect("open replay journal"),
        config,
    );
    let replayed = replay
        .run(&task, CancellationToken::new())
        .await
        .expect("replay reproduces against installed kilnd");
    assert_eq!(replay.provider().diverged_at(), None);
    assert_eq!(replayed.final_answer, outcome.final_answer);

    let after = api.health().await.expect("health after");
    assert!(after.serving, "kilnd unhealthy: {}", after.detail);
    assert_eq!(after.active_sandboxes, 0);
    assert_eq!(after.running_executions, 0);
    assert_eq!(after.reservations.sandboxes, 0);
    assert_eq!(after.reservations.memory_bytes, 0);
    assert_eq!(after.reservations.pids, 0);
    assert_eq!(after.reservations.disk_bytes, 0);
    assert!(
        api.non_deleted_sandboxes()
            .await
            .expect("list after")
            .is_empty()
    );
}
