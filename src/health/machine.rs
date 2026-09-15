// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::clock::Clock;
use super::snapshot::{
    CheckpointIdentity, FatalCode, FatalState, HealthPhase, HealthSnapshot, InputFreshness,
    QueuePressure, ReasonCode,
};

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
    /// Replace non-finite, inverted, or equal watermarks with the built-in defaults.
    pub fn sanitized(self) -> Self {
        let high = finite_or(self.overload_high, 0.90);
        let low = finite_or(self.overload_low, 0.70);
        if high > low {
            Self {
                stale_after: self.stale_after,
                overload_high: high,
                overload_low: low,
            }
        } else {
            Self {
                stale_after: self.stale_after,
                overload_high: 0.90,
                overload_low: 0.70,
            }
        }
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
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

/// Pure health state machine. Drive it with [`super::FakeClock`] in tests.
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
            HealthEvent::ProcessStarted
            | HealthEvent::InitializationCompleted
            | HealthEvent::InitializationFailed { .. } => self.apply_boot(event, now),
            HealthEvent::CheckpointValidated { .. } | HealthEvent::CheckpointRejected { .. } => {
                self.apply_checkpoint(event, now)
            }
            HealthEvent::BeginDrain | HealthEvent::Fatal { .. } => self.apply_terminal(event),
            _ => {}
        }
    }

    fn apply_boot(&mut self, event: HealthEvent, now: Instant) {
        match event {
            HealthEvent::ProcessStarted => self.mark_started(now),
            HealthEvent::InitializationCompleted => self.mark_initialized(),
            HealthEvent::InitializationFailed { detail } => {
                self.enter_fatal(FatalCode::InitializationFailed, detail);
            }
            _ => {}
        }
    }

    fn mark_started(&mut self, now: Instant) {
        if !self.started {
            self.started = true;
            self.started_at = Some(now);
        }
    }

    fn mark_initialized(&mut self) {
        if self.started && self.fatal.is_none() && !self.draining {
            self.initialized = true;
        }
    }

    fn apply_checkpoint(&mut self, event: HealthEvent, now: Instant) {
        match event {
            HealthEvent::CheckpointValidated { identity } => {
                if self.can_accept_checkpoint() {
                    self.checkpoint = Some(identity);
                    self.checkpoint_ok = true;
                    self.checkpoint_at = Some(now);
                }
            }
            HealthEvent::CheckpointRejected { detail } => {
                self.enter_fatal(FatalCode::CheckpointInvalid, detail);
            }
            _ => {}
        }
    }

    fn can_accept_checkpoint(&self) -> bool {
        self.started && self.fatal.is_none() && !self.draining && self.initialized
    }

    fn apply_terminal(&mut self, event: HealthEvent) {
        match event {
            HealthEvent::BeginDrain => {
                if self.fatal.is_none() {
                    self.draining = true;
                }
            }
            HealthEvent::Fatal { code, detail } => self.enter_fatal(code, detail),
            _ => {}
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
        let stale = self.compute_stale(now);
        HealthSnapshot {
            live: self.started,
            ready: self.is_ready(),
            phase: self.phase(stale),
            reasons: self.reasons(stale),
            last_successful_tick_ms: self.last_tick.map(ms),
            tick_age_ms: self
                .last_tick
                .map(|t| duration_ms(now.saturating_duration_since(t))),
            checkpoint: self.checkpoint.clone(),
            input_freshness: InputFreshness {
                age_ms: self.input_age_ms(now),
                stale,
            },
            queue_pressure: self.queue_pressure(),
            fatal: self.fatal.clone(),
            observed_at_ms: ms(now),
        }
    }

    fn is_ready(&self) -> bool {
        self.is_initialized() && self.is_serving()
    }

    fn is_initialized(&self) -> bool {
        self.started && self.initialized && self.checkpoint_ok
    }

    fn is_serving(&self) -> bool {
        !self.draining && self.fatal.is_none()
    }

    fn input_age_ms(&self, now: Instant) -> Option<u64> {
        self.last_ingress
            .or(self.checkpoint_at)
            .map(|t| duration_ms(now.saturating_duration_since(t)))
    }

    fn queue_pressure(&self) -> QueuePressure {
        QueuePressure {
            depth: self.queue_depth,
            capacity: self.queue_capacity,
            ratio: self.queue_ratio(),
            overloaded: self.overloaded,
        }
    }

    fn queue_ratio(&self) -> Option<f64> {
        if self.queue_capacity == 0 {
            None
        } else {
            Some(self.queue_depth as f64 / self.queue_capacity as f64)
        }
    }

    fn reasons(&self, stale: bool) -> Vec<ReasonCode> {
        if self.fatal.is_some() {
            return vec![ReasonCode::Fatal];
        }
        let mut reasons = self.boot_reasons();
        self.push_runtime_reasons(&mut reasons, stale);
        reasons
    }

    fn boot_reasons(&self) -> Vec<ReasonCode> {
        if !self.initialized {
            vec![ReasonCode::Starting]
        } else if !self.checkpoint_ok {
            vec![ReasonCode::CheckpointPending]
        } else {
            Vec::default()
        }
    }

    fn push_runtime_reasons(&self, reasons: &mut Vec<ReasonCode>, stale: bool) {
        if self.draining {
            reasons.push(ReasonCode::Draining);
        }
        if stale {
            reasons.push(ReasonCode::StaleInput);
        }
        if self.overloaded {
            reasons.push(ReasonCode::Overload);
        }
    }

    fn phase(&self, stale: bool) -> HealthPhase {
        self.terminal_phase()
            .unwrap_or_else(|| self.operational_phase(stale))
    }

    fn terminal_phase(&self) -> Option<HealthPhase> {
        if self.fatal.is_some() {
            Some(HealthPhase::Fatal)
        } else if self.draining {
            Some(HealthPhase::Draining)
        } else {
            None
        }
    }

    fn operational_phase(&self, stale: bool) -> HealthPhase {
        if !self.initialized {
            HealthPhase::Starting
        } else {
            self.checkpoint_phase(stale)
        }
    }

    fn checkpoint_phase(&self, stale: bool) -> HealthPhase {
        if !self.checkpoint_ok {
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
