use crate::core::AgentOutput;
use async_trait::async_trait;
use console::{style, Emoji};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use termimad::{crossterm::style::Color::*, MadSkin};

pub struct TuiOutput {
    skin: MadSkin,
    spinner: Arc<Mutex<Option<ProgressBar>>>,
    in_thinking: Arc<Mutex<bool>>,
    line_buffer: Arc<Mutex<String>>,
    in_code_block: Arc<Mutex<bool>>,
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
            in_code_block: Arc::new(Mutex::new(false)),
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
            } else if *in_code_guard {
                println!("{}", style(line).color256(250));
            } else {
                self.skin.print_inline(&line);
                println!();
            }
        }
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
            
            let mut in_code_guard = self.in_code_block.lock().unwrap();
            if line.trim_start().starts_with("```") {
                *in_code_guard = !*in_code_guard;
                println!("{}", style(&line).dim());
            } else if line.starts_with("[System]") {
                println!("{}", style(line.trim()).cyan());
            } else if line.starts_with("[Error]") {
                println!("{}", style(line.trim()).red());
            } else if *in_code_guard {
                println!("{}", style(&line).color256(250));
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
                print!("{}", style(c).dim().italic());
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
        // Don't print full result if it's huge, just a success indicator
        let summary = if result.len() > 100 {
            format!(
                "{}...",
                &result
                    .chars()
                    .take(80)
                    .collect::<String>()
                    .replace('\n', " ")
            )
        } else {
            result.replace('\n', " ")
        };

        println!("  {} {}", style("✔").green(), style(summary).dim());
    }

    async fn on_error(&self, error: &str) {
        self.stop_spinner();
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
        self.stop_spinner();

        println!(
            "  ╭─ {} {}",
            style("Plan").bold().cyan(),
            style("─".repeat(45)).cyan()
        );
        if let Some(goal) = &state.goal {
            println!("  │ {}: {}", style("Goal").bold(), style(goal).italic());
            println!("  │");
        }

        let mut completed = 0;
        for (i, step) in state.plan_steps.iter().enumerate() {
            let (icon, color_step) = match step.status.as_str() {
                "completed" => {
                    completed += 1;
                    ("✅", style(&step.step).green().dim())
                }
                "in_progress" => ("🔄", style(&step.step).blue().bold()),
                _ => ("⏳", style(&step.step).dim()),
            };

            let mut line = format!("  │  {} {}. {}", icon, i + 1, color_step);
            if let Some(note) = &step.note {
                if !note.is_empty() {
                    line.push_str(&format!(" - {}", style(note).dim().italic()));
                }
            }
            println!("{}", line);
        }
        let total = state.plan_steps.len();
        println!("  │");
        println!(
            "  │ {}: {}/{} completed",
            style("Progress").dim(),
            completed,
            total
        );
        println!("  ╰{}", style("─".repeat(50)).cyan());
        println!();
    }

    async fn on_task_finish(&self, summary: &str) {
        self.stop_spinner();
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