# Code Mode Unified Tool Execution Design

## Status

This is the target refactor after the current channel-based code-mode tool bridge.
It is intended to guide implementation, review, and verification.

## Implementation Progress

- [x] Phase 0: Characterization tests added and baseline verified.
- [x] Phase 1: Extract unified executor without behavior change.
- [ ] Phase 2: Introduce `CellRuntimeHost` boundary.
- [ ] Phase 3: Replace sync tool result bridge with Promise/completion queue.
- [ ] Phase 4: Simplify service and driver state.
- [ ] Phase 5: Documentation and trace cleanup.

The main decision is:

- Actual tool execution must be a direct Rust async call path through one shared executor.
- Channels or queues may exist at the QuickJS thread boundary and for event publication.
- `CodeModeService` must not be the nested tool execution relay in the final design.
- `src/code_mode/runtime/*` must not own or inspect real `Arc<dyn Tool>` values.

## Goals

1. Make top-level model tool calls and code-mode nested tool calls share one execution pipeline.
2. Keep tool policy centralized: visible tools, step budget, autopilot, execution guard, timeout, cancellation, trace, and context enrichment.
3. Keep code-mode runtime focused on JavaScript execution and host capability calls.
4. Keep `CodeModeService` focused on session and cell lifecycle: active cell, stored values, wait/flush publication, terminal cleanup, and abort.
5. Remove the service-owned nested tool request/result relay from the steady-state architecture.

## Non-Goals

- Do not give JavaScript direct filesystem, network, shell, or tool registry access.
- Do not create a second tool system parallel to the existing `Tool` trait.
- Do not move user-output rendering or top-level turn effects into the executor.
- Do not require every migration step to replace the QuickJS bridge immediately. The final architecture should, but early steps may preserve behavior behind compatibility wrappers.

## Current Architecture

Current nested code-mode calls flow like this:

```text
JS tools.foo(args)
 -> runtime __callTool sync callback
 -> RuntimeEvent::ToolCallRequested
 -> CodeModeService host loop
 -> CodeModeNestedToolExecutor
 -> ToolInvoker
 -> Tool.execute(...).await
 -> CellDriver::complete_pending_tool_call(...)
 -> std::sync::mpsc tool result channel
 -> JS resumes
```

This works, but it mixes responsibilities:

- `CodeModeService` owns both cell lifecycle and nested tool fulfillment.
- Tool execution policy is split between core top-level dispatch and `CodeModeNestedToolExecutor`.
- The request event path and result channel path are coupled by ordering.
- The QuickJS bridge shape leaks into service-level orchestration.

The important parts to preserve are:

- Nested calls still go through the existing `Tool` trait.
- Runtime events still describe progress for `wait`, trace, and summaries.
- JS still only gains power through nested tool calls, not native OS APIs.

## Target Architecture

Final call paths:

```text
Top-level model tool call
 -> UnifiedToolExecutor.execute(...)
 -> Tool.execute(...).await
```

```text
JS tools.foo(args)
 -> CellRuntimeHost.call_tool(...)
 -> UnifiedToolExecutor.execute(...)
 -> Tool.execute(...).await
 -> JS Promise resolves or rejects
```

Event publication remains separate:

```text
CellRuntimeHost emits RuntimeEvent
 -> CellDriver streams events
 -> CodeModeService records cell state
 -> wait/flush/auto_flush publish summaries
```

The executor is the only owner of actual tool invocation policy. The service observes runtime events; it does not fulfill nested tool calls.

## Ownership Model

| Component | Owns | Must Not Own |
| --- | --- | --- |
| `runtime` | QuickJS setup, JS globals, wrapper script, stored value functions, timer functions, Promise bridge | Real tool list, policy decisions, trace policy |
| `CellRuntimeHost` | Runtime-to-agent boundary, `ToolCallRequested` and `ToolCallDone` events, cancellation view, executor call | Session map, long-term cell publication policy |
| `UnifiedToolExecutor` | Tool lookup, visibility, step budget, autopilot, guard, timeout, cancellation, trace span, `ToolContext`, real `Tool.execute` | JS runtime details, service session map, UI rendering |
| `CodeModeService` | Active cell, stored values, snapshots, wait/flush/auto_flush, abort, terminal cleanup | Direct nested tool fulfillment |
| `AgentLoop` / core | LLM turn loop, output rendering, top-level tool result effects, context recording | Code-mode runtime internals |

