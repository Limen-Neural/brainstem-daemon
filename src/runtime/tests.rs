// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Fault-injection matrix and restart-contract tests.

use super::*;
use anyhow::Result;

fn is_shutdown_fault(point: FaultPoint) -> bool {
    matches!(
        point,
        FaultPoint::Before(Boundary::Shutdown)
            | FaultPoint::After(Boundary::Shutdown)
            | FaultPoint::InterruptedShutdown
    )
}

fn drive(seed: u64, point: FaultPoint) -> Result<DriveReport> {
    let first = run_injected_session(seed, point)?;
    let terminal_health = first.health();
    let live_loop_entered = first.live_loop_entered();
    let published_before_crash = first.published().len() as u64;
    let metrics_before_crash = first.metrics().len() as u64;
    let inflight_at_crash = first.durable().inflight.clone();
    let committed_tick_seq = first.durable().committed_tick_seq;
    let committed_ingress_seq = first.durable().committed_ingress_seq;
    let last_session_id = first.durable().last_session_id;
    let restarted = boot_restart(
        seed,
        first.into_store(),
        committed_tick_seq,
        committed_ingress_seq,
        last_session_id,
    )?;
    Ok(DriveReport {
        terminal_health,
        restart: restarted
            .restart_outcome()
            .expect("boot always records a restart outcome"),
        published_before_crash,
        metrics_before_crash,
        live_loop_entered,
        inflight_at_crash,
        last_session_id,
        restart_session_id: restarted.session_id(),
        incomplete_prior: restarted.incomplete_prior(),
    })
}

fn run_injected_session(seed: u64, point: FaultPoint) -> Result<RuntimeHarness> {
    let mut first = match point {
        FaultPoint::MalformedCheckpoint => RuntimeHarness::builder(seed)
            .store(DurableStore::malformed(b"{not-a-checkpoint"))
            .fault(point)
            .build(),
        _ if is_shutdown_fault(point) => RuntimeHarness::builder(seed).build(),
        _ => RuntimeHarness::builder(seed).fault(point).build(),
    };

    let boot = first.boot();
    if is_shutdown_fault(point) {
        boot?;
        assert_eq!(first.run_tick()?, TickResult::Committed);
        first.arm_fault(point);
        let _ = first.shutdown();
    } else if boot.is_ok() && first.is_live() {
        let _ = first.run_tick();
    }
    Ok(first)
}

fn boot_restart(
    seed: u64,
    store: DurableStore,
    committed_tick_seq: u64,
    committed_ingress_seq: u64,
    last_session_id: u64,
) -> Result<RuntimeHarness> {
    let mut restarted = RuntimeHarness::builder(seed).store(store).build();
    let restart_boot = restarted.boot();
    let restart = restarted
        .restart_outcome()
        .expect("boot always records a restart outcome");
    if restart == RestartOutcome::RejectedInvalid {
        assert!(restart_boot.is_err());
        assert!(!restarted.is_live());
        assert!(!restarted.live_loop_entered());
    } else {
        restart_boot?;
        assert!(restarted.is_live());
        assert!(restarted.core().last_stimuli().is_none());
        assert!(restarted.published().is_empty());
        assert!(restarted.metrics().is_empty());
        assert_eq!(restarted.session_id(), last_session_id + 1);
        assert_eq!(restarted.durable().committed_tick_seq, committed_tick_seq);
        assert_eq!(
            restarted.durable().committed_ingress_seq,
            committed_ingress_seq
        );
    }
    Ok(restarted)
}

struct DriveReport {
    terminal_health: Health,
    restart: RestartOutcome,
    published_before_crash: u64,
    metrics_before_crash: u64,
    live_loop_entered: bool,
    inflight_at_crash: Option<InflightRecord>,
    last_session_id: u64,
    restart_session_id: u64,
    incomplete_prior: bool,
}

#[test]
fn fault_matrix_matches_oracle_for_all_seeds() {
    for &seed in FAULT_SEEDS {
        for &point in FaultPoint::all() {
            let expected = expected_outcome(point);
            let report = drive(seed, point).unwrap_or_else(|err| {
                panic!("seed={seed} point={point:?} failed: {err:#}");
            });
            assert_report_matches_oracle(seed, point, &report, expected);
        }
    }
}

