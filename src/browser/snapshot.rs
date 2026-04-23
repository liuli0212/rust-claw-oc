use std::collections::HashMap;

use chromiumoxide::cdp::browser_protocol::accessibility::AxNode;
use chromiumoxide::cdp::browser_protocol::dom::BackendNodeId;

/// Stable element reference stored between snapshot and act.
#[derive(Debug, Clone)]
pub(crate) struct ElementRef {
    pub backend_node_id: BackendNodeId,
    pub role: String,
    pub name: String,
}

/// Result of processing the accessibility tree.
pub(crate) struct SnapshotResult {
    /// `[id] role "name"` lines, ready for LLM consumption.
    pub output: String,
    /// Map from display ID → element info for use by `act`.
    pub node_map: HashMap<u32, ElementRef>,
    /// Total interactive elements found (before any limit).
    pub total_found: usize,
    /// Number of elements included in the output.
    pub included: usize,
}

/// ARIA roles considered interactive (the element can be clicked/typed/toggled).
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "textbox",
    "searchbox",
    "combobox",
    "spinbutton",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "option",
    "treeitem",
];

/// Roles that are always kept when filtering by hint (form-field family).
const FORM_FIELD_ROLES: &[&str] = &[
    "textbox",
    "searchbox",
    "combobox",
    "spinbutton",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "listbox",
    "select",
];