## Unified Tool Executor

The existing `src/tools/invocation.rs` already contains most of this shape through `ToolInvoker`.
Prefer evolving it into the unified execution boundary instead of creating another parallel abstraction.

Suggested final types:

```rust
pub(crate) enum ToolCallOrigin {
    TopLevel {
        call_id: Option<String>,
    },
    CodeModeNested {
        cell_id: String,
        outer_tool_call_id: Option<String>,
        request_id: String,
        seq: u64,
    },
}

pub(crate) struct ToolExecutionRequest {
    pub(crate) tool_name: String,
    pub(crate) args: serde_json::Value,
    pub(crate) origin: ToolCallOrigin,
    pub(crate) timeout: std::time::Duration,
    pub(crate) trace_ctx: Option<crate::trace::TraceContext>,
    pub(crate) parent_span_id: Option<String>,
    pub(crate) span: Option<ToolInvocationSpanConfig>,
}

pub(crate) struct ToolExecutionOutcome {
    pub(crate) result: String,
    pub(crate) is_error: bool,
    pub(crate) stopped: bool,
}
```

Suggested executor shape:

```rust
pub(crate) struct UnifiedToolExecutor {
    current_tools: Vec<std::sync::Arc<dyn crate::tools::Tool>>,
    visible_tools: Vec<String>,
    extensions: Vec<std::sync::Arc<dyn crate::core::extensions::ExecutionExtension>>,
    session_id: String,
    reply_to: String,
    step_budget: StepBudgetHandle,
    session_deadline: Option<std::time::Instant>,
    trace_bus: std::sync::Arc<crate::trace::TraceBus>,
    cancel_token: std::sync::Arc<tokio::sync::Notify>,
    is_autopilot: bool,
    todos_path: std::path::PathBuf,
    execution_guard_state: std::sync::Arc<std::sync::Mutex<crate::core::ExecutionGuardState>>,
}
```

`StepBudgetHandle` should eventually replace independent `usize` snapshots:

```rust
#[derive(Clone)]
pub(crate) struct StepBudgetHandle(std::sync::Arc<std::sync::Mutex<StepBudgetState>>);

pub(crate) struct StepBudgetState {
    remaining_steps: usize,
}
```

Required behavior:

- `execute` rejects hidden tools before invoking them.
- `execute` consumes one step for side-effectful and read-only calls alike, matching the current nested call limit behavior.
- `execute` applies autopilot denial before consuming the step.
- `execute` invokes exactly one `Tool.execute(args, &ToolContext).await` on success path.
- `execute` applies timeout and cancellation around the real tool future.
- `execute` records trace spans using origin-specific names and attributes.
- `execute` enriches `ToolContext` through all `ExecutionExtension`s before calling the tool.
- `execute` returns a raw tool output string; callers decide how to render or record it.

Top-level output handling remains in core:

- `AgentOutput::on_tool_start`
- `AgentOutput::on_tool_end`
- `AgentOutput::on_error`
- `handle_successful_tool_effects`
- context function response recording

Nested code-mode output handling remains in code-mode event summaries and JS return values.

## CellRuntimeHost

Add a host boundary under `src/code_mode/host.rs` or an equivalent module.

Suggested request type:

```rust
pub(crate) struct RuntimeToolRequest {
    pub(crate) cell_id: String,
    pub(crate) seq: u64,
    pub(crate) request_id: String,
    pub(crate) tool_name: String,
    pub(crate) args_json: String,
    pub(crate) outer_tool_call_id: Option<String>,
}
```

Suggested host trait:

```rust
#[async_trait::async_trait]
pub(crate) trait CellRuntimeHost: Send + Sync {
    fn visible_tool_names(&self) -> Vec<String>;

    fn emit_event(&self, event: crate::code_mode::protocol::RuntimeEvent);

    fn cancellation_reason(&self) -> Option<String>;

    async fn call_tool(
        &self,
        request: RuntimeToolRequest,
    ) -> Result<String, crate::tools::ToolError>;
}
```

The production implementation should:

