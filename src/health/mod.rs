// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Process health: liveness, readiness, recoverable degradation, and sticky fatal state.
//!
//! Supervisors should treat [`HealthSnapshot::live`] and [`HealthSnapshot::ready`] as
//! independent. A live process has not necessarily loaded a valid checkpoint. Recoverable
//! reasons (`stale_input`, `overload`) clear only after the condition is observed healthy.
//! Fatal and draining states never return to ready in the same process.
//!
//! Snapshot reads take a separate lock from the tick loop's backend and network, so they
//! do not wait on `StimulusSource` or `SpikingNetwork::step`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Monotonic clock used by the health state machine.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Wall-clock monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock. Clone and share the same offset with a [`HealthMachine`].
#[derive(Debug, Clone)]
pub struct FakeClock {
    origin: Instant,
    offset_nanos: Arc<AtomicU64>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, duration: Duration) {
        let add = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.offset_nanos.fetch_add(add, Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            offset_nanos: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.origin + Duration::from_nanos(self.offset_nanos.load(Ordering::SeqCst))
    }
}

/// Thresholds for recoverable degradation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthLimits {
    /// Ingress older than this marks `stale_input`.
    pub stale_after: Duration,
    /// Queue fill ratio that *enters* overload (`depth / capacity`).
    pub overload_high: f64,
    /// Queue fill ratio that *exits* overload (hysteresis; must be `<= overload_high`).
    pub overload_low: f64,
}

impl Default for HealthLimits {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_millis(500),
            overload_high: 0.90,
            overload_low: 0.70,
        }
    }
}

impl HealthLimits {
    /// Replace non-finite or inverted watermarks with the built-in defaults.
    pub fn sanitized(self) -> Self {
        let high = finite_or(self.overload_high, 0.90);
        let low = finite_or(self.overload_low, 0.70);
        if high >= low {
            Self {
                stale_after: self.stale_after,
                overload_high: high,
                overload_low: low,
            }
        } else {
            Self::default()
        }
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

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
        push_gauge(
            &mut out,
            "brainstem_live",
            "1 if the process health reporter is running.",
            u8::from(self.live),
        );
        push_gauge(
            &mut out,
            "brainstem_ready",
            "1 if initialized, checkpoint-valid, not draining, not fatal.",
            u8::from(self.ready),
        );

        out.push_str("# HELP brainstem_phase 1 for the current health phase.\n");
        out.push_str("# TYPE brainstem_phase gauge\n");
        for phase in HealthPhase::ALL {
            out.push_str(&format!(
                "brainstem_phase{{phase=\"{}\"}} {}\n",
                phase.as_str(),
                u8::from(self.phase == phase)
            ));
        }

        out.push_str("# HELP brainstem_degraded Recoverable degradation by stable reason code.\n");
        out.push_str("# TYPE brainstem_degraded gauge\n");
        for reason in ReasonCode::RECOVERABLE {
            let active = self.reasons.contains(&reason);
            out.push_str(&format!(
                "brainstem_degraded{{reason=\"{}\"}} {}\n",
                reason.as_str(),
                u8::from(active)
            ));
        }

        push_gauge(
            &mut out,
            "brainstem_fatal",
            "1 if this process has entered a sticky fatal state.",
            u8::from(self.fatal.is_some()),
        );
        push_gauge(
            &mut out,
            "brainstem_last_successful_tick_ms",
            "Milliseconds from process start until the last successful tick (not tick age).",
            self.last_successful_tick_ms.unwrap_or(0),
        );
        push_gauge(
            &mut out,
            "brainstem_tick_age_ms",
            "Milliseconds since the last successful tick; 0 if none yet.",
            self.tick_age_ms.unwrap_or(0),
        );
        push_gauge(
            &mut out,
            "brainstem_input_age_ms",
            "Age of last ingress (or checkpoint, if none) in milliseconds.",
            self.input_freshness.age_ms.unwrap_or(0),
        );
        push_gauge(
            &mut out,
            "brainstem_queue_depth",
            "Ingress queue depth.",
            self.queue_pressure.depth,
        );
        push_gauge(
            &mut out,
            "brainstem_queue_capacity",
            "Ingress queue capacity.",
            self.queue_pressure.capacity,
        );
        out
    }
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

/// State-machine events. Tick-loop I/O never runs while these are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthEvent {
    ProcessStarted,
    InitializationCompleted,
    InitializationFailed { detail: String },
    CheckpointValidated { identity: CheckpointIdentity },
    CheckpointRejected { detail: String },
    TickSucceeded,
    IngressObserved,
    QueuePressure { depth: u64, capacity: u64 },
    BeginDrain,
    Fatal { code: FatalCode, detail: String },
}

