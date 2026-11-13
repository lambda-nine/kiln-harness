//! The model's `exec` through kilnd's `ExecStream`: live output on the host,
//! the runaway-output cutoff, broken streams, stream capacity, and the
//! execution contexts replay keeps, against the fake kilnd.
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{
    FakeApi, ShAnswer, StreamEnd, StreamEvent, StreamScript, Unstreamed, api_error,
    api_error_with_reason, recorded_in, short_lifecycle_config,
};
use kiln_harness::api::{
    ApiError, CapturedOutput, Execution, ExecutionMode, ExecutionState, Feature, LiveDelivery,
    LiveStream, SandboxExec,
};
use kiln_harness::audit::{AuditEvent, LiveEvidence, MemoryAudit};
use kiln_harness::engine::{
    Harness, HarnessConfig, HarnessError, SeedFile, SessionOutcome, SessionTask, TaskCheck,
};
use kiln_harness::journal::{JournalEvent, LoadedJournal, MemoryJournal};
use kiln_harness::live::LiveOutput;
use kiln_harness::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderInfo, ScriptedProvider, ToolCall,
    ToolDefinition,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{
    EXEC_CONTEXT_MISMATCH, EXEC_MODE_MISMATCH, EXEC_STREAM_DESCRIPTION, OUTPUT_NOT_RETAINED,
    RUNAWAY_OUTPUT_BYTES, STREAM_PROTOCOL_VIOLATION, STREAM_RESULT_UNREADABLE, ToolLimits,
    ToolRegistry,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::Code;

type TestHarness = Harness<FakeApi, ScriptedProvider, MemoryAudit, MemoryJournal>;

/// Everything a live sink was told, in order.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
}

impl Recorder {
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("events").clone()
    }
}

impl LiveOutput for Recorder {
    fn started(&self, call_id: &str, execution_id: &str) {
        self.events
            .lock()
            .expect("events")
            .push(format!("{call_id} started {execution_id}"));
    }

    fn chunk(&self, call_id: &str, stream: LiveStream, data: &[u8]) {
        self.events.lock().expect("events").push(format!(
            "{call_id} {} {}",
            stream.as_str(),
            String::from_utf8_lossy(data)
        ));
    }

    fn gap(&self, call_id: &str, stream: LiveStream, dropped: u64, known: bool) {
        self.events.lock().expect("events").push(format!(
            "{call_id} {} gap {dropped} {known}",
            stream.as_str()
        ));
    }

    fn finished(&self, call_id: &str, stopped: Option<&str>) {
        self.events
            .lock()
            .expect("events")
            .push(format!("{call_id} finished {stopped:?}"));
    }
}

fn exec(call_id: &str, argv: &[&str]) -> ModelResponse {
    ModelResponse::tool_use([ToolCall::new(call_id, "exec", json!({ "argv": argv }))])
}

fn wc() -> ModelResponse {
    exec("c1", &["/usr/bin/wc", "/workspace/x"])
}

/// Two `exec` turns and a final answer.
fn two_execs() -> ScriptedProvider {
    ScriptedProvider::new([
        exec("c1", &["/usr/bin/wc", "/workspace/a"]),
        exec("c2", &["/usr/bin/wc", "/workspace/b"]),
        ModelResponse::final_text("done"),
    ])
}

fn session(api: &FakeApi, provider: ScriptedProvider, config: HarnessConfig) -> TestHarness {
    Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    )
}

async fn run(harness: &mut TestHarness) -> Result<SessionOutcome, HarnessError> {
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
}

/// Let the live sink catch up: it is fed by a thread of its own.
fn drain<P: Provider>(harness: &Harness<FakeApi, P, MemoryAudit, MemoryJournal>) {
    assert!(
        harness.drain_live_output(Duration::from_secs(5)),
        "the live sink caught up"
    );
}

/// The session ended at the tool call with `message`, keeping
/// `ex-0000000000000002` as evidence, with no output, and asking the model
/// nothing more; one stream was sent.
fn ended_unusable(api: &FakeApi, harness: &TestHarness, error: &HarnessError, message: &str) {
    assert!(
        matches!(error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition && error.message == message),
        "{error}"
    );
    let results = tool_results(harness);
    assert_eq!(results.len(), 1, "the model is not asked again");
    let (result, ids) = &results[0];
    assert_eq!(ids, &["ex-0000000000000002".to_string()]);
    assert_eq!(result["message"], message, "{result}");
    assert!(result.get("stdout").is_none(), "{result}");
    assert_eq!(api.state().streams.len(), 1);
}

fn tool_results(harness: &TestHarness) -> Vec<(Value, Vec<String>)> {
    harness
        .journal()
        .records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted {
                result,
                execution_ids,
                ..
            } => Some((result.clone(), execution_ids.clone())),
            _ => None,
        })
        .collect()
}

fn completed_live(harness: &TestHarness) -> Vec<(String, Option<LiveEvidence>)> {
    harness
        .audit()
        .records
        .iter()
        .filter_map(|record| match &record.event {
            AuditEvent::ToolCompleted { tool, live, .. } => Some((tool.clone(), live.clone())),
            _ => None,
        })
        .collect()
}

fn exec_context(journal: &[kiln_harness::journal::JournalRecord]) -> Option<String> {
    match &journal.first().map(|record| &record.event) {
        Some(JournalEvent::SessionStarted { exec_context, .. }) => exec_context.clone(),
        other => panic!("no session start: {other:?}"),
    }
}

/// The non-baseline Execs that went unary.
fn unary_tool_execs(api: &FakeApi) -> usize {
    let state = api.state();
    state.execs.len() - state.streams.len()
}

