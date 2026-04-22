//! Sandbox enforcement for tool execution.
//!
//! Provides OS-level isolation via Bubblewrap (`bwrap`) on Linux and
//! Apple Seatbelt (`sandbox-exec`) on macOS for high-risk tools like
//! `execute_bash`, plus application-level path/network guards for file
//! and web tools.

use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

// ── SandboxLevel ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxLevel {
    /// No isolation at all (legacy behavior, full backward compat).
    #[default]
    Unrestricted,
    /// Filesystem access restrictions; sensitive directories hidden.
    Restricted,
    /// Maximum isolation: filesystem + network + PID namespace.
    Strict,
}

impl SandboxLevel {
    /// Returns the stricter of two levels (monotonic tightening).
    pub fn tighten(self, other: Self) -> Self {
        match (self, other) {
            (Self::Strict, _) | (_, Self::Strict) => Self::Strict,
            (Self::Restricted, _) | (_, Self::Restricted) => Self::Restricted,
            _ => Self::Unrestricted,
        }
    }
}

// ── SandboxViolation ──────────────────────────────────────────────────

/// Describes why a sandbox check failed — designed to produce actionable
/// error messages that LLMs can understand and adapt to.
#[derive(Debug, Clone)]
pub enum SandboxViolation {
    PathDenied {
        path: PathBuf,
        reason: String,
        allowed: Vec<PathBuf>,
    },
    DomainDenied {
        domain: String,
        allowed: Vec<String>,
    },
}

impl fmt::Display for SandboxViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathDenied {
                path,
                reason,
                allowed,
            } => {
                let allowed_str: Vec<String> =
                    allowed.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "Sandbox Violation: Access to `{}` was denied.\n\
                     Reason: {}\n\
                     Allowed writable directories are: [{}]\n\
                     Action needed: Please modify your action to operate within the allowed directories.",
                    path.display(),
                    reason,
                    allowed_str.join(", ")
                )
            }
            Self::DomainDenied { domain, allowed } => {
                write!(
                    f,
                    "Sandbox Violation: Network access to `{}` was denied.\n\
                     Allowed domains are: [{}]\n\
                     Action needed: Please use only the allowed domains for network access.",
                    domain,
                    allowed.join(", ")
                )
            }
        }
    }
}

// ── SandboxPolicy ─────────────────────────────────────────────────────

/// Describes the isolation constraints for a single tool execution.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub level: SandboxLevel,
    /// Paths the process may write to (absolute).
    pub writable_paths: Vec<PathBuf>,
    /// Extra paths mounted read-only inside the sandbox.
    pub readonly_paths: Vec<PathBuf>,
    /// Whether to isolate the network namespace.
    pub isolate_network: bool,
    /// Whether to isolate the PID namespace (Linux only; ignored on macOS).
    pub isolate_pid: bool,
    /// Clear all environment variables before entering the sandbox.
    pub clear_env: bool,
    /// Env vars to preserve when `clear_env` is true.
    pub keep_env: Vec<String>,
    /// Domain allowlist for application-level network guards.
    pub allowed_domains: Vec<String>,
    /// Paths to hide by overlaying with tmpfs (Linux) or explicit deny (macOS).
    pub hidden_paths: Vec<PathBuf>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            level: SandboxLevel::Unrestricted,
            writable_paths: Vec::new(),
            readonly_paths: Vec::new(),
            isolate_network: false,
            isolate_pid: false,
            clear_env: false,
            keep_env: Vec::new(),
            allowed_domains: Vec::new(),
            hidden_paths: Vec::new(),
        }
    }
}

impl SandboxPolicy {
    /// Produce a child policy by tightening: the child can only be
    /// *more* restrictive than `self`, never less.
    pub fn tighten(&self, child: &SandboxPolicy) -> SandboxPolicy {
        SandboxPolicy {
            level: self.level.tighten(child.level),
            // Child writable paths must be a subset of parent writable paths
            // (or explicitly overridden when parent is Unrestricted)
            writable_paths: if self.level == SandboxLevel::Unrestricted {
                child.writable_paths.clone()
            } else {
                child
                    .writable_paths
                    .iter()
                    .filter(|p| {
                        self.writable_paths
                            .iter()
                            .any(|parent_p| p.starts_with(parent_p))
                    })
                    .cloned()
                    .collect()
            },
            readonly_paths: {
                let mut merged = self.readonly_paths.clone();
                merged.extend(child.readonly_paths.iter().cloned());
                merged.dedup();
                merged
            },
            isolate_network: self.isolate_network || child.isolate_network,
            isolate_pid: self.isolate_pid || child.isolate_pid,
            clear_env: self.clear_env || child.clear_env,
            keep_env: if child.clear_env {
                // Intersection: child must not gain env vars parent already cleared
                child
                    .keep_env
                    .iter()
                    .filter(|k| !self.clear_env || self.keep_env.contains(k))
                    .cloned()
                    .collect()
            } else {
                self.keep_env.clone()
            },
            allowed_domains: if child.allowed_domains.is_empty() {
                self.allowed_domains.clone()
            } else if self.allowed_domains.is_empty() {
                child.allowed_domains.clone()
            } else {
                // Intersection
                child
                    .allowed_domains
                    .iter()
                    .filter(|d| self.allowed_domains.contains(d))
                    .cloned()
                    .collect()
            },
            hidden_paths: {
                let mut merged = self.hidden_paths.clone();
                merged.extend(child.hidden_paths.iter().cloned());
                merged.dedup();
                merged
            },
        }
    }

