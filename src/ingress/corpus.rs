// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Typed `corpus-ipc` ingress validation.
//!
//! Brainstem consumes published `IpcMessage` payloads. It does not copy the
//! wire structs: decoding goes through `corpus_ipc::{IpcMessage, StimulusBatch}`.

use std::time::Duration;

use crate::backend::IngressPacket;
use corpus_ipc::{IpcMessage, StimulusBatch, Validate};

/// Schema token Brainstem requires in `StimulusBatch.metadata.custom["schema"]`.
pub const STIMULUS_SCHEMA: &str = "corpus-ipc.stimulus.v1";

/// Ingress acceptance policy.
#[derive(Debug, Clone)]
pub struct IngressPolicy {
    /// Expected stimulus width. `0` means unspecified: width is not checked.
    pub expected_channels: usize,
    pub max_age: Option<Duration>,
}

impl IngressPolicy {
    /// Policy for a configured channel width and optional freshness window.
    ///
    /// Pass `expected_channels = 0` to accept any width (unspecified-width mode).
    pub fn new(expected_channels: usize, max_age: Option<Duration>) -> Self {
        Self {
            expected_channels,
            max_age,
        }
    }
}

/// Why a typed ingress payload was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressError {
    Deserialize(String),
    UnexpectedVariant(&'static str),
    Width { expected: usize, got: usize },
    Stale { age_ns: u64, max_age_ns: u64 },
    Future { timestamp_ns: u64, now_ns: u64 },
    Schema(String),
    Stimulus(String),
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deserialize(msg) => write!(f, "IPC schema deserialize error: {msg}"),
            Self::UnexpectedVariant(name) => {
                write!(
                    f,
                    "unexpected IpcMessage variant on stimulus ingress: {name}"
                )
            }
            Self::Width { expected, got } => {
                write!(
                    f,
                    "stimulus width {got} does not match configured channels {expected}"
                )
            }
            Self::Stale { age_ns, max_age_ns } => {
                write!(
                    f,
                    "stimulus is stale: age {age_ns} ns exceeds {max_age_ns} ns"
                )
            }
            Self::Future {
                timestamp_ns,
                now_ns,
            } => {
                write!(
                    f,
                    "stimulus timestamp {timestamp_ns} is in the future (now {now_ns})"
                )
            }
            Self::Schema(msg) => write!(f, "stimulus schema incompatibility: {msg}"),
            Self::Stimulus(msg) => write!(f, "stimulus batch invalid: {msg}"),
        }
    }
}

impl std::error::Error for IngressError {}

/// Decode and validate a JSON `IpcMessage` from the corpus-ipc wire contract.
pub fn accept_ipc_json(
    bytes: &[u8],
    policy: &IngressPolicy,
    now_ns: u64,
) -> Result<IngressPacket, IngressError> {
    let message: IpcMessage =
        serde_json::from_slice(bytes).map_err(|err| IngressError::Deserialize(err.to_string()))?;
    accept_ipc_message(message, policy, now_ns)
}

/// Validate an already-decoded `IpcMessage`.
pub fn accept_ipc_message(
    message: IpcMessage,
    policy: &IngressPolicy,
    now_ns: u64,
) -> Result<IngressPacket, IngressError> {
    match message {
        IpcMessage::Stimuli(batch) => accept_stimulus_batch(batch, policy, now_ns),
        IpcMessage::Neuromodulators(snapshot) => Ok(IngressPacket {
            stimuli: Vec::new(),
            // Positional map from corpus-ipc's DA/cortisol/ACh/tempo snapshot
            // onto neuromod 0.6's DA/5-HT/ACh/NE slots.
            modulators: Some(vec![
                snapshot.dopamine,
                snapshot.cortisol,
                snapshot.acetylcholine,
                snapshot.tempo,
            ]),
            ..IngressPacket::default()
        }),
        IpcMessage::Spikes(_) => Err(IngressError::UnexpectedVariant("Spikes")),
        IpcMessage::Embeddings(_) => Err(IngressError::UnexpectedVariant("Embeddings")),
        IpcMessage::Loss(_) => Err(IngressError::UnexpectedVariant("Loss")),
        IpcMessage::ConfigUpdate(_) => Err(IngressError::UnexpectedVariant("ConfigUpdate")),
        IpcMessage::GradientUpdate(_) => Err(IngressError::UnexpectedVariant("GradientUpdate")),
        IpcMessage::EligibilityTraces(_) => {
            Err(IngressError::UnexpectedVariant("EligibilityTraces"))
        }
        IpcMessage::TrainingComplete => Err(IngressError::UnexpectedVariant("TrainingComplete")),
        IpcMessage::Shutdown => Err(IngressError::UnexpectedVariant("Shutdown")),
        IpcMessage::Ping => Err(IngressError::UnexpectedVariant("Ping")),
    }
}

