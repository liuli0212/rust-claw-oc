use std::path::{Path, PathBuf};

pub fn transcript_path_for_session(base_dir: &Path, session_id: &str) -> PathBuf {
    let sanitized = session_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect::<String>();
    base_dir.join(format!("{sanitized}.jsonl"))
}
