//! Keyed create: the request ID, retries after an ambiguous failure, the
//! settled-state check, and request-ID release at cleanup.
mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{CreateScript, DeleteBehavior, FakeApi, HealthScript, api_error};
use kiln_harness::api::{
    ApiError, Capabilities, Deleted, Execution, Feature, FinalCheck, Health, NodeReservations,
    Sandbox, SandboxApi, SandboxExec, SandboxRequest, SandboxState,
};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask};
use kiln_harness::journal::{JournalEvent, MemoryJournal};
use kiln_harness::provider::ScriptedProvider;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tonic::Code;

type Session = Harness<FakeApi, ScriptedProvider, MemoryAudit, MemoryJournal>;
type Over<A> = Harness<A, ScriptedProvider, MemoryAudit, MemoryJournal>;

fn session(api: &FakeApi, config: HarnessConfig) -> Session {
    Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    )
}

async fn run(harness: &mut Session) -> Result<SessionOutcome, HarnessError> {
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
}

/// Run the session and cancel it as soon as `ready` holds.
async fn run_and_cancel_when(
    harness: &mut Session,
    ready: impl Fn() -> bool,
) -> Result<SessionOutcome, HarnessError> {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let task = SessionTask::demo();
    let watch = async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        trigger.cancel();
    };
    tokio::join!(harness.run(&task, cancel), watch).0
}

fn reasoned(code: Code, reason: &str) -> ApiError {
    common::api_error_with_reason(code, reason, &format!("{reason}: refused"))
}

fn unavailable() -> ApiError {
    api_error(Code::Unavailable, "connection reset")
}

/// Answer the session's own discovery Health from state.
fn discovery() -> HealthScript {
    HealthScript::Delay(Duration::ZERO)
}

fn session_id(harness: &Session) -> String {
    harness.audit().records[0].session_id.clone()
}

fn created(harness: &Session) -> Option<(Option<String>, u32)> {
    harness
        .audit()
        .records
        .iter()
        .find_map(|record| match &record.event {
            AuditEvent::SandboxCreated {
                request_id,
                create_attempts,
                ..
            } => Some((request_id.clone(), *create_attempts)),
            _ => None,
        })
}

/// `(sandbox_ids, deleted, create_attempts)` of the recovery record.
fn recovered<A: SandboxApi>(harness: &Over<A>) -> Option<(Vec<String>, bool, u32)> {
    harness
        .audit()
        .records
        .iter()
        .find_map(|record| match &record.event {
            AuditEvent::SandboxRecovered {
                sandbox_ids,
                deleted,
                create_attempts,
            } => Some((sandbox_ids.clone(), *deleted, *create_attempts)),
            _ => None,
        })
}

/// `(outcome, work_outcome, sandbox_deleted, request_id_released)`.
fn finished<A: SandboxApi>(harness: &Over<A>) -> (String, String, bool, Option<bool>) {
    match harness.audit().records.last().map(|record| &record.event) {
        Some(AuditEvent::SessionFinished {
            outcome,
            work_outcome,
            sandbox_deleted,
            request_id_released,
            ..
        }) => (
            outcome.clone(),
            work_outcome.clone(),
            *sandbox_deleted,
            *request_id_released,
        ),
        other => panic!("no session_finished: {other:?}"),
    }
}

fn audit_json(harness: &Session, event: &str) -> Value {
    harness
        .audit()
        .records
        .iter()
        .map(|record| serde_json::to_value(record).expect("encode"))
        .find(|record| record["event"] == event)
        .unwrap_or_else(|| panic!("no {event}"))
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(ToString::to_string).collect()
}

fn model_was_asked<A: SandboxApi>(harness: &Over<A>) -> bool {
    harness
        .journal()
        .records
        .iter()
        .any(|record| matches!(record.event, JournalEvent::ModelRequested { .. }))
}

fn never_released(api: &FakeApi) -> bool {
    !api.state().deletes_released.contains(&true)
}

// 1. A daemon without idempotent create gets today's create and audit.
#[tokio::test(start_paused = true)]
async fn without_the_feature_no_key_is_sent_and_the_audit_keeps_its_shape() {
    // Release listed alone means nothing without idempotent create.
    let api = FakeApi::with_expiry();
    api.state().features.insert(Feature::RequestIdRelease);
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");

    let state = api.state();
    assert_eq!(state.create_requests.len(), 1);
    assert_eq!(state.create_requests[0].request_id, None);
    assert!(!state.deletes_released.is_empty());
    assert!(!state.deletes_released.contains(&true));
    drop(state);
    assert_eq!(created(&harness), Some((None, 1)));
    assert_eq!(
        finished(&harness),
        ("completed".to_string(), "completed".to_string(), true, None)
    );
    let common = ["sequence", "unix_ms", "session_id", "sandbox_id", "event"];
    let created = audit_json(&harness, "sandbox_created");
    let mut expected = set(&common);
    expected.extend(set(&[
        "backend_id",
        "expiry_lifetime_ms",
        "expiry_idle_ms",
        "create_attempts",
    ]));
    assert_eq!(keys(&created), expected);
    let finished = audit_json(&harness, "session_finished");
    let mut expected = set(&common);
    expected.extend(set(&[
        "outcome",
        "work_outcome",
        "sandbox_deleted",
        "renews",
    ]));
    assert_eq!(keys(&finished), expected);
}

