# Rusty-Claw Code Mode Design

## 1. Goal

Introduce a Codex-style "Code Mode" into `rust-claw-oc` so the model can orchestrate multiple tool calls inside a single LLM turn by emitting JavaScript source to a dedicated runtime, while still preserving ordinary direct tool calls as a first-class path.

The intended outcomes are:

- Reduce LLM <-> tool round trips on multi-step coding tasks.
- Allow loops, conditionals, retries, and local state inside one execution cell.
- Keep direct tool calls available for simple one-shot actions.
- Preserve Rusty-Claw's existing strengths:
  - explicit tool protocol
  - transcripted context/history
  - `finish_task` lifecycle
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
  - for example: use a direct tool to inspect state, then `exec` for batch work, then a direct `finish_task`

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
- immediate `finish_task`

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
- relay nested tool calls to host dispatch
- support terminate/cancel

Non-responsibilities:

- `CodeModeService` should not become the owner of AgentLoop guardrail state
- it should delegate guarded nested dispatch back to an AgentLoop-owned host implementation

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

This is close to `codex-rs` and maps cleanly onto current `dispatch_tool_call()`.

`invoke_tool(...)` should be implemented as guarded nested dispatch, not as a thin wrapper around `tool.execute(...)`.

That means it should apply the relevant top-level safety rules before and around nested execution, instead of letting JS directly bypass them.

At minimum, the guarded nested dispatch path must preserve:

- autopilot restrictions around side-effecting tools
- repeated-action / reflection-strike style loop protection
- timeout and cancellation behavior
- trace/span parentage using `parent_span_id`

Recommended ownership split:

- runtime/service layer:
  - cell lifecycle
  - pending tool promises
  - wait/terminate behavior
- AgentLoop host adapter:
  - guarded nested dispatch
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
6. continue until `finish_task`

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

- nested `finish_task` is not allowed in early phases
- `finish_task` remains a top-level lifecycle tool
- if `exec` determines the task is complete, it should return that conclusion to the model, and the model may then call top-level `finish_task`

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

- `finish_task_summary`
- `await_user`
- file/evidence side effects

Early-phase code mode should not attempt to support the full effect surface area for nested tools.

Recommended boundary:

- nested `finish_task` is disallowed
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

- `finish_task`
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

- Add `WaitTool`
- add `yield_control()`
- add timed yielding and resumable cells
- support session-scoped stored values

Exit criteria:

- long-running `exec` can yield and resume safely

## Phase 4: Hardening

- add timeout helpers
- add output truncation/token budget controls
- add trace/event-log visibility
- add allow-list / policy guards
- expand tests

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
- top-level `finish_task` still ends run after code mode output indicates completion
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