fn mib(n: u64) -> u64 {
    n << 20
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_live_session_streams_only_the_models_exec() {
    let api = FakeApi::default();
    api.state().sh_handler = Some(Arc::new(|_: &[u8], _: &mut _| ShAnswer::exit(0)));
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([
            ToolCall::new(
                "w",
                "write_file",
                json!({"path": "x", "content": "hello\n"}),
            ),
            ToolCall::new("r", "read_file", json!({"path": "x"})),
            ToolCall::new("l", "list_dir", json!({"path": "."})),
            ToolCall::new("d", "workspace_diff", json!({})),
            ToolCall::new(
                "e",
                "exec",
                json!({"argv": ["/usr/bin/wc", "/workspace/x"]}),
            ),
        ]),
        ModelResponse::final_text("done"),
    ]);
    let task = SessionTask {
        seed: vec![SeedFile {
            path: "in/seed.txt".to_string(),
            contents: b"seed\n".to_vec(),
        }],
        check: Some(TaskCheck {
            script: b"exit 0\n".to_vec(),
            timeout: Duration::from_secs(5),
        }),
        ..SessionTask::new("Use the tools.", "Count x.")
    };
    let recorder = Arc::new(Recorder::default());
    let mut harness = session(&api, provider, short_lifecycle_config())
        .with_live_output(Arc::clone(&recorder) as Arc<dyn LiveOutput>);
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");
    assert_eq!(outcome.tool_calls, 5);

    let state = api.state();
    // Only the model's exec streamed; seeding (mkdir, dd), the baseline,
    // the file tools, the diff's manifest and the check stayed unary.
    let streamed: Vec<_> = state
        .streams
        .iter()
        .map(|exec| exec.argv[0].clone())
        .collect();
    assert_eq!(streamed, ["/usr/bin/wc"]);
    let unary: Vec<_> = state
        .execs
        .iter()
        .map(|exec| exec.argv[0].as_str())
        .filter(|program| *program != "/usr/bin/wc")
        .collect();
    assert_eq!(
        unary,
        [
            "/bin/mkdir",
            "/bin/dd",
            "/usr/bin/find",
            "/bin/dd",
            "/bin/stat",
            "/usr/bin/head",
            "/bin/ls",
            "/usr/bin/find",
            "/bin/sh"
        ]
    );
    let modes: Vec<_> = state.executions.iter().map(|row| row.mode).collect();
    assert_eq!(
        modes
            .iter()
            .filter(|mode| **mode == ExecutionMode::Stream)
            .count(),
        1
    );
    assert!(modes.iter().all(|mode| *mode != ExecutionMode::Unspecified));
    let wc_id = state
        .executions
        .iter()
        .find(|row| row.mode == ExecutionMode::Stream)
        .map(|row| row.id.clone())
        .expect("the streamed row");
    drop(state);

    assert_eq!(
        exec_context(&harness.journal().records).as_deref(),
        Some("workspace_stream_v1")
    );
    let results = tool_results(&harness);
    let (exec_result, _) = &results[4];
    assert_eq!(exec_result["ok"], true);
    assert_eq!(exec_result["stdout"], "6 /workspace/x\n");
    assert!(exec_result.get("stopped").is_none(), "{exec_result}");
    // Only the streamed call carries live evidence, kilnd's own totals.
    let live = completed_live(&harness);
    assert!(live[..4].iter().all(|(_, live)| live.is_none()), "{live:?}");
    assert_eq!(
        live[4],
        (
            "exec".to_string(),
            Some(LiveEvidence {
                stdout: LiveDelivery {
                    delivered_bytes: 15,
                    dropped_bytes: 0,
                    dropped_bytes_known: true,
                },
                stderr: LiveDelivery {
                    delivered_bytes: 0,
                    dropped_bytes: 0,
                    dropped_bytes_known: true,
                },
                stopped: None,
            })
        )
    );
    // The audit record of a unary call has no `live` key at all.
    let encoded = harness
        .audit()
        .records
        .iter()
        .filter(|record| matches!(record.event, AuditEvent::ToolCompleted { .. }))
        .map(|record| serde_json::to_string(record).expect("encode"))
        .collect::<Vec<_>>();
    assert!(!encoded[0].contains("\"live\""), "{}", encoded[0]);
    assert!(encoded[4].contains("\"live\""), "{}", encoded[4]);
    // The sink heard the call under its audit ID, and nothing else.
    drain(&harness);
    assert_eq!(
        recorder.events(),
        [
            format!("call-1-5 started {wc_id}"),
            "call-1-5 stdout 6 /workspace/x\n".to_string(),
            "call-1-5 finished None".to_string(),
        ]
    );
}

/// Run two `exec` turns with the first stream scripted, and return the
/// harness after a session that must complete.
async fn first_exec_streams(script: StreamScript) -> (FakeApi, TestHarness, Arc<Recorder>) {
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(script);
    let recorder = Arc::new(Recorder::default());
    let mut harness = session(&api, two_execs(), short_lifecycle_config())
        .with_live_output(Arc::clone(&recorder) as Arc<dyn LiveOutput>);
    let outcome = run(&mut harness).await.expect("the session goes on");
    assert_eq!(outcome.steps, 3);
    assert_eq!(outcome.tool_calls, 2);
    drain(&harness);
    (api, harness, recorder)
}

fn assert_cut(api: &FakeApi, harness: &TestHarness, stream: LiveStream) {
    assert_eq!(api.state().streams_dropped, 1);
    assert_eq!(api.state().streams.len(), 2);
    assert_eq!(unary_tool_execs(api), 1, "only the baseline is unary");
    let results = tool_results(harness);
    let (cut, ids) = &results[0];
    assert_eq!(cut["stopped"], "output_limit", "{cut}");
    assert_eq!(cut["state"], "canceled");
    assert_eq!(cut["ok"], false);
    assert_eq!(ids, &["ex-0000000000000002".to_string()]);
    // The next call works in the same sandbox.
    let (next, _) = &results[1];
    assert_eq!(next["ok"], true, "{next}");
    assert!(next.get("stopped").is_none());
    let live = completed_live(harness);
    let first = live[0].1.as_ref().expect("live evidence");
    assert_eq!(first.stopped.as_deref(), Some("output_limit"));
    // The harness's own counts, a lower bound on what was dropped.
    assert!(!first.stdout.dropped_bytes_known && !first.stderr.dropped_bytes_known);
    let counted = match stream {
        LiveStream::Stdout => first.stdout,
        LiveStream::Stderr => first.stderr,
    };
    assert!(counted.delivered_bytes + counted.dropped_bytes > 0);
    assert!(live[1].1.as_ref().expect("live").stopped.is_none());
}

#[tokio::test]
async fn output_past_the_runaway_limit_stops_the_command_and_the_session_goes_on() {
    for stream in [LiveStream::Stdout, LiveStream::Stderr] {
        // Chunks whose ends pass 8 MiB.
        let (api, harness, recorder) = first_exec_streams(StreamScript {
            events: Some(vec![
                StreamEvent::Chunk(stream, 0, b"yyyy\n".to_vec()),
                StreamEvent::Chunk(stream, RUNAWAY_OUTPUT_BYTES - 4, b"yyyy\n".to_vec()),
                StreamEvent::Chunk(stream, RUNAWAY_OUTPUT_BYTES + 1, b"never seen".to_vec()),
            ]),
            end: StreamEnd::Finished,
        })
        .await;
        assert_cut(&api, &harness, stream);
        let events = recorder.events();
        assert!(!events.iter().any(|event| event.contains("never seen")));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("call-1-1 finished"))
                .collect::<Vec<_>>(),
            [&"call-1-1 finished Some(\"output_limit\")".to_string()]
        );
    }
}

#[tokio::test]
async fn bytes_dropped_past_the_runaway_limit_count_too() {
    for stream in [LiveStream::Stdout, LiveStream::Stderr] {
        let (api, harness, _) = first_exec_streams(StreamScript {
            events: Some(vec![
                StreamEvent::Chunk(stream, 0, b"y\n".to_vec()),
                StreamEvent::Gap(stream, 2, RUNAWAY_OUTPUT_BYTES, true),
            ]),
            end: StreamEnd::Finished,
        })
        .await;
        assert_cut(&api, &harness, stream);
        let live = completed_live(&harness);
        let first = live[0].1.as_ref().expect("live");
        let counted = match stream {
            LiveStream::Stdout => first.stdout,
            LiveStream::Stderr => first.stderr,
        };
        assert_eq!(counted.delivered_bytes, 2);
        assert_eq!(counted.dropped_bytes, RUNAWAY_OUTPUT_BYTES);
    }
}

#[tokio::test]
async fn output_of_exactly_the_runaway_limit_is_not_cut() {
    let (api, harness, _) = first_exec_streams(StreamScript {
        events: Some(vec![
            StreamEvent::Chunk(LiveStream::Stdout, 0, b"y\n".to_vec()),
            StreamEvent::Gap(LiveStream::Stdout, 2, RUNAWAY_OUTPUT_BYTES - 4, true),
            StreamEvent::Chunk(
                LiveStream::Stdout,
                RUNAWAY_OUTPUT_BYTES - 2,
                b"y\n".to_vec(),
            ),
            StreamEvent::Gap(LiveStream::Stderr, 0, mib(8), false),
        ]),
        end: StreamEnd::Finished,
    })
    .await;
    assert_eq!(api.state().streams_dropped, 0);
    let results = tool_results(&harness);
    assert!(results[0].0.get("stopped").is_none(), "{}", results[0].0);
    assert_eq!(results[0].0["ok"], true);
    let live = completed_live(&harness);
    assert_eq!(live[0].1.as_ref().expect("live").stopped, None);
}

/// A stream past the cutoff from its first chunk.
fn runaway() -> StreamScript {
    StreamScript {
        events: Some(vec![StreamEvent::Chunk(
            LiveStream::Stdout,
            0,
            vec![b'y'; usize::try_from(RUNAWAY_OUTPUT_BYTES).expect("fits") + 1],
        )]),
        end: StreamEnd::Finished,
    }
}

