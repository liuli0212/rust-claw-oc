use crate::core::{AgentOutput, OutputRouter, SilentOutputWrapper};
use async_trait::async_trait;
use console::{style, Emoji};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;
use termimad::{crossterm::style::Color::*, MadSkin};

use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style as RatatuiStyle},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::io;

pub struct DashboardStats {
    pub tokens: usize,
    pub max_tokens: usize,
    pub energy: usize,
    pub provider: String,
    pub model: String,
}

pub struct TuiOutput {
    skin: MadSkin,
    spinner: Arc<Mutex<Option<ProgressBar>>>,
    in_thinking: Arc<Mutex<bool>>,
    line_buffer: Arc<Mutex<String>>,
    in_code_block: Arc<Mutex<Option<String>>>,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,

    // Dashboard state
    task_state: Arc<Mutex<Option<crate::task_state::TaskStateSnapshot>>>,
    stats: Arc<Mutex<DashboardStats>>,
    last_plan_fingerprint: Arc<Mutex<String>>,
}

impl TuiOutput {
    pub fn new() -> Self {
        let mut skin = MadSkin::default();
        skin.set_headers_fg(AnsiValue(208)); // Orange for headers
        skin.bold.set_fg(AnsiValue(220)); // Gold for bold
        skin.italic.set_fg(AnsiValue(245)); // Grey for italic
        skin.code_block.set_bg(AnsiValue(236)); // Dark grey bg for code
        skin.code_block.set_fg(AnsiValue(250)); // Light grey fg for code
        skin.inline_code.set_fg(AnsiValue(220)); // Gold for inline code
        skin.inline_code.set_bg(AnsiValue(236));

        Self {
            skin,
            spinner: Arc::new(Mutex::new(None)),
            in_thinking: Arc::new(Mutex::new(false)),
            line_buffer: Arc::new(Mutex::new(String::new())),
            in_code_block: Arc::new(Mutex::new(None)),
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            task_state: Arc::new(Mutex::new(None)),
            stats: Arc::new(Mutex::new(DashboardStats {
                tokens: 0,
                max_tokens: 1,
                energy: 100,
                provider: "Unknown".to_string(),
                model: "Unknown".to_string(),
            })),
            last_plan_fingerprint: Arc::new(Mutex::new(String::new())),
        }
    }

    fn start_spinner(&self, message: &str) {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
                .template("{spinner:.blue} {msg}")
                .unwrap(),
        );
        pb.set_message(message.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));

        let mut guard = self.spinner.lock().unwrap();
        if let Some(old_pb) = guard.replace(pb) {
            old_pb.finish_and_clear();
        }
    }

    fn stop_spinner(&self) {
        let mut guard = self.spinner.lock().unwrap();
        if let Some(pb) = guard.take() {
            pb.finish_and_clear();
        }
    }

    fn print_markdown(&self, text: &str) {
        self.skin.print_text(text);
    }

    fn flush_line_buffer(&self) {
        let mut buffer_guard = self.line_buffer.lock().unwrap();
        if !buffer_guard.is_empty() {
            let line = buffer_guard.clone();
            buffer_guard.clear();

            let in_code_guard = self.in_code_block.lock().unwrap();
            if line.starts_with("[System]") {
                println!("{}", style(line.trim()).cyan());
            } else if line.starts_with("[Error]") {
                println!("{}", style(line.trim()).red());
            } else if let Some(lang) = in_code_guard.as_ref() {
                let syntax = self
                    .syntax_set
                    .find_syntax_by_token(lang)
                    .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
                let mut h =
                    HighlightLines::new(syntax, &self.theme_set.themes["base16-ocean.dark"]);
                let ranges: Vec<(syntect::highlighting::Style, &str)> =
                    h.highlight_line(&line, &self.syntax_set).unwrap();
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                print!("{}", escaped);
                if !line.ends_with('\n') {
                    println!();
                }
            } else {
                self.skin.print_inline(&line);
                println!();
            }
        }
    }

    fn render_dashboard(&self) {
        let task_state = self.task_state.lock().unwrap().clone();
        if task_state.is_none() {
            return;
        }

        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = match Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(10),
            },
        ) {
            Ok(t) => t,
            Err(_) => return,
        };

        let stats = self.stats.lock().unwrap();

        let _ = terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)].as_ref())
                .split(f.area());

            // Left side: Task Plan
            let plan_block = Block::default()
                .title(" 🎯 Active Plan ")
                .borders(Borders::ALL)
                .border_style(RatatuiStyle::default().fg(Color::Cyan));
            if let Some(state) = &task_state {
                let items: Vec<ListItem> = state
                    .plan_steps
                    .iter()
                    .enumerate()
                    .map(|(i, step)| {
                        let icon = match step.status.as_str() {
                            "completed" => "✅",
                            "in_progress" => "🔄",
                            _ => "⏳",
                        };
                        let content = format!("{} {}. {}", icon, i + 1, step.step);
                        let style = if step.status == "in_progress" {
                            RatatuiStyle::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else if step.status == "completed" {
                            RatatuiStyle::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::DIM)
                        } else {
                            RatatuiStyle::default().fg(Color::Gray)
                        };
                        ListItem::new(content).style(style)
                    })
                    .collect();
                let list = List::new(items).block(plan_block);
                f.render_widget(list, chunks[0]);
            } else {
                f.render_widget(
                    Paragraph::new("No active task").block(plan_block),
                    chunks[0],
                );
            }

            // Right side: Stats
            let stats_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(
                    [
                        Constraint::Length(3), // Token Gauge
                        Constraint::Length(3), // Energy Gauge
                        Constraint::Min(2),    // Model Info
                    ]
                    .as_ref(),
                )
                .split(chunks[1]);

            // Token Usage
            let token_pct = (stats.tokens as f64 / stats.max_tokens as f64).min(1.0);
            let token_gauge = Gauge::default()
                .block(
                    Block::default()
                        .title(format!(
                            " 📊 Context: {}/{} ",
                            stats.tokens, stats.max_tokens
                        ))
                        .borders(Borders::ALL),
                )
                .gauge_style(RatatuiStyle::default().fg(if token_pct > 0.8 {
                    Color::Red
                } else {
                    Color::Green
                }))
                .ratio(token_pct);
            f.render_widget(token_gauge, stats_chunks[0]);

            // Energy
            let energy_pct = (stats.energy as f64 / 100.0).min(1.0);
            let energy_gauge = Gauge::default()
                .block(
                    Block::default()
                        .title(format!(" ⚡ Energy: {}% ", stats.energy))
                        .borders(Borders::ALL),
                )
                .gauge_style(RatatuiStyle::default().fg(if energy_pct < 0.2 {
                    Color::Red
                } else {
                    Color::Yellow
                }))
                .ratio(energy_pct);
            f.render_widget(energy_gauge, stats_chunks[1]);

            // Model Info
            let info = format!("Provider: {}\nModel: {}", stats.provider, stats.model);
            let info_p = Paragraph::new(info)
                .block(Block::default().title(" 🤖 System ").borders(Borders::ALL))
                .style(RatatuiStyle::default().fg(Color::Blue))
                .wrap(Wrap { trim: true });
            f.render_widget(info_p, stats_chunks[2]);
        });
    }
}