#[tokio::test(start_paused = true)]
async fn an_unkeyed_create_is_never_retried() {
    let api = FakeApi::with_expiry();
    api.state()
        .create_script
        .push_back(CreateScript::Err(unavailable()));
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("create failed");
    assert!(matches!(error, HarnessError::CreateUncertain(_)), "{error}");
    assert_eq!(api.state().create_requests.len(), 1);
    assert_eq!(api.state().health_calls, 1);
    assert_eq!(recovered(&harness).map(|record| record.2), Some(1));
}

// 2. The key and the equal request.
#[tokio::test(start_paused = true)]
async fn every_attempt_sends_the_session_key_with_an_equal_request() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::Err(unavailable()));
        state
            .create_script
            .push_back(CreateScript::Err(api_error(Code::Unknown, "transport")));
    }
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");

    let key = format!("kiln-harness:{}", session_id(&harness));
    assert!(key.len() <= 64);
    assert!(key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)));
    let state = api.state();
    assert_eq!(state.create_requests.len(), 3);
    assert_eq!(
        state.create_requests[0].request_id.as_deref(),
        Some(key.as_str())
    );
    let first = &state.create_requests[0];
    assert!(state.create_requests.iter().all(|request| request == first));
    drop(state);
    assert_eq!(created(&harness), Some((Some(key), 3)));
}

// 3. A lost response is retried and answered from the key.
#[tokio::test(start_paused = true)]
async fn a_lost_response_is_retried_and_answered_with_the_same_sandbox() {
    let api = FakeApi::keyed(true);
    api.state()
        .create_script
        .push_back(CreateScript::ErrAfterRecord(unavailable()));
    let mut harness = session(&api, HarnessConfig::default());
    let outcome = run(&mut harness).await.expect("session completes");

    assert_eq!(outcome.sandbox_id, "sb-0000000000000001");
    let state = api.state();
    assert_eq!(state.creates.len(), 1, "one sandbox exists");
    assert_eq!(state.create_requests.len(), 2);
    // Discovery, then one reread before the retry.
    assert_eq!(state.health_calls, 2);
    assert_eq!(state.deleted, ["sb-0000000000000001"]);
    drop(state);
    assert_eq!(created(&harness).map(|record| record.1), Some(2));
    assert_eq!(finished(&harness).0, "completed");
}

// 4. A retry that found the original still running is retried again.
#[tokio::test(start_paused = true)]
async fn create_in_flight_is_retried() {
    let api = FakeApi::keyed(false);
    api.state()
        .create_script
        .push_back(CreateScript::ErrAfterRecord(reasoned(
            Code::DeadlineExceeded,
            "create_in_flight",
        )));
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().create_requests.len(), 2);
    assert_eq!(api.state().creates.len(), 1);
    assert_eq!(created(&harness).map(|record| record.1), Some(2));
}

#[tokio::test(start_paused = true)]
async fn every_ambiguous_code_is_retried() {
    for code in [
        Code::Unavailable,
        Code::Unknown,
        Code::DeadlineExceeded,
        Code::Aborted,
        Code::Internal,
        Code::Cancelled,
    ] {
        let api = FakeApi::keyed(false);
        api.state()
            .create_script
            .push_back(CreateScript::Err(api_error(code, "ambiguous")));
        let mut harness = session(&api, HarnessConfig::default());
        run(&mut harness).await.expect("session completes");
        assert_eq!(api.state().create_requests.len(), 2, "{code:?}");
    }
}

