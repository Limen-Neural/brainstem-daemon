// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2024 Raul Montoya Cardenas

//! Headless binary entry point for the brainstem daemon.

use std::path::PathBuf;

use brainstem_daemon::daemon::{BrainstemDaemon, CORPUS_IPC_READOUT_ENV, DaemonConfig};
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

    // `corpus-ipc` reads the ZMQ readout endpoint from this env var during
    // `ZmqBrainBackend::initialize`. Set it on the main thread before any
    // async runtime / worker threads are spawned.
    // SAFETY: no other threads exist at this point in `main`.
    let readout_endpoint = format!("tcp://127.0.0.1:{}", cfg.spine_sub_port);
    unsafe {
        std::env::set_var(CORPUS_IPC_READOUT_ENV, &readout_endpoint);
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

    let daemon = BrainstemDaemon::new(cfg);
    daemon.run().await
}
