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
pub const RUNTIME_TOOLS: &[&str] = &["task_plan"];

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
        name_map.insert("subagent".to_string(), "subagent".to_string());
        name_map.insert("call_skill".to_string(), "subagent".to_string());
        name_map.insert("spawn_subagent".to_string(), "subagent".to_string());
        name_map.insert("get_subagent_result".to_string(), "subagent".to_string());
        name_map.insert("cancel_subagent".to_string(), "subagent".to_string());
        name_map.insert("list_subagent_jobs".to_string(), "subagent".to_string());
        Self { name_map }
    }

    /// Resolve a tool name to its canonical internal form.
    pub fn canonical_name(&self, name: &str) -> String {
        self.name_map
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    pub fn canonicalize_tools(&self, tools: &[String]) -> Vec<String> {
        let mut canonical = Vec::new();
        for tool in tools {
            let mapped = self.canonical_name(tool);
            if !canonical.contains(&mapped) {
                canonical.push(mapped);
            }
        }
        canonical
    }

    pub fn is_runtime_tool(tool_name: &str) -> bool {
        RUNTIME_TOOLS.contains(&tool_name)
    }

    pub fn can_call_with_allowed_names(&self, allowed: &[String], tool_name: &str) -> bool {
        Self::is_runtime_tool(tool_name)
            || allowed.contains(&tool_name.to_string())
            || (tool_name == "wait" && allowed.iter().any(|name| name == "exec"))
    }

    pub fn filter_tools_by_allowed_names(
        &self,
        base_tools: Vec<Arc<dyn Tool>>,
        allowed: &[String],
    ) -> Vec<Arc<dyn Tool>> {
        if allowed.is_empty() {
            return base_tools;
        }

        base_tools
            .into_iter()
            .filter(|tool| self.can_call_with_allowed_names(allowed, &tool.name()))
            .collect()
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

        let allowed = self.canonicalize_tools(&skill.meta.allowed_tools);
        self.filter_tools_by_allowed_names(base_tools, &allowed)
    }

    /// Check if a specific tool is allowed.
    #[allow(dead_code)]
    pub fn can_call(&self, tool_name: &str, skill: &SkillDef) -> bool {
        if skill.meta.allowed_tools.is_empty() {
            return true;
        }
        let allowed = self.canonicalize_tools(&skill.meta.allowed_tools);
        self.can_call_with_allowed_names(&allowed, tool_name)
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
            },
            instructions: String::new(),
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
    fn test_wait_is_allowed_when_exec_is_whitelisted() {
        let policy = SkillToolPolicy::new();
        let skill = make_skill(vec!["exec"]);
        assert!(policy.can_call("exec", &skill));
        assert!(policy.can_call("wait", &skill));
        assert!(!policy.can_call("read_file", &skill));
    }

    #[test]
    fn test_runtime_tools_always_allowed() {
        let policy = SkillToolPolicy::new();
        let skill = make_skill(vec!["read_file"]);
        assert!(policy.can_call("task_plan", &skill));
    }

    #[test]
    fn test_canonical_name_mapping() {
        let policy = SkillToolPolicy::new();
        assert_eq!(policy.canonical_name("Bash"), "execute_bash");
        assert_eq!(policy.canonical_name("Read"), "read_file");
        assert_eq!(policy.canonical_name("call_skill"), "subagent");
        assert_eq!(policy.canonical_name("spawn_subagent"), "subagent");
        assert_eq!(policy.canonical_name("get_subagent_result"), "subagent");
        assert_eq!(policy.canonical_name("unknown_tool"), "unknown_tool");
    }
}
