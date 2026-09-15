// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

use serde::{Deserialize, Serialize};

/// Coarse phase derived from the snapshot. Stable, low-cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthPhase {
    Starting,
    LoadingCheckpoint,
    Running,
    Degraded,
    Draining,
    Fatal,
}

impl HealthPhase {
    pub const ALL: [HealthPhase; 6] = [
        HealthPhase::Starting,
        HealthPhase::LoadingCheckpoint,
        HealthPhase::Running,
        HealthPhase::Degraded,
        HealthPhase::Draining,
        HealthPhase::Fatal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HealthPhase::Starting => "starting",
            HealthPhase::LoadingCheckpoint => "loading_checkpoint",
            HealthPhase::Running => "running",
            HealthPhase::Degraded => "degraded",
            HealthPhase::Draining => "draining",
            HealthPhase::Fatal => "fatal",
        }
    }
}

/// Stable reason codes. Never put detailed error text in metric labels — use these codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    Starting,
    CheckpointPending,
    StaleInput,
    Overload,
    Draining,
    Fatal,
}

impl ReasonCode {
    pub const RECOVERABLE: [ReasonCode; 2] = [ReasonCode::StaleInput, ReasonCode::Overload];

    pub fn as_str(self) -> &'static str {
        match self {
            ReasonCode::Starting => "starting",
            ReasonCode::CheckpointPending => "checkpoint_pending",
            ReasonCode::StaleInput => "stale_input",
            ReasonCode::Overload => "overload",
            ReasonCode::Draining => "draining",
            ReasonCode::Fatal => "fatal",
        }
    }
}

/// Sticky fatal class. Low-cardinality; details live on [`FatalState::detail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FatalCode {
    InitializationFailed,
    CheckpointInvalid,
    Unspecified,
}

impl FatalCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FatalCode::InitializationFailed => "initialization_failed",
            FatalCode::CheckpointInvalid => "checkpoint_invalid",
            FatalCode::Unspecified => "unspecified",
        }
    }
}

/// Identity of the loaded checkpoint. Digest may be absent until real validation lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIdentity {
    pub id: String,
    pub digest: Option<String>,
}

/// Fatal snapshot payload. `detail` is for logs/JSON, never a Prometheus label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatalState {
    pub code: FatalCode,
    pub detail: String,
}

/// Ingress freshness relative to the health clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFreshness {
    pub age_ms: Option<u64>,
    pub stale: bool,
}

/// Bounded-queue pressure. `capacity == 0` means "no queue instrumented".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuePressure {
    pub depth: u64,
    pub capacity: u64,
    pub ratio: Option<f64>,
    pub overloaded: bool,
}

/// Machine-readable health view for supervisors and the control surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub live: bool,
    pub ready: bool,
    pub phase: HealthPhase,
    pub reasons: Vec<ReasonCode>,
    pub last_successful_tick_ms: Option<u64>,
    /// Milliseconds since the last successful tick (`None` if none yet).
    pub tick_age_ms: Option<u64>,
    pub checkpoint: Option<CheckpointIdentity>,
    pub input_freshness: InputFreshness,
    pub queue_pressure: QueuePressure,
    pub fatal: Option<FatalState>,
    pub observed_at_ms: u64,
}

impl HealthSnapshot {
    /// Prometheus text exposition. Labels are stable reason/phase codes only.
    pub fn prometheus_text(&self) -> String {
        let mut out = String::default();
        write_probe_gauges(&mut out, self);
        write_labeled_gauges(&mut out, self);
        write_runtime_gauges(&mut out, self);
        out
    }
}

fn write_probe_gauges(out: &mut String, snap: &HealthSnapshot) {
    push_gauge(
        out,
        "brainstem_live",
        "1 if the process health reporter is running.",
        u8::from(snap.live),
    );
    push_gauge(
        out,
        "brainstem_ready",
        "1 if initialized, checkpoint-valid, not draining, not fatal.",
        u8::from(snap.ready),
    );
}

fn write_labeled_gauges(out: &mut String, snap: &HealthSnapshot) {
    out.push_str("# HELP brainstem_phase 1 for the current health phase.\n");
    out.push_str("# TYPE brainstem_phase gauge\n");
    write_phase_gauges(out, snap.phase);
    out.push_str("# HELP brainstem_degraded Recoverable degradation by stable reason code.\n");
    out.push_str("# TYPE brainstem_degraded gauge\n");
    write_degraded_gauges(out, &snap.reasons);
}

fn write_runtime_gauges(out: &mut String, snap: &HealthSnapshot) {
    push_gauge(
        out,
        "brainstem_fatal",
        "1 if this process has entered a sticky fatal state.",
        u8::from(snap.fatal.is_some()),
    );
    push_gauge(
        out,
        "brainstem_last_successful_tick_ms",
        "Milliseconds from process start until the last successful tick (not tick age).",
        snap.last_successful_tick_ms.unwrap_or(0),
    );
    push_gauge(
        out,
        "brainstem_tick_age_ms",
        "Milliseconds since the last successful tick; 0 if none yet.",
        snap.tick_age_ms.unwrap_or(0),
    );
    push_gauge(
        out,
        "brainstem_input_age_ms",
        "Age of last ingress (or checkpoint, if none) in milliseconds.",
        snap.input_freshness.age_ms.unwrap_or(0),
    );
    push_gauge(
        out,
        "brainstem_queue_depth",
        "Ingress queue depth.",
        snap.queue_pressure.depth,
    );
    push_gauge(
        out,
        "brainstem_queue_capacity",
        "Ingress queue capacity.",
        snap.queue_pressure.capacity,
    );
}

fn push_gauge(out: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" gauge\n");
    out.push_str(name);
    out.push(' ');
    out.push_str(&value.to_string());
    out.push('\n');
}

fn write_phase_gauges(out: &mut String, current: HealthPhase) {
    for phase in HealthPhase::ALL {
        out.push_str(&format!(
            "brainstem_phase{{phase=\"{}\"}} {}\n",
            phase.as_str(),
            u8::from(current == phase)
        ));
    }
}

fn write_degraded_gauges(out: &mut String, reasons: &[ReasonCode]) {
    for reason in ReasonCode::RECOVERABLE {
        out.push_str(&format!(
            "brainstem_degraded{{reason=\"{}\"}} {}\n",
            reason.as_str(),
            u8::from(reasons.contains(&reason))
        ));
    }
}
