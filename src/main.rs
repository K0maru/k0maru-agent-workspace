use clap::{Parser, Subcommand, ValueEnum};
use k0maru_workspace::handle::{CacheType, TeardownAction, WorkspaceProvisionRequest};
use k0maru_workspace::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use k0maru_workspace::router::{
    AgentType, MultiplexerType, RouterDispatchOptions, TaskEnvelope, WorkspaceRouterAdapter,
};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "k0maru-workspace")]
#[command(
    about = "Git Worktree isolation and ephemeral sandbox lifecycle engine for K0maru-Harness",
    long_about = None
)]
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
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        base: Option<String>,
        #[arg(long, value_enum)]
        cache: Vec<CacheArg>,
        #[arg(long)]
        inject: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Route a TaskEnvelope from k0maru-agent-router, provisioning sandbox and binding multiplexer
    Route {
        /// Path to TaskEnvelope JSON file or inline JSON string
        #[arg(long)]
        envelope: String,
        /// Override target repository root
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Override base branch
        #[arg(long)]
        base: Option<String>,
        /// Multiplexer backend (tmux, herdr, none)
        #[arg(long, value_enum, default_value_t = MultiplexerArg::None)]
        multiplexer: MultiplexerArg,
        /// Target agent (agy, claude-code, custom)
        #[arg(long, value_enum, default_value_t = AgentArg::Agy)]
        agent: AgentArg,
        /// Custom agent command if agent is custom
        #[arg(long)]
        cmd: Option<String>,
        /// Shared caches to mount
        #[arg(long, value_enum)]
        cache: Vec<CacheArg>,
        /// Injected configuration files
        #[arg(long)]
        inject: Vec<String>,
        /// Execute multiplexer split command immediately
        #[arg(long)]
        spawn: bool,
        /// Output machine-readable JSON receipt
        #[arg(long)]
        json: bool,
    },
    /// Teardown and clean up an existing worktree sandbox
    Teardown {
        #[arg(long)]
        task_id: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, value_enum, default_value_t = ActionArg::Abort)]
        action: ActionArg,
        #[arg(long)]
        target_branch: Option<String>,
        #[arg(long)]
        squash: bool,
        #[arg(long)]
        json: bool,
    },
    /// List active sandboxes and stale worktrees
    List {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Prune stale or orphaned worktree sandboxes
    Prune {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value_t = 86400)]
        max_age_secs: u64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ActionArg {
    Abort,
    Merge,
    Retain,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CacheArg {
    Cargo,
    Node,
    Python,
}

impl From<CacheArg> for CacheType {
    fn from(arg: CacheArg) -> Self {
        match arg {
            CacheArg::Cargo => Self::CargoTarget,
            CacheArg::Node => Self::NodeModules,
            CacheArg::Python => Self::PythonVenv,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum MultiplexerArg {
    None,
    Tmux,
    Herdr,
}

impl From<MultiplexerArg> for MultiplexerType {
    fn from(arg: MultiplexerArg) -> Self {
        match arg {
            MultiplexerArg::None => Self::None,
            MultiplexerArg::Tmux => Self::Tmux,
            MultiplexerArg::Herdr => Self::Herdr,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum AgentArg {
    Agy,
    ClaudeCode,
    Custom,
}

impl From<AgentArg> for AgentType {
    fn from(arg: AgentArg) -> Self {
        match arg {
            AgentArg::Agy => Self::Agy,
            AgentArg::ClaudeCode => Self::ClaudeCode,
            AgentArg::Custom => Self::Custom,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Provision {
            task_id,
            repo,
            base,
            cache,
            inject,
            json,
        } => {
            let orchestrator = WorktreeOrchestrator::new(&repo);
            let shared_caches: Vec<CacheType> = cache.into_iter().map(Into::into).collect();
            let mut req = WorkspaceProvisionRequest::new(&task_id, &repo)
                .with_caches(shared_caches)
                .with_injected_configs(inject);

            if let Some(base_branch) = base {
                req = req.with_base_branch(base_branch);
            }

            match orchestrator.provision(&req).await {
                Ok(handle) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&handle)?);
                    } else {
                        println!("Provisioned sandbox for task '{}':", handle.task_id);
                        println!("  Workspace Path: {}", handle.workspace_path.display());
                        println!("  Branch:         {}", handle.branch_name);
                        println!(
                            "  Base Branch:    {}",
                            handle.base_branch.as_deref().unwrap_or("HEAD")
                        );
                        if !handle.mounted_caches.is_empty() {
                            println!("  Mounted Caches: {:?}", handle.mounted_caches);
                        }
                        if !handle.injected_files.is_empty() {
                            println!("  Injected Files: {:?}", handle.injected_files);
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Error provisioning workspace: {}", err);
                    std::process::exit(1);
                }
            }
        }
        Commands::Route {
            envelope,
            repo,
            base,
            multiplexer,
            agent,
            cmd,
            cache,
            inject,
            spawn,
            json,
        } => {
            let envelope_content = if Path::new(&envelope).exists() {
                tokio::fs::read_to_string(&envelope).await?
            } else {
                envelope
            };

            let task_envelope: TaskEnvelope = match serde_json::from_str(&envelope_content) {
                Ok(env) => env,
                Err(e) => {
                    eprintln!("Failed to parse TaskEnvelope: {}", e);
                    std::process::exit(1);
                }
            };

            let shared_caches = if cache.is_empty() {
                vec![CacheType::CargoTarget, CacheType::NodeModules]
            } else {
                cache.into_iter().map(Into::into).collect()
            };

            let opts = RouterDispatchOptions {
                target_repo: repo,
                base_branch: base,
                multiplexer: multiplexer.into(),
                agent: agent.into(),
                custom_agent_cmd: cmd,
                shared_caches,
                injected_configs: inject,
                execute_multiplexer: spawn,
            };

            match WorkspaceRouterAdapter::dispatch(&task_envelope, opts).await {
                Ok(receipt) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    } else {
                        println!(
                            "Successfully routed TaskEnvelope '{}':",
                            receipt.envelope_id
                        );
                        println!(
                            "  Sandbox Path:        {}",
                            receipt.sandbox_handle.workspace_path.display()
                        );
                        println!(
                            "  Sandbox Branch:      {}",
                            receipt.sandbox_handle.branch_name
                        );
                        println!("  Multiplexer:         {:?}", receipt.multiplexer_type);
                        if let Some(cmd) = &receipt.multiplexer_command {
                            println!("  Multiplexer Command: {}", cmd);
                        }
                        println!("  Pane Spawned:        {}", receipt.pane_spawned);
                    }
                }
                Err(err) => {
                    eprintln!("Error routing TaskEnvelope: {}", err);
                    std::process::exit(1);
                }
            }
        }
        Commands::Teardown {
            task_id,
            repo,
            action,
            target_branch,
            squash,
            json,
        } => {
            let orchestrator = WorktreeOrchestrator::new(&repo);
            let teardown_action = match action {
                ActionArg::Abort => TeardownAction::Abort,
                ActionArg::Merge => {
                    let target = target_branch.unwrap_or_else(|| "main".to_string());
                    TeardownAction::Merge {
                        target_branch: target,
                        squash,
                    }
                }
                ActionArg::Retain => TeardownAction::Retain,
            };

            match orchestrator.teardown(&task_id, teardown_action).await {
                Ok(receipt) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    } else {
                        println!("Teardown completed for task '{}':", receipt.task_id);
                        println!("  Action Taken:   {}", receipt.action_taken);
                        println!("  Workspace Path: {}", receipt.workspace_path.display());
                        println!("  Branch Deleted: {}", receipt.branch_deleted);
                        if let Some(commit) = &receipt.merge_commit {
                            println!("  Merge Commit:   {}", commit);
                        }
                        if !receipt.unmounted_caches.is_empty() {
                            println!("  Unmounted Caches: {:?}", receipt.unmounted_caches);
                        }
                        println!("  Duration:       {}ms", receipt.duration_ms);
                    }
                }
                Err(err) => {
                    eprintln!("Error tearing down workspace: {}", err);
                    std::process::exit(1);
                }
            }
        }
        Commands::List { repo, json } => {
            let orchestrator = WorktreeOrchestrator::new(&repo);
            match orchestrator.list_sandboxes().await {
                Ok(sandboxes) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&sandboxes)?);
                    } else {
                        println!("Found {} worktree(s):", sandboxes.len());
                        for s in sandboxes {
                            let task_str = s.task_id.as_deref().unwrap_or("n/a");
                            let branch_str = s.branch.as_deref().unwrap_or("detached");
                            println!(
                                "  - [{}] {} (branch: {}, commit: {})",
                                task_str,
                                s.worktree_path.display(),
                                branch_str,
                                s.head_commit
                            );
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Error listing workspaces: {}", err);
                    std::process::exit(1);
                }
            }
        }
        Commands::Prune {
            repo,
            max_age_secs,
            json,
        } => {
            let orchestrator = WorktreeOrchestrator::new(&repo);
            match orchestrator.prune_stale(max_age_secs).await {
                Ok(report) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!("Pruned {} stale sandbox(es):", report.pruned_task_ids.len());
                        for id in &report.pruned_task_ids {
                            println!("  - Pruned task: {}", id);
                        }
                        if !report.errors.is_empty() {
                            eprintln!("Errors during prune:");
                            for err in &report.errors {
                                eprintln!("  - {}", err);
                            }
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Error pruning workspaces: {}", err);
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}
