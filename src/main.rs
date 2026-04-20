use clap::{Parser, Subcommand};
use color_eyre::Result;

#[derive(Parser)]
#[command(name = "koto", about = "Reproducible AI agent teams")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the agent team
    Up,
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
        Command::Up => {
            println!("koto up: not yet implemented");
        }
        Command::Down => {
            println!("koto down: not yet implemented");
        }
        Command::Status => {
            println!("koto status: not yet implemented");
        }
    }

    Ok(())
}
