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
use kiln_harness::api::{ApiLimits, Deleted, ExpiryPolicy, ExpiryReason, Feature};
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
    // A slow tool call needs no heartbeat while it runs: the execution is
    // activity, and kilnd pauses idle until it finishes.
    api.state().tool_exec_delay = Duration::from_secs(50);
    let provider = ObserveRenews::new(
        Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        &api,
    );
    let mut harness = session(
        &api,
        provider,
        HarnessConfig {
            tool_limits: kiln_harness::tools::ToolLimits {
                timeout: Duration::from_secs(60),
                ..kiln_harness::tools::ToolLimits::default()
            },
            // Idle 120 s renews every 40 s, shorter than the 50 s tool.
            ..long_config(120)
        },
    );
    let outcome = run(&mut harness).await.expect("session completes");
    // Nothing renewed during the tool. Its activity is anchored where the
    // request was sent, conservatively, 50 s before the next model call, so
    // that call renews first.
    assert_eq!(harness.provider().renews_at_tools, [0]);
    assert_eq!(harness.provider().renews_at_calls, [0, 1]);
    assert_eq!(outcome.renews, 1);
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
    let error = run(&mut harness)
        .await
        .expect_err("the final check finds the sandbox gone");
    // The refusal was only a tool error: the model was asked again. But a
    // session cannot complete on a sandbox that is gone, so the final check
    // ends it with the deletion reason kilnd reports.
    assert_eq!(harness.provider().calls, 2);
    assert_eq!(expired(&error), Some(ExpiryReason::ExpiredIdle), "{error}");
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
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
    // R2-02: the tool Exec is refused before admission after 18 s of a 30 s
    // idle window. No activity happened, so the next model call must renew
    // at once instead of trusting an invented finish at 18 s, which would
    // put the next renew at 28 s.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error(
        Code::Unavailable,
        "request rejected before execution admission",
    ));
    api.state().tool_exec_delay = Duration::from_secs(18);
    let provider = ObserveRenews::new(
        Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        &api,
    );
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    let mut harness = session(&api, provider, config);
    run(&mut harness).await.expect("session completes");
    assert_eq!(harness.provider().renews_at_calls, [0, 1]);
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
        // A not-ready refusal is terminal when the sandbox does not read
        // READY; the other three are terminal even when it does.
        api.state().not_ready = token == "sandbox_not_ready";
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

/// Wraps a provider and records, at each completed tool call, how many
/// renews the fake kilnd had answered.
struct ObserveRenews {
    inner: Script,
    api: FakeApi,
    renews_at_tools: Vec<u32>,
    renews_at_calls: Vec<u32>,
}

impl ObserveRenews {
    fn new(inner: Script, api: &FakeApi) -> Self {
        Self {
            inner,
            api: api.clone(),
            renews_at_tools: Vec::new(),
            renews_at_calls: Vec::new(),
        }
    }
}

#[async_trait]
impl Provider for ObserveRenews {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.renews_at_calls.push(self.api.state().renews);
        self.inner.complete(request).await
    }

    fn tool_completed(
        &mut self,
        _turn: u32,
        _index: u32,
        _call: &ToolCall,
        _ok: bool,
        _result: &Value,
    ) -> Result<(), ProviderError> {
        self.renews_at_tools.push(self.api.state().renews);
        Ok(())
    }
}

fn exec_call_id(id: &str) -> ToolCall {
    ToolCall::new(id, "exec", json!({"argv": ["/usr/bin/wc", "/workspace/x"]}))
}

#[tokio::test(start_paused = true)]
async fn a_renew_falls_due_between_tool_calls_in_one_turn() {
    // R3-02: one response with two Execs, each refused before admission
    // after 16 s; together they cross the 30 s idle window. The renew must
    // run before the second.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error(
        Code::Unavailable,
        "request rejected before execution admission",
    ));
    api.state().tool_exec_delay = Duration::from_secs(16);
    let watcher = api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if watcher.state().renews == 0 {
            watcher.expire("expired_idle");
        }
    });
    let provider = Script::new(
        Duration::ZERO,
        [
            ModelResponse::tool_use([exec_call_id("e1"), exec_call_id("e2")]),
            ModelResponse::final_text("done"),
        ],
    );
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    config.max_tool_calls_per_turn = 2;
    let mut harness = session(&api, provider, config);
    let outcome = run(&mut harness).await.expect("session completes");
    assert!(outcome.renews >= 1);
    assert_eq!(api.state().expired, None);
}

#[tokio::test(start_paused = true)]
async fn host_only_rejections_send_no_renew() {
    // Idle 30 s renews every 10 s. The first call ends exactly when a renew
    // falls due, so the next kilnd-bound call renews first; an unknown tool
    // and invalid arguments before it reach no kilnd RPC and renew nothing.
    let api = FakeApi::with_expiry();
    let provider = ObserveRenews::new(
        Script::new(
            Duration::from_secs(10),
            [
                ModelResponse::tool_use([
                    ToolCall::new("u1", "not_a_tool", json!({})),
                    ToolCall::new("b1", "exec", json!({"argv": []})),
                    exec_call_id("e1"),
                ]),
                ModelResponse::final_text("done"),
            ],
        ),
        &api,
    );
    let mut config = long_config(30);
    config.max_tool_calls_per_turn = 3;
    let mut harness = session(&api, provider, config);
    run(&mut harness).await.expect("session completes");
    assert_eq!(harness.provider().renews_at_tools, [0, 0, 1]);
}

