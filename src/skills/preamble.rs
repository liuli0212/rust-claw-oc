//! Preamble executor — runs a skill's preamble script at activation time.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::tools::shell::{execute_shell, ShellExecResult};

/// Result of a preamble execution.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PreambleResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub vars: BTreeMap<String, String>,
    pub side_effects: Vec<String>,
}

/// Default preamble timeout.
const PREAMBLE_TIMEOUT_SECS: u64 = 30;

/// Execute a preamble shell script and parse `KEY=VALUE` output lines.
pub async fn execute_preamble(shell_cmd: &str, cwd: Option<&std::path::Path>) -> PreambleResult {
    let timeout = Duration::from_secs(PREAMBLE_TIMEOUT_SECS);

    match execute_shell(shell_cmd, timeout, cwd).await {
        Ok(result) => parse_preamble_result(result),
        Err(e) => PreambleResult {
            ok: false,
            stdout: String::new(),
            stderr: format!("Preamble execution failed: {}", e),
            vars: BTreeMap::new(),
            side_effects: vec![format!("error: {}", e)],
        },
    }
}

/// Parse a `ShellExecResult` into a `PreambleResult`, extracting `KEY=VALUE`
/// pairs from stdout.
fn parse_preamble_result(result: ShellExecResult) -> PreambleResult {
    let mut vars = BTreeMap::new();

    for line in result.stdout.lines() {
        let trimmed = line.trim();
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim();
            let value = trimmed[eq_pos + 1..].trim();
            // Only accept simple KEY=VALUE (no spaces in key, uppercase convention)
            if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                vars.insert(key.to_string(), value.to_string());
            }
        }
    }

    PreambleResult {
        ok: result.ok,
        stdout: result.stdout,
        stderr: result.stderr,
        vars,
        side_effects: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_preamble_basic() {
        let result = execute_preamble("echo 'READY=true'", None).await;
        assert!(result.ok);
        assert_eq!(result.vars.get("READY").map(|s| s.as_str()), Some("true"));
    }

    #[tokio::test]
    async fn test_execute_preamble_multiple_vars() {
        let result = execute_preamble(
            r#"echo "MODE=startup"
echo "VERSION=2.0"
echo "some random line"
echo "DEBUG=1""#,
            None,
        )
        .await;
        assert!(result.ok);
        assert_eq!(result.vars.get("MODE").map(|s| s.as_str()), Some("startup"));
        assert_eq!(result.vars.get("VERSION").map(|s| s.as_str()), Some("2.0"));
        assert_eq!(result.vars.get("DEBUG").map(|s| s.as_str()), Some("1"));
        assert_eq!(result.vars.len(), 3);
    }

    #[tokio::test]
    async fn test_execute_preamble_failure() {
        let result = execute_preamble("exit 1", None).await;
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_execute_preamble_invalid_command() {
        let result = execute_preamble("this_command_does_not_exist_xyz_12345", None).await;
        // The command will run but fail
        assert!(!result.ok);
    }
}
