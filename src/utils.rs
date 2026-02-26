use tracing::Level;

pub fn truncate_log(log: &str) -> String {
    let (max_lines, max_chars) = if tracing::enabled!(Level::TRACE) {
        (2000, 100_000)
    } else if tracing::enabled!(Level::DEBUG) {
        (200, 15_000)
    } else {
        (20, 2_000)
    };
    truncate_impl(log, max_lines, max_chars)
}

pub fn truncate_log_error(log: &str) -> String {
    truncate_impl(log, 200, 15_000)
}

pub fn truncate_tool_output(log: &str) -> String {
    // Generous limit for LLM consumption (independent of log level)
    // 50k chars is approx 12-15k tokens, safe for most modern contexts (128k+)
    truncate_impl(log, 2000, 50_000)
}

fn truncate_impl(log: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = log.lines().collect();

    let truncated_str = if lines.len() <= max_lines {
        log.to_string()
    } else {
        let keep_head = max_lines / 2;
        let keep_tail = max_lines - keep_head;
        let head = lines[0..keep_head].join("\n");
        let tail = lines[lines.len() - keep_tail..].join("\n");
        format!(
            "{}\n\n[... Truncated {} lines ...]\n\n{}",
            head,
            lines.len() - max_lines,
            tail
        )
    };

    if truncated_str.len() <= max_chars {
        truncated_str
    } else {
        let keep = max_chars / 2;
        let head: String = truncated_str.chars().take(keep).collect();
        let tail: String = truncated_str
            .chars()
            .skip(truncated_str.len() - keep)
            .collect();
        format!(
            "{}\n\n[... Truncated {} characters ...]\n\n{}",
            head,
            truncated_str.len() - max_chars,
            tail
        )
    }
}