/// A recovery window of 1 s after the 5 s tool timeout.
fn one_second_recovery() -> HarnessConfig {
    HarnessConfig {
        lifecycle_timeout: Duration::from_secs(1),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    }
}

#[tokio::test(start_paused = true)]
async fn a_cut_command_whose_row_never_settles_ends_the_session_with_its_id() {
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(runaway());
    api.state().stopped_settles_after = Some(u32::MAX);
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("unsettled");
    // The tool followed the row for its whole budget, then ended the
    // session with the ID kept, a fixed message and the cutoff marked.
    ended_unusable(&api, &harness, &error, STREAM_RESULT_UNREADABLE);
    let results = tool_results(&harness);
    assert_eq!(results[0].0["error"], "kilnd_error");
    assert_eq!(results[0].0["stopped"], "output_limit");
    assert!(
        !results[0].0.to_string().contains("ex-"),
        "{}",
        results[0].0
    );
    assert!(api.state().get_execution_calls > 10);
    // Only the known ID was read: no response-loss recovery ran.
    assert!(
        api.state()
            .get_execution_ids
            .iter()
            .all(|id| id == "ex-0000000000000002")
    );

    // A row that settles while the call still has time is the result.
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(runaway());
    api.state().stopped_settles_after = Some(3);
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    run(&mut harness).await.expect("settles in time");
    assert_eq!(tool_results(&harness)[0].0["stopped"], "output_limit");
}