// 5. Definite and conflicting refusals are final at once.
#[tokio::test(start_paused = true)]
async fn refusals_are_not_retried_and_keep_todays_result() {
    for (error, uncertain) in [
        (api_error(Code::InvalidArgument, "bad request_id"), false),
        (reasoned(Code::ResourceExhausted, "request_id_quota"), false),
        (reasoned(Code::AlreadyExists, "request_id_conflict"), true),
        (api_error(Code::FailedPrecondition, "not serving"), false),
        (api_error(Code::PermissionDenied, "denied"), false),
        (api_error(Code::Unimplemented, "unimplemented"), true),
        (api_error(Code::NotFound, "not found"), true),
    ] {
        let api = FakeApi::keyed(true);
        api.state()
            .create_script
            .push_back(CreateScript::Err(error.clone()));
        let mut harness = session(&api, HarnessConfig::default());
        let result = run(&mut harness).await.expect_err("create refused");
        assert_eq!(api.state().create_requests.len(), 1, "{error}");
        assert_eq!(api.state().health_calls, 1, "{error}");
        if uncertain {
            assert!(
                matches!(&result, HarnessError::CreateUncertain(e) if e == &error),
                "{result}"
            );
            assert_eq!(recovered(&harness), Some((Vec::new(), false, 1)));
            assert_eq!(finished(&harness).0, "cleanup_failed");
        } else {
            assert!(
                matches!(&result, HarnessError::Api(e) if e == &error),
                "{result}"
            );
            assert_eq!(recovered(&harness), Some((Vec::new(), true, 1)));
            assert_eq!(finished(&harness).0, "create_failed");
        }
        assert!(never_released(&api));
    }
}

// 6. Health is reread before every retry.
#[tokio::test(start_paused = true)]
async fn a_daemon_not_serving_or_without_the_feature_is_not_sent_a_retry() {
    let withdrawn = BTreeSet::from([Feature::SandboxExpiry, Feature::TypedReasons]);
    for answer in [HealthScript::NotServing, HealthScript::Features(withdrawn)] {
        let api = FakeApi::keyed(true);
        {
            let mut state = api.state();
            state
                .create_script
                .push_back(CreateScript::Err(unavailable()));
            state.health_script.extend([discovery(), answer.clone()]);
        }
        let mut harness = session(&api, HarnessConfig::default());
        let error = run(&mut harness).await.expect_err("no retry");
        assert!(
            matches!(&error, HarnessError::CreateUncertain(e) if e.code == Code::Unavailable),
            "{answer:?}: {error}"
        );
        assert_eq!(api.state().create_requests.len(), 1, "{answer:?}");
        assert_eq!(api.state().health_calls, 2, "{answer:?}");
        assert_eq!(recovered(&harness), Some((Vec::new(), false, 1)));
    }
}

#[tokio::test(start_paused = true)]
async fn a_withdrawn_feature_after_a_lost_response_recovers_the_row_without_release() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::ErrAfterRecord(unavailable()));
        state.health_script.extend([
            discovery(),
            HealthScript::Features(BTreeSet::from([Feature::SandboxExpiry])),
        ]);
    }
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("no retry");
    assert!(
        matches!(&error, HarnessError::Api(e) if e.code == Code::Unavailable),
        "{error}"
    );
    assert_eq!(api.state().create_requests.len(), 1);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 1))
    );
    assert!(never_released(&api));
    assert_eq!(finished(&harness).3, None);
}

#[tokio::test(start_paused = true)]
async fn health_errors_back_off_without_using_an_attempt() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::Err(unavailable()));
        state.health_script.extend([
            discovery(),
            HealthScript::Err(unavailable()),
            HealthScript::Err(unavailable()),
        ]);
    }
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    let state = api.state();
    assert_eq!(state.create_requests.len(), 2);
    assert_eq!(state.health_calls, 4);
    // Backoff grows across the Health rereads: 100, 200, then 400 ms.
    let gap = state.create_sent_at[1] - state.create_sent_at[0];
    assert_eq!(gap, Duration::from_millis(700));
    drop(state);
    assert_eq!(created(&harness).map(|record| record.1), Some(2));
}

// 7. Bounds: attempts, backoff, and the retry window.
#[tokio::test(start_paused = true)]
async fn an_always_unavailable_create_stops_at_eight_attempts_with_capped_backoff() {
    let api = FakeApi::keyed(true);
    api.state().create_rejection = Some(unavailable());
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("never answers");
    assert!(
        matches!(&error, HarnessError::CreateUncertain(e) if e.code == Code::Unavailable),
        "{error}"
    );
    let state = api.state();
    assert_eq!(state.create_requests.len(), 8);
    let gaps = state
        .create_sent_at
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    let expected = [100, 200, 400, 800, 1600, 2000, 2000]
        .map(Duration::from_millis)
        .to_vec();
    assert_eq!(gaps, expected);
    drop(state);
    assert_eq!(recovered(&harness).map(|record| record.2), Some(8));
    assert!(never_released(&api));
}

#[tokio::test(start_paused = true)]
async fn retries_stop_when_the_lifecycle_window_closes() {
    let api = FakeApi::keyed(true);
    api.state().create_rejection = Some(unavailable());
    let config = HarnessConfig {
        lifecycle_timeout: Duration::from_millis(1000),
        ..HarnessConfig::default()
    };
    let mut harness = session(&api, config);
    let started = Instant::now();
    let error = run(&mut harness).await.expect_err("never answers");
    assert!(matches!(error, HarnessError::CreateUncertain(_)), "{error}");
    // Backoffs of 100, 200 and 400 ms fit; the next would end past 1 s.
    assert_eq!(api.state().create_requests.len(), 4);
    assert_eq!(recovered(&harness).map(|record| record.2), Some(4));
    // The retry phase, then the 1 s recovery window.
    assert!(started.elapsed() < Duration::from_millis(1800));
}

