// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Live-mode start → input → tick/output → shutdown against the committed
//! sidecar fixture. Stub backend only; no `libzmq`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use brainstem_daemon::ingress::IngressConfig;
use brainstem_daemon::{
    BackendPair, BrainstemDaemon, DaemonConfig, IngressPacket, RuntimeMode, SpikeEvent, SpikeSink,
    StimulusSource,
};

const FIXTURE_SHA256: &str = "f97771291000a3d805375d410b46a7232b54b003aaa3f1d9662ea5f9b09af848";
const SIZE: usize = 16;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live/snn_model.json")
}

fn live_config() -> DaemonConfig {
    DaemonConfig {
        tick_rate_hz: 1000,
        log_level: "info".into(),
        spine_sub_port: 5555,
        spine_pub_port: 5556,
        spine_pub_bind_host: "127.0.0.1".into(),
        spine_pub_sndhwm: 1000,
        spine_pub_linger_ms: 0,
        spine_pub_send_empty_batches: true,
        model_path: fixture_path(),
        lif_count: SIZE,
        izh_count: 0,
        channels: SIZE,
        runtime_mode: RuntimeMode::Live,
        services: Vec::new(),
        ingress: IngressConfig::default(),
        control_bind: None,
    }
}

struct Drive {
    shut_down: Arc<Mutex<bool>>,
}

impl StimulusSource for Drive {
    fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
        let mut stimuli = vec![0.0; SIZE];
        stimuli[3] = 1.0;
        Ok(Some(IngressPacket {
            stimuli,
            ..IngressPacket::default()
        }))
    }

    fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        *self.shut_down.lock().unwrap() = true;
        Ok(())
    }
}

struct Capture {
    spikes: Arc<Mutex<Vec<SpikeEvent>>>,
    flushes: Arc<Mutex<u32>>,
}

impl SpikeSink for Capture {
    fn emit(&mut self, spikes: &[SpikeEvent], _batch_time: std::time::Duration) -> Result<()> {
        self.spikes.lock().unwrap().extend_from_slice(spikes);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }
}

#[test]
fn live_fixture_restores_ticks_and_shuts_down() {
    let daemon = BrainstemDaemon::try_new(live_config()).unwrap();
    let (_net, provenance) = daemon.restore_network().unwrap();
    assert_eq!(provenance.content_sha256, FIXTURE_SHA256);
    assert_eq!(provenance.model_id, "spikenaut-snn:live-fixture-v1");

    let spikes = Arc::new(Mutex::new(Vec::new()));
    let flushes = Arc::new(Mutex::new(0));
    let shut_down = Arc::new(Mutex::new(false));
    let pair = BackendPair {
        source: Box::new(Drive {
            shut_down: shut_down.clone(),
        }),
        sink: Box::new(Capture {
            spikes: spikes.clone(),
            flushes: flushes.clone(),
        }),
    };
    let stats = BrainstemDaemon::try_with_backend(live_config(), pair)
        .unwrap()
        .run_for_ticks(5)
        .unwrap();

    assert_eq!(stats.ticks, 5);
    assert_eq!(stats.rejected_batches, 0);
    assert_eq!(stats.emit_errors, 0);
    let loaded = stats.loaded_checkpoint.expect("checkpoint loaded");
    assert_eq!(loaded.content_sha256, FIXTURE_SHA256);
    let spikes = spikes.lock().unwrap();
    assert!(!spikes.is_empty(), "driving channel 3 must produce spikes");
    assert!(
        spikes.iter().all(|s| s.channel == 3),
        "only the stimulated neuron may spike"
    );
    assert_eq!(
        *flushes.lock().unwrap(),
        1,
        "sink must be flushed on shutdown"
    );
    assert!(*shut_down.lock().unwrap(), "source shutdown must run");
}