#[async_trait]
impl AgentOutput for TuiOutput {
    async fn on_waiting(&self, message: &str) {
        self.start_spinner(message);
    }

    fn clear_waiting(&self) {
        self.stop_spinner();
    }

    async fn flush(&self) {
        self.stop_spinner();
        self.flush_line_buffer();
    }

    async fn on_text(&self, text: &str) {
        self.stop_spinner();

        // If we were thinking, print a newline to separate from the reply
        {
            let mut thinking_guard = self.in_thinking.lock().unwrap();
            if *thinking_guard {
                println!();
                *thinking_guard = false;
            }
        }

        // Remove internal tags if any leak through
        let clean_chunk = text
            .replace("<final>", "")
            .replace("</final>", "")
            .replace("<think>", "")
            .replace("</think>", "");

        if clean_chunk.is_empty() {
            return;
        }

        let mut buffer_guard = self.line_buffer.lock().unwrap();

        buffer_guard.push_str(&clean_chunk);

        while let Some(pos) = buffer_guard.find('\n') {
            let line = buffer_guard[..pos].to_string();
            *buffer_guard = buffer_guard[pos + 1..].to_string();

            let in_code_guard = self.in_code_block.lock().unwrap();
            if line.trim_start().starts_with("```") {
                let mut in_code_guard = self.in_code_block.lock().unwrap();
                if in_code_guard.is_none() {
                    let lang = line
                        .trim_start()
                        .trim_start_matches("```")
                        .trim()
                        .to_string();
                    *in_code_guard = Some(if lang.is_empty() {
                        "text".to_string()
                    } else {
                        lang
                    });
                } else {
                    *in_code_guard = None;
                }
                println!("{}", style(&line).dim());
            } else if line.starts_with("[System]") {
                println!("{}", style(line.trim()).cyan());
            } else if line.starts_with("[Error]") {
                println!("{}", style(line.trim()).red());
            } else if let Some(lang) = in_code_guard.as_ref() {
                let syntax = self
                    .syntax_set
                    .find_syntax_by_token(lang)
                    .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
                let mut h =
                    HighlightLines::new(syntax, &self.theme_set.themes["base16-ocean.dark"]);
                let ranges: Vec<(syntect::highlighting::Style, &str)> =
                    h.highlight_line(&line, &self.syntax_set).unwrap();
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                print!("{}", escaped);
                if !line.ends_with('\n') {
                    println!();
                }
            } else {
                self.skin.print_inline(&line);
                println!();
            }
        }
    }