// 8. Cancellation and the deadline during the retry phase.
#[tokio::test(start_paused = true)]
async fn cancellation_during_a_backoff_recovers_and_sends_no_further_create() {
    let api = FakeApi::keyed(true);
    api.state()
        .create_script
        .push_back(CreateScript::ErrAfterRecord(unavailable()));
    let observer = api.clone();
    let mut harness = session(&api, HarnessConfig::default());
    let result =
        run_and_cancel_when(&mut harness, || observer.state().create_requests.len() == 1).await;
    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    let state = api.state();
    assert_eq!(state.create_requests.len(), 1);
    assert_eq!(state.health_calls, 1, "canceled before the Health reread");
    assert_eq!(state.deleted, ["sb-0000000000000001"]);
    // Recovery started at once, not after the 100 ms backoff.
    let waited = state.delete_sent_at[0] - state.create_sent_at[0];
    assert!(waited < Duration::from_millis(10), "{waited:?}");
    drop(state);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 1))
    );
    assert_eq!(finished(&harness).1, "canceled");
    assert!(never_released(&api));
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_the_health_reread_recovers_and_sends_no_further_create() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::ErrAfterRecord(unavailable()));
        state
            .health_script
            .extend([discovery(), HealthScript::Delay(Duration::from_secs(5))]);
    }
    let observer = api.clone();
    let mut harness = session(&api, HarnessConfig::default());
    let result = run_and_cancel_when(&mut harness, || observer.state().health_calls == 2).await;
    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    assert_eq!(api.state().create_requests.len(), 1);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 1))
    );
    assert!(never_released(&api));
}

fn one_second_session() -> HarnessConfig {
    HarnessConfig {
        session_timeout: Duration::from_secs(1),
        ..HarnessConfig::default()
    }
}

#[tokio::test(start_paused = true)]
async fn the_deadline_during_a_backoff_is_a_deadline() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::ErrAfterRecord(unavailable()));
        for _ in 0..7 {
            state
                .create_script
                .push_back(CreateScript::Err(unavailable()));
        }
    }
    let mut harness = session(&api, one_second_session());
    let started = Instant::now();
    let error = run(&mut harness).await.expect_err("deadline");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    // 100 + 200 + 400 ms of backoff fit; the 800 ms one reaches the deadline.
    assert_eq!(api.state().create_requests.len(), 4);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 4))
    );
    // The deadline, then recovery's 1 s window at most.
    assert!(started.elapsed() <= Duration::from_secs(2));
    assert_eq!(finished(&harness).1, "create_failed");
}

#[tokio::test(start_paused = true)]
async fn the_deadline_during_the_health_reread_is_a_deadline() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::Err(unavailable()));
        state
            .health_script
            .extend([discovery(), HealthScript::Delay(Duration::from_secs(5))]);
    }
    let mut harness = session(&api, one_second_session());
    let error = run(&mut harness).await.expect_err("deadline");
    // Nothing was recorded, so the deadline leaves the create uncertain.
    assert!(
        matches!(&error, HarnessError::CreateUncertain(e) if e.code == Code::DeadlineExceeded),
        "{error}"
    );
    assert_eq!(api.state().create_requests.len(), 1);
    assert_eq!(recovered(&harness), Some((Vec::new(), false, 1)));
}

// 9. A create answered OK is accepted only READY and, keyed, with our key.
#[tokio::test(start_paused = true)]
async fn a_create_settled_failed_is_recovered_and_reported() {
    let api = FakeApi::keyed(true);
    api.state().create_script.push_back(CreateScript::Settled(
        SandboxState::Failed,
        "backend refused the image".to_string(),
    ));
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("settled FAILED");
    let HarnessError::Api(error) = error else {
        panic!("{error}");
    };
    assert_eq!(error.code, Code::FailedPrecondition);
    assert_eq!(error.reason, None);
    assert_eq!(
        error.message,
        "create settled sandbox sb-0000000000000001 as FAILED: backend refused the image"
    );
    let state = api.state();
    assert_eq!(state.create_requests.len(), 1, "not retried");
    assert!(state.execs.is_empty());
    assert_eq!(state.deleted, ["sb-0000000000000001"]);
    assert!(!state.deletes_released.contains(&true));
    drop(state);
    assert!(!model_was_asked(&harness));
    assert_eq!(created(&harness), None);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 1))
    );
    assert_eq!(
        finished(&harness),
        (
            "create_failed".to_string(),
            "create_failed".to_string(),
            true,
            None
        )
    );
}

