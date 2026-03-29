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
