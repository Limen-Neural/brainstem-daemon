// Copyright 2026 Raul Montoya Cardenas

//! Daemon configuration: TOML-loaded config types, serde defaults, and PUB socket validation/endpoint helpers.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::ingress::IngressConfig;
use crate::logging::validate_log_level;
use crate::registry::ServiceConfig;

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
    /// Bind host for the built-in `corpus-ipc` ZMQ PUB listener.
    ///
    /// Defaults to loopback (`127.0.0.1`) so the daemon does not expose the
    /// spike egress socket on all interfaces implicitly. Set to `0.0.0.0` (or a
    /// specific interface address) to opt in to broader exposure. Parsed but a
    /// no-op under the stub backend (mirrors `spine_pub_port`); only the
    /// `corpus-ipc` PUB path consumes it.
    #[serde(default = "default_spine_pub_bind_host")]
    pub spine_pub_bind_host: String,
    /// Explicit finite send high-water mark (SNDHWM) applied to the PUB socket
    /// before bind. Bounds how many outbound messages ZeroMQ queues per
    /// subscriber before it silently drops. Parsed but a no-op under the stub
    /// backend; only the `corpus-ipc` PUB path consumes it.
    #[serde(default = "default_spine_pub_sndhwm")]
    pub spine_pub_sndhwm: i32,
    /// Finite LINGER (milliseconds) applied to the PUB socket before bind so
    /// teardown and signal shutdown cannot block indefinitely on pending
    /// messages. `0` (the default) drops any pending messages on close, the
    /// safe bounded default for a best-effort PUB. Parsed but a no-op under the
    /// stub backend; only the `corpus-ipc` PUB path consumes it.
    #[serde(default = "default_spine_pub_linger_ms")]
    pub spine_pub_linger_ms: i32,
    /// Empty-spike-batch compatibility policy for the PUB egress.
    ///
    /// `true` (the default) preserves the current behavior where the tick loop
    /// emits a frame every tick even when the batch has zero spikes, so
    /// subscribers relying on per-tick / heartbeat frames keep working. Set to
    /// `false` to suppress empty batches at the sink. Parsed but a no-op under
    /// the stub backend; only the `corpus-ipc` PUB path consumes it.
    #[serde(default = "default_spine_pub_send_empty_batches")]
    pub spine_pub_send_empty_batches: bool,
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

/// Default PUB bind host: loopback, requiring explicit opt-in for broader exposure.
fn default_spine_pub_bind_host() -> String {
    "127.0.0.1".to_string()
}

/// Default finite send high-water mark for the PUB socket.
fn default_spine_pub_sndhwm() -> i32 {
    1000
}

/// Default finite LINGER (ms) for the PUB socket: drop pending on close.
fn default_spine_pub_linger_ms() -> i32 {
    0
}

/// Default empty-batch policy: send empty batches (preserves current behavior).
fn default_spine_pub_send_empty_batches() -> bool {
    true
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
        validate_pub_socket_opts(&cfg)
            .with_context(|| format!("invalid config from {}", path.display()))?;
        Ok(cfg)
    }
}

/// Fail closed on PUB socket options whose libzmq-valid sentinel values would
/// defeat the bounded PUB lifecycle guarantee.
///
/// `spine_pub_sndhwm` must be `> 0`: `0` disables the send high-water mark
/// (unbounded buffering) and negatives are nonsensical. `spine_pub_linger_ms`
/// must be `>= 0`: a negative value requests infinite LINGER, which can block
/// shutdown indefinitely; `0` (the default) is the safe drop-on-close default.
pub(crate) fn validate_pub_socket_opts(config: &DaemonConfig) -> Result<()> {
    if config.spine_pub_sndhwm <= 0 {
        bail!(
            "spine_pub_sndhwm must be > 0 (0 disables the send high-water mark, which defeats the bounded PUB lifecycle)"
        );
    }
    if config.spine_pub_linger_ms < 0 {
        bail!(
            "spine_pub_linger_ms must be >= 0 (a negative value requests infinite LINGER, which can block shutdown indefinitely)"
        );
    }
    Ok(())
}

