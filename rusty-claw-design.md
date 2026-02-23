# Rusty-Claw: 工业级 Rust Agent 架构设计草案

## 1. 核心设计理念
**“小核心，强插件，零信任”**
* **高性能与低占用**：基于 Rust 构建，无 GC 停顿，极低内存占用，适合边缘设备与全天候运行。
* **分发简单**：编译为单一二进制文件，无复杂依赖环境。
* **安全沙箱**：核心只负责调度和路由，所有能力（尤其是文件系统和系统命令）通过严格限制的工具层或 Wasm 沙箱执行。

## 2. 核心模块架构 (Module Architecture)

### A. `core` (大脑与心脏)
* **`AgentLoop`**: 核心状态机（Think -> Act -> Observe）。
* **`ContextManager`**: 动态上下文组装与 Token 预算控制。
* **`EventBus`**: 基于 `tokio::sync::broadcast` 的异步事件总线，解耦消息接收与任务执行。

### B. `llm_client` (大模型接口层)
* **`LLMBackend` Trait**: 统一的 `chat()` 和 `stream()` 接口。
* **Proxy Support**: 基于 `reqwest` 原生支持 HTTP 代理（解决国内直连问题）。
* **Adapters**: 重点适配 Gemini (超长上下文/高性价比)，预留 OpenAI 接口格式。

### C. `tools` (工具箱与执行器)
* **`Tool` Trait**: 定义工具的 Schema 和执行逻辑。
* **Bash & PTY**: 基于 `std::process::Command` 和 `portable-pty`，支持交互式与非交互式 Shell 命令。
* **Skills System**: 动态扫描并加载 Markdown 格式的技能声明（类似 `SKILL.md`），将文本指令实时转化为可执行工具。

### D. `memory` (记忆与状态系统)
* **RAG Vector Store**: 采用 `lance` (Rust 原生列式数据库) 或 `sqlite-vec`，实现记忆的语义检索。
* **File-based Memory**: 维护 `MEMORY.md` 等纯文本长效记忆文件，方便人类审查与修改。

### E. `gateway/cli` (交互接口)
* **CLI REPL**: 基于 `rustyline` 提供带历史记录和高亮的终端交互。
* **UI Rendering**: 使用 `termimad` 渲染 Markdown 输出，结合 `indicatif` 提供执行状态反馈。

---

## 3. 上下文管理：最佳实践 (Context Management)

摒弃暴力的“列表截断”，采用**分层上下文模型 (Layered Context Model)** 结合动态渲染引擎。

### 五大上下文区域 (The 5 Context Zones)
按照对大模型注意力（Attention）的权重，从高到低、由远及近排布：

1. **Zone 1: System & Rules (系统与规则层 - 顶部/固定)**
   * 人设、核心指令、安全边界。利用大模型 KV Cache 持久化。
2. **Zone 2: Environmental Context (环境上下文 - 惰性求值)**
   * 绝对时间、操作系统、当前目录等实时状态注入。
3. **Zone 3: Declarative & Semantic Memory (记忆注入层 - 动态召回)**
   * 从向量库检索出的、与当前任务高度相关的历史经验与教训。
4. **Zone 4: Working Memory (工作记忆/历史对话 - 预算截断)**
   * 最近的对话与工具执行结果。**已实现** Token Budgeting System（基于 `tiktoken-rs`），每次请求前倒序计算 Token 消费，超过 32k 安全线时进行精确的末尾历史淘汰。
5. **Zone 5: Immediate Action (即时行动层 - 底部/强引导)**
   * 最新的用户指令，可静默追加 `<thinking>` 等思维链引导标签。

### 数据结构抽象与渲染解耦
将“客观发生的事实”与“发给 LLM 的 Payload”完全解耦：

```rust
pub struct AgentContext {
    pub system_prompts: Vec<PromptBlock>, 
    pub env_state: EnvState,
    pub long_term_memory: Vec<MemorySnippet>,
    pub dialogue_history: Vec<Turn>, // 完整的客观历史记录
}

pub struct Turn {
    pub turn_id: String,
    pub user_message: Message,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>, // 针对超长 Bash 输出(如 > 2000行)，进行 Head/Tail 截断处理
    pub agent_final_response: Message,
}

impl AgentContext {
    // 渲染引擎：根据当前的 Token 预算，动态编译出最适合大模型视角的 Payload
    pub fn build_llm_payload(&self, tokenizer: &Tokenizer, budget: &Budget) -> Vec<LlmMessage> { ... }
}
```

---

## 4. 推荐技术栈 (The "Rust" Stack)
* 运行时: `tokio`
* 网络请求: `reqwest`
* 序列化: `serde`, `serde_json`
* 终端交互: `clap`, `rustyline`, `termimad`
* 虚拟终端: `portable-pty`
* 向量检索: `lancedb` / `sqlite-vec`
* Token计算: `tiktoken-rs`
## 5. Bash 与工具沙箱 (Tooling / Bash Sandbox)