    /// Produce a terse summary for injection into the LLM system prompt.
    pub fn to_prompt_summary(&self) -> String {
        if self.level == SandboxLevel::Unrestricted {
            return String::new();
        }

        let mut lines = Vec::new();
        lines.push(format!("- Sandbox Level: {:?}", self.level));

        if !self.writable_paths.is_empty() {
            let paths: Vec<String> = self
                .writable_paths
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect();
            lines.push(format!("- Permitted WorkDirs: [{}]", paths.join(", ")));
        }

        if self.isolate_network && !self.allowed_domains.is_empty() {
            lines.push(format!(
                "- Network: Shell commands are offline; web tools are restricted to domains [{}]",
                self.allowed_domains.join(", ")
            ));
        } else if self.isolate_network {
            lines.push("- Network: Shell commands are offline".to_string());
        } else if !self.allowed_domains.is_empty() {
            lines.push(format!(
                "- Network: Web tools are restricted to domains [{}]",
                self.allowed_domains.join(", ")
            ));
        }

        if self.isolate_pid {
            if cfg!(target_os = "macos") {
                lines.push(
                    "- PID Namespace: Isolation requested but not supported on macOS (ignored)"
                        .to_string(),
                );
            } else {
                lines.push("- PID Namespace: Isolated".to_string());
            }
        }

        if !self.hidden_paths.is_empty() {
            lines.push(format!(
                "- Hidden Paths: {} sensitive directories are inaccessible",
                self.hidden_paths.len()
            ));
        }

        lines.join("\n")
    }
}

// ── OsSandbox (platform-specific backend) ────────────────────────────

/// The detected OS-level sandbox backend.
// Variants and helpers for non-current platforms are intentionally compiled in
// for cross-platform consistency; suppress dead_code warnings.
#[allow(dead_code)]
#[derive(Debug, Clone)]
enum OsSandbox {
    /// No OS-level sandbox available on this system.
    Unavailable,
    /// Linux: Bubblewrap (`bwrap`).
    Bwrap(PathBuf),
    /// macOS: Apple Seatbelt (`sandbox-exec`).
    SeatbeltExec(PathBuf),
}

impl OsSandbox {
    fn is_available(&self) -> bool {
        !matches!(self, OsSandbox::Unavailable)
    }

    fn detect() -> Self {
        // Linux: look for bwrap
        #[cfg(target_os = "linux")]
        if let Some(p) = Self::find_bwrap() {
            tracing::info!("Sandbox: bwrap detected at {}", p.display());
            return Self::Bwrap(p);
        }

        // macOS: look for sandbox-exec (Apple Seatbelt)
        #[cfg(target_os = "macos")]
        if let Some(p) = Self::find_sandbox_exec() {
            tracing::info!("Sandbox: sandbox-exec detected at {}", p.display());
            return Self::SeatbeltExec(p);
        }

        tracing::warn!(
            "Sandbox: no OS-level sandbox backend found ({}). \
             OS-level isolation is disabled.",
            Self::platform_name()
        );
        Self::Unavailable
    }

    fn platform_name() -> &'static str {
        if cfg!(target_os = "linux") {
            "Linux/bwrap"
        } else if cfg!(target_os = "macos") {
            "macOS/sandbox-exec"
        } else {
            "unsupported platform"
        }
    }

    fn unavailable_description() -> &'static str {
        if cfg!(target_os = "linux") {
            "Bubblewrap (`bwrap`) is unavailable"
        } else if cfg!(target_os = "macos") {
            "`sandbox-exec` is unavailable"
        } else {
            "OS-level sandbox is not supported on this platform"
        }
    }

    fn install_hint() -> &'static str {
        if cfg!(target_os = "linux") {
            "Install with: apt install bubblewrap"
        } else if cfg!(target_os = "macos") {
            "sandbox-exec should be present at /usr/bin/sandbox-exec; set sandbox.level = \"off\" to disable"
        } else {
            "Set sandbox.level = \"off\" to disable the sandbox requirement"
        }
    }

    #[allow(dead_code)]
    fn find_bwrap() -> Option<PathBuf> {
        let output = std::process::Command::new("which")
            .arg("bwrap")
            .output()
            .ok()?;
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        for candidate in &["/usr/bin/bwrap", "/usr/local/bin/bwrap"] {
            if Path::new(candidate).exists() {
                return Some(PathBuf::from(candidate));
            }
        }
        None
    }

    fn find_sandbox_exec() -> Option<PathBuf> {
        let p = PathBuf::from("/usr/bin/sandbox-exec");
        p.exists().then_some(p)
    }
}

// ── SandboxEnforcer ───────────────────────────────────────────────────

/// The sandbox execution engine. Detects the platform-appropriate sandbox
/// backend at startup, wraps commands, and enforces path/network guards.
#[derive(Debug, Clone)]
pub struct SandboxEnforcer {
    os_sandbox: OsSandbox,
    default_policy: SandboxPolicy,
}

impl SandboxEnforcer {
    /// Probe the system for an OS-level sandbox backend and build the
    /// enforcer with the given default policy.
    pub fn detect(default_policy: SandboxPolicy) -> Self {
        let os_sandbox = OsSandbox::detect();
        Self {
            os_sandbox,
            default_policy,
        }
    }

    /// Create an enforcer without probing (for tests or when sandbox is
    /// explicitly disabled).
    pub fn disabled() -> Self {
        Self::disabled_with_policy(SandboxPolicy::default())
    }

