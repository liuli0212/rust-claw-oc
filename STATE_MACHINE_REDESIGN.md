# AgentLoop State Machine Redesign

## The Problem with Heuristic Exit Conditions
Previously, `Rusty-Claw` relied on a set of heuristic rules to determine when the agent had finished a task. These rules included:
1. **Task Complexity Guessing (`is_complex_task`)**: Analyzing the user's prompt length and keywords to guess if it required multiple steps.
2. **Text Parsing (`is_task_complete`)**: Searching the LLM's output text for words like "done", "completed", "正在", or "接下来" to infer its intent.
3. **Implicit Tool Exits**: Assuming that if a "simple task" produced text but no tool calls, it was safe to exit the loop.

This heuristic approach proved **fragile and unsystematic**. It caused the agent to frequently "silent exit" or terminate prematurely when it merely wanted to explain a plan to the user before executing the next tool call (e.g., replying "I found the bug, I am patching it now..." and then getting forcefully terminated by the system).

## The Systematic Solution: Explicit Tool-Driven Lifecycle
To solve this, we are moving from a "guess-based" state machine to a **deterministic, explicit tool-driven state machine**.

The control over the execution loop is handed over entirely to the LLM via its JSON-structured tool calls.

### Core Architecture Changes

1. **Introduction of `finish_task` Tool**:
   - A new native tool `finish_task(reason: String)` is injected into the context.
   - The LLM is explicitly instructed in the System Prompt: *"You must call `finish_task` when you are absolutely done with the request."*

2. **Removal of Heuristics**:
   - `is_complex_task` and `is_task_complete` text-parsing functions are completely removed.
   - The loop no longer cares how long the user's prompt is or what words the agent uses in its conversational text.

3. **Deterministic Exit Conditions**:
   The `AgentLoop::step` loop will **ONLY** exit under three strict conditions:
   - **Success**: The LLM explicitly calls the `finish_task` tool.
   - **Timeout/Limits**: The loop reaches the hardcoded maximum iteration limit (e.g., 15 iterations) to prevent infinite loops.
   - **Error**: A fatal API or network error occurs.

4. **Handling "Talk-Only" Iterations (The Safety Net)**:
   If the LLM outputs text but fails to call *any* tools (neither working tools nor `finish_task`), the system will **NOT** exit. Instead, it will automatically inject a synthetic user prompt:
   > *"You did not call any tools. If the task is incomplete, please proceed with your next tool call. If the task is fully completed, you MUST call the `finish_task` tool to exit."*
   
   This ensures the LLM has the freedom to dedicate an entire iteration just to explaining complex concepts to the user, without fear of the system abruptly shutting down.

## Implementation Steps
1. Add `FinishTaskTool` to `src/tools.rs`.
2. Register `FinishTaskTool` in `src/main.rs` alongside other native tools.
3. Update `SystemPrompt` in `src/context.rs` to enforce the usage of `finish_task`.
4. Gut the heuristic exit logic from `src/core.rs` (`is_complex_task`, `progress_decision_summary`, etc.).
5. Simplify the `AgentLoop::step` `while` loop to only break upon encountering the `finish_task` tool name.
