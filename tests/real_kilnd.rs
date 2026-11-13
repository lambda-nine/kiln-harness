mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kiln_harness::api::{
    ExpiryPolicy, Feature, LiveStream, SandboxApi, SandboxRequest, TonicKilnd,
};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::engine::{Harness, HarnessConfig, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, read_journal};
use kiln_harness::live::LiveOutput;
use kiln_harness::provider::{ModelResponse, ScriptedProvider, ToolCall};
use kiln_harness::replay::ReplayProvider;
use serde_json::{Value, json};
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
    // kilnd 0c79944 lists expiry, typed reasons, idempotent create and
    // request-ID release, so the harness sends an expiry and a request ID,
    // reads reason trailers, and releases the key at cleanup. Its backend
    // proves exec cwd and env, which every session needs.
    assert!(before.capabilities.supports(Feature::ExecCwd));
    assert!(before.capabilities.supports(Feature::ExecEnv));
    assert!(before.capabilities.supports(Feature::ExecStream));
    assert!(before.capabilities.supports(Feature::SandboxExpiry));
    assert!(before.capabilities.supports(Feature::TypedReasons));
    assert!(before.capabilities.supports(Feature::IdempotentCreate));
    assert!(before.capabilities.supports(Feature::RequestIdRelease));
    // A node may keep sandboxes this test does not own, such as a failed one
    // kilnd still charges because its absence is unproven. Compare against
    // what existed before rather than against zero.
    let sandboxes_before = non_deleted_ids(&api).await;

    let temp = common::private_tempdir().expect("temporary audit directory");
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
    check_expiry_and_reasons(&api, &outcome.sandbox_id, &audited_request_id(&records)).await;
    let completed = records
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid audit JSON"))
        .filter(|record| record.get("event").and_then(Value::as_str) == Some("tool_completed"))
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 5);
    check_audited_expiry(&records);
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
    let config = provider.replay_config(HarnessConfig::default());
    let mut replay = Harness::new(
        api.clone(),
        provider,
        JsonlAudit::open(&temp.path().join("replay.jsonl")).expect("open replay audit"),
        JsonlJournal::open(&temp.path().join("replay-journal.jsonl")).expect("open replay journal"),
        config,
    );
    let replayed = replay
        .replay(CancellationToken::new())
        .await
        .expect("replay reproduces against installed kilnd");
    assert_eq!(replay.provider().diverged_at(), None);
    assert!(replay.provider().exhausted());
    assert_eq!(replayed.final_answer, outcome.final_answer);

    check_keyed_create(&api).await;

    let after = api.health().await.expect("health after");
    assert!(after.serving, "kilnd unhealthy: {}", after.detail);
    assert_eq!(after.active_sandboxes, 0);
    assert_eq!(after.running_executions, 0);
    assert_eq!(after.reservations, before.reservations);
    assert_eq!(non_deleted_ids(&api).await, sandboxes_before);
}

