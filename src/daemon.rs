// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Brainstem daemon runtime and config-driven service registry.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use neuromod::{NeuroModulators, SpikingNetwork};
use serde::Deserialize;
use tokio::signal;
use tokio::sync::watch;
use tokio::time;
use tracing::{error, info, warn};

use crate::backend::{
    BackendPair, IngressPacket, NEUROMODULATOR_COUNT, SpikeEvent as LocalSpikeEvent, SpikeSink,
    StimulusSource,
};
use crate::checkpoint::{self, ModelProvenance};
use crate::health::{
    CheckpointIdentity, FatalCode, HealthEvent, HealthHandle, HealthLimits, HealthSnapshot,
};
use crate::ingress::{BoundedIngress, IngressConfig, MessageClass, OverflowPolicy};
use crate::logging::validate_log_level;
use crate::registry::{ServiceConfig, ServiceRegistry};

/// Env var read by crates.io `corpus-ipc` 0.1 `ZmqIpcBackend::initialize`.
pub const CORPUS_IPC_READOUT_ENV: &str = "CORPUS_IPC_ZMQ_READOUT_IPC";
/// Legacy name still set by the binary for older tooling; published corpus-ipc ignores it.
pub const LEGACY_SPIKENAUT_READOUT_ENV: &str = "SPIKENAUT_ZMQ_READOUT_IPC";

/// How the daemon obtains its `SpikingNetwork` at startup.
///
/// Live mode is the default and **requires** a validated Spikenaut sidecar
/// checkpoint at `model_path`. Simulation mode is the only way to tick a
/// blank `with_dimensions()` network; it must be requested explicitly.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// Restore and validate a Distill sidecar `snn_model.json` before ticks.
    #[default]
    Live,
    /// Construct a blank `SpikingNetwork::with_dimensions(...)` for tests.
    Simulation,
}

/// Daemon configuration loaded from TOML.
#[derive(Debug, Deserialize, Clone)]
pub struct DaemonConfig {
    pub tick_rate_hz: u32,
    pub log_level: String,
    pub spine_sub_port: u16,
    pub spine_pub_port: u16,
    pub model_path: PathBuf,
    pub lif_count: usize,
    pub izh_count: usize,
    pub channels: usize,
    /// `live` (default) or `simulation`. Omitted keys deserialize as live.
    #[serde(default)]
    pub runtime_mode: RuntimeMode,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    /// Bounded per-class ingress. Omitted TOML keys (and an omitted `[ingress]`
    /// section) keep [`IngressConfig::default`]. Rust struct literals still
    /// need this field; use `IngressConfig::default()` or `..` with a complete
    /// value. Serde defaults do not apply to struct literals.
    #[serde(default)]
    pub ingress: IngressConfig,
    /// Optional `ip:port` for the process control surface (`/livez`, `/readyz`, `/health`, `/metrics`).
    ///
    /// Unset by default so existing configs keep opening no extra sockets. This is the
    /// repository's only HTTP listener; do not add a second server beside it.
    #[serde(default)]
    pub control_bind: Option<String>,
}

impl DaemonConfig {
    /// Load daemon configuration from a TOML file.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        // Reject absolute paths containing parent-dir components (e.g. /etc/../foo)
        // to avoid surprising traversals. Relative .. components are allowed
        // (resolved against the process CWD at load time).
        if path.is_absolute()
            && path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            anyhow::bail!(
                "absolute config path contains parent-dir components: {}",
                path.display()
            );
        }
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let cfg: Self = toml::from_str(&data)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;
        validate_log_level(&cfg.log_level)
            .with_context(|| format!("invalid config from {}", path.display()))?;
        Ok(cfg)
    }
}

/// Observable counters from a bounded tick run (smoke / integration harness).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuntimeStats {
    pub ticks: u64,
    pub accepted_batches: u64,
    pub rejected_batches: u64,
    /// Total backend receive failures, including rate-limited repeats.
    pub receive_errors: u64,
    /// Total spike emission failures, including rate-limited repeats.
    pub emit_errors: u64,
    /// Total spikes discarded because their IDs did not fit the wire type.
    pub dropped_spikes: u64,
    /// Tick diagnostics actually emitted after rate limiting.
    pub diagnostic_emissions: u64,
    /// Recurring diagnostic occurrences suppressed by rate limiting.
    pub suppressed_diagnostics: u64,
    pub last_batch_id: Option<u64>,
    pub last_valid_mask: Option<Vec<bool>>,
    pub loaded_checkpoint: Option<ModelProvenance>,
}

/// Headless spiking-network daemon.
///
/// Owns the tick loop and delegates I/O to pluggable `StimulusSource` + `SpikeSink`.
pub struct BrainstemDaemon {
    config: DaemonConfig,
    registry: ServiceRegistry,
    backend: BackendPair,
    ingress: BoundedIngress,
    health: HealthHandle,
}

impl BrainstemDaemon {
    /// Build a daemon from configuration using the **stub** backend.
    ///
    /// **This always uses the in-memory stub backend**, even if the `corpus-ipc`
    /// feature is enabled at compile time.
    ///
    /// The live ZMQ backend (when the feature is on) is only constructed by the
    /// binary (`src/bin/brainstem_daemon.rs`), which knows the ports and sets the
    /// required environment variables, then passed via [`Self::with_backend`]
    /// or [`Self::try_with_backend`].
    ///
    /// This is intentional for the temporary decoupling (PR A / issues #10-14).
    /// Library users wanting the real backend must construct the pair themselves
    /// under the feature gate and call [`Self::with_backend`] or
    /// [`Self::try_with_backend`]. Prefer the fallible constructors
    /// ([`Self::try_new`], [`Self::try_with_backend`]) for user-provided
    /// configuration to get a clear validation error instead of a panic.
    pub fn new(config: DaemonConfig) -> Self {
        Self::with_backend(config, init_runtime_default())
    }

    /// Fallibly build a daemon from configuration using the **stub** backend.
    pub fn try_new(config: DaemonConfig) -> Result<Self> {
        Self::try_with_backend(config, init_runtime_default())
    }

    /// Build a daemon with an explicit backend pair (for tests and custom backends).
    ///
    /// # Panics
    ///
    /// Panics if `lif_count + izh_count` exceeds [`u16::MAX`]. Prefer
    /// [`Self::try_with_backend`] for user-provided configuration so callers can
    /// return a clear validation error instead of aborting construction.
    pub fn with_backend(config: DaemonConfig, backend: BackendPair) -> Self {
        Self::try_with_backend(config, backend)
            .unwrap_or_else(|err| panic!("failed to build daemon: {err}"))
    }

    /// Fallibly build a daemon with an explicit backend pair (for tests and custom backends).
    pub fn try_with_backend(mut config: DaemonConfig, backend: BackendPair) -> Result<Self> {
        validate_log_level(&config.log_level)?;
        validate_neuron_count(&config)?;

        config.ingress.validate()?;
        let ingress = BoundedIngress::new(config.ingress.clone())?;
        let services = std::mem::take(&mut config.services);
        let registry = ServiceRegistry::from_configs(services);
        Ok(Self {
            config,
            registry,
            backend,
            ingress,
            health: HealthHandle::started(HealthLimits::default()),
        })
    }

    /// Return a reference to the config-driven service registry.
    pub fn registry(&self) -> &ServiceRegistry {
        &self.registry
    }

    /// Restore the runtime network from config (live checkpoint or simulation).
    ///
    /// Live mode fails closed: an unreadable or incompatible checkpoint is never
    /// replaced with a blank `with_dimensions()` network.
    pub fn restore_network(&self) -> Result<(SpikingNetwork, ModelProvenance)> {
        checkpoint::restore_network(&self.config)
    }