1. Parse `args_json` into `serde_json::Value`.
2. Emit `RuntimeEvent::ToolCallRequested` before invoking the executor.
3. Call `UnifiedToolExecutor.execute` with `ToolCallOrigin::CodeModeNested`.
4. Normalize successful tool output for JS with `normalize_tool_result_for_js`.
5. Emit `RuntimeEvent::ToolCallDone` exactly once.
6. Return the normalized output or a `ToolError`.

`CellRuntimeHost` may hold the executor behind `tokio::sync::Mutex` if budget and guard mutation require sequential nested dispatch. Parallel nested tool calls should be a deliberate later change, not an accidental side effect of this refactor.

## QuickJS Bridge

The final bridge should make JS tool calls Promise-based:

```javascript
const result = await tools.read_file({ path });
```

Target flow:

```text
JS tools.foo()
 -> __callTool creates a JS Promise
 -> host.call_tool(...) runs as a Rust async future
 -> completion is posted back to the QuickJS thread
 -> Promise is resolved or rejected inside the QuickJS context
```

The exact rquickjs implementation should be chosen after a small spike:

1. Prefer native rquickjs async function or Promise support if it can safely call a Rust future and resolve on the QuickJS context.
2. Otherwise use a runtime-local completion queue:
   - `__callTool` creates a pending promise and request id.
   - A tokio task awaits `CellRuntimeHost::call_tool`.
   - The result is sent to a completion queue.
   - The QuickJS runtime loop drains the queue and resolves or rejects promises on the owning context thread.

Final acceptance criteria for the bridge:

- No nested `Handle::block_on` inside a QuickJS callback.
- No service-owned tool result channel for nested calls.
- No `CellDriver::complete_pending_tool_call` in the final path.
- Tool events are visible to `CodeModeService` while the tool future is running.
- Pending tool calls can be cancelled without corrupting a later tool result.

It is acceptable for an intermediate phase to keep a compatibility channel behind the host abstraction, but the service must not grow new dependency on that detail.

## CodeModeService Changes

In the final design, `CodeModeService::perform_cell_host_loop` should not invoke nested tools.

For `DriverBoundary::PendingTool`, it should only:

1. Record the event in the active cell.
2. Publish a running summary if needed.
3. Keep polling for future runtime events.

For `RuntimeEvent::ToolCallDone`, it should:

1. Record the completion in the active cell.
2. Publish if the progress policy requires it.
3. Continue until terminal, flush, auto-flush, or wait timeout.

Delete or retire these final-path concepts:

- `tool_result_tx`
- `tool_result_rx`
- `tool_call_in_flight`
- `CellDriver::complete_pending_tool_call`
- Service-side nested tool invocation in `DriverBoundary::PendingTool`

`abort_active_cell` should still:

- mark the active cell cancelled or absent in session state
- notify the runtime cancel flag
- send timer/cell cancel commands where needed
- rely on executor cancellation for pending tool futures

## Runtime Event Semantics

`ToolCallRequested` must mean:

- The JS cell asked for a nested tool.
- The request passed from runtime into host ownership.
- The tool may still be running.

`ToolCallDone` must mean:

- The host finished the executor call.
- The request has an `ok` or error outcome.
- The JS Promise has either been resolved or is about to be resolved.

Recommended event fields to preserve or add:

```rust
RuntimeEvent::ToolCallRequested(ToolCallRequestEvent)

RuntimeEvent::ToolCallDone {
    seq: u64,
    request_id: String,
    ok: bool,
}
```

If summaries need better error display, add optional `error_preview` later. Do not block the refactor on richer event payloads.

## Visibility Rules

Nested code-mode tools must exclude runtime and lifecycle tools:

- `exec`
- `wait`
- `finish_task`
- `ask_user_question`
- `subagent`
- `task_plan`
- `manage_schedule`
- `send_telegram_message`

Keep this rule in one place. A good final home is the unified executor policy or a small shared policy module. Avoid duplicating it across `entry.rs`, `executor.rs`, and session factory code.

## Migration Plan

### Phase 0: Characterization Tests

Before refactoring behavior, add or confirm tests for:

- `exec` with one nested successful tool call.
- `exec` with nested tool error surfaced as JS/runtime error.
- long nested tool call publishes `waiting_on_tool_request_id`.
- `flush` before a nested tool still returns the flush summary first.
- `wait` observes final completion after background nested tool work.
- cancellation while waiting on a nested tool does not poison the next cell.
- hidden nested tools are rejected.
- nested step budget exhaustion is rejected.
- autopilot denial still applies to nested side-effectful tools.

