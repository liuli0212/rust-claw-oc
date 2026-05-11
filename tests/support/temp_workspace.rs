#![allow(dead_code)]

use std::path::PathBuf;
use tempfile::TempDir;

pub struct TempWorkspace {
    pub dir: TempDir,
}

impl TempWorkspace {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("Failed to create temp dir"),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
}

pub fn cleanup_session(session_id: &str) {
    let _ = std::fs::remove_dir_all(rusty_claw::schema::StoragePaths::session_dir(session_id));
}

pub fn cleanup_sessions_with_prefix(prefix: &str) {
    let base = std::path::PathBuf::from("rusty_claw").join("sessions");
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let matches_prefix = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with(prefix))
            .unwrap_or(false);
        if matches_prefix {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
