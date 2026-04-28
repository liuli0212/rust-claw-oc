# Rusty-Claw Code Mode Design

## Implementation Status

- [x] Phase 1 foundations: `ToolDefinition` / `ToolKind`, provider capability reporting, capability-gated code-mode prompt notices, and `code_mode` module scaffolding are implemented.
- [x] Phase 2 minimal exec runtime and guarded nested dispatch are implemented.
- [x] Phase 3 minimal `wait` / yield-resume semantics are implemented.
- [x] Phase 4 hardening items such as richer trace coverage, history/sanitization updates, and broader tests are implemented.
- [x] Phase 5 safe non-terminal drain lifecycle is implemented, including service-owned live workers and non-blocking `wait` polling via `wait_timeout_ms`.
- [x] Phase 6 replay removal and event-driven architecture: all replay state (`ResumeState`, `OutputBuffer`, `NotificationBuffer`, `CellResumeState`, `CellResumeProgressDelta`) is deleted. Runtime now emits structured `RuntimeEvent` via `event_tx` channel. `yield_control` is a non-blocking event emission that does not terminate JS execution.
- [x] Phase 7 channel-based tool bridge: completed as an intermediate bridge and now superseded by Phase 8. The old `tool_result_tx` / `tool_result_rx` relay has been removed.
- [x] Phase 8 unified tool execution refactor: service-owned nested tool fulfillment is replaced with `runtime -> CellRuntimeHost -> UnifiedToolExecutor -> Tool.execute`, as specified in [`CODE_MODE_UNIFIED_TOOL_EXECUTION_DESIGN.md`](CODE_MODE_UNIFIED_TOOL_EXECUTION_DESIGN.md).

## 1. Goal

Introduce a Codex-style "Code Mode" into `rust-claw-oc` so the model can orchestrate multiple tool calls inside a single LLM turn by emitting JavaScript source to a dedicated runtime, while still preserving ordinary direct tool calls as a first-class path.

The intended outcomes are:

- Reduce LLM <-> tool round trips on multi-step coding tasks.
- Allow loops, conditionals, retries, and local state inside one execution cell.
- Keep direct tool calls available for simple one-shot actions.
- Preserve Rusty-Claw's existing strengths:
  - explicit tool protocol
  - transcripted context/history
  - final visible text response lifecycle
  - sandbox and trace integration
  - skill and extension hooks

This document is tailored to the current codebase, especially:

- [`src/core.rs`](/Users/liuli/src/rust-claw-oc/src/core.rs)
- [`src/core/step_helpers.rs`](/Users/liuli/src/rust-claw-oc/src/core/step_helpers.rs)
- [`src/tools/protocol.rs`](/Users/liuli/src/rust-claw-oc/src/tools/protocol.rs)
- [`src/llm_client/protocol.rs`](/Users/liuli/src/rust-claw-oc/src/llm_client/protocol.rs)
- [`src/llm_client/openai_compat.rs`](/Users/liuli/src/rust-claw-oc/src/llm_client/openai_compat.rs)
- [`src/context/agent_context.rs`](/Users/liuli/src/rust-claw-oc/src/context/agent_context.rs)

## 2. Why Current Architecture Is Not Enough

Today Rusty-Claw uses standard JSON function tools:

1. `AgentLoop::step()` builds prompt + tool list.
2. `LlmClient::stream()` sends `tools: [{ type: "function", ... }]`.
3. The model emits `StreamEvent::ToolCall(FunctionCall, ...)`.
4. `execute_tool_round()` dispatches each tool call one by one.
5. Tool responses are appended back into context as `function` role messages.

This works, but it has a structural limitation:

- one reasoning step typically produces one batch of tool calls
- the model must wait for results before it can branch or loop again
- multi-file or multi-step workflows consume many extra turns

Codex-style code mode solves exactly this problem by adding a new layer:

- the model emits one freeform `exec` call containing raw JS
- JS runs inside a controlled runtime
- JS invokes nested tools via `await tools.some_tool(...)`
- the host fulfills these nested tool calls and feeds results back into the runtime
- the model only sees the aggregated `exec` output, not every intermediate round-trip

## 3. Design Principles

### 3.0 Additive, Not Substitutive

`exec` and direct tool calls are complementary, not mutually exclusive.

Rusty-Claw should support three execution styles:

- Direct Tool Mode
  - the model calls normal JSON function tools directly
  - best for single-step actions
- Code Mode
  - the model calls `exec` to orchestrate many nested tools inside one cell
  - best for search-read-filter-patch-verify flows
- Hybrid Mode
  - the model mixes both styles across turns
  - for example: use a direct tool to inspect state, then `exec` for batch work, then a final visible text response

Code mode must therefore be added as a higher-level orchestration capability, not as a replacement for the existing tool system.

### 3.1 Preserve Existing Tool Semantics