#[tokio::test(start_paused = true)]
async fn a_cut_that_settles_after_the_calls_own_timeout_keeps_its_result() {
    // M4-03: the row settles on its 30th read, about 5.1 s in, past the
    // 5 s tool timeout but inside the recovery window after it. The model
    // still gets the stopped result, and a replay whose kilnd assigns other
    // IDs matches it.
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(runaway());
    api.state().stopped_settles_after = Some(30);
    let mut harness = session(&api, two_execs(), one_second_recovery());
    run(&mut harness).await.expect("the session goes on");
    let results = tool_results(&harness);
    assert_eq!(results[0].0["stopped"], "output_limit", "{}", results[0].0);
    assert_eq!(results[0].0["state"], "canceled");
    assert_eq!(results[0].1, ["ex-0000000000000002"]);
    assert!(api.state().get_execution_calls >= 30);
    assert_eq!(results[1].0["ok"], true);

    let replay_api = FakeApi::with_expiry();
    replay_api.state().next_execution = 100;
    replay_api.state().stream_script.push_back(runaway());
    replay_api.state().stopped_settles_after = Some(30);
    let loaded = LoadedJournal {
        records: harness.journal().records.clone(),
        complete: true,
    };
    let provider = ReplayProvider::from_journal(&loaded).expect("replayable");
    let config = provider.replay_config(one_second_recovery());
    let mut replay = Harness::new(
        replay_api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    replay
        .replay(CancellationToken::new())
        .await
        .expect("the replay completes");
    assert_eq!(replay.provider().diverged_at(), None);
    assert!(replay.provider().exhausted());
}

#[tokio::test(start_paused = true)]
async fn a_stream_broken_after_its_start_whose_row_never_settles_ends_the_session() {
    // The stream named its execution, then broke; the row stays RUNNING.
    let api = FakeApi::with_expiry();
    api.state().running_exec = Some((2, u32::MAX));
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("unsettled");
    ended_unusable(&api, &harness, &error, STREAM_RESULT_UNREADABLE);
    let (result, _) = &tool_results(&harness)[0];
    // Nothing daemon-assigned reaches the model.
    assert!(!result.to_string().contains("ex-"), "{result}");
    assert!(result.get("stopped").is_none(), "{result}");
    let audited = tool_evidence(&harness.audit().records);
    assert!(
        matches!(&audited[0], AuditEvent::ToolCompleted { execution_ids, .. }
        if execution_ids == &["ex-0000000000000002".to_string()])
    );
}

/// Replay `journal` against `api`.
async fn replay_in(
    journal: &[kiln_harness::journal::JournalRecord],
    api: &FakeApi,
) -> Harness<FakeApi, ReplayProvider, MemoryAudit, MemoryJournal> {
    let loaded = LoadedJournal {
        records: journal.to_vec(),
        complete: true,
    };
    let provider = ReplayProvider::from_journal(&loaded).expect("replayable");
    let config = provider.replay_config(short_lifecycle_config());
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    harness
        .replay(CancellationToken::new())
        .await
        .expect("the replay completes");
    harness
}

fn tool_evidence(records: &[kiln_harness::audit::AuditRecord]) -> Vec<AuditEvent> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            AuditEvent::ToolCompleted { .. } => Some(record.event.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_stream_broken_after_its_start_gives_the_rows_result() {
    // kilnd's row is what `ExecFinished` would have carried, so the model
    // gets the same result as from a stream that finished.
    let finished = FakeApi::with_expiry();
    let mut whole = session(&finished, two_execs(), short_lifecycle_config());
    run(&mut whole).await.expect("finishes");

    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(StreamScript {
        events: None,
        end: StreamEnd::Broken(api_error(Code::Unavailable, "stream broke")),
    });
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    run(&mut harness)
        .await
        .expect("a reusable row lets the session go on");
    let results = tool_results(&harness);
    assert_eq!(results, tool_results(&whole));
    assert_eq!(results[0].0["ok"], true, "{}", results[0].0);
    assert_eq!(results[0].1, ["ex-0000000000000002"]);
    assert_eq!(api.state().streams.len(), 2);
    assert_eq!(api.state().get_execution_ids, ["ex-0000000000000002"]);
}

#[tokio::test]
async fn a_stream_broken_after_its_start_is_followed_by_its_id_alone() {
    // M4-06: the sandbox's recent rows cannot be read, but the stream named
    // its execution, so the harness reads that row directly.
    let api = FakeApi::with_expiry();
    api.state().recent_unavailable = true;
    api.state().stream_script.push_back(StreamScript {
        events: None,
        end: StreamEnd::Broken(api_error(Code::Unavailable, "stream broke")),
    });
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    run(&mut harness).await.expect("the row settled");
    let results = tool_results(&harness);
    assert_eq!(results[0].0["ok"], true, "{}", results[0].0);
    assert_eq!(results[0].1, ["ex-0000000000000002"]);
    assert_eq!(api.state().get_execution_ids, ["ex-0000000000000002"]);
}

/// A live session recorded through [`Unstreamed`], so it can also be
/// replayed as recorded in an earlier context.
async fn recorded_with_hashes(
    api: &FakeApi,
    provider: ScriptedProvider,
) -> (
    Vec<kiln_harness::journal::JournalRecord>,
    Vec<AuditEvent>,
    Vec<(String, String)>,
) {
    let mut harness = Harness::new(
        api.clone(),
        Unstreamed::new(provider),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("a reusable row lets the session go on");
    (
        harness.journal().records.clone(),
        tool_evidence(&harness.audit().records),
        harness.provider().hashes.clone(),
    )
}

#[tokio::test]
async fn a_stream_broken_before_its_start_gives_the_evidence_a_lost_unary_response_gives() {
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(StreamScript {
        events: None,
        end: StreamEnd::BrokenBeforeStarted(api_error(Code::Unavailable, "exec response lost")),
    });
    let (records, audited, hashes) = recorded_with_hashes(&api, two_execs()).await;
    let results = tool_results_of(&records);
    assert_eq!(results[0]["error"], "kilnd_error");
    // Recovery found the row among the recent ones, which leave output
    // out, and read it once more with `GetExecution`.
    assert!(
        api.state()
            .get_execution_ids
            .contains(&"ex-0000000000000002".to_string())
    );

    // The same session recorded unary, replayed with the response lost the
    // unary way, reproduces every tool result and the same evidence.
    let unary = FakeApi::with_expiry();
    unary.state().lose_exec_response = Some(2);
    let earlier = recorded_in(&records, Some("workspace_v1"), &hashes);
    let replayed = replay_in(&earlier, &unary).await;
    assert_eq!(replayed.provider().diverged_at(), None);
    assert!(replayed.provider().exhausted());
    assert!(unary.state().streams.is_empty());
    let strip = |events: Vec<AuditEvent>| {
        events
            .into_iter()
            .map(|event| match event {
                AuditEvent::ToolCompleted {
                    ok,
                    execution_ids,
                    result,
                    stdout,
                    stderr,
                    ..
                } => (ok, execution_ids, result, stdout, stderr),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        strip(audited)[0],
        strip(tool_evidence(&replayed.audit().records))[0]
    );
}

fn output_storage_none(row: &mut Execution) {
    row.stdout = CapturedOutput::default();
    row.stderr = CapturedOutput::default();
}

#[tokio::test]
async fn a_row_without_output_the_stream_showed_ends_the_session() {
    // M4-01: under `--output-storage none` kilnd stores no bytes and no
    // byte count, so only the live stream proves the command wrote any.
    let api = FakeApi::with_expiry();
    api.state().tamper_exec = Some((2, output_storage_none));
    api.state().stream_script.push_back(StreamScript {
        events: Some(vec![StreamEvent::Chunk(
            LiveStream::Stdout,
            0,
            b"live output that the stored row erases\n".to_vec(),
        )]),
        end: StreamEnd::Finished,
    });
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("not retained");
    ended_unusable(&api, &harness, &error, OUTPUT_NOT_RETAINED);
}

#[tokio::test]
async fn a_recovered_streamed_row_is_checked_as_a_returned_one() {
    // M4-02: a row read back after a broken stream, by its ID or found by
    // recovery, passes every check `ExecFinished`'s row does.
    let broke = || api_error(Code::Unavailable, "stream broke");
    let cases: [(&str, StreamEnd, common::Tamper, &str); 4] = [
        (
            "after start, unary",
            StreamEnd::Broken(broke()),
            recorded_unary,
            EXEC_MODE_MISMATCH,
        ),
        (
            "before start, unary",
            StreamEnd::BrokenBeforeStarted(broke()),
            recorded_unary,
            EXEC_MODE_MISMATCH,
        ),
        (
            "before start, not retained",
            StreamEnd::BrokenBeforeStarted(broke()),
            output_not_retained,
            OUTPUT_NOT_RETAINED,
        ),
        (
            "before start, another cwd",
            StreamEnd::BrokenBeforeStarted(broke()),
            wrong_cwd,
            EXEC_CONTEXT_MISMATCH,
        ),
    ];
    for (name, end, tamper, message) in cases {
        let api = FakeApi::with_expiry();
        api.state().tamper_exec = Some((2, tamper));
        api.state()
            .stream_script
            .push_back(StreamScript { events: None, end });
        let mut harness = session(&api, two_execs(), short_lifecycle_config());
        let error = run(&mut harness).await.expect_err(name);
        assert!(
            matches!(&error, HarnessError::Api(error)
                if error.code == Code::FailedPrecondition && error.message == message),
            "{name}: {error}"
        );
        let results = tool_results(&harness);
        assert_eq!(results.len(), 1, "{name}: the model is not asked again");
        assert_eq!(results[0].1, ["ex-0000000000000002"], "{name}");
        assert!(results[0].0.get("stdout").is_none(), "{name}");
        assert_eq!(api.state().streams.len(), 1, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_canceled_stream_is_recovered_by_its_id() {
    // The command is still running when the session is canceled: the
    // call is dropped, but recovery knows the execution the stream named
    // and reads it directly, even with the recent rows unreadable.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_delay = Duration::from_secs(60);
    api.state().recent_unavailable = true;
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let error = harness
        .run(&SessionTask::demo(), cancel)
        .await
        .expect_err("canceled");
    assert!(matches!(error, HarnessError::Canceled), "{error}");
    assert_eq!(api.state().get_execution_ids, ["ex-0000000000000002"]);
    let results = tool_results(&harness);
    assert_eq!(results[0].0["error"], "tool_interrupted");
    assert_eq!(results[0].1, ["ex-0000000000000002"]);
    let audited = tool_evidence(&harness.audit().records);
    assert!(
        matches!(&audited[0], AuditEvent::ToolCompleted { execution_ids, .. }
        if execution_ids == &["ex-0000000000000002".to_string()])
    );
    assert!(!api.state().deleted.is_empty());
}

#[tokio::test(start_paused = true)]
async fn stream_capacity_is_waited_for_never_bypassed() {
    let limit = || {
        api_error_with_reason(
            Code::ResourceExhausted,
            "exec_stream_limit",
            "exec_stream_limit: 8 streams per uid, 16 on the node",
        )
    };
    // Refused once, then admitted: one row and a normal result.
    let api = FakeApi::with_expiry();
    api.state().exec_refusals.push_back(limit());
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    run(&mut harness).await.expect("completes");
    assert_eq!(api.state().streams.len(), 3);
    assert_eq!(unary_tool_execs(&api), 1, "only the baseline is unary");
    assert_eq!(api.state().executions.len(), 3);
    let results = tool_results(&harness);
    assert_eq!(results[0].0["ok"], true, "{}", results[0].0);
    assert_eq!(results[0].1, ["ex-0000000000000002"]);
    {
        let state = api.state();
        let (first, retry) = (&state.streams[0], &state.streams[1]);
        assert_eq!(first.argv, retry.argv);
        assert!(retry.timeout < first.timeout);
    }

    // Refused for the whole call: an ordinary tool error, retried with
    // backoff and never sent unary; the session goes on.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(limit());
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    let outcome = run(&mut harness).await.expect("the session goes on");
    assert_eq!(outcome.steps, 2);
    let results = tool_results(&harness);
    assert_eq!(results[0].0["error"], "kilnd_error");
    assert!(
        results[0].0["message"]
            .as_str()
            .is_some_and(|message| message.contains("stream capacity was busy")),
        "{}",
        results[0].0
    );
    let attempts = api.state().streams.len();
    // 50 ms doubling to 1 s within the 5 s tool timeout: nine attempts.
    assert_eq!(attempts, 9);
    assert_eq!(unary_tool_execs(&api), 1, "only the baseline is unary");
}

#[tokio::test]
async fn a_start_race_on_a_stream_is_sent_once_more() {
    let raced = || {
        api_error_with_reason(
            Code::FailedPrecondition,
            "execution_start_raced",
            "execution_start_raced: the sandbox changed state",
        )
    };
    let api = FakeApi::with_expiry();
    api.state().exec_refusals.push_back(raced());
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    run(&mut harness).await.expect("completes");
    assert_eq!(api.state().streams.len(), 2);
    assert_eq!(tool_results(&harness)[0].0["ok"], true);

    let api = FakeApi::with_expiry();
    api.state().exec_refusals.extend([raced(), raced()]);
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    run(&mut harness)
        .await
        .expect("a second race is recoverable");
    assert_eq!(api.state().streams.len(), 2);
    assert_eq!(unary_tool_execs(&api), 1);
    assert_eq!(tool_results(&harness)[0].0["error"], "kilnd_error");
}

fn recorded_unary(row: &mut Execution) {
    row.mode = ExecutionMode::Unary;
}

fn recorded_unspecified(row: &mut Execution) {
    row.mode = ExecutionMode::Unspecified;
}

fn output_not_retained(row: &mut Execution) {
    row.stdout.data.clear();
}

fn stderr_not_retained(row: &mut Execution) {
    row.stderr.data.clear();
    row.stderr.observed_bytes = 7;
}

fn wrong_cwd(row: &mut Execution) {
    row.cwd = "/".to_string();
}

#[tokio::test]
async fn a_streamed_row_that_contradicts_the_request_ends_the_session() {
    let cases: [(&str, common::Tamper, &str); 5] = [
        ("recorded unary", recorded_unary, EXEC_MODE_MISMATCH),
        ("no mode", recorded_unspecified, EXEC_MODE_MISMATCH),
        (
            "stdout not retained",
            output_not_retained,
            OUTPUT_NOT_RETAINED,
        ),
        (
            "stderr not retained",
            stderr_not_retained,
            OUTPUT_NOT_RETAINED,
        ),
        ("another cwd", wrong_cwd, EXEC_CONTEXT_MISMATCH),
    ];
    for (name, tamper, message) in cases {
        let api = FakeApi::with_expiry();
        // Exec 1 is the baseline; 2 is the model's stream.
        api.state().tamper_exec = Some((2, tamper));
        let mut harness = session(&api, two_execs(), short_lifecycle_config());
        let error = run(&mut harness).await.expect_err(name);
        assert!(
            matches!(&error, HarnessError::Api(error)
                if error.code == Code::FailedPrecondition && error.message == message),
            "{name}: {error}"
        );
        let results = tool_results(&harness);
        assert_eq!(results.len(), 1, "{name}: the model is not asked again");
        let (result, ids) = &results[0];
        // The execution is evidence; its output never reaches the model.
        assert_eq!(ids, &["ex-0000000000000002".to_string()], "{name}");
        assert_eq!(result["message"], message, "{name}");
        assert!(result.get("stdout").is_none(), "{name}: {result}");
        assert_eq!(api.state().streams.len(), 1, "{name}");
    }

    // The same retention check passes a row that observed nothing, and
    // never applies to unary rows: `read_file` of an empty file.
    let api = FakeApi::with_expiry();
    api.state()
        .files
        .insert("/workspace/e".to_string(), Vec::new());
    let provider = ScriptedProvider::new([
        exec("c1", &["/bin/mkdir", "/workspace/d"]),
        ModelResponse::tool_use([ToolCall::new("r", "read_file", json!({"path": "e"}))]),
        ModelResponse::final_text("done"),
    ]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    run(&mut harness)
        .await
        .expect("empty output is not withheld output");
}

#[tokio::test]
async fn a_backend_that_cannot_stream_ends_the_session_at_the_tool() {
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error_with_reason(
        Code::Unimplemented,
        "backend_unsupported:exec_stream",
        "backend_unsupported:exec_stream: this backend has no live output",
    ));
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("unusable");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition
                && error.message == "execution left the sandbox unusable"),
        "{error}"
    );
    assert_eq!(tool_results(&harness)[0].0["error"], "kilnd_error");
    // Refused once; never retried, and never sent unary.
    assert_eq!(api.state().streams.len(), 1);
    assert_eq!(unary_tool_execs(&api), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stream_dropped_at_its_deadline_still_tells_the_sink_it_ended() {
    let api = FakeApi::with_expiry();
    api.state().tool_exec_delay = Duration::from_secs(60);
    let recorder = Arc::new(Recorder::default());
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config())
        .with_live_output(Arc::clone(&recorder) as Arc<dyn LiveOutput>);
    let error = run(&mut harness).await.expect_err("deadline");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    // Recovery read the execution the stream named.
    assert_eq!(tool_results(&harness)[0].1, ["ex-0000000000000002"]);
    assert_eq!(api.state().get_execution_ids, ["ex-0000000000000002"]);
    drain(&harness);
    assert_eq!(
        recorder.events(),
        [
            "call-1-1 started ex-0000000000000002".to_string(),
            "call-1-1 finished None".to_string(),
        ]
    );
}

/// Run a demo session against `api`, which must refuse it before create.
async fn refused_before_create(api: &FakeApi, config: HarnessConfig, expected: &str) {
    let mut harness = session(api, ScriptedProvider::demo(), config);
    let error = run(&mut harness).await.expect_err("refused");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition && error.message.contains(expected)),
        "{error}"
    );
    assert!(api.state().create_requests.is_empty());
    assert!(api.state().execs.is_empty());
    assert!(harness.journal().records.is_empty());
    assert!(harness.audit().records.is_empty());
}

#[tokio::test]
async fn discovery_requires_streams_that_fit_the_tool_timeout() {
    let api = FakeApi::with_expiry();
    api.state().features.remove(&Feature::ExecStream);
    refused_before_create(&api, short_lifecycle_config(), "exec_stream").await;

    // The longest stream must fit the 5 s tool timeout.
    let api = FakeApi::with_expiry();
    if let Some(limits) = api.state().limits.as_mut() {
        limits.max_stream_timeout_ms = 4_999;
    }
    refused_before_create(
        &api,
        short_lifecycle_config(),
        "kilnd advertised max_stream_timeout_ms 4999; kiln-harness needs at least 5000 for \
         exec_stream",
    )
    .await;
    // A longer tool timeout needs a longer stream timeout.
    let api = FakeApi::with_expiry();
    let mut config = short_lifecycle_config();
    config.tool_limits.timeout = Duration::from_secs(9);
    config.session_timeout = Duration::from_secs(60);
    if let Some(limits) = api.state().limits.as_mut() {
        limits.max_stream_timeout_ms = 8_000;
    }
    refused_before_create(&api, config, "needs at least 9000 for exec_stream").await;
    // No limits at all are too small, as for the exec options.
    let api = FakeApi::default();
    api.state().limits = None;
    refused_before_create(&api, short_lifecycle_config(), "max_cwd_bytes").await;

    // Exactly the tool timeout is enough.
    let api = FakeApi::with_expiry();
    if let Some(limits) = api.state().limits.as_mut() {
        limits.max_stream_timeout_ms = 5_000;
    }
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    run(&mut harness).await.expect("fits");
}

/// Record the demo live against a fake, then hand back its journal and
/// each request's hashes, live and as an earlier context sends it.
async fn recorded_demo() -> (
    Vec<kiln_harness::journal::JournalRecord>,
    Vec<(String, String)>,
) {
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        Unstreamed::new(ScriptedProvider::demo()),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("records");
    assert_eq!(api.state().streams.len(), 1);
    (
        harness.journal().records.clone(),
        harness.provider().hashes.clone(),
    )
}

fn every_exec_ran_in_the_workspace(api: &FakeApi) {
    let env = common::workspace_env();
    for exec in &api.state().execs {
        assert_eq!((exec.cwd.as_str(), &exec.env), ("/workspace", &env));
    }
}

#[tokio::test]
async fn replay_keeps_the_recorded_context() {
    let (recording, hashes) = recorded_demo().await;
    assert_eq!(
        exec_context(&recording).as_deref(),
        Some("workspace_stream_v1")
    );

    // A streaming recording streams again, and matches.
    let api = FakeApi::default();
    let replayed = replay_in(&recording, &api).await;
    assert_eq!(replayed.provider().diverged_at(), None);
    assert!(replayed.provider().exhausted());
    let streamed: Vec<_> = api
        .state()
        .streams
        .iter()
        .map(|exec| exec.argv.clone())
        .collect();
    assert_eq!(streamed, [["/usr/bin/wc", "-c", "/workspace/demo.txt"]]);
    assert_eq!(
        exec_context(&replayed.journal().records).as_deref(),
        Some("workspace_stream_v1")
    );
    every_exec_ran_in_the_workspace(&api);

    // A workspace_v1 recording replays unary, every Exec of it, against a
    // daemon that lists streaming and one that does not.
    for streams in [true, false] {
        let api = FakeApi::default();
        if !streams {
            api.state().features.remove(&Feature::ExecStream);
            if let Some(limits) = api.state().limits.as_mut() {
                limits.max_stream_timeout_ms = 0;
            }
        }
        let earlier = recorded_in(&recording, Some("workspace_v1"), &hashes);
        let replayed = replay_in(&earlier, &api).await;
        assert_eq!(replayed.provider().diverged_at(), None, "{streams}");
        assert!(replayed.provider().exhausted(), "{streams}");
        assert!(api.state().streams.is_empty(), "{streams}");
        assert!(
            api.state()
                .executions
                .iter()
                .all(|row| row.mode == ExecutionMode::Unary),
            "{streams}"
        );
        assert_eq!(
            exec_context(&replayed.journal().records).as_deref(),
            Some("workspace_v1")
        );
        every_exec_ran_in_the_workspace(&api);
        // Its result has no field a streamed one could add.
        assert!(
            tool_results_of(&replayed.journal().records)
                .iter()
                .all(|result| result.get("stopped").is_none())
        );
    }

    // A legacy recording needs no exec option and no streaming either.
    let api = FakeApi::default();
    api.state().features.clear();
    api.state().limits = None;
    let replayed = replay_in(&recorded_in(&recording, None, &hashes), &api).await;
    assert!(replayed.provider().exhausted());
    assert!(api.state().streams.is_empty());
    assert!(
        api.state()
            .execs
            .iter()
            .all(|exec| exec.cwd.is_empty() && exec.env.is_empty())
    );
}

fn tool_results_of(records: &[kiln_harness::journal::JournalRecord]) -> Vec<Value> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_stream_answer_needs_its_own_request() {
    // The fake records each ExecStream as sent: argv, stdin, cwd, env and
    // a command timeout under the tool's wall budget less its reserve.
    let api = FakeApi::with_expiry();
    let provider = ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "c1",
            "exec",
            json!({"argv": ["/bin/sh", "-c", "cat"], "stdin": "in", "timeout_ms": 2000}),
        )]),
        ModelResponse::final_text("done"),
    ]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    run(&mut harness).await.expect("completes");
    let state = api.state();
    let sent: &SandboxExec = &state.streams[0];
    assert_eq!(sent.argv, ["/bin/sh", "-c", "cat"]);
    assert_eq!(sent.stdin, b"in");
    assert_eq!(sent.timeout, Duration::from_millis(1_750));
    assert_eq!(sent.cwd, "/workspace");
    assert_eq!(sent.env, common::workspace_env());
}

#[test]
fn errors_from_a_refused_stream_keep_their_reason() {
    use kiln_harness::api::{exec_stream_limit, sandbox_unusable_refusal};
    let error = |code, reason: &str| ApiError {
        code,
        message: reason.to_string(),
        reason: Some(reason.to_string()),
    };
    let limit = error(Code::ResourceExhausted, "exec_stream_limit");
    assert!(exec_stream_limit(&limit, true));
    assert!(!exec_stream_limit(&limit, false));
    assert!(!exec_stream_limit(
        &error(Code::Unavailable, "exec_stream_limit"),
        true
    ));
    assert!(!sandbox_unusable_refusal(&limit, true));
    let unsupported = error(Code::Unimplemented, "backend_unsupported:exec_stream");
    assert!(sandbox_unusable_refusal(&unsupported, true));
    assert!(!sandbox_unusable_refusal(&unsupported, false));
    assert!(!sandbox_unusable_refusal(
        &error(Code::FailedPrecondition, "backend_unsupported:exec_stream"),
        true
    ));
}

/// What a [`GatedSink`] was told.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Heard {
    Started,
    Chunk(usize),
    Gap(u64, bool),
    Finished,
}

