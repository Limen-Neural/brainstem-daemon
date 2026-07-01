// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2024 Raul Montoya Cardenas

//! Brainstem daemon runtime and config-driven service registry.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use corpus_ipc::{NeuralBackend, SpikeBatch, SpikeEvent, SpineMessage, ZmqBrainBackend};
use neuromod::{NeuroModulators, SpikingNetwork};
use serde::Deserialize;
use tokio::signal;
use tokio::time;
use tracing::{error, info, warn};

use crate::registry::{ServiceConfig, ServiceRegistry};

/// Environment variable name used by `corpus-ipc` to discover the ZMQ readout endpoint.
///
/// This is a `corpus-ipc` integration contract; the daemon does not choose the name.
/// Callers are expected to set this variable before initializing the runtime.
pub const CORPUS_IPC_READOUT_ENV: &str = "SPIKENAUT_ZMQ_READOUT_IPC";

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
        // Security: reject absolute paths that traverse outside (e.g. /etc/../foo).
        // Relative parent paths are allowed (common when running from subdirs).
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
/// Owns the tick loop, the neuromod network, and the corpus-ipc / ZeroMQ
/// ingress/egress plumbing. It does **not** own trading, mining, or weight
/// training logic; those live in other project boundaries.
pub struct BrainstemDaemon {
    config: DaemonConfig,
    registry: ServiceRegistry,
}

impl BrainstemDaemon {
    /// Build a daemon from configuration. The service registry is populated
    /// from the config's `services` list; disabled services are ignored.
    ///
    /// # Environment setup for corpus-ipc
    ///
    /// Callers must ensure `CORPUS_IPC_READOUT_ENV` (SPIKENAUT_ZMQ_READOUT_IPC)
    /// is set to the desired ZMQ SUB endpoint *before* calling this constructor
    /// or `run()`. The binary wrapper sets it on the main thread before any
    /// runtime is created. Library users are responsible for the same.
    pub fn new(mut config: DaemonConfig) -> Self {
        if config.lif_count + config.izh_count > u16::MAX as usize {
            // Fail early at construction rather than dropping spike batches at runtime
            // for networks larger than u16 can address in the spike channel field.
            panic!(
                "lif_count + izh_count ({} + {}) exceeds u16::MAX; spike channel ids would not fit",
                config.lif_count, config.izh_count
            );
        }

        let services = std::mem::take(&mut config.services);
        let registry = ServiceRegistry::from_configs(services);
        Self { config, registry }
    }

    /// Return a reference to the config-driven service registry.
    pub fn registry(&self) -> &ServiceRegistry {
        &self.registry
    }

    /// Run the daemon until a termination signal is received.
    pub async fn run(self) -> Result<()> {
        let cfg = self.config;

        if cfg.tick_rate_hz == 0 || cfg.tick_rate_hz > 1_000_000 {
            anyhow::bail!("tick_rate_hz must be in range 1..=1_000_000");
        }

        let tick_duration = Duration::from_nanos(1_000_000_000 / u64::from(cfg.tick_rate_hz));
        let mut ticker = time::interval(tick_duration);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let (mut network, mut ingress, pub_socket) = init_runtime(&cfg)?;
        let mut stimuli = vec![0.0; cfg.channels];
        // Pre-allocate spike buffer to avoid allocation in the 1 kHz hot path.
        let mut spike_buf: Vec<SpikeEvent> = Vec::with_capacity(128);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    run_tick(&mut ingress, &mut network, &pub_socket, &mut stimuli, cfg.channels, &mut spike_buf);
                }
                _ = signal::ctrl_c() => {
                    info!("Termination signal received, shutting down");
                    break;
                }
            }
        }

        Ok(())
    }
}

fn init_runtime(cfg: &DaemonConfig) -> Result<(SpikingNetwork, ZmqBrainBackend, zmq::Socket)> {
    let network = SpikingNetwork::with_dimensions(cfg.lif_count, cfg.izh_count, cfg.channels);
    let mut ingress = ZmqBrainBackend::new();
    // `CORPUS_IPC_READOUT_ENV` must be set by the caller (binary main before runtime,
    // or library user) before initialize so corpus-ipc knows the SUB endpoint.
    ingress.initialize(Some(&cfg.model_path.to_string_lossy()))?;

    let zmq_context = zmq::Context::new();
    let pub_socket = zmq_context.socket(zmq::PUB)?;
    pub_socket.bind(&format!("tcp://*:{}", cfg.spine_pub_port))?;
    let readout_endpoint = format!("tcp://127.0.0.1:{}", cfg.spine_sub_port);
    info!(
        "Ingress SUB {} / Egress PUB tcp://*:{}",
        readout_endpoint, cfg.spine_pub_port
    );

    Ok((network, ingress, pub_socket))
}

