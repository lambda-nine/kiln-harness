//! Discovery, typed reasons, sandbox expiry on create, and the renew
//! heartbeat during provider calls, against the fake kilnd. Heartbeat tests
//! run on paused time, so their minutes take no wall time.
mod common;

use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use common::{
    FakeApi, RenewScript, api_error, api_error_with_reason, kilnd_limits, short_lifecycle_config,
};
use kiln_harness::api::{ApiLimits, ExpiryPolicy, ExpiryReason, Feature};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, MemoryJournal, read_journal};
use kiln_harness::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, ProviderInfo,
    ScriptedProvider, ToolCall,
};
use kiln_harness::replay::ReplayProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::Code;

/// Answers each request from a script after `delay`, optionally expiring
/// the fake's sandbox when it answers `expire_at_turn`.
struct Script {
    responses: VecDeque<ModelResponse>,
    delay: Duration,
    expire: Option<(u32, FakeApi, &'static str)>,
    calls: u32,
}

impl Script {
    fn new(delay: Duration, responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            delay,
            expire: None,
            calls: 0,
        }
    }

    fn final_after(delay: Duration) -> Self {
        Self::new(delay, [ModelResponse::final_text("done")])
    }

    fn expiring(mut self, turn: u32, api: &FakeApi, reason: &'static str) -> Self {
        self.expire = Some((turn, api.clone(), reason));
        self
    }
}

#[async_trait]
impl Provider for Script {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.calls += 1;
        tokio::time::sleep(self.delay).await;
        if let Some((turn, api, reason)) = &self.expire
            && *turn == request.turn
        {
            api.expire(reason);
        }
        self.responses
            .pop_front()
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::Script, "script exhausted"))
    }
}

fn exec_call() -> ModelResponse {
    ModelResponse::tool_use([ToolCall::new(
        "c1",
        "exec",
        json!({"argv": ["/usr/bin/wc", "/workspace/x"]}),
    )])
}

/// A long session, so heartbeat tests are bounded by their provider delays.
fn long_config(idle_secs: u64) -> HarnessConfig {
    HarnessConfig {
        session_timeout: Duration::from_secs(600),
        sandbox_idle_timeout: Duration::from_secs(idle_secs),
        ..short_lifecycle_config()
    }
}

type TestHarness<P> = Harness<FakeApi, P, MemoryAudit, MemoryJournal>;

fn session<P: Provider>(api: &FakeApi, provider: P, config: HarnessConfig) -> TestHarness<P> {
    Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    )
}

async fn run<P: Provider>(harness: &mut TestHarness<P>) -> Result<SessionOutcome, HarnessError> {
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
}

fn audit_events<P: Provider>(harness: &TestHarness<P>) -> Vec<&AuditEvent> {
    harness
        .audit()
        .records
        .iter()
        .map(|record| &record.event)
        .collect()
}

fn journal_events<P: Provider>(harness: &TestHarness<P>) -> Vec<&JournalEvent> {
    harness
        .journal()
        .records
        .iter()
        .map(|record| &record.event)
        .collect()
}

/// The terminal audit record's outcome, deletion, and renew count.
fn finished<P: Provider>(harness: &TestHarness<P>) -> (String, bool, u32) {
    match audit_events(harness).last() {
        Some(AuditEvent::SessionFinished {
            outcome,
            sandbox_deleted,
            renews,
            ..
        }) => (outcome.clone(), *sandbox_deleted, *renews),
        other => panic!("no session_finished: {other:?}"),
    }
}

fn expired(error: &HarnessError) -> Option<ExpiryReason> {
    match error {
        HarnessError::SandboxExpired { reason } => Some(*reason),
        _ => None,
    }
}

#[tokio::test(start_paused = true)]
async fn an_old_daemon_gets_no_expiry_and_no_renews() {
    let api = FakeApi::default();
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(400)),
        long_config(30),
    );
    let outcome = run(&mut harness).await.expect("session completes");

    assert_eq!(outcome.renews, 0);
    let state = api.state();
    assert_eq!(state.health_calls, 1, "one discovery per session");
    assert_eq!(state.creates[0].expiry, None);
    assert_eq!(state.renews, 0);
    drop(state);
    let events = audit_events(&harness);
    assert!(matches!(
        events[0],
        AuditEvent::SessionOpened { features, .. } if features.is_empty()
    ));
    assert!(matches!(
        events[1],
        AuditEvent::SandboxCreated {
            expiry_lifetime_ms: None,
            expiry_idle_ms: None,
            ..
        }
    ));
    // The degraded mode is explicit in the audit as null.
    let created = serde_json::to_value(events[1]).expect("encode");
    assert_eq!(created["expiry_lifetime_ms"], Value::Null);
    assert_eq!(created["expiry_idle_ms"], Value::Null);
    assert_eq!(finished(&harness), ("completed".to_string(), true, 0));
    // The journal is byte-identical to one written before expiry existed.
    let ready = harness
        .journal()
        .lines
        .iter()
        .find(|line| line.contains("\"sandbox_ready\""))
        .expect("sandbox_ready");
    assert!(!ready.contains("expiry"), "{ready}");
}

