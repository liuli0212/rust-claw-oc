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
}

impl TuiOutput {
    pub fn new() -> Self {
        let mut skin = MadSkin::default();
        skin.set_headers_fg(AnsiValue(208)); // Orange for headers
        skin.bold.set_fg(AnsiValue(220));    // Gold for bold
        skin.italic.set_fg(AnsiValue(245));  // Grey for italic
        skin.code_block.set_bg(AnsiValue(236)); // Dark grey bg for code
        skin.code_block.set_fg(AnsiValue(250)); // Light grey fg for code
        skin.inline_code.set_fg(AnsiValue(220)); // Gold for inline code
        skin.inline_code.set_bg(AnsiValue(236));

        Self {
            skin,
            spinner: Arc::new(Mutex::new(None)),
            in_thinking: Arc::new(Mutex::new(false)),
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

    fn update_spinner(&self, message: &str) {
        let guard = self.spinner.lock().unwrap();
        if let Some(pb) = guard.as_ref() {
            pb.set_message(message.to_string());
        }
    }

    fn print_markdown(&self, text: &str) {
        self.skin.print_text(text);
    }
}

#[async_trait]
impl AgentOutput for TuiOutput {
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
        let clean_text = text.replace("<final>", "").replace("</final>", "").replace("<think>", "").replace("</think>", "");
        if clean_text.trim().is_empty() {
            return;
        }

        // Check for special status prefixes and style them
        if clean_text.trim_start().starts_with("[System]") {
            println!("{}", style(clean_text.trim()).cyan());
        } else if clean_text.trim_start().starts_with("[Error]") {
             println!("{}", style(clean_text.trim()).red());
        } else {
            self.print_markdown(&clean_text);
        }
    }

    async fn on_thinking(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        
        self.stop_spinner();

        {
            let mut thinking_guard = self.in_thinking.lock().unwrap();
            if !*thinking_guard {
                println!("  {} {}", style(Emoji("🧠", "*")).cyan(), style("Thinking...").cyan().italic());
                *thinking_guard = true;
            }
        }

        let lines = text.lines();
        for line in lines {
            if !line.trim().is_empty() {
                 println!("    {}", style(line.trim()).dim().italic());
            }
        }
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        self.stop_spinner();
        
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
            format!("{}...", &result.chars().take(80).collect::<String>().replace('\n', " "))
        } else {
            result.replace('\n', " ")
        };
        
        println!(
            "  {} {}", 
            style("✔").green(), 
            style(summary).dim()
        );
    }

    async fn on_error(&self, error: &str) {
        self.stop_spinner();
        println!(
            "{} {}", 
            style("✖").red().bold(), 
            style(error).red()
        );
    }
    
    async fn on_file(&self, path: &str) {
        self.stop_spinner();
        println!(
             "  {} Created {}", 
             style("VG").green(), 
             style(path).bold().underlined()
        );
    }
}
