use tempfile::TempDir;
use std::path::PathBuf;

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
