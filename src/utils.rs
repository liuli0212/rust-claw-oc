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
    let lines_raw: Vec<&str> = log.lines().collect();

    // Line-based truncation
    let truncated_by_lines = if lines_raw.len() <= max_lines {
        log.to_string()
    } else {
        let keep_head = max_lines / 2;
        let keep_tail = max_lines - keep_head;
        let head = lines_raw[0..keep_head].join("\n");
        let tail = lines_raw[lines_raw.len() - keep_tail..].join("\n");
        format!(
            "{}\n\n[... Truncated {} lines ...]\n\n{}",
            head,
            lines_raw.len() - max_lines,
            tail
        )
    };

    // Character-based truncation (second pass)
    if truncated_by_lines.len() <= max_chars {
        truncated_by_lines
    } else {
        // When truncating characters, we must be careful not to break multi-byte UTF-8 sequences.
        // chars().collect() is safer than slicing bytes.
        let chars: Vec<char> = truncated_by_lines.chars().collect();
        let keep = max_chars / 2;
        if chars.len() <= max_chars {
             truncated_by_lines
        } else {
            let head: String = chars.iter().take(keep).collect();
            let tail: String = chars.iter().skip(chars.len() - keep).collect();
            format!(
                "{}\n\n[... Truncated {} characters ...]\n\n{}",
                head,
                chars.len() - max_chars,
                tail
            )
        }
    }
}