#[tokio::test(start_paused = true)]
async fn every_state_but_ready_is_a_settled_create_failure() {
    for state in [
        SandboxState::Creating,
        SandboxState::Deleting,
        SandboxState::Lost,
        SandboxState::Quarantined,
        SandboxState::Other(42),
    ] {
        let api = FakeApi::keyed(true);
        api.state()
            .create_script
            .push_back(CreateScript::Settled(state, String::new()));
        let mut harness = session(&api, HarnessConfig::default());
        let error = run(&mut harness).await.expect_err("not ready");
        assert!(
            matches!(&error, HarnessError::Api(e)
                if e.message == format!("create settled sandbox sb-0000000000000001 as {}", state.as_str())),
            "{error}"
        );
        assert_eq!(api.state().create_requests.len(), 1);
        assert!(api.state().execs.is_empty());
        assert_eq!(finished(&harness).1, "create_failed");
    }
}

#[tokio::test(start_paused = true)]
async fn a_create_settled_deleted_is_confirmed_absent_by_the_listing() {
    let api = FakeApi::keyed(true);
    api.state()
        .create_script
        .push_back(CreateScript::Settled(SandboxState::Deleted, String::new()));
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("settled DELETED");
    assert!(
        matches!(&error, HarnessError::Api(e) if e.message.contains("as DELETED")),
        "{error}"
    );
    assert!(api.state().deletes_released.is_empty());
    assert_eq!(recovered(&harness), Some((Vec::new(), true, 1)));
    assert_eq!(
        finished(&harness),
        (
            "create_failed".to_string(),
            "create_failed".to_string(),
            true,
            None
        )
    );
}

#[tokio::test(start_paused = true)]
async fn an_echo_of_another_key_is_cleaned_up_as_a_create_failure() {
    let api = FakeApi::keyed(true);
    api.state()
        .create_script
        .push_back(CreateScript::EchoRequestId(
            "kiln-harness:another".to_string(),
        ));
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("echo mismatch");
    assert!(
        matches!(&error, HarnessError::Api(e)
            if e.code == Code::Unknown && e.message.contains("kiln-harness:another")),
        "{error}"
    );
    assert!(api.state().execs.is_empty());
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    assert!(never_released(&api));
    assert_eq!(finished(&harness).1, "create_failed");
}

#[tokio::test(start_paused = true)]
async fn an_unrecorded_echo_mismatch_is_absent_by_the_listing_not_uncertain() {
    // The answer is local: only the listing proves absence, and an empty
    // successful listing does.
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state.suppress_create_record = true;
        state
            .create_script
            .push_back(CreateScript::EchoRequestId(String::new()));
    }
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("empty echo");
    assert!(
        matches!(&error, HarnessError::Api(e) if e.code == Code::Unknown),
        "{error}"
    );
    assert_eq!(recovered(&harness), Some((Vec::new(), true, 1)));
    assert_eq!(finished(&harness).0, "create_failed");
}

#[tokio::test(start_paused = true)]
async fn an_unkeyed_create_answered_failed_is_guarded_too() {
    let api = FakeApi::with_expiry();
    api.state()
        .create_script
        .push_back(CreateScript::Settled(SandboxState::Failed, "x".to_string()));
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("settled FAILED");
    assert!(
        matches!(&error, HarnessError::Api(e) if e.message.ends_with("as FAILED: x")),
        "{error}"
    );
    assert!(api.state().execs.is_empty());
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    assert_eq!(finished(&harness).1, "create_failed");
}

#[tokio::test(start_paused = true)]
async fn an_unlisted_settled_row_whose_delete_fails_is_a_cleanup_failure() {
    // A legacy-kiln ambiguous FAILED row: Delete refuses it, so its absence
    // is unconfirmed and reported, never hidden.
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state
            .create_script
            .push_back(CreateScript::Settled(SandboxState::Failed, String::new()));
        state.delete_behavior = DeleteBehavior::AcknowledgeOnly;
    }
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("unconfirmed");
    assert!(matches!(error, HarnessError::Cleanup(_)), "{error}");
    assert_eq!(
        finished(&harness),
        (
            "cleanup_failed".to_string(),
            "create_failed".to_string(),
            false,
            None
        )
    );
    assert!(never_released(&api));
}

// 10. Release at the normal cleanup.
#[tokio::test(start_paused = true)]
async fn cleanup_releases_the_key_when_both_features_are_listed() {
    let api = FakeApi::keyed(true);
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().deletes_released, [true]);
    assert!(api.state().request_id_released);
    assert_eq!(
        finished(&harness),
        (
            "completed".to_string(),
            "completed".to_string(),
            true,
            Some(true)
        )
    );
    assert_eq!(
        audit_json(&harness, "session_finished")["request_id_released"],
        true
    );
}