Suggested commands:

```bash
cargo test --test code_mode_integration
cargo test --test session_flow
cargo test core::tests
```

Progress 2026-04-22:

- Confirmed existing coverage for successful nested calls, nested errors, long nested calls publishing `waiting_on_tool_request_id`, flush-before-tool behavior, wait-after-background-work completion, cancellation cleanup, autopilot nested denial, and nested `ToolContext` enrichment.
- Added characterization coverage for hidden nested tool rejection before execution and nested step budget exhaustion after the first successful nested tool call.
- Finding: `cargo test code_mode_integration` filters by test name and runs zero integration tests in this repository. Use `cargo test --test code_mode_integration` and `cargo test --test session_flow` for the intended integration suites.

### Phase 1: Extract Unified Executor Without Behavior Change

Goal: make top-level and nested code-mode calls use the same execution object while preserving the current service/channel bridge.

Implementation tasks:

- Evolve `ToolInvoker` in `src/tools/invocation.rs` into `UnifiedToolExecutor`, or wrap it with that name.
- Move `CodeModeNestedToolExecutor` checks into executor request policy where practical.
- Keep `CodeModeNestedToolExecutor` temporarily as a thin adapter if this reduces diff size.
- Update core top-level dispatch to call the unified executor for non-code-mode tools.
- Update code-mode nested dispatch to call the same executor through the adapter.
- Preserve current trace event names unless tests are intentionally updated.

Exit criteria:

- There is one code path that calls `Tool.execute`.
- Existing behavior tests pass.
- No QuickJS bridge changes are required in this phase.

Suggested commands:

```bash
cargo test --test code_mode_integration
cargo test --test session_flow
cargo test core::tests
cargo clippy -- -D warnings
```

Progress 2026-04-22:

- Replaced `ToolInvoker` with `UnifiedToolExecutor` and added `ToolCallOrigin`, `ToolExecutionRequest`, `ToolExecutionOutcome`, and `StepBudgetHandle`.
- Routed normal top-level tool execution and code-mode nested tool execution through `UnifiedToolExecutor.execute`, keeping code-mode `exec`/`wait` lifecycle rendering in core/service.
- Centralized code-mode nested visibility exclusions in `src/tools/policy.rs`.
- Preserved current service/channel bridge for this phase; `CodeModeNestedToolExecutor` remains as a thin adapter over the unified executor.
- Finding: top-level `exec`/`wait` are lifecycle entry points rather than direct `Tool.execute` calls, so they still dispatch through code-mode service while sharing executor-owned guard state.
- Finding: `cargo clippy -- -D warnings` also surfaced existing code-mode style debt (`ExecLifecycle` manual default and two high-arity helpers); these were cleaned up or locally allowed before the phase was committed.

### Phase 2: Introduce CellRuntimeHost Boundary

Goal: make runtime depend on a host capability instead of raw tool-result plumbing.

Implementation tasks:

- Add `src/code_mode/host.rs`.
- Move visible tool names and event emission responsibility behind `CellRuntimeHost`.
- Make `CellDriver::spawn_live` and `runtime::run_cell` accept host-facing abstractions rather than ad hoc `invoke_tool` closures.
- Keep any compatibility channel private to the host/driver layer.
- Do not let `CodeModeService` gain new nested tool execution responsibilities.

Exit criteria:

- Runtime code has a single conceptual API for host calls.
- Service code no longer knows how the runtime waits for tool results, even if the compatibility channel still exists internally.
- Tests still pass.

### Phase 3: Replace Sync Tool Result Bridge With Promise/Completion Queue

Goal: nested tool calls are fulfilled by `CellRuntimeHost -> UnifiedToolExecutor` directly, not by service relay.

Implementation tasks:

- Implement JS Promise creation for `__callTool`.
- Spawn or drive the host async tool future without nested `block_on`.
- Resolve or reject the Promise on the QuickJS context thread.
- Emit `ToolCallRequested` and `ToolCallDone` from the host.
- Remove service-side invocation from `DriverBoundary::PendingTool`.
- Remove `CellDriver::complete_pending_tool_call`.

