# Code Mode Background Execution Plan

## 1. Goal

Redesign code mode so that a JavaScript cell becomes a genuinely autonomous background execution unit after `exec` starts it.

The agreed runtime semantics are:

- `exec` starts a live code-mode cell and returns a summary of the current snapshot.
- `flush()` publishes intermediate output but does not pause JS execution.
- `wait` observes cell progress and may block for new visible events, but it does not drive execution forward.
- Timers remain host-provided rather than ECMAScript-native, but they must advance inside the code-mode runtime/host loop without requiring external `wait` calls.
- `cancel` remains the only external control that changes the execution flow of a running cell.

This document is an incremental redesign for the current implementation in:

- [`src/core.rs`](/Users/liuli/src/rust-claw-oc/src/core.rs)
- [`src/core/step_helpers.rs`](/Users/liuli/src/rust-claw-oc/src/core/step_helpers.rs)
- [`src/code_mode/service.rs`](/Users/liuli/src/rust-claw-oc/src/code_mode/service.rs)
- [`src/code_mode/driver.rs`](/Users/liuli/src/rust-claw-oc/src/code_mode/driver.rs)
- [`src/code_mode/executor.rs`](/Users/liuli/src/rust-claw-oc/src/code_mode/executor.rs)
- [`src/code_mode/runtime/mod.rs`](/Users/liuli/src/rust-claw-oc/src/code_mode/runtime/mod.rs)
- [`src/code_mode/runtime/timers.rs`](/Users/liuli/src/rust-claw-oc/src/code_mode/runtime/timers.rs)

## 1.1 Progress Log

- [x] Phase 1 started and completed:
  - `AgentLoop` extensions now use shared ownership via `Vec<Arc<dyn ExecutionExtension>>`.
  - direct tool execution and code-mode nested tool execution now share a single `ExecutionGuardState`.
  - `CodeModeNestedToolExecutor` no longer borrows extension slices or mutable guard references from the foreground `exec` / `wait` frame.
- [x] Phase 2 started and completed:
  - `CodeModeService` now spawns a background `CellHostTask` for each `exec`.
  - nested tool requests are fulfilled by the background host task instead of the foreground `wait` call path.
  - `wait` now observes background progress and only nudges execution when the cell is paused on a JS timer boundary.
- [x] Phase 5 started and completed:
  - Added a dedicated cell background span.
  - Ensured nested tool spans hang under the cell span.
  - Updated docs.
- [ ] Phase 3 not started: timers still require the current resume-based runtime contract.

Phase 1 implementation finding:

- The shared guard state is implemented with `Arc<std::sync::Mutex<ExecutionGuardState>>`, not `tokio::sync::Mutex`.
  This is intentional because the guard updates are short, synchronous, and do not cross `await` points in either the foreground or nested-tool paths.

Phase 2 implementation findings:

- To preserve deterministic `exec` behavior, the background host task publishes its first summary through a one-shot channel before `execute(...)` returns.
  This avoids a race where a fast background continuation could otherwise overwrite the initial flush snapshot with a later terminal snapshot.
- Phase 2 intentionally keeps timer resume semantics unchanged.
  `wait` no longer fulfills nested tools, but it still resumes cells that are blocked at the current timer boundary.

## 2. Why The Current Model Is Still Coupled To `wait`

The current implementation already has service-owned live workers, but it is not yet autonomous in the sense we want.

### 2.1 Nested tool calls are fulfilled only during `exec` or `wait`

`dispatch_exec_tool_call()` creates a temporary `CodeModeNestedToolExecutor`, wraps it in a local async closure, and passes that closure into `CodeModeService::execute(...)` or `wait_with_request(...)`.

As a result, nested JS tool calls are still logically attached to the foreground tool invocation that happens to be active at that moment.

Current coupling points:

- `CodeModeNestedToolExecutor` borrows `extensions`, `action_history`, and `reflection_strike` from the current `AgentLoop` call frame.
- `CodeModeService` requires an `invoke_tool` closure on every `exec` and `wait`.
- `CellDriver::drain_event_batch_with_request(...)` only fulfills `RuntimeEvent::ToolCallRequested(...)` while some foreground caller is actively draining.

### 2.2 Timer progress still depends on external host resumes

The runtime wrapper currently emits `WaitingForTimer`, then calls `__wait_for_resume()`, which blocks until the host sends `CellCommand::Drain`.

That means timers are not merely observed by `wait`; they are actively resumed by `wait`.

This is the key reason the current runtime is not truly autonomous.

