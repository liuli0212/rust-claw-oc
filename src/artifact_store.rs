use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::schema::StoragePaths;

/// Retention policy limits
const MAX_STORE_SIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
const MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60; // 7 Days

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub schema_version: u32,
    pub artifact_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub source_tool: String,
    pub content_type: String,
    pub semantic_type: String,
    pub path: String,
    pub summary: String,
    pub is_truncated: bool,
    pub created_at: u64,
}

pub struct ArtifactStore {
    session_id: String,
    run_id: String,
}

impl ArtifactStore {
    pub fn new(session_id: &str, run_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }
    }

    pub fn get_artifact_dir(&self) -> PathBuf {
        StoragePaths::artifacts_dir(&self.session_id, &self.run_id)
    }

    /// Generates a unique artifact ID for a run
    pub fn generate_id() -> String {
        format!("art_{}", uuid::Uuid::new_v4().simple())
    }

    /// Writes raw content to an artifact file and emits metadata
    pub async fn write_artifact(
        &self,
        artifact_id: &str,
        task_id: Option<String>,
        source_tool: &str,
        content_type: &str,
        semantic_type: &str,
        content: &[u8],
        summary: &str,
        is_truncated: bool,
    ) -> Result<ArtifactMetadata, std::io::Error> {
        let dir = self.get_artifact_dir();
        tokio::fs::create_dir_all(&dir).await?;

        // Format: `art_<uuid>.<ext>` but since we might just want to use the raw ID
        // let's just append `.data` or use the ID directly. We will use the Artifact ID as the filename for metadata
        // and add .bin or .txt according to content type (here we just use raw id as filename or `.raw`)
        let ext = if content_type.starts_with("text/") { "txt" } else { "bin" };
        let artifact_path = dir.join(format!("{}.{}", artifact_id, ext));
        let metadata_path = dir.join(format!("{}.meta.json", artifact_id));

        tokio::fs::write(&artifact_path, content).await?;

        let created_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let metadata = ArtifactMetadata {
            schema_version: crate::schema::CURRENT_SCHEMA_VERSION,
            artifact_id: artifact_id.to_string(),
            session_id: self.session_id.clone(),
            task_id,
            source_tool: source_tool.to_string(),
            content_type: content_type.to_string(),
            semantic_type: semantic_type.to_string(),
            path: artifact_path.to_string_lossy().to_string(),
            summary: summary.to_string(),
            is_truncated,
            created_at,
        };

        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        tokio::fs::write(&metadata_path, metadata_json).await?;

        // Fire & Forget: Trigger GC check asynchronously
        // For safe concurrency we don't await the cleanup in the critical path
        let session_id_clone = self.session_id.clone();
        tokio::spawn(async move {
            let _ = Self::enforce_retention_policy(&session_id_clone).await;
        });

        Ok(metadata)
    }

    pub fn get_metadata(session_id: &str, run_id: &str, artifact_id: &str) -> Option<ArtifactMetadata> {
        let dir = StoragePaths::artifacts_dir(session_id, run_id);
        let path = dir.join(format!("{}.meta.json", artifact_id));
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path) {
                return serde_json::from_str(&content).ok();
            }
        }
        None
    }

    /// Runs garbage collection over all runs for this session
    pub async fn enforce_retention_policy(session_id: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let root = PathBuf::from("rusty_claw").join("artifacts").join(session_id);
        if !root.exists() {
            return Ok(());
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut all_runs = Vec::new();

        // Pass 1: Gather runs and calculate age
        let mut read_dir = tokio::fs::read_dir(&root).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let stat = entry.metadata().await?;
                let mtime = stat.modified()?
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let age = if now > mtime { now - mtime } else { 0 };
                all_runs.push((entry.path(), mtime, age));
            }
        }

        // Pass 2: Expire runs older than MAX_AGE_SECS
        all_runs.retain(|(path, _mtime, age)| {
            if *age > MAX_AGE_SECS {
                let _ = std::fs::remove_dir_all(path);
                false
            } else {
                true
            }
        });

        // Pass 3: Calculate total size of remaining
        let mut total_size = 0u64;
        for (path, _, _) in &all_runs {
            total_size += fs_util_dir_size(path);
        }

        // Pass 4: Sort by oldest first and prune if over capacity
        if total_size > MAX_STORE_SIZE_BYTES {
            // Sort by mtime ascending (oldest first)
            all_runs.sort_by_key(|k| k.1);

            for (path, _, _) in all_runs {
                if total_size <= MAX_STORE_SIZE_BYTES {
                    break;
                }
                let size = fs_util_dir_size(&path);
                if let Ok(_) = std::fs::remove_dir_all(&path) {
                    total_size = total_size.saturating_sub(size);
                }
            }
        }

        Ok(())
    }
}

// Helper: recursively compute directory size (sync to avoid deep futures mapping, fine for local tool)
fn fs_util_dir_size(path: &PathBuf) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    total += fs_util_dir_size(&entry.path());
                } else {
                    total += meta.len();
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_artifact_writer() {
        let store = ArtifactStore::new("test_session", "run_1");
        let _ = tokio::fs::remove_dir_all(store.get_artifact_dir()).await; // clean
        
        // Write mock
        let md = store.write_artifact(
            "art_zzz",
            None,
            "bash",
            "text/plain",
            "command-output",
            b"Hello World Output",
            "Test output",
            false
        ).await.unwrap();

        assert_eq!(md.is_truncated, false);
        assert_eq!(md.content_type, "text/plain");

        // Read meta directly
        let read_md = ArtifactStore::get_metadata("test_session", "run_1", "art_zzz").unwrap();
        assert_eq!(read_md.semantic_type, "command-output");

        let content = std::fs::read_to_string(read_md.path).unwrap();
        assert_eq!(content, "Hello World Output");
    }
}