#[tokio::test]
async fn expiry_is_sent_and_recorded_when_listed() {
    let api = FakeApi::with_expiry();
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    let outcome = run(&mut harness).await.expect("demo completes");

    // 30 s session + 10 s cleanup + 60 s margin; the default idle timeout.
    let sent = ExpiryPolicy {
        max_lifetime_ms: 100_000,
        idle_timeout_ms: 120_000,
    };
    assert_eq!(api.state().creates[0].expiry, Some(sent));
    // An instant provider never waits long enough to renew.
    assert_eq!(outcome.renews, 0);
    let events = audit_events(&harness);
    assert!(matches!(
        events[0],
        AuditEvent::SessionOpened { features, .. }
            if features == &["sandbox_expiry".to_string(), "typed_reasons".to_string()]
    ));
    assert!(matches!(
        events[1],
        AuditEvent::SandboxCreated {
            expiry_lifetime_ms: Some(100_000),
            expiry_idle_ms: Some(120_000),
            ..
        }
    ));
    assert!(journal_events(&harness).iter().any(|event| matches!(
        event,
        JournalEvent::SandboxReady {
            expiry_lifetime_ms: Some(100_000),
            expiry_idle_ms: Some(120_000),
            ..
        }
    )));
}

#[tokio::test]
async fn expiry_is_clamped_into_the_daemons_bounds() {
    let api = FakeApi::with_expiry();
    api.state().limits = Some(ApiLimits {
        min_sandbox_lifetime_ms: 200_000,
        max_sandbox_idle_ms: 30_000,
        ..kilnd_limits()
    });
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    run(&mut harness).await.expect("demo completes");
    assert_eq!(
        api.state().creates[0].expiry,
        Some(ExpiryPolicy {
            max_lifetime_ms: 200_000,
            idle_timeout_ms: 30_000,
        })
    );

    // A cap of 0 is no cap.
    let api = FakeApi::with_expiry();
    api.state().limits = Some(ApiLimits {
        max_sandbox_lifetime_ms: 0,
        max_sandbox_idle_ms: 0,
        ..kilnd_limits()
    });
    let config = HarnessConfig {
        session_timeout: Duration::from_secs(6 * 60 * 60),
        sandbox_idle_timeout: Duration::from_secs(3600),
        ..HarnessConfig::default()
    };
    let mut harness = session(&api, ScriptedProvider::demo(), config);
    run(&mut harness).await.expect("demo completes");
    assert_eq!(
        api.state().creates[0].expiry,
        Some(ExpiryPolicy {
            max_lifetime_ms: 21_670_000,
            idle_timeout_ms: 3_600_000,
        })
    );
}

#[tokio::test]
async fn a_lifetime_cap_shorter_than_the_session_is_refused_before_create() {
    let api = FakeApi::with_expiry();
    // The session needs 30 s + 10 s of cleanup; the cap leaves less.
    api.state().limits = Some(ApiLimits {
        max_sandbox_lifetime_ms: 39_999,
        ..kilnd_limits()
    });
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("refused");
    assert!(
        matches!(&error, HarnessError::InvalidTask(message)
            if message == "daemon max lifetime is shorter than the session"),
        "{error}"
    );
    assert!(api.state().creates.is_empty());
    assert!(harness.audit().records.is_empty());

    // Exactly the session and its cleanup window is enough.
    api.state().limits = Some(ApiLimits {
        max_sandbox_lifetime_ms: 40_000,
        ..kilnd_limits()
    });
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    run(&mut harness).await.expect("demo completes");
    assert_eq!(
        api.state().creates[0]
            .expiry
            .map(|policy| policy.max_lifetime_ms),
        Some(40_000)
    );
}

