// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Real CLI regressions: configuration errors must win over checkpoint/socket errors.

use std::net::TcpListener;
use std::process::Command;

#[test]
fn invalid_configuration_exits_before_checkpoint_or_socket_startup() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/config-startup-tests")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let missing_checkpoint = dir.join("missing-snn_model.json");
    let base = format!(
        r#"
tick_rate_hz = 1000
log_level = "info"
spine_sub_port = 5555
spine_pub_port = {port}
control_bind = "127.0.0.1:{port}"
model_path = '{}'
lif_count = 1
izh_count = 0
channels = 1
"#,
        missing_checkpoint.display()
    );
    for (index, (old, new, field)) in [
        ("tick_rate_hz = 1000", "tick_rate_hz = 0", "tick_rate_hz"),
        ("channels = 1", "channels = 0", "channels"),
        ("channels = 1", "channels = 1048577", "dimensions"),
        ("lif_count = 1", "lif_count = 65536", "lif_count"),
        ("izh_count = 0", "izh_count = 1", "izh_count"),
        (
            "control_bind = \"127.0.0.1:",
            "control_bind = \"localhost:",
            "control_bind",
        ),
        (
            "channels = 1",
            "channels = 1\nspine_pub_bind_host = 'tcp://127.0.0.1'",
            "spine_pub_bind_host",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.join(format!("invalid-{index}.toml"));
        std::fs::write(&path, base.replace(old, new)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_brainstem-daemon"))
            .arg("--config")
            .arg(&path)
            .env_remove("RUST_LOG")
            .output()
            .unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(field), "expected {field}: {stderr}");
        assert!(!stderr.contains("checkpoint not found"), "{stderr}");
        assert!(!stderr.contains("failed to bind"), "{stderr}");
        assert!(output.stdout.is_empty(), "runtime logging must not start");
    }
    std::fs::remove_dir(dir).unwrap();
}
