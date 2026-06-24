// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2024 Raul Montoya Cardenas

//! Headless binary entry point for the brainstem daemon.

use std::path::PathBuf;

use brainstem_daemon::daemon::{BrainstemDaemon, DaemonConfig};
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    let cfg = match DaemonConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config {}: {e}", config_path.display());
            std::process::exit(1);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(cfg.log_level.clone()).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Loaded config from {}", config_path.display());

    let daemon = BrainstemDaemon::new(cfg);
    daemon.run().await
}