#[tokio::test]
async fn expiry_settings_are_validated() {
    for (idle, margin) in [
        (Duration::from_secs(9), Duration::from_secs(60)),
        (Duration::from_secs(3601), Duration::from_secs(60)),
        (Duration::from_millis(10_000_500), Duration::from_secs(60)),
        (Duration::from_micros(30_000_001), Duration::from_secs(60)),
        (Duration::from_secs(120), Duration::ZERO),
        (Duration::from_secs(120), Duration::from_secs(601)),
    ] {
        let config = HarnessConfig {
            sandbox_idle_timeout: idle,
            sandbox_lifetime_margin: margin,
            ..HarnessConfig::default()
        };
        assert!(config.validate().is_err(), "{idle:?} {margin:?}");
        let api = FakeApi::with_expiry();
        let mut harness = session(&api, ScriptedProvider::demo(), config);
        run(&mut harness).await.expect_err("refused");
        assert_eq!(api.state().health_calls, 0, "refused before kilnd");
    }
}

#[tokio::test]
async fn a_daemon_that_is_not_serving_is_refused_before_create() {
    let api = FakeApi::with_expiry();
    api.state().not_serving = true;
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("refused");
    assert!(
        matches!(&error, HarnessError::Api(error) if error.code == Code::Unavailable),
        "{error}"
    );
    assert!(api.state().creates.is_empty());
    assert!(harness.audit().records.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_slow_model_call_is_renewed_and_completes() {
    let api = FakeApi::with_expiry();
    // Idle 30 s renews every 10 s; the call takes just over three intervals.
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(31)),
        long_config(30),
    );
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(outcome.renews, 3);
    assert_eq!(api.state().renews, 3);
    assert_eq!(finished(&harness), ("completed".to_string(), true, 3));
    // A heartbeat leaves no records of its own.
    assert_eq!(harness.journal().records.len(), 5);
}

#[tokio::test(start_paused = true)]
async fn renews_happen_only_during_model_calls() {
    let api = FakeApi::with_expiry();
    // A slow tool call needs no heartbeat: the execution is activity.
    api.state().tool_exec_delay = Duration::from_secs(50);
    let mut harness = session(
        &api,
        Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        HarnessConfig {
            tool_limits: kiln_harness::tools::ToolLimits {
                timeout: Duration::from_secs(60),
                ..kiln_harness::tools::ToolLimits::default()
            },
            ..long_config(30)
        },
    );
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(outcome.renews, 0);
    assert_eq!(api.state().renews, 0);
}

#[tokio::test(start_paused = true)]
async fn an_expiry_reason_during_a_model_call_ends_the_session_and_cleans_up() {
    for (reason, expected) in [
        ("expired_idle", ExpiryReason::ExpiredIdle),
        ("expired_lifetime", ExpiryReason::ExpiredLifetime),
        ("sandbox_deleted", ExpiryReason::SandboxDeleted),
    ] {
        let api = FakeApi::with_expiry();
        api.state().renew_script = VecDeque::from([RenewScript::Ok, RenewScript::Expire(reason)]);
        let provider = Script::final_after(Duration::from_secs(600));
        let mut harness = session(&api, provider, long_config(30));
        let error = run(&mut harness).await.expect_err("sandbox expired");
        assert_eq!(expired(&error), Some(expected), "{error}");
        // The call was dropped at the second renew, not left to finish.
        assert_eq!(api.state().renews, 2);
        // Cleanup still deletes and verifies absence; the expired row has
        // already left the listing.
        assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
        assert_eq!(finished(&harness), ("failed".to_string(), true, 1));
        assert!(matches!(
            journal_events(&harness).last(),
            Some(JournalEvent::SessionEnded {
                work_outcome,
                sandbox_deleted: true,
                ..
            }) if work_outcome == "failed"
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn transient_renew_failures_that_recover_do_not_fail_the_session() {
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([
        RenewScript::Fail(api_error(Code::Unavailable, "busy")),
        RenewScript::Ok,
        RenewScript::Fail(api_error(Code::Unknown, "transport closed")),
        RenewScript::Ok,
    ]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(45)),
        long_config(30),
    );
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().renews, 4);
    assert_eq!(outcome.renews, 2);
}

#[tokio::test(start_paused = true)]
async fn repeated_transient_renew_failures_end_with_renew_failed() {
    // Idle 30 s, interval 10 s: two failures in a row are the limit.
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([
        RenewScript::Ok,
        RenewScript::Fail(api_error(Code::Unavailable, "busy")),
        RenewScript::Fail(api_error(Code::DeadlineExceeded, "slow")),
    ]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(600)),
        long_config(30),
    );
    let error = run(&mut harness).await.expect_err("renew failed");
    assert_eq!(expired(&error), Some(ExpiryReason::RenewFailed), "{error}");
    assert_eq!(api.state().renews, 3);
    assert_eq!(finished(&harness), ("failed".to_string(), true, 1));

    // A renew that never answers times out at the next tick and counts too.
    let api = FakeApi::with_expiry();
    api.state().renew_delay = Duration::from_secs(3600);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(600)),
        long_config(30),
    );
    let started = tokio::time::Instant::now();
    let error = run(&mut harness).await.expect_err("renew failed");
    assert_eq!(expired(&error), Some(ExpiryReason::RenewFailed), "{error}");
    assert_eq!(api.state().renews, 2);
    // Ticks at 10 s and 20 s; the second times out at 30 s.
    assert!(started.elapsed() < Duration::from_secs(31));
}

