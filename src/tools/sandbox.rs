//! Sandbox enforcement for tool execution.
//!
//! Provides OS-level isolation via Bubblewrap (`bwrap`) for high-risk tools
//! like `execute_bash`, and application-level path/network guards for file
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
    /// Whether to isolate the PID namespace.
    pub isolate_pid: bool,
    /// Clear all environment variables before entering the sandbox.
    pub clear_env: bool,
    /// Env vars to preserve when `clear_env` is true.
    pub keep_env: Vec<String>,
    /// Domain allowlist for application-level network guards.
    pub allowed_domains: Vec<String>,
    /// Paths to hide by overlaying with tmpfs.
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

        if self.isolate_network {
            lines.push("- Network: Offline (Internet access is disabled)".to_string());
        } else if !self.allowed_domains.is_empty() {
            lines.push(format!(
                "- Network: Restricted to domains [{}]",
                self.allowed_domains.join(", ")
            ));
        }

        if self.isolate_pid {
            lines.push("- PID Namespace: Isolated".to_string());
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

// ── SandboxEnforcer ───────────────────────────────────────────────────

/// The sandbox execution engine.  Detects `bwrap` at startup, wraps
/// commands, and enforces path/network guards.
#[derive(Debug, Clone)]
pub struct SandboxEnforcer {
    bwrap_path: Option<PathBuf>,
    default_policy: SandboxPolicy,
}

impl SandboxEnforcer {
    /// Probe the system for `bwrap` and build the enforcer with the given
    /// default policy.
    pub fn detect(default_policy: SandboxPolicy) -> Self {
        let bwrap_path = Self::find_bwrap();
        if let Some(ref path) = bwrap_path {
            tracing::info!("Sandbox: bwrap detected at {}", path.display());
        } else {
            tracing::warn!(
                "Sandbox: bwrap not found. OS-level sandbox is disabled. \
                 Install bubblewrap to enable it."
            );
        }
        Self {
            bwrap_path,
            default_policy,
        }
    }

    /// Create an enforcer without probing (for tests or when sandbox is
    /// explicitly disabled).
    pub fn disabled() -> Self {
        Self {
            bwrap_path: None,
            default_policy: SandboxPolicy::default(),
        }
    }

    /// Whether bwrap-based OS isolation is available.
    pub fn is_available(&self) -> bool {
        self.bwrap_path.is_some()
    }

    /// The global default policy (from config).
    pub fn default_policy(&self) -> &SandboxPolicy {
        &self.default_policy
    }

    // ── Command wrapping ──────────────────────────────────────────

    /// Build a `bwrap`-wrapped command.  Panics if `bwrap` is not available;
    /// callers must check `is_available()` first.
    pub fn build_bwrap_args(&self, cmd: &str, policy: &SandboxPolicy, cwd: &Path) -> Vec<String> {
        let bwrap = self
            .bwrap_path
            .as_ref()
            .expect("build_bwrap_args called but bwrap is not available");

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

    /// Build a `CommandBuilder` for portable-pty spawning inside bwrap.
    pub fn build_pty_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> portable_pty::CommandBuilder {
        let bwrap_args = self.build_bwrap_args(cmd, policy, cwd);
        // bwrap_args[0] is the bwrap binary itself
        let mut builder = portable_pty::CommandBuilder::new(&bwrap_args[0]);
        for arg in &bwrap_args[1..] {
            builder.arg(arg);
        }
        // Don't set cwd on the outer process; bwrap handles it via --chdir
        builder
    }

    /// Build a `std::process::Command` for non-PTY execution inside bwrap.
    pub fn build_std_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> std::process::Command {
        let bwrap_args = self.build_bwrap_args(cmd, policy, cwd);
        let mut command = std::process::Command::new(&bwrap_args[0]);
        for arg in &bwrap_args[1..] {
            command.arg(arg);
        }
        command
    }

    /// Build a `tokio::process::Command` for async non-PTY execution.
    pub fn build_tokio_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> tokio::process::Command {
        let bwrap_args = self.build_bwrap_args(cmd, policy, cwd);
        let mut command = tokio::process::Command::new(&bwrap_args[0]);
        for arg in &bwrap_args[1..] {
            command.arg(arg);
        }
        command
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

        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        // Always block hidden (sensitive) paths
        for hidden in &policy.hidden_paths {
            let hidden_canonical = std::fs::canonicalize(hidden).unwrap_or_else(|_| hidden.clone());
            if canonical.starts_with(&hidden_canonical) {
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

        // Also allow /tmp and session-scoped directories
        let in_tmp = canonical.starts_with("/tmp");

        if allowed || in_tmp {
            Ok(())
        } else {
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

    // ── Internal ──────────────────────────────────────────────────

    fn find_bwrap() -> Option<PathBuf> {
        // Try `which bwrap`
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
        // Well-known fallback paths
        for candidate in &["/usr/bin/bwrap", "/usr/local/bin/bwrap"] {
            if Path::new(candidate).exists() {
                return Some(PathBuf::from(candidate));
            }
        }
        None
    }
}

/// Extract the domain from a URL string.
fn extract_domain(url: &str) -> String {
    // Strip any scheme (http://, https://, ftp://, etc.)
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
    /// Block startup if bwrap is missing.
    pub require_bwrap: Option<bool>,
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
        // Create a temp dir to simulate the hidden path
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
            hidden_paths: vec![PathBuf::from("/home/user/.ssh")],
            ..Default::default()
        };
        let summary = policy.to_prompt_summary();
        assert!(summary.contains("Restricted"));
        assert!(summary.contains("/workspace"));
        assert!(summary.contains("Offline"));
        assert!(summary.contains("sensitive directories"));
    }

    #[test]
    fn test_build_bwrap_args_basic_shape() {
        // Even without bwrap installed, we can test with a fake path
        let enforcer = SandboxEnforcer {
            bwrap_path: Some(PathBuf::from("/usr/bin/bwrap")),
            default_policy: SandboxPolicy::default(),
        };
        let policy = SandboxPolicy {
            level: SandboxLevel::Restricted,
            isolate_pid: true,
            clear_env: true,
            keep_env: vec!["PATH".into()],
            hidden_paths: vec![], // skip actual paths for test
            ..Default::default()
        };
        let args = enforcer.build_bwrap_args("echo hello", &policy, Path::new("/tmp/test_cwd"));
        assert_eq!(args[0], "/usr/bin/bwrap");
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        // Last args should be the actual command
        let last_three: Vec<&str> = args[args.len() - 3..].iter().map(|s| s.as_str()).collect();
        assert_eq!(last_three, vec!["bash", "-c", "echo hello"]);
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
