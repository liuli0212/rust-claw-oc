# Browser Tool Design & Implementation Plan (rust-claw-oc)

## 1. 概述 (Overview)
本文档定义了 `rust-claw-oc` 中 `browser` 工具的系统架构与实施计划。该工具旨在赋予大语言模型（LLM）高级的网页浏览与交互能力。
有别于传统的 UI 自动化测试工具，AI 代理操作浏览器的核心难点在于 **DOM 降噪（提取关键结构以适应 Token 限制）** 以及 **在无状态的工具调用中维持浏览器的上下文与生命周期**。

## 2. 技术选型：为什么是 `chromiumoxide`？
在 Rust 生态中，我们坚决放弃传统的 Selenium/WebDriver 以及表面流行的 `headless_chrome`，全面拥抱 **`chromiumoxide`**（底层基于 Chrome DevTools Protocol, CDP）。

### 核心决策依据：
1. **纯粹的异步基因 (Async-First)**：`headless_chrome` 底层存在大量阻塞调用与 `std::thread` 的滥用，在 `tokio` 驱动的高并发 AI 代理系统中极易引发 Executor 饥饿死锁。而 `chromiumoxide` 从底层到上层完全基于 `futures` 和 `tokio`，事件流被抽象为优雅的 `Stream`。
2. **全面的 CDP 类型覆盖**：借助其代码生成机制，我们可以调用最深度的 CDP 接口（如无障碍树、精确的 Input 模拟等），不再受限于高层 API 的贫乏。
3. **更安全的生命周期管理**：通过独立的后台 Handler 任务管理进程通信，极大降低了僵尸 Chromium 进程耗尽主机内存的风险。

## 3. 核心架构设计

浏览器工具将被拆分为三个高度解耦的核心引擎：

### 3.1 会话与生命周期管理器 (Session & Lifecycle Manager)
负责管理底层 Chromium 进程或连接现有进程，使用 `Arc<RwLock<BrowserState>>` 维持状态。
- **沙盒模式 (Isolated/Sandbox)**：启动一个带有独立 `User-Data-Dir` 的无头/有头浏览器进程，确保每次任务环境纯净。
- **中继模式 (Relay/Attach)**：通过配置的 Remote Debugging Port (`--remote-debugging-port=9222`)，动态 Attach 到用户日常使用的浏览器中（兼容现版 OpenClaw 插件中继）。

### 3.2 快照引擎 (The Snapshot Engine) —— 核心技术壁垒
将包含数万节点的复杂 DOM 树压缩成 LLM 能够理解的高密度 Markdown。
- **注入式提取 (Injector)**：通过 `Runtime.evaluate` 注入一段高度优化的 `extractor.js`（在编译期通过 `include_str!` 打包到 Rust 二进制文件中）。
- **可见性裁剪 (Viewport Culling)**：剔除视口外、不可见或被遮挡的元素，只保留当前屏幕内有价值的信息。
- **交互语义提纯**：仅提取 `button`, `input`, `a`, 带有 `onclick` 监听器的元素，以及关键的文本节点。
- **扁平化引用映射 (Ref Mapping)**：为每个提取出的交互元素分配自增 ID（如 `[12]`），并将其确切坐标（X, Y）缓存到页面的全局变量（或 Rust 内存）中，彻底抛弃树状 JSON。

### 3.3 动作执行器 (The Action Engine)
暴露标准化的操作指令，接收来自大模型的扁平化 ID 进行精准打击。
- **CDP 精确制导**：摒弃不靠谱的 JS `element.click()`，根据 ID 查出精确坐标后，调用 `Input.dispatchMouseEvent` 模拟人类点击。
- **状态守卫 (Wait Strategies)**：动作执行后，强制进行智能等待（如等待 Network Idle 或 DOM 稳定），避免因页面渲染延迟导致的后续操作失败（Flaky）。

## 4. API 接口契约设计 (Tool Schema)

暴露给大模型的 Tool 结构（使用 `schemars` 导出给大模型）：

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct BrowserToolParams {
    /// 动作类型：status, start, stop, snapshot, act, navigate
    pub action: String, 
    
    /// 目标 URL (用于 navigate)
    pub target_url: Option<String>,
    
    /// 动作详情 (用于 act)
    pub request: Option<BrowserActionRequest>,
    
    /// 配置文件类型：'chrome' (接管) 或 'openclaw' (沙盒)
    pub profile: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct BrowserActionRequest {
    /// click, type, fill, evaluate...
    pub kind: String, 
    /// 对应快照中的扁平化 ID，例如 "15"
    pub target_id: Option<String>, 
    /// 键盘输入文本
    pub text: Option<String>, 
}
```

## 5. 实施路线图 (Implementation Roadmap)

- [ ] **阶段一：基础设施建立**
  - 在 `Cargo.toml` 中引入 `chromiumoxide`, `tokio`, `schemars`, `serde`。
  - 创建 `browser` 模块，搭建 Trait 接口框架。
  - 实现生命周期管理（Launch / Attach）及资源安全释放。

- [ ] **阶段二：快照引擎开发 (核心)**
  - 编写 `extractor.js`，实现 DOM 降噪、可见性计算与交互元素识别。
  - 实现 `snapshot` 动作，将执行结果格式化为极简 Markdown List（例如：`[15] button "提交"`）。
  - 处理节点 ID 映射缓存的跨会话共享问题。

- [ ] **阶段三：动作引擎开发**
  - 实现基于坐标的 CDP Input 操作（点击、击键输入、滚动）。
  - 实现强健的“操作后等待机制”。
  - 编写集成测试：使用 Rust 启动本地静态 HTML 服务器验证 100% 成功率。

- [ ] **阶段四：Tool 注册与 LLM 联调**
  - 将 Browser Tool 组装进 `rust-claw-oc` 的主循环体系。
  - 微调 Prompt 与返回格式，降低快照消耗的 Token。