#[tokio::test(start_paused = true)]
async fn any_other_renew_error_is_fatal() {
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([RenewScript::Fail(api_error(
        Code::NotFound,
        "no such sandbox",
    ))]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(600)),
        long_config(30),
    );
    let error = run(&mut harness).await.expect_err("fatal");
    assert!(
        matches!(&error, HarnessError::Api(error) if error.code == Code::NotFound),
        "{error}"
    );
    assert_eq!(api.state().renews, 1);
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));

    // FAILED_PRECONDITION for another reason, such as a lost sandbox, too.
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([RenewScript::Fail(api_error_with_reason(
        Code::FailedPrecondition,
        "sandbox_lost",
        "sandbox_lost: sandbox is lost, not ready",
    ))]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(600)),
        long_config(30),
    );
    let error = run(&mut harness).await.expect_err("fatal");
    assert!(matches!(&error, HarnessError::Api(_)), "{error}");
}

#[tokio::test]
async fn an_expired_sandbox_ends_the_session_at_its_next_tool_call() {
    let api = FakeApi::with_expiry();
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    )
    .expiring(1, &api, "expired_lifetime");
    let mut harness = session(&api, provider, short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("sandbox expired");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::ExpiredLifetime),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 1, "the model is not asked again");
    // The tool record and its model-visible result are as before typed
    // reasons; the session then ends and cleanup verifies absence.
    let tool = journal_events(&harness)
        .into_iter()
        .find_map(|event| match event {
            JournalEvent::ToolExecuted { ok, result, .. } => Some((*ok, result.clone())),
            _ => None,
        })
        .expect("tool_executed");
    assert!(!tool.0);
    assert_eq!(tool.1["error"], "kilnd_error");
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
}

#[tokio::test]
async fn without_typed_reasons_an_expiry_refusal_is_only_a_tool_error() {
    let api = FakeApi::with_expiry();
    api.state().features = BTreeSet::from([Feature::SandboxExpiry]);
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    )
    .expiring(1, &api, "expired_idle");
    let mut harness = session(&api, provider, short_lifecycle_config());
    let outcome = run(&mut harness)
        .await
        .expect("the loop continues as before");
    assert_eq!(outcome.steps, 2);
    assert_eq!(finished(&harness), ("completed".to_string(), true, 0));
}

#[tokio::test]
async fn an_expired_sandbox_during_the_baseline_ends_the_session() {
    let api = FakeApi::with_expiry();
    api.expire("sandbox_deleted");
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("sandbox expired");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::SandboxDeleted),
        "{error}"
    );
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
}

#[tokio::test]
async fn backend_unsupported_is_a_definite_create_rejection_only_with_typed_reasons() {
    let rejection = api_error_with_reason(
        Code::Unimplemented,
        "backend_unsupported:exec_env",
        "backend_unsupported:exec_env: the backend cannot set exec env",
    );
    // With the feature: nothing was allocated, so absence is confirmed.
    let api = FakeApi::with_expiry();
    api.state().create_rejection = Some(rejection.clone());
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("rejected");
    assert!(matches!(&error, HarnessError::Api(_)), "{error}");
    assert_eq!(finished(&harness), ("create_failed".to_string(), true, 0));

    // Without the feature the trailer means nothing: ambiguous, as before.
    let api = FakeApi::default();
    api.state().create_rejection = Some(rejection);
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("rejected");
    assert!(
        matches!(&error, HarnessError::CreateUncertain(_)),
        "{error}"
    );

    // UNIMPLEMENTED without a reason stays ambiguous, and so does another
    // reason.
    for reason in [
        None,
        Some("sandbox_unsupported:exec_env"),
        Some("backend_unsupported:"),
    ] {
        let api = FakeApi::with_expiry();
        let mut error = api_error(Code::Unimplemented, "unknown method");
        error.reason = reason.map(str::to_string);
        api.state().create_rejection = Some(error);
        let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
        let error = run(&mut harness).await.expect_err("rejected");
        assert!(
            matches!(&error, HarnessError::CreateUncertain(_)),
            "{error}"
        );
    }
}