#[tokio::test(start_paused = true)]
async fn without_release_listed_the_key_is_kept() {
    let api = FakeApi::keyed(false);
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    assert!(!api.state().deletes_released.is_empty());
    assert!(never_released(&api));
    assert!(created(&harness).and_then(|record| record.0).is_some());
    assert_eq!(finished(&harness).3, None);
    assert!(
        audit_json(&harness, "session_finished")
            .get("request_id_released")
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn a_delete_that_released_nothing_is_repeated_after_absence() {
    let api = FakeApi::keyed(true);
    api.state().release_withheld = 1;
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().deletes_released, [true, true]);
    assert_eq!(finished(&harness).3, Some(true));
}

#[tokio::test(start_paused = true)]
async fn a_delete_acknowledged_before_removal_releases_on_a_later_delete() {
    let api = FakeApi::keyed(true);
    api.state().delete_acknowledgements_before_removal = 1;
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().deletes_released, [true, true]);
    assert_eq!(finished(&harness).3, Some(true));
}

#[tokio::test(start_paused = true)]
async fn a_release_never_confirmed_leaves_cleanup_confirmed() {
    let api = FakeApi::keyed(true);
    api.state().release_refused = true;
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session still completes");
    // The cleanup delete, then two more after absence.
    assert_eq!(api.state().deletes_released, [true, true, true]);
    assert_eq!(
        finished(&harness),
        (
            "completed".to_string(),
            "completed".to_string(),
            true,
            Some(false)
        )
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_cleanup_reports_the_key_unreleased() {
    let api = FakeApi::keyed(true);
    api.state().delete_behavior = DeleteBehavior::AcknowledgeOnly;
    let mut harness = session(&api, HarnessConfig::default());
    let error = run(&mut harness).await.expect_err("cleanup unconfirmed");
    assert!(matches!(error, HarnessError::Cleanup(_)), "{error}");
    assert_eq!(
        finished(&harness),
        (
            "cleanup_failed".to_string(),
            "completed".to_string(),
            false,
            Some(false)
        )
    );
}

// Decision 4: an attempt runs until it answers; none is abandoned and retried.
#[tokio::test(start_paused = true)]
async fn a_slow_keyed_create_is_waited_for_not_retried() {
    let api = FakeApi::keyed(true);
    {
        let mut state = api.state();
        state.create_delay = Duration::from_secs(20);
        // The fake's readiness clock is the wall clock; time here is paused.
        state.create_allows_early_delete = true;
    }
    let mut harness = session(&api, HarnessConfig::default());
    run(&mut harness).await.expect("session completes");
    assert_eq!(api.state().create_requests.len(), 1);
    assert_eq!(api.state().health_calls, 1);
    assert_eq!(created(&harness).map(|record| record.1), Some(1));
}

/// Answers a create with `Some(error)` without reaching the fake.
type CreateHook = dyn Fn(u32, &SandboxRequest) -> Option<ApiError> + Send + Sync;

/// A [`FakeApi`] with hooks that run inside the Create and Health futures,
/// before they answer, given the call's number from 1.
#[derive(Clone)]
struct Hooked {
    inner: FakeApi,
    creates: Arc<AtomicU32>,
    healths: Arc<AtomicU32>,
    on_create: Arc<CreateHook>,
    on_health: Arc<dyn Fn(u32) + Send + Sync>,
}

impl Hooked {
    fn new(inner: &FakeApi) -> Self {
        Self {
            inner: inner.clone(),
            creates: Arc::default(),
            healths: Arc::default(),
            on_create: Arc::new(|_, _| None),
            on_health: Arc::new(|_| {}),
        }
    }

    fn session(&self) -> Over<Self> {
        Harness::new(
            self.clone(),
            ScriptedProvider::demo(),
            MemoryAudit::default(),
            MemoryJournal::default(),
            HarnessConfig::default(),
        )
    }
}

#[async_trait]
impl SandboxApi for Hooked {
    async fn health(&self) -> Result<Health, ApiError> {
        (self.on_health)(self.healths.fetch_add(1, Ordering::SeqCst) + 1);
        self.inner.health().await
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        let call = self.creates.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(error) = (self.on_create)(call, &request) {
            return Err(error);
        }
        self.inner.create(request).await
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        self.inner.exec(request).await
    }

    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        self.inner.recent_executions(sandbox_id).await
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, ApiError> {
        self.inner.delete(sandbox_id, release_request_id).await
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), ApiError> {
        self.inner.renew(sandbox_id).await
    }

    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError> {
        self.inner.get_execution(execution_id).await
    }

    async fn final_check(&self, sandbox_id: &str) -> Result<FinalCheck, ApiError> {
        self.inner.final_check(sandbox_id).await
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        self.inner.non_deleted_sandboxes().await
    }
}

// Review round 1, F-02: after a retry, cleanup keeps the key, so an
// abandoned attempt that reaches kilnd late is answered from the retained
// DELETED row and creates nothing.
#[tokio::test(start_paused = true)]
async fn a_retried_create_keeps_its_key_so_a_late_first_attempt_creates_nothing() {
    let fake = FakeApi::keyed(true);
    let mut api = Hooked::new(&fake);
    // The first attempt fails in transport before kilnd has it; the test
    // delivers it after the session has ended.
    let abandoned = Arc::new(Mutex::new(None));
    let stash = abandoned.clone();
    api.on_create = Arc::new(move |call, request| {
        (call == 1).then(|| {
            *stash.lock().expect("stash") = Some(request.clone());
            api_error(Code::Unavailable, "transport failed before sending")
        })
    });
    let mut harness = api.session();
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("the retry completes the session");

    assert_eq!(api.creates.load(Ordering::SeqCst), 2);
    assert!(
        fake.state().deletes_released.iter().all(|release| !release),
        "{:?}",
        fake.state().deletes_released
    );
    // One cleanup delete, and no extra release deletes.
    assert_eq!(fake.state().deletes_released.len(), 1);
    assert_eq!(
        finished(&harness),
        ("completed".to_string(), "completed".to_string(), true, None)
    );
    assert!(
        harness
            .audit()
            .records
            .iter()
            .map(|record| serde_json::to_value(record).expect("encode"))
            .find(|record| record["event"] == "session_finished")
            .expect("session_finished")
            .get("request_id_released")
            .is_none()
    );

    let late = abandoned
        .lock()
        .expect("stash")
        .take()
        .expect("first attempt");
    let answer = fake.create(late).await.expect("answered from the record");
    assert_eq!(answer.id, "sb-0000000000000001");
    assert_eq!(answer.state, SandboxState::Deleted);
    assert_eq!(fake.state().creates.len(), 1, "no new sandbox");
    assert!(
        fake.non_deleted_sandboxes()
            .await
            .expect("listing")
            .is_empty()
    );
}

// Review round 1, F-03: cancellation inside the last Create, as it fails,
// wins over the attempt cap.
#[tokio::test(start_paused = true)]
async fn cancellation_during_the_last_create_wins_over_the_attempt_cap() {
    let fake = FakeApi::keyed(true);
    for _ in 0..8 {
        fake.state()
            .create_script
            .push_back(CreateScript::ErrAfterRecord(unavailable()));
    }
    let cancel = CancellationToken::new();
    let mut api = Hooked::new(&fake);
    let trigger = cancel.clone();
    api.on_create = Arc::new(move |call, _| {
        if call == 8 {
            trigger.cancel();
        }
        None
    });
    let mut harness = api.session();
    let result = harness.run(&SessionTask::demo(), cancel).await;
    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    assert_eq!(fake.state().create_requests.len(), 8);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 8))
    );
    assert_eq!(finished(&harness).1, "canceled");
    assert!(!fake.state().deletes_released.contains(&true));
}

