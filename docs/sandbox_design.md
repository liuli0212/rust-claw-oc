# Sandbox 设计文档：rusty-claw-oc 工具隔离系统

> **状态**: Draft v1  
> **日期**: 2026-04-01  
> **目标**: 为 Agent 的高风险工具引入基于 Bubblewrap (`bwrap`) 的操作系统级沙箱隔离

---

## 1. 动机与目标

当前 `rusty-claw-oc` 的所有工具（`execute_bash`、`write_file`、`patch_file` 等）都以主进程的完整权限在宿主机上直接执行。这意味着：

- Agent 的 `bash` 命令可以读取 `~/.ssh`、`~/.aws` 等敏感凭证
- 一条误生成的 `rm -rf /` 可以摧毁整个系统
- `web_fetch` 抓取的恶意内容可能被写入任意路径
- Subagent 的写操作虽然有 `claimed_paths` 逻辑约束，但没有 OS 级强制执行

**设计目标**：

1. 为高风险工具提供 OS 级隔离，防止意外或恶意操作
2. 架构上预留扩展点，使所有工具都能按需接入沙箱
3. 当 `bwrap` 不可用时优雅降级（警告但不阻塞）
4. 对性能的影响控制在 < 50ms / 次调用

---

## 2. 技术选型：Bubblewrap (`bwrap`)

| 对比维度 | Bubblewrap | Landlock | Docker |
| :--- | :--- | :--- | :--- |
| 外部依赖 | 仅 `bwrap` 二进制 | 无（内核 >= 5.13） | Docker 守护进程 |
| 启动延迟 | < 10ms | < 1ms | 500ms-2s |
| 隔离粒度 | Namespace 级 | 系统调用级 | 容器级 |
| 业界验证 | Claude Code, Flatpak | 较新，生态偏小 | 工业标准 |
| 适配难度 | 低（命令行包装） | 中（需处理动态库路径） | 高（镜像管理） |

**结论**：选择 Bubblewrap 作为主方案。后续可叠加 Landlock 做二级加固。

---

## 3. 架构设计

### 3.1 核心抽象：`SandboxPolicy` 与 `SandboxEnforcer`

```
┌─────────────────────────────────────────────────────┐
│                    AgentLoop / Core                  │
│                                                     │
│  ┌─────────┐  ┌──────────┐  ┌──────────┐           │
│  │BashTool │  │WriteFile │  │WebFetch  │  ...       │
│  └────┬────┘  └────┬─────┘  └────┬─────┘           │
│       │            │             │                  │
│       ▼            ▼             ▼                  │
│  ┌─────────────────────────────────────────────┐    │
│  │           SandboxEnforcer (中间层)            │    │
│  │                                             │    │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  │    │
│  │  │ Bwrap    │  │ PathGuard│  │ NetGuard │  │    │
│  │  │ Backend  │  │ Backend  │  │ Backend  │  │    │
│  │  └──────────┘  └──────────┘  └──────────┘  │    │
│  └─────────────────────────────────────────────┘    │
│                                                     │
└─────────────────────────────────────────────────────┘
```

### 3.2 沙箱级别

