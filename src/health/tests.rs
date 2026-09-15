
use super::*;
use std::time::Duration;

fn limits() -> HealthLimits {
    HealthLimits {
        stale_after: Duration::from_millis(100),
        overload_high: 0.90,
        overload_low: 0.70,
    }
}

fn machine() -> (HealthMachine, FakeClock) {
    let clock = FakeClock::default();
    let machine = HealthMachine::new(clock.clone(), limits());
    (machine, clock)
}

fn ckpt() -> CheckpointIdentity {
    CheckpointIdentity {
        id: "soma16".into(),
        digest: Some("abc123".into()),
    }
}

fn bring_ready(m: &mut HealthMachine) {
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::InitializationCompleted);
    m.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    m.apply(HealthEvent::IngressObserved);
    m.apply(HealthEvent::TickSucceeded);
}

#[test]
fn process_start_is_live_but_not_ready() {
    let (mut m, _) = machine();
    let before = m.snapshot();
    assert!(!before.live);
    assert!(!before.ready);
    assert_eq!(before.phase, HealthPhase::Starting);

    m.apply(HealthEvent::ProcessStarted);
    let snap = m.snapshot();
    assert!(snap.live);
    assert!(!snap.ready);
    assert_eq!(snap.phase, HealthPhase::Starting);
    assert_eq!(snap.reasons, vec![ReasonCode::Starting]);
}

#[test]
fn initialization_does_not_imply_readiness() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::InitializationCompleted);
    let snap = m.snapshot();
    assert!(snap.live);
    assert!(!snap.ready);
    assert_eq!(snap.phase, HealthPhase::LoadingCheckpoint);
    assert_eq!(snap.reasons, vec![ReasonCode::CheckpointPending]);
}

#[test]
fn checkpoint_validation_makes_ready() {
    let (mut m, _) = machine();
    bring_ready(&mut m);
    let snap = m.snapshot();
    assert!(snap.live);
    assert!(snap.ready);
    assert_eq!(snap.phase, HealthPhase::Running);
    assert!(snap.reasons.is_empty());
    assert_eq!(
        snap.checkpoint.as_ref().map(|c| c.id.as_str()),
        Some("soma16")
    );
    assert_eq!(snap.last_successful_tick_ms, Some(0));
    assert!(!snap.input_freshness.stale);
}

#[test]
fn checkpoint_before_init_is_ignored() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    let snap = m.snapshot();
    assert!(!snap.ready);
    assert_eq!(snap.phase, HealthPhase::Starting);
    assert!(snap.checkpoint.is_none());
}

#[test]
fn stale_input_degrades_and_recovers_only_after_fresh_ingress() {
    let (mut m, clock) = machine();
    bring_ready(&mut m);

    clock.advance(Duration::from_millis(99));
    let still_ok = m.snapshot();
    assert_eq!(still_ok.phase, HealthPhase::Running);
    assert!(still_ok.ready);
    assert!(!still_ok.input_freshness.stale);

    clock.advance(Duration::from_millis(1));
    let degraded = m.snapshot();
    assert!(degraded.live);
    assert!(
        degraded.ready,
        "stale input is recoverable degradation, not unreadiness"
    );
    assert_eq!(degraded.phase, HealthPhase::Degraded);
    assert_eq!(degraded.reasons, vec![ReasonCode::StaleInput]);
    assert!(degraded.input_freshness.stale);

    clock.advance(Duration::from_millis(50));
    m.apply(HealthEvent::TickSucceeded);
    let still_stale = m.snapshot();
    assert!(
        still_stale.input_freshness.stale,
        "ticks without ingress must not clear stale_input"
    );

    m.apply(HealthEvent::IngressObserved);
    let recovered = m.snapshot();
    assert_eq!(recovered.phase, HealthPhase::Running);
    assert!(!recovered.input_freshness.stale);
    assert!(recovered.reasons.is_empty());
    assert!(recovered.ready);
}

#[test]
fn overload_uses_hysteresis_and_clears_only_below_low_watermark() {
    let (mut m, _) = machine();
    bring_ready(&mut m);

    m.apply(HealthEvent::QueuePressure {
        depth: 89,
        capacity: 100,
    });
    assert_eq!(m.snapshot().phase, HealthPhase::Running);

    m.apply(HealthEvent::QueuePressure {
        depth: 90,
        capacity: 100,
    });
    let high = m.snapshot();
    assert_eq!(high.phase, HealthPhase::Degraded);
    assert!(high.ready);
    assert_eq!(high.reasons, vec![ReasonCode::Overload]);
    assert!(high.queue_pressure.overloaded);

    m.apply(HealthEvent::QueuePressure {
        depth: 80,
        capacity: 100,
    });
    let mid = m.snapshot();
    assert!(
        mid.queue_pressure.overloaded,
        "must stay overloaded between high and low watermarks"
    );
    assert_eq!(mid.phase, HealthPhase::Degraded);

    m.apply(HealthEvent::QueuePressure {
        depth: 70,
        capacity: 100,
    });
    let recovered = m.snapshot();
    assert!(!recovered.queue_pressure.overloaded);
    assert_eq!(recovered.phase, HealthPhase::Running);
    assert!(recovered.ready);
}

