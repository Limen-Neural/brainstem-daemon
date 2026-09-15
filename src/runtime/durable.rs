// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Durable session state that survives a simulated process restart.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Schema version for the harness checkpoint blob.
pub const DURABLE_SCHEMA_VERSION: u32 = 1;

/// Identity of the fake-core checkpoint used by the harness.
pub const FAKE_CHECKPOINT_ID: &str = "fake-core-v1";

const MAGIC: &[u8] = b"BSDK1\n";

/// Work that was accepted but not yet published when the process died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InflightRecord {
    pub session_id: u64,
    pub tick_seq: u64,
    pub ingress_seq: u64,
}

/// State that is allowed to survive process restart.
///
/// Everything else (open channels, spike buffers, live metrics, core
/// membrane potentials, in-flight publish buffers) is volatile and must
/// reset when the next session boots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableState {
    pub schema_version: u32,
    pub checkpoint_id: String,
    /// Last session that passed checkpoint validation. `0` means no session.
    pub last_session_id: u64,
    /// Last tick whose output was successfully published.
    pub committed_tick_seq: u64,
    /// Last ingress sequence that was committed as fresh work.
    pub committed_ingress_seq: u64,
    /// Present iff a tick started and did not finish publishing.
    pub inflight: Option<InflightRecord>,
}

impl DurableState {
    /// Empty durable state used for a first boot with a valid fake checkpoint.
    pub fn fresh() -> Self {
        Self {
            schema_version: DURABLE_SCHEMA_VERSION,
            checkpoint_id: FAKE_CHECKPOINT_ID.to_string(),
            last_session_id: 0,
            committed_tick_seq: 0,
            committed_ingress_seq: 0,
            inflight: None,
        }
    }

    /// Return an error if this blob must not enter the live tick loop.
    pub fn validate(&self) -> Result<()> {
        self.validate_identity()?;
        self.validate_session()?;
        self.validate_inflight()
    }

    fn validate_identity(&self) -> Result<()> {
        if self.schema_version != DURABLE_SCHEMA_VERSION {
            bail!(
                "unsupported durable schema {} (expected {DURABLE_SCHEMA_VERSION})",
                self.schema_version
            );
        }
        if self.checkpoint_id.is_empty()
            || self.checkpoint_id.chars().any(char::is_control)
            || self.checkpoint_id != FAKE_CHECKPOINT_ID
        {
            bail!("invalid checkpoint id {:?}", self.checkpoint_id);
        }
        Ok(())
    }

    fn validate_session(&self) -> Result<()> {
        if self.last_session_id == 0
            && (self.committed_tick_seq != 0
                || self.committed_ingress_seq != 0
                || self.inflight.is_some())
        {
            bail!("session 0 cannot hold committed or inflight work");
        }
        Ok(())
    }

    fn validate_inflight(&self) -> Result<()> {
        let Some(inf) = &self.inflight else {
            return Ok(());
        };
        let next = self
            .committed_tick_seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("committed tick sequence overflow"))?;
        if inf.tick_seq != next {
            bail!(
                "inflight tick {} is not committed+1 ({})",
                inf.tick_seq,
                self.committed_tick_seq
            );
        }
        if inf.session_id == 0 || inf.session_id > self.last_session_id {
            bail!(
                "inflight session {} is out of range for last_session_id {}",
                inf.session_id,
                self.last_session_id
            );
        }
        if inf.ingress_seq == 0 {
            bail!("inflight ingress seq must be > 0");
        }
        Ok(())
    }
}

/// In-memory stand-in for a process-local checkpoint file.
///
/// The blob is the only thing a "restart" keeps. Tests never touch the
/// filesystem so the suite stays CPU-only.
#[derive(Clone, Debug, Default)]
pub struct DurableStore {
    blob: Option<Vec<u8>>,
}

impl DurableStore {
    /// No prior session.
    pub fn empty() -> Self {
        Self { blob: None }
    }

    /// Bytes that cannot pass checkpoint validation.
    pub fn malformed(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            blob: Some(bytes.into()),
        }
    }

    /// Persist an already-validated state (used to seed recovery tests).
    pub fn from_state(state: &DurableState) -> Result<Self> {
        state.validate()?;
        let mut store = Self::empty();
        store.persist(state)?;
        Ok(store)
    }

    /// Load and validate. `Ok(None)` means first boot.
    pub fn load(&self) -> Result<Option<DurableState>> {
        match &self.blob {
            None => Ok(None),
            Some(bytes) => Ok(Some(decode(bytes)?)),
        }
    }

    /// Replace the durable blob. Callers must validate before persist.
    pub fn persist(&mut self, state: &DurableState) -> Result<()> {
        state.validate()?;
        self.blob = Some(encode(state)?);
        Ok(())
    }

    /// Raw blob retained across a simulated crash.
    pub fn blob(&self) -> Option<&[u8]> {
        self.blob.as_deref()
    }
}

fn encode(state: &DurableState) -> Result<Vec<u8>> {
    let mut out = MAGIC.to_vec();
    out.extend(serde_json::to_vec(state).context("serialize durable state")?);
    Ok(out)
}

fn decode(bytes: &[u8]) -> Result<DurableState> {
    let json = bytes
        .strip_prefix(MAGIC)
        .ok_or_else(|| anyhow::anyhow!("malformed checkpoint: missing magic"))?;
    let state: DurableState = serde_json::from_slice(json).context("malformed checkpoint: json")?;
    state.validate()?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_is_first_boot() {
        let store = DurableStore::empty();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn round_trip_valid_state() {
        let mut state = DurableState::fresh();
        state.last_session_id = 1;
        let store = DurableStore::from_state(&state).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), state);
    }

    #[test]
    fn malformed_bytes_fail_before_any_state() {
        let store = DurableStore::malformed(b"{not-json");
        let err = store.load().unwrap_err().to_string();
        assert!(
            err.contains("malformed checkpoint"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn inflight_must_be_exactly_next_tick() {
        let mut state = DurableState::fresh();
        state.last_session_id = 1;
        state.committed_tick_seq = 3;
        state.inflight = Some(InflightRecord {
            session_id: 1,
            tick_seq: 3,
            ingress_seq: 1,
        });
        let err = state.validate().unwrap_err().to_string();
        assert!(err.contains("committed+1"), "unexpected error: {err}");
    }

    #[test]
    fn inflight_at_max_committed_tick_is_rejected_without_overflow() {
        let mut state = DurableState::fresh();
        state.last_session_id = 1;
        state.committed_tick_seq = u64::MAX;
        state.inflight = Some(InflightRecord {
            session_id: 1,
            tick_seq: 0,
            ingress_seq: 1,
        });
        let err = state.validate().unwrap_err().to_string();
        assert!(err.contains("overflow"), "unexpected error: {err}");
    }
}
