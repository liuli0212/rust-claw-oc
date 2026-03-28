//! Tool policy — controls which tools are visible to the model during skill execution.

use std::collections::HashMap;
use std::sync::Arc;

use crate::tools::Tool;

use super::definition::SkillDef;

/// Determines the visible tool set based on the active skill's `allowed_tools`.
pub struct SkillToolPolicy {
    /// Canonical name mapping: UI name → internal name.
    name_map: HashMap<String, String>,
}

/// Tools that are always available during skill execution, regardless of whitelist.
const RUNTIME_TOOLS: &[&str] = &["finish_task", "task_plan"];

impl SkillToolPolicy {
    pub fn new() -> Self {
        let mut name_map = HashMap::new();
        // Canonical mappings
        name_map.insert("Bash".to_string(), "execute_bash".to_string());
        name_map.insert("Read".to_string(), "read_file".to_string());
        name_map.insert("Write".to_string(), "write_file".to_string());
        name_map.insert("Edit".to_string(), "patch_file".to_string());
        name_map.insert("WebSearch".to_string(), "web_search".to_string());
        name_map.insert("AskUser".to_string(), "ask_user_question".to_string());
        name_map.insert("ask_user".to_string(), "ask_user_question".to_string());
        name_map.insert(
            "AskUserQuestion".to_string(),
            "ask_user_question".to_string(),
        );
        name_map.insert("spawn_subagent".to_string(), "spawn_subagent".to_string());
        name_map.insert("get_subagent_result".to_string(), "get_subagent_result".to_string());
        name_map.insert("cancel_subagent".to_string(), "cancel_subagent".to_string());
        name_map.insert("list_subagent_jobs".to_string(), "list_subagent_jobs".to_string());
        Self { name_map }
    }

    /// Resolve a tool name to its canonical internal form.
    pub fn canonical_name(&self, name: &str) -> String {
        self.name_map
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// Filter the base tool set according to a skill's `allowed_tools` whitelist.
    ///
    /// If `allowed_tools` is empty, all tools are allowed (no restriction).
    pub fn filter_tools(
        &self,
        base_tools: Vec<Arc<dyn Tool>>,
        skill: &SkillDef,
    ) -> Vec<Arc<dyn Tool>> {
        if skill.meta.allowed_tools.is_empty() {
            return base_tools;
        }

        let allowed: Vec<String> = skill
            .meta
            .allowed_tools
            .iter()
            .map(|n| self.canonical_name(n))
            .collect();

        base_tools
            .into_iter()
            .filter(|tool| {
                let name = tool.name();
                // Always allow runtime-essential tools
                if RUNTIME_TOOLS.contains(&name.as_str()) {
                    return true;
                }
                allowed.contains(&name)
            })
            .collect()
    }

    /// Check if a specific tool is allowed.
    #[allow(dead_code)]
    pub fn can_call(&self, tool_name: &str, skill: &SkillDef) -> bool {
        if skill.meta.allowed_tools.is_empty() {
            return true;
        }
        if RUNTIME_TOOLS.contains(&tool_name) {
            return true;
        }
        let allowed: Vec<String> = skill
            .meta
            .allowed_tools
            .iter()
            .map(|n| self.canonical_name(n))
            .collect();
        allowed.contains(&tool_name.to_string())
    }
}

impl Default for SkillToolPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::definition::*;

    fn make_skill(allowed: Vec<&str>) -> SkillDef {
        SkillDef {
            meta: SkillMeta {
                name: "test".to_string(),
                version: "1.0".to_string(),
                description: "test".to_string(),
                trigger: SkillTrigger::ManualOnly,
                allowed_tools: allowed.into_iter().map(|s| s.to_string()).collect(),
                output_mode: None,
                parameters: None,
            },
            instructions: String::new(),
            preamble: None,
            parameters: None,
            constraints: SkillConstraints::default(),
        }
    }

    #[test]
    fn test_empty_whitelist_allows_all() {
        let policy = SkillToolPolicy::new();
        let skill = make_skill(vec![]);
        assert!(policy.can_call("execute_bash", &skill));
        assert!(policy.can_call("anything", &skill));
    }

    #[test]
    fn test_whitelist_filters() {
        let policy = SkillToolPolicy::new();
        let skill = make_skill(vec!["read_file", "execute_bash"]);
        assert!(policy.can_call("read_file", &skill));
        assert!(policy.can_call("execute_bash", &skill));
        assert!(!policy.can_call("write_file", &skill));
    }

    #[test]
    fn test_runtime_tools_always_allowed() {
        let policy = SkillToolPolicy::new();
        let skill = make_skill(vec!["read_file"]);
        assert!(policy.can_call("finish_task", &skill));
        assert!(policy.can_call("task_plan", &skill));
    }

    #[test]
    fn test_canonical_name_mapping() {
        let policy = SkillToolPolicy::new();
        assert_eq!(policy.canonical_name("Bash"), "execute_bash");
        assert_eq!(policy.canonical_name("Read"), "read_file");
        assert_eq!(policy.canonical_name("unknown_tool"), "unknown_tool");
    }
}
