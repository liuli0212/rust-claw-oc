use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::schema::CURRENT_SCHEMA_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub schema_version: u32,
    pub evidence_id: String,
    pub source_kind: String, // "file", "web", "memory"
    pub source_path: String,
    pub source_version: Option<String>,
    pub retrieved_at: u64,
    pub score: f32,
    pub summary: String,
    pub content: String,
}

impl Evidence {
    pub fn new(
        evidence_id: String,
        source_kind: String,
        source_path: String,
        score: f32,
        summary: String,
        content: String,
    ) -> Self {
        let (source_version, retrieved_at) = Self::generate_freshness(&source_kind, &source_path);

        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            evidence_id,
            source_kind,
            source_path,
            source_version,
            retrieved_at,
            score,
            summary,
            content,
        }
    }

    /// Helper to get the freshness of a file source using hash + mtime.
    pub fn generate_freshness(source_kind: &str, source_path: &str) -> (Option<String>, u64) {
        let retrieved_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if source_kind == "file" {
            let path = PathBuf::from(source_path);
            if path.exists() {
                if let Ok(metadata) = fs::metadata(&path) {
                    if let Ok(mtime) = metadata.modified() {
                        let mtime_secs = mtime
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        // For v1 we can use size + mtime as a pseudo-hash to save heavy file reading,
                        // or we could hash the content. Let's start with a mtime + size based string.
                        let size = metadata.len();
                        let version = format!("v1-{}-{}", mtime_secs, size);
                        return (Some(version), retrieved_at);
                    }
                }
            }
        }
        (None, retrieved_at)
    }

    /// Check if this evidence is still fresh.
    /// Returns (is_fresh, replacement_tombstone)
    pub fn is_fresh(&self) -> (bool, Option<String>) {
        if self.source_kind == "file" {
            let (current_version, _) = Self::generate_freshness(&self.source_kind, &self.source_path);
            if current_version != self.source_version {
                let tombstone = format!(
                    "[Evidence '{}' automatically invalidated because the underlying file was modified since retrieval]",
                    self.source_path
                );
                return (false, Some(tombstone));
            }
        }
        (true, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_evidence_freshness() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file.txt");
        let path_str = file_path.to_str().unwrap().to_string();

        let mut file = fs::File::create(&file_path).unwrap();
        writeln!(file, "Initial content").unwrap();
        file.sync_all().unwrap();

        let evidence = Evidence::new(
            "ev1".to_string(),
            "file".to_string(),
            path_str.clone(),
            0.9,
            "A test file".to_string(),
            "Initial content".to_string(),
        );

        let (is_fresh, _) = evidence.is_fresh();
        assert!(is_fresh);

        // Sleep briefly to ensure mtime ticks past, though depending on OS resolution we might have to write a bigger difference
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let mut file = fs::OpenOptions::new().append(true).open(&file_path).unwrap();
        writeln!(file, "New line").unwrap();
        file.sync_all().unwrap();

        let (is_fresh, tombstone) = evidence.is_fresh();
        assert!(!is_fresh);
        assert!(tombstone.unwrap().contains("invalidated"));
    }
}