/// Build a ZeroMQ TCP endpoint string, bracketing IPv6 literal hosts.
///
/// ZeroMQ TCP endpoints require IPv6 literals to be bracketed
/// (`tcp://[::1]:5556`); an unbracketed `format!("tcp://{host}:{port}")` on
/// `::1` yields the invalid `tcp://::1:5556`. IPv4 literals and hostnames have
/// no `:` and are emitted unchanged; already-bracketed hosts are left as-is.
pub fn pub_endpoint(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("tcp://[{host}]:{port}")
    } else {
        format!("tcp://{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Common valid config body with the given `log_level` and no PUB socket
    /// keys, followed by `extra` lines that add or override a single key under
    /// test. Centralizes the shared TOML so each case only spells out what it
    /// varies.
    fn config_toml_with_log_level(log_level: &str, extra: &str) -> String {
        format!(
            r#"
tick_rate_hz = 1000
log_level = "{log_level}"
spine_sub_port = 5555
spine_pub_port = 5556
model_path = "/tmp/model.mem"
lif_count = 1
izh_count = 0
channels = 1
{extra}"#
        )
    }

    /// Common valid config body with `log_level = "info"`. See
    /// [`config_toml_with_log_level`].
    fn base_config_toml(extra: &str) -> String {
        config_toml_with_log_level("info", extra)
    }

    /// Load a config built from [`base_config_toml`] via a temp file, cleaning
    /// up the file before returning the result.
    fn load_base_config(stem: &str, extra: &str) -> Result<DaemonConfig> {
        let path = write_config_toml(stem, &base_config_toml(extra));
        let result = DaemonConfig::load(&path);
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn config_parses_optional_control_bind() {
        let cfg: DaemonConfig =
            toml::from_str(&base_config_toml("control_bind = \"127.0.0.1:9464\"")).expect("toml");
        assert_eq!(cfg.control_bind.as_deref(), Some("127.0.0.1:9464"));
    }

    #[test]
    fn omitted_runtime_mode_deserializes_as_live() {
        let cfg: DaemonConfig = toml::from_str(&base_config_toml("")).expect("toml");
        assert_eq!(cfg.runtime_mode, RuntimeMode::Live);
    }

    #[test]
    fn config_load_rejects_invalid_pub_socket_and_log_options() {
        // Each case varies a single aspect of the shared valid body and must be
        // rejected by DaemonConfig::load with an error mentioning the offending
        // field/reason. Substrings preserve the per-case assertions the
        // individual tests used to make. `log_level` is threaded through the
        // shared body so the invalid-log-level case can vary it without
        // duplicating the `log_level` key.
        let cases: &[(&str, &str, &str, &[&str])] = &[
            (
                "invalid-log-level",
                "verbose",
                "",
                &[
                    "invalid log_level \"verbose\"",
                    "error, warn, info, debug, trace",
                ],
            ),
            (
                "sndhwm-zero",
                "info",
                "spine_pub_sndhwm = 0",
                &["spine_pub_sndhwm must be > 0"],
            ),
            (
                "sndhwm-negative",
                "info",
                "spine_pub_sndhwm = -1",
                &["spine_pub_sndhwm must be > 0"],
            ),
            (
                "linger-negative",
                "info",
                "spine_pub_linger_ms = -1",
                &["spine_pub_linger_ms must be >= 0"],
            ),
        ];

        for (stem, log_level, extra, expected_substrings) in cases {
            let path = write_config_toml(stem, &config_toml_with_log_level(log_level, extra));
            let result = DaemonConfig::load(&path);
            let _ = std::fs::remove_file(&path);
            let err = result
                .err()
                .unwrap_or_else(|| panic!("expected `{stem}` config to be rejected"));
            let message = format!("{err:#}");
            for needle in *expected_substrings {
                assert!(
                    message.contains(needle),
                    "case `{stem}`: expected error to contain {needle:?}, got {message}"
                );
            }
        }
    }

    #[test]
    fn config_load_accepts_defaults_without_pub_socket_keys() {
        // Backward compat: a minimal TOML omitting the PUB socket keys keeps the
        // safe defaults (sndhwm 1000, linger 0) and loads cleanly.
        let cfg = load_base_config("pub-defaults", "").expect("load minimal config");
        assert_eq!(cfg.spine_pub_sndhwm, 1000);
        assert_eq!(cfg.spine_pub_linger_ms, 0);
        assert_eq!(cfg.spine_pub_bind_host, "127.0.0.1");
    }

    #[test]
    fn config_load_accepts_zero_linger_explicitly() {
        let cfg = load_base_config(
            "linger-zero",
            "spine_pub_sndhwm = 4\nspine_pub_linger_ms = 0",
        )
        .expect("load explicit-zero-linger config");
        assert_eq!(cfg.spine_pub_sndhwm, 4);
        assert_eq!(cfg.spine_pub_linger_ms, 0);
    }

    #[test]
    fn pub_endpoint_brackets_ipv6_literals() {
        assert_eq!(super::pub_endpoint("::1", 5556), "tcp://[::1]:5556");
        assert_eq!(super::pub_endpoint("fe80::1", 5556), "tcp://[fe80::1]:5556");
    }

    #[test]
    fn pub_endpoint_leaves_ipv4_and_hostnames_unbracketed() {
        assert_eq!(
            super::pub_endpoint("127.0.0.1", 5556),
            "tcp://127.0.0.1:5556"
        );
        assert_eq!(super::pub_endpoint("0.0.0.0", 5556), "tcp://0.0.0.0:5556");
        assert_eq!(
            super::pub_endpoint("localhost", 5556),
            "tcp://localhost:5556"
        );
    }

    #[test]
    fn pub_endpoint_leaves_already_bracketed_hosts_untouched() {
        assert_eq!(super::pub_endpoint("[::1]", 5556), "tcp://[::1]:5556");
    }
}
