// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Thalamic-side fixture for the Brainstem integration smoke test.
//!
//! This module must not depend on `neuromod` or own a `SpikingNetwork`.
//! It produces typed `corpus-ipc` sensory frames from simulated telemetry and
//! keeps a local safety/health flag independent of whether Brainstem is
//! reachable.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use brainstem_daemon::ingress::STIMULUS_SCHEMA;
use corpus_ipc::{BatchMetadata, IpcMessage, StimulusBatch, Validate};

/// Simulated Thalamic producer: sensory + safety only.
#[derive(Debug)]
pub struct ThalamicProducer {
    /// Deterministic hardware-safety path. Independent of IPC/Brainstem.
    pub safety_healthy: bool,
    pub published: u64,
    pub publish_errors: u64,
}

impl ThalamicProducer {
    pub fn new() -> Self {
        Self {
            safety_healthy: true,
            published: 0,
            publish_errors: 0,
        }
    }

    /// Build a representative normalized telemetry frame.
    pub fn simulate_telemetry(&self, batch_id: u64, channels: usize) -> StimulusBatch {
        let mut values = vec![0.0; channels];
        let mut valid_mask = vec![true; channels];
        if channels > 0 {
            values[0] = 1.0;
        }
        if channels > 1 {
            // Channel 1 is missing this tick: placeholder 0.0, mask false.
            values[1] = 0.0;
            valid_mask[1] = false;
        }
        let mut custom = HashMap::new();
        custom.insert("schema".into(), STIMULUS_SCHEMA.to_string());
        custom.insert("provenance".into(), "simulated-telemetry".into());
        StimulusBatch {
            session_id: Some("thalamic-smoke".into()),
            batch_id,
            timestamp: now_ns(),
            values,
            valid_mask: Some(valid_mask),
            metadata: Some(BatchMetadata {
                processing_latency_ns: Some(1_000),
                source: Some("thalamic-relay-fixture".into()),
                custom,
            }),
        }
    }

    /// Encode through the published `IpcMessage` contract (not a copied struct).
    pub fn encode_frame(&self, batch: &StimulusBatch) -> Result<Vec<u8>> {
        batch
            .validate()
            .map_err(|err| anyhow!("thalamic fixture produced an invalid StimulusBatch: {err}"))?;
        Ok(serde_json::to_vec(&IpcMessage::Stimuli(batch.clone()))?)
    }

    /// Attempt to publish. Publish failure never clears `safety_healthy`.
    pub fn publish(&mut self, transport: Option<&mut dyn StimulusTransport>, frame: &[u8]) {
        match transport {
            Some(tx) => match tx.send(frame) {
                Ok(()) => self.published += 1,
                Err(_) => self.publish_errors += 1,
            },
            None => {
                // Brainstem / transport unavailable.
                self.publish_errors += 1;
            }
        }
    }

    /// Evaluate a trivial independent protection predicate.
    pub fn safety_tick(&mut self, thermal_ok: bool) {
        self.safety_healthy = thermal_ok;
    }
}

impl Default for ThalamicProducer {
    fn default() -> Self {
        Self::new()
    }
}

/// Narrow send side used by the fixture. Production Thalamic would use ZMQ PUB.
pub trait StimulusTransport {
    fn send(&mut self, frame: &[u8]) -> Result<()>;
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}