    /// Cloneable producer handle for in-process classified ingress.
    pub fn ingress(&self) -> BoundedIngress {
        self.ingress.clone()
    }

    /// Clone the health handle (independent of the tick-loop backend lock).
    pub fn health(&self) -> HealthHandle {
        self.health.clone()
    }

    /// Current health snapshot. Does not wait on ingress or `SpikingNetwork::step`.
    pub fn health_snapshot(&self) -> HealthSnapshot {
        self.health.snapshot()
    }

    /// Run the daemon until a termination signal is received.
    ///
    /// Restores the network from config. The binary should prefer
    /// [`Self::run_with_restored_network`] so the checkpoint is read once,
    /// before sockets open.
    pub async fn run(self) -> Result<()> {
        self.run_loop(None).await
    }

    /// Run with a network already restored by the caller (typically the binary).
    ///
    /// This avoids a second disk read after the pre-socket fail-closed check.
    /// Live mode still verifies that `provenance` is a Spikenaut sidecar schema
    /// and that network dimensions match the daemon config, so a blank or
    /// mismatched pair cannot enter the tick loop.
    pub async fn run_with_restored_network(
        self,
        network: SpikingNetwork,
        provenance: ModelProvenance,
    ) -> Result<()> {
        self.run_loop(Some((network, provenance))).await
    }

    async fn run_loop(self, restored: Option<(SpikingNetwork, ModelProvenance)>) -> Result<()> {
        let cfg = self.config;
        let mut backend = self.backend;
        let ingress = self.ingress;
        let health = self.health;

        if cfg.tick_rate_hz == 0 || cfg.tick_rate_hz > 1_000_000 {
            abort_startup(&ingress, &mut backend, None, None).await;
            anyhow::bail!("tick_rate_hz must be in range 1..=1_000_000");
        }

        let (control_stop, control_task) =
            match start_control(cfg.control_bind.as_deref(), health.clone()).await {
                Ok(pair) => pair,
                Err(err) => {
                    abort_startup(&ingress, &mut backend, None, None).await;
                    return Err(err);
                }
            };

        let (mut network, _) = match boot_network(&mut *backend.source, &cfg, &health, restored) {
            Ok(pair) => pair,
            Err(err) => {
                abort_startup(&ingress, &mut backend, control_stop, control_task).await;
                return Err(err);
            }
        };

        let tick_duration = Duration::from_nanos(1_000_000_000 / u64::from(cfg.tick_rate_hz));
        let mut ticker = time::interval(tick_duration);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut stimuli = vec![0.0; cfg.channels];
        let mut spike_buf: Vec<LocalSpikeEvent> = Vec::with_capacity(128);
        let mut stats = RuntimeStats::default();
        let mut diagnostics = TickDiagnostics::default();
        let mut heartbeat = time::interval_at(
            time::Instant::now() + Duration::from_secs(60),
            Duration::from_secs(60),
        );
        heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut shutdown = std::pin::pin!(shutdown_signal());

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    run_tick(
                        &mut *backend.source,
                        &mut network,
                        &mut *backend.sink,
                        &mut stimuli,
                        &mut spike_buf,
                        &ingress,
                        &mut TickReport {
                            health: &health,
                            stats: &mut stats,
                            diagnostics: &mut diagnostics,
                        },
                    );
                }
                _ = heartbeat.tick() => {
                    info!(
                        ticks = stats.ticks,
                        accepted_batches = stats.accepted_batches,
                        rejected_batches = stats.rejected_batches,
                        receive_errors = stats.receive_errors,
                        emit_errors = stats.emit_errors,
                        dropped_spikes = stats.dropped_spikes,
                        diagnostic_emissions = stats.diagnostic_emissions,
                        suppressed_diagnostics = stats.suppressed_diagnostics,
                        "runtime heartbeat and shed summary"
                    );
                }
                _ = &mut shutdown => {
                    info!("Termination signal received, shutting down");
                    health.apply(HealthEvent::BeginDrain);
                    ingress.shutdown();
                    break;
                }
            }
        }

        stop_control(control_stop, control_task).await;
        shutdown_backend(&mut backend);
        Ok(())
    }

    /// Drive a bounded number of ticks without waiting for a termination signal.
    ///
    /// Used by the CPU-only Thalamic → corpus-ipc → Brainstem smoke harness.
    /// Calls [`StimulusSource::initialize`] (same as [`Self::run`]) so a ZMQ
    /// source is connected once. Source shutdown always runs, even when sink
    /// flush fails. Does not bind `control_bind`.
    pub fn run_for_ticks(self, ticks: u64) -> Result<RuntimeStats> {
        let cfg = self.config;
        let mut backend = self.backend;
        let ingress = self.ingress;
        let health = self.health;
        let mut stats = RuntimeStats::default();

        let (mut network, provenance) =
            match boot_network(&mut *backend.source, &cfg, &health, None) {
                Ok(pair) => pair,
                Err(err) => {
                    ingress.shutdown();
                    shutdown_backend(&mut backend);
                    return Err(err);
                }
            };
        stats.loaded_checkpoint = Some(provenance);
        drive_ticks(
            ticks,
            &mut backend,
            &mut network,
            &ingress,
            &health,
            &mut stats,
            cfg.channels,
        );
        finish_bounded_run(&mut backend, &ingress)?;
        Ok(stats)
    }
}

fn shutdown_backend(backend: &mut BackendPair) {
    // Explicit backend lifecycle hooks (flush sink, shutdown source) are invoked
    // for custom backends. Current built-ins are no-ops, but this satisfies
    // CodeAnt/CodeRabbit "missing cleanup" notes.
    if let Err(e) = backend.sink.flush() {
        warn!("Failed to flush spike sink on shutdown: {e}");
    }
    if let Err(e) = backend.source.shutdown() {
        warn!("Failed to shut down stimulus source: {e}");
    }
}

fn drive_ticks(
    ticks: u64,
    backend: &mut BackendPair,
    network: &mut SpikingNetwork,
    ingress: &BoundedIngress,
    health: &HealthHandle,
    stats: &mut RuntimeStats,
    channels: usize,
) {
    let mut stimuli = vec![0.0; channels];
    let mut spike_buf: Vec<LocalSpikeEvent> = Vec::with_capacity(128);
    let mut diagnostics = TickDiagnostics::default();
    for _ in 0..ticks {
        run_tick(
            &mut *backend.source,
            network,
            &mut *backend.sink,
            &mut stimuli,
            &mut spike_buf,
            ingress,
            &mut TickReport {
                health,
                stats,
                diagnostics: &mut diagnostics,
            },
        );
    }
}

fn finish_bounded_run(backend: &mut BackendPair, ingress: &BoundedIngress) -> Result<()> {
    let flush_err = backend
        .sink
        .flush()
        .context("failed to flush spike sink")
        .err();
    let shutdown_err = backend
        .source
        .shutdown()
        .context("failed to shut down stimulus source")
        .err();
    ingress.shutdown();
    if let Some(err) = flush_err {
        return Err(err);
    }
    if let Some(err) = shutdown_err {
        return Err(err);
    }
    Ok(())
}

