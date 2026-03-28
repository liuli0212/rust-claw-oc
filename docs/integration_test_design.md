# Rusty-Claw 自动化集成测试方案设计

## 1. 背景与目标 (Background & Goals)
随着 `rusty-claw-oc` 项目的复杂度增加，手动测试 CLI 和 Telegram 接口以及工具调用的成本越来越高。为了保证每次代码改动后系统的核心链路（CLI 交互、Telegram 消息处理、工具调用）不被破坏，需要引入一套自动化的集成测试方案。

**核心目标：**
- 确保 CLI 和 Telegram 接口在每次改动后能正常运行。
- 确保 Agent 能够正确解析并调用工具（如 `write_file`, `execute_bash`）。
- 消除对真实 LLM API 和 Telegram API 的依赖，实现快速、稳定、零成本的本地自动化测试。

## 2. 核心思路 (Core Concept)
本方案采用 **Mock Server + 环境隔离** 的策略：
- **Mock Server**: 在测试进程中启动一个本地 HTTP 服务器（例如使用 `wiremock` 或 `axum`），模拟 OpenAI 兼容的 LLM API 和 Telegram Bot API。
- **环境隔离**: 使用 `tempfile` 创建临时工作目录，防止测试过程中的文件操作（如 `write_file` 工具）污染真实项目代码。
- **依赖注入/配置覆盖**: 通过环境变量或配置文件，将系统的 LLM Base URL 和 Telegram API URL 指向本地 Mock Server。

## 3. 系统改造需求 (System Modifications Needed)
为了支持上述测试方案，需要对现有系统进行少量非侵入式改造：

### 3.1 Telegram 模块改造
修改 `src/telegram.rs`，允许通过环境变量（如 `TELEGRAM_API_URL`）覆盖默认的 Telegram API 地址。
```rust
// 伪代码示例
let mut bot = Bot::with_client(token.clone(), client);
if let Ok(api_url) = std::env::var("TELEGRAM_API_URL") {
    if let Ok(url) = reqwest::Url::parse(&api_url) {
        // teloxide 支持自定义 API URL
        // bot = bot.set_api_url(url); 
    }
}
```

### 3.2 LLM Client 模块
现有的 `OpenAiCompatClient` 已经支持 `base_url` 配置，无需修改代码。只需在测试时提供一个包含 `base_url = "http://127.0.0.1:<mock_port>/v1"` 的 `config.toml` 或环境变量即可。

### 3.3 可测试性暴露 (Testability)
确保 `AgentLoop` 和 `SessionManager` 的初始化逻辑可以被测试代码方便地���用，而不是全部硬编码在 `main.rs` 中。如果目前耦合较深，建议将 `main.rs` 中的核心组装逻辑提取到 `app.rs` 或 `lib.rs` 中，以便在测试用例中直接实例化。

## 4. 测试用例设计 (Test Cases Design)

### Case 1: CLI 基础对话与工具调用 (CLI Flow & Tool Usage)
- **Setup**: 启动 Mock LLM Server，配置 `base_url`。创建临时目录作为工作区。
- **Action**: 模拟用户在 CLI 输入 "创建一个名为 test.txt 的文件，内容为 hello"。
- **Mock LLM 行为**: 接收到 prompt 后，返回一个调用 `write_file` 工具的 JSON 响应。
- **Assert**: 
  - 检查临时目录中是否成功生成了 `test.txt` 且内容为 "hello"。
  - 检查 Agent 的最终输出是否包含成功提示。

### Case 2: Telegram 消息处理 (Telegram Flow)
- **Setup**: 启动 Mock LLM Server 和 Mock Telegram Server。配置 `TELEGRAM_API_URL`。
- **Action**: Mock Telegram Server 模拟推送一条 `/getUpdates` 响应，包含用户发送的消息 "执行 ls 命令"。
- **Mock LLM 行为**: 返回调用 `execute_bash` 工具的响应。
- **Assert**: 拦截 Mock Telegram Server 的 `/sendMessage` 请求，验证 Agent 是否将 `ls` 命令的执行结果发送回了正确的 `chat_id`。

## 5. 技术栈选择 (Tech Stack)
- **测试框架**: Rust 原生 `cargo test` + `tokio::test`。
- **Mock Server**: `wiremock` (推荐，专门用于 HTTP mock 测试，支持灵活的路由匹配和响应预设) 或 `axum` (手写简单的 mock 逻辑)。
- **断言库**: `assert_cmd` (用于黑盒测试 CLI 二进制文件) 或直接在代码层面实例化 `AgentLoop` 进行白盒测试。
- **环境隔离**: `tempfile` crate。

## 6. 实施步骤 (Implementation Steps)
1. **引入依赖**: 在 `Cargo.toml` 的 `[dev-dependencies]` 中添加 `wiremock`, `tempfile`, `assert_cmd`。
2. **系统改造**: 修改 `src/telegram.rs` 支持自定义 API URL。
3. **编写 Mock Server**: 在 `tests/integration/` 目录下编写通用的 Mock Server 启动函数。
4. **编写测试用例**: 实现 CLI 和 Telegram 的集成测试用例。
5. **CI 集成**: 在 GitHub Actions (或其他 CI 工具) 中添加 `cargo test --test integration` 步骤。