fn accept_stimulus_batch(
    batch: StimulusBatch,
    policy: &IngressPolicy,
    now_ns: u64,
) -> Result<IngressPacket, IngressError> {
    batch
        .validate()
        .map_err(|err| IngressError::Stimulus(err.to_string()))?;

    match batch.metadata.as_ref() {
        Some(meta) => match meta.custom.get("schema") {
            Some(schema) if schema == STIMULUS_SCHEMA => {}
            Some(schema) => {
                return Err(IngressError::Schema(format!(
                    "unsupported schema '{schema}', expected {STIMULUS_SCHEMA}"
                )));
            }
            None => {
                return Err(IngressError::Schema(
                    "missing metadata.custom.schema".into(),
                ));
            }
        },
        None => {
            return Err(IngressError::Schema("missing batch metadata".into()));
        }
    }

    if policy.expected_channels != 0 && batch.values.len() != policy.expected_channels {
        return Err(IngressError::Width {
            expected: policy.expected_channels,
            got: batch.values.len(),
        });
    }

    if let Some(max_age) = policy.max_age {
        if batch.timestamp > now_ns {
            return Err(IngressError::Future {
                timestamp_ns: batch.timestamp,
                now_ns,
            });
        }
        let max_age_ns = max_age.as_nanos() as u64;
        let age_ns = now_ns - batch.timestamp;
        if age_ns > max_age_ns {
            return Err(IngressError::Stale { age_ns, max_age_ns });
        }
    }

    // Do not zero masked slots here. `valid_mask` is preserved so downstream
    // consumers can tell a producer 0.0 from a masked placeholder; the tick
    // loop applies the mask once in `decode_inputs`.
    Ok(IngressPacket {
        stimuli: batch.values,
        modulators: None,
        valid_mask: batch.valid_mask,
        batch_id: Some(batch.batch_id),
        timestamp_ns: Some(batch.timestamp),
        session_id: batch.session_id,
        rejected: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use corpus_ipc::BatchMetadata;
    use std::collections::HashMap;

    fn sample_batch() -> StimulusBatch {
        let mut custom = HashMap::new();
        custom.insert("schema".into(), STIMULUS_SCHEMA.into());
        StimulusBatch {
            session_id: Some("smoke".into()),
            batch_id: 7,
            timestamp: 1_000,
            values: vec![1.0, 0.0, 0.25, 0.5],
            valid_mask: Some(vec![true, false, true, true]),
            metadata: Some(BatchMetadata {
                processing_latency_ns: None,
                source: Some("thalamic-relay-fixture".into()),
                custom,
            }),
        }
    }

    fn policy() -> IngressPolicy {
        IngressPolicy::new(4, Some(Duration::from_secs(1)))
    }

    #[test]
    fn valid_mask_survives_json_round_trip() {
        let message = IpcMessage::Stimuli(sample_batch());
        let bytes = serde_json::to_vec(&message).unwrap();
        let packet = accept_ipc_json(&bytes, &policy(), 1_000).unwrap();
        assert_eq!(packet.valid_mask, Some(vec![true, false, true, true]));
        // Masked channel 1 keeps the producer placeholder; decode_inputs zeros it.
        assert_eq!(packet.stimuli, vec![1.0, 0.0, 0.25, 0.5]);
        assert_eq!(packet.batch_id, Some(7));
        assert_eq!(packet.session_id.as_deref(), Some("smoke"));
    }

    #[test]
    fn width_mismatch_fails() {
        let mut batch = sample_batch();
        batch.values.push(0.1);
        batch.valid_mask = Some(vec![true, false, true, true, true]);
        let err = accept_ipc_message(IpcMessage::Stimuli(batch), &policy(), 1_000).unwrap_err();
        assert!(matches!(
            err,
            IngressError::Width {
                expected: 4,
                got: 5
            }
        ));
    }

    #[test]
    fn unspecified_width_accepts_any_channel_count() {
        let packet = accept_ipc_message(
            IpcMessage::Stimuli(sample_batch()),
            &IngressPolicy::new(0, Some(Duration::from_secs(1))),
            1_000,
        )
        .unwrap();
        assert_eq!(packet.stimuli.len(), 4);
    }

    #[test]
    fn stale_batch_fails() {
        let mut batch = sample_batch();
        batch.timestamp = 0;
        let err = accept_ipc_message(
            IpcMessage::Stimuli(batch),
            &policy(),
            Duration::from_secs(2).as_nanos() as u64,
        )
        .unwrap_err();
        assert!(matches!(err, IngressError::Stale { .. }));
    }

    #[test]
    fn future_timestamp_fails() {
        let mut batch = sample_batch();
        batch.timestamp = 5_000;
        let err = accept_ipc_message(IpcMessage::Stimuli(batch), &policy(), 1_000).unwrap_err();
        assert!(matches!(
            err,
            IngressError::Future {
                timestamp_ns: 5_000,
                now_ns: 1_000
            }
        ));
    }

    #[test]
    fn unknown_schema_fails_loudly() {
        let mut batch = sample_batch();
        batch
            .metadata
            .as_mut()
            .unwrap()
            .custom
            .insert("schema".into(), "corpus-ipc.stimulus.v0".into());
        let err = accept_ipc_message(IpcMessage::Stimuli(batch), &policy(), 1_000).unwrap_err();
        match err {
            IngressError::Schema(msg) => assert!(msg.contains("unsupported schema")),
            other => panic!("expected schema error, got {other}"),
        }
    }

    #[test]
    fn copied_udp_json_is_not_ipc_message() {
        let bytes = br#"{"type":"Stimuli","values":[0.1,0.2]}"#;
        let err = accept_ipc_json(bytes, &policy(), 1_000).unwrap_err();
        assert!(matches!(err, IngressError::Deserialize(_)));
    }

    #[test]
    fn unexpected_variant_fails() {
        let err = accept_ipc_message(IpcMessage::Ping, &policy(), 1_000).unwrap_err();
        assert_eq!(err, IngressError::UnexpectedVariant("Ping"));
    }
}
