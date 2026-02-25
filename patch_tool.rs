use std::fs;
use std::path::{Path, PathBuf};

/// Supported top-level patch operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    Update,
    Add,
    Delete,
}

/// A single line inside a hunk.
///
/// We keep explicit variants so apply logic can reason about:
/// - what must already exist (`Context`, `Remove`)
/// - what should be created (`Context`, `Add`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

/// A hunk is a sequence of typed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Vec<HunkLine>,
}

/// Parsed patch for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub op: PatchOp,
    pub file_path: String,
    pub hunks: Vec<Hunk>,
    pub add_file_lines: Vec<String>,
}

/// Result of applying a patch group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub changed_paths: Vec<String>,
}

pub struct PatchEngine;

impl PatchEngine {
    /// Parse patch text into strongly-typed operations.
    ///
    /// Supported syntax:
    /// - `*** Begin Patch` (and tolerant to `*** Begin Patch ***`)
    /// - `*** Update File: path`
    /// - `*** Add File: path` with body lines prefixed by `+`
    /// - `*** Delete File: path`
    /// - hunk blocks starting with `@@`, using line prefixes:
    ///   - ` ` (context), `-` (remove), `+` (add)
    /// - `*** End Patch` (and tolerant to `*** End Patch ***`)
    pub fn parse_patch(input: &str) -> Result<Vec<Patch>, String> {
        let lines: Vec<&str> = input.lines().collect();
        let mut idx = 0usize;
        let mut patches = Vec::new();

        while idx < lines.len() {
            if Self::is_begin_marker(lines[idx]) {
                idx += 1;
                while idx < lines.len() && !Self::is_end_marker(lines[idx]) {
                    let line = lines[idx].trim();
                    if let Some(path) = line.strip_prefix("*** Update File:") {
                        let file_path = path.trim().to_string();
                        if file_path.is_empty() {
                            return Err(format!("line {}: empty update path", idx + 1));
                        }
                        idx += 1;
                        let mut hunks = Vec::new();
                        while idx < lines.len() {
                            let current = lines[idx].trim();
                            if current.starts_with("*** ") || Self::is_end_marker(lines[idx]) {
                                break;
                            }
                            if !current.starts_with("@@") {
                                return Err(format!(
                                    "line {}: expected hunk header '@@' for update '{}'",
                                    idx + 1,
                                    file_path
                                ));
                            }
                            let (hunk, next_idx) = Self::parse_hunk(&lines, idx)?;
                            hunks.push(hunk);
                            idx = next_idx;
                        }
                        if hunks.is_empty() {
                            return Err(format!(
                                "update '{}' must contain at least one hunk",
                                file_path
                            ));
                        }
                        patches.push(Patch {
                            op: PatchOp::Update,
                            file_path,
                            hunks,
                            add_file_lines: Vec::new(),
                        });
                        continue;
                    }

                    if let Some(path) = line.strip_prefix("*** Add File:") {
                        let file_path = path.trim().to_string();
                        if file_path.is_empty() {
                            return Err(format!("line {}: empty add path", idx + 1));
                        }
                        idx += 1;
                        let mut add_lines = Vec::new();
                        while idx < lines.len() {
                            let current = lines[idx];
                            let trimmed = current.trim();
                            if trimmed.starts_with("*** ") || Self::is_end_marker(current) {
                                break;
                            }
                            if !current.starts_with('+') {
                                return Err(format!(
                                    "line {}: add file '{}' requires '+' prefixed lines",
                                    idx + 1,
                                    file_path
                                ));
                            }
                            add_lines.push(current[1..].to_string());
                            idx += 1;
                        }
                        patches.push(Patch {
                            op: PatchOp::Add,
                            file_path,
                            hunks: Vec::new(),
                            add_file_lines: add_lines,
                        });
                        continue;
                    }

                    if let Some(path) = line.strip_prefix("*** Delete File:") {
                        let file_path = path.trim().to_string();
                        if file_path.is_empty() {
                            return Err(format!("line {}: empty delete path", idx + 1));
                        }
                        patches.push(Patch {
                            op: PatchOp::Delete,
                            file_path,
                            hunks: Vec::new(),
                            add_file_lines: Vec::new(),
                        });
                        idx += 1;
                        continue;
                    }

                    return Err(format!("line {}: unexpected patch directive '{}'", idx + 1, line));
                }

                if idx >= lines.len() || !Self::is_end_marker(lines[idx]) {
                    return Err("missing '*** End Patch'".to_string());
                }
            }
            idx += 1;
        }

        if patches.is_empty() {
            return Err("no patch operations found".to_string());
        }
        Ok(patches)
    }

