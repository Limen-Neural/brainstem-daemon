// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::IngressPacket;

use super::{
    BoundedIngress, ClassMetrics, EnqueueOutcome, IngressConfig, MAX_BLOCK_TIMEOUT_MS,
    MAX_QUEUE_CAPACITY, MessageClass, OverflowPolicy,
};

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if started.elapsed() >= timeout {
            return pred();
        }
        thread::yield_now();
    }
}

fn pkt(tag: f32) -> IngressPacket {
    IngressPacket {
        stimuli: vec![tag],
        modulators: None,
    }
}

fn reward_pkt(tag: f32) -> IngressPacket {
    IngressPacket {
        stimuli: Vec::new(),
        modulators: Some(vec![tag, 0.0, 0.0, 0.0]),
    }
}

fn identity_holds(attempts: u64, metrics: ClassMetrics) {
    assert_eq!(
        metrics.accepted + metrics.rejected + metrics.shutdown_refused,
        attempts,
        "accepted + rejected + shutdown_refused must equal enqueue attempts: {metrics:?}"
    );
    assert_eq!(
        metrics.depth as u64 + metrics.lost_or_coalesced() + metrics.shutdown_refused,
        attempts,
        "stalled identity: depth + lost/coalesced + shutdown = attempts: {metrics:?}"
    );
}

#[test]
fn drop_oldest_bounds_depth_with_stalled_consumer() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    const N: u64 = 50;
    for i in 0..N {
        let outcome = ingress.enqueue(MessageClass::Sensory, pkt(i as f32));
        assert!(outcome.accepted());
    }
    let m = ingress.metrics().sensory;
    assert_eq!(m.depth, 2);
    assert_eq!(m.high_water_mark, 2);
    assert_eq!(m.accepted, N);
    assert_eq!(m.dropped, N - 2);
    assert_eq!(m.rejected, 0);
    assert_eq!(m.coalesced, 0);
    identity_holds(N, m);

    let a = ingress.pop(MessageClass::Sensory).unwrap();
    let b = ingress.pop(MessageClass::Sensory).unwrap();
    assert_eq!(a.stimuli[0], (N - 2) as f32);
    assert_eq!(b.stimuli[0], (N - 1) as f32);
    assert!(ingress.pop(MessageClass::Sensory).is_none());
}

#[test]
fn reject_does_not_grow_past_capacity() {
    let mut cfg = IngressConfig::tiny_fixture();
    cfg.sensory_policy = OverflowPolicy::Reject;
    cfg.sensory_capacity = 1;
    let ingress = BoundedIngress::new(cfg).unwrap();

    assert!(ingress.enqueue(MessageClass::Sensory, pkt(1.0)).accepted());
    assert_eq!(
        ingress.enqueue(MessageClass::Sensory, pkt(2.0)),
        EnqueueOutcome::Rejected
    );
    assert_eq!(
        ingress.enqueue(MessageClass::Sensory, pkt(3.0)),
        EnqueueOutcome::Rejected
    );

    let m = ingress.metrics().sensory;
    assert_eq!(m.depth, 1);
    assert_eq!(m.accepted, 1);
    assert_eq!(m.rejected, 2);
    identity_holds(3, m);
    assert_eq!(ingress.pop(MessageClass::Sensory).unwrap().stimuli[0], 1.0);
}

#[test]
fn coalesce_keeps_latest_snapshot() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    assert!(
        ingress
            .enqueue(MessageClass::Reward, reward_pkt(1.0))
            .accepted()
    );
    assert_eq!(
        ingress.enqueue(MessageClass::Reward, reward_pkt(2.0)),
        EnqueueOutcome::Coalesced { depth: 1 }
    );
    assert_eq!(
        ingress.enqueue(MessageClass::Reward, reward_pkt(3.0)),
        EnqueueOutcome::Coalesced { depth: 1 }
    );

    let m = ingress.metrics().reward;
    assert_eq!(m.depth, 1);
    assert_eq!(m.high_water_mark, 1);
    assert_eq!(m.accepted, 3);
    assert_eq!(m.coalesced, 2);
    assert_eq!(m.dropped, 0);
    assert_eq!(m.rejected, 0);
    identity_holds(3, m);

    let got = ingress.pop(MessageClass::Reward).unwrap();
    assert_eq!(got.modulators.unwrap()[0], 3.0);
}

#[test]
fn lost_or_coalesced_increments_exactly_one_counter() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    for i in 0..10 {
        let _ = ingress.enqueue(MessageClass::Sensory, pkt(i as f32));
        let _ = ingress.enqueue(MessageClass::Reward, reward_pkt(i as f32));
        let _ = ingress.enqueue(MessageClass::Control, pkt(i as f32));
        let _ = ingress.enqueue(MessageClass::Telemetry, pkt(i as f32));
    }

    for class in [
        MessageClass::Sensory,
        MessageClass::Reward,
        MessageClass::Control,
        MessageClass::Telemetry,
    ] {
        let m = ingress.metrics().class(class);
        let distinct_loss_buckets =
            u64::from(m.rejected > 0) + u64::from(m.dropped > 0) + u64::from(m.coalesced > 0);
        assert!(
            distinct_loss_buckets <= 1,
            "{class:?} mixed loss counters: {m:?}"
        );
        identity_holds(10, m);
    }
}