    pub fn disabled_with_policy(default_policy: SandboxPolicy) -> Self {
        Self {
            os_sandbox: OsSandbox::Unavailable,
            default_policy,
        }
    }

    /// Whether OS-level isolation is available on this system.
    pub fn is_available(&self) -> bool {
        self.os_sandbox.is_available()
    }

    /// The global default policy (from config).
    pub fn default_policy(&self) -> &SandboxPolicy {
        &self.default_policy
    }

    pub fn prompt_summary(&self) -> String {
        let mut summary = self.default_policy.to_prompt_summary();
        if self.default_policy.level != SandboxLevel::Unrestricted && !self.is_available() {
            if !summary.is_empty() {
                summary.push('\n');
            }
            summary.push_str(&format!(
                "- Shell Execution: Disabled because {}. \
                 File and web tool restrictions still apply.",
                OsSandbox::unavailable_description()
            ));
        }
        summary
    }

    pub fn shell_execution_error(&self) -> String {
        format!(
            "Sandbox is enabled, but {}. Shell execution is disabled to avoid running \
             commands outside the sandbox. {}",
            OsSandbox::unavailable_description(),
            OsSandbox::install_hint(),
        )
    }

    // ── Command wrapping ──────────────────────────────────────────

    /// Build a sandboxed PTY command for `portable-pty` spawning.
    /// Panics if no sandbox backend is available; callers must check
    /// `is_available()` first.
    pub fn build_pty_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> portable_pty::CommandBuilder {
        match &self.os_sandbox {
            OsSandbox::Bwrap(_) => {
                let args = self.build_bwrap_args(cmd, policy, cwd);
                let mut builder = portable_pty::CommandBuilder::new(&args[0]);
                for arg in &args[1..] {
                    builder.arg(arg);
                }
                // bwrap handles cwd via --chdir; don't set it on the outer process
                builder
            }
            OsSandbox::SeatbeltExec(_) => {
                let args = self.build_seatbelt_args(cmd, policy, cwd);
                let mut builder = portable_pty::CommandBuilder::new(&args[0]);
                for arg in &args[1..] {
                    builder.arg(arg);
                }
                // sandbox-exec inherits the outer process cwd
                builder.cwd(cwd);
                builder
            }
            OsSandbox::Unavailable => {
                panic!("build_pty_command called but no sandbox backend is available")
            }
        }
    }

    /// Build a `std::process::Command` for non-PTY execution inside the sandbox.
    pub fn build_std_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> std::process::Command {
        match &self.os_sandbox {
            OsSandbox::Bwrap(_) => {
                let args = self.build_bwrap_args(cmd, policy, cwd);
                let mut command = std::process::Command::new(&args[0]);
                for arg in &args[1..] {
                    command.arg(arg);
                }
                command
            }
            OsSandbox::SeatbeltExec(_) => {
                let args = self.build_seatbelt_args(cmd, policy, cwd);
                let mut command = std::process::Command::new(&args[0]);
                for arg in &args[1..] {
                    command.arg(arg);
                }
                command.current_dir(cwd);
                command
            }
            OsSandbox::Unavailable => {
                panic!("build_std_command called but no sandbox backend is available")
            }
        }
    }

    /// Build a `tokio::process::Command` for async non-PTY execution.
    pub fn build_tokio_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> tokio::process::Command {
        match &self.os_sandbox {
            OsSandbox::Bwrap(_) => {
                let args = self.build_bwrap_args(cmd, policy, cwd);
                let mut command = tokio::process::Command::new(&args[0]);
                for arg in &args[1..] {
                    command.arg(arg);
                }
                command
            }
            OsSandbox::SeatbeltExec(_) => {
                let args = self.build_seatbelt_args(cmd, policy, cwd);
                let mut command = tokio::process::Command::new(&args[0]);
                for arg in &args[1..] {
                    command.arg(arg);
                }
                command.current_dir(cwd);
                command
            }
            OsSandbox::Unavailable => {
                panic!("build_tokio_command called but no sandbox backend is available")
            }
        }
    }

    // ── Linux: Bubblewrap argument builder ───────────────────────

    fn build_bwrap_args(&self, cmd: &str, policy: &SandboxPolicy, cwd: &Path) -> Vec<String> {
        let bwrap = match &self.os_sandbox {
            OsSandbox::Bwrap(p) => p,
            _ => panic!("build_bwrap_args called but bwrap is not available"),
        };

        let mut args: Vec<String> = Vec::new();
        args.push(bwrap.display().to_string());

        // ── Base filesystem (read-only) ───────────────────────────
        for sys_dir in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
            if Path::new(sys_dir).exists() {
                args.extend(["--ro-bind".into(), (*sys_dir).into(), (*sys_dir).into()]);
            }
        }

        // /proc and /dev (minimal)
        args.extend(["--proc".into(), "/proc".into()]);
        args.extend(["--dev".into(), "/dev".into()]);

        // /tmp → session-scoped temporary directory
        let session_tmp = cwd.join(".tmp");
        let _ = std::fs::create_dir_all(&session_tmp);
        args.extend([
            "--bind".into(),
            session_tmp.display().to_string(),
            "/tmp".into(),
        ]);

        // ── Environment variable isolation ────────────────────────
        if policy.clear_env {
            args.push("--clearenv".into());
            for env_key in &policy.keep_env {
                if let Ok(val) = std::env::var(env_key) {
                    args.extend(["--setenv".into(), env_key.clone(), val]);
                }
            }
        }