/// A live sink that blocks in its first chunk until released, as a
/// printer whose pipe nobody reads, and records what it was told.
#[derive(Default)]
struct GatedSink {
    entered: AtomicBool,
    gate: (Mutex<bool>, Condvar),
    heard: Mutex<Vec<Heard>>,
}

impl GatedSink {
    fn release(&self) {
        *self.gate.0.lock().expect("gate") = true;
        self.gate.1.notify_all();
    }

    fn heard(&self) -> Vec<Heard> {
        self.heard.lock().expect("heard").clone()
    }
}

impl LiveOutput for GatedSink {
    fn started(&self, _call_id: &str, _execution_id: &str) {
        self.heard.lock().expect("heard").push(Heard::Started);
    }

    fn chunk(&self, _call_id: &str, _stream: LiveStream, data: &[u8]) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.gate.0.lock().expect("gate");
        while !*released {
            released = self.gate.1.wait(released).expect("gate wait");
        }
        drop(released);
        self.heard
            .lock()
            .expect("heard")
            .push(Heard::Chunk(data.len()));
    }

    fn gap(&self, _call_id: &str, _stream: LiveStream, dropped: u64, known: bool) {
        self.heard
            .lock()
            .expect("heard")
            .push(Heard::Gap(dropped, known));
    }

    fn finished(&self, _call_id: &str, _stopped: Option<&str>) {
        self.heard.lock().expect("heard").push(Heard::Finished);
    }
}

