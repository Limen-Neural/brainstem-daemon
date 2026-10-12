// Copyright 2026 Raul Montoya Cardenas

//! Daemon configuration: TOML-loaded config types, serde defaults, and PUB socket validation/endpoint helpers.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::ingress::IngressConfig;
use crate::logging::validate_log_level;
use crate::registry::ServiceConfig;

/// Hard cap on neuron × input-channel slots (weights and per-input state).
/// Keeps a valid per-packet width from requesting an unbounded dense network.
pub const MAX_NETWORK_INPUTS: usize = 1_048_576;

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
    /// Validate without allocating a network, reading a checkpoint, initializing
    /// a backend, or binding sockets. Call before constructing a custom backend;
    /// daemon constructors cannot undo side effects in a caller-supplied pair.
    /// Raw serde deserialization does not invoke this validation automatically.
    pub fn validate(&self) -> Result<()> {
        if !(1..=1_000_000).contains(&self.tick_rate_hz) {
            bail!(
                "tick_rate_hz must be in range 1..=1_000_000, got {}",
                self.tick_rate_hz
            );
        }
        validate_dimensions(self)?;
        self.ingress.validate()?;
        if self.channels == 0 || self.channels > self.ingress.max_payload_len {
            bail!(
                "channels must be in range 1..=ingress.max_payload_len ({}), got {}",
                self.ingress.max_payload_len,
                self.channels
            );
        }
        if self.runtime_mode == RuntimeMode::Live && self.izh_count != 0 {
            bail!(
                "live Spikenaut checkpoints are LIF-only; izh_count must be 0, got {}",
                self.izh_count
            );
        }
        validate_log_level(&self.log_level)?;
        validate_pub_bind_host(&self.spine_pub_bind_host)?;
        validate_pub_socket_opts(self)?;
        if let Some(bind) = &self.control_bind {
            bind.parse::<std::net::SocketAddr>().with_context(|| {
                format!("invalid control_bind {bind:?}; expected numeric IP:port (IPv6 as [address]:port)")
            })?;
        }
        Ok(())
    }

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
        cfg.validate()
            .with_context(|| format!("invalid config from {}", path.display()))?;
        Ok(cfg)
    }
}

fn validate_dimensions(config: &DaemonConfig) -> Result<()> {
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
    if total == 0 {
        bail!("lif_count + izh_count must be in range 1..=u16::MAX (65535), got 0");
    }
    if total > u16::MAX as usize {
        bail!(
            "lif_count + izh_count ({} + {}) exceeds u16::MAX ({})",
            config.lif_count,
            config.izh_count,
            u16::MAX
        );
    }
    let inputs = total.checked_mul(config.channels).ok_or_else(|| {
        anyhow::anyhow!("dimensions (lif_count + izh_count) * channels overflows usize")
    })?;
    if inputs > MAX_NETWORK_INPUTS {
        bail!(
            "dimensions (lif_count + izh_count) * channels ({inputs}) exceeds MAX_NETWORK_INPUTS ({MAX_NETWORK_INPUTS})"
        );
    }
    Ok(())
}

/// Validate host syntax without resolving DNS or opening a socket.
fn validate_pub_bind_host(host: &str) -> Result<()> {
    let ip = host.parse::<std::net::IpAddr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(|s| s.parse::<std::net::Ipv6Addr>().is_ok());
    if ip || host == "*" {
        return Ok(());
    }
    if is_dns_hostname(host) {
        return Ok(());
    }
    bail!(
        "invalid spine_pub_bind_host {host:?}; expected an IPv4/IPv6 address, hostname, or * (no scheme or port)"
    );
}