// Cancellation inside the Health reread wins over a daemon that is no
// longer serving.
#[tokio::test(start_paused = true)]
async fn cancellation_during_a_refusing_health_reread_is_canceled() {
    let fake = FakeApi::keyed(true);
    {
        let mut state = fake.state();
        state
            .create_script
            .push_back(CreateScript::ErrAfterRecord(unavailable()));
        state
            .health_script
            .extend([discovery(), HealthScript::NotServing]);
    }
    let cancel = CancellationToken::new();
    let mut api = Hooked::new(&fake);
    let trigger = cancel.clone();
    api.on_health = Arc::new(move |call| {
        if call == 2 {
            trigger.cancel();
        }
    });
    let mut harness = api.session();
    let result = harness.run(&SessionTask::demo(), cancel).await;
    assert!(matches!(result, Err(HarnessError::Canceled)), "{result:?}");
    assert_eq!(fake.state().create_requests.len(), 1);
    assert_eq!(fake.state().health_calls, 2);
    assert_eq!(
        recovered(&harness),
        Some((vec!["sb-0000000000000001".to_string()], true, 1))
    );
    assert_eq!(finished(&harness).1, "canceled");
}

// A create that is never sent is not counted as an attempt. Cancellation
// inside the discovery Health still lets discovery answer, so the session
// reaches the create already canceled.
#[tokio::test(start_paused = true)]
async fn a_session_canceled_before_its_create_counts_no_attempt() {
    let fake = FakeApi::keyed(true);
    let cancel = CancellationToken::new();
    let mut api = Hooked::new(&fake);
    let trigger = cancel.clone();
    api.on_health = Arc::new(move |_| trigger.cancel());
    let mut harness = api.session();
    let result = harness.run(&SessionTask::demo(), cancel).await;
    assert!(
        matches!(result, Err(HarnessError::CreateUncertain(ref error)) if error.code == Code::Cancelled),
        "{result:?}"
    );
    assert_eq!(api.creates.load(Ordering::SeqCst), 0);
    assert_eq!(recovered(&harness).map(|record| record.2), Some(0));
    assert_eq!(finished(&harness).1, "canceled");
}