#[test]
fn sandbox_ready_expiry_is_a_bounded_pair() {
    // The recorded session timeout is 30 s.
    let ceiling = 6 * 3_600_000 + 60_000 + 600_000;
    let temp = common::private_tempdir().expect("tempdir");
    for (lifetime, idle, valid) in [
        (None, None, true),
        (Some(100_000), Some(120_000), true),
        (Some(100_000), None, false),
        (None, Some(120_000), false),
        (Some(0), Some(120_000), false),
        (Some(100_000), Some(0), false),
        // F-06: an expiry the engine could never have sent for the session.
        (Some(1), Some(1), false),
        (Some(30_000), Some(120_000), false),
        (Some(30_001), Some(10_000), true),
        (Some(100_000), Some(9_999), false),
        (Some(100_000), Some(3_600_000), true),
        (Some(100_000), Some(3_600_001), false),
        (Some(ceiling), Some(120_000), true),
        (Some(ceiling + 1), Some(120_000), false),
    ] {
        let path = temp.path().join(format!("{lifetime:?}-{idle:?}.jsonl"));
        let lines = common::chained_lines(
            "hs-expiry",
            vec![
                common_session_started(),
                JournalEvent::SandboxReady {
                    sandbox_id: "sb-1".to_string(),
                    baseline_file_count: 0,
                    expiry_lifetime_ms: lifetime,
                    expiry_idle_ms: idle,
                },
            ],
        );
        common::write_journal_lines(&path, &lines);
        assert_eq!(
            read_journal(&path).is_ok(),
            valid,
            "{lifetime:?} {idle:?}: {:?}",
            read_journal(&path).err()
        );
    }
}

fn common_session_started() -> JournalEvent {
    JournalEvent::SessionStarted {
        schema: kiln_harness::journal::JOURNAL_SCHEMA,
        harness_version: env!("CARGO_PKG_VERSION").to_string(),
        provider: ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        },
        system: String::new(),
        task: "task".to_string(),
        tools: kiln_harness::tools::ToolRegistry::new(kiln_harness::tools::ToolLimits::default())
            .definitions()
            .to_vec(),
        limits: HarnessConfig::default().limits(),
    }
}

#[tokio::test]
async fn a_journal_with_expiry_verifies_and_replays() {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("recorded.jsonl");
    let api = FakeApi::with_expiry();
    let mut recording = Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    let recorded_outcome = recording
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("recording");
    drop(recording);
    let recorded = read_journal(&path).expect("journal verifies");
    assert!(recorded.complete);

    let provider = ReplayProvider::from_journal(&recorded).expect("replayable");
    let task = provider.task();
    let mut config = short_lifecycle_config();
    config.apply_limits(&provider.limits());
    let mut replay = Harness::new(
        FakeApi::with_expiry(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let replayed = replay
        .run(&task, CancellationToken::new())
        .await
        .expect("replay");
    assert_eq!(replayed.final_answer, recorded_outcome.final_answer);
    assert_eq!(replay.provider().diverged_at(), None);
    assert!(replay.provider().exhausted());
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_a_renewed_model_call_stops_both_and_cleans_up() {
    let api = FakeApi::with_expiry();
    // The renew in flight when cancellation lands never answers.
    api.state().renew_script = VecDeque::from([RenewScript::Ok]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(600)),
        long_config(30),
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let delayed = api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        delayed.state().renew_delay = Duration::from_secs(3600);
        tokio::time::sleep(Duration::from_secs(10)).await;
        trigger.cancel();
    });
    let started = tokio::time::Instant::now();
    let error = harness
        .run(&SessionTask::demo(), cancel)
        .await
        .expect_err("canceled");
    assert!(matches!(error, HarnessError::Canceled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(26));
    // Renews at 10 s (answered) and 20 s (stalled, then dropped).
    assert_eq!(api.state().renews, 2);
    assert_eq!(finished(&harness), ("canceled".to_string(), true, 1));
}

#[tokio::test(start_paused = true)]
async fn short_turns_share_one_renew_schedule() {
    // F-01: four 3 s turns with no execution at a 10 s idle timeout. Each
    // is shorter than the 3.334 s interval, but together they cross the
    // idle window, so renews must carry across turns.
    let api = FakeApi::with_expiry();
    let watcher = api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        if watcher.state().renews == 0 {
            watcher.expire("expired_idle");
        }
    });
    let unknown = |id: &str| ModelResponse::tool_use([ToolCall::new(id, "not_a_tool", json!({}))]);
    let provider = Script::new(
        Duration::from_secs(3),
        [
            unknown("u1"),
            unknown("u2"),
            unknown("u3"),
            exec_call(),
            ModelResponse::final_text("done"),
        ],
    );
    let mut harness = session(&api, provider, long_config(10));
    let outcome = run(&mut harness).await.expect("session completes");
    assert!(outcome.renews >= 3, "renews {}", outcome.renews);
    assert_eq!(api.state().expired, None);
}