### 2.3 The event drain path is acting as both observer and scheduler

Today the drain path has two distinct responsibilities:

- observe and summarize runtime events for the LLM
- schedule tool fulfillment and timer continuation

Those two responsibilities should be split. `wait` should keep only the first one.

## 3. Constraints And Non-Goals

### 3.1 Timers cannot be "pure JS engine closed loop"

In this codebase, code mode runs on `rquickjs`/QuickJS as an embedded JavaScript executor. `setTimeout` is not an ECMAScript language feature; it is a host capability.

Therefore, timers will still be host-owned after the redesign. The change is not "move timers into the JS engine". The change is:

- stop exposing timer progression as an external `wait`-driven resume protocol
- keep timer progression inside the code-mode worker/runtime lifecycle

### 3.2 We do not need VM snapshot serialization for this redesign

The current implementation already keeps a live worker alive after `exec`.

This redesign does not require serializing and restoring VM snapshots. It only changes who owns:

- nested tool fulfillment
- timer continuation
- progress publication

### 3.3 `flush()` must stay non-blocking

`flush()` should remain a visibility primitive only. It must not become a yield/resume boundary.

## 4. Proposed Architecture

### 4.1 Introduce a background `CellHostTask`

Each `exec` creates a dedicated background host task for that cell.

The `CellHostTask` becomes the sole owner of:

- the live `CellDriver`
- the nested tool executor used by JS tool calls
- the published cell snapshot and terminal result
- change notifications for observers such as `wait`

High-level flow:

1. `exec` constructs a spawn-time execution snapshot from the current `AgentLoop`.
2. `CodeModeService` creates a session-local `CellHostTask`.
3. The host task continuously drains runtime events in the background.
4. Nested tool requests are fulfilled directly by the host task.
5. Timer waits are handled inside the runtime/worker, not by foreground `wait`.
6. `wait` only reads the latest published snapshot and optionally waits for new visible events.

### 4.2 Freeze a spawn-time execution snapshot

Background execution must not hold `&mut AgentLoop` borrows.

At cell spawn time, build a `CodeModeExecutionSnapshot` with owned or shared dependencies:

- `current_tools: Vec<Arc<dyn Tool>>`
- `extensions: Vec<Arc<dyn ExecutionExtension>>`
- `session_id`
- `reply_to`
- `session_deadline`
- `trace_bus`
- `provider`
- `model`
- `cancel_token`
- autopilot flags
- `todos_path`
- a shared execution guard state

This snapshot is the background cell's contract with the foreground loop.

### 4.3 Replace borrowed guard state with shared guard state

Replace the current borrowed fields:

- `&mut VecDeque<String>`
- `&mut u8`

with a dedicated shared state object:

```rust
pub struct ExecutionGuardState {
    pub action_history: VecDeque<String>,
    pub reflection_strike: u8,
}
```

and inject it as:

```rust
Arc<tokio::sync::Mutex<ExecutionGuardState>>
```

This preserves consistent autopilot loop-protection semantics even if background code-mode execution overlaps with later foreground tool activity.

### 4.4 Move extensions to shared ownership

`AgentLoop` currently stores:

```rust
Vec<Box<dyn ExecutionExtension>>
```

For background execution, change this to shared ownership:

```rust
Vec<Arc<dyn ExecutionExtension>>
```

This lets background code-mode execution call `enrich_tool_context(...)` without borrowing the foreground loop.

### 4.5 Keep trace semantics explicit

Background nested tool calls should no longer appear as children of whichever foreground `wait` or `exec` invocation happened to observe them.

Instead:

- `exec` creates or records a dedicated cell background span such as `code_mode_cell_started`
- nested tool spans hang under that cell span
- `wait` creates its own observation spans, but it is not the parent of background nested tool work

This is more semantically correct than the current model.

## 5. Runtime Model Changes

### 5.1 Remove `Drain` as an execution prerequisite

`CellCommand::Drain` should stop being part of the timer execution model.

After this redesign:

- `CellCommand::Cancel` remains
- `CellCommand::Drain` is removed
- `wait` no longer resumes runtime execution

### 5.2 Replace `__wait_for_resume()` with internal timer sleeping

The current timer flow in the JS wrapper is:

1. detect pending timers
2. emit `WaitingForTimer`
3. call `__wait_for_resume()`
4. continue only after an external drain command

Replace this with:

1. detect pending timers
2. emit `WaitingForTimer`
3. call a host-provided blocking timer wait helper, for example `__sleep_for_timer(ms)`
4. continue automatically when the timer expires or stop on cancel