定义三个 **SandboxLevel**，由配置或 ToolContext 决定：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxLevel {
    /// 不做任何隔离（现有行为，向后兼容）
    Unrestricted,
    /// 限制文件系统访问，保护敏感目录
    Restricted,
    /// 最严格模式：文件系统 + 网络 + PID 全部隔离
    Strict,
}
```

### 3.3 `SandboxPolicy` — 描述隔离策略

```rust
/// 描述一次工具执行的沙箱策略
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub level: SandboxLevel,
    /// 允许读写的路径列表（绝对路径）
    pub writable_paths: Vec<PathBuf>,
    /// 额外的只读挂载路径
    pub readonly_paths: Vec<PathBuf>,
    /// 是否隔离网络
    pub isolate_network: bool,
    /// 是否隔离 PID namespace
    pub isolate_pid: bool,
    /// 是否清空环境变量 (防泄漏)
    pub clear_env: bool,
    /// 允许保留的环境变量 (仅当 clear_env 为 true 时生效)
    pub keep_env: Vec<String>,
    /// 允许访问的网络域名白名单（仅在 isolate_network=false 时生效）
    pub allowed_domains: Vec<String>,
    /// 要在沙箱中隐藏的敏感路径
    pub hidden_paths: Vec<PathBuf>,
}
```

### 3.4 `SandboxEnforcer` — 执行隔离的引擎

```rust
/// 沙箱执行引擎，负责将 SandboxPolicy 转化为具体的 OS 隔离操作
pub struct SandboxEnforcer {
    /// bwrap 二进制路径，None 表示未检测到
    bwrap_path: Option<PathBuf>,
    /// 全局默认策略
    default_policy: SandboxPolicy,
}

impl SandboxEnforcer {
    /// 启动时探测 bwrap 是否可用
    pub fn detect() -> Self { ... }

    /// 是否支持沙箱隔离
    pub fn is_available(&self) -> bool { ... }

    /// 将一个 Command 包装成沙箱化的 Command
    pub fn wrap_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> std::process::Command { ... }

    /// 检查一个文件路径是否被策略允许
    pub fn check_path_access(
        &self,
        path: &Path,
        write: bool,
        policy: &SandboxPolicy,
    ) -> Result<(), SandboxViolation> { ... }

    /// 检查一个 URL 是否被策略的域名白名单允许
    pub fn check_network_access(
        &self,
        url: &str,
        policy: &SandboxPolicy,
    ) -> Result<(), SandboxViolation> { ... }
}
```

### 3.5 `Tool` trait 扩展

在现有 `Tool` trait 上新增一个默认方法，让每个工具可以声明自己的沙箱需求：

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;

    fn has_side_effects(&self) -> bool { true }

    /// 声明该工具需要的沙箱策略。
    /// 返回 None 表示该工具不需要沙箱（如 task_plan）。
    fn sandbox_policy(&self) -> Option<SandboxPolicy> {
        None  // 默认不要求沙箱，保持向后兼容
    }
}
```

### 3.6 `ToolContext` 扩展

在 `ToolContext` 中注入沙箱引擎的引用，让工具在 `execute` 中可以访问：

```rust
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub reply_to: String,
    pub visible_tools: Vec<String>,
    pub active_skill_name: Option<String>,
    pub skill_call_context: Option<SkillCallContext>,
    pub skill_budget: SkillBudget,
    // ---- 新增 ----
    /// 当前会话的沙箱执行引擎（Arc 共享）
    pub sandbox: Option<Arc<SandboxEnforcer>>,
}
```

---

## 4. 分阶段实现

### Phase 1：BashTool 沙箱化 (优先级 P0)

**目标**：让 `execute_bash` 的所有命令在 `bwrap` 气泡中运行。

**改动文件**：
- 新建 `src/tools/sandbox.rs` — SandboxPolicy, SandboxEnforcer, SandboxLevel
- 修改 `src/tools/bash.rs` — 在命令执行前通过 SandboxEnforcer 包装
- 修改 `src/tools/shell.rs` — 同上，影响非 PTY 路径
- 修改 `src/tools/mod.rs` — 导出 sandbox 模块

**`bwrap` 命令生成逻辑**（核心）：