#[tokio::test(start_paused = true)]
async fn executions_between_short_turns_need_no_renew() {
    let api = FakeApi::with_expiry();
    let provider = Script::new(
        Duration::from_secs(3),
        [
            exec_call(),
            exec_call(),
            exec_call(),
            exec_call(),
            ModelResponse::final_text("done"),
        ],
    );
    let mut harness = session(&api, provider, long_config(10));
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(outcome.renews, 0);
    assert_eq!(api.state().renews, 0);
}

#[tokio::test(start_paused = true)]
async fn a_renew_due_at_a_turn_boundary_is_not_skipped() {
    // Idle 30 s renews every 10 s. Each call ends exactly when a renew falls
    // due, so the renew runs at the start of the next call instead.
    let api = FakeApi::with_expiry();
    let unknown = |id: &str| ModelResponse::tool_use([ToolCall::new(id, "not_a_tool", json!({}))]);
    let provider = Script::new(
        Duration::from_secs(10),
        [
            unknown("u1"),
            unknown("u2"),
            ModelResponse::final_text("done"),
        ],
    );
    let mut harness = session(&api, provider, long_config(30));
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(outcome.renews, 2);
}

#[tokio::test(start_paused = true)]
async fn renew_admission_refusals_are_transient() {
    // F-03: one RESOURCE_EXHAUSTED, then a healthy renew.
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([
        RenewScript::Fail(api_error(Code::ResourceExhausted, "RPC admission full")),
        RenewScript::Ok,
    ]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(25)),
        long_config(30),
    );
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().renews, 2);
    assert_eq!(outcome.renews, 1);

    // Repeated refusals still exhaust the budget.
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([
        RenewScript::Fail(api_error(Code::ResourceExhausted, "RPC admission full")),
        RenewScript::Fail(api_error(Code::ResourceExhausted, "RPC admission full")),
    ]);
    let mut harness = session(
        &api,
        Script::final_after(Duration::from_secs(600)),
        long_config(30),
    );
    let error = run(&mut harness).await.expect_err("renew failed");
    assert_eq!(expired(&error), Some(ExpiryReason::RenewFailed), "{error}");
}

#[tokio::test]
async fn inconsistent_advertised_limits_are_refused_before_create() {
    // F-05: an empty range, and defaults outside the range.
    for limits in [
        ApiLimits {
            min_sandbox_idle_ms: 20_000,
            max_sandbox_idle_ms: 10_000,
            ..kilnd_limits()
        },
        ApiLimits {
            min_sandbox_lifetime_ms: 200_000,
            max_sandbox_lifetime_ms: 100_000,
            ..kilnd_limits()
        },
        ApiLimits {
            default_sandbox_idle_ms: 5_000,
            ..kilnd_limits()
        },
        ApiLimits {
            default_sandbox_lifetime_ms: 8 * 86_400_000,
            ..kilnd_limits()
        },
    ] {
        let api = FakeApi::with_expiry();
        api.state().limits = Some(limits);
        let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
        let error = run(&mut harness).await.expect_err("refused");
        assert!(
            matches!(&error, HarnessError::Api(error)
                if error.code == Code::FailedPrecondition
                    && error.message == "kilnd advertised inconsistent expiry limits"),
            "{limits:?}: {error}"
        );
        assert!(api.state().creates.is_empty());
        assert!(harness.audit().records.is_empty());
    }
    // A default inside the range is fine.
    let api = FakeApi::with_expiry();
    api.state().limits = Some(ApiLimits {
        default_sandbox_idle_ms: 60_000,
        default_sandbox_lifetime_ms: 3_600_000,
        ..kilnd_limits()
    });
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    run(&mut harness).await.expect("demo completes");
}

