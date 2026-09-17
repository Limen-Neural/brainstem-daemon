// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Brainstem daemon runtime and config-driven service registry.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use neuromod::{NeuroModulators, SpikingNetwork};
use serde::Deserialize;
use tokio::signal;
use tokio::time;
use tracing::{error, info, warn};

use crate::backend::{
    BackendPair, IngressPacket, NEUROMODULATOR_COUNT, SpikeEvent as LocalSpikeEvent, SpikeSink,
    StimulusSource,
};
use crate::registry::{ServiceConfig, ServiceRegistry};

/// Env var read by crates.io `corpus-ipc` 0.1 `ZmqIpcBackend::initialize`.
pub const CORPUS_IPC_READOUT_ENV: &str = "CORPUS_IPC_ZMQ_READOUT_IPC";
/// Legacy name still set by the binary for older tooling; published corpus-ipc ignores it.
pub const LEGACY_SPIKENAUT_READOUT_ENV: &str = "SPIKENAUT_ZMQ_READOUT_IPC";

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
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
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
        Ok(cfg)
    }
}

/// Headless spiking-network daemon.
///
/// Owns the tick loop and delegates I/O to pluggable `StimulusSource` + `SpikeSink`.
pub struct BrainstemDaemon {
    config: DaemonConfig,
    registry: ServiceRegistry,
    backend: BackendPair,
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
        validate_neuron_count(&config)?;

        let services = std::mem::take(&mut config.services);
        let registry = ServiceRegistry::from_configs(services);
        Ok(Self {
            config,
            registry,
            backend,
        })
    }

    /// Return a reference to the config-driven service registry.
    pub fn registry(&self) -> &ServiceRegistry {
        &self.registry
    }

    /// Run the daemon until a termination signal is received.
    pub async fn run(self) -> Result<()> {
        let cfg = self.config;
        let mut backend = self.backend;

        if cfg.tick_rate_hz == 0 || cfg.tick_rate_hz > 1_000_000 {
            anyhow::bail!("tick_rate_hz must be in range 1..=1_000_000");
        }

        let tick_duration = Duration::from_nanos(1_000_000_000 / u64::from(cfg.tick_rate_hz));
        let mut ticker = time::interval(tick_duration);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        // Blank `neuromod` 0.6.0 network (crates.io). Checkpoint restore is #41;
        // that work must deserialize this crate's `SpikingNetwork` (no in-tree fork).
        let mut network =
            SpikingNetwork::with_dimensions(cfg.lif_count, cfg.izh_count, cfg.channels);
        let mut stimuli = vec![0.0; cfg.channels];
        let mut spike_buf: Vec<LocalSpikeEvent> = Vec::with_capacity(128);

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
                    );
                }
                _ = &mut shutdown => {
                    info!("Termination signal received, shutting down");
                    break;
                }
            }
        }

        // Explicit backend lifecycle hooks (flush sink, shutdown source) are invoked
        // for custom backends. Current built-ins are no-ops, but this satisfies
        // CodeAnt/CodeRabbit "missing cleanup" notes.
        if let Err(e) = backend.sink.flush() {
            warn!("Failed to flush spike sink on shutdown: {e}");
        }
        if let Err(e) = backend.source.shutdown() {
            warn!("Failed to shut down stimulus source: {e}");
        }

        Ok(())
    }
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

