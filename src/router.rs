use crate::error::WorkspaceError;
use crate::handle::{CacheType, IsolationStrategy, SandboxHandle, WorkspaceProvisionRequest};
use crate::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Caller metadata conforming to agent-task-v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerInfo {
    pub workspace: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// Target destination metadata conforming to agent-task-v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub is_external: bool,
}

/// Task content payload conforming to agent-task-v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPayload {
    pub title: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
}

/// TaskEnvelope protocol contract conforming to agent-task-v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEnvelope {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub id: String,
    pub created_at: String,
    pub caller: CallerInfo,
    pub target: TargetInfo,
    pub intent: String,
    pub payload: TaskPayload,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Terminal Multiplexer Target
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MultiplexerType {
    #[default]
    None,
    Tmux,
    Herdr,
}

/// Agent binary type to launch in the isolated pane
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    #[default]
    Agy,
    ClaudeCode,
    Custom,
}

/// Options controlling how the TaskEnvelope is dispatched and provisioned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterDispatchOptions {
    pub target_repo: Option<PathBuf>,
    pub base_branch: Option<String>,
    pub multiplexer: MultiplexerType,
    pub agent: AgentType,
    pub custom_agent_cmd: Option<String>,
    pub shared_caches: Vec<CacheType>,
    pub injected_configs: Vec<String>,
    pub execute_multiplexer: bool,
    #[serde(default)]
    pub isolate_strategy: Option<IsolationStrategy>,
}

impl Default for RouterDispatchOptions {
    fn default() -> Self {
        Self {
            target_repo: None,
            base_branch: None,
            multiplexer: MultiplexerType::None,
            agent: AgentType::Agy,
            custom_agent_cmd: None,
            shared_caches: vec![CacheType::CargoTarget, CacheType::NodeModules],
            injected_configs: Vec::new(),
            execute_multiplexer: false,
            isolate_strategy: None,
        }
    }
}

/// Proof of task envelope dispatch, sandbox provision, and multiplexer binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterDispatchReceipt {
    pub task_id: String,
    pub envelope_id: String,
    pub sandbox_handle: SandboxHandle,
    pub multiplexer_type: MultiplexerType,
    pub multiplexer_command: Option<String>,
    pub initial_prompt: String,
    pub pane_spawned: bool,
}

pub struct WorkspaceRouterAdapter;

impl WorkspaceRouterAdapter {
    /// Formats the prompt delivered to the agent inside the newly spawned worktree.
    pub fn build_agent_prompt(envelope: &TaskEnvelope) -> String {
        let mut prompt = format!(
            "[K0maru Task Dispatch] {} (Severity: {}, Intent: {})\nTitle: {}\nCaller: {} in {}\n",
            envelope.id,
            envelope.payload.severity,
            envelope.intent,
            envelope.payload.title,
            envelope.caller.agent,
            envelope.caller.workspace
        );

        if let Some(expected) = &envelope.payload.expected {
            prompt.push_str(&format!("\nExpected Behavior:\n{}\n", expected));
        }

        if let Some(repro) = &envelope.payload.reproduction {
            prompt.push_str("\nReproduction Details:\n");
            if let Some(obj) = repro.as_object() {
                if let Some(snippet) = obj.get("snippet").and_then(|v| v.as_str()) {
                    prompt.push_str(&format!("Snippet:\n```\n{}\n```\n", snippet));
                }
                if let Some(error) = obj.get("error").and_then(|v| v.as_str()) {
                    prompt.push_str(&format!("Error:\n```\n{}\n```\n", error));
                }
            } else {
                prompt.push_str(&repro.to_string());
                prompt.push('\n');
            }
        }

        prompt.push_str("\nPlease investigate and resolve this issue in the current workspace.");
        prompt
    }

    /// Synthesizes the shell command to split/spawn a pane into the isolated sandbox.
    pub fn build_multiplexer_command(
        multiplexer: MultiplexerType,
        agent: AgentType,
        custom_cmd: Option<&str>,
        sandbox_path: &Path,
        prompt: &str,
    ) -> Option<String> {
        let path_str = sandbox_path.to_string_lossy();
        let agent_cmd = match agent {
            AgentType::Agy => format!("agy \"{}\"", prompt.replace('"', "\\\"")),
            AgentType::ClaudeCode => format!("claude \"{}\"", prompt.replace('"', "\\\"")),
            AgentType::Custom => custom_cmd.unwrap_or("sh").to_string(),
        };

        match multiplexer {
            MultiplexerType::None => None,
            MultiplexerType::Tmux => Some(format!(
                "tmux split-window -c \"{}\" '{}'",
                path_str, agent_cmd
            )),
            MultiplexerType::Herdr => Some(format!(
                "herdr split --cwd \"{}\" --cmd '{}'",
                path_str, agent_cmd
            )),
        }
    }

