//! Logging configuration shared by the binary and isolated unit tests.

use anyhow::{Result, bail};
use tracing_subscriber::EnvFilter;

/// Result of resolving the process log filter.
pub struct ResolvedFilter {
    pub filter: EnvFilter,
    /// A single startup diagnostic to print before tracing is initialized.
    pub fallback_diagnostic: Option<String>,
}

/// Validate the deliberately small TOML `log_level` vocabulary.
pub fn validate_log_level(level: &str) -> Result<()> {
    match level {
        "error" | "warn" | "info" | "debug" | "trace" => Ok(()),
        _ => bail!("invalid log_level {level:?}; expected one of: error, warn, info, debug, trace"),
    }
}

/// Resolve `RUST_LOG`, which takes precedence over the validated TOML level.
///
/// An invalid environment filter falls back to TOML and returns exactly one
/// diagnostic for the caller to print. This prevents a typo from silently
/// disabling every log event.
pub fn resolve_filter(toml_level: &str, rust_log: Option<&str>) -> Result<ResolvedFilter> {
    validate_log_level(toml_level)?;
    let toml_filter = || EnvFilter::new(toml_level);

    match rust_log {
        Some(value) => match EnvFilter::try_new(value) {
            Ok(filter) => Ok(ResolvedFilter {
                filter,
                fallback_diagnostic: None,
            }),
            Err(err) => Ok(ResolvedFilter {
                filter: toml_filter(),
                fallback_diagnostic: Some(format!(
                    "Invalid RUST_LOG {value:?}: {err}; falling back to TOML log_level={toml_level:?}"
                )),
            }),
        },
        None => Ok(ResolvedFilter {
            filter: toml_filter(),
            fallback_diagnostic: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_all_supported_toml_levels() {
        for level in ["error", "warn", "info", "debug", "trace"] {
            assert!(resolve_filter(level, None).is_ok(), "rejected {level}");
        }
    }

    #[test]
    fn rejects_target_syntax_as_a_toml_level() {
        let err = validate_log_level("brainstem_daemon=debug").unwrap_err();
        assert!(err.to_string().contains("expected one of"));
    }

    #[test]
    fn valid_environment_filter_overrides_without_a_diagnostic() {
        let resolved = resolve_filter("error", Some("brainstem_daemon=debug,info")).unwrap();
        assert!(resolved.fallback_diagnostic.is_none());
    }

    #[test]
    fn invalid_environment_filter_falls_back_with_one_diagnostic() {
        let resolved = resolve_filter("debug", Some("[invalid")).unwrap();
        let diagnostic = resolved.fallback_diagnostic.unwrap();
        assert!(diagnostic.contains("Invalid RUST_LOG"));
        assert!(diagnostic.contains("falling back to TOML log_level=\"debug\""));
    }
}
