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

/// Identity of the loaded checkpoint. Live mode uses the sidecar SHA-256; simulation uses `None`.
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

/// Monotonic runtime counters published from the tick loop.
///
/// Mirrors the numeric `RuntimeStats` counters. Values only increase for the
/// life of the process; the health machine ignores any publish that would
/// lower one. Exposed in `/health` JSON and as unlabeled `_total` Prometheus
/// counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCounters {
    pub ticks: u64,
    pub accepted_batches: u64,
    pub rejected_batches: u64,
    pub receive_errors: u64,
    pub emit_errors: u64,
    pub dropped_spikes: u64,
    pub diagnostic_emissions: u64,
    pub suppressed_diagnostics: u64,
}

impl RuntimeCounters {
    /// Field-wise maximum, so a published counter can never move backwards.
    pub fn merged_monotonic(self, next: RuntimeCounters) -> RuntimeCounters {
        RuntimeCounters {
            ticks: self.ticks.max(next.ticks),
            accepted_batches: self.accepted_batches.max(next.accepted_batches),
            rejected_batches: self.rejected_batches.max(next.rejected_batches),
            receive_errors: self.receive_errors.max(next.receive_errors),
            emit_errors: self.emit_errors.max(next.emit_errors),
            dropped_spikes: self.dropped_spikes.max(next.dropped_spikes),
            diagnostic_emissions: self.diagnostic_emissions.max(next.diagnostic_emissions),
            suppressed_diagnostics: self.suppressed_diagnostics.max(next.suppressed_diagnostics),
        }
    }

    /// `(metric name, help, value)` for each counter. Names are stable.
    fn metrics(&self) -> [(&'static str, &'static str, u64); 8] {
        [
            (
                "brainstem_ticks_total",
                "Ticks that stepped the network.",
                self.ticks,
            ),
            (
                "brainstem_accepted_batches_total",
                "Backend batches accepted into ingress.",
                self.accepted_batches,
            ),
            (
                "brainstem_rejected_batches_total",
                "Batches rejected (backend-flagged, receive failure, or non-finite).",
                self.rejected_batches,
            ),
            (
                "brainstem_receive_errors_total",
                "Backend receive failures, including rate-limited repeats.",
                self.receive_errors,
            ),
            (
                "brainstem_emit_errors_total",
                "Spike emission failures, including rate-limited repeats.",
                self.emit_errors,
            ),
            (
                "brainstem_dropped_spikes_total",
                "Spikes discarded because their IDs did not fit the wire type.",
                self.dropped_spikes,
            ),
            (
                "brainstem_diagnostic_emissions_total",
                "Tick diagnostics emitted after rate limiting.",
                self.diagnostic_emissions,
            ),
            (
                "brainstem_suppressed_diagnostics_total",
                "Recurring diagnostic occurrences suppressed by rate limiting.",
                self.suppressed_diagnostics,
            ),
        ]
    }
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
    /// Absent in snapshots serialized before counters existed; defaults to zero.
    #[serde(default)]
    pub counters: RuntimeCounters,
    pub observed_at_ms: u64,
}

impl HealthSnapshot {
    /// Prometheus text exposition. Labels are stable reason/phase codes only.
    pub fn prometheus_text(&self) -> String {
        let mut out = String::default();
        write_probe_gauges(&mut out, self);
        write_labeled_gauges(&mut out, self);
        write_runtime_gauges(&mut out, self);
        write_runtime_counters(&mut out, &self.counters);
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
    push_optional_gauge(
        out,
        "brainstem_last_successful_tick_ms",
        "Milliseconds from process start until the last successful tick (not tick age); NaN if none yet.",
        snap.last_successful_tick_ms,
    );
    push_optional_gauge(
        out,
        "brainstem_tick_age_ms",
        "Milliseconds since the last successful tick; NaN if none yet.",
        snap.tick_age_ms,
    );
    push_optional_gauge(
        out,
        "brainstem_input_age_ms",
        "Age of last ingress (or checkpoint, if none) in milliseconds; NaN if none yet.",
        snap.input_freshness.age_ms,
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

fn write_runtime_counters(out: &mut String, counters: &RuntimeCounters) {
    for (name, help, value) in counters.metrics() {
        push_metric_header(out, name, help, "counter");
        push_sample(out, name, value);
    }
}

fn push_optional_gauge(out: &mut String, name: &str, help: &str, value: Option<u64>) {
    match value {
        Some(v) => push_gauge(out, name, help, v),
        None => push_gauge(out, name, help, "NaN"),
    }
}

fn push_gauge(out: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    push_metric_header(out, name, help, "gauge");
    push_sample(out, name, value);
}

fn push_metric_header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

fn push_sample(out: &mut String, name: &str, value: impl std::fmt::Display) {
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