#[test]
fn control_is_not_starved_by_full_telemetry() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    for i in 0..20 {
        let _ = ingress.enqueue(MessageClass::Telemetry, pkt(i as f32));
    }
    let tel = ingress.metrics().telemetry;
    assert_eq!(tel.depth, 2);
    assert!(tel.dropped >= 18);

    let outcome = ingress.enqueue(MessageClass::Control, pkt(99.0));
    assert_eq!(outcome, EnqueueOutcome::Accepted { depth: 1 });

    let drained = ingress.drain_for_tick();
    assert_eq!(drained.control.len(), 1);
    assert_eq!(drained.control[0].stimuli[0], 99.0);
    assert_eq!(drained.telemetry_drained, 2);
    assert!(drained.sensory.is_none());
}

#[test]
fn drain_for_tick_priority_is_control_then_reward_then_sensory() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    let _ = ingress.enqueue(MessageClass::Telemetry, pkt(1.0));
    let _ = ingress.enqueue(MessageClass::Sensory, pkt(2.0));
    let _ = ingress.enqueue(MessageClass::Reward, reward_pkt(3.0));
    let _ = ingress.enqueue(MessageClass::Control, pkt(4.0));

    let drained = ingress.drain_for_tick();
    assert_eq!(drained.control.len(), 1);
    assert_eq!(drained.control[0].stimuli[0], 4.0);
    assert_eq!(drained.telemetry_drained, 1);
    let packet = drained.into_packet();
    assert_eq!(packet.stimuli, vec![2.0]);
    assert_eq!(packet.modulators.unwrap()[0], 3.0);
}

#[test]
fn block_timeout_zero_rejects_when_full_without_waiting() {
    let mut cfg = IngressConfig::tiny_fixture();
    cfg.control_policy = OverflowPolicy::BlockTimeout;
    cfg.control_capacity = 1;
    cfg.block_timeout_ms = 0;
    let ingress = BoundedIngress::new(cfg).unwrap();

    assert!(ingress.enqueue(MessageClass::Control, pkt(1.0)).accepted());
    assert_eq!(
        ingress.enqueue(MessageClass::Control, pkt(2.0)),
        EnqueueOutcome::Rejected
    );
    let m = ingress.metrics().control;
    assert_eq!(m.producer_waits, 0);
    assert_eq!(m.rejected, 1);
    assert_eq!(m.depth, 1);
}

#[test]
fn try_enqueue_never_blocks_block_timeout_class() {
    let cfg = IngressConfig {
        control_policy: OverflowPolicy::BlockTimeout,
        control_capacity: 1,
        block_timeout_ms: 60_000,
        ..IngressConfig::default()
    };
    let ingress = BoundedIngress::new(cfg).unwrap();
    assert!(
        ingress
            .try_enqueue(MessageClass::Control, pkt(1.0))
            .accepted()
    );
    let started = Instant::now();
    assert_eq!(
        ingress.try_enqueue(MessageClass::Control, pkt(2.0)),
        EnqueueOutcome::Rejected
    );
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "try_enqueue must return without honoring block_timeout_ms"
    );
}

#[test]
fn shutdown_unblocks_waiting_producer() {
    let mut cfg = IngressConfig::tiny_fixture();
    cfg.control_policy = OverflowPolicy::BlockTimeout;
    cfg.control_capacity = 1;
    cfg.block_timeout_ms = 60_000;
    let ingress = BoundedIngress::new(cfg).unwrap();
    assert!(ingress.enqueue(MessageClass::Control, pkt(1.0)).accepted());

    let producer = ingress.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome = producer.enqueue(MessageClass::Control, pkt(2.0));
        let _ = tx.send(outcome);
    });

    assert!(
        wait_until(Duration::from_secs(2), || {
            ingress.metrics().control.producer_waits > 0
        }),
        "producer never entered block_timeout wait"
    );
    ingress.shutdown();
    let outcome = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("producer hung after shutdown");
    assert_eq!(outcome, EnqueueOutcome::Shutdown);
    assert!(ingress.is_shutdown());

    assert_eq!(
        ingress.enqueue(MessageClass::Control, pkt(3.0)),
        EnqueueOutcome::Shutdown
    );
    let m = ingress.metrics().control;
    assert!(m.shutdown_refused >= 1);
}

#[test]
fn shutdown_rejects_every_class() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    ingress.shutdown();
    for class in MessageClass::ALL {
        assert_eq!(
            ingress.enqueue(class, pkt(1.0)),
            EnqueueOutcome::Shutdown,
            "{class:?} accepted after shutdown"
        );
    }
    assert!(ingress.is_shutdown());
}