/// A fake kilnd that wakes a waiter whenever a renew is answered.
#[derive(Clone)]
struct NotifyingApi {
    inner: FakeApi,
    renew_answered: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait]
impl kiln_harness::api::SandboxApi for NotifyingApi {
    async fn health(&self) -> Result<kiln_harness::api::Health, kiln_harness::api::ApiError> {
        self.inner.health().await
    }

    async fn create(
        &self,
        request: kiln_harness::api::SandboxRequest,
    ) -> Result<kiln_harness::api::Sandbox, kiln_harness::api::ApiError> {
        self.inner.create(request).await
    }

    async fn exec(
        &self,
        request: kiln_harness::api::SandboxExec,
    ) -> Result<kiln_harness::api::Execution, kiln_harness::api::ApiError> {
        self.inner.exec(request).await
    }

    async fn recent_executions(
        &self,
        sandbox_id: &str,
    ) -> Result<Vec<kiln_harness::api::Execution>, kiln_harness::api::ApiError> {
        self.inner.recent_executions(sandbox_id).await
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, kiln_harness::api::ApiError> {
        self.inner.delete(sandbox_id, release_request_id).await
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), kiln_harness::api::ApiError> {
        let result = self.inner.renew(sandbox_id).await;
        self.renew_answered.notify_one();
        result
    }

    async fn get_execution(
        &self,
        execution_id: &str,
    ) -> Result<kiln_harness::api::Execution, kiln_harness::api::ApiError> {
        self.inner.get_execution(execution_id).await
    }

    async fn sandbox_ready(&self, sandbox_id: &str) -> Result<bool, kiln_harness::api::ApiError> {
        self.inner.sandbox_ready(sandbox_id).await
    }

    async fn final_check(
        &self,
        sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, kiln_harness::api::ApiError> {
        self.inner.final_check(sandbox_id).await
    }

    async fn non_deleted_sandboxes(
        &self,
    ) -> Result<Vec<kiln_harness::api::Sandbox>, kiln_harness::api::ApiError> {
        self.inner.non_deleted_sandboxes().await
    }
}

/// Answers its first request with an Exec as soon as a renew is answered,
/// in the same clock tick, and its second after `delay`.
struct ToolAfterFirstRenew {
    renew_answered: std::sync::Arc<tokio::sync::Notify>,
    delay: Duration,
    calls: u32,
}

#[async_trait]
impl Provider for ToolAfterFirstRenew {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        }
    }

    async fn complete(
        &mut self,
        _request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.calls += 1;
        if self.calls == 1 {
            self.renew_answered.notified().await;
            return Ok(exec_call());
        }
        tokio::time::sleep(self.delay).await;
        Ok(ModelResponse::final_text("done"))
    }
}

#[tokio::test(start_paused = true)]
async fn same_tick_activity_resets_the_renew_failures() {
    // R3-03: the first renew fails at 10 s, and an Exec completes in the
    // same clock tick. That Exec proves the sandbox alive after the
    // failure, so a second failure at 20 s is the first in a row, and the
    // retry at 30 s succeeds.
    let inner = FakeApi::with_expiry();
    inner.state().renew_script = VecDeque::from([
        RenewScript::Fail(api_error(Code::Unavailable, "first transient failure")),
        RenewScript::Fail(api_error(Code::Unavailable, "second transient failure")),
        RenewScript::Ok,
    ]);
    let renew_answered = std::sync::Arc::new(tokio::sync::Notify::new());
    let api = NotifyingApi {
        inner: inner.clone(),
        renew_answered: renew_answered.clone(),
    };
    let provider = ToolAfterFirstRenew {
        renew_answered,
        delay: Duration::from_secs(35),
        calls: 0,
    };
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        long_config(30),
    );
    let outcome = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("the stale failure no longer counts");
    // Renews at 10 s (failed), 20 s (failed), 30 s and 40 s (both answered).
    assert_eq!(inner.state().renews, 4);
    assert!(inner.state().renew_script.is_empty());
    assert_eq!(outcome.renews, 2);
}

#[tokio::test]
async fn a_recovered_row_is_evidence_only_once_it_settles() {
    use kiln_harness::api::CancelReason;
    // R3-01 through the fake: the tool's Exec is published RUNNING and its
    // response lost. Recovery follows the row until it is terminal.
    let api = FakeApi::with_expiry();
    api.state().running_exec = Some((2, 3));
    api.state().unusable_exec = Some((2, Some(CancelReason::SandboxDeleted)));
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    );
    let mut config = short_lifecycle_config();
    config.lifecycle_timeout = Duration::from_secs(1);
    let mut harness = session(&api, provider, config);
    let error = run(&mut harness).await.expect_err("sandbox gone");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::SandboxDeleted),
        "{error}"
    );
    assert!(api.state().get_execution_calls >= 3);
    assert_eq!(harness.provider().calls, 1);

    // A row that never settles fails closed, and no later Exec runs.
    let api = FakeApi::with_expiry();
    api.state().running_exec = Some((2, u32::MAX));
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    );
    let mut harness = session(&api, provider, short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("unsettled");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition
                && error.message
                    == "execution ex-0000000000000002 did not settle after its response was lost"),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 1);
    assert_eq!(api.state().execs.len(), 2);
    assert!(journal_events(&harness).iter().any(|event| matches!(
        event,
        JournalEvent::ToolExecuted { execution_ids, ok: false, .. }
            if execution_ids == &["ex-0000000000000002".to_string()]
    )));
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));

    // During the baseline.
    let api = FakeApi::with_expiry();
    api.state().running_exec = Some((1, u32::MAX));
    let mut harness = session(
        &api,
        Script::final_after(Duration::ZERO),
        short_lifecycle_config(),
    );
    let error = run(&mut harness).await.expect_err("unsettled baseline");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.message
                == "execution ex-0000000000000001 did not settle after its response was lost"),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 0);

    // A row that settles without the usability field, as from an older
    // daemon, keeps the old behavior: the loop continues.
    let api = FakeApi::default();
    api.state().running_exec = Some((2, 1));
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

