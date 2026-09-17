use super::*;
use std::time::Duration;

#[test]
fn documented_running_snapshot_uses_null_digest() {
    let (mut healthy, _) = machine();
    standin_ready(&mut healthy);
    assert_eq!(
        serde_json::to_value(healthy.snapshot()).unwrap(),
        serde_json::json!({
            "live": true,
            "ready": true,
            "phase": "running",
            "reasons": [],
            "last_successful_tick_ms": 0,
            "tick_age_ms": 0,
            "checkpoint": { "id": "soma16", "digest": null },
            "input_freshness": { "age_ms": 0, "stale": false },
            "queue_pressure": { "depth": 0, "capacity": 0, "ratio": null, "overloaded": false },
            "fatal": null,
            "observed_at_ms": 0
        })
    );
}

#[test]
fn documented_degraded_snapshot_stays_ready() {
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
}

#[test]
fn documented_fatal_snapshot_drops_readiness() {
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
fn missing_observations_render_as_prometheus_nan() {
    let (mut m, _) = machine();
    m.apply(HealthEvent::ProcessStarted);
    let text = m.snapshot().prometheus_text();
    assert!(text.contains("brainstem_last_successful_tick_ms NaN"));
    assert!(text.contains("brainstem_tick_age_ms NaN"));
    assert!(text.contains("brainstem_input_age_ms NaN"));
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

#[test]
fn inverted_overload_limits_keep_stale_after() {
    let limits = HealthLimits {
        stale_after: Duration::from_millis(100),
        overload_high: 0.50,
        overload_low: 0.90,
    }
    .sanitized();
    assert_eq!(limits.stale_after, Duration::from_millis(100));
    assert_eq!(limits.overload_high, 0.90);
    assert_eq!(limits.overload_low, 0.70);
}

#[test]
fn equal_overload_watermarks_are_replaced() {
    let limits = HealthLimits {
        stale_after: Duration::from_millis(100),
        overload_high: 0.80,
        overload_low: 0.80,
    }
    .sanitized();
    assert_eq!(limits.stale_after, Duration::from_millis(100));
    assert_eq!(limits.overload_high, 0.90);
    assert_eq!(limits.overload_low, 0.70);
}

#[test]
fn out_of_range_overload_watermarks_are_replaced() {
    let limits = HealthLimits {
        stale_after: Duration::from_millis(100),
        overload_high: 1.5,
        overload_low: -0.1,
    }
    .sanitized();
    assert_eq!(limits.stale_after, Duration::from_millis(100));
    assert_eq!(limits.overload_high, 0.90);
    assert_eq!(limits.overload_low, 0.70);
}
