// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! CPU-only stand-ins for the network core and scripted ingress.

use std::collections::VecDeque;

use anyhow::Result;

use super::SequencedIngress;

/// CPU-only stand-in for `neuromod::SpikingNetwork`.
///
/// Membrane / last-stimuli fields are volatile: a restarted harness gets a
/// fresh core even when the durable store is reused.
#[derive(Debug, Clone, Default)]
pub struct FakeCore {
    pub steps: u64,
    last_stimuli: Option<Vec<f32>>,
}

impl FakeCore {
    pub fn last_stimuli(&self) -> Option<&[f32]> {
        self.last_stimuli.as_deref()
    }

    pub(crate) fn step(&mut self, stimuli: &[f32]) -> Result<Vec<u16>> {
        self.steps += 1;
        self.last_stimuli = Some(stimuli.to_vec());
        Ok(stimuli
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 0.5)
            .filter_map(|(i, _)| u16::try_from(i).ok())
            .collect())
    }
}

/// Scripted ingress. No sockets.
#[derive(Debug, Clone, Default)]
pub struct ScriptedSource {
    packets: VecDeque<SequencedIngress>,
}

impl ScriptedSource {
    pub fn new(packets: impl IntoIterator<Item = SequencedIngress>) -> Self {
        Self {
            packets: packets.into_iter().collect(),
        }
    }

    pub fn push(&mut self, packet: SequencedIngress) {
        self.packets.push_back(packet);
    }

    pub(crate) fn push_front(&mut self, packet: SequencedIngress) {
        self.packets.push_front(packet);
    }

    pub(crate) fn next(&mut self) -> Option<SequencedIngress> {
        self.packets.pop_front()
    }
}

/// One successfully published tick. Appended only after metric publication.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedOutput {
    pub session_id: u64,
    pub tick_seq: u64,
    pub ingress_seq: u64,
    pub spike_ids: Vec<u16>,
    pub time_ns: u64,
}

/// Metric publication that is allowed only for committed ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricEvent {
    pub session_id: u64,
    pub tick_seq: u64,
    pub ingress_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickResult {
    Committed,
    SkippedReplay,
    NoIngress,
}

/// Scripted packets for a seed. Stimuli (and therefore fake-core spikes)
/// vary, but sequence numbers stay `1..=8`.
pub fn packets_for_seed(seed: u64) -> Vec<SequencedIngress> {
    (1..=8)
        .map(|seq| SequencedIngress {
            seq,
            stimuli: stimuli_for(seed, seq),
        })
        .collect()
}

fn stimuli_for(seed: u64, seq: u64) -> Vec<f32> {
    let v = ((seed.wrapping_add(seq.wrapping_mul(17))) % 10) as f32 / 10.0;
    vec![v, 1.0 - v]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_core_drops_out_of_range_spike_ids() {
        let mut core = FakeCore::default();
        let mut stimuli = vec![0.0; (u16::MAX as usize) + 2];
        stimuli[0] = 0.9;
        stimuli[u16::MAX as usize + 1] = 0.9;
        let spikes = core.step(&stimuli).unwrap();
        assert_eq!(spikes, vec![0]);
    }
}