    async fn on_thinking(&self, text: &str) {
        if text.is_empty() {
            return;
        }

        self.stop_spinner();
        self.flush_line_buffer();

        {
            let mut thinking_guard = self.in_thinking.lock().unwrap();
            if !*thinking_guard {
                println!(
                    "  {} {}",
                    style(Emoji("🧠", "*")).cyan(),
                    style("Thinking...").cyan().italic()
                );
                *thinking_guard = true;
                print!("    ");
            }
        }

        // For streaming thinking, we print characters immediately.
        // If there's a newline, we indent the next line.
        for c in text.chars() {
            if c == '\n' {
                println!();
                print!("    ");
            } else {
                print!("{}", style(c).color256(14).italic());
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        self.stop_spinner();
        self.flush_line_buffer();

        // Close thinking block if active
        {
            let mut thinking_guard = self.in_thinking.lock().unwrap();
            if *thinking_guard {
                println!();
                *thinking_guard = false;
            }
        }

        let tool_icon = match name {
            "read_file" => "📄",
            "write_file" => "📝",
            "execute_bash" => "💻",
            "web_fetch" | "browser" => "🌐",
            "search" => "🔍",
            _ => "🔧",
        };

        // Truncate args for display
        let display_args = if args.len() > 60 {
            format!("{}...", &args.chars().take(57).collect::<String>())
        } else {
            args.to_string()
        };

        println!(
            "{} {} {}",
            style(tool_icon).bold(),
            style(name).cyan().bold(),
            style(format!("({})", display_args)).dim()
        );

        self.start_spinner(&format!("Running {}...", name));
    }

    async fn on_tool_end(&self, result: &str) {
        self.stop_spinner();

        let mut ok = true;
        let mut output_text = result.to_string();

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
            if let Some(b) = val.get("ok").and_then(|v| v.as_bool()) {
                ok = b;
            }
            if let Some(o) = val.get("output").and_then(|v| v.as_str()) {
                output_text = o.to_string();
            }
        }

        // Don't print full result if it's huge, just a success indicator
        let summary = if output_text.len() > 100 {
            format!(
                "{}...",
                &output_text
                    .chars()
                    .take(80)
                    .collect::<String>()
                    .replace('\n', " ")
            )
        } else {
            output_text.replace('\n', " ")
        };

        let icon = if ok { style("✔").green() } else { style("✖").red() };
        println!("  {} {}", icon, style(summary).dim());
    }

    async fn on_error(&self, error: &str) {
        self.stop_spinner();
        {
            let mut guard = self.task_state.lock().unwrap();
            *guard = None;
        }
        println!("{} {}", style("✖").red().bold(), style(error).red());
    }

    async fn on_file(&self, path: &str) {
        self.stop_spinner();
        println!(
            "  {} Created {}",
            style("VG").green(),
            style(path).bold().underlined()
        );
    }

    async fn on_plan_update(&self, state: &crate::task_state::TaskStateSnapshot) {
        if state.plan_steps.is_empty() {
            return;
        }

        // Calculate fingerprint to avoid redundant renders
        let fingerprint = format!(
            "{}:{}",
            state.goal.as_deref().unwrap_or(""),
            state.plan_steps.iter().map(|s| format!("{}{}", s.step, s.status)).collect::<String>()
        );

        // Update internal state
        {
            let mut fp_guard = self.last_plan_fingerprint.lock().unwrap();
            if *fp_guard == fingerprint {
                return; // No meaningful change
            }
            *fp_guard = fingerprint;
            let mut guard = self.task_state.lock().unwrap();
            *guard = Some(state.clone());
        }
        self.stop_spinner();

        // Trigger dashboard render
        self.render_dashboard();
    }

    async fn on_status_update(
        &self,
        tokens: usize,
        max_tokens: usize,
        energy: usize,
        provider: &str,
        model: &str,
    ) {
        {
            let mut guard = self.stats.lock().unwrap();
            guard.tokens = tokens;
            guard.max_tokens = max_tokens;
            guard.energy = energy;
            guard.provider = provider.to_string();
            guard.model = model.to_string();
        }
        self.render_dashboard();
    }

    async fn on_task_finish(&self, summary: &str) {
        self.stop_spinner();
        {
            let mut guard = self.task_state.lock().unwrap();
            *guard = None;
        }
        self.flush_line_buffer();
        println!(
            "\n{} {}\n",
            Emoji("🎉", "*"),
            style("Task Completed!").green().bold()
        );
        self.print_markdown(summary);
        println!();
    }
}

pub struct TuiOutputRouter;

impl OutputRouter for TuiOutputRouter {
    fn try_route(&self, reply_to: &str) -> Option<Arc<dyn AgentOutput>> {
        if reply_to == "cli" {
            let base_output = Arc::new(TuiOutput::new());
            return Some(Arc::new(SilentOutputWrapper { inner: base_output }));
        }
        None
    }
}
