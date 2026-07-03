// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Config-driven service registry for the brainstem daemon.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a single logical service that the daemon may interact with.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ServiceConfig {
    /// Human-readable service name. Trading/mining-specific names are
    /// intentionally not part of the default configuration.
    pub name: String,
    /// Whether the service is enabled in this daemon instance.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl ServiceConfig {
    /// Create a new service config with the given name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            enabled: true,
        }
    }
}

/// Registry of named services keyed by service name.
#[derive(Debug, Clone, Default)]
pub struct ServiceRegistry {
    services: HashMap<String, ServiceConfig>,
}

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
        }
    }

    /// Build a registry from a list of service configs.
    ///
    /// Entries are processed in order:
    /// - If enabled, the service is registered (last enabled wins for duplicates).
    /// - If disabled, any prior registration for that name is removed.
    ///
    /// This allows a later `enabled: false` to revoke an earlier `enabled: true`.
    pub fn from_configs(configs: Vec<ServiceConfig>) -> Self {
        let mut services = HashMap::with_capacity(configs.len());
        for cfg in configs {
            if cfg.enabled {
                services.insert(cfg.name.clone(), cfg);
            } else {
                services.remove(&cfg.name);
            }
        }
        Self { services }
    }

    /// Look up a service by name.
    pub fn get(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.get(name)
    }

    /// Iterate over all registered services.
    pub fn iter(&self) -> impl Iterator<Item = &ServiceConfig> {
        self.services.values()
    }

    /// Return the number of enabled services in the registry.
    pub fn len(&self) -> usize {
        self.services.len()
    }

    /// Return true when the registry contains no services.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Return true if a service with the given name is registered and enabled.
    pub fn contains(&self, name: &str) -> bool {
        self.services.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry() {
        let registry = ServiceRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(!registry.contains("telemetry"));
    }

    #[test]
    fn from_configs_ignores_disabled() {
        let configs = vec![
            ServiceConfig::named("telemetry"),
            ServiceConfig {
                name: "mining-adapter".to_string(),
                enabled: false,
            },
        ];
        let registry = ServiceRegistry::from_configs(configs);
        assert_eq!(registry.len(), 1);
        assert!(registry.contains("telemetry"));
        assert!(!registry.contains("mining-adapter"));
    }

    #[test]
    fn lookup_returns_service() {
        let registry = ServiceRegistry::from_configs(vec![
            ServiceConfig::named("telemetry"),
            ServiceConfig::named("critic-ipc"),
        ]);
        assert!(registry.get("telemetry").is_some());
        assert!(registry.get("critic-ipc").is_some());
        assert!(registry.get("unknown").is_none());
    }

    #[test]
    fn duplicate_names_last_enabled_wins() {
        let configs = vec![
            ServiceConfig {
                name: "telemetry".to_string(),
                enabled: false,
            },
            ServiceConfig::named("telemetry"),
        ];
        let registry = ServiceRegistry::from_configs(configs);
        assert_eq!(registry.len(), 1);
        assert!(registry.get("telemetry").unwrap().enabled);
    }

    #[test]
    fn duplicate_enabled_then_disabled_is_removed() {
        let configs = vec![
            ServiceConfig::named("telemetry"),
            ServiceConfig {
                name: "telemetry".to_string(),
                enabled: false,
            },
        ];
        let registry = ServiceRegistry::from_configs(configs);
        assert!(!registry.contains("telemetry"));
        assert_eq!(registry.len(), 0);
    }
}