/// Wait for a termination request: `SIGINT` (Ctrl-C) on every platform, plus
/// `SIGTERM` (the default `systemctl stop` / `kill` signal) on Unix.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::SignalKind;

    let mut sigterm = match signal::unix::signal(SignalKind::terminate()) {
        Ok(sigterm) => sigterm,
        Err(e) => {
            warn!("Failed to install SIGTERM handler: {e}; only SIGINT will trigger shutdown");
            let _ = signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

/// Wait for a termination request. Windows has no `SIGTERM`; `Ctrl-C` is the
/// only graceful-shutdown signal available there.
#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = signal::ctrl_c().await;
}

/// Internal default backend factory.
///
/// This **always** returns the in-memory stub backend, regardless of Cargo features.
/// The real ZMQ-based backend (when `corpus-ipc` feature is enabled) is constructed
/// explicitly by the binary (`brainstem-daemon`) which knows the spine ports and sets the
/// required environment variable(s), then injected via `BrainstemDaemon::with_backend`.
///
/// Library callers that want the live ZMQ backend must do the same: build the pair
/// themselves (under `#[cfg(feature = "corpus-ipc")]`) and call `with_backend`.
///
/// NOTE: Intentionally always stub for PR A (decoupling). Codacy "MEDIUM RISK" is
/// acknowledged; the contract is documented and the binary is the only path that
/// wires a real backend. This is the intended temporary state.
fn init_runtime_default() -> BackendPair {
    BackendPair::stub()
}

async fn abort_startup(
    ingress: &BoundedIngress,
    backend: &mut BackendPair,
    control_stop: Option<watch::Sender<bool>>,
    control_task: Option<tokio::task::JoinHandle<()>>,
) {
    ingress.shutdown();
    stop_control(control_stop, control_task).await;
    shutdown_backend(backend);
}

async fn start_control(
    bind: Option<&str>,
    health: HealthHandle,
) -> Result<(
    Option<watch::Sender<bool>>,
    Option<tokio::task::JoinHandle<()>>,
)> {
    let Some(bind) = bind else {
        return Ok((None, None));
    };
    let listener = bind_control(bind).await?;
    Ok(spawn_control_task(listener, health))
}

async fn bind_control(bind: &str) -> Result<tokio::net::TcpListener> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid control_bind {bind}"))?;
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind control surface on {addr}"))
}

fn spawn_control_task(
    listener: tokio::net::TcpListener,
    health: HealthHandle,
) -> (
    Option<watch::Sender<bool>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        if let Err(e) = crate::control::serve_listener(listener, health, rx).await {
            warn!("control surface stopped: {e}");
        }
    });
    (Some(tx), Some(task))
}

async fn stop_control(
    stop: Option<watch::Sender<bool>>,
    task: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(tx) = stop {
        let _ = tx.send(true);
    }
    if let Some(task) = task {
        let _ = task.await;
    }
}

fn initialize_source(
    source: &mut dyn StimulusSource,
    cfg: &DaemonConfig,
    health: &HealthHandle,
) -> Result<()> {
    let model_path = cfg.model_path.to_string_lossy();
    if let Err(e) = source.initialize(Some(model_path.as_ref())) {
        health.apply(HealthEvent::InitializationFailed {
            detail: e.to_string(),
        });
        error!("Stimulus source initialization failed: {e}");
        return Err(e).context("failed to initialize stimulus source");
    }
    health.apply(HealthEvent::InitializationCompleted);
    Ok(())
}

fn boot_network(
    source: &mut dyn StimulusSource,
    cfg: &DaemonConfig,
    health: &HealthHandle,
    restored: Option<(SpikingNetwork, ModelProvenance)>,
) -> Result<(SpikingNetwork, ModelProvenance)> {
    initialize_source(source, cfg, health)?;
    let (network, provenance) = match take_restored_network(cfg, restored) {
        Ok(pair) => pair,
        Err(err) => {
            health.apply(HealthEvent::CheckpointRejected {
                detail: err.to_string(),
            });
            return Err(err);
        }
    };
    health.apply(HealthEvent::CheckpointValidated {
        identity: checkpoint_identity(&provenance),
    });
    log_model_provenance(cfg, &provenance);
    Ok((network, provenance))
}

fn checkpoint_identity(provenance: &ModelProvenance) -> CheckpointIdentity {
    let digest = if provenance.content_sha256 == "none" {
        None
    } else {
        Some(provenance.content_sha256.clone())
    };
    CheckpointIdentity {
        id: provenance.model_id.clone(),
        digest,
    }
}

fn packet_carries_input(packet: &IngressPacket) -> bool {
    !packet.stimuli.is_empty()
        || packet
            .modulators
            .as_ref()
            .is_some_and(|mods| !mods.is_empty())
}

fn apply_queue_pressure(health: &HealthHandle, ingress: &BoundedIngress) {
    let metrics = ingress.metrics();
    let cfg = ingress.config();
    let mut depth = 0u64;
    let mut capacity = 0u64;
    for class in MessageClass::ALL {
        depth += u64::try_from(metrics.class(class).depth).unwrap_or(u64::MAX);
        let cap = match cfg.policy(class) {
            OverflowPolicy::Coalesce => 1,
            _ => cfg.capacity(class),
        };
        capacity += u64::try_from(cap).unwrap_or(u64::MAX);
    }
    health.apply(HealthEvent::QueuePressure { depth, capacity });
}

fn log_model_provenance(config: &DaemonConfig, provenance: &ModelProvenance) {
    match config.runtime_mode {
        RuntimeMode::Simulation => {
            warn!(
                schema_id = %provenance.schema_id,
                model_id = %provenance.model_id,
                lif_count = config.lif_count,
                izh_count = config.izh_count,
                channels = config.channels,
                "Simulation mode: blank with_dimensions() network; not a loaded Spikenaut checkpoint"
            );
        }
        RuntimeMode::Live => {
            info!(
                schema_id = %provenance.schema_id,
                model_id = %provenance.model_id,
                path = %provenance.source_path.display(),
                sha256 = %provenance.content_sha256,
                encoder = provenance.encoder.as_deref().unwrap_or("-"),
                source = provenance.source.as_deref().unwrap_or("-"),
                lineage = provenance.frozen_lineage.as_deref().unwrap_or("-"),
                lif_count = config.lif_count,
                izh_count = config.izh_count,
                channels = config.channels,
                "Loaded Spikenaut checkpoint; entering tick loop"
            );
        }
    }
}

pub(crate) fn validate_neuron_count(config: &DaemonConfig) -> Result<()> {
    let total = config
        .lif_count
        .checked_add(config.izh_count)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "lif_count + izh_count ({} + {}) overflows usize",
                config.lif_count,
                config.izh_count
            )
        })?;

    if total > u16::MAX as usize {
        bail!(
            "lif_count + izh_count ({} + {}) exceeds u16::MAX ({})",
            config.lif_count,
            config.izh_count,
            u16::MAX
        );
    }

    Ok(())
}

fn take_restored_network(
    cfg: &DaemonConfig,
    restored: Option<(SpikingNetwork, ModelProvenance)>,
) -> Result<(SpikingNetwork, ModelProvenance)> {
    let (network, provenance) = match restored {
        Some(pair) => pair,
        None => checkpoint::restore_network(cfg)
            .context("failed to restore runtime network before entering the tick loop")?,
    };
    validate_restored_pair(cfg, &network, &provenance)?;
    Ok((network, provenance))
}

fn validate_restored_pair(
    cfg: &DaemonConfig,
    network: &SpikingNetwork,
    provenance: &ModelProvenance,
) -> Result<()> {
    match cfg.runtime_mode {
        RuntimeMode::Live => {
            if provenance.schema_id != checkpoint::SCHEMA_ID {
                bail!(
                    "live mode requires a validated Spikenaut checkpoint (schema {}), got {}",
                    checkpoint::SCHEMA_ID,
                    provenance.schema_id
                );
            }
        }
        RuntimeMode::Simulation => {
            if provenance.schema_id != checkpoint::SIMULATION_SCHEMA_ID {
                bail!(
                    "simulation mode requires schema {}, got {}",
                    checkpoint::SIMULATION_SCHEMA_ID,
                    provenance.schema_id
                );
            }
        }
    }
    if network.neurons.len() != cfg.lif_count
        || network.iz_neurons.len() != cfg.izh_count
        || network.num_channels != cfg.channels
    {
        bail!(
            "restored network dimensions ({}/{}/{}) do not match config lif_count/izh_count/channels ({}/{}/{})",
            network.neurons.len(),
            network.iz_neurons.len(),
            network.num_channels,
            cfg.lif_count,
            cfg.izh_count,
            cfg.channels
        );
    }
    Ok(())
}