#[tokio::test]
async fn a_not_ready_race_on_a_ready_sandbox_is_recoverable() {
    // kilnd answers sandbox_not_ready, "retry", when an Exec lost a race
    // with its own row insert on a READY sandbox. One lookup tells them
    // apart: READY returns an ordinary tool error and the loop continues.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error_with_reason(
        Code::FailedPrecondition,
        "sandbox_not_ready",
        "sandbox_not_ready: execution insert lost a race; retry",
    ));
    let provider = Script::new(
        Duration::ZERO,
        [exec_call(), ModelResponse::final_text("done")],
    );
    let mut harness = session(&api, provider, short_lifecycle_config());
    let outcome = run(&mut harness).await.expect("the loop continues");
    assert_eq!(outcome.steps, 2);
    let result = journal_events(&harness)
        .into_iter()
        .find_map(|event| match event {
            JournalEvent::ToolExecuted { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("tool_executed");
    assert_eq!(result["error"], "kilnd_error");
}

#[tokio::test]
async fn a_tool_budget_that_does_not_fit_the_idle_window_is_refused() {
    // R4-01 (a): r4-repro-01's 9 s tool timeout with a 10 s idle timeout.
    // One refused Exec could outlive the sandbox, so nothing is created.
    let api = FakeApi::with_expiry();
    let mut config = long_config(10);
    config.tool_limits.timeout = Duration::from_secs(9);
    let mut harness = session(&api, Script::final_after(Duration::ZERO), config);
    let error = run(&mut harness).await.expect_err("refused");
    assert!(
        matches!(&error, HarnessError::InvalidTask(message)
            if message.contains("9100 ms") && message.contains("10000 ms")),
        "{error}"
    );
    assert!(api.state().creates.is_empty());
    assert!(harness.audit().records.is_empty());

    // The idle timeout checked is the one sent: kilnd's cap lowers it.
    let api = FakeApi::with_expiry();
    api.state().limits = Some(ApiLimits {
        max_sandbox_idle_ms: 10_000,
        ..kilnd_limits()
    });
    let mut config = long_config(120);
    config.tool_limits.timeout = Duration::from_secs(9);
    let mut harness = session(&api, Script::final_after(Duration::ZERO), config);
    run(&mut harness)
        .await
        .expect_err("refused after the clamp");
    assert!(api.state().creates.is_empty());

    // Without expiry the relation does not apply.
    let api = FakeApi::default();
    let mut config = long_config(10);
    config.tool_limits.timeout = Duration::from_secs(9);
    let mut harness = session(&api, Script::final_after(Duration::ZERO), config);
    run(&mut harness)
        .await
        .expect("an old daemon runs as before");

    // The defaults fit.
    let api = FakeApi::with_expiry();
    let mut config = HarnessConfig::default();
    config.tool_limits.timeout = Duration::from_secs(30);
    let mut harness = session(&api, ScriptedProvider::demo(), config);
    run(&mut harness)
        .await
        .expect("30 s tools fit a 120 s idle timeout");
}

#[tokio::test(start_paused = true)]
async fn a_slow_refused_exec_within_the_budget_does_not_outlive_the_sandbox() {
    // R4-01 with a valid configuration: idle 30 s (renew every 10 s), tool
    // timeout 19 s. The Exec starts at 9.9 s, just before the tick, and is
    // refused before admission just under its timeout. The renew before the
    // next model call still comes before the idle deadline.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error(
        Code::Unavailable,
        "request rejected before execution admission",
    ));
    api.state().tool_exec_delay = Duration::from_millis(18_900);
    let watcher = api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if watcher.state().renews == 0 {
            watcher.expire("expired_idle");
        }
    });
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    let mut harness = session(
        &api,
        Script::new(
            Duration::from_millis(9_900),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        config,
    );
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().expired, None);
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_could_outlive_the_idle_window_renews_first_despite_backoff() {
    // R4-01 (b): a renew fails at 10 s, so the retry is not due until 20 s.
    // The model asks for an Exec at 15 s; with a 19.1 s budget it could run
    // past the 30 s idle deadline, so a renew is forced before it.
    let api = FakeApi::with_expiry();
    api.state().renew_script =
        VecDeque::from([RenewScript::Fail(api_error(Code::Unavailable, "transient"))]);
    let provider = ObserveRenews::new(
        Script::new(
            Duration::from_secs(15),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        &api,
    );
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    let mut harness = session(&api, provider, config);
    run(&mut harness).await.expect("session completes");
    // The failed renew at 10 s, then the forced one at 15 s, before the tool.
    assert_eq!(harness.provider().renews_at_tools, [2]);
}

#[tokio::test(start_paused = true)]
async fn the_session_deadline_wins_over_a_failing_pre_tool_renew() {
    // A renew fails at 10 s; the forced renew before the tool at 15 s hangs
    // and is cut off by the 25 s session deadline, which is also the second
    // failure in a row. The deadline is the outcome, not renew_failed.
    let api = FakeApi::with_expiry();
    api.state().renew_script =
        VecDeque::from([RenewScript::Fail(api_error(Code::Unavailable, "transient"))]);
    api.state().renew_delay = Duration::from_secs(3600);
    let mut config = long_config(30);
    config.session_timeout = Duration::from_secs(25);
    config.tool_limits.timeout = Duration::from_secs(19);
    let mut harness = session(
        &api,
        Script::new(
            Duration::from_secs(15),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        config,
    );
    let error = run(&mut harness).await.expect_err("deadline");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    assert_eq!(api.state().renews, 2);
}

#[tokio::test(start_paused = true)]
async fn a_fatal_pre_tool_renew_leaves_the_call_audited_but_not_completed() {
    // The first call ends exactly when a renew falls due, so the renew runs
    // before the tool, and kilnd refuses it for good.
    let api = FakeApi::with_expiry();
    api.state().renew_script = VecDeque::from([RenewScript::Fail(api_error(
        Code::NotFound,
        "no such sandbox",
    ))]);
    let mut harness = session(
        &api,
        Script::new(
            Duration::from_secs(10),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        long_config(30),
    );
    let error = run(&mut harness).await.expect_err("fatal renew");
    assert!(
        matches!(&error, HarnessError::Api(error) if error.code == Code::NotFound),
        "{error}"
    );
    let events = audit_events(&harness);
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ModelTurn { action, audit_call_id: Some(id), .. }
            if action == "tool_call" && id == "call-1-1"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AuditEvent::ToolCompleted { .. }))
    );
    assert_eq!(api.state().execs.len(), 1, "only the baseline ran");
}