The helper does not need to live in the JS engine. It can simply block the worker thread while checking cancellation.

This keeps timer ownership in the host, but closes the loop inside the cell worker.

### 5.3 Keep visible timer events for observers

Even though timers no longer require `wait`, the runtime should still publish timer-related visibility:

- `WaitingForTimer { resume_after_ms }`
- optional `TimerResumed` if we want clearer progress inspection

This preserves the ability for `wait` or user-facing surfaces to explain why a cell is still running.

## 6. Service And Driver Responsibilities

### 6.1 `CodeModeService` becomes a cell supervisor

`CodeModeService` should own per-session state that includes:

- active cell metadata
- a handle to the background host task
- the latest published `ActiveCellHandle`
- a change notification primitive for `wait`

Recommended shape:

```rust
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, StoredValue>,
    active_cell: Option<ActiveCellHandle>,
    host_handle: Option<Arc<CellHostHandle>>,
}
```

Where `CellHostHandle` exposes:

- snapshot reads
- change notification
- cancellation

### 6.2 `CellDriver` stops requiring per-call `invoke_tool`

Today `CellDriver::drain_event_batch_with_request(...)` takes an `invoke_tool` closure from the foreground caller.

After the redesign:

- the background `CellHostTask` owns the nested tool executor permanently
- the host task drains `event_rx` continuously
- tool requests are fulfilled directly inside the host task
- the driver no longer depends on external `exec`/`wait` callers for progress

This is the core decoupling.

### 6.3 `wait` becomes a snapshot read plus optional blocking

`wait` should support:

- immediate snapshot return if new visible state already exists
- blocking for up to `wait_timeout_ms` for the next visible update
- returning terminal state if the cell already finished

`refresh_slice_ms` should no longer influence execution. It may either:

- be deprecated and removed, or
- remain as an output slicing hint only

It should not drive timers or nested tool fulfillment.

## 7. Executor Responsibilities

### 7.1 Remove the lifetime-bound executor

`CodeModeNestedToolExecutor<'a>` should be replaced by an owned background-safe executor such as:

```rust
pub struct CodeModeBackgroundExecutor {
    current_tools: Vec<Arc<dyn Tool>>,
    extensions: Vec<Arc<dyn ExecutionExtension>>,
    session_id: String,
    reply_to: String,
    visible_tools: Vec<String>,
    session_deadline: Option<Instant>,
    trace_bus: Arc<TraceBus>,
    provider: String,
    model: String,
    cancel_token: Arc<Notify>,
    is_autopilot: bool,
    todos_path: PathBuf,
    guard_state: Arc<tokio::sync::Mutex<ExecutionGuardState>>,
    cell_trace_ctx: Option<TraceContext>,
    cell_span_id: Option<String>,
    outer_tool_call_id: Option<String>,
}
```

This executor is created once per cell and reused for all nested tool calls during the cell lifetime.

### 7.2 Preserve existing safety and trace behavior

The background executor must continue to enforce:

- nested-tool allowlist checks
- autopilot/TODOS gating
- timeout handling
- cancellation handling
- trace emission

The redesign changes ownership and lifecycle, not safety policy.

## 8. `AgentLoop` Integration

### 8.1 Build a shared execution environment at `exec` launch

When `dispatch_exec_tool_call(...)` handles `exec`, it should assemble a background execution snapshot from the current loop state and pass it into `CodeModeService::execute(...)`.

This includes:

- the current visible tools
- provider/model metadata
- session trace seed
- shared execution guard state
- shared extensions

### 8.2 Do not rebuild the nested executor on every `wait`

`wait` should no longer instantiate nested execution machinery.

It should only:

- validate session/cell identity
- fetch or await the latest published state
- return an `ExecRunResult`

### 8.3 Keep foreground tool semantics separate

Foreground normal tool calls should continue to work while code mode exists.

The redesign should not make ordinary tool dispatch depend on the code-mode background task.

## 9. Detailed File Plan

### 9.1 `src/core.rs`

- Introduce shared execution-guard state on `AgentLoop`.
- Change extension storage from `Vec<Box<dyn ExecutionExtension>>` to `Vec<Arc<dyn ExecutionExtension>>`.
- Update `add_extension(...)` accordingly.

### 9.2 `src/core/step_helpers.rs`

- Replace the temporary nested-executor construction inside `dispatch_exec_tool_call(...)`.
- Build a `CodeModeExecutionSnapshot` for `exec`.
- Route `wait` to an observation-only service API.
- Remove any assumption that `wait` owns nested tool fulfillment or timer progress.