#[tokio::test]
async fn limits_that_leave_no_recordable_policy_are_refused() {
    // Coherent limits whose clamp would land outside what the journal
    // grammar accepts: an idle cap under kilnd's 10 s floor.
    let api = FakeApi::with_expiry();
    api.state().limits = Some(ApiLimits {
        min_sandbox_idle_ms: 0,
        max_sandbox_idle_ms: 5_000,
        ..kilnd_limits()
    });
    let mut harness = session(&api, ScriptedProvider::demo(), HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("refused");
    assert!(
        matches!(&error, HarnessError::Api(error) if error.code == Code::FailedPrecondition),
        "{error}"
    );
    assert!(api.state().creates.is_empty());
}

#[tokio::test]
async fn a_baseline_that_left_the_sandbox_unusable_ends_the_session() {
    use kiln_harness::api::CancelReason;
    // F-02 during baseline capture: a proven lifetime expiry.
    let api = FakeApi::with_expiry();
    api.state().unusable_exec = Some((1, Some(CancelReason::SandboxExpiredLifetime)));
    let mut harness = session(
        &api,
        Script::final_after(Duration::ZERO),
        short_lifecycle_config(),
    );
    let error = run(&mut harness).await.expect_err("sandbox gone");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::ExpiredLifetime),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 0);
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));

    // With no stated cause it fails closed as an API error.
    let api = FakeApi::with_expiry();
    api.state().unusable_exec = Some((1, None));
    let mut harness = session(
        &api,
        Script::final_after(Duration::ZERO),
        short_lifecycle_config(),
    );
    let error = run(&mut harness).await.expect_err("sandbox unusable");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.message == "execution left the sandbox unusable"),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 0);
}

#[tokio::test]
async fn a_tool_execution_with_an_unknown_unusable_cause_fails_closed() {
    use kiln_harness::api::CancelReason;
    for cause in [None, Some(CancelReason::DaemonShutdown)] {
        let api = FakeApi::with_expiry();
        // Exec 1 is the baseline; exec 2 is the tool's.
        api.state().unusable_exec = Some((2, cause));
        let provider = Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        );
        let mut harness = session(&api, provider, short_lifecycle_config());
        let error = run(&mut harness).await.expect_err("sandbox unusable");
        assert!(
            matches!(&error, HarnessError::Api(error)
                if error.message == "execution left the sandbox unusable"),
            "{cause:?}: {error}"
        );
        assert_eq!(harness.provider().calls, 1, "the model is not asked again");
        // The tool result is journaled before the session ends.
        assert!(
            journal_events(&harness)
                .iter()
                .any(|event| matches!(event, JournalEvent::ToolExecuted { ok: false, .. }))
        );
        assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
    }
}

#[tokio::test]
async fn a_multi_execution_tool_stops_at_the_first_unusable_result() {
    use kiln_harness::api::CancelReason;
    // read_file runs stat, then head. A non-reusable stat must not be
    // followed by another execution.
    let api = FakeApi::with_expiry();
    api.state()
        .files
        .insert("/workspace/x".to_string(), b"data".to_vec());
    api.state().unusable_exec = Some((2, Some(CancelReason::SandboxDeleted)));
    let read = ModelResponse::tool_use([ToolCall::new("r1", "read_file", json!({"path": "x"}))]);
    let provider = Script::new(Duration::ZERO, [read, ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("sandbox deleted");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::SandboxDeleted),
        "{error}"
    );
    assert_eq!(api.state().execs.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_exec_rpc_is_not_activity() {
    // R2-02: the tool Exec is refused before admission after 8 s of a 10 s
    // idle window. No activity happened, so the next model call must renew
    // at once instead of trusting an invented finish at 8 s.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error(
        Code::Unavailable,
        "request rejected before execution admission",
    ));
    api.state().tool_exec_delay = Duration::from_secs(8);
    let watcher = api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        if watcher.state().renews == 0 {
            watcher.expire("expired_idle");
        }
    });
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    );
    let mut config = long_config(10);
    config.tool_limits.timeout = Duration::from_secs(9);
    let slow_second = SlowAfterFirst {
        inner: provider,
        delay: Duration::from_millis(2_500),
    };
    let mut harness = session(&api, slow_second, config);
    let outcome = run(&mut harness).await.expect("session completes");
    assert!(outcome.renews >= 1, "renews {}", outcome.renews);
    assert_eq!(api.state().expired, None);
}

/// Answers the first request at once and later ones after `delay`.
struct SlowAfterFirst {
    inner: Script,
    delay: Duration,
}

#[async_trait]
impl Provider for SlowAfterFirst {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        if self.inner.calls > 0 {
            tokio::time::sleep(self.delay).await;
        }
        self.inner.complete(request).await
    }
}

