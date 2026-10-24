// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Output frames received from the daemon PUB socket.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use corpus_ipc::IpcMessage;

/// Which part of the transport run a frame arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Subscribe,
    Prime,
    Measure,
    Tail,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscribe => "subscribe",
            Self::Prime => "prime",
            Self::Measure => "measure",
            Self::Tail => "tail",
        }
    }

    /// Frames that count toward the measurement (measured pulses and their tail).
    pub fn measured(self) -> bool {
        matches!(self, Self::Measure | Self::Tail)
    }
}

#[derive(Debug)]
pub struct Frame {
    pub phase: Phase,
    pub recv: Instant,
    pub channels: Vec<u16>,
    pub batch_id: u64,
    pub timestamp: u64,
}

impl Frame {
    pub fn measured_nonempty(&self) -> bool {
        self.phase.measured() && !self.channels.is_empty()
    }
}

fn parse_spikes(bytes: &[u8]) -> Result<(Vec<u16>, u64, u64)> {
    match serde_json::from_slice::<IpcMessage>(bytes).context("decode PUB frame")? {
        IpcMessage::Spikes(batch) => {
            let mut channels: Vec<u16> = batch.spikes.iter().map(|s| s.channel).collect();
            channels.sort_unstable();
            Ok((channels, batch.batch_id, batch.timestamp))
        }
        other => bail!("unexpected PUB frame variant: {other:?}"),
    }
}

/// One frame if any arrives before `deadline`, else `None`.
fn recv_until(sub: &zmq::Socket, deadline: Instant) -> Result<Option<Vec<u8>>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    let timeout = i64::try_from(remaining.as_millis().max(1))?;
    if sub.poll(zmq::POLLIN, timeout)? == 0 {
        return Ok(None);
    }
    Ok(Some(sub.recv_bytes(zmq::DONTWAIT)?))
}

/// Drain every frame available within `wait`, appending to `frames`.
pub fn collect_frames(
    sub: &zmq::Socket,
    wait: Duration,
    phase: Phase,
    frames: &mut Vec<Frame>,
) -> Result<()> {
    let deadline = Instant::now() + wait;
    while let Some(bytes) = recv_until(sub, deadline)? {
        let recv = Instant::now();
        let (channels, batch_id, timestamp) = parse_spikes(&bytes)?;
        frames.push(Frame {
            phase,
            recv,
            channels,
            batch_id,
            timestamp,
        });
    }
    Ok(())
}
