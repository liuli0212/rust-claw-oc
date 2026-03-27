//! Skill registry — recursive discovery and lookup.

use std::collections::BTreeMap;
use std::path::Path;

use super::definition::SkillDef;
use super::parser::parse_skill_md;

/// In-memory registry of all discovered skills.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillDef>,
}

impl SkillRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Recursively scan `base_dir` for `SKILL.md` files and register them.
    ///
    /// Also scans for legacy `*.md` files (non-SKILL.md) and attempts to
    /// parse them with the unified parser first, falling back if needed.
    pub fn discover(&mut self, base_dir: &Path) {
        if !base_dir.exists() || !base_dir.is_dir() {
            return;
        }
        self.walk_dir(base_dir);
    }

    fn walk_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                self.walk_dir(&path);
            } else if path.is_file() {
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();

                // Only process .md files
                if !filename.ends_with(".md") {
                    continue;
                }

                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some(def) = parse_skill_md(&content) {
                        self.skills.insert(def.meta.name.clone(), def);
                    }
                }
            }
        }
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&SkillDef> {
        self.skills.get(name)
    }

    /// List all registered skill names.
    pub fn names(&self) -> Vec<&str> {
        self.skills.keys().map(|s| s.as_str()).collect()
    }

    /// Total number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Insert a skill definition directly (used by migration).
    pub fn insert(&mut self, def: SkillDef) {
        self.skills.insert(def.meta.name.clone(), def);
    }

    pub fn clone_skill(&self, name: &str) -> Option<SkillDef> {
        self.skills.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_discover_skill_md() {
        let dir = tempdir().unwrap();

        // Create a SKILL.md in a subdirectory
        let sub = dir.path().join("my_skill");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("SKILL.md"),
            r#"---
name: my_skill
description: A test skill
---
# Instructions
Do something.
"#,
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        reg.discover(dir.path());

        assert_eq!(reg.len(), 1);
        assert!(reg.get("my_skill").is_some());
        assert_eq!(reg.names(), vec!["my_skill"]);
    }

    #[test]
    fn test_discover_nested() {
        let dir = tempdir().unwrap();

        let deep = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        fs::write(
            deep.join("SKILL.md"),
            r#"---
name: deep_skill
description: A deep skill
---
body
"#,
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        reg.discover(dir.path());

        assert_eq!(reg.len(), 1);
        assert!(reg.get("deep_skill").is_some());
    }

    #[test]
    fn test_discover_empty_dir() {
        let dir = tempdir().unwrap();
        let mut reg = SkillRegistry::new();
        reg.discover(dir.path());
        assert!(reg.is_empty());
    }

    #[test]
    fn test_discover_nonexistent_dir() {
        let mut reg = SkillRegistry::new();
        reg.discover(Path::new("/nonexistent/path/12345"));
        assert!(reg.is_empty());
    }
}