        // ── Working directory (read-write) ────────────────────────
        let cwd_str = cwd.display().to_string();
        args.extend(["--bind".into(), cwd_str.clone(), cwd_str.clone()]);

        // ── Extra writable paths ──────────────────────────────────
        for path in &policy.writable_paths {
            let p = path.display().to_string();
            args.extend(["--bind".into(), p.clone(), p]);
        }

        // ── Extra read-only paths ─────────────────────────────────
        for path in &policy.readonly_paths {
            let p = path.display().to_string();
            args.extend(["--ro-bind".into(), p.clone(), p]);
        }

        // ── Hide sensitive paths ──────────────────────────────────
        for path in &policy.hidden_paths {
            if path.exists() {
                args.extend(["--tmpfs".into(), path.display().to_string()]);
            }
        }

        // ── Network isolation ─────────────────────────────────────
        if policy.isolate_network {
            args.push("--unshare-net".into());
        }

        // ── PID isolation ─────────────────────────────────────────
        if policy.isolate_pid {
            args.push("--unshare-pid".into());
        }

        // ── Security hardening ────────────────────────────────────
        args.push("--die-with-parent".into());
        args.push("--new-session".into());

        // ── Set CWD inside the sandbox ────────────────────────────
        args.extend(["--chdir".into(), cwd_str]);

        // ── Actual command ────────────────────────────────────────
        args.extend(["--".into(), "bash".into(), "-c".into(), cmd.into()]);