    /// Dispatches a TaskEnvelope: provisions an isolated worktree and prepares the multiplexer pane.
    pub async fn dispatch(
        envelope: &TaskEnvelope,
        opts: RouterDispatchOptions,
    ) -> Result<RouterDispatchReceipt, WorkspaceError> {
        let repo_root = opts
            .target_repo
            .or_else(|| envelope.target.workspace.as_ref().map(PathBuf::from))
            .ok_or_else(|| {
                WorkspaceError::InvalidPath(
                    "Target repository root must be specified in envelope.target.workspace or dispatch options"
                        .into(),
                )
            })?;

        let task_id = envelope.id.clone();
        let orchestrator = WorktreeOrchestrator::new(&repo_root);

        let isolate_strategy = match opts.isolate_strategy {
            Some(strategy) => strategy,
            None => {
                let is_inplace = envelope.tags.iter().any(|t| {
                    let s = t.to_ascii_lowercase();
                    s == "read_only" || s == "in_place" || s == "query"
                });
                if is_inplace {
                    IsolationStrategy::InPlace
                } else {
                    IsolationStrategy::Worktree
                }
            }
        };

        let mut req = WorkspaceProvisionRequest::new(&task_id, &repo_root)
            .with_strategy(isolate_strategy)
            .with_caches(opts.shared_caches)
            .with_injected_configs(opts.injected_configs);

        if let Some(base) = opts.base_branch {
            req = req.with_base_branch(base);
        }

        let sandbox_handle = orchestrator.provision(&req).await?;
        let initial_prompt = Self::build_agent_prompt(envelope);

        let multiplexer_cmd = Self::build_multiplexer_command(
            opts.multiplexer,
            opts.agent,
            opts.custom_agent_cmd.as_deref(),
            &sandbox_handle.workspace_path,
            &initial_prompt,
        );

        let mut pane_spawned = false;
        if opts.execute_multiplexer && opts.multiplexer != MultiplexerType::None {
            if let Some(ref cmd_str) = multiplexer_cmd {
                let status = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd_str)
                    .status()
                    .await;
                if let Ok(st) = status {
                    pane_spawned = st.success();
                }
            }
        }

