use clap::{Parser, Subcommand, ValueEnum};
use k0maru_workspace::handle::{
    CacheType, IsolationStrategy, TeardownAction, WorkspaceProvisionRequest,
};
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
        #[arg(long, value_enum, default_value_t = StrategyArg::Worktree)]
        strategy: StrategyArg,
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
        /// Override isolation strategy (worktree, inplace, tempdir)
        #[arg(long, value_enum)]
        strategy: Option<StrategyArg>,
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
    /// Teardown and clean up an existing worktree sandbox.
    ///
    /// Note: Pre-merge quality verification and test oracle execution are delegated to `k0maru-verifier`.
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
        force: bool,
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
    /// Claim a TASK_READY tuple from Linda Tuple Space, provision a sandbox, and emit SANDBOX_READY
    #[command(name = "tuple-claim", alias = "tuple_claim")]
    TupleClaim {
        /// Target domain identifier (e.g. WORKSPACE, AUTH-SRV)
        #[arg(long)]
        domain: String,
        /// Optional specific task ID to claim
        #[arg(long)]
        task_id: Option<String>,
        /// Target repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Base branch to branch off
        #[arg(long)]
        base: Option<String>,
        /// Isolation strategy (worktree, inplace, tempdir)
        #[arg(long, value_enum, default_value_t = StrategyArg::Worktree)]
        strategy: StrategyArg,
        /// Shared caches to mount
        #[arg(long, value_enum)]
        cache: Vec<CacheArg>,
        /// Injected configuration files
        #[arg(long)]
        inject: Vec<String>,
        /// Override tuple space directory (defaults to $K0MARU_TUPLE_DIR or ~/.k0maru/tuples)
        #[arg(long)]
        tuple_dir: Option<PathBuf>,
        /// Output machine-readable JSON receipt
        #[arg(long)]
        json: bool,
    },
    /// Settle a task sandbox: abort if TASK_BLOCKED, or verify receipt, merge to target branch, teardown, and emit TASK_COMPLETED
    #[command(name = "tuple-settle", alias = "tuple_settle")]
    TupleSettle {
        /// Associated task identifier to settle
        #[arg(long)]
        task_id: String,
        /// Target repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Target branch to merge into
        #[arg(long, default_value = "dev")]
        target_branch: String,
        /// Squash merge into single commit
        #[arg(long)]
        squash: bool,
        /// Force emergency bypass (skips receipt & hash verification)
        #[arg(long)]
        force: bool,
        /// Override tuple space directory (defaults to $K0MARU_TUPLE_DIR or ~/.k0maru/tuples)
        #[arg(long)]
        tuple_dir: Option<PathBuf>,
        /// Output machine-readable JSON receipt
        #[arg(long)]
        json: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum StrategyArg {
    #[value(name = "worktree")]
    Worktree,
    #[value(name = "inplace", alias = "in-place")]
    InPlace,
    #[value(name = "tempdir", alias = "temp-dir")]
    TempDir,
}

impl From<StrategyArg> for IsolationStrategy {
    fn from(arg: StrategyArg) -> Self {
        match arg {
            StrategyArg::Worktree => Self::Worktree,
            StrategyArg::InPlace => Self::InPlace,
            StrategyArg::TempDir => Self::TempDir,
        }
    }
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
            strategy,
            cache,
            inject,
            json,
        } => {
            let orchestrator = WorktreeOrchestrator::new(&repo);
            let shared_caches: Vec<CacheType> = cache.into_iter().map(Into::into).collect();
            let mut req = WorkspaceProvisionRequest::new(&task_id, &repo)
                .with_strategy(strategy.into())
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
                        println!("  Strategy:       {:?}", handle.strategy);
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
            strategy,
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
                isolate_strategy: strategy.map(Into::into),
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
            force,
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
                        force,
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
                    let exit_code = match err {
                        k0maru_workspace::error::WorkspaceError::VerificationReceiptRequired {
                            ..
                        } => k0maru_workspace::receipt::EXIT_CODE_VERIFICATION_FAILED,
                        _ => 1,
                    };
                    std::process::exit(exit_code);
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
        Commands::TupleClaim {
            domain,
            task_id,
            repo,
            base,
            strategy,
            cache,
            inject,
            tuple_dir,
            json,
        } => {
            let opts = k0maru_workspace::tuple::TupleClaimOptions {
                domain,
                task_id,
                repo_root: repo,
                base_branch: base,
                strategy: strategy.into(),
                caches: cache.into_iter().map(Into::into).collect(),
                injected_configs: inject,
                tuple_dir,
            };
            match k0maru_workspace::tuple::tuple_claim(opts).await {
                Ok(receipt) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    } else {
                        println!(
                            "Successfully claimed task '{}' from domain '{}':",
                            receipt.sandbox.task_id, receipt.claimed_tuple.domain
                        );
                        println!(
                            "  Workspace Path: {}",
                            receipt.sandbox.workspace_path.display()
                        );
                        println!("  Branch:         {}", receipt.sandbox.branch_name);
                        println!("  Strategy:       {:?}", receipt.sandbox.strategy);
                        println!("  Claimed Tuple:  {}", receipt.claimed_tuple.id);
                        println!("  Emitted Tuple:  {}", receipt.emitted_tuple.id);
                    }
                }
                Err(err) => {
                    eprintln!("Error claiming tuple task: {}", err);
                    std::process::exit(1);
                }
            }
        }
        Commands::TupleSettle {
            task_id,
            repo,
            target_branch,
            squash,
            force,
            tuple_dir,
            json,
        } => {
            let opts = k0maru_workspace::tuple::TupleSettleOptions {
                task_id,
                repo_root: repo,
                target_branch,
                squash,
                force,
                tuple_dir,
            };
            match k0maru_workspace::tuple::tuple_settle(opts).await {
                Ok(receipt) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    } else {
                        println!(
                            "Settled task '{}' (action: {}):",
                            receipt.task_id, receipt.action
                        );
                        if let Some(target) = &receipt.target_branch {
                            println!("  Merged To:      {}", target);
                        }
                        if let Some(commit) = &receipt.teardown_receipt.merge_commit {
                            println!("  Merge Commit:   {}", commit);
                        }
                        println!(
                            "  Emitted Tuple:  {} (kind: {})",
                            receipt.emitted_tuple.id, receipt.emitted_tuple.kind
                        );
                        println!(
                            "  Duration:       {}ms",
                            receipt.teardown_receipt.duration_ms
                        );
                    }
                }
                Err(err) => {
                    eprintln!("Error settling tuple task: {}", err);
                    let exit_code = match err {
                        k0maru_workspace::error::WorkspaceError::VerificationReceiptRequired {
                            ..
                        } => k0maru_workspace::receipt::EXIT_CODE_VERIFICATION_FAILED,
                        _ => 1,
                    };
                    std::process::exit(exit_code);
                }
            }
        }
    }

    Ok(())
}
