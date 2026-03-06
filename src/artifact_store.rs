use crate::schema::{new_artifact_id, new_run_id, ARTIFACT_SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub schema_version: u32,
    pub artifact_id: String,
    pub session_id: String,
    pub task_id: String,
    pub source_tool: String,
    pub content_type: String,
    pub semantic_type: String,
    pub path: String,
    pub summary: String,
    pub is_truncated: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupStats {
    pub deleted_runs: usize,
    pub deleted_bytes: u64,
}

pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn write_tool_artifact(
        &self,
        session_id: &str,
        task_id: &str,
        tool_name: &str,
        content: &str,
        summary: &str,
        is_truncated: bool,
    ) -> std::io::Result<ArtifactMetadata> {
        let run_id = new_run_id();
        let artifact_id = new_artifact_id();
        let run_dir = self.root.join(session_id).join(&run_id);
        fs::create_dir_all(&run_dir)?;

        let output_path = run_dir.join("output.txt");
        fs::write(&output_path, content)?;

        let metadata = ArtifactMetadata {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            artifact_id,
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
            source_tool: tool_name.to_string(),
            content_type: "text/plain".to_string(),
            semantic_type: "command-output".to_string(),
            path: output_path.display().to_string(),
            summary: summary.to_string(),
            is_truncated,
            created_at: unix_now(),
        };

        let metadata_path = run_dir.join("metadata.json");
        fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&metadata).map_err(invalid_data)?,
        )?;

        Ok(metadata)
    }

    pub fn cleanup(
        &self,
        now_unix: u64,
        max_age_secs: u64,
        max_bytes: u64,
    ) -> std::io::Result<CleanupStats> {
        let mut stats = CleanupStats::default();
        let mut runs = collect_runs(&self.root)?;
        runs.sort_by_key(|run| (run.is_expired(now_unix, max_age_secs), run.created_at));

        for run in runs
            .iter()
            .filter(|run| run.is_expired(now_unix, max_age_secs))
        {
            if fs::remove_dir_all(&run.path).is_ok() {
                stats.deleted_runs += 1;
                stats.deleted_bytes = stats.deleted_bytes.saturating_add(run.bytes);
            }
        }

        let mut remaining = collect_runs(&self.root)?;
        remaining.sort_by_key(|run| run.created_at);
        let mut total_bytes: u64 = remaining.iter().map(|run| run.bytes).sum();
        for run in remaining {
            if total_bytes <= max_bytes {
                break;
            }
            total_bytes = total_bytes.saturating_sub(run.bytes);
            if fs::remove_dir_all(&run.path).is_ok() {
                stats.deleted_runs += 1;
                stats.deleted_bytes = stats.deleted_bytes.saturating_add(run.bytes);
            }
        }

        Ok(stats)
    }

    #[allow(dead_code)]
    pub fn find_metadata(&self, artifact_id: &str) -> std::io::Result<Option<ArtifactMetadata>> {
        for run in collect_runs(&self.root)? {
            let metadata_path = run.path.join("metadata.json");
            let metadata: ArtifactMetadata =
                serde_json::from_str(&fs::read_to_string(&metadata_path)?).map_err(invalid_data)?;
            if metadata.artifact_id == artifact_id {
                return Ok(Some(metadata));
            }
        }
        Ok(None)
    }
}

#[derive(Debug)]
struct RunInfo {
    path: PathBuf,
    created_at: u64,
    bytes: u64,
}

impl RunInfo {
    fn is_expired(&self, now_unix: u64, max_age_secs: u64) -> bool {
        now_unix.saturating_sub(self.created_at) > max_age_secs
    }
}

fn collect_runs(root: &Path) -> std::io::Result<Vec<RunInfo>> {
    let mut runs = Vec::new();
    if !root.exists() {
        return Ok(runs);
    }

    for session_dir in fs::read_dir(root)? {
        let session_dir = session_dir?.path();
        if !session_dir.is_dir() {
            continue;
        }
        for run_dir in fs::read_dir(session_dir)? {
            let run_dir = run_dir?.path();
            if !run_dir.is_dir() {
                continue;
            }
            let metadata_path = run_dir.join("metadata.json");
            if !metadata_path.exists() {
                continue;
            }
            let metadata: ArtifactMetadata =
                serde_json::from_str(&fs::read_to_string(&metadata_path)?).map_err(invalid_data)?;
            runs.push(RunInfo {
                path: run_dir,
                created_at: metadata.created_at,
                bytes: dir_size(metadata_path.parent().unwrap_or(root))?,
            });
        }
    }

    Ok(runs)
}

fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

fn invalid_data(err: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_write_tool_artifact_persists_metadata() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().to_path_buf());
        let metadata = store
            .write_tool_artifact("cli", "task_1", "execute_bash", "output", "summary", false)
            .unwrap();

        assert!(Path::new(&metadata.path).exists());
        let metadata_path = Path::new(&metadata.path)
            .parent()
            .unwrap()
            .join("metadata.json");
        assert!(metadata_path.exists());
    }

    #[test]
    fn test_cleanup_removes_expired_runs() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().to_path_buf());
        let metadata = store
            .write_tool_artifact("cli", "task_1", "execute_bash", "output", "summary", false)
            .unwrap();
        let metadata_path = Path::new(&metadata.path)
            .parent()
            .unwrap()
            .join("metadata.json");
        let mut parsed: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&metadata_path).unwrap()).unwrap();
        parsed.created_at = 1;
        fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&parsed).unwrap(),
        )
        .unwrap();

        let stats = store.cleanup(10_000, 60, 10_000_000).unwrap();
        assert!(!metadata_path.exists());
        assert_eq!(stats.deleted_runs, 1);
    }

    #[test]
    fn test_find_metadata_by_artifact_id() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().to_path_buf());
        let metadata = store
            .write_tool_artifact("cli", "task_1", "execute_bash", "output", "summary", false)
            .unwrap();

        let found = store.find_metadata(&metadata.artifact_id).unwrap().unwrap();
        assert_eq!(found.artifact_id, metadata.artifact_id);
    }
}