    /// Apply parsed patches against a workspace root.
    ///
    /// Safety guarantees:
    /// - path traversal outside root is rejected
    /// - update hunks must match uniquely (prevents accidental wrong edits)
    pub fn apply_patches(root: &Path, patches: &[Patch]) -> Result<ApplyResult, String> {
        let root = fs::canonicalize(root).map_err(|e| format!("invalid root: {}", e))?;
        let mut changed = Vec::new();

        for patch in patches {
            let target = Self::resolve_under_root(&root, &patch.file_path)?;
            match patch.op {
                PatchOp::Add => {
                    if target.exists() {
                        return Err(format!("add failed: '{}' already exists", patch.file_path));
                    }
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).map_err(|e| {
                            format!("failed to create parent '{}': {}", parent.display(), e)
                        })?;
                    }
                    let content = if patch.add_file_lines.is_empty() {
                        String::new()
                    } else {
                        // For brand-new files we normalize to '\n' and end with newline.
                        let mut s = patch.add_file_lines.join("\n");
                        s.push('\n');
                        s
                    };
                    fs::write(&target, content)
                        .map_err(|e| format!("write '{}': {}", patch.file_path, e))?;
                    changed.push(patch.file_path.clone());
                }
                PatchOp::Delete => {
                    if target.exists() {
                        fs::remove_file(&target)
                            .map_err(|e| format!("delete '{}': {}", patch.file_path, e))?;
                        changed.push(patch.file_path.clone());
                    } else {
                        return Err(format!("delete failed: '{}' does not exist", patch.file_path));
                    }
                }
                PatchOp::Update => {
                    if !target.exists() {
                        return Err(format!("update failed: '{}' does not exist", patch.file_path));
                    }
                    let raw = fs::read_to_string(&target)
                        .map_err(|e| format!("read '{}': {}", patch.file_path, e))?;
                    let (mut file_lines, ending, had_trailing_newline) = Self::split_lines(&raw);

                    let mut cursor = 0usize;
                    for hunk in &patch.hunks {
                        let old_seq = Self::old_sequence(hunk);
                        let new_seq = Self::new_sequence(hunk);
                        if old_seq.is_empty() {
                            return Err(format!(
                                "update failed '{}': hunk old sequence is empty",
                                patch.file_path
                            ));
                        }

                        // Find unique match from current cursor onward.
                        let mut matches = Vec::new();
                        for i in cursor..=file_lines.len().saturating_sub(old_seq.len()) {
                            if file_lines[i..i + old_seq.len()] == old_seq[..] {
                                matches.push(i);
                            }
                        }

                        if matches.is_empty() {
                            return Err(format!(
                                "verification failed '{}': hunk context not found",
                                patch.file_path
                            ));
                        }
                        if matches.len() > 1 {
                            return Err(format!(
                                "verification failed '{}': ambiguous hunk ({} matches)",
                                patch.file_path,
                                matches.len()
                            ));
                        }

                        let at = matches[0];
                        file_lines.splice(at..at + old_seq.len(), new_seq.clone());
                        cursor = at + new_seq.len();
                    }

                    let mut out = file_lines.join(ending);
                    if had_trailing_newline {
                        out.push_str(ending);
                    }
                    fs::write(&target, out)
                        .map_err(|e| format!("write '{}': {}", patch.file_path, e))?;
                    changed.push(patch.file_path.clone());
                }
            }
        }

        Ok(ApplyResult {
            changed_paths: changed,
        })
    }

    fn parse_hunk(lines: &[&str], header_idx: usize) -> Result<(Hunk, usize), String> {
        let mut idx = header_idx + 1;
        let mut out = Vec::new();
        while idx < lines.len() {
            let line = lines[idx];
            let trimmed = line.trim();
            if trimmed.starts_with("@@") || trimmed.starts_with("*** ") || Self::is_end_marker(line) {
                break;
            }
            let mut chars = line.chars();
            let prefix = chars
                .next()
                .ok_or_else(|| format!("line {}: empty hunk line", idx + 1))?;
            let body = chars.collect::<String>();
            match prefix {
                ' ' => out.push(HunkLine::Context(body)),
                '-' => out.push(HunkLine::Remove(body)),
                '+' => out.push(HunkLine::Add(body)),
                _ => {
                    return Err(format!(
                        "line {}: invalid hunk line prefix '{}'",
                        idx + 1,
                        prefix
                    ))
                }
            }
            idx += 1;
        }
        if out.is_empty() {
            return Err(format!("line {}: empty hunk", header_idx + 1));
        }
        Ok((Hunk { lines: out }, idx))
    }

    fn old_sequence(hunk: &Hunk) -> Vec<String> {
        hunk.lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.clone()),
                HunkLine::Add(_) => None,
            })
            .collect()
    }

    fn new_sequence(hunk: &Hunk) -> Vec<String> {
        hunk.lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Add(s) => Some(s.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect()
    }

    fn split_lines(content: &str) -> (Vec<String>, &'static str, bool) {
        let line_ending = if content.contains("\r\n") { "\r\n" } else { "\n" };
        let had_trailing_newline = content.ends_with('\n');
        let normalized = content.replace("\r\n", "\n");
        if normalized.is_empty() {
            return (Vec::new(), line_ending, had_trailing_newline);
        }
        let mut lines: Vec<String> = normalized.split('\n').map(|s| s.to_string()).collect();
        if had_trailing_newline {
            let _ = lines.pop();
        }
        (lines, line_ending, had_trailing_newline)
    }

    fn resolve_under_root(root: &Path, raw_path: &str) -> Result<PathBuf, String> {
        if raw_path.trim().is_empty() {
            return Err("empty file path".to_string());
        }
        let rel = Path::new(raw_path);
        if rel.is_absolute() {
            return Err(format!("absolute paths are not allowed: '{}'", raw_path));
        }
        let candidate = root.join(rel);
        let normalized = Self::normalize_path(&candidate);
        if !normalized.starts_with(root) {
            return Err(format!("path escapes workspace root: '{}'", raw_path));
        }
        Ok(normalized)
    }

    fn normalize_path(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for c in path.components() {
            use std::path::Component;
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                _ => out.push(c),
            }
        }
        out
    }

    fn is_begin_marker(line: &str) -> bool {
        let t = line.trim();
        t == "*** Begin Patch" || t == "*** Begin Patch ***"
    }

    fn is_end_marker(line: &str) -> bool {
        let t = line.trim();
        t == "*** End Patch" || t == "*** End Patch ***"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_update_patch() {
        let patch = r#"
*** Begin Patch
*** Update File: a.txt
@@
 line1
-line2
+line2_new
 line3
*** End Patch
"#;
        let parsed = PatchEngine::parse_patch(patch).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].op, PatchOp::Update);
        assert_eq!(parsed[0].file_path, "a.txt");
        assert_eq!(parsed[0].hunks.len(), 1);
    }

    #[test]
    fn apply_update_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        fs::write(&p, "line1\nline2\nline3\n").unwrap();

        let patch = r#"
