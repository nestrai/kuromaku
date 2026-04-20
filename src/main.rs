use std::path::Path;
use std::time::Instant;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;

mod config;
mod dag;
#[allow(dead_code)]
mod llm;
mod runner;
mod state;
#[allow(dead_code)]
mod ui;

#[derive(Parser)]
#[command(name = "koto", about = "Reproducible AI agent teams")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the agent team
    Up {
        /// Path to the flow config file
        #[arg(short, long, default_value = "koto.yaml")]
        file: String,
    },
    /// Stop the agent team
    Down,
    /// Show running agents and state
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Up { file } => run_up(&file).await?,
        Command::Down => {
            println!("koto down: not yet implemented");
        }
        Command::Status => {
            println!("koto status: not yet implemented");
        }
    }

    Ok(())
}

async fn run_up(file: &str) -> Result<()> {
    let flow_start = Instant::now();
    let path = Path::new(file);

    if !path.exists() {
        return Err(eyre!(
            "config file '{}' not found\n\nhint: create a koto.yaml in the current directory, or use --file <path>",
            file
        ));
    }

    ui::print_command(&format!("koto up {file}"));

    let flow_config = config::load_config(path)?;
    ui::print_flow_start(
        &flow_config.name,
        file,
        flow_config.stages.len(),
        flow_config.agents.len(),
    );

    // Check for API key early if any agent uses the api backend
    let needs_api_key = flow_config
        .agents
        .iter()
        .any(|a| a.backend == config::Backend::Api)
        || flow_config
            .stages
            .iter()
            .any(|s| s.backend == Some(config::Backend::Api));
    if needs_api_key
        && std::env::var("ANTHROPIC_API_KEY")
            .unwrap_or_default()
            .is_empty()
    {
        return Err(eyre!(
            "ANTHROPIC_API_KEY is not set but one or more agents use the 'api' backend\n\nhint: export ANTHROPIC_API_KEY=sk-..."
        ));
    }

    // Validate DAG and get execution order
    let stages = dag::validate_dag(&flow_config)?;

    // Resolve and print backends
    let mut seen_backends = std::collections::HashSet::new();
    let mut backend_list: Vec<(&str, &str)> = Vec::new();
    for agent in &flow_config.agents {
        let name = match agent.backend {
            config::Backend::Api => "api",
            config::Backend::ClaudeCli => "claude-cli",
            config::Backend::Ollama => "ollama",
        };
        if seen_backends.insert(name) {
            backend_list.push((name, ""));
        }
    }
    ui::print_backends_ok(&backend_list);

    // Run stages
    let state_path = Path::new(&flow_config.state.path);
    let results = runner::run_stages(&flow_config, &stages, state_path).await?;

    // Print summary
    let total_elapsed = flow_start.elapsed();
    let summary = runner::build_summary(&results);
    let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
    let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();

    ui::print_flow_complete(
        &summary,
        &format_elapsed(total_elapsed),
        &total_in.to_string(),
        &total_out.to_string(),
        "—",
        &flow_config.state.path,
    );

    Ok(())
}

fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    }
}