        args
    }

    // ── macOS: Apple Seatbelt argument builder ────────────────────

    /// Build the `sandbox-exec` argument list for macOS.
    fn build_seatbelt_args(&self, cmd: &str, policy: &SandboxPolicy, cwd: &Path) -> Vec<String> {
        let sandbox_exec = match &self.os_sandbox {
            OsSandbox::SeatbeltExec(p) => p,
            _ => panic!("build_seatbelt_args called but sandbox-exec is not available"),
        };

        if policy.isolate_pid {
            tracing::debug!("Sandbox: isolate_pid is not supported on macOS and will be ignored");
        }

        // Create a session-scoped tmp dir (mirrors bwrap's per-session /tmp bind).
        // TMPDIR is redirected here so sandboxed processes don't share the
        // global /private/tmp with other users/processes.
        let session_tmp = cwd.join(".tmp");
        let _ = std::fs::create_dir_all(&session_tmp);

        let profile = Self::build_sbpl_profile(policy, cwd, &session_tmp);

        let mut args = vec![sandbox_exec.display().to_string(), "-p".into(), profile];

        // Use `env` to set TMPDIR and optionally clear the environment.
        // Without -i, `env` only adds/overrides the listed variables.
        args.push("/usr/bin/env".into());
        if policy.clear_env {
            args.push("-i".into());
        }
        args.push(format!("TMPDIR={}", session_tmp.display()));
        if policy.clear_env {
            for env_key in &policy.keep_env {
                if let Ok(val) = std::env::var(env_key) {
                    args.push(format!("{}={}", env_key, val));
                }
            }
        }

        args.extend(["bash".into(), "-c".into(), cmd.into()]);
        args
    }

    /// Generate an SBPL (Sandbox Profile Language) profile string that
    /// implements the given `SandboxPolicy` for Apple Seatbelt.
    ///
    /// `session_tmp` is the per-session temporary directory (cwd/.tmp);
    /// it is the only writable path under /tmp so the sandbox cannot
    /// interfere with other processes' temp files.
    fn build_sbpl_profile(policy: &SandboxPolicy, cwd: &Path, session_tmp: &Path) -> String {
        // Minimal Mach services required for basic bash execution.
        // com.apple.SecurityServer is intentionally excluded so that
        // sandboxed commands cannot access the user's Keychain.
        let mach_services = [
            "com.apple.system.logger",
            "com.apple.system.opendirectoryd.api",
            "com.apple.system.opendirectoryd.membership",
            "com.apple.system.DirectoryService.libinfo_v1",
            "com.apple.bsd.dirhelper",
        ];
        let mach_block = format!(
            "(allow mach-lookup\n{})",
            mach_services
                .iter()
                .map(|s| format!("    (global-name \"{s}\")"))
                .collect::<Vec<_>>()
                .join("\n")
        );

        let mut lines: Vec<String> = vec![
            "(version 1)".into(),
            "(deny default)".into(),
            // Process operations
            "(allow process-exec)".into(),
            "(allow process-fork)".into(),
            "(allow signal (target self))".into(),
            "(allow sysctl-read)".into(),
            // Read the root directory itself (required for basic path resolution)
            r#"(allow file-read-data (literal "/"))"#.into(),
            // Minimal Mach IPC (see allowlist above; no SecurityServer = no Keychain)
            mach_block,
            "(allow ipc-posix-shm)".into(),
            // Core system paths (read-only)
            r#"(allow file-read* (subpath "/usr"))"#.into(),
            r#"(allow file-read* (subpath "/bin"))"#.into(),
            r#"(allow file-read* (subpath "/sbin"))"#.into(),
            r#"(allow file-read* (subpath "/System"))"#.into(),
            r#"(allow file-read* (subpath "/Library/Apple"))"#.into(),
            r#"(allow file-read* (subpath "/private/etc"))"#.into(),
            // Homebrew on Apple Silicon (/opt/homebrew) and Intel (/usr/local)
            r#"(allow file-read* (subpath "/opt"))"#.into(),
            r#"(allow file-read* (subpath "/usr/local"))"#.into(),
            // Filesystem metadata everywhere (needed for ls, find, stat, etc.)
            r#"(allow file-read-metadata (subpath "/"))"#.into(),
            // Minimal /dev access. PTY file descriptors are opened by the
            // parent before sandbox-exec applies the profile, so avoid
            // granting broad host /dev access.
            r#"(allow file-read* file-write* (literal "/dev/null"))"#.into(),
            r#"(allow file-read* (literal "/dev/zero"))"#.into(),
            r#"(allow file-read* (literal "/dev/random"))"#.into(),
            r#"(allow file-read* (literal "/dev/urandom"))"#.into(),
            r#"(allow file-read* file-write* (literal "/dev/tty"))"#.into(),
            r#"(allow file-ioctl (literal "/dev/tty"))"#.into(),
        ];

        // Session-scoped tmp: only this dir is writable under /tmp,
        // preventing interference with other processes' temp files.
        // TMPDIR is set to this path in build_seatbelt_args.
        lines.push(format!(
            r#"(allow file-read* file-write* (subpath "{}"))"#,
            sbpl_escape(&session_tmp.display().to_string())
        ));

        // Working directory (read+write)
        let cwd_str = cwd.display().to_string();
        lines.push(format!(
            r#"(allow file-read* file-write* (subpath "{}"))"#,
            sbpl_escape(&cwd_str)
        ));

        // Extra writable paths
        for path in &policy.writable_paths {
            let p = path.display().to_string();
            if p != cwd_str {
                lines.push(format!(
                    r#"(allow file-read* file-write* (subpath "{}"))"#,
                    sbpl_escape(&p)
                ));
            }
        }

        // Extra read-only paths
        for path in &policy.readonly_paths {
            let p = path.display().to_string();
            lines.push(format!(
                r#"(allow file-read* (subpath "{}"))"#,
                sbpl_escape(&p)
            ));
        }

        // Hidden paths: explicit deny overrides any broader allow above.
        // SBPL evaluates rules in order and last-match-wins, so placing
        // these denies after the allows ensures they take effect.
        for path in &policy.hidden_paths {
            let p = path.display().to_string();
            lines.push(format!(
                r#"(deny file-read* file-write* (subpath "{}"))"#,
                sbpl_escape(&p)
            ));
        }

        // Network: denied by default (from `deny default`).
        // Explicitly allow when not isolating.
        if !policy.isolate_network {
            lines.push("(allow network*)".into());
        }

        lines.join("\n")
    }

    // ── Path guard ────────────────────────────────────────────────

    /// Check whether a file access is permitted by the policy.
    pub fn check_path_access(
        &self,
        path: &Path,
        write: bool,
        policy: &SandboxPolicy,
    ) -> Result<(), SandboxViolation> {
        if policy.level == SandboxLevel::Unrestricted {
            return Ok(());
        }

        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let canonical = std::fs::canonicalize(&candidate).unwrap_or(candidate);

        // Always block hidden (sensitive) paths
        for hidden in &policy.hidden_paths {
            let hidden_canonical = std::fs::canonicalize(hidden).unwrap_or_else(|_| hidden.clone());
            if canonical.starts_with(&hidden_canonical) {
                tracing::warn!(
                    "Sandbox: Blocked {} access to protected path '{}'",
                    if write { "write" } else { "read" },
                    path.display()
                );
                return Err(SandboxViolation::PathDenied {
                    path: path.to_path_buf(),
                    reason: "This path is in a protected sensitive directory.".to_string(),
                    allowed: policy.writable_paths.clone(),
                });
            }
        }

        if !write {
            // Read access: only blocked for hidden paths (already checked above)
            return Ok(());
        }

        // Write access: must be under a writable path
        let allowed = policy.writable_paths.iter().any(|allowed_path| {
            let allowed_canonical =
                std::fs::canonicalize(allowed_path).unwrap_or_else(|_| allowed_path.clone());
            canonical.starts_with(&allowed_canonical)
        });

        if allowed {
            Ok(())
        } else {
            tracing::warn!(
                "Sandbox: Blocked write access to '{}' outside writable paths",
                path.display()
            );
            Err(SandboxViolation::PathDenied {
                path: path.to_path_buf(),
                reason: "This environment is sandboxed. Write access is restricted.".to_string(),
                allowed: policy.writable_paths.clone(),
            })
        }
    }

    // ── Network guard ─────────────────────────────────────────────

    /// Check whether a URL is permitted by the domain allowlist.
    pub fn check_network_access(
        &self,
        url: &str,
        policy: &SandboxPolicy,
    ) -> Result<(), SandboxViolation> {
        if policy.level == SandboxLevel::Unrestricted || policy.allowed_domains.is_empty() {
            return Ok(());
        }

        let domain = extract_domain(url);
        if domain.is_empty() {
            return Err(SandboxViolation::DomainDenied {
                domain: url.to_string(),
                allowed: policy.allowed_domains.clone(),
            });
        }

        let allowed = policy
            .allowed_domains
            .iter()
            .any(|d| domain == *d || domain.ends_with(&format!(".{}", d)));

        if allowed {
            Ok(())
        } else {
            tracing::warn!(
                "Sandbox: Blocked network access to domain '{}' (URL: {})",
                domain,
                url
            );
            Err(SandboxViolation::DomainDenied {
                domain,
                allowed: policy.allowed_domains.clone(),
            })
        }
    }
}

// ── SBPL helpers ──────────────────────────────────────────────────────

