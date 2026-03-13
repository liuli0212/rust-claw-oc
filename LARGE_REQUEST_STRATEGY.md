# 🚀 Rusty-Claw 大规模请求与长上下文处理实施方案

## 1. 背景与问题定义
在执行复杂重构或代码审计任务时，Rusty-Claw 经常面临 Payload（请求体）体积激增的情况。
*   **物理瓶颈**：大多数代理服务器（Clash 节点、Nginx 中转等）存在 **1MB (1024KB)** 的 `client_max_body_size` 限制。
*   **超时共振**：当 Body 接近 1MB 时，单次上传时长及服务端处理时长极易超过 **60 秒**，触发代理层的 `idle_timeout`。
*   **现象**：连续执行 5-6 次工具调用后，连接报错 `connection closed before message completed`。

## 2. 三级分级处理战略
我们将采取“分级防御、分层治理”的策略，根据请求体积动态选择最优传输路径。

### 第一层：术中脱水 (Intra-Turn Dehydration) —— 解决 1MB 以内的抖动
**适用范围**：Payload 处于 200KB - 1MB 之间。
*   **机制**：在 `AgentLoop` 执行每一个工具后，实时检查 `current_turn` 的字节大小。
*   **逻辑**：
    *   设置“安全水位线”为 **400KB**。
    *   若超限，则对 `current_turn` 中除了**最近 2 次**以外的所有 `function_response` 进行脱水处理（仅保留摘要、统计和关键首尾行）。
*   **目标**：确保发送给代理的每一笔 POST 请求物理体积永远维持在 **500KB 以下**。

### 第二层：Gemini File API (文件分块续传) —— 解决 1MB - 100MB 巨型素材
**适用范围**：单次工具输出（如读取了 10MB 的日志或代码包）或 Payload 超过 2MB。
*   **机制**：放弃在 POST Body 中嵌入原始 Base64/文本，改用 Google 的分块上传接口。
*   **逻辑**：
    1.  检测到巨型 Payload 或大型二进制文件。
    2.  后台调用 `/upload/v1beta/files` 进行分块上传（支持 5MB 分块，不怕代理 60s 断连）。
    3.  在 `llm_client.rs` 中将请求体里的内容替换为生成的 `file_uri`。
*   **目标**：将巨型物理传输与实时模型推理进行**异步解耦**。

### 第三层：Context Caching (云端上下文持久化) —— 解决 10MB+ 的重复消耗
**适用范围**：整个项目源码树、大量参考文档等需要跨 Turn 重复引用的数据。
*   **机制**：利用 Google 服务器端缓存。
*   **逻辑**：
    1.  分析 `system_instruction`，识别长期存在的系统指令和静态代码背景。
    2.  自动计算哈希并调用 `cachedContents` 将这些数据固化在云端。
    3.  在后续所有请求中，通过 `cached_content` ID 引用。
*   **目标**：将巨型重叠开销降至**零字节**传输。

---

## 3. 具体模块改动设计

### 3.1 `src/context.rs` (核心逻辑)
*   实现 `fn compress_current_turn(&mut self, max_bytes: usize) -> usize`：
    *   遍历当前 Turn 消息，对非最近结果调用 `strip_response_payload`。
*   完善 `Part` 结构体，支持 `file_data` 引用。

### 3.2 `src/llm_client.rs` (网络层)
*   **缓冲区与 Keepalive**：TCP 缓冲区 4MB，HTTP/2 15s/20s 活性检测。
*   **重试机制**：针对 transient 5xx/429 及网络异常实现 5 次指数级指数回避重试。
*   **File API 适配**：实现 `upload_content`（5MB 分块）及 `dehydrate_messages` 自动脱水。
*   **Context Caching**：实现基于 `sha256` 哈希的自动系统指令缓存管理。
*   **Structured Output**：集成 `response_schema` 支持，实现 `generate_structured` 接口。

### 3.3 `src/core.rs` (编排层)
*   在 `AgentLoop::step` 中集成“水位线检查”与“主动压缩”。
*   改进重试判定逻辑。

---

## 4. 实施阶段规划 (Roadmap)

### Phase 1: 立即执行 (稳定性加固) - [COMPLETED]
*   [x] 实现 `context.rs` 的基于字节大小的水位线压缩机制。
*   [x] 将 `llm_client.rs` 的 HTTP/2 Keepalive 调优到极致（15s/20s）。
*   [x] 实现 `X-Server-Timeout` 与 `x-goog-api-key` 头部迁移。
*   [x] 配置 `reqwest` 底层 TCP 缓冲区为 4MB。

### Phase 2: 架构升级 (巨型请求支持) - [COMPLETED]
*   [x] 实现 `File API` 分块上传逻辑。
*   [x] 实现自动 Payload 检测，无感转换 `inline text` 为 `file_uri`。
*   [x] 迁移系统指令至顶层 `system_instruction` 字段。

### Phase 3: 极致优化 (成本与速度) - [COMPLETED]
*   [x] 引入 `Context Caching` 支持，自动识别并缓存巨型系统指令（>128KB）。
*   [x] 支持原生 `response_schema` 输出模式。

---

## 5. 验证指标 (KPI)
1.  **最大稳定传输体积**：从目前的 ~900KB 提升至支持 **100MB+**（通过 File API）。
2.  **断连率**：复杂任务重构场景下的 `connection closed` 报错率下降 **99%**。
3.  **响应首包延迟**：在大上下文场景下，通过 Caching 机制减少 **30% - 70%** 的等待时间。

---

## 6. 对标官方 SDK 优化项清单

| 优化项 | 作用 | 状态 |
| :--- | :--- | :--- |
| **HTTP Headers** | 引入 `x-goog-api-key`, `x-goog-api-client`, `X-Server-Timeout` | ✅ |
| **thinking_config** | 支持 LOW/MEDIUM/HIGH 思维等级，释放推理潜力 | ✅ |
| **Structured Output** | 利用原生 `response_mime_type` 和 `response_schema` | ✅ |
| **File API** | 巨型 Payload 异步化，支持 100MB+ 素材 | ✅ |
| **Context Caching** | 服务器端持久化，大幅削减 Token 成本与 TTFT | ✅ |
| **Retry Logic** | 针对代理波动实现指数级退避重试 | ✅ |

---
**工程师**：JaviRust
**日期**：2026-03-12