Nested code-mode tool calls must still go through the existing `Tool` trait:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;
}
```

We should not build a second, incompatible tool execution system.

### 3.2 Provider Capability Awareness

The current `OpenAiCompatClient` only supports standard JSON function tools. Code mode requires freeform/custom tool support. Therefore:

- providers that support custom/freeform tools can use true code mode
- providers that do not support it must fall back to current JSON function mode

This is a hard architectural requirement. Code mode cannot be implemented only in `core.rs`.

The design must assume:

- direct tool mode is the baseline capability
- code mode is enabled only when provider + endpoint support it, or when we intentionally expose a function-tool fallback form
- prompt instructions and visible tools must always match the actual provider capability

### 3.3 Side Effects Must Remain Explicit

Code mode JS must not get direct filesystem or network APIs. It should only gain power through nested tool calls. This keeps safety, tracing, skill policy, and sandboxing centralized in Rust.

### 3.4 Incremental Rollout

The design should support staged rollout:

1. Phase 0: provider/runtime/context feasibility spikes
2. Phase 1: provider-aware `exec` with runtime + nested tool bridge
3. Phase 2: resumable `wait`, richer output items, stored state, and orchestration polish

## 4. Proposed User-Facing Behavior

When enabled, the model may see two special tools in addition to the normal tool set:

- `exec`: accepts raw JS source
- `wait`: resumes a still-running execution cell

The model is instructed:

- prefer `exec` for multi-step workflows
- prefer direct JSON function tools when a single tool call is enough
- use `wait` only if `exec` previously yielded a running cell

Examples of tasks that should benefit:

- search/inspect many files, then patch selected ones
- run several shell checks and branch on outputs
- batch read/transform/write flows
- retry loops around flaky operations

Tasks that should still use plain function tools:

- a single `read_file`
- one quick `execute_bash`
- one `ask_user`
- immediate final visible text response

The important behavioral rule is:

- `exec` is optional and situational
- direct tool calls remain available and valid even when code mode is enabled

## 5. High-Level Architecture

### 5.1 New Modules

Add a new top-level module group:

- `src/code_mode/mod.rs`
- `src/code_mode/description.rs`
- `src/code_mode/runtime/mod.rs`
- `src/code_mode/runtime/globals.rs`
- `src/code_mode/runtime/callbacks.rs`
- `src/code_mode/runtime/timers.rs`
- `src/code_mode/runtime/value.rs`
- `src/code_mode/service.rs`
- `src/code_mode/response.rs`

This mirrors the separation that worked well in `codex-rs`:

- description/tool metadata
- runtime/isolate execution
- service/session lifecycle
- response/output types

### 5.2 Existing Integration Points

The new module integrates into:

- `src/llm_client/protocol.rs`
  - add custom/freeform stream events and capability metadata
- `src/llm_client/openai_compat.rs`
  - serialize `exec` as a custom tool instead of a normal function tool
  - parse custom tool call deltas from the provider stream
- `src/tools/mod.rs`
  - export `ExecTool` and `WaitTool`
- `src/core/step_helpers.rs`
  - dispatch `exec` and `wait` through a specialized service
  - preserve tracing, cancellation, and context recording
- `src/context/*`
  - store `exec` calls and outputs in the same turn model without losing replay fidelity

Additional integration files that must be considered explicitly:

- `src/session/factory.rs`
  - subagent tool filtering and runtime-tool visibility rules
- `src/context/sanitize.rs`
  - history sanitization and envelope-aware response stripping
- `src/context/history.rs`
  - current-turn compression, tool-result truncation, and argument summarization in compacted history
- `src/telegram/output.rs`
  - user-facing rendering/summarization of tool starts and tool outputs for chat surfaces

### 5.3 Provider Compatibility Matrix

Before implementation, Rusty-Claw should maintain an explicit compatibility table for target providers/endpoints.

Suggested columns:

- provider name
- endpoint family
- standard function tools
- custom/freeform tools
- streaming custom tool calls
- recommended code mode path
- fallback path

Initial example:

| Provider | Endpoint | Function Tools | Custom Tools | Streaming Custom Calls | Recommended Path | Fallback |
|------|------|------|------|------|------|------|
| OpenAI-compatible chat completions | chat/completions-style | Yes | Usually No | Usually No | Direct Tool Mode | `exec({code})` function fallback or omit |
| Responses/custom-tool capable endpoint | responses-style | Yes | Yes | Yes | Native freeform `exec` | Direct Tool Mode |
| Gemini current path in this repo | Existing Gemini adapter path | Verify | Verify | Verify | Unknown until spike | Direct Tool Mode |

This matrix should be treated as a design input, not an afterthought.

## 6. Tool Model Changes

### 6.1 Problem

The current `Tool` trait only models JSON-schema function tools. Code mode needs two tool classes:

- function tool
- freeform/custom tool

### 6.2 Proposed Abstraction

Introduce a richer tool definition model alongside the existing trait:

```rust
pub enum ToolKind {
    Function,
    Freeform,
}

pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub kind: ToolKind,
    pub input_schema: Option<serde_json::Value>,
    pub freeform_format: Option<FreeformToolFormat>,
}
```

Where native freeform tools need extra wire-format metadata:

```rust
pub struct FreeformToolFormat {
    pub syntax: String,
    pub definition: String,
}
```

Then extend `Tool` with a defaulted method:

```rust
fn definition(&self) -> ToolDefinition {
    ToolDefinition {
        name: self.name(),
        description: self.description(),
        kind: ToolKind::Function,
        input_schema: Some(self.parameters_schema()),
        freeform_format: None,
    }
}
```

Benefits:

- keeps old tools working unchanged
- allows `exec` to be declared as `Freeform`
- keeps serialization responsibility in the client layer

### 6.3 Tool Execution Path

Do not special-case nested code-mode tool invocation at the trait layer. Instead:

- external model-facing `exec` and `wait` appear as tools
- inside runtime, nested tool calls target the existing normal tool registry
- host executes nested tools by reusing current dispatch logic and `ToolContext`

This reuse must include not only raw tool execution, but also the relevant AgentLoop-level control logic that currently lives above tools, especially:

- autopilot/TODOS gating
- repeated-action / deadloop protection
- trace span creation and propagation
- selected tool-effect handling boundaries

Otherwise code mode would accidentally bypass protections that are currently enforced in `execute_tool_round()`.

Design clarification:

- `CodeModeService` should own runtime/session mechanics
- `AgentLoop` should remain the owner of mutable orchestration state and policy state
- guarded nested dispatch should therefore live in an AgentLoop-owned host adapter, not in the runtime crate itself

Examples of AgentLoop-owned state that must not silently fork:

- `action_history`
- `reflection_strike`
- autopilot/TODOS state
- `task_state_store`
- evidence and tool-effect handling
- output routing and task-finish signaling

Phase 0 must explicitly assess whether this state can be:

- reused through extraction of a shared guarded-dispatch helper
- or only reused by routing nested calls back through AgentLoop-owned methods

If that refactor is not feasible without large churn, the first implementation scope must be reduced accordingly.

### 6.4 Exec Input Canonical Form

There are two externally visible ways to represent `exec` input:

1. Native custom-tool path
   - provider sends raw string input
   - example: `input = "const x = await tools.read_file(...)"` 
2. Function-tool fallback path
   - provider sends JSON args
   - canonical shape: `{ "code": "const x = await tools.read_file(...)" }`

To avoid history and replay divergence, Rusty-Claw should normalize both into the same internal canonical form when storing them in context/transcripts:

```json
{
  "code": "const x = await tools.read_file(...)"
}
```

Recommendation:

- provider wire format may differ
- persisted conversation format should be canonicalized to JSON object form
- top-level transcript replay should not depend on whether the upstream provider used native custom tools or function fallback

Concrete integration hooks:

- Inbound normalization hook
  - when streaming model output, normalize any native custom `exec`/`wait` invocation into canonical `FunctionCall.args`
  - intended insertion point: the stream collection path that currently handles `StreamEvent::ToolCall(...)`
- Outbound replay hook
  - when serializing assistant history back to a provider that expects native custom tools, detect canonicalized `exec`/`wait` calls and rehydrate them into provider-native raw input
  - intended insertion point: the client-layer serialization path that currently emits assistant `tool_calls`

This way:

- context/transcript stay stable and provider-agnostic
- provider wire compatibility is handled only at the edge

## 7. LLM Client Changes

### 7.1 Capability Negotiation

Add provider capability metadata:

```rust
pub struct LlmCapabilities {
    pub function_tools: bool,
    pub custom_tools: bool,
    pub parallel_tool_calls: bool,
    pub supports_code_mode: bool,
}
```

Extend `LlmClient`:

```rust
fn capabilities(&self) -> LlmCapabilities;
```

Initial expectation:

- `OpenAiCompatClient`: `function_tools = true`
- `custom_tools = false` at first unless the target endpoint is Responses/custom-tool compatible

If the chosen endpoint cannot do native custom tools, Rusty-Claw should choose one of two explicit strategies:

- omit `exec`/`wait` entirely and stay in Direct Tool Mode
- expose a function-tool fallback form such as `exec({ code: string })`

Prompt and tool list must strictly align with the chosen strategy.

### 7.2 Stream Protocol Extension

Current `StreamEvent` only supports JSON `ToolCall(FunctionCall, Option<String>)`.

Add:

```rust
pub enum ToolInvocation {
    Function(crate::context::FunctionCall),
    Custom {
        name: String,
        input: String,
        id: Option<String>,
    },
}

pub enum StreamEvent {
    Text(String),
    Thought(String),
    ToolCall(ToolInvocation, Option<String>),
    Error(String),
    Done,
}
```

This is the minimum protocol extension needed to represent freeform tool calls.

However, this extension should only be introduced if we actually support native custom tools on the target provider path.

If the first implementation uses function-tool fallback form only, we can defer this protocol change and represent `exec` as an ordinary `FunctionCall`.

That gives us two valid implementation paths:

- Native path
  - change `StreamEvent`
  - support raw custom/freeform tool invocations
- Fallback path
  - keep current `StreamEvent`
  - model calls `exec` with JSON args `{ "code": "..." }`

If the native path is chosen, the implementation should add an explicit normalization helper so that custom invocations are converted into canonical `FunctionCall`-compatible replay form before they are stored in context.

### 7.3 OpenAI-Compatible Serialization

For function tools, keep current behavior.

For freeform tools, serialize like Codex-style custom tools when supported:

```json
{
  "type": "custom",
  "name": "exec",
  "description": "Run JavaScript code to orchestrate tool calls",
  "format": {
    "type": "grammar",
    "syntax": "lark",
    "definition": "..."
  }
}
```

Important note:

- this is only valid on providers/endpoints that actually support custom tools
- `OpenAiCompatClient` likely needs either:
  - a new Responses-API-oriented client, or
  - an extended compatibility mode that can talk to a custom-tool-capable endpoint

Recommendation:

- do not overload current chat-completions-only code too aggressively
- add a dedicated client path if the upstream provider format diverges materially

Native-path requirement:

- `ToolDefinition` must carry enough metadata for client serialization to emit not only `type: "custom"` but also the associated grammar/format block
- `ToolKind::Freeform` by itself is insufficient

Decision gate:

- before implementation begins, explicitly decide whether the first target is:
  - native freeform `exec`
  - or function-tool fallback `exec({code})`

## 8. Code Mode Runtime

### 8.1 Runtime Choice

There are three realistic options:

1. `rusty_v8`
2. `boa_engine`
3. `rquickjs`

Recommendation:

- do not lock this choice before a spike
- evaluate at least:
  - `boa_engine`
  - `rquickjs`
  - `rusty_v8`

Decision criteria:

- promise / await support
- interruption / termination support
- callback ergonomics
- build complexity
- portability inside this repo

Current preference:

- `rquickjs` or `boa_engine` if async semantics are solid enough
- `rusty_v8` only if smaller engines fail the spike

### 8.2 Runtime Contract

Each `exec` call creates an isolated execution cell with:

- JS source code
- enabled nested tool metadata
- a copy of stored values for the session
- output buffer
- pending tool calls
- cancellation handle

The runtime exposes only controlled globals:

- `tools`
- `ALL_TOOLS`
- `text(value)`
- `store(key, value)`
- `load(key)`
- `notify(value)`
- `yield_control()`
- `setTimeout(callback, delayMs?)`
- `clearTimeout(id?)`
- `exit()`

Optional Stage B helpers:

- `image(...)`
- `parallel(...)`

No direct access to:

- filesystem
- sockets
- process spawning
- environment variables
- `require` / module import

### 8.3 Nested Tool Bridge

When JS runs:

```js
const result = await tools.read_file({ path: "src/core.rs" });
```

the runtime should:

1. serialize the argument into JSON
2. emit `RuntimeEvent::ToolCall`
3. let the host execute the real Rust tool
4. resolve or reject the JS-side promise

This bridge must reuse:

- `ToolContext`
- sandbox enforcement
- skill/runtime extension enrichment
- trace propagation
- cancellation behavior

In other words, nested code-mode tools are not a new execution system; they are a new orchestration surface over the same execution system.

This is a hard requirement:

- nested `ToolContext` construction must flow through the same extension enrichment path used by top-level tool execution
- otherwise extensions such as sandbox injection can be bypassed

Concretely, the nested path should preserve the equivalent of:

- visible tool population
- `skill_budget` propagation
- trace propagation
- repeated `ext.enrich_tool_context(ctx).await`

Nested tool failures should propagate like this:

- a nested tool success resolves the JS-side awaitable value
- a nested tool failure rejects the JS-side awaitable value
- JS can handle the failure with `try/catch`
- if uncaught, the whole `exec` call fails with a clear summarized error

Nested trace propagation should work like this:

- outer `exec` starts a code-mode span
- every nested tool call receives a `ToolContext.trace` derived from that span
- nested tool spans use the outer exec span as parent
- top-level iteration trace remains the ancestor of the whole cell

The intended hierarchy is:

- AgentLoop turn/iteration span
- outer exec tool span
- nested tool span

This hierarchy must remain visible in the existing trace bus.

## 9. Code Mode Service

Introduce a `CodeModeService` owned by `AgentLoop` or injected beside it.

Responsibilities:

- allocate `cell_id`
- spawn runtime cells
- manage `wait`
- manage stored values across `exec` calls in one agent session
- record nested tool request/done events for summaries and wait/flush publication
- support terminate/cancel

Non-responsibilities:

- `CodeModeService` should not become the owner of AgentLoop guardrail state
- it should not fulfill nested tool calls; `CellRuntimeHost` delegates guarded nested dispatch to `UnifiedToolExecutor`

### 9.1 Suggested Traits

```rust
#[async_trait]
pub trait CodeModeTurnHost: Send + Sync {
    async fn invoke_tool(
        &self,
        tool_name: String,
        input: Option<serde_json::Value>,
        parent_span_id: Option<String>,
    ) -> Result<serde_json::Value, crate::tools::ToolError>;

    async fn notify(
        &self,
        exec_call_id: String,
        cell_id: String,
        text: String,
    ) -> Result<(), crate::tools::ToolError>;
}
```

This sketch was the earlier host-adapter direction and maps cleanly onto `dispatch_tool_call()`.
The implemented Phase 8 boundary is `CellRuntimeHost`, which owns runtime-facing host calls and delegates actual tool execution to `UnifiedToolExecutor`.

`invoke_tool(...)` should be implemented as guarded nested dispatch, not as a thin wrapper around `tool.execute(...)`.

That means it should apply the relevant top-level safety rules before and around nested execution, instead of letting JS directly bypass them.

At minimum, the guarded nested dispatch path must preserve:

- autopilot restrictions around side-effecting tools
- repeated-action / reflection-strike style loop protection
- timeout and cancellation behavior
- trace/span parentage using `parent_span_id`

Implemented ownership split:

- runtime/service layer:
  - cell lifecycle
  - runtime events
  - wait/terminate behavior
- AgentLoop host adapter:
  - runtime-facing `CellRuntimeHost`
  - guarded nested dispatch through `UnifiedToolExecutor`
  - access to mutable run/autopilot state
  - trace parent propagation
  - top-level output/effect integration policy
  - extension-based `ToolContext` enrichment

## 10. Core Loop Integration

## 10.1 Current Flow

Current execution in `step()` is roughly:

1. collect stream events
2. accumulate text + tool calls
3. record model turn
4. execute tool round
5. append function responses
6. continue until final visible text response

## 10.2 New Flow

With code mode enabled:

1. collect stream events
2. allow either:
   - normal function tool calls
   - custom `exec` / `wait`
3. for normal tool calls, keep existing path
4. for `exec` / `wait`, route to `CodeModeService`
5. convert code-mode outputs into the same context message format used for other tool responses

Suggested rule:

- `exec`/`wait` are handled as tools from the model's perspective
- nested tool calls inside `exec` do not get appended individually to context
- only the `exec`/`wait` result is appended back to the conversation

This preserves the token-saving benefit.

Important semantic rule:

- nested completion signaling is not allowed in early phases
- final visible text response remains the top-level lifecycle signal
- if `exec` determines the task is complete, it should return that conclusion to the model, and the model may then provide a final visible text response

Nested dispatch rule:

- nested tool calls must flow through a controlled host path
- they must not directly call raw `tool.execute(...)` without the surrounding AgentLoop safeguards

## 10.3 Transcript Representation

We should preserve auditability without replaying every nested call into main dialogue history.

Recommended approach:

- main conversation transcript stores:
  - model emitted custom tool call `exec(...)`
  - function response/result from `exec`
- trace/event log stores:
  - nested tool call timeline
  - nested tool args previews
  - nested tool outputs previews

This keeps prompt history compact while preserving observability.

Failure visibility rule:

- aggregated `exec` output must include enough error locality for the model to recover
- at minimum, include:
  - nested tool name
  - error summary
  - optional short args preview
- full nested detail belongs in trace/event logs, not prompt history

## 11. Context and Prompt Changes

Update [`src/context/agent_context.rs`](/Users/liuli/src/rust-claw-oc/src/context/agent_context.rs) system prompt to include a code-mode protocol, only when the chosen execution path allows it.

Suggested additions:

- For multi-step work, prefer `exec` to compose tool calls in one turn.
- `exec` input must be raw JavaScript source, not JSON or markdown fences.
- Use direct tools only for trivial one-shot actions.
- Use `wait` only after an `exec` result explicitly says the cell is still running.

Important:

- do not enable this prompt universally if the provider cannot actually accept custom tools
- prompt and tool availability must stay aligned

Concrete insertion point:

- capability-dependent code-mode prompt text should be injected via `AgentContext.execution_notices`
- this fits the current prompt assembly path without mutating the static `system_prompts` baseline
- static identity text in `system_prompts` should remain provider-agnostic

Additional context integration requirements:

- `sanitize.rs` must know how to strip/compress persisted `exec` results without destroying their envelope semantics
- `history.rs` must define how large `exec` args/results are summarized inside compaction and truncation paths

### 11.1 Context Data Representation

For replay fidelity and compatibility with the existing context model:

- top-level `exec` should be stored as a normal `Part::function_call`
- its canonical args should be JSON object form:
  - `{ "code": "..." }`
- top-level `exec` result should be stored as `Part::function_response`
- no new `Part` variant is required in early phases

This aligns with the existing structures in [`src/context/model.rs`](/Users/liuli/src/rust-claw-oc/src/context/model.rs).

### 11.2 Output Truncation Strategy

`exec` can generate much larger output than ordinary tools. Therefore:

- the code-mode runtime should enforce an output size budget
- when exceeded, aggregate output should be truncated before being inserted into context
- transcript may keep a larger version if desired, but prompt history should receive the bounded version

Recommended initial rule:

- cap visible `exec` response text to a dedicated threshold separate from generic tool output
- mark truncation explicitly in the returned text/envelope

### 11.3 Early-Phase Effect Boundaries

Today `AgentLoop` extracts important effects from top-level tool envelopes, such as:

- final response summary
- `await_user`
- file/evidence side effects

Early-phase code mode should not attempt to support the full effect surface area for nested tools.

Recommended boundary:

- nested completion signaling is disallowed
- nested `ask_user` is disallowed
- `ExecTool` should return its own top-level envelope/result
- only a small, explicit subset of nested-tool side effects may be summarized into the final `exec` result if needed

This keeps the first implementation from turning into a full "effect merge engine".

Stage A explicit rule:

- `ExecTool` should always return a valid `ToolExecutionEnvelope` JSON string
- even if the payload is text-only, it should still flow through the existing envelope parser
- recommended `payload_kind`: `code_mode_exec`

This avoids creating a second success path outside the current `handle_successful_tool_effects()` / envelope parsing pipeline.

Output-surface implication:

- chat-oriented `AgentOutput` implementations such as Telegram should continue to see standard top-level tool lifecycle events
- `ExecTool` therefore needs a concise, stable visible summary so chat surfaces do not become unreadable when code mode is used

## 12. Output Model

Current tool outputs are strings, often JSON envelopes serialized as strings.

Code mode should return structured content items internally:

```rust
pub enum ExecOutputItem {
    Text(String),
    Image { image_url: String, detail: Option<String> },
}
```

But the outer tool response returned to the model can initially stay simple:

- a JSON string envelope consistent with current tool response patterns
- whose `result.output` may contain plain aggregated text

Recommendation:

- Stage A: text-only payload wrapped inside `ToolExecutionEnvelope`
- Stage B: structured multimodal output if the provider path needs it

## 13. Safety Model

## 13.1 Runtime Safety

JS code is untrusted model output. Safety controls:

- isolated runtime per cell
- no filesystem/network/process globals
- import disabled
- output size limits
- execution timeout
- explicit termination on user cancel

## 13.2 Tool Safety

Nested tools still honor:

- `ToolContext.sandbox`
- skill policy
- visible tool list
- session timeout/remaining step budget

This is a major advantage over giving JS direct system APIs.

### 13.2.1 Guarded Nested Dispatch

One additional safety requirement is specific to this repository:

many operational protections are currently enforced in `AgentLoop` orchestration code, not inside the raw `Tool` implementations.

Examples include:

- autopilot `TODOS.md` gating
- action deduplication / repeated-failure detection
- reflection strike / meltdown escalation

Therefore, code mode must not treat nested tool execution as a direct call to `tool.execute(...)`.

Instead, nested execution must go through a guarded dispatch path that reproduces the relevant orchestration-time checks for JS-originated tool calls.

Implementation note:

- reuse should happen by extracting or re-invoking shared guardrail logic from AgentLoop paths
- not by duplicating a second, drifting copy of the same protections inside `code_mode`

Subagent/tool-visibility implication:

- the nested allow-list must also respect the effective visible-tool filtering model already used for subagents and restricted sessions
- implementation should not assume that every top-level registered tool is always eligible for nested code-mode calls

## 13.3 Policy Restrictions

Initially exclude some tools from nested code mode:

- `ask_user`
- `subagent`
- perhaps scheduler or long-lived daemon tools

Rationale:

- they complicate control flow and resumption
- they can be added later after the base runtime is stable

Introduce an allow-list:

```rust
fn is_code_mode_nested_tool(tool_name: &str) -> bool
```

Stage A recommended allow-list:

- read/write/patch file tools
- shell/bash tools
- web fetch/search
- lsp read tools
- memory read/write if desired

Explicitly excluded in early phases:

- final visible text response
- `ask_user`
- `subagent`

Reason:

- these tools alter top-level control flow or hand execution to another actor
- they should remain top-level decisions until code mode semantics are proven

This exclusion is especially important because some of these tools do not just return data; they also trigger control-flow effects in the outer loop.

## 14. Error Handling

Errors can occur at three layers:

1. provider/custom-tool parsing
2. JS runtime evaluation
3. nested tool execution

Design rules:

- JS syntax/runtime errors become `exec` tool errors, not fatal process errors
- nested tool failures reject the JS promise so the model can handle them in JS with `try/catch`
- host-level unrecoverable runtime failures return a clear `exec runtime failed: ...`
- cancellation should terminate the cell and surface a normal interrupted result

For guarded nested dispatch specifically:

- guardrail-triggered denials should be surfaced back into JS as ordinary rejected tool calls
- JS may choose to recover from them
- if uncaught, they should appear in the outer `exec` result with enough locality for the model to change strategy

## 15. Trace and Telemetry

This project already has a strong trace model. Code mode should extend it instead of bypassing it.

Add trace spans/events for:

- `code_mode_exec_started`
- `code_mode_exec_yielded`
- `code_mode_exec_finished`
- `code_mode_exec_terminated`
- `code_mode_nested_tool_started`
- `code_mode_nested_tool_finished`

Attributes to record:

- `cell_id`
- outer tool call id
- provider/model
- source length
- nested tool count
- total output size
- termination reason

This data should go into the existing trace bus, not a separate telemetry path.

Implementation requirement:

- nested tool tracing must not collapse into flat events
- the trace tree should preserve the parent-child relation between iteration, exec, and nested tool execution

## 16. Phased Rollout Plan

## Phase 0: Feasibility Spikes

- build provider compatibility matrix
- decide first target path:
  - native freeform `exec`
  - or function-tool fallback `exec({code})`
- evaluate `execute_tool_round` guardrail reuse/refactor feasibility
- spike runtime engines for:
  - promise/await
  - callback bridge
  - cancellation
- define canonical context/transcript representation for `exec`

Exit criteria:

- provider capability reporting works
- chosen runtime passes minimal async viability checks
- canonical `exec` replay format is agreed
- there is a concrete plan for reusing or extracting guarded dispatch without bypassing AgentLoop state

## Phase 1: Foundations

- Add `ToolKind` / `ToolDefinition`
- Extend `LlmClient` capability reporting
- Add code-mode prompt fragments behind capability checks
- If native path is selected, add stream protocol support for custom tool calls
- Add freeform-tool format metadata for native client serialization
- Decide the concrete integration points in:
  - `session/factory.rs`
  - `context/sanitize.rs`
  - `context/history.rs`
  - output adapters such as `telegram/output.rs`

Exit criteria:

- normal JSON tools still work unchanged
- providers can declare whether custom tools are supported
- prompt/tool visibility stays aligned with capabilities
- native freeform metadata is sufficient to serialize `exec` without hard-coded special cases

## Phase 2: Minimal Exec

- Add `ExecTool`
- Add `code_mode::service`
- Add JS runtime with:
  - `tools`
  - `text`
  - `exit`
  - `store/load`
- If fallback path is selected, expose `exec({code})`
- If native path is selected, expose freeform `exec`
- Route nested tools through existing tool execution path
- Add guarded nested dispatch so JS-originated tool calls do not bypass existing AgentLoop protections
- Canonicalize persisted `exec` calls into `{ "code": ... }` replay form
- Return `ExecTool` results as `ToolExecutionEnvelope`
- Ensure nested `ToolContext` goes through extension enrichment before tool execution

Exit criteria:

- one `exec` can call nested tools and return aggregated text
- nested side-effecting tools still respect autopilot / loop-protection rules
- code-mode outputs successfully flow through the existing envelope/effects parser
- nested code-mode calls still receive sandbox and other extension-provided context

## Phase 3: Wait / Yield

- [x] Add `WaitTool`
- [x] add `yield_control()`
- [x] add resumable cells
- [x] support session-scoped stored values
- [x] add timed yielding helpers such as `setTimeout()`-driven resume flows

Exit criteria:

- [x] long-running `exec` can yield and resume safely

## Phase 4: Hardening

- [x] add timeout helpers
- [x] add output truncation/token budget controls
- [x] add trace/event-log visibility
- [x] add allow-list / policy guards
- [x] expand tests

## 17. Testing Strategy

Add tests at four layers.

### 17.1 Unit Tests

- `parse_exec_source`
- tool name normalization
- provider capability gating
- transcript serialization of custom tool calls
- native/custom `exec` canonicalizes into `{ "code": ... }`
- canonical `{ "code": ... }` rehydrates into provider-native custom input when needed
- `sanitize.rs` preserves `exec` envelope semantics while stripping large responses
- `history.rs` compaction summarizes large `exec` args/results safely

### 17.2 Runtime Tests

- `text("hello")` returns output
- `exit()` stops execution
- `store/load` survive across cells in one session
- `setTimeout` + `yield_control`
- unsupported import fails

### 17.3 Service Tests

- `exec` creates `cell_id`
- `wait` returns incremental output
- terminate waits for runtime shutdown
- nested tool result resolves JS promise
- nested guarded dispatch receives and applies parent trace/span linkage
- nested guarded dispatch does not bypass autopilot denials
- `ExecTool` emits a valid `ToolExecutionEnvelope`
- nested `ToolContext` is passed through extension enrichment before execution

### 17.4 Integration Tests

- model emits `exec`, host executes nested `read_file` + `patch_file`
- top-level final visible text response still ends the run after code mode output indicates completion
- unsupported provider falls back to standard tool mode
- repeated failing nested side-effecting calls still trigger the intended protection path

## 18. Open Questions

### 18.1 Runtime Engine

Do not assume `boa_engine` is sufficient without a spike.

- promise support maturity
- interrupt/termination behavior
- callback ergonomics under Tokio

If those prove insufficient, move runtime internals to `rquickjs` or `rusty_v8` behind the same service boundary.

### 18.2 Provider Path

The biggest non-runtime risk is provider support. Current `OpenAiCompatClient` appears chat-completions oriented. If the target deployment wants Codex-like custom tools, we may need:

- a dedicated Responses API client
- or a second OpenAI-compatible path specialized for custom tools

This is likely the single most important architectural decision, and it should be made in Phase 0 rather than deferred.

### 18.3 Context Replay

We must decide how much nested-tool detail should be stored in transcripts vs only trace logs. Recommendation remains:

- compact in prompt history
- rich in trace logs

The canonical replay format should remain:

- provider-agnostic in stored context
- edge-converted at client serialization/deserialization time

### 18.4 Guardrail Reuse Boundary

We still need to decide exactly which current AgentLoop protections are:

- reused as-is
- refactored into a shared guarded-dispatch layer
- intentionally left top-level only

This is an implementation-shaping decision, because too little reuse creates safety gaps, while too much reuse risks duplicating the whole outer loop inside code mode.

### 18.5 Additional Integration Surface

The design must continue to track how code mode interacts with:

- subagent/restricted-session tool filtering in `session/factory.rs`
- sanitization and compression in `context/sanitize.rs`
- history compaction/truncation in `context/history.rs`
- output adapters such as `telegram/output.rs`

These are not optional polish files; they shape whether code mode behaves coherently across the existing product surfaces.

## 19. Recommended First Implementation Scope

To keep the first milestone realistic, implement one of two explicit entry paths.

### Option A: Function Fallback First

- keep current `StreamEvent`
- expose `exec` as a normal function tool with args `{ "code": "..." }`
- use JS runtime + nested tool bridge
- preserve direct tool mode alongside it

Pros:

- minimal protocol churn
- easier integration with current `openai_compat.rs`
- validates product value quickly

Cons:

- not true native freeform code mode
- model still emits JSON-wrapped code

### Option B: Native Freeform First

- extend provider/client protocol for custom tools
- expose freeform `exec`
- add custom-tool parsing and serialization
- preserve direct tool mode alongside it

Pros:

- closest to Codex model interaction
- cleanest long-term shape

Cons:

- depends on provider support
- larger protocol changes up front

Recommended default:

- start with Option A unless provider support for native freeform tools is already confirmed
- use that to validate runtime, nested tool bridging, context representation, and UX
- then graduate to Option B if the provider path warrants it
- in either option, prove early that nested dispatch does not bypass existing AgentLoop guardrails

## 20. Final Recommendation

Rusty-Claw should adopt code mode, but as a provider-aware, additive orchestration layer rather than a direct port or a replacement for direct tools.

The most important implementation choices are:

1. treat direct tools and `exec` as complementary execution paths
2. decide in Phase 0 whether the first shipping form is native freeform or JSON fallback
3. extend the tool model to support freeform/custom tools when needed
4. build a dedicated `code_mode` service that reuses existing tool dispatch
5. make nested dispatch guarded, so it preserves trace and safety semantics from AgentLoop
6. keep JS isolated and tool-mediated
7. roll out in phases, with provider/runtime/context feasibility validated first

If we follow this plan, Rusty-Claw will gain the main advantage of Codex code mode, lower orchestration latency and richer within-turn execution, without sacrificing the safety and observability already built into the project.

## 21. Live-Cell Migration Plan (No Replay Fallback)

This section defines the next implementation step after the current replay-style `wait` / yield milestone.

The chosen target architecture is:

- one live runtime worker per active code-mode cell
- `wait` attaches to the same live cell and drains incremental events
- no replay-based resume path
- no attempt to reconstruct JS state after a worker exits

If a live cell crashes, is cancelled, or otherwise terminates unexpectedly, it is considered failed and cannot be resumed.

### 21.1 Target Semantics

The target user-visible semantics should be:

- each `exec` creates a fresh `cell_id` and a dedicated live runtime worker
- at most one live code-mode cell exists per session in the initial implementation
- different sessions may execute concurrently, but the same session remains single-owner and its top-level `step()` executions are serialized through `SessionManager`
- `wait` validates the optional `cell_id`, connects to the same live cell, and returns only new output / status since the last drain
- runtime events are delivered monotonically by per-cell sequence number, and `wait` drains only not-yet-delivered items
- `yield_control(value)` emits a host-visible yield event but does not re-run the script on the next `wait`
- local JS variables, closures, and pending promises remain valid only while that live worker remains alive
- `store/load` remain the only explicit session-scoped persistence boundary across cells
- `reply_to` and concrete `AgentOutput` sinks are invocation-scoped routing data and are not part of the durable live-cell state
- `setTimeout()` and `clearTimeout()` remain JS runtime features and must not be reset by `wait`
- a separate host-side slice timer may yield control back to the model, and that host timer may be refreshed on `wait`
- if a cell becomes terminal after the last drain, `wait` returns the sticky terminal snapshot and clears the active cell handle
- once a cell reaches `completed`, `failed`, or `cancelled`, later `wait` calls must return a terminal response instead of attempting recovery

This architecture intentionally does not include a replay fallback.

### 21.2 Non-Goals for This Migration

To keep the migration bounded, the first live-cell implementation should not attempt:

- multiple concurrent live cells in one session
- heap snapshots or VM serialization
- replay-based recovery after runtime failure
- cross-process persistence of an in-flight JS runtime
- background execution APIs outside tool-mediated host services

### 21.3 Primary Refactor Workstreams

#### 21.3.1 Extract Nested Tool Execution from `AgentLoop`

The highest-priority refactor is to separate guarded nested tool execution from the current `AgentLoop` re-entry path.

Deliverables:

- add a reusable code-mode nested tool executor abstraction
- move tool lookup, extension-enriched `ToolContext` construction, trace/span linkage, autopilot checks, timeout handling, and cancellation handling into that abstraction
- remove the current `unsafe` pointer-based callback path from `dispatch_exec_tool_call`

This should become the stable boundary used by the live runtime worker whenever JS calls `await tools.some_tool(...)`.

#### 21.3.2 Replace Replay State with Live Cell Handles

`CodeModeService` should stop storing replay metadata such as:

- recorded nested tool calls
- suppressed output counters
- skipped yield counters
- replay-oriented timer reconstruction state

Instead, service state should converge on:

- `next_cell_seq`
- session-scoped `stored_values`
- `active_cell: Option<ActiveCellHandle>`

`ActiveCellHandle` should contain at least:

- `cell_id`
- broker command channel
- background driver handle
- buffered event log or drain snapshot handle
- visible tool metadata
- last-known status and drain cursor metadata
- sticky terminal snapshot metadata

#### 21.3.3 Move Runtime Semantics to Event-Driven Yielding

The runtime should stop modeling `yield_control()` as a thrown exception that terminates the current JS execution attempt.

Instead, the runtime should emit structured events such as:

- `Text`
- `Notification`
- `Yield`
- `ToolCallRequest`
- `ToolCallResolved`
- `Completed`
- `Failed`
- `Cancelled`

The runtime host should fulfill nested tool calls through `UnifiedToolExecutor`.
The service consumes the resulting request/done/progress events and exposes incremental drains to `exec` / `wait` callers.

#### 21.3.4 Separate Host Slice Timing from JS Timer Semantics

Two timer concepts must remain distinct:

- JS timers
  - `setTimeout()` / `clearTimeout()` inside the runtime
  - preserve script semantics
  - are not reset just because `wait` is called
- host slice timer
  - determines when control is yielded back to the model
  - may be refreshed by `wait`
  - should not mutate JS callback deadlines

This split is required to avoid conflating runtime semantics with polling semantics.

### 21.4 Proposed Module and Type Changes

The migration should keep the existing top-level entry points, but refactor internals around a live worker model.

Each active cell should be managed by a background `CellDriver` or equivalent broker task that owns the JS runtime, nested tool bridge, and undrained event buffer between `exec` / `wait` calls.

Files that should be changed deliberately:

- `src/core/step_helpers.rs`
  - replace the current code-mode nested callback bridge with an executor object or closure that does not require `unsafe` re-entry into `AgentLoop`
  - keep the top-level `dispatch_exec_tool_call` contract stable for the rest of the loop
- `src/code_mode/service.rs`
  - replace `PendingCellState` replay metadata with `ActiveCellHandle`
  - manage worker lifecycle, drain behavior, and terminal cleanup
- `src/code_mode/runtime/mod.rs`
  - keep one live runtime active until completion, failure, or cancellation
  - emit events instead of encoding yields through exceptions that force re-entry
- `src/code_mode/runtime/callbacks.rs`
  - define the runtime-to-host event types and nested tool request / response structures
- `src/code_mode/runtime/timers.rs`
  - keep JS callback timing logic only
  - remove replay reconstruction helpers once the migration is complete
- `src/code_mode/response.rs`
  - adapt response rendering to live statuses such as `running`, `completed`, `failed`, and `cancelled`
- `src/session/factory.rs`
  - keep `exec` / `wait` visibility rules paired for restricted sessions and subagents
- `src/context/sanitize.rs`
  - keep envelope-aware stripping compatible with longer-lived live-cell outputs
- `src/context/history.rs`
  - ensure compaction still summarizes large `exec` outputs safely
- `src/telegram/output.rs`
  - verify live-cell output remains readable on chat surfaces

New helper modules are acceptable if they simplify the service boundary, for example:

- `src/code_mode/driver.rs`
- `src/code_mode/executor.rs`
- `src/code_mode/runtime/events.rs`

### 21.4.1 Session Ownership and Concurrency Contract

The live-cell architecture is session-scoped, not frontend-scoped.

Rules:

- `SessionManager` remains the only supported owner for top-level `AgentLoop` instances
- exactly one `Arc<AsyncMutex<AgentLoop>>` may exist per `session_id`
- different `session_id` values may execute concurrently
- the same `session_id` must serialize top-level `step()` execution behind its session mutex
- a live code-mode cell belongs to the session's `CodeModeService`, never to a specific frontend adapter such as CLI, Telegram, ACP, scheduler, or Discord
- the initial live-cell implementation supports exactly one active or terminal-undrained cell per session
- frontends may choose their own busy-message behavior if they cannot obtain the session lock quickly, but they must not create a second independent loop for the same session

Consequences:

- `exec` must fail fast if the session already has a nonterminal or terminal-undrained cell
- `wait` is the only supported way to continue or collect that existing cell
- same-session concurrency is a product-level busy state, not a second runtime

### 21.4.2 Invocation Context Contract

The design must separate durable session state from per-call routing state.

Session-scoped state:

- transcript and history
- task state
- `CodeModeService`
- session-scoped `stored_values`
- active cell handle
- session cancellation primitives
- long-lived tool inventory

Invocation-scoped state:

- `reply_to`
- concrete `AgentOutput`
- current top-level trace parent / span linkage
- per-call waiting hints such as a preferred drain timeout or slice refresh value

Rules:

- nested code-mode `ToolContext` values must be built from the current invocation context plus durable session state at dispatch time
- a live cell must not hold a long-lived pointer to a concrete `AgentOutput`; it appends events to its buffered drain state, and the current `exec` or `wait` caller renders those events
- if the same session is resumed from a different frontend, the new invocation context becomes authoritative for that call only
- implementation must stop relying on constructor-time `reply_to` alone for nested code-mode dispatch; the current invocation context must be available when nested tools execute

### 21.4.3 Runtime-Service Protocol

Each active cell should be represented by a background `CellDriver` task.

The driver owns:

- the JS runtime
- JS timer state
- the nested tool bridge
- an ordered, bounded event buffer
- the latest cell status snapshot

`CodeModeService` interacts with the driver through commands and drain results rather than directly babysitting the runtime inside a single `exec` or `wait` call.

Suggested command and event shapes:

```rust
enum CellCommand {
    Drain {
        wait_for_event: bool,
        wait_timeout_ms: Option<u64>,
        refresh_slice_ms: Option<u64>,
    },
    ToolResult {
        request_id: u64,
        outcome: Result<String, ToolErrorPayload>,
    },
    Cancel {
        reason: String,
    },
}

enum RuntimeEvent {
    Text {
        seq: u64,
        chunk: String,
    },
    Notification {
        seq: u64,
        message: String,
    },
    Yield {
        seq: u64,
        kind: YieldKind,
        value: Option<serde_json::Value>,
        resume_after_ms: Option<u64>,
    },
    ToolCallRequest {
        seq: u64,
        request_id: u64,
        tool_name: String,
        args_json: String,
    },
    ToolCallResolved {
        seq: u64,
        request_id: u64,
        outcome: Result<String, ToolErrorPayload>,
    },
    Completed {
        seq: u64,
        return_value: Option<serde_json::Value>,
    },
    Failed {
        seq: u64,
        error: String,
    },
    Cancelled {
        seq: u64,
        reason: String,
    },
}
```

Protocol invariants:

- `seq` is strictly monotonic within one cell
- exactly one terminal event exists per cell: `Completed`, `Failed`, or `Cancelled`
- no later user-visible events may be appended after a terminal event
- `ToolCallRequest` is an internal control event for the service / driver handshake and is not rendered directly to the model
- a matching `ToolCallResolved` must be delivered exactly once for each `ToolCallRequest`
- `yield_control()` appends a `Yield` event but does not imply that the driver pauses or exits
- the driver may continue running and may continue servicing nested tools and timers between `wait` calls
- the buffered event log must remain bounded; if undrained output exceeds the configured budget, the service must mark the drain result as truncated and append an explicit truncation marker instead of growing without limit

Drain semantics:

- `exec` is defined as spawn + initial drain
- `wait` sends `CellCommand::Drain` to the existing driver
- if `wait_for_event` is true, the driver may block up to `wait_timeout_ms` while waiting for at least one new user-visible event or a terminal transition
- if no new event arrives before the timeout and the cell is still nonterminal, the drain result may be empty but must still return the current status snapshot
- `refresh_slice_ms` affects only the host slice timer and must not mutate JS timer deadlines
- once a terminal event has been drained, the service clears the active cell handle

### 21.4.4 Lifecycle and State Machine

Recommended cell status model:

```rust
enum CellStatus {
    Starting,
    Running,
    WaitingOnTool { request_id: u64 },
    WaitingOnJsTimer { next_due_in_ms: Option<u64> },
    Completed,
    Failed,
    Cancelled,
}
```

State rules:

- `Yield` is an event, not a terminal state
- `WaitingOnTool` and `WaitingOnJsTimer` are nonterminal states
- `Completed`, `Failed`, and `Cancelled` are terminal states
- terminal states are sticky until the final drain is collected and the active cell handle is cleared

Operation matrix:

- no active cell + `exec`
  - spawn a new driver, begin initial drain, and register the active cell
- no active cell + `wait`
  - return a structured error indicating that no code-mode cell is active for the session
- nonterminal active cell + `exec`
  - reject the call and instruct the caller to `wait` or cancel the active cell first
- nonterminal active cell + `wait` with no `cell_id`
  - drain the current active cell
- nonterminal active cell + `wait` with a matching `cell_id`
  - drain the specified active cell
- nonterminal active cell + `wait` with a mismatched `cell_id`
  - return a structured mismatch error
- terminal-undrained active cell + `wait`
  - return the sticky terminal snapshot and clear the active cell handle after a successful drain
- terminal-undrained active cell + `exec`
  - reject the call and require the caller to collect the terminal result with `wait` or reset the session
- `cancel_session` while a live cell exists
  - cancel both the top-level run and the cell driver, then transition the cell to `Cancelled`
- session reset or `/new` while a live cell exists
  - cancel the driver, wait for shutdown, clear the active cell handle, and then proceed with ordinary session-reset behavior

### 21.5 Incremental Delivery Plan

#### Phase A: Lock the Live-Cell Contract

Deliverables:

- document the live-cell semantics in this design doc and prompt notices
- define the session ownership, invocation-context, runtime-protocol, and lifecycle contracts in this design doc
- define the new internal status and event types
- keep the external `exec` / `wait` tool schema unchanged

Exit criteria:

- the intended semantics are explicit
- there is no ambiguity about whether `wait` re-runs JS
- session ownership, busy-state behavior, and frontend handoff rules are explicit
- terminal state handling and drain semantics are explicit
- prompt instructions align with the new live-cell model

#### Phase B: Extract Guarded Nested Tool Execution

Deliverables:

- move nested code-mode tool execution into a reusable executor abstraction
- preserve trace parentage, sandbox context, autopilot denials, extension enrichment, and cancellation semantics
- remove `unsafe` callback bridging from the code-mode path

Exit criteria:

- nested tools still behave exactly once per live runtime request
- existing guardrails still apply
- trace output still reflects the exec -> nested tool parent-child relationship

#### Phase C: Introduce Live Cell Service Scaffolding

Deliverables:

- add a background `CellDriver` or equivalent broker task per active cell
- add worker command and event channels
- add `ActiveCellHandle`
- teach `CodeModeService::execute()` to spawn and register a live worker
- keep one active cell per session

Exit criteria:

- `execute()` creates a live cell instead of storing replay state
- service can track whether a session has an active cell
- terminal cleanup removes the active cell reliably

#### Phase D: Convert Runtime Yielding to Event Emission

Deliverables:

- replace throw-based `yield_control()` handling with event-driven yielding
- allow the runtime to continue across multiple `wait` calls without re-running the script
- ensure nested tool requests are fulfilled through the service-managed bridge

Exit criteria:

- a multi-yield script preserves local JS state between waits
- output is monotonic and not duplicated by replay
- runtime completion produces one terminal result

#### Phase E: Add Poll/Drain Semantics and Host Slice Timing

Deliverables:

- implement `wait` as a drain on an existing live cell
- add host-side slice timing and yield hints
- ensure JS timers and host slice timers remain independent

Exit criteria:

- `wait` never re-runs the original JS source
- calling `wait` may refresh host slice timing but must not change JS timer deadlines
- incremental output remains stable and comprehensible

#### Phase F: Remove Replay-Specific State and Tests

Deliverables:

- delete replay-oriented service/runtime code paths
- replace replay-oriented tests with live-cell lifecycle tests
- simplify service state, runtime helpers, and timer utilities

Exit criteria:

- there is no replay fallback path left in the live-cell implementation
- there are no replay counters or recorded nested-call state fields left in steady-state service state
- documentation and tests describe only the live-cell semantics

### 21.6 Suggested PR Breakdown

A practical PR sequence is:

1. executor extraction and no-`unsafe` nested dispatch cleanup
2. live-cell status and runtime event type introduction
3. `CodeModeService` live worker scaffolding and active-cell tracking
4. runtime event-driven `yield_control()` and live nested tool bridge
5. `wait` drain semantics, host slice timing, and response rendering
6. replay-state removal, test rewrites, and final doc/prompt cleanup

Each PR should preserve a runnable tree and keep `cargo test` green.

### 21.7 Testing Plan for the Migration

#### Unit Tests

- host slice timer refreshes on `wait`
- JS timers do not refresh on `wait`
- runtime events are serialized and drained in order
- terminal cell states reject further `wait`
- session lifecycle helpers reject illegal `exec` / `wait` combinations by state

#### Runtime Tests

- `yield_control()` yields without requiring a later JS replay
- local variables survive across multiple waits within one live cell
- `setTimeout()` callbacks fire according to original JS deadlines
- nested tool promise resolution continues working after one or more yields
- the driver can continue handling nested tool requests even while no `wait` call is active

#### Service Tests

- `exec` creates one active live cell per session
- `wait` returns only incremental output from the same cell
- cancelling a live cell tears down the worker and clears service state
- runtime failure clears the active cell and returns a terminal error
- restricted-session tool filtering still keeps `wait` paired with `exec`
- same-session concurrent calls are serialized or busy-fail predictably
- different sessions do not share active-cell state
- a frontend handoff updates invocation-scoped routing without corrupting session-scoped code-mode state

#### Integration Tests

- a multi-yield script produces monotonic output without duplication
- nested side-effecting tools still obey autopilot and sandbox protections
- trace spans preserve the hierarchy between iteration, `exec`, and nested tools
- top-level final visible text response still works after code-mode completion
- CLI and Telegram style sessions can run concurrently without sharing code-mode state

### 21.8 Acceptance Criteria

The migration is complete when all of the following are true:

- `wait` does not re-run JS source
- one live cell can yield multiple times while preserving local JS state
- nested tool dispatch no longer relies on `unsafe` re-entry into `AgentLoop`
- replay-specific service state has been removed
- user-visible output is incremental, bounded, and understandable
- trace, extension, sandbox, and autopilot semantics remain intact for nested calls
- failed or cancelled cells terminate cleanly with no hidden recovery path
- same-session execution is serialized while different sessions may still run concurrently
- invocation-scoped `reply_to` / output routing can change between turns without corrupting session-scoped code-mode state
- the background driver can continue servicing nested tools and timers between `wait` calls

At that point, the live-cell model becomes the sole supported implementation for code-mode `wait` / yield behavior.

## 22. Post-Replay Event-Driven Architecture (Phase 6)

This section documents the current architecture after the replay-removal refactor. It supersedes the transitional replay-based implementation described in Phase 5 and replaces the migration plan in Section 21 with the delivered result.

### 22.1 Design Summary

The code-mode runtime is now fully event-driven. Each `exec` call spawns a single live JS runtime worker that runs to completion (or flush) without ever being re-executed. All output, notifications, flush/timer-wait signals, tool requests, and terminal events flow through a typed `RuntimeEvent` channel (`tokio::sync::mpsc::unbounded_channel`). There is no replay fallback.

Key properties:

- **One live worker per cell.** The JS runtime runs once. Local variables, closures, and pending promises remain valid for the lifetime of the worker.
- **`flush()` is non-throwing.** It emits a `RuntimeEvent::Flush` event and allows the host drain loop to surface `await_user`; execution can later be resumed with `wait`.
- **`text()` output is streamed as events.** Each `text(value)` call emits a `RuntimeEvent::Text` event. Output accumulates in the event log, not in an in-runtime buffer.
- **No replay counters.** `ResumeState`, `OutputBuffer`, `NotificationBuffer`, `CellResumeState`, `CellResumeProgressDelta`, and all suppressed-call counters have been deleted.
- **Drain semantics are pure event consumption.** `exec` spawns + does an initial drain. `wait` drains the same live cell's event buffer. Neither re-runs JS.

### 22.2 Module Responsibilities

#### `src/code_mode/runtime/mod.rs` — JS Runtime

Owns the `rquickjs` execution. Provides `run_cell()` which:

1. Creates an `AsyncRuntime` + `AsyncContext`.
2. Registers global functions (`__text`, `__notify`, `__flush`, `__waiting_for_timer`, `__store`, `__load`, `__callTool`, `__setTimeout`, `__clearTimeout`, `__markTimeoutComplete`, `__timerStateJson`, `__dueTimersJson`, `__wait_for_resume`).
3. Wraps user code in an async IIFE via `build_wrapper_script()`.
4. Executes the wrapped script to completion (including timer callback draining).
5. Returns `(ExecRunResult, HashMap<String, StoredValue>)`.

All side-effect globals communicate through:

- `event_tx: UnboundedSender<RuntimeEvent>` — for streaming `Text`, `Notification`, `Yield`, and `ToolCallRequested` events to the driver/service layer.
- `next_seq: Arc<AtomicUsize>` — monotonic sequence counter shared across all event-emitting globals within one cell.
- `CellRuntimeHost` — runtime-facing host boundary for visible tool names, event emission, cancellation, and async nested tool calls.

##### Deleted from this module:

- `OutputBuffer` / `NotificationBuffer` — output is no longer buffered in-runtime; each `text()` call sends an event immediately.
- `ResumeState` fields (`replayed_tool_calls`, `recorded_timer_calls`, `skipped_yields`, `suppressed_text_calls`, `suppressed_notification_calls`) — no replay means no suppression counters.
- `RunCellMetadata` fields (`total_text_calls`, `total_notification_calls`, `newly_recorded_tool_calls`) — metadata tracking for replay advancement is gone.

##### `ResumeState` and `RunCellMetadata`

Both structs still exist but are empty (`#[derive(Default)]`). They are retained as zero-cost placeholders to avoid churn in function signatures during the transition. They may be fully removed in a future cleanup pass.

#### `src/code_mode/protocol.rs` — Event and Command Types

Defines the typed channel protocol:

```rust
enum RuntimeEvent {
    Text { seq, text },
    Notification { seq, message },
    Yield { seq, kind, value, resume_after_ms },
    ToolCallRequested(ToolCallRequestEvent),
    ToolCallResolved { seq, request_id, ok },
    Completed { seq, return_value },
    Failed { seq, error },
    Cancelled { seq, reason },
    WorkerCompleted(Result<RuntimeCellResult, String>),
    TimerRegistrationChanged { seq, timer_calls },
}

enum CellCommand {
    ToolResult { request_id: String, outcome },
    Drain(DrainRequest),
    Cancel { reason },
}
```

Key changes from the replay era:

- `RuntimeEvent` now derives `Clone` directly (no manual impl).
- `Text` field is named `text` (was `chunk`).
- `request_id` is `String` (was `u64`), formatted as `"{tool_name}-{seq}"`.
- `RuntimeCellResult` is a 2-tuple `(ExecRunResult, HashMap<String, StoredValue>)` — the third `RunCellMetadata` element is removed.
- `ToolCallRequestEvent` (renamed from `ToolCallRequest`) carries `request_id: String`.

#### `src/code_mode/driver.rs` — CellDriver (Background Broker)

Manages the background worker task and provides drain semantics.

```
CellDriver::spawn() / spawn_live()
  → tokio::spawn worker thread
    → runtime::run_cell(code, host)
      → JS runs: text(), yield_control(), tools.X(), etc.
      → each JS global → host.emit_event(RuntimeEvent::*)
      → tools.X() → CellRuntimeHost.call_tool()
        → UnifiedToolExecutor.execute()
      → script completes → WorkerCompleted event
  → driver.next_update()
    → consumes events from event_rx
    → returns DriverUpdate { batch, boundary }
```

Deleted from this module:

- `WorkerRuntimeState` — the worker no longer needs a shared state struct; it uses captured locals.
- stale tool-result relay channels — nested tool completion is now returned through the host Promise path.
- `resume_progress: Arc<Mutex<CellResumeProgressDelta>>` — no replay progress tracking.
- `DriverDrainBatch.resume_progress` field — removed.

#### `src/code_mode/cell.rs` — ActiveCellHandle and Snapshots

Tracks the per-session active cell state:

```rust
struct ActiveCellHandle {
    cell_id: String,
    status: CellStatus,
    events: Vec<RuntimeEvent>,       // full event log
    last_summary: Option<ExecRunResult>,
}
```

Deleted from this module:

- `CellResumeState` — replay-oriented state (replayed tool calls, suppressed counters, skipped yields).
- `CellResumeProgressDelta` — incremental replay progress for advancing resume state between drains.
- All `advance_with_yield`, `advance_with_progress`, `runtime_resume_state` methods.
- `code`, `visible_tools` fields from `ActiveCellHandle` — no longer needed since the worker is never re-spawned.

Retained:

- `CellStatus` enum (now uses `request_id: String` instead of `u64` for `WaitingOnTool`).
- `CellDrainSnapshot` for rendering output to the LLM.
- `recent_visible_events()` for bounded event window rendering.

#### `src/code_mode/service.rs` — CodeModeService

Session-scoped service that owns cell lifecycle.

```rust
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, StoredValue>,
    active_cell: Option<ActiveCellHandle>,
    live_driver: Option<SharedCellDriver>,
}
```

Provides `execute_live()` which:

1. Allocates a `cell_id`.
2. Spawns a `CellDriver::spawn_live()`.
3. Performs an initial drain via `driver.drain_event_batch_with_request()`.
4. Registers `ActiveCellHandle` and `live_driver` in `SessionState`.
5. Returns a `CellDrainSnapshot`.

Deleted from this module:

- `PendingDrainBatch`, `PendingDrainResolution` — intermediate drain resolution helpers for replay-based progress tracking.
- `RuntimeBatchInvocation::for_pending_cell()` — replay-oriented cell re-spawn helper.
- `resume_state` field from `RuntimeBatchInvocation`.
- The original `execute()` method — replaced by `execute_live()`.
- All replay-specific `wait` / `wait_with_request` method bodies — replaced by live drain.

#### `src/code_mode/executor.rs` — Guarded Nested Tool Executor

Standalone executor for nested tool calls from JS. Preserves all AgentLoop guardrails:

- Autopilot / TODOS gating
- Repeated-action / reflection-strike loop protection
- Trace span parentage propagation
- Extension-based `ToolContext` enrichment
- Timeout and cancellation handling

This module is unchanged by the replay removal. It was already extracted as a clean boundary during Phase B of the live-cell migration.

#### `src/code_mode/response.rs` — Output Rendering

`DrainRenderState::from_events()` builds a render state from the event stream:

- Accumulates `Text` events into `output_text`.
- Collects `Notification` events.
- Captures terminal state from `Yield`, `Completed`, `Failed`, `Cancelled` events.

The `render_output()` / `render_output_with_status()` methods produce the final LLM-visible string.

Key change: `Text` field matching now uses `text` instead of `chunk`.

### 22.3 `flush()` / Timer Wait Semantics

The `flush(value?)` and timer wait bridge work as follows:

1. JS calls `flush(value)` to publish a host-visible checkpoint.
2. The wrapper script calls `__flush(JSON.stringify(value))`.
3. `__flush` sends `RuntimeEvent::Flush { value, ... }` via `event_tx`.
4. For pending timers, the wrapper script emits `RuntimeEvent::WaitingForTimer { resume_after_ms, ... }` via `__waiting_for_timer(...)` and blocks on `__wait_for_resume()`.
5. `wait` sends `CellCommand::Drain(...)`, unblocking the worker and letting it continue from the same runtime state.

This means:

- `flush()` emits a host-visible drain event without throwing a JS exception.
- Timer waits preserve runtime-local state and resume only through `wait`.
- The LLM receives accumulated text/notifications plus flush status in each drain batch.

### 22.4 Data Flow: `exec` Call

```
LLM emits exec({code})
  → AgentLoop.dispatch_exec_tool_call()
    → CodeModeService.execute()
      → CellDriver::spawn_live()
        → tokio::spawn worker thread
          → runtime::run_cell(code, CellRuntimeHost)
            → JS runs: text(), yield_control(), tools.X(), etc.
            → each JS global → CellRuntimeHost.emit_event(RuntimeEvent::*)
            → tools.X() → CellRuntimeHost.call_tool()
              → UnifiedToolExecutor.execute()
            → script completes → WorkerCompleted event
      → driver.next_update(initial timeout)
        → consumes events from event_rx
        → returns DriverUpdate
    → ActiveCellHandle.record_driver_update()
    → snapshot.render_state().render_output()
  → StructuredToolOutput envelope → LLM context
```

### 22.5 Data Flow: `wait` Call

```
LLM emits wait({cell_id?, wait_timeout_ms?, refresh_slice_ms?})
  → AgentLoop.dispatch_exec_tool_call()
    → CodeModeService.wait_with_request()
      → validates cell_id against active_cell
      → acquires live_driver lock
      → driver.next_update(wait timeout)
        → if wait_for_event: blocks up to wait_timeout_ms
        → if auto/explicit flush boundary: returns progress
        → on terminal: returns DriverUpdate with terminal_result
      → ActiveCellHandle.record_driver_update()
      → snapshot.render_state().render_output()
    → cleared active_cell if terminal
  → StructuredToolOutput envelope → LLM context
```

### 22.6 Timer Semantics (Unchanged)

JS timers (`setTimeout` / `clearTimeout`) remain runtime-internal:

- `setTimeout(callback, delayMs)` registers a timer via `register_timeout()`.
- If `delayMs == 0`, the callback is pushed to `__dueTimeoutCallbacks` and executed inline after the main script body.
- If the timer is pending (future), it is not executed. The wrapper script detects pending timers and returns a `yielded: true, yieldKind: 'timer'` result.
- Timer state is communicated to the service via `RuntimeEvent::TimerRegistrationChanged`.
- Host slice timing (`refresh_slice_ms` in `DrainRequest`) is independent of JS timer deadlines.

- [x] `wait_with_request()` implementation wired to driver drain loop.
- [x] `step_helpers.rs` integration for `execute` and `wait`.
- [x] Cleanup of stale `ResumeState`, `RunCellMetadata`, and `ReplayState` structs.
- [x] Unified host/executor nested tool bridge replacing the intermediate channel bridge.
- [x] Updated test suite (17/17 passing) and clean clippy status.

## 23. Unified Tool Execution Bridge Implementation

The earlier channel-based tool bridge was an intermediate implementation. It has been superseded by the unified runtime host path:

1. **Request Flow**: When JS calls `tools.read_file()`, the runtime's async `__callTool` creates a `RuntimeToolRequest` and awaits `CellRuntimeHost::call_tool`.
2. **Host Dispatch**: `ExecutorCellRuntimeHost` emits `RuntimeEvent::ToolCallRequested`, parses JSON arguments, and calls `UnifiedToolExecutor.execute` with `ToolCallOrigin::CodeModeNested`.
3. **Policy Boundary**: `UnifiedToolExecutor` owns visibility, step budget, autopilot, guard state, timeout, cancellation, tracing, and `ToolContext` enrichment before invoking `Tool.execute`.
4. **Promise Completion**: The host emits `RuntimeEvent::ToolCallDone` exactly once, then resolves the JavaScript Promise with normalized tool output or a structured tool error.

`CodeModeService` no longer fulfills nested tools. It records runtime events, maintains cell snapshots, and publishes `exec` / `wait` summaries.

## 24. Next Steps

**Phase 3 (Integration & Event-Driven Migration) Completed**:
- [x] Map Code Mode flush/timer-wait events to tool output `await_user` effect.
- [x] Fix event propagation in `ActiveCellHandle::record_driver_update` for flush/timer states.
- [x] Fix `to_completion()` drain requests and blocking logic.
- [x] Implement rendered text output emission to `AgentOutput`.
- [x] Verify full lifecycle with integration tests.

Focused work for the next phase:

- [ ] **Production Hardening**:
    - [ ] Add explicit timeout monitoring at the `CodeModeService` level for long-running workers.
    - [ ] Implement graceful termination for orphaned workers.
    - [ ] Refine error reporting for nested tool failures.
- [ ] **Performance Optimization**:
    - [ ] Profile `rquickjs` context initialization overhead.
    - [ ] Optimize large output buffering to avoid excessive string copying.
- [ ] **Expanded Capabilities**:
    - [x] Multi-cell session testing (Verified state persistence across turns).
    - [ ] Parallel tool call support inside `exec` (concurrent nested dispatch).