#[test]
fn zero_capacity_queue_is_not_overload() {
    let (mut m, _) = machine();
    bring_ready(&mut m);
    m.apply(HealthEvent::QueuePressure {
        depth: 0,
        capacity: 0,
    });
    let snap = m.snapshot();
    assert!(!snap.queue_pressure.overloaded);
    assert_eq!(snap.queue_pressure.ratio, None);
    assert_eq!(snap.phase, HealthPhase::Running);
}

#[test]
fn combined_degradation_clears_independently() {
    let (mut m, clock) = machine();
    bring_ready(&mut m);
    m.apply(HealthEvent::QueuePressure {
        depth: 95,
        capacity: 100,
    });
    clock.advance(Duration::from_millis(100));
    let both = m.snapshot();
    assert_eq!(
        both.reasons,
        vec![ReasonCode::StaleInput, ReasonCode::Overload]
    );

    m.apply(HealthEvent::IngressObserved);
    let only_overload = m.snapshot();
    assert_eq!(only_overload.reasons, vec![ReasonCode::Overload]);
    assert_eq!(only_overload.phase, HealthPhase::Degraded);

    m.apply(HealthEvent::QueuePressure {
        depth: 10,
        capacity: 100,
    });
    let clear = m.snapshot();
    assert!(clear.reasons.is_empty());
    assert_eq!(clear.phase, HealthPhase::Running);
}

#[test]
fn draining_drops_readiness_and_does_not_return_to_ready() {
    let (mut m, _) = machine();
    bring_ready(&mut m);
    m.apply(HealthEvent::BeginDrain);
    let snap = m.snapshot();
    assert!(snap.live);
    assert!(!snap.ready);
    assert_eq!(snap.phase, HealthPhase::Draining);
    assert!(snap.reasons.contains(&ReasonCode::Draining));

    m.apply(HealthEvent::TickSucceeded);
    m.apply(HealthEvent::IngressObserved);
    m.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    m.apply(HealthEvent::InitializationCompleted);
    let after = m.snapshot();
    assert!(!after.ready);
    assert_eq!(after.phase, HealthPhase::Draining);
}

#[test]
fn initialization_failure_is_sticky_fatal() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::InitializationFailed {
        detail: "socket bind exploded with secret=hunter2".into(),
    });
    let snap = m.snapshot();
    assert!(snap.live);
    assert!(!snap.ready);
    assert_eq!(snap.phase, HealthPhase::Fatal);
    assert_eq!(snap.reasons, vec![ReasonCode::Fatal]);
    assert_eq!(
        snap.fatal.as_ref().map(|f| f.code),
        Some(FatalCode::InitializationFailed)
    );

    m.apply(HealthEvent::InitializationCompleted);
    m.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    m.apply(HealthEvent::TickSucceeded);
    let after = m.snapshot();
    assert!(!after.ready);
    assert_eq!(after.phase, HealthPhase::Fatal);
    assert_eq!(
        after.fatal.as_ref().map(|f| f.code),
        Some(FatalCode::InitializationFailed)
    );
}

#[test]
fn checkpoint_reject_is_sticky_fatal_from_loading() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::InitializationCompleted);
    m.apply(HealthEvent::CheckpointRejected {
        detail: "digest mismatch".into(),
    });
    assert_eq!(m.snapshot().phase, HealthPhase::Fatal);

    m.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    assert!(!m.snapshot().ready);
    assert_eq!(m.snapshot().phase, HealthPhase::Fatal);
}

#[test]
fn fatal_from_running_and_degraded_never_returns_to_ready() {
    let (mut running, _) = machine();
    bring_ready(&mut running);
    running.apply(HealthEvent::Fatal {
        code: FatalCode::Unspecified,
        detail: "network step invariant broken".into(),
    });
    assert_eq!(running.snapshot().phase, HealthPhase::Fatal);
    running.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    assert!(!running.snapshot().ready);

    let (mut degraded, clock) = machine();
    bring_ready(&mut degraded);
    clock.advance(Duration::from_millis(100));
    assert_eq!(degraded.snapshot().phase, HealthPhase::Degraded);
    degraded.apply(HealthEvent::Fatal {
        code: FatalCode::Unspecified,
        detail: "boom".into(),
    });
    degraded.apply(HealthEvent::IngressObserved);
    degraded.apply(HealthEvent::BeginDrain);
    let snap = degraded.snapshot();
    assert_eq!(snap.phase, HealthPhase::Fatal);
    assert!(!snap.ready);
    assert!(snap.live);
}