/// Record the demo against `api` under `config` into a journal file and
/// return its replay provider.
async fn record_demo(
    api: &FakeApi,
    config: HarnessConfig,
    dir: &std::path::Path,
) -> ReplayProvider {
    let path = dir.join("recorded.jsonl");
    let mut recording = Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        config,
    );
    recording
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("recording");
    drop(recording);
    ReplayProvider::from_journal(&read_journal(&path).expect("journal verifies"))
        .expect("replayable")
}

async fn replay_against(
    api: &FakeApi,
    provider: ReplayProvider,
) -> Result<SessionOutcome, HarnessError> {
    let task = provider.task();
    let config = provider.replay_config(HarnessConfig::default());
    let mut replay = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    replay.run(&task, CancellationToken::new()).await
}

#[tokio::test]
async fn replay_uses_the_recorded_idle_timeout() {
    let temp = common::private_tempdir().expect("tempdir");
    let config = HarnessConfig {
        sandbox_idle_timeout: Duration::from_secs(60),
        ..HarnessConfig::default()
    };
    let provider = record_demo(&FakeApi::with_expiry(), config, temp.path()).await;
    assert_eq!(
        provider.recorded_sandbox_idle_timeout(),
        Some(Duration::from_secs(60))
    );
    let api = FakeApi::with_expiry();
    replay_against(&api, provider).await.expect("replay");
    assert_eq!(
        api.state().creates[0]
            .expiry
            .map(|policy| policy.idle_timeout_ms),
        Some(60_000)
    );
}

#[tokio::test]
async fn replay_without_recorded_expiry_fits_the_idle_timeout_to_the_tool_budget() {
    // Recorded against an old daemon with 90 s tools: 90 s plus 1 s of
    // recovery is a 91 s budget, and 1.5 times that is 137 s, whole.
    let temp = common::private_tempdir().expect("tempdir");
    let mut config = HarnessConfig::default();
    config.tool_limits.timeout = Duration::from_secs(90);
    let provider = record_demo(&FakeApi::default(), config, temp.path()).await;
    assert_eq!(provider.recorded_sandbox_idle_timeout(), None);
    assert_eq!(
        provider
            .replay_config(HarnessConfig::default())
            .sandbox_idle_timeout,
        Duration::from_secs(137)
    );
    let api = FakeApi::with_expiry();
    replay_against(&api, provider).await.expect("replay");
    assert_eq!(
        api.state().creates[0]
            .expiry
            .map(|policy| policy.idle_timeout_ms),
        Some(137_000)
    );

    // Small tool budgets keep the 120 s default.
    let temp = common::private_tempdir().expect("tempdir");
    let provider = record_demo(&FakeApi::default(), HarnessConfig::default(), temp.path()).await;
    assert_eq!(
        provider
            .replay_config(HarnessConfig::default())
            .sandbox_idle_timeout,
        Duration::from_secs(120)
    );
}

#[tokio::test]
async fn a_replay_whose_tool_budget_the_daemon_cannot_fit_is_refused() {
    // The largest recordable tool timeout, 10 min, needs a 902 s idle
    // timeout: within the 1 h ceiling, so the ceiling itself cannot refuse
    // a valid recording. A daemon that caps idle below it does, before
    // anything is created.
    let temp = common::private_tempdir().expect("tempdir");
    let mut config = HarnessConfig::default();
    config.tool_limits.timeout = Duration::from_secs(600);
    let provider = record_demo(&FakeApi::default(), config, temp.path()).await;
    assert_eq!(
        provider
            .replay_config(HarnessConfig::default())
            .sandbox_idle_timeout,
        Duration::from_secs(902)
    );
    let api = FakeApi::with_expiry();
    api.state().limits = Some(ApiLimits {
        max_sandbox_idle_ms: 600_000,
        ..kilnd_limits()
    });
    let error = replay_against(&api, provider).await.expect_err("refused");
    assert!(matches!(error, HarnessError::InvalidTask(_)), "{error}");
    assert!(api.state().creates.is_empty());
}

