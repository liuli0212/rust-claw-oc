use clap::{Parser, Subcommand};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "acp-client")]
#[command(about = "Rusty-Claw ACP Protocol CLI Client", long_about = None)]
struct Cli {
    /// ACP Server URL
    #[arg(short, long, default_value = "http://localhost:18080")]
    url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List available agent capabilities
    List,
    /// Run a natural language task
    Run {
        /// The task to execute
        task: String,
        /// Optional session ID (will use a new UUID if not provided)
        #[arg(short, long)]
        session: Option<String>,
    },
}

#[derive(Deserialize, Debug)]
struct Capability {
    name: String,
    description: String,
    // Ignoring parameters_schema for now in the simple CLI display
}

#[derive(Deserialize, Debug)]
struct CapabilitiesResponse {
    agent_id: String,
    capabilities: Vec<Capability>,
}

#[derive(Serialize)]
struct RunRequest {
    task: String,
    session_id: Option<String>,
}

#[derive(Deserialize, Debug)]
struct RunResponse {
    session_id: String,
    status: String,
    output: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let client = Client::builder()
        .timeout(Duration::from_secs(300)) // Tasks can take time
        .build()?;

    match &cli.command {
        Commands::List => {
            let url = format!("{}/capabilities", cli.url.trim_end_matches('/'));
            println!("🔍 Fetching capabilities from {}...", url);
            let res = client.get(&url).send().await?;
            if !res.status().is_success() {
                eprintln!("❌ Error: Server returned status {}", res.status());
                return Ok(());
            }
            let data: CapabilitiesResponse = res.json().await?;
            println!("\n🤖 Agent ID: \x1b[1;36m{}\x1b[0m", data.agent_id);
            println!("\x1b[1;34mAvailable Capabilities:\x1b[0m");
            for cap in data.capabilities {
                println!("  • \x1b[1m{}\x1b[0m: {}", cap.name, cap.description);
            }
        }
        Commands::Run { task, session } => {
            let url = format!("{}/run", cli.url.trim_end_matches('/'));
            println!("🚀 Sending task to {}...", url);
            println!("📝 Task: \x1b[1;33m{}\x1b[0m", task);

            let req_body = RunRequest {
                task: task.clone(),
                session_id: session.clone(),
            };

            let res = client.post(&url).json(&req_body).send().await?;

            if !res.status().is_success() {
                eprintln!("❌ Error: Server returned status {}", res.status());
                return Ok(());
            }

            let data: RunResponse = res.json().await?;
            println!("\n✅ \x1b[1;32mExecution Finished\x1b[0m");
            println!("🆔 Session ID: {}", data.session_id);
            println!("📊 Status: {}", data.status);
            println!("\n\x1b[1;34m--- OUTPUT ---\x1b[0m\n");
            println!("{}", data.output);
            println!("\n\x1b[1;34m--------------\x1b[0m");
        }
    }

    Ok(())
}