fn run_tick(
    ingress: &mut ZmqBrainBackend,
    network: &mut SpikingNetwork,
    pub_socket: &zmq::Socket,
    stimuli: &mut [f32],
    channels: usize,
    spike_buf: &mut Vec<SpikeEvent>,
) {
    // ZMQ calls are synchronous. This is a dedicated current_thread real-time
    // loop (no other tasks). Blocking here is by design for lowest jitter at 1 kHz.
    let readout = match ingress.process_signals(&[]) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to receive from corpus-ipc backend: {e}");
            return;
        }
    };

    let modulators = decode_inputs(&readout, channels, stimuli);
    let spike_ids = match network.step(stimuli, &modulators) {
        Ok(spikes) => spikes,
        Err(e) => {
            error!("Network step failed: {e:?}");
            return;
        }
    };

    if let Err(e) = publish_spikes(pub_socket, &spike_ids, spike_buf) {
        warn!("Failed to publish spikes: {e}");
    }
}

fn decode_inputs(readout: &[f32], channels: usize, stimuli: &mut [f32]) -> NeuroModulators {
    let upto = readout.len().min(channels);
    stimuli[..upto].copy_from_slice(&readout[..upto]);
    stimuli[upto..].fill(0.0);

    if readout.len() >= channels + 4 {
        NeuroModulators {
            dopamine: readout[channels],
            cortisol: readout[channels + 1],
            acetylcholine: readout[channels + 2],
            tempo: readout[channels + 3],
            aux_dopamine: 0.0,
        }
    } else {
        NeuroModulators::default()
    }
}

fn publish_spikes(
    pub_socket: &zmq::Socket,
    spike_ids: &[usize],
    out: &mut Vec<SpikeEvent>,
) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let tick = now.as_millis() as u64;

    out.clear();
    let mut dropped = 0usize;
    for &idx in spike_ids {
        match u16::try_from(idx) {
            Ok(channel) => {
                out.push(SpikeEvent {
                    channel,
                    time: (tick & (u32::MAX as u64)) as u32,
                    strength: 1.0,
                });
            }
            Err(e) => {
                dropped += 1;
                warn!(
                    "spike id exceeds u16 range ({}), dropping spike: {}",
                    idx, e
                );
            }
        }
    }

    if dropped > 0 {
        warn!(
            "dropped {} spikes with out-of-range IDs this tick (network may be larger than u16)",
            dropped
        );
    }

    if out.is_empty() && !spike_ids.is_empty() {
        // nothing valid to publish; avoid sending an empty batch
        return Ok(());
    }

    // Hand the current buffer to the message while leaving `out` with matching capacity.
    // This preserves the pre-allocation across ticks (avoids repeated heap growth at 1 kHz).
    let spikes = std::mem::replace(out, Vec::with_capacity(out.capacity()));
    let msg = SpineMessage::Spikes(SpikeBatch {
        session_id: None,
        batch_id: tick,
        timestamp: now.as_nanos() as u64,
        spikes,
        metadata: None,
    });
    let payload = serde_json::to_vec(&msg)?;
    pub_socket.send(payload, 0)?;
    // `out` is now empty but retains the pre-allocated capacity for the next tick.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let readout = vec![0.1, 0.2, 0.3, 0.4];
        let mut stimuli = vec![0.0; 4];
        let _mods = decode_inputs(&readout, 4, &mut stimuli);
        assert_eq!(stimuli, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn decode_inputs_takes_modulators_when_present() {
        let readout = vec![0.0; 4]
            .into_iter()
            .chain([0.5, 0.6, 0.7, 0.8])
            .collect::<Vec<_>>();
        let mut stimuli = vec![0.0; 4];
        let mods = decode_inputs(&readout, 4, &mut stimuli);
        assert_eq!(mods.dopamine, 0.5);
        assert_eq!(mods.cortisol, 0.6);
        assert_eq!(mods.acetylcholine, 0.7);
        assert_eq!(mods.tempo, 0.8);
    }

    #[test]
    fn decode_inputs_defaults_modulators_when_short() {
        let readout = vec![0.1, 0.2];
        let mut stimuli = vec![0.0; 4];
        let mods = decode_inputs(&readout, 4, &mut stimuli);
        assert_eq!(stimuli, vec![0.1, 0.2, 0.0, 0.0]);
        assert_eq!(mods, NeuroModulators::default());
    }
}
