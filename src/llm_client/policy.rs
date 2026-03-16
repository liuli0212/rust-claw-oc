pub fn estimate_context_window(model: &str) -> usize {
    let m = model.to_lowercase();
    if m.contains("gemini-2") || m.contains("gemini-3") {
        1_000_000
    } else if m.contains("1.5-pro") || m.contains("1.5-flash") {
        1_000_000
    } else if m.contains("gpt-4o")
        || m.contains("gpt-4-turbo")
        || m.contains("o1")
        || m.contains("o3")
    {
        128_000
    } else if m.contains("claude-3-5") || m.contains("claude-3-opus") {
        200_000
    } else if m.contains("deepseek") {
        64_000
    } else if m.contains("qwen") {
        128_000
    } else {
        128_000
    }
}
