# Critical Fix: API Key Panic & Token Limits

## 1. Problem Analysis
### Issue A: Panic on Missing API Key
- **Cause**: `unwrap()` / `.expect()` calls in `llm_client.rs` when reading API keys.
- **Symptom**: CLI crashes entirely if `DEEPSEEK_API_KEY` (or others) is missing.
- **Fix**: Replace `.expect()` with `?` operator and `ok_or_else` to return `Result`.

### Issue B: Token Limit Too Small (32k)
- **Cause**: `estimate_context_window` defaults to 32,000 for unknown models.
- **Symptom**: Context truncation for modern models like `gemini-3.1-pro` or `qwen-plus`.
- **Fix**: Update heuristic logic and increase default to 128,000.

### Issue C: Model Override Bug (OpenAI Compat)
- **Cause**: `prov_config.model.clone().or(model)` prioritization is inverted.
- **Symptom**: CLI arg `/model deepseek-coder` is ignored if config has a default model.
- **Fix**: Swap logic to `model.or(prov_config.model.clone())`.

## 2. Implementation Plan
We will patch `src/llm_client.rs` to address all three issues in one go.

### Changes to `src/llm_client.rs`

- [x] `src/llm_client.rs` modified.
- [x] `estimate_context_window` handles `gemini-2`, `gemini-3`, `qwen`, `deepseek` correctly.
- [x] `create_llm_client` returns `Err(String)` instead of panicking when keys are missing.
- [x] CLI model argument takes precedence over config file for OpenAI/Aliyun providers.
- [x] `cargo check` passes.
    - Add `gemini-2` -> 1M, `gemini-3` -> 2M (safe default 1M).
    - Add `qwen` -> 128k (standard).
    - Add `deepseek` -> 64k/128k.
    - **Fallback**: Change from 32,000 to **128,000**.

2.  **Refactor `create_llm_client`**:
    - **Gemini Block**: Replace `unwrap/expect` with `ok_or_else(|| ...)?`.
    - **OpenAI Block**: Replace `unwrap/expect` with `ok_or_else(|| ...)?`.
    - **OpenAI Block**: Fix model override logic: `model.or(...)` instead of `...or(model)`.
    - **Default Fallback Block**: Ensure `std::env::var` returns error string, not panic.

## 3. Verification
- Compile check (`cargo check`).
- Run `cargo test estimate_context_window`.
- Manual check: Run `rusty-claw --provider deepseek` without key -> Should show error, not crash.

## 4. Execution
Run `/start-work` to apply fixes.
