# Rusty-Claw 自动化测试与回归保障方案

## 1. 背景

随着 `rusty-claw-oc` 的执行链路越来越复杂，单靠手工回归已经很难稳定覆盖以下高风险路径：

- 用户输入进入 `SessionManager` / `AgentLoop` 后的完整执行链路
- LLM 触发工具调用、工具结果回填、继续推理直到结束
- CLI、Telegram 等入口对会话系统的正确接入
- 取消、中断、恢复、多轮会话、后台任务等状态性行为

如果继续把主要精力放在人工验证上，会出现三个问题：

- 回归成本持续上升，开发速度下降
- 缺陷复现依赖个人记忆，难以沉淀
- 看似“测过了”，实际只覆盖了 happy path

因此需要建立一套以自动化为主、手工检查为辅的回归保障体系。

## 2. 目标与非目标

### 2.1 目标

本方案的目标不是“模拟所有真实外部环境”，而是以最低维护成本保障系统基本可用。

核心目标：

- 覆盖系统最有价值的核心链路：用户请求 -> LLM 决策 -> 工具调用 -> 最终结束
- 显著降低日常手工回归压力
- 在不依赖真实 LLM/Telegram 网络的前提下，提供稳定、快速、可重复的测试
- 让已知 bug 可以沉淀为固定回归场景
- 让 CI 能稳定发现“基本功能坏了”的问题，而不是引入大量偶发红

### 2.2 非目标

以下内容不作为第一阶段目标：

- 不追求完整仿真 Telegram Bot API
- 不追求对第三方库内部行为做端到端验证
- 不追求用少量超重的黑盒测试覆盖全部逻辑
- 不追求把所有人工验证完全取消

## 3. 总体策略

本方案采用“分层保障”而不是“单一全链路集成测试”。

当前实施范围分为三层：

1. 会话级集成测试
2. 入口适配层测试
3. 少量黑盒冒烟测试

后续增强项：

4. 工具契约测试

各层职责如下：

### 3.1 会话级集成测试

这是主力测试层，直接围绕 `SessionManager`、`AgentLoop`、`Tool` 协议和 `AgentOutput` 进行验证。

特点：

- 不走真实网络
- 不依赖真实 LLM provider
- 使用假的 `LlmClient` 回放固定场景
- 尽量使用真实工具或受控的测试工具
- 重点验证“系统会不会按预期跑通”

这是最值得投入的层，因为它最接近系统核心价值，同时维护成本明显低于全链路 HTTP mock。

### 3.2 入口适配层测试

对 CLI、Telegram、Discord 等入口，验证“是否正确接入会话系统”，而不是去模拟整个外部生态。

例如：

- CLI 是否把输入正确送入 session
- Telegram handler 是否把 message / command 正确映射到 `SessionManager`
- output router 是否把结果路由回正确目标

这层是薄测试，不承担主要逻辑回归压力。

### 3.3 黑盒冒烟测试

保留极少量真正起二进制的 smoke test，用来证明：

- 程序能启动
- 最基础命令能跑
- 关键依赖没有在集成层面彻底断掉

黑盒测试数量必须严格控制，否则维护成本和脆弱性会快速上升。

## 4. 为什么不用“重型 Mock Server 方案”作为主线

初看之下，“Mock OpenAI 兼容 API + Mock Telegram API”似乎很直接，但它有几个问题：

- 当前 `OpenAiCompatClient` 是流式协议，包含文本增量、thinking、tool call 聚合等行为，mock 成本并不低
- 当前 Telegram 接入基于 `teloxide` dispatcher，若要走完整链路，需要模拟比 `/getUpdates` 和 `/sendMessage` 更多的行为
- 一旦外部库升级或协议细节变化，测试会优先坏在 mock 边界，而不是坏在我们真正关心的业务行为
- 这种测试通常慢、脆、难调试

因此，第一阶段不把 HTTP Mock Server 作为主方案，只将其保留为后续可选补充。

## 5. 测试分层设计

### 5.1 第一层：会话级集成测试

#### 5.1.1 测试边界

测试边界定义为：

- 从 `SessionManager::get_or_create_session(...)` 或直接构造 `AgentLoop` 开始
- 使用测试版 `LlmClient` 驱动流式输出或 tool call
- 使用真实 `Tool` 或测试 `Tool`
- 使用测试版 `AgentOutput` 捕获输出
- 在受控临时目录中验证副作用

