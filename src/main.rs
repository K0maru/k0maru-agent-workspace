use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "k0maru-workspace")]
#[command(about = "Git Worktree isolation and sandbox lifecycle engine for K0maru-Harness", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Provision a new isolated worktree sandbox for a task
    Provision {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        base: Option<String>,
    },
    /// Teardown and clean up an existing worktree sandbox
    Teardown {
        #[arg(long)]
        task_id: String,
    },
    /// List active sandboxes and stale worktrees
    List,
    /// Prune stale or orphaned worktree sandboxes
    Prune {
        #[arg(long, default_value_t = 86400)]
        max_age_secs: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Provision { task_id, repo, base } => {
            println!("Provisioning sandbox for task {} in repo {} (base: {:?})", task_id, repo, base);
        }
        Commands::Teardown { task_id } => {
            println!("Tearing down sandbox for task {}", task_id);
        }
        Commands::List => {
            println!("Listing active sandboxes...");
        }
        Commands::Prune { max_age_secs } => {
            println!("Pruning stale sandboxes older than {} seconds...", max_age_secs);
        }
    }
    Ok(())
}