Exit criteria:

- The actual nested tool invocation path is:

```text
CellRuntimeHost.call_tool
 -> UnifiedToolExecutor.execute
 -> Tool.execute
```

- `CodeModeService` only records and publishes runtime events.
- Cancellation tests pass.
- Long nested tool calls still publish running summaries.

### Phase 4: Simplify Service and Driver State

Goal: remove obsolete relay state and make cell lifecycle easier to reason about.

Implementation tasks:

- Delete `tool_result_tx`, `tool_result_rx`, and `tool_call_in_flight`.
- Simplify `CellDriverControl::request_cancel`.
- Revisit `DriverBoundary::PendingTool`: it may remain as a publication boundary, but it must not imply service-side fulfillment.
- Ensure `Drop for CellDriver` still terminates infinite loops, timer waits, and pending tool work.
- Tighten event ordering tests.

Exit criteria:

- Driver state describes runtime execution, not tool result routing.
- Service state describes cell snapshots and publications, not tool dispatch.

### Phase 5: Documentation and Trace Cleanup

Goal: make the new ownership visible to maintainers and traces.

Implementation tasks:

- Update `docs/CODE_MODE_DESIGN.md` to mark the channel bridge as superseded by this design.
- Update trace event names only if needed. Prefer preserving existing event names for dashboard compatibility.
- Add comments at the runtime/host/executor boundaries explaining ownership.
- Remove stale comments describing service-owned nested tool fulfillment.

Exit criteria:

- The architecture is reflected in docs and code comments.
- Trace output still identifies top-level tool calls and nested code-mode tool calls separately.

## Verification Matrix

| Area | Required Verification |
| --- | --- |
| Basic code mode | `cargo test code_mode_integration` |
| Session behavior | `cargo test session_flow` |
| Core dispatch | `cargo test core::tests` |
| Service unit tests | `cargo test code_mode::service` |
| Driver/runtime unit tests | `cargo test code_mode::driver` and `cargo test code_mode::runtime` if present |
| Lints | `cargo clippy -- -D warnings` |
| Formatting | `cargo fmt --check` |

Add targeted tests as implementation proceeds. Prefer small tests around the boundary being changed rather than relying only on integration tests.

## Invariants

These must be true in the final design:

- `runtime` never owns `Arc<dyn Tool>`.
- All real tool execution goes through `UnifiedToolExecutor`.
- `CodeModeService` never calls a nested tool in response to `ToolCallRequested`.
- Runtime events remain sufficient for `wait` summaries.
- A hidden nested tool returns an error before `Tool.execute` is called.
- A cancelled cell cannot deliver a stale tool result into a later tool call.
- Top-level output rendering remains outside the executor.
- Nested tool calls still receive enriched `ToolContext`.
- Existing structured tool output envelopes remain valid.

## Open Questions

1. Should nested code-mode tool outputs trigger `after_tool_result` extension hooks?
   Current behavior appears to reserve top-level result effects for core dispatch. Keep this unchanged during the refactor unless a separate product decision says otherwise.

2. Should nested code-mode tools share one mutable step budget with the parent turn?
   The target design says yes through `StepBudgetHandle`. Phase 1 may keep snapshots to reduce risk, but final behavior should avoid budget drift.

3. Should nested code-mode calls run concurrently?
   The initial target should preserve sequential semantics. Parallel nested calls require explicit budget, trace, output ordering, and cancellation policy.

4. Should `ToolCallDone` include an error preview?
   Useful for summaries, but not required for the ownership refactor.

5. Can rquickjs native async functions fully replace the completion queue?
   Decide after a spike. The architecture only requires that `CellRuntimeHost.call_tool` reaches the executor directly and that Promise resolution happens on the QuickJS context thread.

## Review Checklist

Use this checklist for each implementation PR:

- Does this PR reduce duplicated tool policy?
- Does this PR preserve existing code-mode user-visible behavior?
- Does this PR keep runtime free of real tool registry knowledge?
- Does this PR move service away from nested tool fulfillment?
- Are cancellation and timeout paths tested?
- Are trace parent ids preserved for top-level and nested calls?
- Are hidden tools and budget exhaustion tested?
- Did `cargo test` cover the changed boundary?