/// Pure health state machine. Drive it with [`FakeClock`] in tests.
pub struct HealthMachine {
    clock: Arc<dyn Clock>,
    limits: HealthLimits,
    started: bool,
    initialized: bool,
    checkpoint_ok: bool,
    draining: bool,
    started_at: Option<Instant>,
    checkpoint_at: Option<Instant>,
    last_tick: Option<Instant>,
    last_ingress: Option<Instant>,
    checkpoint: Option<CheckpointIdentity>,
    queue_depth: u64,
    queue_capacity: u64,
    overloaded: bool,
    fatal: Option<FatalState>,
}

impl HealthMachine {
    pub fn new(clock: impl Clock + 'static, limits: HealthLimits) -> Self {
        Self {
            clock: Arc::new(clock),
            limits: limits.sanitized(),
            started: false,
            initialized: false,
            checkpoint_ok: false,
            draining: false,
            started_at: None,
            checkpoint_at: None,
            last_tick: None,
            last_ingress: None,
            checkpoint: None,
            queue_depth: 0,
            queue_capacity: 0,
            overloaded: false,
            fatal: None,
        }
    }

    pub fn apply(&mut self, event: HealthEvent) {
        let now = self.clock.now();
        match event {
            HealthEvent::TickSucceeded
            | HealthEvent::IngressObserved
            | HealthEvent::QueuePressure { .. } => self.apply_runtime(event, now),
            other => self.apply_lifecycle(other, now),
        }
    }

    fn apply_lifecycle(&mut self, event: HealthEvent, now: Instant) {
        match event {
            HealthEvent::ProcessStarted => {
                if !self.started {
                    self.started = true;
                    self.started_at = Some(now);
                }
            }
            HealthEvent::InitializationCompleted => {
                if self.fatal.is_none() && !self.draining {
                    self.initialized = true;
                }
            }
            HealthEvent::InitializationFailed { detail } => {
                self.enter_fatal(FatalCode::InitializationFailed, detail);
            }
            HealthEvent::CheckpointValidated { identity } => {
                if self.fatal.is_none() && !self.draining && self.initialized {
                    self.checkpoint = Some(identity);
                    self.checkpoint_ok = true;
                    self.checkpoint_at = Some(now);
                }
            }
            HealthEvent::CheckpointRejected { detail } => {
                self.enter_fatal(FatalCode::CheckpointInvalid, detail);
            }
            HealthEvent::BeginDrain => {
                if self.fatal.is_none() {
                    self.draining = true;
                }
            }
            HealthEvent::Fatal { code, detail } => {
                self.enter_fatal(code, detail);
            }
            HealthEvent::TickSucceeded
            | HealthEvent::IngressObserved
            | HealthEvent::QueuePressure { .. } => {}
        }
    }