```rust
impl SandboxEnforcer {
    pub fn wrap_command(
        &self,
        cmd: &str,
        policy: &SandboxPolicy,
        cwd: &Path,
    ) -> std::process::Command {
        let bwrap = self.bwrap_path.as_ref().expect("bwrap not available");
        let mut command = std::process::Command::new(bwrap);

        // ---- 基础文件系统 ----
        // 只读挂载系统关键目录
        for sys_dir in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
            if Path::new(sys_dir).exists() {
                command.arg("--ro-bind").arg(sys_dir).arg(sys_dir);
            }
        }

        // /proc 和 /dev 的最小挂载
        command.arg("--proc").arg("/proc");
        command.arg("--dev").arg("/dev");

        // 临时目录：映射到 session 专属的临时目录，避免读写与宿主机割裂
        let session_tmp = cwd.join(".tmp");
        std::fs::create_dir_all(&session_tmp).ok();
        command.arg("--bind").arg(&session_tmp).arg("/tmp");

        // ---- 环境变量隔离 ----
        if policy.clear_env {
            command.arg("--clearenv");
            for env_key in &policy.keep_env {
                if let Ok(val) = std::env::var(env_key) {
                    command.arg("--setenv").arg(env_key).arg(val);
                }
            }
        }

        // ---- 工作目录 ----
        command.arg("--bind").arg(cwd).arg(cwd);

        // ---- 额外可写路径 ----
        for path in &policy.writable_paths {
            command.arg("--bind").arg(path).arg(path);
        }

        // ---- 额外只读路径 ----
        for path in &policy.readonly_paths {
            command.arg("--ro-bind").arg(path).arg(path);
        }

        // ---- 隐藏敏感路径 ----
        for path in &policy.hidden_paths {
            command.arg("--tmpfs").arg(path);
        }

        // ---- 网络隔离 ----
        if policy.isolate_network {
            command.arg("--unshare-net");
        }

        // ---- PID 隔离 ----
        if policy.isolate_pid {
            command.arg("--unshare-pid");
        }

        // ---- 安全加固 ----
        command.arg("--die-with-parent");     // 父进程退出则杀子进程
        command.arg("--new-session");         // 防止 TTY 劫持

        // ---- 设置 CWD ----
        command.arg("--chdir").arg(cwd);

        // ---- 实际要执行的命令 ----
        command.arg("--").arg("bash").arg("-c").arg(cmd);

        command
    }
}
```

**BashTool 改动示意**：

```rust
// src/tools/bash.rs — execute 方法内
async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
    // ... 参数解析 ...

    // 如果沙箱可用，使用沙箱执行
    if let Some(sandbox) = &ctx.sandbox {
        if sandbox.is_available() {
            let policy = self.sandbox_policy()
                .unwrap_or_else(|| sandbox.default_policy());
            // 使用 bwrap 包装的 Command 替代原始 Command
            let wrapped_cmd = sandbox.wrap_command(&cmd_str, &policy, &self.work_dir);
            return self.execute_with_pty_wrapped(wrapped_cmd, timeout_secs).await;
        }
    }

    // fallback: 原始执行路径（保持向后兼容）
    self.execute_raw(&cmd_str, timeout_secs).await
}
```

**BashTool 的默认 SandboxPolicy**：

```rust
impl Tool for BashTool {
    fn sandbox_policy(&self) -> Option<SandboxPolicy> {
        Some(SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![PathBuf::from(&self.work_dir)],
            readonly_paths: vec![],
            isolate_network: false,  // bash 经常需要 git clone, curl 等
            isolate_pid: true,
            clear_env: true,
            keep_env: vec!["PATH".into(), "TERM".into(), "HOME".into(), "USER".into(), "LANG".into()],
            allowed_domains: vec![],
            hidden_paths: vec![
                dirs::home_dir().unwrap_or_default().join(".ssh"),
                dirs::home_dir().unwrap_or_default().join(".aws"),
                dirs::home_dir().unwrap_or_default().join(".gnupg"),
                dirs::home_dir().unwrap_or_default().join(".env"),
                // API keys 配置
                dirs::config_dir().unwrap_or_default().join("rusty-claw"),
            ],
        })
    }
}
```

### Phase 2：WriteFileTool / PatchFileTool 路径守卫 (P1)

**目标**：在文件写入前通过 `SandboxEnforcer::check_path_access` 进行路径检查。