工具系统（Tools）是 Agent 与现实世界交互的“手”。在 Rust 中，我们要兼顾**极致的灵活性（执行任意 Bash）**和**严苛的安全性（防暴走、防死锁）**。

### A. 工具调用的抽象协议

所有工具必须实现统一的 `Tool` Trait。这个设计使得大模型能够动态理解工具的能力，并在运行时决定如何调用。

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    // LLM 可见的名称 (例如 "execute_bash", "read_file")
    fn name(&self) -> &'static str;
    
    // LLM 可见的描述，引导模型何时使用该工具
    fn description(&self) -> &'static str;
    
    // 工具参数的 JSON Schema 定义
    fn parameters_schema(&self) -> serde_json::Value;
    
    // 异步执行入口，返回成功结果的字符串或错误信息
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError>;
}
```

### B. 交互式 Bash 执行器 (The Bash Executor)

传统的 `std::process::Command` 只能非交互式地运行命令，对于需要用户输入（如 `git commit` 需要打开编辑器，或 `sudo` 需要密码）的场景无能为力。

**架构设计：基于 PTY 的流式执行器**
*   **依赖 (已实现)**: 引入 **`portable-pty`**，为每个 Bash 进程分配一个伪终端（Pseudo-Terminal）。这使得 `isatty()` 检测为真，兼容所有交互式工具，并附带了正则 ANSI 洗白机制（剥离颜色代码保护 Token）。
*   **非阻塞读取**: Agent 发出命令后，并不“死等”命令结束。而是启动一个 Tokio 异步任务，持续监听 PTY 的 `stdout`。
*   **智能截断与超时防死锁 (已实现)**: 这是防止上下文爆炸的关键。如果一个构建脚本吐出了 10 万行日志：
    1.  **内存截断**: 工具执行器仅保留日志的“头部 500 行”和“尾部 500 行”，中间部分用 `[... Truncated 99,000 lines ...]` 替换。
    2.  **超时控制**: 必须有严格的 Timeout 机制（例如 `tokio::time::timeout(Duration::from_secs(60), ...)`），一旦超时立刻发送 `SIGTERM` 甚至 `SIGKILL` 终止僵尸进程。

### C. 技能系统 (Skills System: Markdown as Code)

参考 OpenClaw，我们希望用户**不需要写 Rust 代码就能教 Agent 新技能** (已通过 `skills.rs` 结合 `serde_yaml` 和动态 Schema 生成实现)。

*   **技能定义**: 在 `skills/` 目录下存放 Markdown 文件（如 `skills/docker-cleanup/SKILL.md`）。
*   **动态解析**: 
    1.  启动时，Agent 扫描该目录。
    2.  读取 Markdown 的 Frontmatter（YAML 头），提取技能名称、描述和参数。
    3.  正文部分包含预设的提示词（Prompt）和 Bash 脚本片段。
*   **运行时映射**: 当 LLM 决定调用某个技能时，Agent 将参数注入脚本模板并交给 Bash Executor 执行。

---

## 6. 状态持久化与向量检索 (Memory & RAG)

“失忆”是长线任务的致命伤。一个生产级的 Agent 必须拥有自己的“海马体”和“大脑皮层”。

### A. 记忆的三层架构

1. **瞬时记忆 (Working Memory)**
   * **存储**: 纯内存 (`Vec<Turn>`)。
   * **生命周期**: 当前 Session。随进程重启而丢失。
   * **机制**: Token 预算控制（前文已述）。

2. **工作空间记忆 (Workspace Memory)**
   * **存储**: 纯文本文件（如工作区根目录的 `MEMORY.md` 或 `AGENTS.md`）。
   * **生命周期**: 永久（除非手动删除或被 Agent 编辑）。
   * **机制**: 提供一组专门的 `memory_read` / `memory_write` 工具，允许 Agent 像写日记一样，把关键决策、代码路径、项目上下文写入文件。这部分内容会在每次请求的 Zone 3（记忆注入层）被完整加载。

3. **语义记忆 (Semantic RAG Storage)**
   * **存储**: 本地向量数据库。
   * **生命周期**: 永久。
   * **机制**: 将海量的历史对话、技术文档切块（Chunking）后转化为向量。当用户提出宽泛问题时，进行语义召回。

### B. 向量检索的技术选型与实现

在实施中为了坚持“纯 Rust、无繁重 C++ 依赖、单一可执行文件”的设计理念，我们放弃了笨重的 LanceDB，采用了极致轻量的纯 Rust 模型引擎。

*   **当前实施方案：`fastembed` + 本地内存读写锁 (已实现)**
    *   *优势*: 可以在本地全离线、零外部服务调用的情况下，下载并运行极小巧的 `AllMiniLML6V2` 嵌入模型。生成的 768 维向量直接利用 `RwLock` 托管在内存，进行实时的 Cosine Similarity 计算，实现了毫秒级语义检索召回。
    *   *架构*: 
        1. `src/rag.rs` 维护 `TextEmbedding` 实例。
        2. `src/tools.rs` 向大模型暴露 `memorize_knowledge`（写入）和 `search_knowledge_base`（向量搜索）工具。

### C. RAG 工作流 (The Recall Loop)

1.  **触发**: Agent 在回答前（或者通过一个隐式的 `search_memory` 工具），决定是否需要历史背景。
2.  **嵌入 (Embedding)**: 将用户的 Query 转化为高维向量（如 768 维）。
3.  **近似最近邻搜索 (ANN)**: 计算余弦相似度（Cosine Similarity），取最高的 Top-K 个片段（Chunks）。
4.  **注入**: 将找回的片段（带上时间和来源标记）作为 Context Zone 3，一起交给 Gemini 进行最终的推理（Generation）。

## 7. 结构化输出与容错自修复 (Structured Output & Self-Correction)

在 Rust 的强类型世界里，处理大模型不稳定输出（JSON 格式错误、多余的文本解释）是核心挑战。

### A. 基于 Rust 类型的 Schema 生成
为了避免手动维护容易出错的 JSON Schema，我们使用 **`schemars`** 库。所有的工具参数直接定义为 Rust `struct`。

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCmdArgs {
    /// The shell command to execute
    pub command: String,
    /// Timeout in seconds (default: 30)
    pub timeout: Option<u64>,
}

// 启动时，Agent 自动将上述 Struct 转换为 LLM 期望的 JSON Schema，并注册为 Tool。
```