*** Begin Patch
*** Update File: a.txt
@@
 line1
-line2
+line2_new
 line3
*** End Patch
"#;
        let parsed = PatchEngine::parse_patch(patch).unwrap();
        PatchEngine::apply_patches(dir.path(), &parsed).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert_eq!(got, "line1\nline2_new\nline3\n");
    }

    #[test]
    fn rejects_ambiguous_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        fs::write(&p, "x\nx\n").unwrap();

        let patch = r#"
*** Begin Patch
*** Update File: a.txt
@@
 x
-x
+y
*** End Patch
"#;
        let parsed = PatchEngine::parse_patch(patch).unwrap();
        let err = PatchEngine::apply_patches(dir.path(), &parsed).unwrap_err();
        assert!(err.contains("ambiguous hunk"));
    }

    #[test]
    fn add_and_delete_file() {
        let dir = tempfile::tempdir().unwrap();
        let patch = r#"
*** Begin Patch
*** Add File: b.txt
+hello
+world
*** Delete File: c.txt
*** End Patch
"#;
        fs::write(dir.path().join("c.txt"), "bye\n").unwrap();
        let parsed = PatchEngine::parse_patch(patch).unwrap();
        PatchEngine::apply_patches(dir.path(), &parsed).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "hello\nworld\n"
        );
        assert!(!dir.path().join("c.txt").exists());
    }

    #[test]
    fn rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let patch = r#"
*** Begin Patch
*** Add File: ../oops.txt
+bad
*** End Patch
"#;
        let parsed = PatchEngine::parse_patch(patch).unwrap();
        let err = PatchEngine::apply_patches(dir.path(), &parsed).unwrap_err();
        assert!(err.contains("escapes workspace root"));
    }
}