#[tokio::test]
async fn a_typed_non_ready_exec_refusal_ends_the_session() {
    // R2-03: lost, quarantined, failed or not ready: the sandbox cannot run
    // another call in this session.
    for token in [
        "sandbox_lost",
        "sandbox_quarantined",
        "sandbox_failed",
        "sandbox_not_ready",
    ] {
        let api = FakeApi::with_expiry();
        api.state().tool_exec_error = Some(api_error_with_reason(
            Code::FailedPrecondition,
            token,
            &format!("{token}: sandbox sb-0000000000000001 is not ready"),
        ));
        let provider = Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        );
        let mut harness = session(&api, provider, short_lifecycle_config());
        let error = run(&mut harness).await.expect_err("sandbox unusable");
        assert!(
            matches!(&error, HarnessError::Api(error)
                if error.code == Code::FailedPrecondition
                    && error.message == "execution left the sandbox unusable"),
            "{token}: {error}"
        );
        assert_eq!(harness.provider().calls, 1, "{token}");
        // The model-visible result is the same kilnd_error, journaled first.
        let result = journal_events(&harness)
            .into_iter()
            .find_map(|event| match event {
                JournalEvent::ToolExecuted { result, .. } => Some(result.clone()),
                _ => None,
            })
            .expect("tool_executed");
        assert_eq!(result["error"], "kilnd_error", "{token}");
        assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
    }

    // Without typed reasons the refusal stays an ordinary tool error.
    let api = FakeApi::with_expiry();
    api.state().features = BTreeSet::from([Feature::SandboxExpiry]);
    api.state().tool_exec_error = Some(api_error_with_reason(
        Code::FailedPrecondition,
        "sandbox_lost",
        "sandbox_lost: sandbox is lost, not ready",
    ));
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    );
    let mut harness = session(&api, provider, short_lifecycle_config());
    run(&mut harness)
        .await
        .expect("the loop continues as before");
}

#[tokio::test(start_paused = true)]
async fn a_typed_non_ready_renew_refusal_is_terminal() {
    for token in [
        "sandbox_lost",
        "sandbox_quarantined",
        "sandbox_failed",
        "sandbox_not_ready",
    ] {
        let api = FakeApi::with_expiry();
        api.state().renew_script = VecDeque::from([
            RenewScript::Fail(api_error_with_reason(
                Code::FailedPrecondition,
                token,
                &format!("{token}: sandbox is not ready"),
            )),
            RenewScript::Ok,
        ]);
        let mut harness = session(
            &api,
            Script::final_after(Duration::from_secs(600)),
            long_config(30),
        );
        let error = run(&mut harness).await.expect_err("terminal");
        assert!(
            matches!(&error, HarnessError::Api(error) if error.reason.as_deref() == Some(token)),
            "{token}: {error}"
        );
        assert_eq!(api.state().renews, 1, "{token}: not retried");
    }
}

#[tokio::test]
async fn a_recovered_row_that_left_the_sandbox_unusable_ends_the_session() {
    use kiln_harness::api::CancelReason;
    // R2-01 through the fake: the tool's row is published, then its
    // response is lost, and recovery finds it.
    for (cause, expected) in [
        (
            Some(CancelReason::SandboxDeleted),
            Some(ExpiryReason::SandboxDeleted),
        ),
        (
            Some(CancelReason::SandboxExpiredLifetime),
            Some(ExpiryReason::ExpiredLifetime),
        ),
        (Some(CancelReason::DaemonShutdown), None),
        (None, None),
    ] {
        let api = FakeApi::with_expiry();
        api.state().unusable_exec = Some((2, cause));
        api.state().lose_exec_response = Some(2);
        let provider = Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        );
        let mut harness = session(&api, provider, short_lifecycle_config());
        let error = run(&mut harness).await.expect_err("sandbox gone");
        match expected {
            Some(reason) => assert_eq!(expired(&error), Some(reason), "{cause:?}: {error}"),
            None => assert!(
                matches!(&error, HarnessError::Api(error)
                    if error.message == "execution left the sandbox unusable"),
                "{cause:?}: {error}"
            ),
        }
        assert_eq!(harness.provider().calls, 1, "{cause:?}");
        assert!(journal_events(&harness).iter().any(|event| matches!(
            event,
            JournalEvent::ToolExecuted { execution_ids, .. } if !execution_ids.is_empty()
        )));
        assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
    }

    // During the baseline.
    let api = FakeApi::with_expiry();
    api.state().unusable_exec = Some((1, Some(CancelReason::SandboxExpiredLifetime)));
    api.state().lose_exec_response = Some(1);
    let mut harness = session(
        &api,
        Script::final_after(Duration::ZERO),
        short_lifecycle_config(),
    );
    let error = run(&mut harness).await.expect_err("sandbox gone");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::ExpiredLifetime),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 0);

    // A lost response whose recovered row says nothing, as from an older
    // daemon, keeps the old behavior: the loop continues.
    let api = FakeApi::default();
    api.state().lose_exec_response = Some(2);
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    );
    let mut harness = session(&api, provider, short_lifecycle_config());
    let outcome = run(&mut harness)
        .await
        .expect("the loop continues as before");
    assert_eq!(outcome.steps, 2);
}