> [!NOTE]
> 这两个工具不需要 bwrap 包装（它们不 spawn 子进程），而是使用 `check_path_access` 做纯逻辑层面的路径守卫。

```rust
impl Tool for WriteFileTool {
    fn sandbox_policy(&self) -> Option<SandboxPolicy> {
        Some(SandboxPolicy {
            level: SandboxLevel::Restricted,
            writable_paths: vec![], // 由会话动态注入
            isolate_network: false,
            isolate_pid: false,
            hidden_paths: vec![],
            ..Default::default()
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let parsed: WriteFileArgs = ...;

        // 路径守卫
        if let Some(sandbox) = &ctx.sandbox {
            let policy = self.sandbox_policy().unwrap();
            sandbox.check_path_access(
                Path::new(&parsed.path),
                true, // write
                &policy,
            ).map_err(|v| ToolError::ExecutionFailed(
                format!("Sandbox violation: {}", v)
            ))?;
        }

        // ... 正常写入逻辑 ...
    }
}
```

**守卫规则**：

| 路径模式 | 行为 |
| :--- | :--- |
| `<work_dir>/**` | ✅ 允许 |
| `/etc/**`, `/usr/**` | ❌ 拒绝 |
| `~/.ssh/**`, `~/.aws/**` | ❌ 拒绝 |
| `/tmp/**` | ✅ 允许（临时文件） |
| `sessions/**` | ✅ 允许（Agent 自身的数据） |

### Phase 3：WebFetchTool 网络域名白名单 (P2)

**目标**：限制 `web_fetch` 和 `web_search` 只能访问预定义的域名白名单。

> [!IMPORTANT]
> 这一阶段的"隔离"不依赖 bwrap，而是在应用层做 URL 检查。这比 bwrap 的 `--unshare-net` 更灵活（后者是全开/全关）。

```rust
impl Tool for WebFetchTool {
    fn sandbox_policy(&self) -> Option<SandboxPolicy> {
        Some(SandboxPolicy {
            level: SandboxLevel::Restricted,
            isolate_network: false,
            allowed_domains: vec![
                "github.com".into(),
                "raw.githubusercontent.com".into(),
                "docs.rs".into(),
                "crates.io".into(),
                "api.tavily.com".into(),
                // 可通过 config.toml 扩展
            ],
            ..Default::default()
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let parsed: WebFetchArgs = ...;

        // 域名白名单检查
        if let Some(sandbox) = &ctx.sandbox {
            let policy = self.sandbox_policy().unwrap();
            sandbox.check_network_access(&parsed.url, &policy)
                .map_err(|v| ToolError::ExecutionFailed(
                    format!("Sandbox violation: {}", v)
                ))?;
        }

        // ... 正常请求逻辑 ...
    }
}
```

### Phase 4：Subagent 沙箱继承 (P2)

**目标**：Subagent 继承父 Agent 的沙箱策略，且可以进一步收紧但不能放松。

**改动文件**：`src/session/factory.rs`

```rust
// 在 build_subagent_session 中
fn build_subagent_session(...) -> Result<BuiltSubagentSession, String> {
    // ... 现有逻辑 ...

    // 继承并收紧沙箱策略
    let sub_sandbox = parent_sandbox.clone().tighten(SandboxPolicy {
        writable_paths: claimed_paths.iter().map(PathBuf::from).collect(),
        isolate_network: mode == SubagentBuildMode::AsyncReadonly,
        ..Default::default()
    });

    // 注入到 subagent 的 ToolContext 中
    // ...
}
```

**策略继承规则**（单调收紧原则）：

```
父级 Unrestricted + 子级 Restricted → 子级生效 Restricted
父级 Restricted  + 子级 Strict →     子级生效 Strict
父级 Strict      + 子级 Unrestricted → 子级生效 Strict（不能放松）
```

---

## 5. 可观测性设计 (Observability)

在引入拦截与隔离机制后，必须确保系统行为对开发者和管理员完全透明：