#[tokio::test]
async fn tools_run_in_the_workspace_with_the_fixed_env_through_installed_kilnd() {
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
    assert_eq!(before.active_sandboxes, 0);
    let sandboxes_before = non_deleted_ids(&api).await;

    // A relative write_file path and a relative read from the shell both
    // resolve under /workspace; the shell sees kiln's HOME and the three
    // overrides, with TERM replacing kiln's xterm.
    let probe =
        "pwd; printf '%s|%s|%s|%s\\n' \"$TERM\" \"$NO_COLOR\" \"$PAGER\" \"$HOME\"; cat probe.txt";
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "call-1",
            "write_file",
            json!({"path": "probe.txt", "content": "relative\n"}),
        )]),
        ModelResponse::tool_use([ToolCall::new(
            "call-2",
            "exec",
            json!({"argv": ["/bin/sh", "-c", probe]}),
        )]),
        ModelResponse::final_text("probed"),
    ]);
    let temp = common::private_tempdir().expect("temporary directory");
    let journal_path = temp.path().join("probe-journal.jsonl");
    let mut harness = Harness::new(
        api.clone(),
        provider,
        JsonlAudit::open(&temp.path().join("probe-audit.jsonl")).expect("open audit"),
        JsonlJournal::open(&journal_path).expect("open journal"),
        HarnessConfig::default(),
    );
    // Completion also means every returned execution row showed the cwd,
    // env names and env digest the harness sent.
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("the probe session completes");

    let recorded = read_journal(&journal_path).expect("journal verifies");
    assert!(recorded.complete);
    let JournalEvent::SessionStarted { exec_context, .. } = &recorded.records[0].event else {
        panic!("session start");
    };
    assert_eq!(exec_context.as_deref(), Some("workspace_stream_v1"));
    let stdout = recorded
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ToolExecuted { call, result, .. } if call.tool == "exec" => result
                .get("stdout")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .expect("the exec result");
    assert_eq!(stdout, "/workspace\ndumb|1|cat|/root\nrelative\n");

    let after = api.health().await.expect("health after");
    assert_eq!(after.active_sandboxes, 0);
    assert_eq!(after.running_executions, 0);
    assert_eq!(after.reservations, before.reservations);
    assert_eq!(non_deleted_ids(&api).await, sandboxes_before);
}

/// Every live event of a session, with when it arrived.
#[derive(Default)]
struct Timeline {
    events: std::sync::Mutex<Vec<(Instant, String, String)>>,
}

impl Timeline {
    fn push(&self, call_id: &str, event: String) {
        self.events
            .lock()
            .expect("timeline")
            .push((Instant::now(), call_id.to_string(), event));
    }

    fn of(&self, call_id: &str) -> Vec<(Instant, String)> {
        self.events
            .lock()
            .expect("timeline")
            .iter()
            .filter(|(_, call, _)| call == call_id)
            .map(|(at, _, event)| (*at, event.clone()))
            .collect()
    }
}

impl LiveOutput for Timeline {
    fn started(&self, call_id: &str, _execution_id: &str) {
        self.push(call_id, "started".to_string());
    }

    fn chunk(&self, call_id: &str, stream: LiveStream, data: &[u8]) {
        self.push(
            call_id,
            format!("{} {}", stream.as_str(), String::from_utf8_lossy(data)),
        );
    }

    fn gap(&self, call_id: &str, stream: LiveStream, dropped: u64, known: bool) {
        self.push(
            call_id,
            format!("{} gap {dropped} {known}", stream.as_str()),
        );
    }

    fn finished(&self, call_id: &str, stopped: Option<&str>) {
        self.push(call_id, format!("finished {stopped:?}"));
    }
}