/// A daemon whose endpoint is taken over by an older build that ignores
/// `request_id` once the retry's Health reread has answered.
#[derive(Clone, Default)]
struct Replaced {
    state: Arc<Mutex<ReplacedState>>,
}

#[derive(Default)]
struct ReplacedState {
    health_calls: u32,
    create_calls: u32,
    old_daemon: bool,
    rows: Vec<Sandbox>,
    deleted: BTreeSet<String>,
    release_flags: Vec<bool>,
    max_live: usize,
}

impl ReplacedState {
    fn live(&self) -> impl Iterator<Item = &Sandbox> {
        self.rows
            .iter()
            .filter(|row| !self.deleted.contains(&row.id))
    }
}

#[async_trait]
impl SandboxApi for Replaced {
    async fn health(&self) -> Result<Health, ApiError> {
        let mut state = self.state.lock().expect("state");
        state.health_calls += 1;
        // The reread is answered by the current daemon; the older one takes
        // the endpoint before the retry is sent.
        if state.health_calls == 2 {
            state.old_daemon = true;
        }
        Ok(Health {
            serving: true,
            detail: String::new(),
            active_sandboxes: 0,
            running_executions: 0,
            reservations: NodeReservations::default(),
            capabilities: Capabilities {
                features: BTreeSet::from([
                    Feature::IdempotentCreate,
                    Feature::RequestIdRelease,
                    Feature::ExecCwd,
                    Feature::ExecEnv,
                ]),
                limits: Some(common::exec_limits()),
            },
        })
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        let mut state = self.state.lock().expect("state");
        state.create_calls += 1;
        // The older daemon ignores the key: a new sandbox, no echo.
        let (id, echo) = if state.old_daemon {
            ("sb-00000000000000b2", String::new())
        } else {
            (
                "sb-00000000000000a1",
                request.request_id.clone().unwrap_or_default(),
            )
        };
        let sandbox = Sandbox {
            id: id.to_string(),
            backend_id: format!("backend-{id}"),
            labels: request.labels,
            expiry: None,
            state: SandboxState::Ready,
            failure_reason: String::new(),
            request_id: echo,
            request_id_released: false,
        };
        state.rows.push(sandbox.clone());
        state.max_live = state.max_live.max(state.live().count());
        if state.create_calls == 1 {
            return Err(api_error(Code::Unavailable, "first response lost"));
        }
        Ok(sandbox)
    }

    async fn exec(&self, _request: SandboxExec) -> Result<Execution, ApiError> {
        unreachable!("the empty echo stops the session before work")
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        unreachable!("the empty echo stops the session before work")
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, ApiError> {
        let mut state = self.state.lock().expect("state");
        state.release_flags.push(release_request_id);
        state.deleted.insert(sandbox_id.to_string());
        Ok(Deleted::default())
    }

    async fn renew(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        unreachable!("the empty echo stops the session before work")
    }

    async fn get_execution(&self, _execution_id: &str) -> Result<Execution, ApiError> {
        unreachable!("the empty echo stops the session before work")
    }

    async fn final_check(&self, _sandbox_id: &str) -> Result<FinalCheck, ApiError> {
        unreachable!("the empty echo stops the session before work")
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        Ok(self.state.lock().expect("state").live().cloned().collect())
    }
}

// Review round 1, F-01, recorded rather than fixed: the Health reread cannot
// be bound to the Create after it. A replacement in that gap gets the retry
// and makes a second sandbox; the empty echo stops the session, and recovery
// deletes both rows before it ends, releasing nothing.
#[tokio::test(start_paused = true)]
async fn a_replacement_between_the_reread_and_the_retry_is_stopped_and_recovered() {
    let api = Replaced::default();
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig::default(),
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("the empty echo is a create failure");
    assert!(
        matches!(error, HarnessError::Api(ref error) if error.code == Code::Unknown),
        "{error}"
    );

    let state = api.state.lock().expect("state");
    assert_eq!((state.health_calls, state.create_calls), (2, 2));
    assert_eq!(state.max_live, 2, "a brief duplicate existed");
    assert_eq!(state.live().count(), 0, "both rows are gone");
    assert!(state.release_flags.iter().all(|release| !release));
    drop(state);
    assert_eq!(
        recovered(&harness),
        Some((
            vec![
                "sb-00000000000000a1".to_string(),
                "sb-00000000000000b2".to_string()
            ],
            true,
            2
        ))
    );
    assert_eq!(
        finished(&harness),
        (
            "create_failed".to_string(),
            "create_failed".to_string(),
            true,
            None
        )
    );
    assert!(!model_was_asked(&harness));
}