#[test]
fn fatal_from_draining_stays_fatal() {
    let (mut m, _) = machine();
    bring_ready(&mut m);
    m.apply(HealthEvent::BeginDrain);
    m.apply(HealthEvent::Fatal {
        code: FatalCode::Unspecified,
        detail: "flush failed".into(),
    });
    let snap = m.snapshot();
    assert_eq!(snap.phase, HealthPhase::Fatal);
    assert!(!snap.ready);
    m.apply(HealthEvent::CheckpointValidated { identity: ckpt() });
    assert_eq!(m.snapshot().phase, HealthPhase::Fatal);
}

#[test]
fn second_fatal_does_not_replace_the_first() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::InitializationFailed {
        detail: "first".into(),
    });
    m.apply(HealthEvent::Fatal {
        code: FatalCode::Unspecified,
        detail: "second".into(),
    });
    let fatal = m.snapshot().fatal.expect("fatal");
    assert_eq!(fatal.code, FatalCode::InitializationFailed);
    assert_eq!(fatal.detail, "first");
}

#[test]
fn prometheus_labels_are_low_cardinality_and_omit_detail() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    m.apply(HealthEvent::InitializationFailed {
        detail: "secret token xyzzy should never be a label".into(),
    });
    let text = m.snapshot().prometheus_text();
    assert!(text.contains("brainstem_live 1"));
    assert!(text.contains("brainstem_ready 0"));
    assert!(text.contains("brainstem_fatal 1"));
    assert!(text.contains("phase=\"fatal\""));
    assert!(!text.contains("xyzzy"));
    assert!(!text.contains("secret token"));
    assert!(text.contains("brainstem_tick_age_ms"));
    assert!(text.contains("reason=\"stale_input\""));
    assert!(text.contains("reason=\"overload\""));
}

#[test]
fn try_snapshot_does_not_block_on_write_lock() {
    let handle = HealthHandle::started(limits());
    let start = Instant::now();
    {
        let _guard = handle.lock_write_for_test();
        assert!(handle.try_snapshot().is_none());
    }
    assert!(start.elapsed() < Duration::from_millis(50));
    assert!(handle.try_snapshot().is_some());
    let snap = handle.snapshot();
    assert!(snap.live);
    assert!(!snap.ready);
}

#[test]
fn example_snapshots_match_documented_shapes() {
    let (mut healthy, _) = machine();
    bring_ready(&mut healthy);
    let healthy = healthy.snapshot();
    assert_eq!(
        serde_json::to_value(&healthy).unwrap(),
        serde_json::json!({
            "live": true,
            "ready": true,
            "phase": "running",
            "reasons": [],
            "last_successful_tick_ms": 0,
            "tick_age_ms": 0,
            "checkpoint": { "id": "soma16", "digest": "abc123" },
            "input_freshness": { "age_ms": 0, "stale": false },
            "queue_pressure": { "depth": 0, "capacity": 0, "ratio": null, "overloaded": false },
            "fatal": null,
            "observed_at_ms": 0
        })
    );

    let (mut degraded, clock) = machine();
    bring_ready(&mut degraded);
    degraded.apply(HealthEvent::QueuePressure {
        depth: 95,
        capacity: 100,
    });
    clock.advance(Duration::from_millis(100));
    let degraded = degraded.snapshot();
    assert_eq!(degraded.phase, HealthPhase::Degraded);
    assert_eq!(
        degraded.reasons,
        vec![ReasonCode::StaleInput, ReasonCode::Overload]
    );
    assert!(degraded.ready);

    let (mut fatal, _) = machine();
    fatal.apply(HealthEvent::ProcessStarted);
    fatal.apply(HealthEvent::InitializationCompleted);
    fatal.apply(HealthEvent::CheckpointRejected {
        detail: "blank weights".into(),
    });
    let fatal = fatal.snapshot();
    assert_eq!(fatal.phase, HealthPhase::Fatal);
    assert!(!fatal.ready);
    assert!(fatal.live);
    assert_eq!(fatal.fatal.unwrap().code, FatalCode::CheckpointInvalid);
}

#[test]
fn missing_digest_serializes_as_json_null() {
    let identity = CheckpointIdentity {
        id: "soma16".into(),
        digest: None,
    };
    assert_eq!(
        serde_json::to_value(&identity).unwrap(),
        serde_json::json!({ "id": "soma16", "digest": null })
    );
}

#[test]
fn non_finite_overload_limits_are_sanitized() {
    let clock = FakeClock::default();
    let mut machine = HealthMachine::new(
        clock,
        HealthLimits {
            stale_after: Duration::from_millis(100),
            overload_high: f64::NAN,
            overload_low: f64::NAN,
        },
    );
    bring_ready(&mut machine);
    machine.apply(HealthEvent::QueuePressure {
        depth: 95,
        capacity: 100,
    });
    assert!(machine.snapshot().queue_pressure.overloaded);
}