不包含：

- 真实 CLI 终端交互
- 真实 Telegram 网络
- 真实 LLM HTTP API

#### 5.1.2 测试环境要求

- 每个测试使用独立 session id
- 每个测试使用独立临时工作目录
- 每个测试清理自己的 session/transcript/task state 产物
- 不依赖测试执行顺序
- 对异步后台行为设置明确超时

#### 5.1.3 LLM 测试替身策略

第一阶段不走 HTTP mock，而是实现“脚本式 LLM”。

建议新增一个测试辅助模块，例如：

- `tests/support/scenario_llm.rs`

该替身支持以下事件：

- 输出文本
- 输出 thinking
- 发出一个或多个 tool call
- 返回错误
- 返回 done

每个测试场景由固定脚本描述，例如：

1. 第一轮请求到来时，调用 `write_file`
2. 收到 `write_file` 工具结果后，调用 `finish_task`

这样可以把真实缺陷快速沉淀为稳定的回放场景。

#### 5.1.4 场景测试的首批覆盖面

第一批必须覆盖以下能力：

1. 单轮工具调用成功
2. 多轮工具调用
3. 工具调用失败后恢复
4. `finish_task` 正常结束
5. transcript/session 恢复
6. 取消当前任务
7. 背景 subagent 的创建、收集、取消
8. 大工具输出被压缩后，系统仍能继续执行
9. task state 在多轮中保持一致
10. 错误信息能够透出到输出层

### 5.2 后续增强：工具契约测试

这一层有价值，但不纳入当前实施范围。

原因：

- 当前最紧迫的问题是先把主执行链路的回归保障立住
- 如果现在同时铺开工具契约测试，会拉长首轮落地周期
- 部分工具的边界和返回约定还在演进，过早固化会增加维护负担

因此本层保留为第二阶段增强项，等会话级主干测试稳定后再补。

#### 5.2.1 重点工具

后续若启动本层，优先覆盖以下工具：

- `write_file`
- `patch_file`
- `execute_bash`
- `finish_task`
- `send_telegram_message`
- `task_plan`
- subagent 相关工具

#### 5.2.2 每个契约测试至少覆盖

- 合法输入成功
- 非法输入被拒绝
- 返回格式满足 `ToolExecutionEnvelope`
- 错误路径有可读错误信息
- 副作用仅发生在预期路径内

### 5.3 第三层：入口适配层测试

#### 5.3.1 CLI

CLI 不作为主集成层，而是验证薄接入：

- headless 模式能创建/获取 session
- 输入文本会进入 agent 执行
- 结束状态能正确映射到 stdout/stderr

这层测试优先走代码级调用，例如直接调用 `run_headless_command(...)`，只在少数情况下再起二进制。

#### 5.3.2 Telegram

Telegram 第一阶段不做完整 Bot API mock。

建议拆成两类测试：

- handler / command mapping 测试
- output routing 测试

验证重点：

- 用户消息能映射为正确 session id
- `/new`、`/cancel`、`/model` 等命令能进入正确分支
- Telegram 输出能路由到正确 `chat_id`

如果后续确实需要更强保障，可以在第二阶段再为 `run_telegram_bot` 增加受控适配层和有限 mock。

### 5.4 第四层：黑盒冒烟测试

黑盒测试限制在 2 到 5 个以内。

建议首批保留：

1. 二进制 headless 模式能启动
2. 通过固定测试配置运行一个最小场景
3. 配置缺失时输出明确错误

黑盒测试的定位是“证明系统没有整体瘫痪”，不是承担详细行为覆盖。

## 6. 首批测试用例清单

### 6.1 会话级集成测试

#### Case A1：创建文件并结束任务

- Setup：
  - 使用 `ScenarioLlm`
  - 工作目录切换到 `tempdir`
  - 注册真实 `WriteFileTool`
- LLM 脚本：
  - 第一步调用 `write_file`
  - 第二步调用 `finish_task`
- Assert：
  - 文件存在
  - 文件内容正确
  - 最终 summary 正确

#### Case A2：先读文件后写文件

