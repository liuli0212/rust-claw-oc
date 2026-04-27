#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextExecCommand {
    pub code: String,
    pub auto_flush_ms: Option<u64>,
    pub cell_timeout_ms: Option<u64>,
}

pub fn parse_text_exec_command(input: &str) -> Option<TextExecCommand> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut auto_flush_ms = None;
    let mut cell_timeout_ms = None;
    let mut saw_marker = false;
    let mut code_start = 0usize;

    for segment in trimmed.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        let directive = parse_directive(line.trim())?;

        match directive {
            Directive::ExecMarker if !saw_marker => {
                saw_marker = true;
                code_start += segment.len();
            }
            Directive::ExecMarker => break,
            Directive::AutoFlushMs(value) => {
                if !saw_marker {
                    return None;
                }
                auto_flush_ms = Some(value);
                code_start += segment.len();
            }
            Directive::CellTimeoutMs(value) => {
                if !saw_marker {
                    return None;
                }
                cell_timeout_ms = Some(value);
                code_start += segment.len();
            }
            Directive::Blank if saw_marker => {
                code_start += segment.len();
            }
            Directive::Code => break,
            Directive::Blank => return None,
        }
    }

    if !saw_marker {
        return None;
    }

    let code = trimmed[code_start..].trim();
    if code.is_empty() {
        return None;
    }

    Some(TextExecCommand {
        code: code.to_string(),
        auto_flush_ms,
        cell_timeout_ms,
    })
}

enum Directive {
    ExecMarker,
    AutoFlushMs(u64),
    CellTimeoutMs(u64),
    Blank,
    Code,
}

fn parse_directive(line: &str) -> Option<Directive> {
    if line.is_empty() {
        return Some(Directive::Blank);
    }

    let Some(comment) = line.strip_prefix("//").map(str::trim) else {
        return Some(Directive::Code);
    };

    if comment == "rusty-claw: exec" {
        return Some(Directive::ExecMarker);
    }

    if let Some(value) = parse_u64_directive(comment, "auto_flush_ms")
        .or_else(|| parse_u64_directive(comment, "auto-flush-ms"))
    {
        return Some(Directive::AutoFlushMs(value));
    }

    if let Some(value) = parse_u64_directive(comment, "cell_timeout_ms")
        .or_else(|| parse_u64_directive(comment, "cell-timeout-ms"))
    {
        return Some(Directive::CellTimeoutMs(value));
    }

    Some(Directive::Code)
}

fn parse_u64_directive(comment: &str, key: &str) -> Option<u64> {
    let rest = comment.strip_prefix(key)?.trim_start();
    let value = rest
        .strip_prefix('=')
        .or_else(|| rest.strip_prefix(':'))?
        .trim();
    if value.is_empty() || value.chars().any(|ch| !ch.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comment_metadata_header() {
        let parsed = parse_text_exec_command(
            r#"
// rusty-claw: exec
// auto_flush_ms=1000
// cell_timeout_ms: 120000
const value = await tools.read_file({ path: "src/main.rs" });
text(value);
"#,
        )
        .expect("command should parse");

        assert_eq!(parsed.auto_flush_ms, Some(1000));
        assert_eq!(parsed.cell_timeout_ms, Some(120000));
        assert!(parsed.code.starts_with("const value"));
        assert!(parsed.code.contains("text(value);"));
    }

    #[test]
    fn parses_exec_marker_when_no_options_are_needed() {
        let parsed = parse_text_exec_command(
            r#"
// rusty-claw: exec

text(`raw template string`);
"#,
        )
        .expect("command should parse");

        assert_eq!(parsed.auto_flush_ms, None);
        assert_eq!(parsed.cell_timeout_ms, None);
        assert_eq!(parsed.code, "text(`raw template string`);");
    }

    #[test]
    fn treats_unrecognized_comment_after_directives_as_code() {
        let parsed = parse_text_exec_command(
            r#"
// rusty-claw: exec
// auto_flush_ms=50
// keep this code comment
text("ok");
"#,
        )
        .expect("command should parse");

        assert_eq!(parsed.auto_flush_ms, Some(50));
        assert!(parsed.code.starts_with("// keep this code comment"));
    }

    #[test]
    fn rejects_non_command_text() {
        assert!(parse_text_exec_command("Here is what I found.").is_none());
        assert!(parse_text_exec_command("// just a normal comment").is_none());
        assert!(parse_text_exec_command("// auto_flush_ms=abc\ntext('x');").is_none());
        assert!(parse_text_exec_command("// auto_flush_ms=50\ntext('x');").is_none());
        assert!(parse_text_exec_command("// exec\ntext('x');").is_none());
    }
}