/// Syntactic hostname check. The final label must not be all digits: such a
/// name is never a resolvable bind host (`5555`, `999.999.999.999`) and would
/// otherwise only fail later, once libzmq tries to bind it.
fn is_dns_hostname(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host);
    let last_label_is_numeric = name
        .rsplit('.')
        .next()
        .is_some_and(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_digit()));
    !name.is_empty()
        && !last_label_is_numeric
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Fail closed on PUB socket options whose libzmq-valid sentinel values would
/// defeat the bounded PUB lifecycle guarantee.
///
/// `spine_pub_sndhwm` must be `> 0`: `0` disables the send high-water mark
/// (unbounded buffering) and negatives are nonsensical. `spine_pub_linger_ms`
/// must be `>= 0`: a negative value requests infinite LINGER, which can block
/// shutdown indefinitely; `0` (the default) is the safe drop-on-close default.
fn validate_pub_socket_opts(config: &DaemonConfig) -> Result<()> {
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

    // Each fixture replaces a valid TOML field or appends optional settings,
    // and names the configuration error that must precede checkpoint errors.
    const INVALID_RUNTIME_CONFIGS: &[(&str, &str, &str)] = &[
        ("tick_rate_hz = 1000", "tick_rate_hz = 0", "tick_rate_hz"),
        (
            "tick_rate_hz = 1000",
            "tick_rate_hz = 1000001",
            "tick_rate_hz",
        ),
        ("channels = 1", "channels = 0", "channels"),
        ("channels = 1", "channels = 4097", "channels"),
        ("lif_count = 1", "lif_count = 0", "lif_count + izh_count"),
        (
            "lif_count = 1",
            "lif_count = 65536",
            "lif_count + izh_count",
        ),
        (
            "lif_count = 1\nizh_count = 0\nchannels = 1",
            "lif_count = 257\nizh_count = 0\nchannels = 4096",
            "dimensions",
        ),
        ("", "control_bind = \"localhost:9464\"", "control_bind"),
        (
            "",
            "spine_pub_bind_host = \"tcp://127.0.0.1\"",
            "spine_pub_bind_host",
        ),
        ("", "[ingress]\nsensory_capacity = 0", "sensory"),
        (
            "",
            "[ingress]\nmax_payload_len = 1048577",
            "max_payload_len",
        ),
        (
            "channels = 1",
            "channels = 3\n[ingress]\nmax_payload_len = 2",
            "channels",
        ),
    ];

    #[test]
    fn invalid_runtime_config_is_rejected_at_every_entry_point() {
        use crate::backend::BackendPair;
        use crate::daemon::BrainstemDaemon;

        for (index, (old, new, field)) in INVALID_RUNTIME_CONFIGS.iter().copied().enumerate() {
            let body = if old.is_empty() {
                base_config_toml(new)
            } else {
                base_config_toml("").replace(old, new)
            };
            let path = write_config_toml(&format!("runtime-invalid-{index}"), &body);
            let loaded = DaemonConfig::load(&path);
            std::fs::remove_file(path).unwrap();
            let err = loaded.expect_err("invalid TOML configuration must fail at load");
            assert!(format!("{err:#}").contains(field), "{err:#}");

            // Raw serde and Rust literals need not be validated; consumers must
            // validate them before startup, even when no timed loop is used.
            for mode in [RuntimeMode::Live, RuntimeMode::Simulation] {
                let mut cfg: DaemonConfig = toml::from_str(&body).unwrap();
                cfg.runtime_mode = mode;
                let err = BrainstemDaemon::try_new(cfg.clone())
                    .and_then(|daemon| daemon.run_for_ticks(0))
                    .expect_err("invalid config must not reach bounded run");
                assert!(format!("{err:#}").contains(field), "{err:#}");
                assert!(
                    BrainstemDaemon::try_with_backend(cfg.clone(), BackendPair::stub()).is_err()
                );
                let err = crate::restore_network(&cfg)
                    .err()
                    .expect("invalid config must not restore");
                assert!(format!("{err:#}").contains(field), "{err:#}");
            }
        }
    }

    #[test]
    fn live_mode_rejects_izh_cells_at_load_and_construction() {
        use crate::daemon::BrainstemDaemon;

        let body = base_config_toml("").replace("izh_count = 0", "izh_count = 1");
        let path = write_config_toml("live-izh", &body);
        let loaded = DaemonConfig::load(&path);
        std::fs::remove_file(path).unwrap();
        let err = loaded.expect_err("live sidecars cannot restore Izhikevich cells");
        assert!(format!("{err:#}").contains("izh_count"), "{err:#}");
        let cfg: DaemonConfig = toml::from_str(&body).unwrap();
        let err = BrainstemDaemon::try_new(cfg)
            .err()
            .expect("live config must fail before startup");
        assert!(err.to_string().contains("izh_count"), "{err:#}");
    }

    #[test]
    fn rust_dimensions_reject_multiplication_overflow_before_allocation() {
        use crate::daemon::BrainstemDaemon;

        let mut cfg: DaemonConfig = toml::from_str(&base_config_toml("")).unwrap();
        cfg.lif_count = 2;
        cfg.channels = usize::MAX;
        for mode in [RuntimeMode::Live, RuntimeMode::Simulation] {
            cfg.runtime_mode = mode;
            let err = crate::restore_network(&cfg)
                .err()
                .expect("overflow must not allocate");
            assert!(err.to_string().contains("overflows usize"), "{err:#}");
            let err = BrainstemDaemon::try_new(cfg.clone())
                .err()
                .expect("overflow must not construct");
            assert!(err.to_string().contains("overflows usize"), "{err:#}");
        }
    }

    #[test]
    fn supported_runtime_boundaries_remain_valid() {
        use crate::daemon::BrainstemDaemon;

        for rate in [1, 1_000_000] {
            let body = base_config_toml("runtime_mode = \"simulation\"")
                .replace("tick_rate_hz = 1000", &format!("tick_rate_hz = {rate}"));
            let path = write_config_toml(&format!("runtime-valid-{rate}"), &body);
            let cfg = DaemonConfig::load(&path).unwrap();
            std::fs::remove_file(path).unwrap();
            let stats = BrainstemDaemon::try_new(cfg)
                .unwrap()
                .run_for_ticks(1)
                .unwrap();
            assert_eq!(stats.ticks, 1);
            assert_eq!(
                stats.loaded_checkpoint.unwrap().schema_id,
                crate::checkpoint::SIMULATION_SCHEMA_ID
            );
        }
        for (lif, izh, channels, payload) in [
            (65535, 0, 1, 4096),
            (0, 1, 1, 4096),
            (256, 0, 4096, 4096),
            (1, 0, 1048576, 1048576),
        ] {
            let mut cfg: DaemonConfig = toml::from_str(&base_config_toml("runtime_mode = \"simulation\"\ncontrol_bind = \"[::1]:0\"\nspine_pub_bind_host = \"[::1]\"" )).unwrap();
            cfg.lif_count = lif;
            cfg.izh_count = izh;
            cfg.channels = channels;
            cfg.ingress.max_payload_len = payload;
            assert!(BrainstemDaemon::try_new(cfg).is_ok());
        }
    }

    #[test]
    fn pub_bind_host_accepts_supported_hosts_and_rejects_malformed_endpoints() {
        for host in [
            "",
            "tcp://127.0.0.1",
            "127.0.0.1:5556",
            "[127.0.0.1]",
            "[::1",
            "::1]",
            "bad host",
            "-bad.example",
            "bad..example",
            "5555",
            "999.999.999.999",
            "pub.example.123",
        ] {
            let err = load_base_config("invalid-host", &format!("spine_pub_bind_host = {host:?}"))
                .expect_err("malformed PUB host must fail before socket startup");
            assert!(
                format!("{err:#}").contains("spine_pub_bind_host"),
                "{err:#}"
            );
        }
        for host in [
            "127.0.0.1",
            "0.0.0.0",
            "::1",
            "[::1]",
            "localhost",
            "pub.example.",
            "1.pub.example",
            "*",
        ] {
            load_base_config("valid-host", &format!("spine_pub_bind_host = {host:?}"))
                .expect("supported PUB host syntax");
        }
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