/// The model's `exec` streams through installed kilnd: output written a
/// second apart arrives live, a second apart, before the call finishes; a
/// runaway is cut at the output limit; and the next exec in the same
/// sandbox runs normally.
#[tokio::test]
async fn model_exec_streams_live_and_cuts_a_runaway_through_installed_kilnd() {
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
    assert!(before.capabilities.supports(Feature::ExecStream));
    assert_eq!(before.active_sandboxes, 0);
    let sandboxes_before = non_deleted_ids(&api).await;

    let exec = |id: &str, script: &str| {
        ModelResponse::tool_use([ToolCall::new(
            id,
            "exec",
            json!({"argv": ["/bin/sh", "-c", script]}),
        )])
    };
    let provider = ScriptedProvider::new([
        exec("call-1", "echo first; sleep 1; echo second"),
        exec("call-2", "head -c 9000000 /dev/zero | tr '\\0' x"),
        exec("call-3", "echo after"),
        ModelResponse::final_text("streamed"),
    ]);
    let temp = common::private_tempdir().expect("temporary directory");
    let journal_path = temp.path().join("stream-journal.jsonl");
    let timeline = std::sync::Arc::new(Timeline::default());
    let mut harness = Harness::new(
        api.clone(),
        provider,
        JsonlAudit::open(&temp.path().join("stream-audit.jsonl")).expect("open audit"),
        JsonlJournal::open(&journal_path).expect("open journal"),
        HarnessConfig::default(),
    )
    .with_live_output(std::sync::Arc::clone(&timeline) as std::sync::Arc<dyn LiveOutput>);
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("the stream session completes");
    // The sink is fed by a thread of its own; let it catch up.
    assert!(harness.drain_live_output(Duration::from_secs(5)));

    // Both lines arrived live, about a second apart, before the end.
    let first = timeline.of("call-1-1");
    assert_eq!(
        first.first().map(|(_, event)| event.as_str()),
        Some("started")
    );
    let at = |text: &str| {
        first
            .iter()
            .find(|(_, event)| event.starts_with("stdout") && event.contains(text))
            .map_or_else(|| panic!("no live {text:?}: {first:?}"), |(at, _)| *at)
    };
    let finished = first
        .iter()
        .find(|(_, event)| event.starts_with("finished"))
        .map(|(at, _)| *at)
        .expect("finished");
    assert!(
        at("second") >= at("first") + Duration::from_millis(500),
        "{first:?}"
    );
    assert!(at("second") <= finished);
    assert_eq!(
        first.last().map(|(_, event)| event.as_str()),
        Some("finished None")
    );

    let recorded = read_journal(&journal_path).expect("journal verifies");
    let JournalEvent::SessionStarted { exec_context, .. } = &recorded.records[0].event else {
        panic!("session start");
    };
    assert_eq!(exec_context.as_deref(), Some("workspace_stream_v1"));
    let results: Vec<Value> = recorded
        .records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["stdout"], "first\nsecond\n");
    assert!(results[0].get("stopped").is_none());
    // The runaway was cut, whatever state kilnd settled it in.
    assert_eq!(results[1]["stopped"], "output_limit", "{}", results[1]);
    assert_eq!(
        timeline
            .of("call-2-1")
            .last()
            .map(|(_, event)| event.clone()),
        Some("finished Some(\"output_limit\")".to_string())
    );
    assert_eq!(results[2]["ok"], true, "{}", results[2]);
    assert_eq!(results[2]["stdout"], "after\n");

    let after = api.health().await.expect("health after");
    assert_eq!(after.active_sandboxes, 0);
    assert_eq!(after.running_executions, 0);
    assert_eq!(after.reservations, before.reservations);
    assert_eq!(non_deleted_ids(&api).await, sandboxes_before);
}

async fn non_deleted_ids(api: &TonicKilnd) -> BTreeSet<String> {
    api.non_deleted_sandboxes()
        .await
        .expect("list non-deleted sandboxes")
        .into_iter()
        .map(|sandbox| sandbox.id)
        .collect()
}

/// The session's request ID from its audit, after checking that the
/// session released it at cleanup.
fn audited_request_id(records: &str) -> String {
    let events = records
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid audit JSON"))
        .collect::<Vec<_>>();
    let event = |name: &str| {
        events
            .iter()
            .find(|record| record.get("event").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("no {name}"))
    };
    let request_id = event("sandbox_created")["request_id"]
        .as_str()
        .expect("a keyed create")
        .to_string();
    assert!(request_id.starts_with("kiln-harness:"), "{request_id}");
    assert_eq!(event("session_finished")["request_id_released"], true);
    request_id
}