    fn apply_runtime(&mut self, event: HealthEvent, now: Instant) {
        match event {
            HealthEvent::TickSucceeded => self.last_tick = Some(now),
            HealthEvent::IngressObserved => self.last_ingress = Some(now),
            HealthEvent::QueuePressure { depth, capacity } => {
                self.queue_depth = depth;
                self.queue_capacity = capacity;
                self.overloaded = next_overload(
                    self.overloaded,
                    depth,
                    capacity,
                    self.limits.overload_high,
                    self.limits.overload_low,
                );
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        let now = self.clock.now();
        let origin = self.started_at.unwrap_or(now);
        let ms = |t: Instant| duration_ms(t.saturating_duration_since(origin));
        let age_base = self.last_ingress.or(self.checkpoint_at);
        let age_ms = age_base.map(|t| duration_ms(now.saturating_duration_since(t)));
        let stale = self.compute_stale(now);
        let live = self.started;
        let ready = live
            && self.initialized
            && self.checkpoint_ok
            && !self.draining
            && self.fatal.is_none();
        let ratio = if self.queue_capacity == 0 {
            None
        } else {
            Some(self.queue_depth as f64 / self.queue_capacity as f64)
        };

        HealthSnapshot {
            live,
            ready,
            phase: self.phase(stale),
            reasons: self.reasons(stale),
            last_successful_tick_ms: self.last_tick.map(ms),
            tick_age_ms: self
                .last_tick
                .map(|t| duration_ms(now.saturating_duration_since(t))),
            checkpoint: self.checkpoint.clone(),
            input_freshness: InputFreshness { age_ms, stale },
            queue_pressure: QueuePressure {
                depth: self.queue_depth,
                capacity: self.queue_capacity,
                ratio,
                overloaded: self.overloaded,
            },
            fatal: self.fatal.clone(),
            observed_at_ms: ms(now),
        }
    }

    fn reasons(&self, stale: bool) -> Vec<ReasonCode> {
        if self.fatal.is_some() {
            return vec![ReasonCode::Fatal];
        }
        let mut reasons = Vec::default();
        if !self.initialized {
            reasons.push(ReasonCode::Starting);
        } else if !self.checkpoint_ok {
            reasons.push(ReasonCode::CheckpointPending);
        }
        if self.draining {
            reasons.push(ReasonCode::Draining);
        }
        if stale {
            reasons.push(ReasonCode::StaleInput);
        }
        if self.overloaded {
            reasons.push(ReasonCode::Overload);
        }
        reasons
    }

    fn phase(&self, stale: bool) -> HealthPhase {
        if self.fatal.is_some() {
            HealthPhase::Fatal
        } else if self.draining {
            HealthPhase::Draining
        } else if !self.initialized {
            HealthPhase::Starting
        } else if !self.checkpoint_ok {
            HealthPhase::LoadingCheckpoint
        } else if stale || self.overloaded {
            HealthPhase::Degraded
        } else {
            HealthPhase::Running
        }
    }

    fn enter_fatal(&mut self, code: FatalCode, detail: String) {
        if self.fatal.is_some() {
            return;
        }
        self.fatal = Some(FatalState { code, detail });
        self.checkpoint_ok = false;
    }

    fn compute_stale(&self, now: Instant) -> bool {
        if !self.initialized || !self.checkpoint_ok {
            return false;
        }
        let baseline = match self.last_ingress.or(self.checkpoint_at) {
            Some(t) => t,
            None => return false,
        };
        now.saturating_duration_since(baseline) >= self.limits.stale_after
    }
}

fn next_overload(currently: bool, depth: u64, capacity: u64, high: f64, low: f64) -> bool {
    if capacity == 0 {
        return false;
    }
    let ratio = depth as f64 / capacity as f64;
    if currently {
        ratio > low
    } else {
        ratio >= high
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Cloneable, non-tick-blocking handle for supervisors and the control surface.
#[derive(Clone)]
pub struct HealthHandle {
    inner: Arc<RwLock<HealthMachine>>,
}

impl HealthHandle {
    /// Live process, not yet ready. Used when the daemon is constructed.
    pub fn started(limits: HealthLimits) -> Self {
        let mut machine = HealthMachine::new(SystemClock, limits);
        machine.apply(HealthEvent::ProcessStarted);
        Self {
            inner: Arc::new(RwLock::new(machine)),
        }
    }

    pub fn from_machine(machine: HealthMachine) -> Self {
        Self {
            inner: Arc::new(RwLock::new(machine)),
        }
    }

    pub fn apply(&self, event: HealthEvent) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.apply(event);
    }

    /// Clone the current snapshot. May wait only for an in-flight `apply` (no I/O).
    pub fn snapshot(&self) -> HealthSnapshot {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.snapshot()
    }

    /// Never waits. Returns `None` if a writer currently holds the lock.
    pub fn try_snapshot(&self) -> Option<HealthSnapshot> {
        self.inner.try_read().ok().map(|guard| guard.snapshot())
    }

    #[cfg(test)]
    pub(crate) fn lock_write_for_test(&self) -> std::sync::RwLockWriteGuard<'_, HealthMachine> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests;
