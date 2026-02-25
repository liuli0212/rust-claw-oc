# 阿里云模型支持实现设计方案

为了在 `rusty-claw-oc` 中支持阿里云 (DashScope) 模型，并允许通过命令行动态切换，我们将采取以下设计步骤：

## 1. 核心抽象：`LlmClient` Trait
目前 `GeminiClient` 被硬编码在多处。我们需要定义一个通用的接口，以便支持多种供应商。

```rust
// src/llm_client.rs

#[async_trait::async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn crate::tools::Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError>;

    // 可选：用于非流式请求
    async fn generate_text(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
    ) -> Result<String, LlmError>;
}
```

## 2. 阿里云适配器：`AliyunClient`
阿里云 DashScope 提供了与 OpenAI 兼容的接口。我们将实现一个 `AliyunClient`（或者更通用的 `OpenAiCompatClient`），它：
- 使用 `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions`。
- 将 `Message` 转换为 OpenAI 格式。
- 将我们的工具定义 (JSON Schema) 转换为 OpenAI 的 `tools` 格式。
- 解析 SSE 流，并将 OpenAI 的响应映射回我们的 `StreamEvent`。

## 3. 命令行切换模型
为了支持命令行动态切换，我们需要修改 `main.rs` 中的 `CliArgs` 和初始化逻辑：

- **添加 `--provider` 参数**：允许用户指定 `gemini` 或 `aliyun`。
- **更新 `--model` 参数**：根据 provider 提供默认模型名（如 `qwen-turbo`）。
- **环境变量支持**：读取 `DASHSCOPE_API_KEY`。

## 4. 实施步骤
1. **重构 `src/llm_client.rs`**：定义 `LlmClient` trait，并将 `GeminiClient` 改为实现该 trait。
2. **实现 `AliyunClient`**：在 `src/llm_client.rs` 中增加对阿里云/OpenAI 兼容协议的支持。
3. **修改 `src/session_manager.rs` 和 `src/main.rs`**：
   - 将 `Arc<GeminiClient>` 替换为 `Arc<dyn LlmClient>`。
   - 根据命令行参数动态选择实例化哪个客户端。

## 5. 预期用法示例
```bash
# 使用默认 Gemini (当前行为)
cargo run

# 使用阿里云模型
cargo run -- --provider aliyun --model qwen-max

# 使用指定的 Gemini 模型
cargo run -- --model gemini-1.5-flash
```

BOSS，如果您对这个方案满意，我将开始分步骤执行。第一步是重构 `llm_client.rs` 定义 Trait。