fn assert_report_matches_oracle(
    seed: u64,
    point: FaultPoint,
    report: &DriveReport,
    expected: ExpectedOutcome,
) {
    assert_eq!(
        report.terminal_health, expected.terminal_health,
        "health seed={seed} point={point:?}"
    );
    assert_eq!(
        report.restart, expected.restart,
        "restart seed={seed} point={point:?}"
    );
    assert_eq!(
        report.published_before_crash, expected.published_before_crash,
        "published seed={seed} point={point:?}"
    );
    assert_eq!(
        report.metrics_before_crash, expected.published_before_crash,
        "metrics must match committed publishes seed={seed} point={point:?}"
    );
    assert_eq!(
        report.live_loop_entered, expected.live_loop_entered,
        "live loop seed={seed} point={point:?}"
    );
    assert_eq!(
        report.incomplete_prior,
        expected.restart == RestartOutcome::RecoveredIncomplete,
        "incomplete prior seed={seed} point={point:?}"
    );
    assert_session_and_inflight(seed, point, report, expected);
}

fn assert_session_and_inflight(
    seed: u64,
    point: FaultPoint,
    report: &DriveReport,
    expected: ExpectedOutcome,
) {
    if expected.restart != RestartOutcome::RejectedInvalid {
        assert_eq!(
            report.restart_session_id,
            report.last_session_id + 1,
            "monotonic session seed={seed} point={point:?}"
        );
    }
    if expected.restart == RestartOutcome::RecoveredIncomplete {
        assert!(
            report.inflight_at_crash.is_some(),
            "inflight missing seed={seed} point={point:?}"
        );
        assert!(report.restart_session_id > report.last_session_id);
    } else {
        assert!(
            report.inflight_at_crash.is_none(),
            "unexpected inflight seed={seed} point={point:?}"
        );
    }
}

#[test]
fn invalid_durable_state_fails_before_live_tick_loop() {
    for &seed in FAULT_SEEDS {
        for store in invalid_store_fixtures() {
            assert_boot_rejects_invalid(seed, store);
        }
    }
}

fn invalid_store_fixtures() -> Vec<DurableStore> {
    vec![
        DurableStore::malformed(b"{not-json"),
        DurableStore::malformed(b"BSDK0\n{}"),
        DurableStore::malformed(empty_checkpoint_blob()),
    ]
}

fn empty_checkpoint_blob() -> Vec<u8> {
    let mut raw = b"BSDK1\n".to_vec();
    raw.extend(
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "checkpoint_id": "",
            "last_session_id": 1,
            "committed_tick_seq": 0,
            "committed_ingress_seq": 0,
            "inflight": null
        }))
        .unwrap(),
    );
    raw
}

fn assert_boot_rejects_invalid(seed: u64, store: DurableStore) {
    let mut h = RuntimeHarness::builder(seed).store(store).build();
    let err = h.boot().unwrap_err();
    assert!(
        h.health() == Health::CheckpointInvalid,
        "seed={seed} health={:?} err={err}",
        h.health()
    );
    assert!(!h.is_live());
    assert!(!h.live_loop_entered());
    assert_eq!(h.restart_outcome(), Some(RestartOutcome::RejectedInvalid));
    assert!(
        h.run_tick().is_err(),
        "tick loop must refuse to start seed={seed}"
    );
}