/// Escape a path string for embedding in an SBPL profile (double-quoted).
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Extract the domain from a URL string.
fn extract_domain(url: &str) -> String {
    let without_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    without_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

// ── SandboxConfig ─────────────────────────────────────────────────────

/// Configuration from `config.toml [sandbox]`.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct SandboxConfig {
    /// Global level: "off" | "restricted" | "strict"
    pub level: Option<String>,
    /// Block startup if the OS sandbox backend is missing.
    pub require_os_sandbox: Option<bool>,
    /// Extra writable directories.
    pub writable_paths: Option<Vec<String>>,
    /// Sensitive directories to hide.
    pub hidden_paths: Option<Vec<String>>,
    /// Domain allowlist for web tools.
    pub allowed_domains: Option<Vec<String>>,
    /// Bash-specific overrides.
    pub bash: Option<BashSandboxConfig>,
    /// Subagent-specific overrides.
    pub subagent: Option<SubagentSandboxConfig>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct BashSandboxConfig {
    pub isolate_network: Option<bool>,
    pub isolate_pid: Option<bool>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SubagentSandboxConfig {
    pub isolate_network: Option<bool>,
    pub force_strict: Option<bool>,
}

impl SandboxConfig {
    /// Parse the string level into `SandboxLevel`.
    pub fn parsed_level(&self) -> SandboxLevel {
        match self.level.as_deref() {
            Some("strict") => SandboxLevel::Strict,
            Some("restricted") => SandboxLevel::Restricted,
            Some("off") | None => SandboxLevel::Unrestricted,
            Some(other) => {
                tracing::warn!(
                    "Unknown sandbox level '{}', falling back to unrestricted",
                    other
                );
                SandboxLevel::Unrestricted
            }
        }
    }

    /// Resolve the tilde `~` in hidden_paths to the actual home directory.
    fn expand_tilde(path: &str) -> PathBuf {
        if let Some(rest) = path.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        PathBuf::from(path)
    }

    /// Build the default `SandboxPolicy` from this config, using the given
    /// working directory as the primary writable path.
    pub fn build_default_policy(&self, work_dir: &Path) -> SandboxPolicy {
        let level = self.parsed_level();
        if level == SandboxLevel::Unrestricted {
            return SandboxPolicy::default();
        }

        let mut writable_paths = vec![work_dir.to_path_buf()];
        if let Some(extra) = &self.writable_paths {
            writable_paths.extend(extra.iter().map(|p| Self::expand_tilde(p)));
        }

        let default_hidden = ["~/.ssh", "~/.aws", "~/.gnupg", "~/.config/rusty-claw"];
        let hidden_paths: Vec<PathBuf> = self
            .hidden_paths
            .as_deref()
            .unwrap_or(
                &default_hidden
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .iter()
            .map(|p| Self::expand_tilde(p))
            .collect();

        let allowed_domains = self.allowed_domains.clone().unwrap_or_else(|| {
            vec![
                "github.com".into(),
                "raw.githubusercontent.com".into(),
                "docs.rs".into(),
                "crates.io".into(),
                "api.tavily.com".into(),
            ]
        });

        SandboxPolicy {
            level,
            writable_paths,
            readonly_paths: Vec::new(),
            isolate_network: level == SandboxLevel::Strict,
            isolate_pid: self
                .bash
                .as_ref()
                .and_then(|b| b.isolate_pid)
                .unwrap_or(true),
            clear_env: level == SandboxLevel::Strict,
            keep_env: vec![
                "PATH".into(),
                "TERM".into(),
                "HOME".into(),
                "USER".into(),
                "LANG".into(),
            ],
            allowed_domains,
            hidden_paths,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Test constructors that bypass OS detection for platform-agnostic tests.
    impl SandboxEnforcer {
        fn new_bwrap_for_testing(bwrap_path: PathBuf, policy: SandboxPolicy) -> Self {
            Self {
                os_sandbox: OsSandbox::Bwrap(bwrap_path),
                default_policy: policy,
            }
        }

        fn new_seatbelt_for_testing(sandbox_exec_path: PathBuf, policy: SandboxPolicy) -> Self {
            Self {
                os_sandbox: OsSandbox::SeatbeltExec(sandbox_exec_path),
                default_policy: policy,
            }
        }
    }

    #[test]
    fn test_sandbox_level_tighten_monotonic() {
        assert_eq!(
            SandboxLevel::Unrestricted.tighten(SandboxLevel::Restricted),
            SandboxLevel::Restricted
        );
        assert_eq!(
            SandboxLevel::Restricted.tighten(SandboxLevel::Strict),
            SandboxLevel::Strict
        );
        // Cannot relax
        assert_eq!(
            SandboxLevel::Strict.tighten(SandboxLevel::Unrestricted),
            SandboxLevel::Strict
        );
    }

    #[test]
    fn test_extract_domain() {
        assert_eq!(extract_domain("https://github.com/foo/bar"), "github.com");
        assert_eq!(
            extract_domain("http://api.tavily.com:443/search"),
            "api.tavily.com"
        );
        assert_eq!(extract_domain("ftp://example.com"), "example.com");
    }

    #[test]
    fn test_check_network_access_allows_whitelisted() {
        let enforcer = SandboxEnforcer::disabled();
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            allowed_domains: vec!["github.com".into(), "docs.rs".into()],
            ..Default::default()
        };
        assert!(enforcer
            .check_network_access("https://github.com/repo", &policy)
            .is_ok());
        assert!(enforcer
            .check_network_access("https://docs.rs/crate", &policy)
            .is_ok());
    }

    #[test]
    fn test_check_network_access_blocks_unknown() {
        let enforcer = SandboxEnforcer::disabled();
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            allowed_domains: vec!["github.com".into()],
            ..Default::default()
        };
        let result = enforcer.check_network_access("https://evil.com/steal", &policy);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_network_access_allows_subdomain() {
        let enforcer = SandboxEnforcer::disabled();
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            allowed_domains: vec!["github.com".into()],
            ..Default::default()
        };
        assert!(enforcer
            .check_network_access("https://raw.githubusercontent.com/file", &policy)
            .is_err());
        // But if we add the subdomain explicitly:
        let policy2 = SandboxPolicy {
            level: SandboxLevel::Restricted,
            allowed_domains: vec!["github.com".into(), "raw.githubusercontent.com".into()],
            ..Default::default()
        };
        assert!(enforcer
            .check_network_access("https://raw.githubusercontent.com/file", &policy2)
            .is_ok());
    }

    #[test]
    fn test_check_network_unrestricted_level_allows_all() {
        let enforcer = SandboxEnforcer::disabled();
        let policy = SandboxPolicy {
            level: SandboxLevel::Unrestricted,
            allowed_domains: vec!["github.com".into()],
            ..Default::default()
        };
        assert!(enforcer
            .check_network_access("https://evil.com/steal", &policy)
            .is_ok());
    }

    #[test]
    fn test_check_path_access_blocks_hidden() {
        let enforcer = SandboxEnforcer::disabled();
        let tmp = std::env::temp_dir().join("fake_ssh");
        let _ = std::fs::create_dir_all(&tmp);
        let test_file = tmp.join("id_rsa");
        let _ = std::fs::write(&test_file, "secret");

        let result = enforcer.check_path_access(
            &test_file,
            false,
            &SandboxPolicy {
                level: SandboxLevel::Restricted,
                hidden_paths: vec![tmp.clone()],
                ..Default::default()
            },
        );
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_check_path_unrestricted_allows_all() {
        let enforcer = SandboxEnforcer::disabled();
        let policy = SandboxPolicy::default(); // Unrestricted
        assert!(enforcer
            .check_path_access(Path::new("/etc/passwd"), true, &policy)
            .is_ok());
    }

    #[test]
    fn test_check_path_access_blocks_global_tmp_write_by_default() {
        let enforcer = SandboxEnforcer::disabled();
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![PathBuf::from("/workspace")],
            ..Default::default()
        };
        let result = enforcer.check_path_access(
            Path::new("/private/tmp/rusty_claw_global_tmp_probe"),
            true,
            &policy,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_path_access_allows_new_relative_file_under_cwd() {
        let enforcer = SandboxEnforcer::disabled();
        let cwd = std::env::current_dir().unwrap();
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![cwd],
            ..Default::default()
        };
        let result =
            enforcer.check_path_access(Path::new("new_sandbox_relative_file.txt"), true, &policy);
        assert!(result.is_ok());
    }

    #[test]
    fn test_policy_tighten_cannot_relax() {
        let parent = SandboxPolicy {
            level: SandboxLevel::Strict,
            isolate_network: true,
            isolate_pid: true,
            ..Default::default()
        };
        let child = SandboxPolicy {
            level: SandboxLevel::Unrestricted,
            isolate_network: false,
            isolate_pid: false,
            ..Default::default()
        };
        let result = parent.tighten(&child);
        assert_eq!(result.level, SandboxLevel::Strict);
        assert!(result.isolate_network);
        assert!(result.isolate_pid);
    }

    #[test]
    fn test_policy_to_prompt_summary_unrestricted() {
        let policy = SandboxPolicy::default();
        assert!(policy.to_prompt_summary().is_empty());
    }

    #[test]
    fn test_policy_to_prompt_summary_restricted() {
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![PathBuf::from("/workspace")],
            isolate_network: true,
            allowed_domains: vec!["github.com".into()],
            hidden_paths: vec![PathBuf::from("/home/user/.ssh")],
            ..Default::default()
        };
        let summary = policy.to_prompt_summary();
        assert!(summary.contains("Restricted"));
        assert!(summary.contains("/workspace"));
        assert!(summary.contains("Shell commands are offline"));
        assert!(summary.contains("web tools are restricted to domains [github.com]"));
        assert!(summary.contains("sensitive directories"));
    }

    #[test]
    fn test_enforcer_prompt_summary_mentions_shell_disabled_without_sandbox() {
        let enforcer = SandboxEnforcer::disabled_with_policy(SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![PathBuf::from("/workspace")],
            allowed_domains: vec!["github.com".into()],
            ..Default::default()
        });
        let summary = enforcer.prompt_summary();
        assert!(summary.contains("Shell Execution: Disabled"));
        assert!(summary.contains("github.com"));
    }

    #[test]
    fn test_build_bwrap_args_basic_shape() {
        let enforcer = SandboxEnforcer::new_bwrap_for_testing(
            PathBuf::from("/usr/bin/bwrap"),
            SandboxPolicy::default(),
        );
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            isolate_pid: true,
            clear_env: true,
            keep_env: vec!["PATH".into()],
            hidden_paths: vec![],
            ..Default::default()
        };
        let args = enforcer.build_bwrap_args("echo hello", &policy, Path::new("/tmp/test_cwd"));
        assert_eq!(args[0], "/usr/bin/bwrap");
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        let last_three: Vec<&str> = args[args.len() - 3..].iter().map(|s| s.as_str()).collect();
        assert_eq!(last_three, vec!["bash", "-c", "echo hello"]);
    }

    #[test]
    fn test_build_seatbelt_args_basic_shape() {
        let enforcer = SandboxEnforcer::new_seatbelt_for_testing(
            PathBuf::from("/usr/bin/sandbox-exec"),
            SandboxPolicy::default(),
        );
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            isolate_network: false,
            hidden_paths: vec![],
            ..Default::default()
        };
        let args = enforcer.build_seatbelt_args("echo hello", &policy, Path::new("/tmp/test_cwd"));
        assert_eq!(args[0], "/usr/bin/sandbox-exec");
        assert_eq!(args[1], "-p");
        // args[2] is the SBPL profile string
        let profile = &args[2];
        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow network*)"));
        // cwd and session tmp both present
        assert!(profile.contains("/tmp/test_cwd"));
        assert!(profile.contains("/tmp/test_cwd/.tmp"));
        // No global /private/tmp access
        assert!(!profile.contains("subpath \"/private/tmp\""));
        // No unrestricted mach-lookup
        assert!(!profile.contains("(allow mach-lookup)"));
        // No mach-register
        assert!(!profile.contains("mach-register"));
        // Needs file-read-data "/" for basic path resolution
        assert!(profile.contains("file-read-data (literal \"/\")"));
        // env is used to set TMPDIR
        assert!(args.contains(&"/usr/bin/env".to_string()));
        assert!(args.iter().any(|a| a.starts_with("TMPDIR=")));
        // Last three args should be the shell invocation
        let last_three: Vec<&str> = args[args.len() - 3..].iter().map(|s| s.as_str()).collect();
        assert_eq!(last_three, vec!["bash", "-c", "echo hello"]);
    }

    #[test]
    fn test_build_seatbelt_args_network_isolated() {
        let enforcer = SandboxEnforcer::new_seatbelt_for_testing(
            PathBuf::from("/usr/bin/sandbox-exec"),
            SandboxPolicy::default(),
        );
        let policy = SandboxPolicy {
            level: SandboxLevel::Strict,
            isolate_network: true,
            hidden_paths: vec![],
            ..Default::default()
        };
        let args = enforcer.build_seatbelt_args("echo hello", &policy, Path::new("/tmp/test_cwd"));
        let profile = &args[2];
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn test_build_seatbelt_args_clear_env() {
        let enforcer = SandboxEnforcer::new_seatbelt_for_testing(
            PathBuf::from("/usr/bin/sandbox-exec"),
            SandboxPolicy::default(),
        );
        let policy = SandboxPolicy {
            level: SandboxLevel::Strict,
            clear_env: true,
            keep_env: vec!["PATH".into()],
            hidden_paths: vec![],
            ..Default::default()
        };
        let args = enforcer.build_seatbelt_args("echo hello", &policy, Path::new("/tmp/test_cwd"));
        // env is always present (for TMPDIR), with -i when clear_env is set
        assert!(args.contains(&"/usr/bin/env".to_string()));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.iter().any(|a| a.starts_with("TMPDIR=")));
    }

    #[test]
    fn test_sbpl_profile_contains_hidden_path_deny() {
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            hidden_paths: vec![PathBuf::from("/Users/test/.ssh")],
            ..Default::default()
        };
        let profile = SandboxEnforcer::build_sbpl_profile(
            &policy,
            Path::new("/workspace"),
            Path::new("/workspace/.tmp"),
        );
        assert!(profile.contains(r#"(deny file-read* file-write* (subpath "/Users/test/.ssh"))"#));
    }

    #[test]
    fn test_sbpl_profile_writable_and_readonly_paths() {
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![PathBuf::from("/data/output")],
            readonly_paths: vec![PathBuf::from("/data/input")],
            hidden_paths: vec![],
            ..Default::default()
        };
        let profile = SandboxEnforcer::build_sbpl_profile(
            &policy,
            Path::new("/workspace"),
            Path::new("/workspace/.tmp"),
        );
        assert!(profile.contains(r#"(allow file-read* file-write* (subpath "/data/output"))"#));
        assert!(profile.contains(r#"(allow file-read* (subpath "/data/input"))"#));
    }

    #[test]
    fn test_sbpl_escape_special_chars() {
        assert_eq!(
            sbpl_escape(r#"/path/with "quotes""#),
            r#"/path/with \"quotes\""#
        );
        assert_eq!(
            sbpl_escape(r#"/path\with\backslash"#),
            r#"/path\\with\\backslash"#
        );
    }

    #[test]
    fn test_sandbox_config_build_default_policy() {
        let config = SandboxConfig {
            level: Some("restricted".into()),
            ..Default::default()
        };
        let policy = config.build_default_policy(Path::new("/workspace"));
        assert_eq!(policy.level, SandboxLevel::Restricted);
        assert!(policy.writable_paths.contains(&PathBuf::from("/workspace")));
        assert!(!policy.hidden_paths.is_empty());
        assert!(!policy.allowed_domains.is_empty());
    }

    #[test]
    fn test_sandbox_violation_display_actionable() {
        let violation = SandboxViolation::PathDenied {
            path: PathBuf::from("/etc/passwd"),
            reason: "Sandboxed environment".into(),
            allowed: vec![PathBuf::from("/workspace")],
        };
        let msg = violation.to_string();
        assert!(msg.contains("/etc/passwd"));
        assert!(msg.contains("Sandbox Violation"));
        assert!(msg.contains("/workspace"));
        assert!(msg.contains("Action needed"));
    }
}