type Ended = (Result<SessionOutcome, HarnessError>, TestHarness);

/// Run `harness`'s session on a runtime on a thread of its own, so a sink
/// that blocked it could not block the test too.
fn run_on_own_thread(
    mut harness: TestHarness,
    cancel: CancellationToken,
) -> std::sync::mpsc::Receiver<Ended> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let ended = runtime.block_on(harness.run(&SessionTask::demo(), cancel));
        let _ = sender.send((ended, harness));
    });
    receiver
}

/// The pump's chunk-data bound, as `src/live.rs` sets it.
const LIVE_QUEUE_BYTES_IN_TESTS: usize = 1 << 20;

#[test]
fn a_blocked_live_sink_never_holds_up_the_session() {
    // 3 MiB of output in 64 KiB chunks while the sink is stuck in the first
    // one: the session finishes regardless, and once released the sink
    // hears every byte, shown or as a gap, in order.
    const CHUNK: usize = 64 << 10;
    const CHUNKS: usize = 48;
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(StreamScript {
        events: Some(
            (0..CHUNKS)
                .map(|index| {
                    StreamEvent::Chunk(
                        LiveStream::Stdout,
                        (index * CHUNK) as u64,
                        vec![b'y'; CHUNK],
                    )
                })
                .collect(),
        ),
        end: StreamEnd::Finished,
    });
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let sink = Arc::new(GatedSink::default());
    let harness = session(&api, provider, short_lifecycle_config())
        .with_live_output(Arc::clone(&sink) as Arc<dyn LiveOutput>);
    let ended =
        run_on_own_thread(harness, CancellationToken::new()).recv_timeout(Duration::from_secs(5));
    // The writer thread may lag the session; no chunk has reached the sink,
    // and it is, or soon will be, stuck in the first.
    let stuck = !sink
        .heard()
        .iter()
        .any(|event| matches!(event, Heard::Chunk(_)));
    let (outcome, harness) = ended.expect("the session never waits for the sink");
    outcome.expect("completes");
    let entered_by = std::time::Instant::now() + Duration::from_secs(5);
    while !sink.entered.load(Ordering::Acquire) && std::time::Instant::now() < entered_by {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(sink.entered.load(Ordering::Acquire));
    assert!(stuck, "the sink was still stuck: {:?}", sink.heard());
    assert!(!harness.drain_live_output(Duration::from_millis(10)));

    sink.release();
    drain(&harness);
    // In order: started, then chunks and gaps, then finished. Every byte is
    // either shown or in a gap, a gap is never followed by another (they
    // merge), and while the sink was stuck some output had to be left out.
    // How much was shown depends on when the writer took the first chunk.
    let heard = sink.heard();
    assert_eq!(heard.first(), Some(&Heard::Started), "{heard:?}");
    assert_eq!(heard.last(), Some(&Heard::Finished), "{heard:?}");
    let middle = &heard[1..heard.len() - 1];
    let (mut shown, mut left_out) = (0u64, 0u64);
    for (index, event) in middle.iter().enumerate() {
        match event {
            Heard::Chunk(length) => shown += *length as u64,
            Heard::Gap(dropped, known) => {
                assert!(*known, "{heard:?}");
                assert!(
                    !matches!(middle.get(index + 1), Some(Heard::Gap(..))),
                    "{heard:?}"
                );
                left_out += dropped;
            }
            other => panic!("{other:?} in the middle: {heard:?}"),
        }
    }
    assert_eq!(shown + left_out, (CHUNK * CHUNKS) as u64, "{heard:?}");
    assert!(shown > 0 && left_out > 0, "{heard:?}");
    assert!(
        shown <= (LIVE_QUEUE_BYTES_IN_TESTS + 2 * CHUNK) as u64,
        "{heard:?}"
    );
}

#[test]
fn a_blocked_live_sink_never_holds_up_cancellation_or_cleanup() {
    // M4-04: the command writes a line, then runs on; the sink is stuck in
    // that line when the session is canceled. Cleanup deletes the sandbox
    // while the sink is still stuck.
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(StreamScript {
        events: Some(vec![
            StreamEvent::Chunk(LiveStream::Stdout, 0, b"one chunk\n".to_vec()),
            StreamEvent::Sleep(Duration::from_secs(60)),
        ]),
        end: StreamEnd::Finished,
    });
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let sink = Arc::new(GatedSink::default());
    let cancel = CancellationToken::new();
    let harness = session(&api, provider, short_lifecycle_config())
        .with_live_output(Arc::clone(&sink) as Arc<dyn LiveOutput>);
    let ended = run_on_own_thread(harness, cancel.clone());
    let entered_by = std::time::Instant::now() + Duration::from_secs(5);
    while !sink.entered.load(Ordering::Acquire) && std::time::Instant::now() < entered_by {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(sink.entered.load(Ordering::Acquire), "the sink was entered");
    cancel.cancel();
    let ended = ended.recv_timeout(Duration::from_secs(5));
    let deleted = !api.state().deleted.is_empty();
    let stuck = sink.heard() == [Heard::Started];
    sink.release();
    let (outcome, _) = ended.expect("cancellation does not wait for the sink");
    assert!(
        matches!(outcome, Err(HarnessError::Canceled)),
        "{outcome:?}"
    );
    assert!(deleted, "cleanup deleted the sandbox");
    assert!(stuck, "while the sink was still stuck");
}

/// A provider that keeps each request's system prompt and tools.
struct Requests {
    inner: ScriptedProvider,
    seen: Vec<(String, Vec<ToolDefinition>)>,
}

#[async_trait]
impl Provider for Requests {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.seen
            .push((request.system.to_string(), request.tools.to_vec()));
        self.inner.complete(request).await
    }
}

#[tokio::test]
async fn a_streaming_session_states_the_cutoff_in_the_exec_description() {
    // M4-08: the model is told of the cutoff whatever system prompt the
    // session brings, the default one included, which does not say it.
    let limits = ToolLimits::default();
    let prompt = kiln_harness::providers::default_system_prompt(&limits);
    assert!(!prompt.contains("8 MiB"), "{prompt}");
    for system in [prompt.clone(), "A custom prompt.".to_string()] {
        let api = FakeApi::with_expiry();
        let mut harness = Harness::new(
            api.clone(),
            Requests {
                inner: ScriptedProvider::new([wc(), ModelResponse::final_text("done")]),
                seen: Vec::new(),
            },
            MemoryAudit::default(),
            MemoryJournal::default(),
            short_lifecycle_config(),
        );
        let task = SessionTask::new(&system, "Count x.");
        harness
            .run(&task, CancellationToken::new())
            .await
            .expect("completes");
        let (sent_system, tools) = &harness.provider().seen[0];
        assert_eq!(*sent_system, system);
        assert!(!sent_system.contains("8 MiB"));
        let exec = tools.iter().find(|tool| tool.name == "exec").expect("exec");
        assert!(
            exec.description
                .ends_with(" A command whose stdout or stderr passes 8 MiB is stopped."),
            "{}",
            exec.description
        );
        assert!(exec.description.ends_with(EXEC_STREAM_DESCRIPTION));
        // Every other definition is the earlier context's, unchanged.
        let earlier = ToolRegistry::new(limits.clone());
        assert_eq!(tools[1..], earlier.definitions()[1..]);
        assert_eq!(
            exec.description,
            format!(
                "{}{EXEC_STREAM_DESCRIPTION}",
                earlier.definitions()[0].description
            )
        );
        assert!(!earlier.definitions()[0].description.contains("8 MiB"));
    }
}

/// Changes the fake kilnd once the model is first asked, after seeding and
/// the baseline: what follows applies to the model's calls only.
type Arm = Box<dyn Fn(&mut common::FakeState) + Send>;

struct ArmedAtTurnOne {
    api: FakeApi,
    arm: Option<Arm>,
    inner: ScriptedProvider,
}

#[async_trait]
impl Provider for ArmedAtTurnOne {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        if let Some(arm) = self.arm.take() {
            arm(&mut self.api.state());
        }
        self.inner.complete(request).await
    }
}