### 9.3 `src/code_mode/executor.rs`

- Replace lifetime-bound config with owned/shared config.
- Introduce `ExecutionGuardState`.
- Keep tool-context enrichment, autopilot gating, timeout, and trace logic.

### 9.4 `src/code_mode/service.rs`

- Introduce `CellHostTask` or equivalent background supervisor.
- Store per-session host handle and snapshot state.
- Implement observation-only `wait`.
- Keep `abort_active_cell(...)` as the cancellation path.

### 9.5 `src/code_mode/driver.rs`

- Remove the external `invoke_tool` dependency from public drain APIs.
- Let the background host task continuously consume runtime events.
- Keep worker shutdown and cancellation handling.

### 9.6 `src/code_mode/runtime/mod.rs`

- Remove `__wait_for_resume`.
- Replace timer resume-by-command with internal worker blocking wait.
- Keep timer visibility events and flush visibility semantics.

### 9.7 `src/code_mode/protocol.rs`

- Remove `CellCommand::Drain`.
- Keep `Cancel`.
- Optionally add clearer runtime events if helpful for observation.

### 9.8 `src/code_mode/cell.rs`

- Extend published cell snapshot state if necessary to support observer-only `wait`.
- Ensure status transitions remain correct when the host task publishes background progress continuously.

## 10. Recommended Implementation Sequence

Implement in the following order.

### Phase 1: make dependencies background-safe

- Add `ExecutionGuardState`.
- Move extensions to `Arc<dyn ExecutionExtension>`.
- Convert the nested executor to owned/shared config.

This phase should compile without changing runtime behavior yet.

### Phase 2: background nested tool fulfillment

- Introduce `CellHostTask`.
- Move nested tool fulfillment into the host task.
- Remove `invoke_tool` from `wait`.

At the end of this phase, JS tool calls no longer depend on a foreground `wait` call.

### Phase 3: timer autonomy

- Remove `CellCommand::Drain`.
- Remove `__wait_for_resume()`.
- Add internal timer blocking/wake behavior in the runtime worker.
- Keep publishing `WaitingForTimer`.

At the end of this phase, timers no longer depend on `wait`.

### Phase 4: observation-only `wait`

- simplify `wait` to snapshot/read semantics
- deprecate or repurpose `refresh_slice_ms`
- tighten terminal cleanup and session state transitions

### Phase 5: trace and polish

- add a dedicated cell background span
- ensure nested tool spans hang under the cell span
- update docs and status files

## 11. Verification Plan

The redesign is complete when the following behaviors are covered by tests.

### 11.1 Nested tool autonomy

- `exec` starts a cell that triggers nested tool calls after the initial `exec` summary is returned.
- The nested tools still execute successfully without any `wait`.
- `wait` later observes the updated cell state rather than causing it.

### 11.2 Timer autonomy

- `exec` starts a cell that uses `setTimeout(...)`.
- The timer callback eventually runs without any `wait`.
- `wait` can observe intermediate `WaitingForTimer` state and later terminal state.

### 11.3 `flush()` semantics

- `flush()` publishes intermediate output.
- Execution continues automatically after `flush()`.
- No explicit resume is required.

### 11.4 Cancellation

- Cancelling an active cell interrupts timer waits.
- Cancelling an active cell interrupts nested tool waits.
- Terminal state is published exactly once.

### 11.5 Guard-state correctness

- autopilot loop protection still triggers correctly for background nested tool repetition
- shared guard state does not deadlock

## 12. Risks And Tradeoffs

### 12.1 Shared state complexity increases

Moving guard state and extensions to shared ownership adds more `Arc`-based plumbing. This is acceptable because it removes a stronger and less correct lifetime coupling to the foreground turn.

### 12.2 Trace topology changes

Trace trees will change. Nested background tool spans will no longer appear under `wait`. This is expected and desired.

### 12.3 Existing `wait` timing expectations will change

Some current tests or model expectations may assume that `wait` advances timers. Those need to be updated to the new observer-only contract.

## 13. Final Recommendation

Proceed with the redesign, but do it in two separable steps:

1. decouple nested tool execution from foreground `wait`
2. decouple timer continuation from foreground `wait`

This keeps the rollout controlled, preserves the current safety model, and directly matches the intended semantics we agreed on:

- background JS cells keep running after `exec`
- `flush()` only publishes
- `wait` only observes
- timers remain host-provided, but no longer require external resume