1. **安全审计日志 (Audit Logging)**
   在 `SandboxEnforcer::check_path_access` 和 `wrap_command` 被触发、尤其是策略拒绝（Deny）时，必须通过 `tracing::warn!` 记录结构化审计日志，包含 `session_id`, `tool_name`, 尝试跨越的边界，以及应用的相关策略，便于后续追溯。
   
2. **底层进程故障隔离 (Process Fault Isolation)**
   如果 `bwrap` 自身因参数或内核权限启动失败（如返回非 0 状态码，且无标准输出），需要捕获其 `stderr`（如隔离空间创建失败信息），并使用 `tracing::error!` 单独上报，避免与 `bash` 内脚本原本的执行失败常规日志（Exit Code）混淆。

3. **性能指标反馈 (Metrics)**
   在包装命令和路径校验链路上添加带有计时的监控 Span（如 `tracing::instrument`），统计沙箱准备阶段的耗时分布，确保在高并发文件操作或命令执行下严格控制在设计的 `< 50ms` 预算内。

---

## 6. 与 LLM 的友好交互 (LLM-Friendly)

沙箱的直接交互对象是 LLM，不能仅仅进行底层“拦截”，更要将约束上抛，使 LLM 能理解并主动规避：

### 6.1 前置宣告 (System Prompt Injection)
在 `src/context.rs` 初始化 Agent 或 Subagent 时，将 `SandboxPolicy` 的关键约束注入其 System Prompt 中，使得 LLM 提前知道物理边界：
```markdown
<environment_constraints>
- Network: Offline (Internet access is disabled)
- Permitted WorkDirs: ["/home/liuli/workspace"]
</environment_constraints>
```

### 6.2 动态工具描述 (Dynamic Tool Description)
系统的沙箱级别应能动态影响 `Tool::description()` 的输出。例如当 `isolate_network = true` 激活时，告诉 LLM：“你在隔离的网络下运行，任何依赖网络的操作（如 curl, pip install 联网获取资源等）将会失败”。

### 6.3 可操作的错误建议 (Actionable Error Messages)
拦截发生时，抛出的 `ToolError::ExecutionFailed` 必须具备指导性。不要仅仅返回 `"Sandbox violation"`，而应返回：
```text
Sandbox Violation: Access to `/etc/passwd` was denied.
Reason: This environment is strictly sandboxed. 
Allowed writable directories are: ["/path/to/cwd"]
Action needed: Please modify your action to operate within the allowed directories.
```

---

## 7. 配置系统

在 `config.toml` 中新增 `[sandbox]` section：

```toml
[sandbox]
# 全局开关：off / restricted / strict
level = "restricted"

# 是否在 bwrap 不可用时阻止执行（false = 降级为 unrestricted）
require_os_sandbox = false

# 额外的可写目录
writable_paths = ["/data/shared"]

# 敏感目录（隐藏）
hidden_paths = [
    "~/.ssh",
    "~/.aws",
    "~/.gnupg",
    "~/.config/rusty-claw",
]

# 网络白名单（用于 web_fetch/web_search 的应用层检查）
allowed_domains = [
    "github.com",
    "raw.githubusercontent.com",
    "docs.rs",
    "crates.io",
    "api.tavily.com",
]

# Bash 工具的专项配置
[sandbox.bash]
isolate_network = false   # bash 保留网络（git clone 等需要）
isolate_pid = true

# Subagent 的专项配置
[sandbox.subagent]
isolate_network = true    # 后台 subagent 默认断网
force_strict = false      # 是否强制所有 subagent 用 strict 模式
```

**对应的 Rust 结构体**：

```rust
#[derive(Debug, Deserialize, Default, Clone)]
pub struct SandboxConfig {
    pub level: Option<String>,          // "off" | "restricted" | "strict"
    pub require_os_sandbox: Option<bool>,
    pub writable_paths: Option<Vec<String>>,
    pub hidden_paths: Option<Vec<String>>,
    pub allowed_domains: Option<Vec<String>>,
    pub bash: Option<BashSandboxConfig>,
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
```

