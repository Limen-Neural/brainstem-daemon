// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Headless binary entry point for the brainstem daemon.

use std::path::PathBuf;

use brainstem_daemon::backend::BackendPair;
use brainstem_daemon::daemon::{BrainstemDaemon, DaemonConfig};

#[cfg(feature = "corpus-ipc")]
use brainstem_daemon::daemon::CORPUS_IPC_READOUT_ENV;

#[cfg(feature = "corpus-ipc")]
use brainstem_daemon::StimulusSource;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// CLI arguments.
#[derive(Parser, Debug)]
#[command(version, about = "Soma Spiking Network Daemon", long_about = None)]
struct Cli {
    /// Override configuration file path.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("soma/daemon.toml")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    let cfg = DaemonConfig::load(&config_path).map_err(|e| {
        eprintln!("Failed to load config {}: {e}", config_path.display());
        std::process::exit(1);
    })?;

    // Set the readout endpoint env var(s) when corpus-ipc feature is enabled.
    // Binary controls the endpoint; we set both the documented SPIKENAUT name
    // and the CORPUS_IPC_ZMQ name that the pinned corpus-ipc backend reads.
    #[cfg(feature = "corpus-ipc")]
    {
        let readout_endpoint = format!("tcp://127.0.0.1:{}", cfg.spine_sub_port);
        // SAFETY: no other threads exist at this point in `main`.
        unsafe {
            std::env::set_var(CORPUS_IPC_READOUT_ENV, &readout_endpoint);
            std::env::set_var("CORPUS_IPC_ZMQ_READOUT_IPC", &readout_endpoint);
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cfg, config_path))
}

async fn run(cfg: DaemonConfig, config_path: PathBuf) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(cfg.log_level.clone()).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Loaded config from {}", config_path.display());

    // Choose backend explicitly so we can log the mode.
    #[cfg(feature = "corpus-ipc")]
    let pair = {
        // Build a real ZMQ pair (binary is responsible for the SUB endpoint via env).
        // We still need to create the PUB side here because the default `new()` path
        // is intentionally conservative.
        let mut source = brainstem_daemon::backend::ZmqStimulusSource::with_channels(cfg.channels);

        // Pass the model path through (was dropped before).
        let model_path = cfg.model_path.to_string_lossy();
        source
            .initialize(Some(model_path.as_ref()))
            .map_err(|e| anyhow::anyhow!("failed to initialize ZMQ stimulus source: {e}"))?;

        let zmq_context = zmq::Context::new();
        let pub_socket = zmq_context
            .socket(zmq::PUB)
            .map_err(|e| anyhow::anyhow!("failed to create ZMQ PUB socket: {e}"))?;
        pub_socket
            .bind(&format!("tcp://*:{}", cfg.spine_pub_port))
            .map_err(|e| {
                anyhow::anyhow!("failed to bind ZMQ PUB on {}: {e}", cfg.spine_pub_port)
            })?;

        info!("📡 Using ZMQ corpus-ipc backend (spine ports active)");

        BackendPair {
            source: Box::new(source),
            sink: Box::new(brainstem_daemon::backend::ZmqSpikeSink::new(pub_socket)),
        }
    };

    #[cfg(not(feature = "corpus-ipc"))]
    let pair = {
        info!("🔌 Using stub backend (corpus-ipc disabled)");
        BackendPair::stub()
    };

    let daemon = BrainstemDaemon::with_backend(cfg, pair);
    daemon.run().await
}
