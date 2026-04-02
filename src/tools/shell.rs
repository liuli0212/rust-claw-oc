//! Unified shell execution component.
//!
//! Provides a lightweight, non-PTY shell executor for non-interactive shell
//! work. `BashTool` retains its own PTY-based implementation for interactive
//! use, but this module can be extended to consolidate further in the future.

use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;

use super::protocol::ToolError;

/// Result of a non-interactive shell execution.
#[derive(Debug, Clone)]
pub struct ShellExecResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    #[allow(dead_code)]
    pub duration_ms: u128,
}

/// Execute a shell command via `bash -c` with a timeout.
///
/// This is the **non-PTY** path for any context where interactive terminal
/// emulation is not required.
pub async fn execute_shell(
    command: &str,
    timeout: Duration,
    cwd: Option<&Path>,
) -> Result<ShellExecResult, ToolError> {
    execute_shell_inner(command, timeout, cwd, None, None).await
}

/// Execute a shell command with optional sandbox enforcement.
pub async fn execute_shell_sandboxed(
    command: &str,
    timeout: Duration,
    cwd: Option<&Path>,
    sandbox: Option<&super::sandbox::SandboxEnforcer>,
    policy: Option<&super::sandbox::SandboxPolicy>,
) -> Result<ShellExecResult, ToolError> {
    execute_shell_inner(command, timeout, cwd, sandbox, policy).await
}

async fn execute_shell_inner(
    command: &str,
    timeout: Duration,
    cwd: Option<&Path>,
    sandbox: Option<&super::sandbox::SandboxEnforcer>,
    policy: Option<&super::sandbox::SandboxPolicy>,
) -> Result<ShellExecResult, ToolError> {
    let start = Instant::now();

    let effective_cwd = cwd.unwrap_or_else(|| Path::new("."));

    let mut cmd = if let (Some(sb), Some(pol)) = (sandbox.filter(|s| s.is_available()), policy) {
        tracing::info!("shell: executing in bwrap sandbox");
        sb.build_tokio_command(command, pol, effective_cwd)
    } else {
        let mut c = Command::new("bash");
        c.arg("-c").arg(command);
        c
    };

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.env("GIT_PAGER", "cat");
    cmd.env("PAGER", "cat");
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    if !sandbox.is_some_and(|s| s.is_available()) {
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
    }

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| ToolError::Timeout)?
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();
    let ok = output.status.success();

    Ok(ShellExecResult {
        ok,
        stdout,
        stderr,
        exit_code,
        duration_ms: start.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_shell_echo() {
        let result = execute_shell("echo hello", Duration::from_secs(5), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.stdout.trim().contains("hello"));
        assert!(result.exit_code == Some(0));
    }

    #[tokio::test]
    async fn test_execute_shell_failure() {
        let result = execute_shell("exit 42", Duration::from_secs(5), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn test_execute_shell_timeout() {
        let result = execute_shell("sleep 60", Duration::from_millis(200), None).await;
        assert!(matches!(result, Err(ToolError::Timeout)));
    }

    #[tokio::test]
    async fn test_execute_shell_with_cwd() {
        let result = execute_shell("pwd", Duration::from_secs(5), Some(Path::new("/tmp")))
            .await
            .unwrap();
        assert!(result.ok);
        // macOS resolves /tmp -> /private/tmp
        assert!(
            result.stdout.trim() == "/tmp" || result.stdout.trim() == "/private/tmp",
            "unexpected cwd: {}",
            result.stdout.trim()
        );
    }
}