/// Extract the string value from an `AxValue`, if present.
fn ax_value_str(val: &Option<chromiumoxide::cdp::browser_protocol::accessibility::AxValue>) -> String {
    val.as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Intermediate struct for scoring.
struct ScoredElement {
    role: String,
    name: String,
    value: String,
    backend_node_id: BackendNodeId,
    score: usize,
    is_form_field: bool,
}

/// Process the raw AX tree nodes into a filtered, scored snapshot.
pub(crate) fn build_snapshot(
    nodes: &[AxNode],
    hint: Option<&str>,
    max_elements: usize,
    max_chars: usize,
) -> SnapshotResult {
    // 1. Filter to interactive nodes with a backend DOM node.
    let mut elements: Vec<ScoredElement> = Vec::new();

    for node in nodes {
        if node.ignored {
            continue;
        }
        let role = ax_value_str(&node.role);
        if role.is_empty() {
            continue;
        }
        let role_lower = role.to_lowercase();
        if !INTERACTIVE_ROLES.contains(&role_lower.as_str()) {
            continue;
        }
        let backend_node_id = match node.backend_dom_node_id {
            Some(id) => id,
            None => continue,
        };
        let name = ax_value_str(&node.name);
        let value = ax_value_str(&node.value);

        let is_form_field = FORM_FIELD_ROLES.contains(&role_lower.as_str());

        elements.push(ScoredElement {
            role: role_lower,
            name,
            value,
            backend_node_id,
            score: 0,
            is_form_field,
        });
    }

    let total_found = elements.len();

    // 2. Keyword overlap scoring (if hint provided).
    if let Some(hint_text) = hint {
        let hint_tokens = tokenize(hint_text);
        for el in &mut elements {
            let el_text = format!("{} {} {}", el.role, el.name, el.value).to_lowercase();
            let el_tokens = tokenize(&el_text);
            el.score = el_tokens.iter().filter(|t| hint_tokens.contains(*t)).count();
            // Form fields get a bonus so they're never filtered out.
            if el.is_form_field {
                el.score += 100;
            }
        }
        // Stable sort: higher score first, preserve original order for ties.
        elements.sort_by(|a, b| b.score.cmp(&a.score));
    }

    // 3. Apply max_elements limit.
    elements.truncate(max_elements);

    // 4. Re-assign sequential IDs after filtering (for a clean 1..N sequence).
    let mut node_map = HashMap::new();
    let mut lines = Vec::new();
    let mut char_count = 0;
    let mut included = 0;

    for (i, el) in elements.iter().enumerate() {
        let display_id = (i + 1) as u32;
        let line = if el.value.is_empty() {
            if el.name.is_empty() {
                format!("[{}] {}", display_id, el.role)
            } else {
                format!("[{}] {} \"{}\"", display_id, el.role, truncate_name(&el.name, 80))
            }
        } else {
            format!(
                "[{}] {} \"{}\" = \"{}\"",
                display_id,
                el.role,
                truncate_name(&el.name, 60),
                truncate_name(&el.value, 40),
            )
        };

        if char_count + line.len() > max_chars && included > 0 {
            break;
        }

        char_count += line.len() + 1; // +1 for newline
        lines.push(line);
        node_map.insert(display_id, ElementRef {
            backend_node_id: el.backend_node_id,
            role: el.role.clone(),
            name: el.name.clone(),
        });
        included += 1;
    }

    let output = if lines.is_empty() {
        "No interactive elements found on this page.".to_string()
    } else {
        lines.join("\n")
    };

    SnapshotResult {
        output,
        node_map,
        total_found,
        included,
    }
}

/// Unicode-aware tokenization: extract sequences of letters/digits.
fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in lower.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn truncate_name(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::browser_protocol::accessibility::{AxNode, AxValue, AxValueType};

    fn make_node(role: &str, name: &str, backend_id: i64) -> AxNode {
        let mut node = AxNode::new(format!("node-{}", backend_id), false);
        node.role = Some(AxValue {
            r#type: AxValueType::Role,
            value: Some(serde_json::Value::String(role.to_string())),
            related_nodes: None,
            sources: None,
        });
        node.name = Some(AxValue {
            r#type: AxValueType::String,
            value: Some(serde_json::Value::String(name.to_string())),
            related_nodes: None,
            sources: None,
        });
        node.backend_dom_node_id = Some(BackendNodeId::new(backend_id));
        node
    }

    #[test]
    fn filters_to_interactive_roles_only() {
        let nodes = vec![
            make_node("button", "Submit", 1),
            make_node("generic", "wrapper div", 2),
            make_node("link", "Home", 3),
            make_node("heading", "Page Title", 4),
            make_node("textbox", "Email", 5),
        ];

        let result = build_snapshot(&nodes, None, 100, 8000);
        assert_eq!(result.total_found, 3);
        assert_eq!(result.included, 3);
        assert!(result.output.contains("button \"Submit\""));
        assert!(result.output.contains("link \"Home\""));
        assert!(result.output.contains("textbox \"Email\""));
        assert!(!result.output.contains("generic"));
        assert!(!result.output.contains("heading"));
    }

    #[test]
    fn hint_scoring_prioritizes_relevant_elements() {
        let nodes = vec![
            make_node("link", "Privacy Policy", 1),
            make_node("link", "Terms of Service", 2),
            make_node("button", "Login", 3),
            make_node("textbox", "Username", 4),
            make_node("textbox", "Password", 5),
            make_node("link", "About Us", 6),
        ];

        // With hint "login", the login button should be first, form fields kept via bonus.
        let result = build_snapshot(&nodes, Some("login username password"), 3, 8000);
        assert_eq!(result.included, 3);
        // Form fields always have high priority due to bonus score.
        assert!(result.output.contains("textbox"));
        assert!(result.output.contains("Login"));
    }

    #[test]
    fn respects_max_elements_limit() {
        let nodes: Vec<AxNode> = (0..200)
            .map(|i| make_node("link", &format!("Link {}", i), i as i64 + 1))
            .collect();

        let result = build_snapshot(&nodes, None, 50, 80000);
        assert_eq!(result.total_found, 200);
        assert_eq!(result.included, 50);
    }

    #[test]
    fn respects_max_chars_limit() {
        let nodes: Vec<AxNode> = (0..200)
            .map(|i| make_node("link", &format!("A very long link name number {}", i), i as i64 + 1))
            .collect();

        let result = build_snapshot(&nodes, None, 200, 500);
        assert!(result.included < 200);
        assert!(result.output.len() <= 600); // some slack for last line
    }

    #[test]
    fn sequential_ids_after_filtering() {
        let nodes = vec![
            make_node("link", "A", 10),
            make_node("button", "B", 20),
            make_node("textbox", "C", 30),
        ];

        let result = build_snapshot(&nodes, None, 100, 8000);
        assert!(result.output.starts_with("[1]"));
        assert!(result.output.contains("[2]"));
        assert!(result.output.contains("[3]"));
        assert!(result.node_map.contains_key(&1));
        assert!(result.node_map.contains_key(&2));
        assert!(result.node_map.contains_key(&3));
    }

    #[test]
    fn empty_page_returns_friendly_message() {
        let nodes = vec![
            make_node("generic", "div", 1),
            make_node("heading", "Title", 2),
        ];

        let result = build_snapshot(&nodes, None, 100, 8000);
        assert_eq!(result.total_found, 0);
        assert!(result.output.contains("No interactive elements"));
    }

    #[test]
    fn tokenize_handles_unicode() {
        let tokens = tokenize("搜索 Bluetooth Kopfhörer");
        assert!(tokens.contains(&"搜索".to_string()));
        assert!(tokens.contains(&"bluetooth".to_string()));
        assert!(tokens.contains(&"kopfhörer".to_string()));
    }
}