### B. 健壮的 JSON 提取与容错循环
LLM 有时会无视 Schema 或者输出 `Here is the JSON: { ... }` 这样的包裹文本。

*   **提取器 (JSON Extractor)**：实现一个简单的状态机或使用正则（如 `(?s)\{.*?\}`）从 LLM 响应中精准定位 JSON 块。
*   **自修复循环 (Self-Correction Loop)**：
    *   当 `serde_json::from_str::<ExecuteCmdArgs>(&json_str)` 失败时，**绝不能直接崩溃**。
    *   捕获 `serde` 极其详细的报错信息（如：`missing field 'command'` 或 `expected u64 at line 2`）。
    *   构造一条系统消息（如：`"Your tool call failed to parse: <error>. Please output valid JSON matching the schema."`），重新发回给 LLM 强制重试。设定最大重试次数（如 3 次），超过则终止本次行动。

---

## 8. 流式响应与中断机制 (Streaming & Interruption)

为了实现“打字机”般丝滑的 CLI 体验，必须彻底拥抱 Tokio 的异步流机制，并处理复杂的截断与中断场景。

### A. 基于 Channel 的流式解析架构 (Stream Parser)

LLM 返回的不仅是给用户的闲聊（Text），还可能夹杂着工具调用（Tool Calls）。

1.  **生产者 (`tx`)**：LLM HTTP 客户端收到 Server-Sent Events (SSE) 数据块后，原封不动地推入 `tokio::sync::mpsc::channel`。
2.  **消费者 (`rx`)**：一个独立的 Stream Parser 任务负责消费这些 Chunk。
3.  **状态机分流**：
    *   **状态：文本输出** -> 如果当前未处于工具调用块，将收到的 Token 实时推给终端渲染器（如 `termimad`）。
    *   **状态：工具拦截** -> 如果检测到类似于 `{"function_call":` 的开始标记，**立刻停止向终端输出**，转为在内存中拼接完整的 JSON 字符串。拼接完成后，触发上述的 JSON 解析逻辑。

### B. 优雅的中断与取消 (Graceful Cancellation)

在 CLI 环境中，用户按下 `Ctrl+C` 是一种常态。但在 Agent 运行中，这不仅仅是退出程序那么简单。

*   **信号监听**：通过 `tokio::signal::ctrl_c()` 启动全局监听任务。
*   **Token 传递 (CancellationToken)**：使用 `tokio-util` 库中的 `CancellationToken`。
    *   所有的网络请求（等待 LLM 响应）和所有的工具执行（如正在跑着一个死循环的 Bash 脚本），都必须绑定这个 Token。
*   **行为链**：当用户觉得 Agent 在胡言乱语按下 `Ctrl+C`：
    1.  触发 Token Cancellation。
    2.  LLM 的 HTTP 请求被立刻掐断，停止计费。
    3.  正在执行的 Bash 进程收到 `SIGTERM` 强制终止（防止后台留有僵尸进程，如死循环的 `while true`）。
    4.  当前状态回滚，并向模型追加一条 `"Action cancelled by user."` 的系统提示，等待下一次指令。