/// Two keyed creates with one fresh key and equal requests make one sandbox;
/// another request under the key is a conflict; a releasing delete gives
/// the key up. The sandbox is deleted, with retries, whatever happens.
async fn check_keyed_create(api: &TonicKilnd) {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let key = format!("kiln-harness:it-{unix_ms:x}-{:x}", std::process::id());
    let request = SandboxRequest {
        image: "alpine".to_string(),
        memory_bytes: 256 << 20,
        cpu_millis: 500,
        pids: 64,
        disk_bytes: 256 << 20,
        network_none: true,
        read_only_root: true,
        writable_paths: vec!["/workspace".to_string()],
        labels: BTreeMap::from([("owner".to_string(), "kiln-harness-it".to_string())]),
        expiry: Some(ExpiryPolicy {
            max_lifetime_ms: 120_000,
            idle_timeout_ms: 60_000,
        }),
        request_id: Some(key.clone()),
    };
    let mut created = BTreeSet::new();
    let checked = keyed_create_checks(api, &request, &mut created).await;
    let mut residue = Vec::new();
    for sandbox_id in &created {
        if !delete_until_absent(api, sandbox_id).await {
            residue.push(sandbox_id.clone());
        }
    }
    assert!(residue.is_empty(), "keyed create left {residue:?}");
    let released = checked.expect("keyed create checks");
    assert!(released, "release_request_id was never confirmed");
    let row = api
        .get_sandbox(created.first().expect("one sandbox"))
        .await
        .expect("get the deleted sandbox");
    assert_eq!(row.request_id, key);
    assert!(row.request_id_released);
}