/// One way a model call can name no execution: its name, how kilnd is
/// changed at turn 1, and when the session is canceled, if it is.
type SetupCase = (&'static str, fn() -> Arm, Option<Duration>);

/// How a seeded or unseeded session with one `exec`, its kilnd changed by
/// `arm` at turn 1 and canceled after `cancel_after` if set, ended: the
/// error, if any, the call's result and IDs, and the seed's IDs.
async fn setup_then_one_exec(
    seeded: bool,
    arm: Arm,
    cancel_after: Option<Duration>,
) -> (Option<HarnessError>, Value, Vec<String>, Vec<String>) {
    let api = FakeApi::with_expiry();
    let task = SessionTask {
        seed: seeded
            .then(|| SeedFile {
                path: "notes.txt".to_string(),
                contents: b"x".to_vec(),
            })
            .into_iter()
            .collect(),
        ..SessionTask::new("Use the tools.", "Count x.")
    };
    let mut harness = Harness::new(
        api.clone(),
        ArmedAtTurnOne {
            api: api.clone(),
            arm: Some(arm),
            inner: ScriptedProvider::new([
                exec("c1", &["/usr/bin/wc", "/workspace/notes.txt"]),
                ModelResponse::final_text("done"),
            ]),
        },
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let cancel = CancellationToken::new();
    if let Some(after) = cancel_after {
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            trigger.cancel();
        });
    }
    let error = harness.run(&task, cancel).await.err();
    let records = &harness.journal().records;
    let (result, ids) = tool_results_of_records(records)
        .into_iter()
        .next()
        .expect("the call is journaled");
    let seed_ids = harness
        .audit()
        .records
        .iter()
        .find_map(|record| match &record.event {
            AuditEvent::WorkspaceSeeded { execution_ids, .. } => Some(execution_ids.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(seed_ids.is_empty(), !seeded);
    (error, result, ids, seed_ids)
}

fn tool_results_of_records(
    records: &[kiln_harness::journal::JournalRecord],
) -> Vec<(Value, Vec<String>)> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted {
                result,
                execution_ids,
                ..
            } => Some((result.clone(), execution_ids.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_seed_row_is_never_taken_for_a_model_calls() {
    // M4-09: recovery after a model call that named no execution looks
    // for rows the harness has not seen; the seed's are known, so a seeded
    // session ends exactly as an unseeded one does.
    let busy = || {
        api_error_with_reason(
            Code::ResourceExhausted,
            "exec_stream_limit",
            "exec_stream_limit: stream capacity is busy",
        )
    };
    let cases: [SetupCase; 5] = [
        (
            "capacity busy for the whole call",
            || {
                Box::new(|state: &mut common::FakeState| {
                    state.tool_exec_error = Some(api_error_with_reason(
                        Code::ResourceExhausted,
                        "exec_stream_limit",
                        "exec_stream_limit: stream capacity is busy",
                    ));
                })
            },
            None,
        ),
        (
            "raced twice",
            || {
                Box::new(|state: &mut common::FakeState| {
                    let raced = api_error_with_reason(
                        Code::FailedPrecondition,
                        "execution_start_raced",
                        "execution_start_raced: the sandbox changed state",
                    );
                    state.exec_refusals.extend([raced.clone(), raced]);
                })
            },
            None,
        ),
        (
            "broken before its start",
            || {
                Box::new(|state: &mut common::FakeState| {
                    state.stream_script.push_back(StreamScript {
                        events: None,
                        end: StreamEnd::BrokenBeforeStarted(api_error(
                            Code::Unavailable,
                            "stream broke",
                        )),
                    });
                })
            },
            None,
        ),
        (
            "canceled before its start",
            || {
                Box::new(|state: &mut common::FakeState| {
                    state.tool_exec_error = Some(api_error(Code::Unavailable, "slow refusal"));
                    state.tool_exec_delay = Duration::from_secs(60);
                })
            },
            Some(Duration::from_secs(1)),
        ),
        (
            "deadline before its start",
            || {
                Box::new(|state: &mut common::FakeState| {
                    state.tool_exec_error = Some(api_error(Code::Unavailable, "slow refusal"));
                    state.tool_exec_delay = Duration::from_secs(60);
                })
            },
            None,
        ),
    ];
    for (name, arm, cancel_after) in cases {
        let unseeded = setup_then_one_exec(false, arm(), cancel_after).await;
        let seeded = setup_then_one_exec(true, arm(), cancel_after).await;
        let (error, result, ids, seed_ids) = &seeded;
        assert!(!seed_ids.is_empty(), "{name}");
        assert!(
            ids.iter().all(|id| !seed_ids.contains(id)),
            "{name}: a seed ID {ids:?} was attached to the call"
        );
        // Same ending, same result, and the same number of IDs (a row the
        // call made has another number after a seed).
        assert_eq!(
            error.as_ref().map(ToString::to_string),
            unseeded.0.as_ref().map(ToString::to_string),
            "{name}"
        );
        assert_eq!(*result, unseeded.1, "{name}");
        assert_eq!(ids.len(), unseeded.2.len(), "{name}");
    }

    // The ordinary endings, spelled out.
    let (error, result, ids, _) = setup_then_one_exec(
        true,
        Box::new(move |state: &mut common::FakeState| state.tool_exec_error = Some(busy())),
        None,
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(result["error"], "kilnd_error");
    assert!(ids.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_cancellation_during_recovery_keeps_the_started_execution() {
    // M4-10: the stream named ex-2, the call hit its 5 s deadline with the
    // row still running, and the session was canceled 20 ms into the
    // recovery that followed. The ID stays evidence, out of the result.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_delay = Duration::from_secs(60);
    api.state().running_exec = Some((2, u32::MAX));
    api.state().recent_unavailable = true;
    let provider = ScriptedProvider::new([wc(), ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5_020)).await;
        trigger.cancel();
    });
    let error = harness
        .run(&SessionTask::demo(), cancel)
        .await
        .expect_err("the deadline ends the session");
    // The call's own stop keeps precedence.
    assert!(matches!(error, HarnessError::Deadline), "{error:?}");
    assert!(
        api.state()
            .get_execution_ids
            .iter()
            .any(|id| id == "ex-0000000000000002"),
        "recovery was following the ID when canceled"
    );
    let results = tool_results(&harness);
    assert_eq!(results[0].1, ["ex-0000000000000002"]);
    assert_eq!(results[0].0["error"], "tool_interrupted");
    assert!(
        !results[0].0.to_string().contains("ex-"),
        "{}",
        results[0].0
    );
    let audited = tool_evidence(&harness.audit().records);
    assert!(
        matches!(&audited[0], AuditEvent::ToolCompleted { execution_ids, .. }
        if execution_ids == &["ex-0000000000000002".to_string()])
    );
}

fn interrupted_unary(row: &mut Execution) {
    interrupted_by_restart(row);
    row.mode = ExecutionMode::Unary;
}

fn interrupted_by_restart(row: &mut Execution) {
    row.state = ExecutionState::Interrupted;
    row.exit_code = None;
    row.failure_reason = "kilnd stopped while the execution was running".to_string();
    row.stdout = CapturedOutput::default();
    row.stderr = CapturedOutput::default();
    row.sandbox_reusable = None;
    row.cancel_reason = None;
}

#[tokio::test]
async fn a_restart_gives_an_interrupted_result_whenever_it_came() {
    // M4-11: kilnd's restart reconciliation keeps no output for a running
    // row, whatever its storage mode. After live output as before any, the
    // row is the ordinary interrupted result, not a storage fault.
    let mut outcomes = Vec::new();
    for events in [
        vec![StreamEvent::Chunk(
            LiveStream::Stdout,
            0,
            b"compiling...\n".to_vec(),
        )],
        Vec::new(),
    ] {
        let api = FakeApi::with_expiry();
        api.state().tamper_exec = Some((2, interrupted_by_restart));
        api.state().stream_script.push_back(StreamScript {
            events: Some(events),
            end: StreamEnd::Broken(api_error(Code::Unavailable, "connection reset")),
        });
        let mut harness = session(&api, two_execs(), short_lifecycle_config());
        run(&mut harness).await.expect("the session goes on");
        let results = tool_results(&harness);
        assert_eq!(results[0].0["state"], "interrupted", "{}", results[0].0);
        assert_eq!(results[0].1, ["ex-0000000000000002"]);
        outcomes.push(results[0].clone());
    }
    assert_eq!(outcomes[0], outcomes[1]);

    // Interrupted rows still get the context and mode checks.
    let api = FakeApi::with_expiry();
    api.state().tamper_exec = Some((2, interrupted_unary));
    api.state().stream_script.push_back(StreamScript {
        events: None,
        end: StreamEnd::Broken(api_error(Code::Unavailable, "connection reset")),
    });
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("another mode");
    ended_unusable(&api, &harness, &error, EXEC_MODE_MISMATCH);
}

#[tokio::test]
async fn a_protocol_violation_ends_the_session_without_reading_the_row() {
    // M4-12, through the fake: after live output kilnd breaks the stream's
    // rules. Nothing it sends for the command is trusted, its row neither.
    let api = FakeApi::with_expiry();
    api.state().stream_script.push_back(StreamScript {
        events: Some(vec![StreamEvent::Chunk(
            LiveStream::Stdout,
            0,
            b"partial\n".to_vec(),
        )]),
        end: StreamEnd::Violation,
    });
    let mut harness = session(&api, two_execs(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("a broken protocol");
    ended_unusable(&api, &harness, &error, STREAM_PROTOCOL_VIOLATION);
    let (result, _) = &tool_results(&harness)[0];
    assert!(!result.to_string().contains("ex-"), "{result}");
    assert!(result.get("stopped").is_none(), "{result}");
    assert!(
        api.state().get_execution_ids.is_empty(),
        "the row was not read"
    );
}

#[test]
fn an_unbounded_drain_never_panics() {
    // M4-13: a wait too long to be a point in time waits without a
    // deadline; on an idle pump that is no wait at all.
    let harness = session(
        &FakeApi::with_expiry(),
        ScriptedProvider::new([]),
        short_lifecycle_config(),
    )
    .with_live_output(Arc::new(Recorder::default()) as Arc<dyn LiveOutput>);
    let begun = std::time::Instant::now();
    assert!(harness.drain_live_output(Duration::MAX));
    assert!(begun.elapsed() < Duration::from_secs(1));
}