---

## 8. 文件结构变更

```
src/
├── tools/
│   ├── sandbox.rs          # [新建] SandboxPolicy, SandboxEnforcer, SandboxLevel, SandboxViolation
│   ├── bash.rs             # [修改] 接入 SandboxEnforcer::wrap_command
│   ├── shell.rs            # [修改] 接入 SandboxEnforcer::wrap_command
│   ├── files.rs            # [修改] WriteFileTool, PatchFileTool 增加 check_path_access
│   ├── web.rs              # [修改] WebFetchTool 增加 check_network_access
│   ├── protocol.rs         # [修改] ToolContext 增加 sandbox 字段; Tool trait 增加 sandbox_policy
│   ├── mod.rs              # [修改] 导出 sandbox
│   └── ...
├── config.rs               # [修改] AppConfig 增加 SandboxConfig
├── session/
│   └── factory.rs          # [修改] 沙箱注入和 subagent 继承
└── ...
```

---

## 9. 降级策略

```
┌──────────────────────────────────────────┐
│           程序启动                         │
│                                          │
│  SandboxEnforcer::detect()               │
│          │                               │
│          ▼                               │
│  bwrap 存在？──── Yes ──→ 正常沙箱模式     │
│          │                               │
│         No                               │
│          │                               │
│          ▼                               │
│  config.require_os_sandbox?              │
│          │                               │
│    Yes ──┤──→ 启动失败，打印安装指南        │
│          │                               │
│    No  ──┤──→ 降级模式 + 警告日志          │
│          │   "⚠ bwrap not found,         │
│          │    sandbox disabled.           │
│          │    Install: apt install         │
│          │    bubblewrap"                 │
│          ▼                               │
│  照常运行（Unrestricted 模式）             │
└──────────────────────────────────────────┘
```

---

## 10. 测试策略

### 10.1 单元测试 (`src/tools/sandbox.rs`)

```rust
#[cfg(test)]
mod tests {
    // 测试 bwrap 命令行生成是否正确
    #[test]
    fn test_wrap_command_generates_correct_args() { ... }

    // 测试路径守卫：允许的路径
    #[test]
    fn test_check_path_access_allows_workdir() { ... }

    // 测试路径守卫：拒绝的路径
    #[test]
    fn test_check_path_access_blocks_ssh_dir() { ... }

    // 测试域名白名单
    #[test]
    fn test_check_network_access_allows_whitelisted() { ... }

    // 测试域名白名单：拒绝
    #[test]
    fn test_check_network_access_blocks_unknown() { ... }

    // 测试策略继承的单调收紧
    #[test]
    fn test_policy_tighten_cannot_relax() { ... }
}
```

### 10.2 集成测试

```rust
#[tokio::test]
#[ignore] // 需要 bwrap 已安装
async fn test_bash_tool_cannot_read_ssh_keys_in_sandbox() {
    let tool = BashTool::new();
    let ctx = /* 构建带 sandbox 的 ToolContext */;
    let result = tool.execute(
        json!({"command": "cat ~/.ssh/id_rsa"}),
        &ctx,
    ).await.unwrap();
    // 验证无法读取
    assert!(result.contains("No such file"));
}

#[tokio::test]
#[ignore]
async fn test_bash_tool_can_write_to_workdir_in_sandbox() {
    let tool = BashTool::new();
    let ctx = /* 构建带 sandbox 的 ToolContext */;
    let result = tool.execute(
        json!({"command": "echo 'hello' > /tmp/test.txt && cat /tmp/test.txt"}),
        &ctx,
    ).await.unwrap();
    assert!(result.contains("hello"));
}
```

---

## 11. 安全注意事项

> [!WARNING]
> **已知限制**