#[test]
fn the_fitted_replay_idle_timeout_is_capped_at_the_ceiling() {
    // Beyond what a valid recording can hold, the ceiling applies.
    let mut config = HarnessConfig::default();
    config.tool_limits.timeout = Duration::from_secs(3_000);
    config.fit_replay_idle_timeout(None);
    assert_eq!(config.sandbox_idle_timeout, Duration::from_secs(3_600));
    config.fit_replay_idle_timeout(Some(Duration::from_secs(45)));
    assert_eq!(config.sandbox_idle_timeout, Duration::from_secs(45));
}

#[tokio::test(start_paused = true)]
async fn a_late_renew_reply_does_not_move_the_activity_anchor_forward() {
    // R5-01: every renew reply arrives 9 s after kilnd committed it. The
    // anchor is when the renew was sent (10 s), so a tool at 24 s that could
    // run past 40 s forces a renew first.
    let api = FakeApi::with_expiry();
    api.state().renew_delay = Duration::from_secs(9);
    api.state().tool_exec_error = Some(api_error(
        Code::Unavailable,
        "request rejected before execution admission",
    ));
    api.state().tool_exec_delay = Duration::from_secs(18);
    let watcher = api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(40)).await;
        if watcher.state().renews == 1 {
            watcher.expire("expired_idle");
        }
    });
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    let provider = ObserveRenews::new(
        Script::new(
            Duration::from_secs(24),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        &api,
    );
    let mut harness = session(&api, provider, config);
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().expired, None);
    // The heartbeat at 10 s, another at 20 s, and the forced one at 24 s.
    assert_eq!(harness.provider().renews_at_tools, [3]);
}

/// Answers `head` only after `delay`, refusing it before admission; every
/// other call goes to the fake.
#[derive(Clone)]
struct SlowRefusedHead {
    inner: FakeApi,
    delay: Duration,
}

#[async_trait]
impl kiln_harness::api::SandboxApi for SlowRefusedHead {
    async fn health(&self) -> Result<kiln_harness::api::Health, kiln_harness::api::ApiError> {
        self.inner.health().await
    }

    async fn create(
        &self,
        request: kiln_harness::api::SandboxRequest,
    ) -> Result<kiln_harness::api::Sandbox, kiln_harness::api::ApiError> {
        self.inner.create(request).await
    }

    async fn exec(
        &self,
        request: kiln_harness::api::SandboxExec,
    ) -> Result<kiln_harness::api::Execution, kiln_harness::api::ApiError> {
        if request.argv.first().map(String::as_str) == Some("/usr/bin/head") {
            tokio::time::sleep(self.delay).await;
            return Err(api_error(
                Code::Unavailable,
                "head request rejected before execution admission",
            ));
        }
        self.inner.exec(request).await
    }

