use crate::core::AgentOutput;
use crate::session_manager::{ForegroundTaskKind, SessionManager};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const HARD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CLI_VISIBLE_LIMIT_BYTES: usize = 64 * 1024;
const CLI_TAIL_CHARS: usize = 16 * 1024;
const WINDOW_TAIL_CHARS: usize = 3_000;
const DISCORD_WINDOW_TAIL_CHARS: usize = 1_600;
const WINDOW_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEscapeEntrypoint {
    Cli,
    Headless,
    Telegram,
    Discord,
    Acp,
}

impl ShellEscapeEntrypoint {
    fn env_var(self) -> &'static str {
        match self {
            ShellEscapeEntrypoint::Cli => "RUSTY_CLAW_CLI_SHELL_ESCAPE",
            ShellEscapeEntrypoint::Headless => "RUSTY_CLAW_HEADLESS_SHELL_ESCAPE",
            ShellEscapeEntrypoint::Telegram => "RUSTY_CLAW_TELEGRAM_SHELL_ESCAPE",
            ShellEscapeEntrypoint::Discord => "RUSTY_CLAW_DISCORD_SHELL_ESCAPE",
            ShellEscapeEntrypoint::Acp => "RUSTY_CLAW_ACP_SHELL_ESCAPE",
        }
    }

    fn default_enabled(self) -> bool {
        matches!(
            self,
            ShellEscapeEntrypoint::Cli | ShellEscapeEntrypoint::Headless
        )
    }

    fn label(self) -> &'static str {
        match self {
            ShellEscapeEntrypoint::Cli => "CLI",
            ShellEscapeEntrypoint::Headless => "headless",
            ShellEscapeEntrypoint::Telegram => "Telegram",
            ShellEscapeEntrypoint::Discord => "Discord",
            ShellEscapeEntrypoint::Acp => "ACP",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ShellOutputMode {
    Stream {
        visible_limit_bytes: usize,
        tail_chars: usize,
    },
    Window {
        tail_chars: usize,
        update_interval: Duration,
    },
}

impl ShellOutputMode {
    pub fn cli_stream() -> Self {
        Self::Stream {
            visible_limit_bytes: CLI_VISIBLE_LIMIT_BYTES,
            tail_chars: CLI_TAIL_CHARS,
        }
    }

    pub fn live_window() -> Self {
        Self::Window {
            tail_chars: WINDOW_TAIL_CHARS,
            update_interval: WINDOW_UPDATE_INTERVAL,
        }
    }

    pub fn discord_window() -> Self {
        Self::Window {
            tail_chars: DISCORD_WINDOW_TAIL_CHARS,
            update_interval: WINDOW_UPDATE_INTERVAL,
        }
    }

    fn tail_chars(self) -> usize {
        match self {
            ShellOutputMode::Stream { tail_chars, .. } => tail_chars,
            ShellOutputMode::Window { tail_chars, .. } => tail_chars,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEscapeParseError {
    EmptyCommand,
}

impl std::fmt::Display for ShellEscapeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellEscapeParseError::EmptyCommand => write!(f, "Shell command after ! is empty."),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShellRunSummary {
    pub exit_code: Option<i32>,
    pub duration: Duration,
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub tail: String,
}

impl ShellRunSummary {
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }

    pub fn process_exit_code(&self) -> i32 {
        if self.cancelled {
            130
        } else if self.timed_out {
            124
        } else {
            self.exit_code.unwrap_or(1)
        }
    }
}

pub fn parse_shell_escape(input: &str) -> Option<Result<&str, ShellEscapeParseError>> {
    let command = input.trim_start().strip_prefix('!')?;
    if command.trim().is_empty() {
        Some(Err(ShellEscapeParseError::EmptyCommand))
    } else {
        Some(Ok(command))
    }
}

pub async fn run_shell_escape_command(
    session_manager: Arc<SessionManager>,
    session_id: &str,
    entrypoint: ShellEscapeEntrypoint,
    output: Arc<dyn AgentOutput>,
    command: &str,
    mode: ShellOutputMode,
) -> Result<ShellRunSummary, String> {
    if !shell_escape_enabled(entrypoint) {
        return Err(format!(
            "Shell escape is disabled for {}. Set {}=1 to enable it.",
            entrypoint.label(),
            entrypoint.env_var()
        ));
    }

    let foreground = session_manager
        .try_acquire_foreground(session_id, ForegroundTaskKind::Shell)
        .map_err(|e| e.to_string())?;
    let cancel_token = foreground.cancel_token();
    output.on_waiting("Running shell command...").await;
    let summary = run_shell_escape(command, output, mode, cancel_token).await?;
    drop(foreground);
    Ok(summary)
}

pub async fn run_shell_escape(
    command: &str,
    output: Arc<dyn AgentOutput>,
    mode: ShellOutputMode,
    cancel_token: CancellationToken,
) -> Result<ShellRunSummary, String> {
    let started = Instant::now();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
    let mut process = Command::new(shell);
    process.arg("-c").arg(command);
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    process.stdin(Stdio::null());
    process.kill_on_drop(true);
    process.env("GIT_PAGER", "cat");
    process.env("PAGER", "cat");
    process.env("GIT_TERMINAL_PROMPT", "0");
    configure_process_group(&mut process);

    let mut child = process
        .spawn()
        .map_err(|e| format!("Failed to start shell command: {}", e))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<String>();
    if let Some(stdout) = stdout {
        tokio::spawn(read_pipe(stdout, chunk_tx.clone()));
    }
    if let Some(stderr) = stderr {
        tokio::spawn(read_pipe(stderr, chunk_tx.clone()));
    }
    drop(chunk_tx);

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let (end_tx, mut end_rx) = oneshot::channel::<ProcessEnd>();
    tokio::spawn(async move {
        let process_end = wait_for_process(child, kill_rx).await;
        let _ = end_tx.send(process_end);
    });

    let mut tail = TailBuffer::new(mode.tail_chars());
    let mut visible = VisibleOutput::new(mode);
    let mut process_end: Option<ProcessEnd> = None;
    let mut chunks_done = false;
    let mut kill_tx = Some(kill_tx);
    let timeout_sleep = tokio::time::sleep(HARD_TIMEOUT);
    tokio::pin!(timeout_sleep);

    if matches!(mode, ShellOutputMode::Window { .. }) {
        output
            .on_text_replace(&render_window(
                command,
                tail.as_str(),
                false,
                started.elapsed(),
                true,
            ))
            .await;
    }

    loop {
        tokio::select! {
            maybe_chunk = chunk_rx.recv(), if !chunks_done => {
                match maybe_chunk {
                    Some(chunk) => {
                        let chunk = normalize_chunk(&chunk);
                        tail.push(&chunk);
                        visible.handle_chunk(command, &chunk, tail.as_str(), tail.truncated(), started, output.as_ref()).await;
                    }
                    None => chunks_done = true,
                }
            }
            result = &mut end_rx, if process_end.is_none() => {
                process_end = Some(result.map_err(|_| "Shell process monitor stopped unexpectedly.".to_string())?);
                kill_tx = None;
            }
            _ = cancel_token.cancelled(), if kill_tx.is_some() && process_end.is_none() => {
                if let Some(tx) = kill_tx.take() {
                    let _ = tx.send(KillReason::Cancelled);
                }
            }
            _ = &mut timeout_sleep, if kill_tx.is_some() && process_end.is_none() => {
                if let Some(tx) = kill_tx.take() {
                    let _ = tx.send(KillReason::TimedOut);
                }
            }
        }

        if chunks_done && process_end.is_some() {
            break;
        }
    }

    let process_end =
        process_end.ok_or_else(|| "Shell process ended without a status.".to_string())?;
    let summary = ShellRunSummary {
        exit_code: process_end.exit_code,
        duration: started.elapsed(),
        truncated: tail.truncated() || visible.truncated(),
        timed_out: matches!(process_end.kill_reason, Some(KillReason::TimedOut)),
        cancelled: matches!(process_end.kill_reason, Some(KillReason::Cancelled)),
        tail: tail.as_str().to_string(),
    };

    visible.finish(command, &summary, output.as_ref()).await;
    Ok(summary)
}

async fn read_pipe<R>(mut reader: R, tx: mpsc::UnboundedSender<String>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx
                    .send(String::from_utf8_lossy(&buf[..n]).to_string())
                    .is_err()
                {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(format!("\n[shell stream read error: {}]\n", e));
                break;
            }
        }
    }
}

async fn wait_for_process(
    mut child: tokio::process::Child,
    kill_rx: oneshot::Receiver<KillReason>,
) -> ProcessEnd {
    tokio::select! {
        status = child.wait() => ProcessEnd {
            exit_code: status.ok().and_then(|status| status.code()),
            kill_reason: None,
        },
        reason = kill_rx => {
            let kill_reason = reason.unwrap_or(KillReason::Cancelled);
            terminate_child_tree(&mut child).await;
            let status = child.wait().await;
            ProcessEnd {
                exit_code: status.ok().and_then(|status| status.code()),
                kill_reason: Some(kill_reason),
            }
        }
    }
}

#[cfg(unix)]
fn configure_process_group(process: &mut Command) {
    unsafe {
        process.pre_exec(|| {
            if setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_process: &mut Command) {}

#[cfg(unix)]
async fn terminate_child_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        let process_group = -pid;
        unsafe {
            let _ = kill(process_group, SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        unsafe {
            let _ = kill(process_group, SIGKILL);
        }
    }

    let _ = child.start_kill();
}

#[cfg(not(unix))]
async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillReason {
    Cancelled,
    TimedOut,
}

#[derive(Debug)]
struct ProcessEnd {
    exit_code: Option<i32>,
    kill_reason: Option<KillReason>,
}

struct VisibleOutput {
    mode: ShellOutputMode,
    sent_bytes: usize,
    truncated: bool,
    last_window_update: Instant,
}

impl VisibleOutput {
    fn new(mode: ShellOutputMode) -> Self {
        Self {
            mode,
            sent_bytes: 0,
            truncated: false,
            last_window_update: Instant::now(),
        }
    }

    fn truncated(&self) -> bool {
        self.truncated
    }

    async fn handle_chunk(
        &mut self,
        command: &str,
        chunk: &str,
        tail: &str,
        tail_truncated: bool,
        started: Instant,
        output: &dyn AgentOutput,
    ) {
        match self.mode {
            ShellOutputMode::Stream {
                visible_limit_bytes,
                ..
            } => {
                if self.sent_bytes < visible_limit_bytes {
                    let remaining = visible_limit_bytes - self.sent_bytes;
                    let visible = prefix_by_bytes(chunk, remaining);
                    if !visible.is_empty() {
                        self.sent_bytes += visible.len();
                        output.on_text(&visible).await;
                    }
                    if visible.len() < chunk.len() {
                        self.truncated = true;
                        output
                            .on_text(
                                "\n[shell output truncated; keeping latest output for summary]\n",
                            )
                            .await;
                        output.flush().await;
                    }
                } else if !self.truncated {
                    self.truncated = true;
                    output
                        .on_text("\n[shell output truncated; keeping latest output for summary]\n")
                        .await;
                    output.flush().await;
                }
            }
            ShellOutputMode::Window {
                update_interval, ..
            } => {
                if self.last_window_update.elapsed() >= update_interval {
                    output
                        .on_text_replace(&render_window(
                            command,
                            tail,
                            tail_truncated,
                            started.elapsed(),
                            true,
                        ))
                        .await;
                    self.last_window_update = Instant::now();
                }
            }
        }
    }

    async fn finish(&mut self, command: &str, summary: &ShellRunSummary, output: &dyn AgentOutput) {
        match self.mode {
            ShellOutputMode::Stream { .. } => {
                if self.truncated {
                    output.on_text("\n[shell tail]\n").await;
                    output.on_text(&summary.tail).await;
                    if !summary.tail.ends_with('\n') {
                        output.on_text("\n").await;
                    }
                }
                output.on_text(&render_summary(summary)).await;
                output.flush().await;
            }
            ShellOutputMode::Window { .. } => {
                output
                    .on_text_replace(&render_window(
                        command,
                        &summary.tail,
                        summary.truncated,
                        summary.duration,
                        false,
                    ))
                    .await;
                output.flush().await;
                output.on_text(&render_summary(summary)).await;
                output.flush().await;
            }
        }
    }
}

struct TailBuffer {
    text: String,
    max_chars: usize,
    truncated: bool,
}

impl TailBuffer {
    fn new(max_chars: usize) -> Self {
        Self {
            text: String::new(),
            max_chars,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        let char_count = self.text.chars().count();
        if char_count <= self.max_chars {
            return;
        }

        let drop_chars = char_count - self.max_chars;
        let byte_idx = self
            .text
            .char_indices()
            .nth(drop_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(self.text.len());
        self.text.drain(..byte_idx);
        self.truncated = true;
    }

    fn as_str(&self) -> &str {
        &self.text
    }

    fn truncated(&self) -> bool {
        self.truncated
    }
}

fn normalize_chunk(chunk: &str) -> String {
    chunk.replace('\r', "\n")
}

fn prefix_by_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn render_window(
    command: &str,
    tail: &str,
    truncated: bool,
    duration: Duration,
    running: bool,
) -> String {
    let status = if running { "running" } else { "latest output" };
    let mut rendered = format!(
        "[! {}] {} ({:.1}s)\n",
        display_command(command),
        status,
        duration.as_secs_f32()
    );
    if truncated {
        rendered.push_str("[showing latest output]\n");
    }
    if tail.is_empty() {
        rendered.push_str("[waiting for output]\n");
    } else {
        rendered.push_str(tail);
        if !tail.ends_with('\n') {
            rendered.push('\n');
        }
    }
    rendered
}

fn render_summary(summary: &ShellRunSummary) -> String {
    let status = if summary.cancelled {
        "cancelled"
    } else if summary.timed_out {
        "timed out"
    } else {
        "done"
    };
    let exit_code = summary
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    format!(
        "[shell {}] exit={} duration={:.1}s truncated={}\n",
        status,
        exit_code,
        summary.duration.as_secs_f32(),
        if summary.truncated { "yes" } else { "no" }
    )
}

fn display_command(command: &str) -> String {
    let trimmed = command.trim();
    let mut display: String = trimmed.chars().take(120).collect();
    if trimmed.chars().count() > 120 {
        display.push_str("...");
    }
    display
}

fn shell_escape_enabled(entrypoint: ShellEscapeEntrypoint) -> bool {
    env_bool(entrypoint.env_var())
        .or_else(|| env_bool("RUSTY_CLAW_SHELL_ESCAPE"))
        .unwrap_or_else(|| entrypoint.default_enabled())
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_bool(&value))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestOutput {
        text: Mutex<String>,
        replacements: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl AgentOutput for TestOutput {
        async fn on_text(&self, text: &str) {
            self.text.lock().unwrap().push_str(text);
        }

        async fn on_text_replace(&self, text: &str) {
            self.replacements.lock().unwrap().push(text.to_string());
        }

        async fn on_tool_start(&self, _name: &str, _args: &str) {}

        async fn on_tool_end(&self, _result: &str) {}

        async fn on_error(&self, _error: &str) {}
    }

    #[test]
    fn test_parse_shell_escape() {
        assert!(parse_shell_escape("hello").is_none());
        assert_eq!(parse_shell_escape("!echo hi").unwrap().unwrap(), "echo hi");
        assert_eq!(
            parse_shell_escape("  ! echo hi").unwrap().unwrap(),
            " echo hi"
        );
        assert_eq!(
            parse_shell_escape("!   ").unwrap().unwrap_err(),
            ShellEscapeParseError::EmptyCommand
        );
    }

    #[tokio::test]
    async fn test_run_shell_escape_stream_mode() {
        let output = Arc::new(TestOutput::default());
        let summary = run_shell_escape(
            "printf hello",
            output.clone(),
            ShellOutputMode::Stream {
                visible_limit_bytes: 1024,
                tail_chars: 1024,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(summary.is_success());
        let text = output.text.lock().unwrap().clone();
        assert!(text.contains("hello"));
        assert!(text.contains("[shell done]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cancel_kills_background_child_process() {
        let output = Arc::new(TestOutput::default());
        let marker = std::env::temp_dir().join(format!(
            "rusty_claw_shell_cancel_{}",
            uuid::Uuid::new_v4().simple()
        ));
        let command = format!("sh -c 'sleep 1; echo leaked > {}' & wait", marker.display());
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let summary = run_shell_escape(
            &command,
            output,
            ShellOutputMode::Stream {
                visible_limit_bytes: 1024,
                tail_chars: 1024,
            },
            cancel_token,
        )
        .await
        .unwrap();

        assert!(summary.cancelled);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "background child process survived cancellation"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn test_tail_buffer_truncates() {
        let mut tail = TailBuffer::new(5);
        tail.push("hello");
        tail.push(" world");
        assert_eq!(tail.as_str(), "world");
        assert!(tail.truncated());
    }
}