        Ok(RouterDispatchReceipt {
            task_id,
            envelope_id: envelope.id.clone(),
            sandbox_handle,
            multiplexer_type: opts.multiplexer,
            multiplexer_command: multiplexer_cmd,
            initial_prompt,
            pane_spawned,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_envelope_serde_and_prompt_generation() {
        let json = r#"{
            "$schema": "https://k0maru.dev/schemas/agent-task-v1.json",
            "id": "task_ws_bootstrap_01",
            "created_at": "2026-10-12T13:10:40Z",
            "caller": {
                "workspace": "/Users/k0maru3/Documents/SecondBrain",
                "agent": "antigravity-meta-harness"
            },
            "target": {
                "identifier": "k0maru-agent-workspace",
                "workspace": "/Users/k0maru3/workspace/k0maru-agent-workspace",
                "is_external": false
            },
            "intent": "bootstrap_and_implement_phase_1",
            "payload": {
                "title": "Bootstrap k0maru-agent-workspace",
                "severity": "high",
                "expected": "Implement WorktreeOrchestrator core",
                "reproduction": {
                    "snippet": "k0maru-workspace provision",
                    "error": "Not implemented"
                }
            }
        }"#;

        let envelope: TaskEnvelope = serde_json::from_str(json).expect("deserialize envelope");
        assert_eq!(envelope.id, "task_ws_bootstrap_01");
        assert_eq!(envelope.payload.severity, "high");

        let prompt = WorkspaceRouterAdapter::build_agent_prompt(&envelope);
        assert!(prompt.contains("[K0maru Task Dispatch] task_ws_bootstrap_01"));
        assert!(prompt.contains("Title: Bootstrap k0maru-agent-workspace"));
        assert!(prompt.contains("Expected Behavior:\nImplement WorktreeOrchestrator core"));
        assert!(prompt.contains("Snippet:\n```\nk0maru-workspace provision\n```"));
    }

    #[test]
    fn test_build_multiplexer_command() {
        let sandbox_path = Path::new("/path/to/repo/.worktrees/task-1");
        let prompt = "Fix this bug";

        let tmux_cmd = WorkspaceRouterAdapter::build_multiplexer_command(
            MultiplexerType::Tmux,
            AgentType::Agy,
            None,
            sandbox_path,
            prompt,
        );
        assert!(tmux_cmd.unwrap().starts_with(
            "tmux split-window -c \"/path/to/repo/.worktrees/task-1\" 'agy \"Fix this bug\"'"
        ));

        let herdr_cmd = WorkspaceRouterAdapter::build_multiplexer_command(
            MultiplexerType::Herdr,
            AgentType::ClaudeCode,
            None,
            sandbox_path,
            prompt,
        );
        assert!(herdr_cmd.unwrap().starts_with(
            "herdr split --cwd \"/path/to/repo/.worktrees/task-1\" --cmd 'claude \"Fix this bug\"'"
        ));
    }

    #[tokio::test]
    async fn test_router_dispatch_strategy_auto_deduction() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();

        tokio::process::Command::new("git")
            .current_dir(path)
            .args(["init", "-b", "main"])
            .output()
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .current_dir(path)
            .args(["config", "user.name", "Tester"])
            .output()
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .current_dir(path)
            .args(["config", "user.email", "tester@example.com"])
            .output()
            .await
            .unwrap();

        tokio::fs::write(path.join("README.md"), "# Router Test\n")
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .current_dir(path)
            .args(["add", "."])
            .output()
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .current_dir(path)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        // 1. Tag "read_only" -> auto-deduces InPlace
        let mut envelope = TaskEnvelope {
            schema: None,
            id: "task_ro_01".into(),
            created_at: "2026-10-12T00:00:00Z".into(),
            caller: CallerInfo {
                workspace: "test".into(),
                agent: "test".into(),
                pane_id: None,
            },
            target: TargetInfo {
                identifier: "test".into(),
                workspace: Some(path.to_str().unwrap().into()),
                is_external: false,
            },
            intent: "query".into(),
            payload: TaskPayload {
                title: "Inspect code".into(),
                severity: "low".into(),
                reproduction: None,
                expected: None,
            },
            tags: vec!["read_only".into()],
        };

        let opts = RouterDispatchOptions {
            target_repo: Some(path.to_path_buf()),
            ..Default::default()
        };

        let receipt = WorkspaceRouterAdapter::dispatch(&envelope, opts.clone())
            .await
            .expect("dispatch read_only");
        assert_eq!(receipt.sandbox_handle.strategy, IsolationStrategy::InPlace);
        assert_eq!(receipt.sandbox_handle.workspace_path, path);
        assert!(!receipt.sandbox_handle.is_isolated);

        // Teardown
        let orchestrator = WorktreeOrchestrator::new(path);
        orchestrator
            .teardown("task_ro_01", crate::handle::TeardownAction::Abort)
            .await
            .unwrap();

        // 2. Tag "in_place" -> auto-deduces InPlace
        envelope.id = "task_ip_01".into();
        envelope.tags = vec!["in_place".into()];
        let receipt_ip = WorkspaceRouterAdapter::dispatch(&envelope, opts.clone())
            .await
            .expect("dispatch in_place");
        assert_eq!(
            receipt_ip.sandbox_handle.strategy,
            IsolationStrategy::InPlace
        );

        orchestrator
            .teardown("task_ip_01", crate::handle::TeardownAction::Abort)
            .await
            .unwrap();

        // 3. Tag "query" -> auto-deduces InPlace
        envelope.id = "task_query_01".into();
        envelope.tags = vec!["query".into()];
        let receipt_q = WorkspaceRouterAdapter::dispatch(&envelope, opts.clone())
            .await
            .expect("dispatch query");
        assert_eq!(
            receipt_q.sandbox_handle.strategy,
            IsolationStrategy::InPlace
        );

        orchestrator
            .teardown("task_query_01", crate::handle::TeardownAction::Abort)
            .await
            .unwrap();

        // 4. No tags -> defaults to Worktree
        envelope.id = "task_wt_01".into();
        envelope.tags = Vec::new();
        let receipt_wt = WorkspaceRouterAdapter::dispatch(&envelope, opts.clone())
            .await
            .expect("dispatch default");
        assert_eq!(
            receipt_wt.sandbox_handle.strategy,
            IsolationStrategy::Worktree
        );
        assert!(receipt_wt.sandbox_handle.is_isolated);

        orchestrator
            .teardown("task_wt_01", crate::handle::TeardownAction::Abort)
            .await
            .unwrap();

        // 5. Explicit isolate_strategy overrides tags
        envelope.id = "task_override_01".into();
        envelope.tags = vec!["read_only".into()];
        let mut opts_override = opts;
        opts_override.isolate_strategy = Some(IsolationStrategy::TempDir);
        let receipt_ov = WorkspaceRouterAdapter::dispatch(&envelope, opts_override)
            .await
            .expect("dispatch override");
        assert_eq!(
            receipt_ov.sandbox_handle.strategy,
            IsolationStrategy::TempDir
        );

        orchestrator
            .teardown("task_override_01", crate::handle::TeardownAction::Abort)
            .await
            .unwrap();
    }
}
