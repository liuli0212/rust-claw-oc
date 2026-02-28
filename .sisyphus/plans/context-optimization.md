# Context Optimization Plan

## 1. Fix: "Current Turn" showing 0
- **Problem**: `current_turn` is `None` when the agent is idle (waiting for user), so `context` command shows 0 tokens.
- **Solution**: If `current_turn` is None, fall back to calculating the *Last Turn* from history. This gives the user relevant info about the most recent interaction.
- **Action**: Modify `get_context_status` and `get_detailed_stats` in `src/context.rs`.

## 2. Fix: Project Context Token Mismatch
- **Problem**: `get_detailed_stats` simply reads and counts the full file content. `build_system_prompt` (what is actually sent to LLM) adds headers and *truncates* the content.
- **Result**: The reported "Project Context" token count is inaccurate (usually too high).
- **Solution**: Refactor the project context building logic into a shared helper function `build_project_context_string()` that both methods call. This ensures the reported stats match the actual prompt.

## 3. Implementation Details
- **Refactor**: Create `fn get_project_context_string() -> String` that handles the reading, headers, and truncation.
- **Update**: Call this helper in `build_system_prompt`.
- **Update**: Call this helper in `get_detailed_stats` for accurate counting.
- **Update**: In `get_detailed_stats` and `get_context_status`, add logic: `let display_tokens = current.unwrap_or(last_turn_tokens)`.

## 4. Execution
Run `/start-work` to apply these changes.