- Setup：
  - 在 `tempdir` 预置输入文件
  - 注册真实 `ReadFileTool`、`WriteFileTool`
- LLM 脚本：
  - 第一步调用 `read_file`
  - 第二步调用 `write_file`
  - 第三步调用 `finish_task`
- Assert：
  - 输出文件内容符合预期
  - 历史里包含工具往返

#### Case A3：工具失败后恢复

- Setup：
  - 注册一个受控失败的测试工具
- LLM 脚本：
  - 第一步调用失败工具
  - 第二步根据错误改用正确工具
  - 第三步 `finish_task`
- Assert：
  - 中间错误被记录
  - 最终任务仍能成功结束

#### Case A4：session 恢复

- Setup：
  - 先创建 session 并完成一轮交互
  - 销毁内存中的 session
  - 重新 `get_or_create_session`
- Action：
  - 再次发送 follow-up
- Assert：
  - transcript 被正确加载
  - 第二轮能感知前文状态

#### Case A5：大输出压缩后继续执行

- Setup：
  - 注册返回超大字符串的测试工具
- LLM 脚本：
  - 先调用大输出工具
  - 再基于其结果调用 `finish_task`
- Assert：
  - 当前 turn 被压缩/截断
  - agent 没有因历史过大而崩溃

#### Case A6：取消当前任务

- Setup：
  - 使用会阻塞的测试 LLM 或测试工具
- Action：
  - 启动执行后触发 cancel
- Assert：
  - 任务及时退出
  - session 没有死锁
  - 后续输入还能继续处理

#### Case A7：后台 subagent 创建并收集结果

- 复用现有 `session_manager` 中的异步子代理测试思路
- Assert：
  - job 状态变化正确
  - 结果收集正确
  - 父会话能继续推进

#### Case A8：后台 subagent 失败/取消

- Assert：
  - 失败状态可观测
  - 取消不会阻塞父会话

### 6.2 后续增强：工具契约测试

#### Case B1：`write_file`

- 合法路径写入成功
- 非法参数失败
- 返回格式可被 envelope 解析

#### Case B2：`patch_file`

- 补丁成功
- 非法 patch 给出明确错误

#### Case B3：`execute_bash`

- 简单命令可执行
- 超时/非法输入/错误返回能被正确封装

#### Case B4：`send_telegram_message`

- 参数校验
- 错误消息可读
- 如果第二阶段支持可配置 base URL，再补网络层 mock

### 6.3 入口适配层测试

#### Case C1：headless CLI 能把输入送入 session

- 调用 `run_headless_command(...)`
- 使用测试版 `SessionManager`
- Assert 输出与退出状态

#### Case C2：Telegram 命令映射

- 构造命令 update
- 验证命令进入预期分支

#### Case C3：Telegram 输出路由

- 验证 `reply_to = telegram:<chat_id>` 时能构造正确输出对象

### 6.4 黑盒冒烟测试

#### Case D1：二进制最小启动

- 使用固定测试配置
- 运行一条 headless 命令
- 断言进程退出成功

#### Case D2：配置缺失时报错明确

- 不提供 LLM 配置
- 断言错误信息可读

## 7. 测试目录与代码组织

建议目录如下：

```text
tests/
  support/
    mod.rs
    scenario_llm.rs
    capture_output.rs
    temp_workspace.rs
    test_tools.rs
  session_flow.rs
  cli_adapter.rs
  telegram_adapter.rs
  smoke_headless.rs
```

说明：

- `tests/support/` 放测试基础设施
- 会话级集成测试单独成文件，避免和工具契约测试混杂
- 黑盒测试单独放，便于未来单独执行或标记

## 8. 需要的最小系统改造

本方案强调“最小但明确”的改造，不追求零改动。

### 8.1 必要改造

#### 改造 1：提取可复用的测试组装入口

目标：

- 避免测试反复复制 `main.rs` 中的组装逻辑
- 让 `SessionManager`、tool bootstrap、输出捕获更容易在测试中构建

建议：

- 将应用组装保持在 `app` 模块下
- 新增一个面向测试/应用初始化的轻量构造辅助，而不是让测试依赖 `main.rs`

例如可考虑提供：

- `build_app_bootstrap()`
- `build_session_manager_for_tests(...)`

