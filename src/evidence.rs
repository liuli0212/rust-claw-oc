use crate::schema::EVIDENCE_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub schema_version: u32,
    pub evidence_id: String,
    pub source_kind: String,
    pub source_path: String,
    pub source_version: String,
    pub retrieved_at: u64,
    pub summary: String,
    pub content: String,
    pub mtime_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceFreshness {
    Fresh,
    RefreshRequired,
    Invalidated,
}

pub fn generic_evidence(
    evidence_id: String,
    source_kind: String,
    source_path: String,
    retrieved_at: u64,
    summary: String,
    content: String,
) -> Evidence {
    let source_version = format!("hash:{}", stable_hash(&content));
    Evidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        evidence_id,
        source_kind,
        source_path,
        source_version,
        retrieved_at,
        summary,
        content,
        mtime_unix: None,
    }
}

pub fn file_evidence(
    evidence_id: String,
    path: &Path,
    retrieved_at: u64,
    summary: String,
    content: String,
) -> std::io::Result<Evidence> {
    let meta = fs::metadata(path)?;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    let source_version = format!("hash:{}", stable_hash(&content));

    Ok(Evidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        evidence_id,
        source_kind: "file".to_string(),
        source_path: path.display().to_string(),
        source_version,
        retrieved_at,
        summary,
        content,
        mtime_unix,
    })
}

pub fn check_file_evidence_freshness(evidence: &Evidence) -> EvidenceFreshness {
    let path = Path::new(&evidence.source_path);
    let Ok(meta) = fs::metadata(path) else {
        return EvidenceFreshness::Invalidated;
    };

    let current_mtime = meta
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());

    if current_mtime != evidence.mtime_unix {
        return EvidenceFreshness::RefreshRequired;
    }

    match fs::read_to_string(path) {
        Ok(content) if format!("hash:{}", stable_hash(&content)) == evidence.source_version => {
            EvidenceFreshness::Fresh
        }
        Ok(_) => EvidenceFreshness::RefreshRequired,
        Err(_) => EvidenceFreshness::Invalidated,
    }
}

pub fn reconcile_evidence(evidence: &Evidence) -> (EvidenceFreshness, Option<Evidence>) {
    if evidence.source_kind != "file" {
        return (EvidenceFreshness::Fresh, Some(evidence.clone()));
    }

    match check_file_evidence_freshness(evidence) {
        EvidenceFreshness::Fresh => (EvidenceFreshness::Fresh, Some(evidence.clone())),
        EvidenceFreshness::RefreshRequired => {
            let path = Path::new(&evidence.source_path);
            match fs::read_to_string(path) {
                Ok(content) => match file_evidence(
                    evidence.evidence_id.clone(),
                    path,
                    evidence.retrieved_at,
                    evidence.summary.clone(),
                    content,
                ) {
                    Ok(refreshed) => (EvidenceFreshness::RefreshRequired, Some(refreshed)),
                    Err(_) => (EvidenceFreshness::Invalidated, None),
                },
                Err(_) => (EvidenceFreshness::Invalidated, None),
            }
        }
        EvidenceFreshness::Invalidated => (EvidenceFreshness::Invalidated, None),
    }
}

fn stable_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_file_evidence_freshness_changes_after_edit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        fs::write(&path, "hello").unwrap();

        let evidence = file_evidence(
            "ev_1".to_string(),
            &path,
            1,
            "sample".to_string(),
            "hello".to_string(),
        )
        .unwrap();

        assert_eq!(
            check_file_evidence_freshness(&evidence),
            EvidenceFreshness::Fresh
        );

        fs::write(&path, "changed").unwrap();
        assert_eq!(
            check_file_evidence_freshness(&evidence),
            EvidenceFreshness::RefreshRequired
        );
    }

    #[test]
    fn test_reconcile_evidence_refreshes_file_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        fs::write(&path, "hello").unwrap();

        let evidence = file_evidence(
            "ev_1".to_string(),
            &path,
            1,
            "sample".to_string(),
            "hello".to_string(),
        )
        .unwrap();

        fs::write(&path, "changed").unwrap();

        let (freshness, reconciled) = reconcile_evidence(&evidence);
        assert_eq!(freshness, EvidenceFreshness::RefreshRequired);
        assert_eq!(reconciled.unwrap().content, "changed");
    }
}