1. **User Namespace 依赖**：未以 root 运行时，`bwrap` 依赖内核的 User Namespace 支持。某些 hardened 内核（如部分 Debian 配置）默认关闭了 `kernel.unprivileged_userns_clone`，需要手动开启。
2. **符号链接逃逸**：如果可写目录中存在指向沙箱外部的符号链接，进程可能通过符号链接写入沙箱外的文件。`bwrap` 不会自动解析符号链接目标。
3. **Setuid 二进制**：沙箱内如果存在 setuid 程序（如 `/usr/bin/sudo`），理论上存在提权风险。建议在 Strict 模式下使用 `--unshare-user` 来防止 setuid 生效。
4. **资源耗尽 (Resource Exhaustion)**：`bwrap` 主要负责隔离视界，不提供 Cgroups 内存/CPU 资源限制。对于死循环或 Fork 炸弹（如 `:(){ :|:& };:`），应结合 `timeout` 命令或系统级 `prlimit` 做底层防护。
5. **Bash 网络突破边界**：即使 `web_fetch` 应用了域名白名单，若 Bash 沙箱的 `isolate_network = false`，LLM 依然可直接通过 Bash 环境内的 `curl` 随意访问外部网络（Data Exfiltration风险）。由于基于 bwrap 的网络隔离非黑即白，无法做到仅允许访问某些主机的细粒度保护。
6. **性能影响**：每次 bash 调用增加约 5-10ms 的 `bwrap` 启动开销，对于高频调用场景（如连续执行多个 `ls`）可能有可感知的延迟累积。

> [!TIP]
> **最佳实践**

- 在服务器部署时，建议将 `level` 设为 `"strict"` 并开启 `require_os_sandbox = true`
- 在本地开发时，建议使用 `"restricted"` 以平衡安全与便利
- 对于 Autopilot 模式（无人值守），强烈建议使用 `"strict"` 模式

---

## 12. 工具沙箱需求速查表

| 工具名 | Phase | 沙箱类型 | 默认级别 | 隔离手段 |
| :--- | :--- | :--- | :--- | :--- |
| `execute_bash` | P0 | 进程级 | Restricted | bwrap 命令包装 |
| `write_file` | P1 | 路径级 | Restricted | check_path_access |
| `patch_file` | P1 | 路径级 | Restricted | check_path_access |
| `send_file` | P1 | 路径级 | Restricted | check_path_access |
| `web_fetch` | P2 | 网络级 | Restricted | check_network_access |
| `web_search` | P2 | 网络级 | Restricted | check_network_access（API 级） |
| `read_file` | P2 | 路径级 | Unrestricted | check_path_access（仅保护敏感路径） |
| `write_memory` | P2 | 路径级 | Unrestricted | check_path_access（限定 sessions/ 下） |
| `rag_insert` | P2 | 路径级 | Unrestricted | check_path_access |
| `task_plan` | - | 无 | - | 纯内存操作，无需沙箱 |
| final visible text completion | - | 无 | - | 纯内存状态更新，无需沙箱 |
| `ask_user` | - | 无 | - | 仅 UI 交互，无需沙箱 |
| `dispatch_subagent` | P2 | 继承 | 从父级继承 | 策略透传 + 收紧 |
| `spawn_subagent` | P2 | 继承 | 从父级继承 | 策略透传 + 收紧 |

---

## 13. 里程碑与排期建议

| 里程碑 | 内容 | 预估工作量 |
| :--- | :--- | :--- |
| **M1** | `sandbox.rs` 模块 + BashTool 集成 + config.toml 支持 + 降级逻辑 | 2-3 天 |
| **M2** | WriteFile / PatchFile / SendFile 路径守卫 | 1 天 |
| **M3** | WebFetch / WebSearch 域名白名单 | 0.5 天 |
| **M4** | Subagent 沙箱继承 + factory.rs 改动 | 1 天 |
| **M5** | 集成测试 + 文档 + AGENTS.md 更新 | 0.5 天 |