#[test]
fn restart_after_partial_tick_does_not_publish_false_success_or_duplicate() {
    for &seed in FAULT_SEEDS {
        let mut first = RuntimeHarness::builder(seed)
            .fault(FaultPoint::After(Boundary::TickExecute))
            .build();
        first.boot().unwrap();
        assert!(first.run_tick().is_err());
        assert!(
            first.published().is_empty(),
            "partial tick must not publish seed={seed}"
        );
        assert!(
            first.metrics().is_empty(),
            "partial tick must not record success metrics seed={seed}"
        );
        let inflight = first.durable().inflight.clone().unwrap();
        let store = first.into_store();

        let mut second = RuntimeHarness::builder(seed).store(store).build();
        second.boot().unwrap();
        assert_eq!(
            second.restart_outcome(),
            Some(RestartOutcome::RecoveredIncomplete)
        );
        assert!(second.incomplete_prior());
        assert!(second.published().is_empty());
        assert_eq!(second.run_tick().unwrap(), TickResult::Committed);
        assert_eq!(second.published().len(), 1);
        assert_eq!(second.published()[0].tick_seq, inflight.tick_seq);
        assert_eq!(second.published()[0].ingress_seq, inflight.ingress_seq);
        assert!(second.published()[0].session_id > inflight.session_id);

        second.push_ingress_front(SequencedIngress {
            seq: inflight.ingress_seq,
            stimuli: vec![0.9, 0.1],
        });
        assert_eq!(second.run_tick().unwrap(), TickResult::SkippedReplay);
        assert_eq!(second.published().len(), 1);
        assert_eq!(second.metrics().len(), 1);
    }
}

#[test]
fn replayed_ingress_is_not_counted_as_fresh_work() {
    for &seed in FAULT_SEEDS {
        let mut first = RuntimeHarness::builder(seed).build();
        first.boot().unwrap();
        assert_eq!(first.run_tick().unwrap(), TickResult::Committed);
        assert_eq!(first.durable().committed_ingress_seq, 1);
        let session = first.session_id();
        let store = first.into_store();

        let mut second = RuntimeHarness::builder(seed).store(store).build();
        second.boot().unwrap();
        assert!(second.session_id() > session);
        assert_eq!(second.run_tick().unwrap(), TickResult::SkippedReplay);
        assert_eq!(second.skipped_replays(), 1);
        assert!(second.published().is_empty());
        assert_eq!(second.run_tick().unwrap(), TickResult::Committed);
        assert_eq!(second.published().len(), 1);
        assert_eq!(second.published()[0].ingress_seq, 2);
        assert_eq!(second.published()[0].tick_seq, 2);
    }
}

#[test]
fn session_and_tick_ids_are_monotonic_across_restart() {
    for &seed in FAULT_SEEDS {
        let mut first = RuntimeHarness::builder(seed).build();
        first.boot().unwrap();
        let s1 = first.session_id();
        assert_eq!(s1, 1);
        assert_eq!(first.run_tick().unwrap(), TickResult::Committed);
        assert_eq!(first.run_tick().unwrap(), TickResult::Committed);
        let t1 = first.durable().committed_tick_seq;
        assert_eq!(t1, 2);
        let store = first.into_store();

        let mut second = RuntimeHarness::builder(seed).store(store).build();
        second.boot().unwrap();
        let s2 = second.session_id();
        assert!(s2 > s1);
        assert_eq!(second.run_tick().unwrap(), TickResult::SkippedReplay);
        assert_eq!(second.run_tick().unwrap(), TickResult::SkippedReplay);
        assert_eq!(second.run_tick().unwrap(), TickResult::Committed);
        assert_eq!(second.published()[0].tick_seq, t1 + 1);
        assert!(second.published()[0].session_id > s1);
    }
}

#[test]
fn harness_is_synchronous_step_bounded_and_leaves_no_background_tasks() {
    let mut h = RuntimeHarness::builder(42).build();
    let start_ns = h.clock().now_ns();
    h.boot().unwrap();
    for _ in 0..8 {
        let _ = h.run_tick().unwrap();
    }
    h.shutdown().unwrap();
    assert!(h.steps_taken() <= MAX_STEPS);
    assert_eq!(h.health(), Health::Stopped);
    assert!(!h.channel_open());
    assert_eq!(h.clock().now_ns(), start_ns + TICK_PERIOD_NS * 8);
}

#[test]
fn expected_outcome_covers_every_injection_point() {
    for &point in FaultPoint::all() {
        let _ = expected_outcome(point);
    }
    assert_eq!(FaultPoint::all().len(), 17);
    assert_eq!(FAULT_SEEDS.len(), 6);
}
