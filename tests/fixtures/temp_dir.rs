// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Per-test scratch directory shared by the integration tests.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique scratch directory that is removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        // Cargo sets CARGO_TARGET_TMPDIR for tests; fall back to this crate's
        // target/ so fixtures never use the shared system temp directory.
        let root = std::env::var_os("CARGO_TARGET_TMPDIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
            PathBuf::from,
        );
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = root.join(format!("{prefix}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