    async fn recent_executions(
        &self,
        sandbox_id: &str,
    ) -> Result<Vec<kiln_harness::api::Execution>, kiln_harness::api::ApiError> {
        self.inner.recent_executions(sandbox_id).await
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, kiln_harness::api::ApiError> {
        self.inner.delete(sandbox_id, release_request_id).await
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), kiln_harness::api::ApiError> {
        self.inner.renew(sandbox_id).await
    }

    async fn get_execution(
        &self,
        execution_id: &str,
    ) -> Result<kiln_harness::api::Execution, kiln_harness::api::ApiError> {
        self.inner.get_execution(execution_id).await
    }

    async fn sandbox_ready(&self, sandbox_id: &str) -> Result<bool, kiln_harness::api::ApiError> {
        self.inner.sandbox_ready(sandbox_id).await
    }

    async fn final_check(
        &self,
        sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, kiln_harness::api::ApiError> {
        self.inner.final_check(sandbox_id).await
    }

    async fn non_deleted_sandboxes(
        &self,
    ) -> Result<Vec<kiln_harness::api::Sandbox>, kiln_harness::api::ApiError> {
        self.inner.non_deleted_sandboxes().await
    }
}

#[tokio::test(start_paused = true)]
async fn a_multi_exec_tool_ends_within_one_tool_timeout() {
    // R5-01: read_file's stat answers at 18 s of a 19 s tool timeout; its
    // head would be refused only at 31 s, past the 30 s idle deadline. The
    // tool timeout bounds the whole tool, so head gets the 1 s that remains
    // and the tool ends at its deadline instead.
    let inner = FakeApi::with_expiry();
    inner
        .state()
        .files
        .insert("/workspace/a.txt".to_string(), b"hello".to_vec());
    inner.state().tool_exec_delay = Duration::from_secs(18);
    let api = SlowRefusedHead {
        inner: inner.clone(),
        delay: Duration::from_secs(13),
    };
    let watcher = inner.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if watcher.state().renews == 0 {
            watcher.expire("expired_idle");
        }
    });
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    let read = ModelResponse::tool_use([ToolCall::new(
        "r1",
        "read_file",
        json!({"path": "a.txt", "max_bytes": 64}),
    )]);
    let mut harness = Harness::new(
        api,
        Script::new(Duration::ZERO, [read, ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = tokio::time::Instant::now();
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("the tool reaches its deadline");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    assert!(started.elapsed() < Duration::from_secs(21));
    assert_eq!(inner.state().expired, None);
}

#[tokio::test(start_paused = true)]
async fn a_forced_renew_must_succeed_before_the_tool_runs() {
    // R5-02: idle 30 s, a 19.1 s tool budget. Renews take 2 s to answer.
    // The heartbeat renew sent at 10 s is still in flight when the model
    // answers at 11 s, so it proves nothing. The tool could then reach the
    // idle deadline, so the renew before it is forced; its first attempt
    // fails, and the tool waits for the retry to succeed.
    let api = FakeApi::with_expiry();
    api.state().renew_delay = Duration::from_secs(2);
    api.state().renew_script = VecDeque::from([
        RenewScript::Ok,
        RenewScript::Fail(api_error(Code::Unavailable, "transient")),
    ]);
    let provider = ObserveRenews::new(
        Script::new(
            Duration::from_secs(11),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        &api,
    );
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    let mut harness = session(&api, provider, config.clone());
    run(&mut harness).await.expect("session completes");
    assert_eq!(harness.provider().renews_at_tools, [3]);

    // When the retry fails too, the tool never runs.
    let api = FakeApi::with_expiry();
    api.state().renew_delay = Duration::from_secs(2);
    api.state().renew_script = VecDeque::from([
        RenewScript::Ok,
        RenewScript::Fail(api_error(Code::Unavailable, "transient")),
        RenewScript::Fail(api_error(Code::Unavailable, "transient")),
    ]);
    let mut harness = session(
        &api,
        Script::new(
            Duration::from_secs(11),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        config,
    );
    let error = run(&mut harness).await.expect_err("renew failed");
    assert_eq!(expired(&error), Some(ExpiryReason::RenewFailed), "{error}");
    assert_eq!(api.state().execs.len(), 1, "only the baseline ran");
}

#[tokio::test(start_paused = true)]
async fn a_long_command_then_another_tool_forces_a_renew_between_them() {
    // The first Exec succeeds after 18 s. Its activity is anchored when it
    // was sent, so the second, which could run another 19.1 s, renews first.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_delay = Duration::from_secs(18);
    let provider = ObserveRenews::new(
        Script::new(
            Duration::ZERO,
            [
                ModelResponse::tool_use([exec_call_id("e1"), exec_call_id("e2")]),
                ModelResponse::final_text("done"),
            ],
        ),
        &api,
    );
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    config.max_tool_calls_per_turn = 2;
    let mut harness = session(&api, provider, config);
    let outcome = run(&mut harness).await.expect("session completes");
    assert_eq!(harness.provider().renews_at_tools, [0, 1]);
    assert!(outcome.renews >= 1);
}

fn missed_row(
    state: kiln_harness::api::ExecutionState,
    reusable: Option<bool>,
) -> kiln_harness::api::Execution {
    kiln_harness::api::Execution {
        id: "ex-00000000000000ff".to_string(),
        state,
        exit_code: None,
        failure_reason: String::new(),
        stdout: common::captured(Vec::new()),
        stderr: common::captured(Vec::new()),
        sandbox_reusable: reusable,
        cancel_reason: Some(kiln_harness::api::CancelReason::SandboxDeleted),
    }
}

fn final_check_event<P: Provider>(harness: &TestHarness<P>) -> Option<(String, usize, bool)> {
    audit_events(harness)
        .into_iter()
        .find_map(|event| match event {
            AuditEvent::FinalCheck {
                state,
                unknown_executions,
                usable,
            } => Some((state.clone(), *unknown_executions, *usable)),
            _ => None,
        })
}

#[tokio::test]
async fn every_completed_session_passes_a_final_check() {
    let api = FakeApi::with_expiry();
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    run(&mut harness).await.expect("demo completes");
    assert_eq!(api.state().final_checks, 1);
    assert_eq!(
        final_check_event(&harness),
        Some(("ready".to_string(), 0, true))
    );
}

#[tokio::test]
async fn the_final_check_refuses_completion_on_an_unusable_sandbox() {
    use kiln_harness::api::ExecutionState;
    let not_usable = "sandbox was not usable when the session finished";
    // A row the session never saw left the sandbox unusable: its cause.
    let api = FakeApi::with_expiry();
    api.state().final_rows = vec![missed_row(ExecutionState::Canceled, Some(false))];
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("not usable");
    assert_eq!(
        expired(&error),
        Some(ExpiryReason::SandboxDeleted),
        "{error}"
    );
    assert_eq!(
        final_check_event(&harness),
        Some(("ready".to_string(), 1, false))
    );
    assert_eq!(finished(&harness), ("failed".to_string(), true, 0));
    // The final answer stays journaled as the model's response.
    assert!(journal_events(&harness).iter().any(|event| matches!(
        event,
        JournalEvent::ModelResponded { response, .. } if response.tool_calls.is_empty()
    )));

    // A row the session never saw is still running.
    let api = FakeApi::with_expiry();
    api.state().final_rows = vec![missed_row(ExecutionState::Unknown, None)];
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("not usable");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition && error.message == not_usable),
        "{error}"
    );

    // The sandbox is not ready, with no deletion reason.
    let api = FakeApi::with_expiry();
    let provider = Script::new(Duration::ZERO, [ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    api.state().not_ready = true;
    let error = run(&mut harness).await.expect_err("not usable");
    assert!(
        matches!(&error, HarnessError::Api(error) if error.message == not_usable),
        "{error}"
    );

    // The check cannot be made.
    let api = FakeApi::with_expiry();
    api.state().final_check_error = Some(api_error(Code::Unavailable, "socket closed"));
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    let error = run(&mut harness).await.expect_err("unconfirmed");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::Unavailable
                && error.message == "could not confirm the sandbox at the end of the session"),
        "{error}"
    );
    assert_eq!(
        final_check_event(&harness),
        Some(("unknown".to_string(), 0, false))
    );
}

#[tokio::test]
async fn a_failed_final_check_journal_verifies() {
    use kiln_harness::api::ExecutionState;
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("failed.jsonl");
    let api = FakeApi::with_expiry();
    api.state().final_rows = vec![missed_row(ExecutionState::Canceled, Some(false))];
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("not usable");
    drop(harness);
    let journal = read_journal(&path).expect("journal verifies");
    assert!(journal.complete);
}

#[tokio::test]
async fn an_older_daemon_is_checked_by_state_only() {
    use kiln_harness::api::ExecutionState;
    // Without expiry, rows do not report usability; only state counts.
    let api = FakeApi::default();
    api.state().final_rows = vec![missed_row(ExecutionState::Unknown, None)];
    let mut harness = session(&api, ScriptedProvider::demo(), short_lifecycle_config());
    run(&mut harness).await.expect("completes on state");

    let api = FakeApi::default();
    let provider = Script::new(Duration::ZERO, [ModelResponse::final_text("done")]);
    let mut harness = session(&api, provider, short_lifecycle_config());
    api.state().not_ready = true;
    run(&mut harness).await.expect_err("not ready");
}

/// The first renew answers after 9 s; every later one never answers.
#[derive(Clone)]
struct DelayedThenHangingRenew {
    inner: FakeApi,
    calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

#[async_trait]
impl kiln_harness::api::SandboxApi for DelayedThenHangingRenew {
    async fn health(&self) -> Result<kiln_harness::api::Health, kiln_harness::api::ApiError> {
        self.inner.health().await
    }

    async fn create(
        &self,
        request: kiln_harness::api::SandboxRequest,
    ) -> Result<kiln_harness::api::Sandbox, kiln_harness::api::ApiError> {
        self.inner.create(request).await
    }

    async fn exec(
        &self,
        request: kiln_harness::api::SandboxExec,
    ) -> Result<kiln_harness::api::Execution, kiln_harness::api::ApiError> {
        self.inner.exec(request).await
    }

    async fn recent_executions(
        &self,
        sandbox_id: &str,
    ) -> Result<Vec<kiln_harness::api::Execution>, kiln_harness::api::ApiError> {
        self.inner.recent_executions(sandbox_id).await
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, kiln_harness::api::ApiError> {
        self.inner.delete(sandbox_id, release_request_id).await
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), kiln_harness::api::ApiError> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if call == 1 {
            tokio::time::sleep(Duration::from_secs(9)).await;
            return self.inner.renew(sandbox_id).await;
        }
        std::future::pending().await
    }

    async fn get_execution(
        &self,
        execution_id: &str,
    ) -> Result<kiln_harness::api::Execution, kiln_harness::api::ApiError> {
        self.inner.get_execution(execution_id).await
    }

    async fn sandbox_ready(&self, sandbox_id: &str) -> Result<bool, kiln_harness::api::ApiError> {
        self.inner.sandbox_ready(sandbox_id).await
    }

    async fn final_check(
        &self,
        sandbox_id: &str,
    ) -> Result<kiln_harness::api::FinalCheck, kiln_harness::api::ApiError> {
        self.inner.final_check(sandbox_id).await
    }

    async fn non_deleted_sandboxes(
        &self,
    ) -> Result<Vec<kiln_harness::api::Sandbox>, kiln_harness::api::ApiError> {
        self.inner.non_deleted_sandboxes().await
    }
}

#[tokio::test(start_paused = true)]
async fn a_forced_renew_never_runs_past_its_give_up() {
    // R6-03: activity anchored at 10 s gives up at 40 s. The forced attempt
    // at 20 s times out at 30 s and the retry at 35 s is cut off at 40 s,
    // not at its own 45 s.
    let inner = FakeApi::with_expiry();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let api = DelayedThenHangingRenew {
        inner: inner.clone(),
        calls: calls.clone(),
    };
    let mut config = long_config(30);
    config.tool_limits.timeout = Duration::from_secs(19);
    // A 1 s recovery window makes the tool budget 20 s.
    config.lifecycle_timeout = Duration::from_secs(1);
    config.lifecycle_retry_delay = Duration::from_secs(5);
    let mut harness = Harness::new(
        api,
        Script::new(
            Duration::from_secs(20),
            [exec_call(), ModelResponse::final_text("done")],
        ),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = tokio::time::Instant::now();
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("renew failed");
    assert_eq!(expired(&error), Some(ExpiryReason::RenewFailed), "{error}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(40) && elapsed < Duration::from_secs(41),
        "{elapsed:?}"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(inner.state().execs.len(), 1, "only the baseline ran");
}

#[tokio::test(start_paused = true)]
async fn recovery_never_runs_past_the_session_deadline() {
    // R6-02: an Exec refused without a row at 900 ms of a 1 s session.
    // Recovery would have a 1 s window, but the session deadline bounds it.
    let api = FakeApi::default();
    api.state().tool_exec_error = Some(api_error(Code::Unavailable, "refused"));
    api.state().tool_exec_delay = Duration::from_millis(900);
    let config = HarnessConfig {
        session_timeout: Duration::from_secs(1),
        lifecycle_timeout: Duration::from_secs(1),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    };
    // The second model call takes 50 ms, so the session's 1 s deadline is
    // what ends it, observed when it passes.
    let provider = SlowAfterFirstCall {
        inner: Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        delay: Duration::from_millis(50),
    };
    let mut harness = session(&api, provider, config);
    let started = tokio::time::Instant::now();
    let error = run(&mut harness).await.expect_err("deadline");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    let elapsed = started.elapsed();
    assert!(elapsed <= Duration::from_millis(1_005), "{elapsed:?}");
}

#[tokio::test(start_paused = true)]
async fn the_tool_timeout_bounds_execs_and_recovery_adds_its_window() {
    // R6-02, as documented: the tool timeout bounds the tool's Execs, and
    // response-loss recovery may add its window (1 s here) after them.
    let api = FakeApi::default();
    api.state().tool_exec_error = Some(api_error(Code::Unavailable, "refused"));
    api.state().tool_exec_delay = Duration::from_millis(900);
    let mut config = HarnessConfig {
        session_timeout: Duration::from_secs(60),
        lifecycle_timeout: Duration::from_secs(1),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    };
    config.tool_limits.timeout = Duration::from_secs(1);
    let mut harness = session(
        &api,
        Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        config,
    );
    let started = tokio::time::Instant::now();
    run(&mut harness).await.expect("completes");
    let elapsed = started.elapsed();
    assert!(elapsed <= Duration::from_secs(2), "{elapsed:?}");
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_recovery_returns_at_once() {
    let api = FakeApi::default();
    api.state().tool_exec_error = Some(api_error(Code::Unavailable, "refused"));
    let config = HarnessConfig {
        lifecycle_timeout: Duration::from_secs(1),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    };
    let mut harness = session(
        &api,
        Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        config,
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = tokio::time::Instant::now();
    let error = harness
        .run(&SessionTask::demo(), cancel)
        .await
        .expect_err("canceled");
    assert!(matches!(error, HarnessError::Canceled), "{error}");
    assert!(started.elapsed() < Duration::from_millis(400));
}

/// Answers the first request at once and later ones after `delay`.
struct SlowAfterFirstCall {
    inner: Script,
    delay: Duration,
}

#[async_trait]
impl Provider for SlowAfterFirstCall {
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

#[tokio::test(start_paused = true)]
async fn an_unreadable_recovery_with_no_rows_stops_the_session() {
    // R7-01: the tool's Exec fails and every recovery read fails too. With
    // no row and nothing read, the call ends the session; the model is not
    // asked again.
    let api = FakeApi::with_expiry();
    api.state().tool_exec_error = Some(api_error(Code::Unavailable, "refused"));
    api.state().recent_unavailable = true;
    let mut harness = session(
        &api,
        Script::new(
            Duration::ZERO,
            [exec_call(), ModelResponse::final_text("done")],
        ),
        short_lifecycle_config(),
    );
    let error = run(&mut harness).await.expect_err("unreadable");
    assert!(
        matches!(&error, HarnessError::Api(error)
            if error.code == Code::FailedPrecondition
                && error.message
                    == "execution evidence could not be read after its response was lost"),
        "{error}"
    );
    assert_eq!(harness.provider().calls, 1);
    assert_eq!(api.state().final_checks, 0);
}

/// Cancels the session in the same poll that returns the final answer.
struct CancelWithFinalAnswer {
    cancel: CancellationToken,
}

#[async_trait]
impl Provider for CancelWithFinalAnswer {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        }
    }

    async fn complete(
        &mut self,
        _request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.cancel.cancel();
        Ok(ModelResponse::final_text("done"))
    }
}

#[tokio::test]
async fn a_cancellation_set_before_the_final_check_always_wins() {
    // R7-02: the check never races a cancellation that is already set.
    for attempt in 0..128 {
        let api = FakeApi::with_expiry();
        let cancel = CancellationToken::new();
        let mut harness = Harness::new(
            api.clone(),
            CancelWithFinalAnswer {
                cancel: cancel.clone(),
            },
            MemoryAudit::default(),
            MemoryJournal::default(),
            short_lifecycle_config(),
        );
        let error = harness
            .run(&SessionTask::demo(), cancel)
            .await
            .expect_err("canceled");
        assert!(
            matches!(error, HarnessError::Canceled),
            "{attempt}: {error}"
        );
        assert_eq!(api.state().final_checks, 0, "{attempt}");
    }
}

/// An audit whose final-answer record blocks the thread for a while, as a
/// slow disk would.
#[derive(Default)]
struct StallingAudit {
    inner: MemoryAudit,
    stall: Duration,
}

impl kiln_harness::audit::AuditSink for StallingAudit {
    fn append(
        &mut self,
        session_id: &str,
        sandbox_id: Option<&str>,
        event: AuditEvent,
    ) -> Result<(), kiln_harness::audit::AuditError> {
        if matches!(event, AuditEvent::FinalAnswer { .. }) {
            std::thread::sleep(self.stall);
        }
        self.inner.append(session_id, sandbox_id, event)
    }
}

#[tokio::test]
async fn a_final_check_after_the_deadline_cannot_complete_the_session() {
    // R7-03: the final answer's audit record stalls past a 50 ms session
    // deadline; the check must not record completion after it.
    let api = FakeApi::default();
    let config = HarnessConfig {
        session_timeout: Duration::from_millis(50),
        ..short_lifecycle_config()
    };
    let mut harness = Harness::new(
        api.clone(),
        Script::new(Duration::ZERO, [ModelResponse::final_text("done")]),
        StallingAudit {
            inner: MemoryAudit::default(),
            stall: Duration::from_millis(100),
        },
        MemoryJournal::default(),
        config,
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("deadline");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    assert_eq!(api.state().final_checks, 0);
}