// Trait-based tick loop (works with or without corpus-ipc feature)

/// Health reporter plus smoke counters updated together on every tick.
struct TickReport<'a> {
    health: &'a HealthHandle,
    stats: &'a mut RuntimeStats,
    diagnostics: &'a mut TickDiagnostics,
}

const DIAGNOSTIC_INTERVAL: u64 = 1_000;
/// Floor for diagnostic re-emission so high tick rates cannot flood logs.
const DIAGNOSTIC_MIN_GAP: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct OccurrenceLimiter {
    interval: u64,
    min_gap: Duration,
    occurrences: u64,
    emitted: u64,
    last_key: Option<u64>,
    last_emitted_at: Option<time::Instant>,
}

impl OccurrenceLimiter {
    fn new(interval: u64) -> Self {
        Self {
            interval,
            // Unit tests construct via TickDiagnostics::new(interval) and need
            // pure count-based emission; production Default applies DIAGNOSTIC_MIN_GAP.
            min_gap: Duration::ZERO,
            occurrences: 0,
            emitted: 0,
            last_key: None,
            last_emitted_at: None,
        }
    }

    /// Return the number suppressed since the preceding emission.
    ///
    /// `key` identifies the diagnostic payload (e.g. hashed error text). A key
    /// change resets suppression so a new failure is not hidden behind the
    /// previous one's interval. Re-emission also requires `min_gap` so a high
    /// `tick_rate_hz` cannot turn the occurrence interval into a log flood.
    fn record(&mut self, key: u64, now: time::Instant) -> Option<u64> {
        if self.last_key != Some(key) {
            self.occurrences = 0;
            self.emitted = 0;
            self.last_key = Some(key);
            self.last_emitted_at = None;
        }
        self.occurrences = self.occurrences.saturating_add(1);
        let count_due =
            self.occurrences == 1 || (self.occurrences - 1).is_multiple_of(self.interval);
        let time_due = match self.last_emitted_at {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= self.min_gap,
        };
        if self.occurrences == 1 || (count_due && time_due) {
            let suppressed = self.occurrences.saturating_sub(self.emitted + 1);
            self.emitted = self.occurrences;
            self.last_emitted_at = Some(now);
            Some(suppressed)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn suppressed(&self) -> u64 {
        self.occurrences.saturating_sub(self.emitted)
    }
}

fn diagnostic_key(message: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    message.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug)]
struct TickDiagnostics {
    receive: OccurrenceLimiter,
    emit: OccurrenceLimiter,
    dropped: OccurrenceLimiter,
}

impl TickDiagnostics {
    fn new(interval: u64) -> Self {
        Self {
            receive: OccurrenceLimiter::new(interval),
            emit: OccurrenceLimiter::new(interval),
            dropped: OccurrenceLimiter::new(interval),
        }
    }