fn validate_neuron_count(config: &DaemonConfig) -> Result<()> {
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

// Trait-based tick loop (works with or without corpus-ipc feature)

fn run_tick(
    source: &mut dyn StimulusSource,
    network: &mut SpikingNetwork,
    sink: &mut dyn SpikeSink,
    stimuli: &mut [f32],
    spike_buf: &mut Vec<LocalSpikeEvent>,
) {
    let packet = match source.next_ingress() {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Per StimulusSource contract: None means skip ingress this tick but still
            // advance the network with zeroed stimuli (maintains tick cadence).
            // decode_inputs will zero-fill the stimuli buffer based on the empty readout.
            IngressPacket {
                stimuli: Vec::new(),
                modulators: None,
            }
        }
        Err(e) => {
            warn!("Failed to receive from stimulus source: {e}");
            return;
        }
    };

    let modulators = decode_inputs(&packet, stimuli);

    // Note: decode_inputs already zero-fills any remaining channels when packet.stimuli is shorter.

    // `step` is the thread-local RNG wrapper around 0.6 `step_with_rng`.
    // The generator is not stored on the network and is not checkpointed.
    let spike_ids = match network.step(stimuli, &modulators) {
        Ok(spikes) => spikes,
        Err(e) => {
            error!("Network step failed: {e}");
            return;
        }
    };

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
        warn!(
            "dropped {} spikes with out-of-range IDs this tick (network may be larger than u16)",
            dropped
        );
    }

    if spike_buf.is_empty() && !spike_ids.is_empty() {
        // Had spikes from network but all IDs were out of u16 range (dropped).
        // Nothing valid to publish; skip to avoid empty batch for dropped case.
        return;
    }

    // Emit the batch for this tick.
    // - May be empty if no neurons fired this tick (original behavior for some
    //   downstream consumers that expect a message per tick).
    // - We deliberately do not suppress empty batches here to keep test
    //   expectations (CollectingSpikeSink) and wire behavior stable.
    if let Err(e) = sink.emit(spike_buf, now) {
        warn!("Failed to emit spikes: {e}");
    }
}

/// Decode stimuli plus a `neuromod` 0.6 modulator snapshot from an
/// [`IngressPacket`].
///
/// `packet.modulators` is already in 0.6 order (dopamine, serotonin,
/// acetylcholine, norepinephrine). The `corpus-ipc` ZMQ adapter converts
/// the corpus-ipc 0.1 Nero tail before this function sees it, so cortisol/tempo
/// are never copied onto serotonin/norepinephrine here.
/// When `packet.modulators` is `None` (the common stub path in PR A), we
/// return defaults. This mirrors the previous "short readout" fallback.
fn decode_inputs(packet: &IngressPacket, stimuli: &mut [f32]) -> NeuroModulators {
    let readout = &packet.stimuli;
    let channels = stimuli.len();
    let upto = readout.len().min(channels);
    stimuli[..upto].copy_from_slice(&readout[..upto]);
    if readout.len() < channels {
        stimuli[upto..].fill(0.0);
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
    run_tick(source, network, sink, stimuli, spike_buf);
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
            services: vec![
                ServiceConfig::named("telemetry"),
                ServiceConfig::named("critic-ipc"),
            ],
        }
    }

    #[test]
    fn daemon_builds_registry_from_config() {
        let daemon = BrainstemDaemon::new(sample_config());
        assert_eq!(daemon.registry().len(), 2);
        assert!(daemon.registry().contains("telemetry"));
        assert!(daemon.registry().contains("critic-ipc"));
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
        };
        let mut stimuli = vec![0.0; 4];
        let mods = decode_inputs(&packet, &mut stimuli);
        assert_eq!(stimuli, vec![0.1, 0.2, 0.0, 0.0]);
        assert_eq!(mods, NeuroModulators::default());
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
    fn stub_backend_basic_tick() {
        let mut source = crate::backend::StubStimulusSource;
        let mut sink = CollectingSpikeSink::new();
        let mut network = SpikingNetwork::with_dimensions(2, 0, 2);
        let mut stimuli = vec![0.0; 2];
        let mut spike_buf: Vec<crate::backend::SpikeEvent> = Vec::new();

        // Prime one tick
        run_tick_for_test(
            &mut source,
            &mut network,
            &mut sink,
            &mut stimuli,
            &mut spike_buf,
        );

        // Sink should have received one (possibly empty) batch
        assert_eq!(sink.emitted.len(), 1);
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
