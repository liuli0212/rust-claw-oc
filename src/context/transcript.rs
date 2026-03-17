use super::model::Turn;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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

pub fn load_turns(path: &Path) -> std::io::Result<Vec<Turn>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut turns = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(turn) = serde_json::from_str::<Turn>(trimmed) {
            turns.push(turn);
        }
    }
    Ok(turns)
}

pub fn append_turn(path: Option<&Path>, turn: &Turn) -> std::io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let serialized = serde_json::to_string(turn)?;
    writeln!(file, "{serialized}")?;
    Ok(())
}
