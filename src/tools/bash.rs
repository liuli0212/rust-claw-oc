use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};
use async_trait::async_trait;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::time::{Duration, Instant};
use tokio::time::timeout;

pub struct BashTool {
    work_dir: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCmdArgs {
    /// The shell command to execute
    pub command: String,
    /// Timeout in seconds (default: 30)
    pub timeout: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BashExecutionResult {
    pub ok: bool,
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub truncated: bool,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BashTool {
    pub fn new() -> Self {
        let work_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .to_string_lossy()
            .to_string();
        Self { work_dir }
    }

    fn classify_command_effect(cmd: &str) -> Option<(&'static str, String, String)> {
        let cmd_trim = cmd.trim();
        let is_diagnostic = cmd_trim.contains("cargo ")
            || cmd_trim.contains("npm run")
            || cmd_trim.contains("pytest")
            || cmd_trim.contains("tsc")
            || cmd_trim.contains("make");
        let is_dir_list = cmd_trim.starts_with("ls ")
            || cmd_trim == "ls"
            || cmd_trim.starts_with("tree ")
            || cmd_trim == "tree"
            || cmd_trim.starts_with("find ");

        if is_diagnostic {
            Some((
                "diagnostic",
                "workspace_state".to_string(),
                format!("Bash snapshot: {}", cmd_trim)
                    .chars()
                    .take(200)
                    .collect(),
            ))
        } else if is_dir_list {
            Some((
                "directory",
                cmd_trim.to_string(),
                format!("Bash snapshot: {}", cmd_trim)
                    .chars()
                    .take(200)
                    .collect(),
            ))
        } else {
            None
        }
    }
}


/// RAII guard that ensures both the child process and PTY master are cleaned up
/// when the execution future is dropped (e.g., by `tokio::time::timeout` or task abort).
/// Dropping the master PTY causes the reader thread to receive EOF and exit.
struct BashExecutionGuard {
    child: std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl Drop for BashExecutionGuard {
    fn drop(&mut self) {
        // 1. Kill the child process
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
        // 2. Drop the master PTY to unblock any reader threads waiting on read()
        self.master.take();
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> String {
        "execute_bash".to_string()
    }

    fn description(&self) -> String {
        "Executes a bash command. Returns stdout and stderr. Use carefully.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        let mut val = clean_schema(serde_json::to_value(schema_for!(ExecuteCmdArgs)).unwrap());
        if let Some(properties) = val.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if let Some(timeout) = properties
                .get_mut("timeout")
                .and_then(|t| t.as_object_mut())
            {
                if let Some(type_arr) = timeout.get("type").and_then(|t| t.as_array()) {
                    if let Some(first) = type_arr.first() {
                        timeout.insert("type".to_string(), first.clone());
                    }
                }
            }
        }
        val
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &crate::tools::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        let parsed_args: ExecuteCmdArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            if policy.level != crate::tools::sandbox::SandboxLevel::Unrestricted
                && !sandbox.is_available()
            {
                return Err(ToolError::ExecutionFailed(sandbox.shell_execution_error()));
            }
        }

        let timeout_secs = parsed_args.timeout.unwrap_or(30);
        let cmd_str = parsed_args.command;
        let start = Instant::now();

        tracing::info!("Executing bash via PTY: {}", cmd_str);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| {
                tracing::error!("BashTool Error: Failed to open PTY - {}", e);
                ToolError::ExecutionFailed(e.to_string())
            })?;

        // ── Sandbox-aware command construction ──
        let work_dir_path = std::path::Path::new(&self.work_dir);
        let mut cmd = if let Some(sandbox) = ctx.sandbox.as_ref().filter(|s| s.is_available()) {
            let policy = sandbox.default_policy();
            tracing::info!(
                "BashTool: executing in bwrap sandbox (level={:?})",
                policy.level
            );
            sandbox.build_pty_command(&cmd_str, policy, work_dir_path)
        } else {
            let mut c = CommandBuilder::new("bash");
            c.cwd(self.work_dir.clone());
            c.arg("-c");
            c.arg(&cmd_str);
            c
        };

        cmd.env("GIT_PAGER", "cat");
        cmd.env("PAGER", "cat");
        cmd.env("GIT_TERMINAL_PROMPT", "0");

        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            tracing::error!(
                "BashTool Error: Failed to spawn command '{}' - {}",
                cmd_str,
                e
            );
            ToolError::ExecutionFailed(e.to_string())
        })?;
        let child = std::sync::Arc::new(std::sync::Mutex::new(child));
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        // RAII guard: owns both child and master. When the async future is dropped
        // (timeout/abort), the guard kills the child and closes the PTY master,
        // which unblocks the reader thread.
        let guard = BashExecutionGuard {
            child: child.clone(),
            master: Some(pair.master),
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let child_clone = child.clone();
        let read_future = async move {
            // Guard lives inside the future; it is dropped when the future completes
            // or is cancelled by timeout.
            let _guard = guard;

            let mut raw_output = String::new();
            while let Some(chunk) = rx.recv().await {
                raw_output.push_str(&String::from_utf8_lossy(&chunk));
            }

            let exit_status = tokio::task::spawn_blocking(move || {
                let mut c = child_clone.lock().unwrap();
                c.wait()
            })
            .await
            .map_err(|e| e.to_string());

            (raw_output, exit_status)
        };

        match timeout(Duration::from_secs(timeout_secs), read_future).await {
            Ok((raw_output, exit_status_res)) => {
                let status_res = exit_status_res
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

                let re = Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").unwrap();
                let clean_output = re.replace_all(&raw_output, "").into_owned();
                let clean_output = clean_output.replace("\r\n", "\n");
                let raw_trimmed = clean_output.trim().to_string();
                let truncated_stdout = crate::utils::truncate_tool_output(&raw_trimmed);
                let truncated = truncated_stdout != raw_trimmed;
                let result = BashExecutionResult {
                    ok: status_res.success(),
                    command: cmd_str.clone(),
                    stdout: truncated_stdout,
                    stderr: String::new(),
                    exit_code: i32::try_from(status_res.exit_code()).unwrap_or(i32::MAX),
                    duration_ms: start.elapsed().as_millis(),
                    truncated,
                };
                let output = if result.stderr.trim().is_empty() {
                    result.stdout.clone()
                } else if result.stdout.trim().is_empty() {
                    result.stderr.clone()
                } else {
                    format!("{}\n{}", result.stdout, result.stderr)
                };
                let mut structured = StructuredToolOutput::new(
                    "execute_bash",
                    result.ok,
                    output,
                    Some(result.exit_code),
                    Some(result.duration_ms),
                    result.truncated,
                );
                if let Some((kind, source_path, summary)) = Self::classify_command_effect(&cmd_str)
                {
                    structured = structured.with_evidence(kind, source_path, summary);
                }
                structured.to_json_string()
            }
            Err(_) => {
                // Timeout: the read_future was dropped, which dropped the guard,
                // which killed the child and closed the PTY master.
                tracing::warn!(
                    "Bash command timed out after {}s: {}",
                    timeout_secs,
                    cmd_str
                );
                Err(ToolError::Timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sandbox::{SandboxEnforcer, SandboxLevel, SandboxPolicy};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_execute_bash_blocks_without_bwrap_when_sandbox_enabled() {
        let tool = BashTool::new();
        let mut ctx = crate::tools::ToolContext::new("test", "test");
        ctx.sandbox = Some(Arc::new(SandboxEnforcer::disabled_with_policy(
            SandboxPolicy {
                level: SandboxLevel::Restricted,
                ..Default::default()
            },
        )));

        let err = tool
            .execute(
                serde_json::json!({
                    "command": "echo hello",
                    "timeout": 1,
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert!(err
            .to_string()
            .contains("Bubblewrap (`bwrap`) is unavailable"));
    }
}