    #[cfg(test)]
    fn suppressed_total(&self) -> u64 {
        self.receive.suppressed() + self.emit.suppressed() + self.dropped.suppressed()
    }
}

impl Default for TickDiagnostics {
    fn default() -> Self {
        // Inline construction (avoid `Self::new` in `Default` for DeepSource RS-W1090).
        let mut receive = OccurrenceLimiter::new(DIAGNOSTIC_INTERVAL);
        let mut emit = OccurrenceLimiter::new(DIAGNOSTIC_INTERVAL);
        let mut dropped = OccurrenceLimiter::new(DIAGNOSTIC_INTERVAL);
        receive.min_gap = DIAGNOSTIC_MIN_GAP;
        emit.min_gap = DIAGNOSTIC_MIN_GAP;
        dropped.min_gap = DIAGNOSTIC_MIN_GAP;
        Self {
            receive,
            emit,
            dropped,
        }
    }
}

fn run_tick(
    source: &mut dyn StimulusSource,
    network: &mut SpikingNetwork,
    sink: &mut dyn SpikeSink,
    stimuli: &mut [f32],
    spike_buf: &mut Vec<LocalSpikeEvent>,
    ingress: &BoundedIngress,
    report: &mut TickReport<'_>,
) {
    let TickReport {
        health,
        stats,
        diagnostics,
    } = report;
    let backend_packet = match source.next_ingress() {
        Ok(Some(p)) => Some(p),
        Ok(None) => None,
        Err(e) => {
            stats.receive_errors = stats.receive_errors.saturating_add(1);
            stats.rejected_batches += 1;
            let msg = format!("{e}");
            let now = time::Instant::now();
            match diagnostics.receive.record(diagnostic_key(&msg), now) {
                Some(suppressed) => {
                    stats.diagnostic_emissions = stats.diagnostic_emissions.saturating_add(1);
                    warn!(
                        total = stats.receive_errors,
                        suppressed, "Failed to receive from stimulus source: {msg}"
                    );
                }
                None => {
                    stats.suppressed_diagnostics = stats.suppressed_diagnostics.saturating_add(1);
                }
            }
            None
        }
    };

    if backend_packet.as_ref().is_some_and(packet_carries_input) {
        health.apply(HealthEvent::IngressObserved);
    }

    // Admit through bounded class queues so a bursty backend cannot grow
    // unbounded in-process, then drain control-first for this tick.
    // `None` (skip or error) does not enqueue a placeholder that could evict
    // in-process sensory. decode_inputs zero-fills when drain yields no stimuli.
    if let Some(packet) = backend_packet {
        if packet.rejected {
            stats.rejected_batches += 1;
        } else if packet.batch_id.is_some() {
            stats.accepted_batches += 1;
            stats.last_batch_id = packet.batch_id;
            stats.last_valid_mask.clone_from(&packet.valid_mask);
        }
        ingress.admit_backend_packet(packet);
    }
    let drained = ingress.drain_for_tick();
    observe_control_envelopes(&drained.control);
    let packet = drained.into_packet();
    apply_queue_pressure(health, ingress);

    let modulators = decode_inputs(&packet, stimuli);

    // Note: decode_inputs already zero-fills any remaining channels when packet.stimuli is shorter.

    // `step` is the thread-local RNG wrapper around 0.6 `step_with_rng`.
    // The generator is not stored on the network and is not checkpointed.
    let spike_ids = match network.step(stimuli, &modulators) {
        Ok(spikes) => spikes,
        Err(e) => {
            error!("Network step failed: {e}");
            health.apply(HealthEvent::Fatal {
                code: FatalCode::Unspecified,
                detail: e.to_string(),
            });
            return;
        }
    };
    stats.ticks += 1;

    // Single timestamp for both per-spike time and batch metadata (keeps them consistent).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let tick = now.as_millis() as u64;

    spike_buf.clear();
    let mut dropped = 0usize;
    for &idx in &spike_ids {
        match u16::try_from(idx) {
            Ok(channel) => {
                spike_buf.push(LocalSpikeEvent {
                    channel,
                    time: (tick & (u32::MAX as u64)) as u32,
                    strength: 1.0,
                });
            }
            Err(_) => {
                dropped += 1;
            }
        }
    }
    if dropped > 0 {
        stats.dropped_spikes = stats.dropped_spikes.saturating_add(dropped as u64);
        let msg = "dropped spikes with out-of-range IDs";
        let now = time::Instant::now();
        match diagnostics.dropped.record(diagnostic_key(msg), now) {
            Some(suppressed) => {
                stats.diagnostic_emissions = stats.diagnostic_emissions.saturating_add(1);
                warn!(
                    dropped_this_tick = dropped,
                    total_dropped_spikes = stats.dropped_spikes,
                    suppressed,
                    "{msg} (network may be larger than u16)"
                );
            }
            None => {
                stats.suppressed_diagnostics = stats.suppressed_diagnostics.saturating_add(1);
            }
        }
    }

    if spike_buf.is_empty() && !spike_ids.is_empty() {
        // Had spikes from network but all IDs were out of u16 range (dropped).
        // Nothing valid to publish; skip to avoid empty batch for dropped case.
        health.apply(HealthEvent::TickSucceeded);
        return;
    }

    // Emit the batch for this tick.
    // - May be empty if no neurons fired this tick (original behavior for some
    //   downstream consumers that expect a message per tick).
    // - We deliberately do not suppress empty batches here to keep test
    //   expectations (CollectingSpikeSink) and wire behavior stable.
    if let Err(e) = sink.emit(spike_buf, now) {
        stats.emit_errors = stats.emit_errors.saturating_add(1);
        let msg = format!("{e}");
        let now_diag = time::Instant::now();
        match diagnostics.emit.record(diagnostic_key(&msg), now_diag) {
            Some(suppressed) => {
                stats.diagnostic_emissions = stats.diagnostic_emissions.saturating_add(1);
                error!(
                    total = stats.emit_errors,
                    suppressed, "Failed to emit spikes: {msg}"
                );
            }
            None => {
                stats.suppressed_diagnostics = stats.suppressed_diagnostics.saturating_add(1);
            }
        }
        health.apply(HealthEvent::Fatal {
            code: FatalCode::Unspecified,
            detail: e.to_string(),
        });
        return;
    }
    health.apply(HealthEvent::TickSucceeded);
}

/// Observe drained in-band control/safety envelopes.
///
/// LIM-1216 bounds and prioritizes this class so it cannot starve behind bulk
/// telemetry. There is no network control actuator in this crate; OS
/// `SIGINT`/`SIGTERM` remain the live shutdown path. Callers that inject
/// control packets can inspect `DrainedTick.control` via `drain_for_tick`.
fn observe_control_envelopes(packets: &[IngressPacket]) {
    if packets.is_empty() {
        return;
    }
    info!(
        count = packets.len(),
        "observed in-band control envelopes (no network actuator; OS signals remain shutdown)"
    );
}

/// decode_inputs now takes an IngressPacket.
/// When packet.modulators is None (the common stub path in PR A), we return defaults.
/// This mirrors the previous "short readout" fallback behavior.
fn decode_inputs(packet: &IngressPacket, stimuli: &mut [f32]) -> NeuroModulators {
    let readout = &packet.stimuli;
    let channels = stimuli.len();
    let upto = readout.len().min(channels);
    stimuli[..upto].copy_from_slice(&readout[..upto]);
    if readout.len() < channels {
        stimuli[upto..].fill(0.0);
    }
    if let Some(mask) = packet.valid_mask.as_ref() {
        for (idx, valid) in mask.iter().enumerate().take(channels) {
            if !*valid {
                stimuli[idx] = 0.0;
            }
        }
    }

    match packet.modulators.as_ref() {
        Some(mods) if mods.len() >= NEUROMODULATOR_COUNT => {
            return NeuroModulators {
                dopamine: mods[0],
                serotonin: mods[1],
                acetylcholine: mods[2],
                norepinephrine: mods[3],
            };
        }
        _ => {}
    }

    // No modulators provided (or short) → defaults.
    // Comment: this is the hot path for stub backends in the temporary decoupling.
    NeuroModulators::default()
}

// Test hook so we can drive the tick logic from unit tests without making run_tick public.
#[cfg(test)]
pub(crate) fn run_tick_for_test(
    source: &mut dyn StimulusSource,
    network: &mut SpikingNetwork,
    sink: &mut dyn SpikeSink,
    stimuli: &mut [f32],
    spike_buf: &mut Vec<LocalSpikeEvent>,
) {
    let ingress = BoundedIngress::new(IngressConfig::default()).expect("default ingress");
    let health = HealthHandle::started(HealthLimits::default());
    run_tick(
        source,
        network,
        sink,
        stimuli,
        spike_buf,
        &ingress,
        &mut TickReport {
            health: &health,
            stats: &mut RuntimeStats::default(),
            diagnostics: &mut TickDiagnostics::default(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CollectingSpikeSink;
    use crate::registry::ServiceConfig;

    fn sample_config() -> DaemonConfig {
        DaemonConfig {
            tick_rate_hz: 1000,
            log_level: "info".to_string(),
            spine_sub_port: 5555,
            spine_pub_port: 5556,
            model_path: PathBuf::from("/tmp/model.mem"),
            lif_count: 16,
            izh_count: 5,
            channels: 16,
            runtime_mode: RuntimeMode::Simulation,
            services: vec![
                ServiceConfig::named("telemetry"),
                ServiceConfig::named("critic-ipc"),
            ],
            ingress: IngressConfig::default(),
            control_bind: None,
        }
    }

    fn tagged_packet(tag: f32) -> IngressPacket {
        IngressPacket {
            stimuli: vec![tag],
            modulators: None,
            ..Default::default()
        }
    }

    fn write_config_toml(stem: &str, body: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("daemon-test-toml");
        std::fs::create_dir_all(&dir).expect("create test-toml dir");
        let path = dir.join(format!(
            "{stem}-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, body).expect("write test toml");
        path
    }

    #[test]
    fn daemon_builds_registry_from_config() {
        let daemon = BrainstemDaemon::new(sample_config());
        assert_eq!(daemon.registry().len(), 2);
        assert!(daemon.registry().contains("telemetry"));
        assert!(daemon.registry().contains("critic-ipc"));
    }

    #[test]
    fn daemon_is_live_not_ready_before_run() {
        let daemon = BrainstemDaemon::new(sample_config());
        let snap = daemon.health_snapshot();
        assert!(snap.live);
        assert!(!snap.ready);
        assert_eq!(snap.phase, crate::health::HealthPhase::Starting);
        assert!(!snap.reasons.is_empty());
    }

    #[test]
    fn config_parses_optional_control_bind() {
        let cfg: DaemonConfig = toml::from_str(
            r#"
tick_rate_hz = 1000
log_level = "info"
spine_sub_port = 5555
spine_pub_port = 5556
model_path = "/tmp/model.mem"
lif_count = 1
izh_count = 0
channels = 1
control_bind = "127.0.0.1:9464"
"#,
        )
        .expect("toml");
        assert_eq!(cfg.control_bind.as_deref(), Some("127.0.0.1:9464"));
    }

    #[test]
    fn config_load_rejects_unknown_log_level() {
        let path = write_config_toml(
            "invalid-log-level",
            r#"
tick_rate_hz = 1000
log_level = "verbose"
spine_sub_port = 5555
spine_pub_port = 5556
model_path = "/tmp/model.mem"
lif_count = 1
izh_count = 0
channels = 1
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("invalid log_level \"verbose\""),
            "{message}"
        );
        assert!(
            message.contains("error, warn, info, debug, trace"),
            "{message}"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn daemon_ignores_disabled_services() {
        let mut cfg = sample_config();
        cfg.services.push(ServiceConfig {
            name: "mining-adapter".to_string(),
            enabled: false,
        });
        let daemon = BrainstemDaemon::new(cfg);
        assert!(!daemon.registry().contains("mining-adapter"));
    }

    #[test]
    fn decode_inputs_fills_stimuli() {
        let packet = IngressPacket {
            stimuli: vec![0.1, 0.2, 0.3, 0.4],
            modulators: None,
            ..Default::default()
        };
        let mut stimuli = vec![0.0; 4];
        let _mods = decode_inputs(&packet, &mut stimuli);
        assert_eq!(stimuli, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn decode_inputs_takes_modulators_when_present() {
        let packet = IngressPacket {
            stimuli: vec![0.0; 4],
            modulators: Some(vec![0.5, 0.6, 0.7, 0.8]),
            ..Default::default()
        };
        let mut stimuli = vec![0.0; 4];
        let mods = decode_inputs(&packet, &mut stimuli);
        assert_eq!(mods.dopamine, 0.5);
        assert_eq!(mods.serotonin, 0.6);
        assert_eq!(mods.acetylcholine, 0.7);
        assert_eq!(mods.norepinephrine, 0.8);
    }

    #[test]
    fn decode_inputs_ignores_extra_modulator_tail() {
        let packet = IngressPacket {
            stimuli: vec![0.0; 2],
            modulators: Some(vec![0.1, 0.2, 0.3, 0.4, 0.9]),
            ..Default::default()
        };
        let mut stimuli = vec![0.0; 2];
        let mods = decode_inputs(&packet, &mut stimuli);
        assert_eq!(
            mods,
            NeuroModulators {
                dopamine: 0.1,
                serotonin: 0.2,
                acetylcholine: 0.3,
                norepinephrine: 0.4,
            }
        );
    }

    #[test]
    fn decode_inputs_defaults_modulators_when_short() {
        let packet = IngressPacket {
            stimuli: vec![0.1, 0.2],
            modulators: None,
            ..Default::default()
        };
        let mut stimuli = vec![0.0; 4];
        let mods = decode_inputs(&packet, &mut stimuli);
        assert_eq!(stimuli, vec![0.1, 0.2, 0.0, 0.0]);
        assert_eq!(mods, NeuroModulators::default());
    }

    #[test]
    fn decode_inputs_zeros_invalid_channels() {
        let packet = IngressPacket {
            stimuli: vec![1.0, 0.5, 0.25, 0.1],
            modulators: None,
            valid_mask: Some(vec![true, false, true, true]),
            batch_id: Some(1),
            timestamp_ns: Some(1),
            ..Default::default()
        };
        let mut stimuli = vec![0.0; 4];
        let _mods = decode_inputs(&packet, &mut stimuli);
        assert_eq!(stimuli, vec![1.0, 0.0, 0.25, 0.1]);
    }

    #[test]
    fn daemon_allows_u16_max_total_neurons() {
        let mut cfg = sample_config();
        cfg.lif_count = u16::MAX as usize;
        cfg.izh_count = 0;

        let daemon = BrainstemDaemon::try_with_backend(cfg, BackendPair::stub());

        assert!(daemon.is_ok());
    }

    #[test]
    fn daemon_rejects_total_neurons_above_u16_max() {
        let mut cfg = sample_config();
        cfg.lif_count = u16::MAX as usize;
        cfg.izh_count = 1;

        let err = match BrainstemDaemon::try_with_backend(cfg, BackendPair::stub()) {
            Ok(_) => panic!("expected invalid neuron count to fail"),
            Err(err) => err,
        };
        let message = err.to_string();

        assert!(
            message.contains("lif_count + izh_count") && message.contains("exceeds u16::MAX"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn daemon_rejects_neuron_count_usize_overflow() {
        let mut cfg = sample_config();
        cfg.lif_count = usize::MAX;
        cfg.izh_count = 1;

        let err = match BrainstemDaemon::try_with_backend(cfg, BackendPair::stub()) {
            Ok(_) => panic!("expected usize overflow to fail"),
            Err(err) => err,
        };
        let message = err.to_string();

        assert!(
            message.contains("overflows usize"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn omitted_runtime_mode_deserializes_as_live() {
        let text = r#"
tick_rate_hz = 1000
log_level = "info"
spine_sub_port = 5555
spine_pub_port = 5556
model_path = "snn_model.json"
lif_count = 16
izh_count = 0
channels = 16
"#;
        let cfg: DaemonConfig = toml::from_str(text).expect("toml");
        assert_eq!(cfg.runtime_mode, RuntimeMode::Live);
    }

    #[test]
    fn live_restore_fails_closed_without_replacing_with_blank_network() {
        let mut cfg = sample_config();
        cfg.runtime_mode = RuntimeMode::Live;
        cfg.izh_count = 0;
        cfg.model_path = PathBuf::from("/no/such/snn_model.json");
        let daemon = BrainstemDaemon::try_new(cfg).expect("construction does not load weights");
        let err = match daemon.restore_network() {
            Ok(_) => panic!("live mode must fail closed"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("checkpoint not found"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn run_fails_before_tick_loop_when_live_checkpoint_is_missing() {
        let mut cfg = sample_config();
        cfg.runtime_mode = RuntimeMode::Live;
        cfg.izh_count = 0;
        cfg.model_path = PathBuf::from("/no/such/snn_model.json");
        let daemon = BrainstemDaemon::try_new(cfg).unwrap();
        let ingress = daemon.ingress();
        let result = tokio::time::timeout(Duration::from_millis(500), daemon.run()).await;
        let inner = result.expect("run must return immediately rather than tick");
        assert!(inner.is_err());
        assert!(
            ingress.is_shutdown(),
            "startup failure must close ingress so blocked producers do not wait out block_timeout_ms"
        );
    }

    #[tokio::test]
    async fn run_shuts_down_ingress_when_tick_rate_invalid() {
        let mut cfg = sample_config();
        cfg.tick_rate_hz = 0;
        let daemon = BrainstemDaemon::try_new(cfg).unwrap();
        let ingress = daemon.ingress();
        let result = tokio::time::timeout(Duration::from_millis(500), daemon.run()).await;
        let inner = result.expect("run must return immediately rather than tick");
        assert!(inner.is_err());
        assert!(ingress.is_shutdown());
    }

    #[tokio::test]
    async fn run_unblocks_waiting_producer_when_startup_fails() {
        use crate::ingress::{EnqueueOutcome, MessageClass, OverflowPolicy};
        use std::sync::mpsc;
        use std::thread;

        let mut cfg = sample_config();
        cfg.runtime_mode = RuntimeMode::Live;
        cfg.izh_count = 0;
        cfg.model_path = PathBuf::from("/no/such/snn_model.json");
        cfg.ingress.control_policy = OverflowPolicy::BlockTimeout;
        cfg.ingress.control_capacity = 1;
        cfg.ingress.block_timeout_ms = 60_000;
        let daemon = BrainstemDaemon::try_new(cfg).unwrap();
        let ingress = daemon.ingress();
        assert!(
            ingress
                .enqueue(MessageClass::Control, tagged_packet(1.0))
                .accepted()
        );

        let producer = ingress.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(producer.enqueue(MessageClass::Control, tagged_packet(2.0)));
        });

        let started = std::time::Instant::now();
        while ingress.metrics().control.producer_waits == 0 {
            if started.elapsed() > Duration::from_secs(2) {
                panic!("producer never entered block_timeout wait");
            }
            thread::yield_now();
        }

        let inner = tokio::time::timeout(Duration::from_millis(500), daemon.run())
            .await
            .expect("run must return immediately rather than tick");
        assert!(inner.is_err());
        let outcome = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("producer hung after failed startup");
        assert_eq!(outcome, EnqueueOutcome::Shutdown);
        assert!(ingress.is_shutdown());
    }

    #[test]
    fn live_run_rejects_simulation_provenance() {
        let mut cfg = sample_config();
        cfg.runtime_mode = RuntimeMode::Live;
        cfg.izh_count = 0;
        let network = SpikingNetwork::with_dimensions(cfg.lif_count, cfg.izh_count, cfg.channels);
        let provenance = ModelProvenance {
            schema_id: crate::checkpoint::SIMULATION_SCHEMA_ID.to_string(),
            source_path: PathBuf::from("<simulation>"),
            content_sha256: "none".to_string(),
            model_id: "simulation/blank".to_string(),
            encoder: None,
            source: None,
            frozen_lineage: None,
        };
        let err = match take_restored_network(&cfg, Some((network, provenance))) {
            Ok(_) => panic!("live mode must reject a blank simulation pair"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("validated Spikenaut checkpoint"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn live_run_rejects_dimension_mismatched_restored_network() {
        let mut cfg = sample_config();
        cfg.runtime_mode = RuntimeMode::Live;
        cfg.izh_count = 0;
        let network = SpikingNetwork::with_dimensions(1, 0, 1);
        let provenance = ModelProvenance {
            schema_id: crate::checkpoint::SCHEMA_ID.to_string(),
            source_path: PathBuf::from("snn_model.json"),
            content_sha256: "abcd".to_string(),
            model_id: "spikenaut-snn:test".to_string(),
            encoder: None,
            source: None,
            frozen_lineage: None,
        };
        let err = match take_restored_network(&cfg, Some((network, provenance))) {
            Ok(_) => panic!("live mode must reject mismatched dimensions"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("dimensions"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn stub_backend_basic_tick() {
        let mut source = crate::backend::StubStimulusSource;
        let mut sink = CollectingSpikeSink::new();
        let mut network = SpikingNetwork::with_dimensions(2, 0, 2);
        let mut stimuli = vec![0.0; 2];
        let mut spike_buf: Vec<crate::backend::SpikeEvent> = Vec::new();
        let ingress = BoundedIngress::new(IngressConfig::default()).expect("default ingress");
        let health = HealthHandle::started(HealthLimits::default());

        run_tick(
            &mut source,
            &mut network,
            &mut sink,
            &mut stimuli,
            &mut spike_buf,
            &ingress,
            &mut TickReport {
                health: &health,
                stats: &mut RuntimeStats::default(),
                diagnostics: &mut TickDiagnostics::default(),
            },
        );

        assert_eq!(sink.emitted.len(), 1);
        let snap = health.snapshot();
        assert!(snap.live);
        assert!(
            snap.last_successful_tick_ms.is_some(),
            "a successful network step must update last_successful_tick"
        );
        assert!(
            !snap.input_freshness.stale,
            "stale only applies after checkpoint validation"
        );
        assert!(
            snap.input_freshness.age_ms.is_none(),
            "empty stub packets must not count as ingress"
        );
    }

    #[test]
    fn repeated_receive_errors_are_bounded_and_fully_counted() {
        struct FailingSource;
        impl StimulusSource for FailingSource {
            fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
                bail!("synthetic receive failure")
            }

            fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
                Ok(())
            }
        }

        let ingress = BoundedIngress::new(IngressConfig::default()).unwrap();
        let health = HealthHandle::started(HealthLimits::default());
        let mut source = FailingSource;
        let mut sink = CollectingSpikeSink::new();
        let mut network = SpikingNetwork::with_dimensions(1, 0, 1);
        let mut stimuli = [0.0];
        let mut spike_buf = Vec::new();
        let mut stats = RuntimeStats::default();
        let mut diagnostics = TickDiagnostics::new(10);

        for _ in 0..25 {
            run_tick(
                &mut source,
                &mut network,
                &mut sink,
                &mut stimuli,
                &mut spike_buf,
                &ingress,
                &mut TickReport {
                    health: &health,
                    stats: &mut stats,
                    diagnostics: &mut diagnostics,
                },
            );
        }

        assert_eq!(stats.receive_errors, 25);
        assert_eq!(stats.rejected_batches, 25);
        assert_eq!(stats.diagnostic_emissions, 3);
        assert_eq!(stats.suppressed_diagnostics, 22);
        assert_eq!(diagnostics.suppressed_total(), 4);
    }

    struct ScriptedStimulusSource {
        packet: IngressPacket,
    }

    impl StimulusSource for ScriptedStimulusSource {
        fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
            Ok(Some(self.packet.clone()))
        }

        fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
            Ok(())
        }
    }

    fn tick_once(packet: IngressPacket, network: &mut SpikingNetwork) -> CollectingSpikeSink {
        let mut source = ScriptedStimulusSource { packet };
        let mut sink = CollectingSpikeSink::new();
        let mut stimuli = vec![0.0; network.num_channels];
        let mut spike_buf: Vec<crate::backend::SpikeEvent> = Vec::new();
        run_tick_for_test(
            &mut source,
            network,
            &mut sink,
            &mut stimuli,
            &mut spike_buf,
        );
        sink
    }

    #[test]
    fn tick_applies_neuromod_06_modulator_snapshot() {
        let mut network = SpikingNetwork::with_dimensions(2, 1, 2);
        let packet = IngressPacket {
            stimuli: vec![0.0; 2],
            modulators: Some(vec![0.5, 0.25, 0.8, 0.1]),
            ..Default::default()
        };

        let sink = tick_once(packet, &mut network);

        assert_eq!(sink.emitted.len(), 1);
        assert_eq!(network.global_step, 1);
        assert_eq!(
            network.modulators,
            NeuroModulators {
                dopamine: 0.5,
                serotonin: 0.25,
                acetylcholine: 0.8,
                norepinephrine: 0.1,
            }
        );
        // Engine assigns LIF decay from acetylcholine: 0.15 - 0.05 * ACh.
        assert!(
            network
                .neurons
                .iter()
                .all(|n| (n.decay_rate - 0.11).abs() < 1e-6)
        );
    }

    #[test]
    fn tick_defaults_modulators_when_ingress_omits_them() {
        let mut network = SpikingNetwork::with_dimensions(2, 0, 2);
        let packet = IngressPacket {
            stimuli: vec![0.2, 0.3],
            modulators: None,
            ..Default::default()
        };

        let sink = tick_once(packet, &mut network);

        assert_eq!(sink.emitted.len(), 1);
        assert_eq!(network.modulators, NeuroModulators::default());
        assert!(
            network
                .neurons
                .iter()
                .all(|n| (n.decay_rate - 0.15).abs() < 1e-6)
        );
    }

    #[test]
    fn tick_loop_emits_one_batch_per_step() {
        let mut network = SpikingNetwork::with_dimensions(2, 0, 2);
        let packet = IngressPacket {
            stimuli: vec![0.4, 0.1],
            modulators: Some(vec![0.0, 0.0, 0.0, 0.0]),
            ..Default::default()
        };

        let mut source = ScriptedStimulusSource { packet };
        let mut sink = CollectingSpikeSink::new();
        let mut stimuli = vec![0.0; 2];
        let mut spike_buf: Vec<crate::backend::SpikeEvent> = Vec::new();

        for _ in 0..3 {
            run_tick_for_test(
                &mut source,
                &mut network,
                &mut sink,
                &mut stimuli,
                &mut spike_buf,
            );
        }

        assert_eq!(sink.emitted.len(), 3);
        assert_eq!(network.global_step, 3);
    }

    #[test]
    fn spiking_network_serde_matches_neuromod_06_contract() {
        // #41 checkpoint loading must use this crates.io `neuromod` 0.6.0 shape.
        // Do not fork `stdp_config` / `eligibility` in-tree.
        let network = SpikingNetwork::with_dimensions(2, 1, 2);
        let json = serde_json::to_value(&network).expect("serialize blank network");

        let default_stdp =
            serde_json::to_value(neuromod::RmStdpConfig::default()).expect("serialize stdp_config");
        assert_eq!(json.get("stdp_config"), Some(&default_stdp));
        assert!(json.get("neurons").and_then(|n| n.get(0)).is_some());
        let eligibility = json["neurons"][0]
            .get("eligibility")
            .and_then(|e| e.as_array())
            .expect("0.6.0 LIF neurons serialize eligibility traces");
        assert_eq!(eligibility.len(), 2);
        let blank_trace = serde_json::to_value(neuromod::EligibilityTrace::default())
            .expect("serialize eligibility trace");
        assert_eq!(eligibility[0], blank_trace);

        let restored: SpikingNetwork =
            serde_json::from_value(json).expect("deserialize neuromod 0.6 network");
        assert_eq!(restored.num_channels, 2);
        assert_eq!(restored.neurons.len(), 2);
        assert_eq!(restored.iz_neurons.len(), 1);
        assert_eq!(restored.modulators, NeuroModulators::default());
        assert_eq!(restored.stdp_config, neuromod::RmStdpConfig::default());
        assert_eq!(restored.neurons[0].eligibility.len(), 2);
    }

    #[test]
    fn pre_0_6_checkpoint_json_still_deserializes() {
        // 0.6.0 JSON self-describing formats default missing R-STDP fields.
        let network = SpikingNetwork::with_dimensions(2, 1, 2);
        let mut json = serde_json::to_value(&network).expect("serialize blank network");
        let object = json.as_object_mut().expect("network is a JSON object");
        object.remove("stdp_config");
        for neuron in object["neurons"].as_array_mut().expect("neurons array") {
            neuron
                .as_object_mut()
                .expect("neuron object")
                .remove("eligibility");
        }

        let restored: SpikingNetwork =
            serde_json::from_value(json).expect("deserialize pre-0.6 JSON");
        assert_eq!(restored.stdp_config, neuromod::RmStdpConfig::default());
        assert!(restored.neurons.iter().all(|n| n.eligibility.is_empty()));
    }

    #[test]
    fn daemon_rejects_zero_ingress_capacity() {
        let mut cfg = sample_config();
        cfg.ingress.sensory_capacity = 0;
        let err = match BrainstemDaemon::try_with_backend(cfg, BackendPair::stub()) {
            Ok(_) => panic!("expected zero ingress capacity to fail"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("sensory") && message.contains("capacity"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn config_defaults_ingress_when_section_omitted() {
        let path = write_config_toml(
            "ingress-omit",
            r#"
tick_rate_hz = 1000
log_level = "info"
spine_sub_port = 5555
spine_pub_port = 5556
model_path = "/tmp/model.mem"
lif_count = 16
izh_count = 5
channels = 16
"#,
        );
        let cfg = DaemonConfig::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.ingress, IngressConfig::default());
    }

    #[test]
    fn config_parses_ingress_section() {
        let path = write_config_toml(
            "ingress-set",
            r#"
tick_rate_hz = 1000
log_level = "info"
spine_sub_port = 5555
spine_pub_port = 5556
model_path = "/tmp/model.mem"
lif_count = 16
izh_count = 5
channels = 16

[ingress]
sensory_capacity = 2
sensory_policy = "drop_oldest"
reward_policy = "coalesce"
control_policy = "block_timeout"
telemetry_policy = "reject"
block_timeout_ms = 0
"#,
        );
        let cfg = DaemonConfig::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.ingress.sensory_capacity, 2);
        assert_eq!(
            cfg.ingress.sensory_policy,
            crate::ingress::OverflowPolicy::DropOldest
        );
        assert_eq!(
            cfg.ingress.reward_policy,
            crate::ingress::OverflowPolicy::Coalesce
        );
        assert_eq!(
            cfg.ingress.control_policy,
            crate::ingress::OverflowPolicy::BlockTimeout
        );
        assert_eq!(
            cfg.ingress.telemetry_policy,
            crate::ingress::OverflowPolicy::Reject
        );
        assert_eq!(cfg.ingress.block_timeout_ms, 0);
    }

    #[test]
    fn run_tick_drains_control_and_backend_sensory() {
        use crate::backend::CollectingSpikeSink;
        use crate::ingress::MessageClass;

        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        let _ = ingress.enqueue(
            MessageClass::Control,
            IngressPacket {
                stimuli: vec![9.0],
                modulators: None,
                ..Default::default()
            },
        );

        struct PacketSource;
        impl StimulusSource for PacketSource {
            fn next_ingress(&mut self) -> anyhow::Result<Option<IngressPacket>> {
                Ok(Some(IngressPacket {
                    stimuli: vec![0.1, 0.2],
                    modulators: Some(vec![0.5, 0.0, 0.0, 0.0]),
                    ..Default::default()
                }))
            }

            fn initialize(&mut self, _model_path: Option<&str>) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut source = PacketSource;
        let mut sink = CollectingSpikeSink::new();
        let mut network = SpikingNetwork::with_dimensions(2, 0, 2);
        let mut stimuli = vec![0.0; 2];
        let mut spike_buf: Vec<crate::backend::SpikeEvent> = Vec::new();
        let health = HealthHandle::started(HealthLimits::default());
        super::run_tick(
            &mut source,
            &mut network,
            &mut sink,
            &mut stimuli,
            &mut spike_buf,
            &ingress,
            &mut super::TickReport {
                health: &health,
                stats: &mut RuntimeStats::default(),
                diagnostics: &mut super::TickDiagnostics::default(),
            },
        );

        assert_eq!(stimuli, vec![0.1, 0.2]);
        assert_eq!(ingress.metrics().control.depth, 0);
        assert_eq!(ingress.metrics().sensory.depth, 0);
        assert_eq!(ingress.metrics().reward.depth, 0);
        assert_eq!(sink.emitted.len(), 1);
    }

    #[test]
    fn run_tick_skips_backend_none_without_evicting_sensory() {
        use crate::backend::CollectingSpikeSink;
        use crate::ingress::MessageClass;

        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        let _ = ingress.enqueue(
            MessageClass::Sensory,
            IngressPacket {
                stimuli: vec![0.3, 0.4],
                modulators: None,
                ..Default::default()
            },
        );

        struct NoneSource;
        impl StimulusSource for NoneSource {
            fn next_ingress(&mut self) -> anyhow::Result<Option<IngressPacket>> {
                Ok(None)
            }

            fn initialize(&mut self, _model_path: Option<&str>) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut source = NoneSource;
        let mut sink = CollectingSpikeSink::new();
        let mut network = SpikingNetwork::with_dimensions(2, 0, 2);
        let mut stimuli = vec![0.0; 2];
        let mut spike_buf: Vec<crate::backend::SpikeEvent> = Vec::new();
        let health = HealthHandle::started(HealthLimits::default());
        super::run_tick(
            &mut source,
            &mut network,
            &mut sink,
            &mut stimuli,
            &mut spike_buf,
            &ingress,
            &mut super::TickReport {
                health: &health,
                stats: &mut RuntimeStats::default(),
                diagnostics: &mut super::TickDiagnostics::default(),
            },
        );

        assert_eq!(stimuli, vec![0.3, 0.4]);
        assert_eq!(ingress.metrics().sensory.depth, 0);
        assert_eq!(sink.emitted.len(), 1);
    }

    // Sends a real SIGTERM to this test process, so it's `#[ignore]`d by default:
    // `cargo test` runs the whole crate's tests in parallel threads of one process,
    // and a process-wide SIGTERM delivered before the handler below finishes
    // registering would fall through to the OS default disposition and kill the
    // entire test binary, taking sibling tests down with it. Run explicitly and in
    // isolation to verify: `cargo test --lib -- --ignored --test-threads=1 shutdown_signal_returns_on_sigterm`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "sends a real SIGTERM to the whole test process; run in isolation, see comment above"]
    async fn shutdown_signal_returns_on_sigterm() {
        let handle = tokio::spawn(shutdown_signal());

        // Give the signal handler a moment to register before raising it.
        time::sleep(Duration::from_millis(50)).await;

        let pid = std::process::id();
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("failed to invoke `kill`");
        assert!(status.success(), "`kill -TERM` failed: {status:?}");

        time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shutdown_signal did not return after SIGTERM")
            .expect("shutdown_signal task panicked");
    }
}