当前仓库已经有 `build_app_bootstrap()`，测试侧只需要继续收敛调用方式，而不是再绕回 `main.rs`。

#### 改造 2：为高副作用测试提供受控工作目录

目标：

- 防止 `write_file` / `patch_file` / `execute_bash` 污染真实仓库

建议：

- 测试中统一切换到 `tempdir`
- 对依赖 `current_dir` 的初始化逻辑进行显式控制

如果发现某些模块把当前目录写死在构造时，应为其补充可注入路径的构造方式。

#### 改造 3：统一测试清理工具

目标：

- 清理 session 目录、task state、临时文件
- 避免测试互相污染

建议：

- 在 `tests/support/temp_workspace.rs` 中封装清理逻辑

### 8.2 暂不做的改造

第一阶段明确不做：

- 不为 Telegram bot 主循环引入完整 HTTP mock 适配
- 不强行给所有外部服务都增加可注入 base URL
- 不改造主循环去专门适配某一类端到端测试

如果后续确实需要真实协议级测试，再在第二阶段单独设计。

## 9. CI 策略

CI 必须区分快速稳定测试和较慢/较脆的测试。

建议分三档：

### 9.1 PR 必跑

- `cargo test --lib`
- `cargo test --test session_flow`
- `cargo test --test cli_adapter`

要求：

- 快
- 稳
- 不依赖外网
- 不允许 `#[ignore]`

### 9.2 每日或手动触发

- `cargo test --test telegram_adapter`
- `cargo test --test smoke_headless`
- 所有 timing-sensitive 场景

这类测试允许更慢，但也必须不依赖真实外部网络。

### 9.3 本地开发推荐

推荐提供如下开发习惯：

- 改核心执行链路后先跑会话级集成测试
- 改入口层后再跑适配层测试

## 10. 手工回归的最小清单

自动化不会完全替代手工验证，但可以把手工验证压缩为固定 checklist。

建议保留一个 5 分钟手工清单：

1. CLI 单轮问答
2. CLI 文件写入
3. CLI bash 执行
4. Telegram 收到一条普通消息并回复
5. 一个失败场景能看到可读错误

只有发版前、重大入口改造后或怀疑 UI/集成异常时才跑这份清单。

## 11. 实施计划

### Phase 1：搭测试基础设施

产出：

- `tests/support/scenario_llm.rs`
- `tests/support/capture_output.rs`
- `tests/support/temp_workspace.rs`
- 第一批受控测试工具

完成标准：

- 能写出一个完整的会话级集成 case

### Phase 2：补会话级主干用例

优先顺序：

1. 创建文件并结束任务
2. 多轮工具调用
3. 工具失败后恢复
4. session 恢复
5. 大输出压缩
6. cancel / subagent 场景

完成标准：

- 核心主循环的主要风险路径都至少有一个稳定 case

### Phase 3：补入口适配层与少量 smoke

优先顺序：

1. CLI headless 适配
2. Telegram handler / output routing
3. 极少量黑盒 smoke

完成标准：

- 外部入口至少有基本接入保障

### Phase 4：后续增强项

优先顺序：

1. 工具契约测试
2. 更强的 Telegram 集成保障
3. 其他高维护成本但高价值的补充测试

完成标准：

- 在不显著拖慢 CI 的前提下继续扩大回归面

## 12. 风险与控制措施

### 风险 1：测试仍然变脆

控制措施：

- 避免重型网络 mock
- 不把第三方库内部行为纳入主要回归面
- 对异步场景设置统一超时与轮询工具

### 风险 2：测试写得太多、太散

控制措施：

- 坚持分层
- 黑盒数量严格限制
- 每新增一个集成 case，都必须对应明确历史 bug 或高风险链路

### 风险 3：测试环境污染真实仓库

控制措施：

- 统一使用临时工作目录
- 统一清理 session 与状态文件
- 对有副作用工具优先使用受控路径

## 13. 结论

本方案不再把“完整模拟外部 API”作为第一选择，而是把回归保障的重心放在：

- 会话主链路
- 入口适配正确性
- 极少量黑盒冒烟

这是当前 `rusty-claw-oc` 更现实、更稳、更易维护的自动化测试策略。

按照该方案推进后，可以显著减少重复手工测试，同时把“基本功能可用”的保障建立在更稳定的自动化资产上。