/// The checks of [`check_keyed_create`], without panicking, so its caller
/// always cleans up. Every sandbox ID seen goes into `created`. Returns
/// whether a releasing delete reported the key released.
async fn keyed_create_checks(
    api: &TonicKilnd,
    request: &SandboxRequest,
    created: &mut BTreeSet<String>,
) -> Result<bool, String> {
    let first = api
        .create(request.clone())
        .await
        .map_err(|e| e.to_string())?;
    created.insert(first.id.clone());
    let second = api
        .create(request.clone())
        .await
        .map_err(|e| e.to_string())?;
    created.insert(second.id.clone());
    if second.id != first.id {
        return Err(format!("retry made {} beside {}", second.id, first.id));
    }
    if first.request_id != request.request_id.clone().unwrap_or_default() {
        return Err(format!("echoed {:?}", first.request_id));
    }
    let mut other = request.clone();
    other
        .labels
        .insert("variant".to_string(), "other".to_string());
    match api.create(other).await {
        Ok(sandbox) => {
            created.insert(sandbox.id.clone());
            return Err(format!("an unequal request made {}", sandbox.id));
        }
        Err(error)
            if error.code == tonic::Code::AlreadyExists
                && error.reason.as_deref() == Some("request_id_conflict") => {}
        Err(error) => return Err(format!("unequal request: {error}")),
    }
    let mut released = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(deleted) = api.delete(&first.id, true).await {
            released |= deleted.request_id_released;
        }
        let listed = api
            .non_deleted_sandboxes()
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .any(|sandbox| sandbox.id == first.id);
        if !listed && released {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(released)
}

/// Delete `sandbox_id` until a listing no longer shows it, for at most
/// ten seconds.
async fn delete_until_absent(api: &TonicKilnd, sandbox_id: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let _ = api.delete(sandbox_id, true).await;
        if let Ok(sandboxes) = api.non_deleted_sandboxes().await
            && sandboxes.iter().all(|sandbox| sandbox.id != sandbox_id)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn check_expiry_and_reasons(api: &TonicKilnd, sandbox_id: &str, request_id: &str) {
    // The retained row keeps the effective expiry the harness set: 30 s
    // session + 10 s cleanup + 60 s margin, and the 120 s idle default.
    let sandbox = api
        .get_sandbox(sandbox_id)
        .await
        .expect("get the deleted sandbox");
    assert_eq!(
        sandbox.expiry.and_then(|status| status.effective),
        Some(ExpiryPolicy {
            max_lifetime_ms: 100_000,
            idle_timeout_ms: 120_000,
        })
    );
    // The row keeps the session's key, released at cleanup.
    assert_eq!(sandbox.request_id, request_id);
    assert!(sandbox.request_id_released);
    // A typed reason survives the wire: renewing the deleted sandbox is
    // refused with its deletion reason, not NOT_FOUND.
    let refused = api
        .renew(sandbox_id)
        .await
        .expect_err("a deleted sandbox cannot be renewed");
    assert_eq!(refused.code, tonic::Code::FailedPrecondition);
    assert_eq!(refused.reason.as_deref(), Some("sandbox_deleted"));
}

fn check_audited_expiry(records: &str) {
    let event = |name: &str| {
        records
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid audit JSON"))
            .find(|record| record.get("event").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("no {name}"))
    };
    let opened = event("session_opened");
    let features = opened["features"].as_array().expect("features");
    assert!(features.contains(&Value::from("sandbox_expiry")));
    assert!(features.contains(&Value::from("typed_reasons")));
    let created = event("sandbox_created");
    assert_eq!(created["expiry_lifetime_ms"], 100_000);
    assert_eq!(created["expiry_idle_ms"], 120_000);
}

/// The eval self-test against installed kilnd: `hello-file` and
/// `config-edit` fail their checks on the bare seed and pass after their
/// reference solutions, and the four sessions leave nothing behind.
#[tokio::test]
async fn eval_selftest_proves_two_starter_tasks_against_installed_kilnd() {
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
    let sandboxes_before = non_deleted_ids(&api).await;

    let config = kiln_harness::eval::selftest_config();
    let tasks = kiln_harness::eval::load_tasks(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals/tasks"),
        &["hello-file".to_string(), "config-edit".to_string()],
        &config,
    )
    .expect("the starter tasks load");
    assert_eq!(tasks.len(), 2);
    let temp = common::private_tempdir().expect("temporary output directory");
    let out = temp.path().join("selftest");
    let summary = kiln_harness::eval::selftest(
        &api,
        &tasks,
        &config,
        &out,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .await
    .expect("the self-test runs");
    assert_eq!(summary.stopped, None);
    for result in &summary.results {
        assert_eq!(result.problem, None, "{}", result.task);
    }
    assert_eq!(summary.results.len(), 2);
    for task in ["hello-file", "config-edit"] {
        for kind in ["bare", "solved"] {
            let journal = read_journal(&out.join(task).join(kind).join("journal.jsonl"))
                .expect("self-test journal verifies");
            assert!(journal.complete);
        }
    }

    let after = api.health().await.expect("health after");
    assert!(after.serving, "kilnd unhealthy: {}", after.detail);
    assert_eq!(after.active_sandboxes, 0);
    assert_eq!(after.running_executions, 0);
    assert_eq!(after.reservations, before.reservations);
    assert_eq!(non_deleted_ids(&api).await, sandboxes_before);
}

/// Eight eval runs at once against installed kilnd: each holds its own
/// connection and sandbox and streams one command that sleeps, and their
/// commands overlap in time. Nothing is left behind.
#[tokio::test]
async fn concurrent_eval_runs_stream_side_by_side_through_installed_kilnd() {
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
    assert!(before.capabilities.supports(Feature::ExecStream));
    let sandboxes_before = non_deleted_ids(&api).await;

    let temp = common::private_tempdir().expect("temporary directory");
    write_side_by_side_task(&temp.path().join("tasks"));
    let config = kiln_harness::eval::selftest_config();
    let tasks = kiln_harness::eval::load_tasks(&temp.path().join("tasks"), &[], &config)
        .expect("the task loads");
    let out = temp.path().join("out");
    let options = kiln_harness::eval::SuiteOptions {
        out: out.clone(),
        trials: 8,
        concurrency: 8,
        max_run_secs: None,
        effort: None,
        replay: false,
        label: None,
        base_config: config,
        provider: kiln_harness::provider::ProviderInfo {
            name: "scripted".to_string(),
            model: "scripted".to_string(),
        },
        base_url_host: None,
    };
    // Each run's one command prints when it started and ended, around a
    // sleep long enough for the eight to overlap.
    let script = "date +%s%N; sleep 6; echo done > out.txt; date +%s%N";
    let summary = kiln_harness::eval::run_suite(
        &api,
        &tasks,
        &options,
        || {
            Ok(ScriptedProvider::new([
                ModelResponse::tool_use([ToolCall::new(
                    "call-1",
                    "exec",
                    json!({"argv": ["/bin/sh", "-c", script]}),
                )]),
                ModelResponse::final_text("done"),
            ]))
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");
    assert_eq!(summary.stopped, None);
    assert_eq!(summary.results.len(), 8);
    let mut spans = Vec::new();
    for result in &summary.results {
        assert_eq!(result.outcome, "completed", "{result:?}");
        assert!(result.passed(), "{result:?}");
        let journal = read_journal(
            &out.join("runs")
                .join(&result.task)
                .join(result.trial.to_string())
                .join("journal.jsonl"),
        )
        .expect("each journal verifies");
        assert!(journal.complete);
        spans.push(exec_span(&journal));
    }
    let peak = peak_overlap(&spans);
    eprintln!("peak overlap of the eight commands: {peak}");
    assert!(peak >= 4, "only {peak} commands overlapped: {spans:?}");

    let after = api.health().await.expect("health after");
    assert!(after.serving, "kilnd unhealthy: {}", after.detail);
    assert_eq!(after.active_sandboxes, 0);
    assert_eq!(after.running_executions, 0);
    assert_eq!(after.reservations, before.reservations);
    assert_eq!(non_deleted_ids(&api).await, sandboxes_before);
}

/// One task under `root` whose check passes once `out.txt` exists.
fn write_side_by_side_task(root: &std::path::Path) {
    let task = root.join("side-by-side");
    std::fs::create_dir_all(task.join("seed")).expect("task directory");
    let files: [(&str, &str); 5] = [
        (
            "task.json",
            r#"{"schema": 1, "id": "side-by-side", "description": "runs side by side", "check_timeout_secs": 30}"#,
        ),
        ("prompt.md", "Write out.txt.\n"),
        ("check.sh", "cd /workspace || exit 2\ntest -f out.txt\n"),
        ("solution.sh", "echo done > /workspace/out.txt\n"),
        ("seed/in.txt", "input\n"),
    ];
    for (path, contents) in files {
        std::fs::write(task.join(path), contents).expect("task file");
    }
}

/// When the run's one command started and ended, from the two timestamps
/// it printed.
fn exec_span(journal: &kiln_harness::journal::LoadedJournal) -> (u128, u128) {
    let stdout = journal
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ToolExecuted { call, result, .. } if call.tool == "exec" => result
                .get("stdout")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .expect("the exec result");
    let stamps = stdout.lines().map(nanos).collect::<Vec<_>>();
    assert_eq!(stamps.len(), 2, "{stdout:?}");
    assert!(stamps[0] <= stamps[1], "{stdout:?}");
    (stamps[0], stamps[1])
}

/// A `date +%s%N` line in nanoseconds. A `date` without `%N` leaves seconds
/// and the unexpanded rest, which still orders whole seconds.
fn nanos(line: &str) -> u128 {
    let digits = line
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let value: u128 = digits
        .parse()
        .unwrap_or_else(|_| panic!("not a timestamp: {line:?}"));
    if digits.len() <= 10 {
        value * 1_000_000_000
    } else {
        value
    }
}

/// The most closed `[start, end]` spans that hold one instant.
fn peak_overlap(spans: &[(u128, u128)]) -> usize {
    let mut edges = spans
        .iter()
        .flat_map(|&(start, end)| [(start, 0, 1_i64), (end, 1, -1)])
        .collect::<Vec<_>>();
    // At one instant a span that starts counts before one that ends.
    edges.sort_unstable();
    let (mut open, mut peak) = (0_i64, 0_i64);
    for (_, _, change) in edges {
        open += change;
        peak = peak.max(open);
    }
    usize::try_from(peak).unwrap_or(0)
}
