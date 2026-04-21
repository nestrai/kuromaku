use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;

mod config;
mod dag;
#[allow(dead_code)]
mod executor;
#[allow(dead_code)]
mod llm;
mod runner;
mod state;
#[allow(dead_code)]
mod ui;

const KOTO_DIR: &str = ".koto";
const FLOWS_DIR: &str = ".koto/flows";

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
        /// Flow name (looks in .koto/flows/<name>.yaml)
        flow: Option<String>,

        /// Path to the flow config file (overrides flow name lookup)
        #[arg(short, long)]
        file: Option<String>,

        /// Executor name: "local" (default), or a name from .koto/executors/<name>.yaml
        /// Can also be set via KOTO_EXECUTOR env var.
        #[arg(short = 'e', long)]
        executor: Option<String>,
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
        Command::Up {
            flow,
            file,
            executor,
        } => run_up(flow.as_deref(), file.as_deref(), executor.as_deref()).await?,
        Command::Down => {
            println!("koto down: not yet implemented");
        }
        Command::Status => {
            println!("koto status: not yet implemented");
        }
    }

    Ok(())
}

/// Resolve the flow config file path from CLI arguments.
fn resolve_flow_path(flow: Option<&str>, file: Option<&str>) -> Result<PathBuf> {
    // --file takes precedence
    if let Some(f) = file {
        let path = PathBuf::from(f);
        if !path.exists() {
            return Err(eyre!("config file '{}' not found", f));
        }
        return Ok(path);
    }

    // If a flow name is given, look in .koto/flows/
    if let Some(name) = flow {
        let path = PathBuf::from(FLOWS_DIR).join(format!("{name}.yaml"));
        if !path.exists() {
            return Err(eyre!(
                "flow '{name}' not found at {}\n\nhint: create {0} or use --file <path>",
                path.display()
            ));
        }
        return Ok(path);
    }

    // No args: list available flows
    let flows_dir = Path::new(FLOWS_DIR);
    if !flows_dir.exists() {
        return Err(eyre!(
            "no .koto/flows/ directory found\n\nhint: create .koto/flows/<name>.yaml, or use --file <path>"
        ));
    }

    let mut flows: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(flows_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".yaml") || name_str.ends_with(".yml") {
                flows.push(
                    name_str
                        .trim_end_matches(".yaml")
                        .trim_end_matches(".yml")
                        .to_string(),
                );
            }
        }
    }

    if flows.is_empty() {
        return Err(eyre!(
            "no flows found in .koto/flows/\n\nhint: create .koto/flows/<name>.yaml"
        ));
    }

    flows.sort();
    let list = flows
        .iter()
        .map(|f| format!("  - {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(eyre!(
        "no flow specified\n\navailable flows:\n{list}\n\nusage: koto up <flow-name>"
    ))
}

async fn run_up(flow: Option<&str>, file: Option<&str>, executor: Option<&str>) -> Result<()> {
    let flow_start = Instant::now();
    let path = resolve_flow_path(flow, file)?;
    let display_path = path.display().to_string();

    ui::print_command(&format!("koto up {}", flow.unwrap_or(&display_path)));

    let flow_config = config::load_config(&path)?;

    // Derive flow name for tmux sessions
    let flow_name = flow
        .map(|s| s.to_string())
        .unwrap_or_else(|| flow_config.name.clone());

    ui::print_flow_start(
        &flow_config.name,
        &display_path,
        flow_config.stages.len(),
        flow_config.agents.len(),
    );

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

    // Resolve executor: --executor flag > KOTO_EXECUTOR env var > default (local)
    let koto_dir = Path::new(KOTO_DIR);
    let executor_target = executor::resolve_executor_target(executor, koto_dir)?;

    // Load guide and rules context
    let guide = runner::load_guide(koto_dir);
    let rules_cache = runner::load_rules_for_agents(&flow_config.agents, koto_dir)?;

    // Run stages
    let state_path = Path::new(&flow_config.state.path);
    let results = runner::run_stages(
        &flow_config,
        &stages,
        state_path,
        &flow_name,
        &guide,
        &rules_cache,
        &executor_target,
    )
    .await?;

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