#[test]
fn sustained_overload_bounds_memory_and_accounts_every_event() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    const N: u64 = 10_000;
    for i in 0..N {
        let _ = ingress.enqueue(MessageClass::Sensory, pkt(i as f32));
        let _ = ingress.enqueue(MessageClass::Reward, reward_pkt(i as f32));
        let _ = ingress.enqueue(MessageClass::Telemetry, pkt(i as f32));
        if i % 100 == 0 {
            let _ = ingress.enqueue(MessageClass::Control, pkt(i as f32));
        }
    }

    let m = ingress.metrics();
    assert_eq!(m.sensory.depth, 2);
    assert_eq!(m.sensory.high_water_mark, 2);
    identity_holds(N, m.sensory);

    assert_eq!(m.reward.depth, 1);
    assert_eq!(m.reward.high_water_mark, 1);
    identity_holds(N, m.reward);

    assert_eq!(m.telemetry.depth, 2);
    identity_holds(N, m.telemetry);

    let control_attempts = N / 100;
    assert_eq!(m.control.depth, 4);
    identity_holds(control_attempts, m.control);
    assert_eq!(m.control.accepted, 4);
    assert_eq!(m.control.rejected, control_attempts - 4);

    let snapshot = ingress.render_snapshot();
    assert!(snapshot.contains("| sensory | drop_oldest |"));
    assert!(snapshot.contains("| control | reject |"));
    assert!(snapshot.contains("| reward | coalesce |"));
    eprintln!("overload fixture metrics:\n{snapshot}");
}

#[test]
fn zero_capacity_is_rejected_at_construction() {
    let cfg = IngressConfig {
        sensory_capacity: 0,
        ..IngressConfig::default()
    };
    let err = match BoundedIngress::new(cfg) {
        Ok(_) => panic!("expected zero capacity to fail"),
        Err(err) => err,
    };
    let err = err.to_string();
    assert!(err.contains("sensory"));
    assert!(err.contains("capacity"));
}

#[test]
fn capacity_above_max_is_rejected_at_construction() {
    let cfg = IngressConfig {
        sensory_capacity: MAX_QUEUE_CAPACITY + 1,
        ..IngressConfig::default()
    };
    let err = BoundedIngress::new(cfg)
        .err()
        .expect("expected oversized capacity to fail")
        .to_string();
    assert!(err.contains("MAX_QUEUE_CAPACITY"));
}

#[test]
fn block_timeout_above_max_is_rejected_at_construction() {
    let cfg = IngressConfig {
        block_timeout_ms: MAX_BLOCK_TIMEOUT_MS + 1,
        ..IngressConfig::default()
    };
    let err = BoundedIngress::new(cfg)
        .err()
        .expect("expected oversized block_timeout_ms to fail")
        .to_string();
    assert!(err.contains("MAX_BLOCK_TIMEOUT_MS"));
}

#[test]
fn oversize_payload_is_rejected() {
    let mut cfg = IngressConfig::tiny_fixture();
    cfg.max_payload_len = 2;
    let ingress = BoundedIngress::new(cfg).unwrap();
    let big = IngressPacket {
        stimuli: vec![1.0, 2.0, 3.0],
        modulators: None,
    };
    assert_eq!(
        ingress.enqueue(MessageClass::Sensory, big),
        EnqueueOutcome::Rejected
    );
    assert_eq!(ingress.metrics().sensory.rejected, 1);
    assert_eq!(ingress.metrics().sensory.depth, 0);
}

#[test]
fn admit_backend_packet_splits_stimuli_and_modulators() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    ingress.admit_backend_packet(IngressPacket {
        stimuli: vec![0.5, 0.25],
        modulators: Some(vec![1.0, 2.0, 3.0, 4.0]),
    });
    let drained = ingress.drain_for_tick();
    let packet = drained.into_packet();
    assert_eq!(packet.stimuli, vec![0.5, 0.25]);
    assert_eq!(packet.modulators.unwrap()[0], 1.0);
}

#[test]
fn admit_skips_empty_backend_placeholder() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    let _ = ingress.enqueue(MessageClass::Sensory, pkt(7.0));
    ingress.admit_backend_packet(IngressPacket::default());
    assert_eq!(ingress.metrics().sensory.depth, 1);
    assert_eq!(ingress.metrics().reward.depth, 0);
    assert_eq!(ingress.pop(MessageClass::Sensory).unwrap().stimuli[0], 7.0);
}

#[test]
fn admit_empty_stimuli_still_enqueues_modulators() {
    let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
    ingress.admit_backend_packet(IngressPacket {
        stimuli: Vec::new(),
        modulators: Some(vec![1.0, 2.0, 3.0, 4.0]),
    });
    assert_eq!(ingress.metrics().sensory.depth, 0);
    assert_eq!(ingress.metrics().reward.depth, 1);
    assert_eq!(
        ingress
            .pop(MessageClass::Reward)
            .unwrap()
            .modulators
            .unwrap()[0],
        1.0
    );
}
