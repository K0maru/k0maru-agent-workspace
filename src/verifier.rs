//! Deterministic verifier integration, execution receipts, and automated merge gate.

use crate::error::WorkspaceError;
use crate::handle::{TeardownAction, TeardownReceipt};
use crate::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;

/// JSON schema URL for agent receipts.
pub const AGENT_RECEIPT_SCHEMA: &str = "https://k0maru.dev/schemas/agent-receipt-v1.json";

/// Overall verdict of the verification pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Success,
    Failed,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Success => write!(f, "SUCCESS"),
            Verdict::Failed => write!(f, "FAILED"),
        }
    }
}

/// Status of an individual check within the verification pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Skipped,
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckStatus::Passed => write!(f, "PASSED"),
            CheckStatus::Failed => write!(f, "FAILED"),
            CheckStatus::Skipped => write!(f, "SKIPPED"),
        }
    }
}

/// Result of executing an external verification command (build, test, or linter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCheckResult {
    pub status: CheckStatus,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
}

/// Result of scope guard validating modified files against authorized whitelist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeGuardResult {
    pub status: CheckStatus,
    pub modified_files: Vec<String>,
    pub unauthorized_files: Vec<String>,
}

/// Collection of all checks executed during verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChecksReceipt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<CommandCheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<CommandCheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linter: Option<CommandCheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_guard: Option<ScopeGuardResult>,
}

/// Artifacts generated during sandbox execution, including git patch digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactsReceipt {
    pub git_patch_sha256: Option<String>,
    pub git_diff_summary: String,
    pub receipt_timestamp: String,
}

/// Immutable execution receipt conforming to agent-receipt-v1.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub task_id: String,
    pub workspace: PathBuf,
    pub verdict: Verdict,
    pub checks: ChecksReceipt,
    pub artifacts: ArtifactsReceipt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Configuration options for the deterministic workspace verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierConfig {
    pub build_command: Option<String>,
    pub test_command: Option<String>,
    pub linter_command: Option<String>,
    #[serde(default)]
    pub allowed_files: Vec<String>,
    #[serde(default)]
    pub auto_merge_on_pass: bool,
    #[serde(default)]
    pub auto_rollback_on_fail: bool,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub squash: bool,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            build_command: None,
            test_command: None,
            linter_command: None,
            allowed_files: Vec::new(),
            auto_merge_on_pass: false,
            auto_rollback_on_fail: false,
            target_branch: None,
            squash: true,
        }
    }
}

impl VerifierConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_build_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.build_command = Some(cmd.into());
        self
    }

    pub fn with_test_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.test_command = Some(cmd.into());
        self
    }

    pub fn with_linter_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.linter_command = Some(cmd.into());
        self
    }

    pub fn with_allowed_files(mut self, files: Vec<String>) -> Self {
        self.allowed_files = files;
        self
    }

    pub fn with_auto_merge(mut self, auto_merge: bool) -> Self {
        self.auto_merge_on_pass = auto_merge;
        self
    }

    pub fn with_auto_rollback(mut self, auto_rollback: bool) -> Self {
        self.auto_rollback_on_fail = auto_rollback;
        self
    }

    pub fn with_target_branch(mut self, branch: impl Into<String>) -> Self {
        self.target_branch = Some(branch.into());
        self
    }

    pub fn with_squash(mut self, squash: bool) -> Self {
        self.squash = squash;
        self
    }
}

/// Workspace verifier engine executing physical checks in sandbox CWD.
pub struct WorkspaceVerifier;

impl WorkspaceVerifier {
    /// Executes the full verification pipeline inside the task sandbox,
    /// generating an execution receipt and optionally triggering auto-merge or rollback.
    pub async fn verify_sandbox(
        repo_root: &Path,
        task_id: &str,
        config: &VerifierConfig,
        orchestrator: &WorktreeOrchestrator,
    ) -> Result<(ExecutionReceipt, Option<TeardownReceipt>), WorkspaceError> {
        WorktreeOrchestrator::validate_task_id(task_id)?;

        let workspace_path = orchestrator.task_worktree_path(task_id);
        if !workspace_path.exists() {
            return Err(WorkspaceError::NotFound {
                task_id: task_id.to_string(),
            });
        }

        // Determine base branch / ref for diff and scope guard
        let base_ref =
            Self::resolve_base_ref(repo_root, config.target_branch.as_deref(), orchestrator).await;

        let mut checks = ChecksReceipt::default();
        let mut has_failure = false;
        let mut failure_messages: Vec<String> = Vec::new();

        // 1. Build Command Check
        if let Some(ref build_cmd) = config.build_command {
            let res = Self::run_command(&workspace_path, build_cmd).await;
            if res.status == CheckStatus::Failed {
                has_failure = true;
                failure_messages.push(format!("Build check failed: {}", res.stderr.trim()));
            }
            checks.build = Some(res);
        }

        // 2. Test Command Check
        if let Some(ref test_cmd) = config.test_command {
            if has_failure {
                checks.test = Some(CommandCheckResult {
                    status: CheckStatus::Skipped,
                    command: test_cmd.clone(),
                    exit_code: None,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: "Skipped due to prior check failure".to_string(),
                });
            } else {
                let res = Self::run_command(&workspace_path, test_cmd).await;
                if res.status == CheckStatus::Failed {
                    has_failure = true;
                    failure_messages.push(format!("Test check failed: {}", res.stderr.trim()));
                }
                checks.test = Some(res);
            }
        }

        // 3. Linter Command Check
        if let Some(ref linter_cmd) = config.linter_command {
            if has_failure {
                checks.linter = Some(CommandCheckResult {
                    status: CheckStatus::Skipped,
                    command: linter_cmd.clone(),
                    exit_code: None,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: "Skipped due to prior check failure".to_string(),
                });
            } else {
                let res = Self::run_command(&workspace_path, linter_cmd).await;
                if res.status == CheckStatus::Failed {
                    has_failure = true;
                    failure_messages.push(format!("Linter check failed: {}", res.stderr.trim()));
                }
                checks.linter = Some(res);
            }
        }

        // 4. Scope Guard Check (if allowed_files whitelist is provided)
        if !config.allowed_files.is_empty() {
            let modified = Self::detect_modified_files(&workspace_path, &base_ref).await?;
            let mut unauthorized = Vec::new();
            for file in &modified {
                let is_allowed = config
                    .allowed_files
                    .iter()
                    .any(|allowed| Self::file_matches_allowed(file, allowed));
                if !is_allowed {
                    unauthorized.push(file.clone());
                }
            }

            let status = if unauthorized.is_empty() {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };

            if status == CheckStatus::Failed {
                has_failure = true;
                failure_messages.push(format!(
                    "Scope guard failed: unauthorized files modified: {:?}",
                    unauthorized
                ));
            }

            checks.scope_guard = Some(ScopeGuardResult {
                status,
                modified_files: modified,
                unauthorized_files: unauthorized,
            });
        }

        // Compute artifacts (git diff, SHA-256 patch hash, diff summary)
        let artifacts = Self::compute_artifacts(&workspace_path, &base_ref).await;

        let verdict = if has_failure {
            Verdict::Failed
        } else {
            Verdict::Success
        };

        let error_message = if failure_messages.is_empty() {
            None
        } else {
            Some(failure_messages.join("; "))
        };

        let receipt = ExecutionReceipt {
            schema: AGENT_RECEIPT_SCHEMA.to_string(),
            task_id: task_id.to_string(),
            workspace: workspace_path.clone(),
            verdict,
            checks,
            artifacts,
            error_message,
        };

        // Automated lifecycle transitions based on verdict
        let mut teardown_receipt = None;
        match verdict {
            Verdict::Success => {
                if config.auto_merge_on_pass {
                    // Commit any uncommitted / untracked changes before merging
                    let status_out = Command::new("git")
                        .current_dir(&workspace_path)
                        .args(["status", "--porcelain"])
                        .output()
                        .await;

                    if let Ok(out) = status_out {
                        if !out.stdout.is_empty() {
                            let _ = Command::new("git")
                                .current_dir(&workspace_path)
                                .args(["add", "-A"])
                                .output()
                                .await;
                            let commit_msg = format!("feat({}): verified sandbox changes", task_id);
                            let _ = Command::new("git")
                                .current_dir(&workspace_path)
                                .args(["commit", "-m", &commit_msg])
                                .output()
                                .await;
                        }
                    }

                    let target_branch = if let Some(ref b) = config.target_branch {
                        b.clone()
                    } else if orchestrator.branch_exists("dev").await {
                        "dev".to_string()
                    } else {
                        "main".to_string()
                    };

                    let action = TeardownAction::Merge {
                        target_branch,
                        squash: config.squash,
                    };
                    let td = orchestrator.teardown(task_id, action).await?;
                    teardown_receipt = Some(td);
                }
            }
            Verdict::Failed => {
                if config.auto_rollback_on_fail {
                    let td = orchestrator
                        .teardown(task_id, TeardownAction::Abort)
                        .await?;
                    teardown_receipt = Some(td);
                }
            }
        }

        Ok((receipt, teardown_receipt))
    }

    /// Resolves target or base reference for diff comparison.
    async fn resolve_base_ref(
        _repo_root: &Path,
        config_target: Option<&str>,
        orchestrator: &WorktreeOrchestrator,
    ) -> String {
        if let Some(target) = config_target {
            return target.to_string();
        }
        if orchestrator.branch_exists("dev").await {
            return "dev".to_string();
        }
        if orchestrator.branch_exists("main").await {
            return "main".to_string();
        }
        "HEAD".to_string()
    }

    /// Runs a shell command inside the specified working directory, capturing output and duration.
    pub async fn run_command(cwd: &Path, command_str: &str) -> CommandCheckResult {
        let start = Instant::now();
        let res = Command::new("sh")
            .arg("-c")
            .arg(command_str)
            .current_dir(cwd)
            .output()
            .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match res {
            Ok(output) => {
                let exit_code = output.status.code();
                let status = if output.status.success() {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                };
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                CommandCheckResult {
                    status,
                    command: command_str.to_string(),
                    exit_code,
                    duration_ms,
                    stdout,
                    stderr,
                }
            }
            Err(e) => CommandCheckResult {
                status: CheckStatus::Failed,
                command: command_str.to_string(),
                exit_code: None,
                duration_ms,
                stdout: String::new(),
                stderr: format!("Failed to execute command: {}", e),
            },
        }
    }

    /// Detects all modified, staged, and untracked files relative to the base branch.
    pub async fn detect_modified_files(
        workspace_path: &Path,
        base_ref: &str,
    ) -> Result<Vec<String>, WorkspaceError> {
        let mut modified_set = BTreeSet::new();

        // 1. git status --porcelain for working tree and untracked files
        let status_output = Command::new("git")
            .current_dir(workspace_path)
            .args(["status", "--porcelain"])
            .output()
            .await
            .map_err(WorkspaceError::IoError)?;

        let status_str = String::from_utf8_lossy(&status_output.stdout);
        for line in status_str.lines() {
            let line = line.trim_end();
            if line.len() >= 3 {
                let path_part = line[3..].trim().trim_matches('"');
                if let Some((_old, new)) = path_part.split_once(" -> ") {
                    modified_set.insert(new.trim().trim_matches('"').to_string());
                } else {
                    modified_set.insert(path_part.to_string());
                }
            }
        }

        // 2. git diff --name-only <base_ref> for committed changes on feature branch
        let diff_output = Command::new("git")
            .current_dir(workspace_path)
            .args(["diff", "--name-only", base_ref])
            .output()
            .await;

        if let Ok(out) = diff_output {
            if out.status.success() {
                let diff_str = String::from_utf8_lossy(&out.stdout);
                for line in diff_str.lines() {
                    let trimmed = line.trim().trim_matches('"');
                    if !trimmed.is_empty() {
                        modified_set.insert(trimmed.to_string());
                    }
                }
            }
        }

        Ok(modified_set.into_iter().collect())
    }

    /// Checks if a file path matches an allowed pattern (exact, prefix, wildcard).
    pub fn file_matches_allowed(file: &str, allowed: &str) -> bool {
        let f = file.trim().strip_prefix("./").unwrap_or(file.trim());
        let a = allowed.trim().strip_prefix("./").unwrap_or(allowed.trim());

        if a == "*" || a == "**" || f == a {
            return true;
        }

        if a.ends_with('/') && f.starts_with(a) {
            return true;
        }

        if f.starts_with(&format!("{}/", a)) {
            return true;
        }

        if let Some(suffix) = a.strip_prefix("*.") {
            if f.ends_with(&format!(".{}", suffix)) {
                return true;
            }
        }

        if let Some(prefix) = a.strip_suffix("/*") {
            if f.starts_with(&format!("{}/", prefix)) {
                return true;
            }
        }

        false
    }

    /// Computes git diff patch and SHA-256 digest.
    pub async fn compute_artifacts(workspace_path: &Path, base_ref: &str) -> ArtifactsReceipt {
        // Stage intent-to-add so untracked files appear in git diff
        let _ = Command::new("git")
            .current_dir(workspace_path)
            .args(["add", "-N", "."])
            .output()
            .await;

        // Try diff against base_ref
        let diff_res = Command::new("git")
            .current_dir(workspace_path)
            .args(["diff", base_ref])
            .output()
            .await;

        let (patch, stat) = if let Ok(out) = diff_res {
            if out.status.success() {
                let patch = String::from_utf8_lossy(&out.stdout).to_string();
                let stat_out = Command::new("git")
                    .current_dir(workspace_path)
                    .args(["diff", "--stat", base_ref])
                    .output()
                    .await;
                let stat = stat_out
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                (patch, stat)
            } else {
                Self::diff_fallback(workspace_path).await
            }
        } else {
            Self::diff_fallback(workspace_path).await
        };

        let git_patch_sha256 = if !patch.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(patch.as_bytes());
            Some(hex::encode(hasher.finalize()))
        } else {
            None
        };

        let git_diff_summary = if stat.is_empty() {
            "0 files changed".to_string()
        } else {
            stat
        };

        ArtifactsReceipt {
            git_patch_sha256,
            git_diff_summary,
            receipt_timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Fallback diff against HEAD if base branch diff is not available.
    async fn diff_fallback(workspace_path: &Path) -> (String, String) {
        let diff_out = Command::new("git")
            .current_dir(workspace_path)
            .args(["diff", "HEAD"])
            .output()
            .await;
        let patch = diff_out
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let stat_out = Command::new("git")
            .current_dir(workspace_path)
            .args(["diff", "--stat", "HEAD"])
            .output()
            .await;
        let stat = stat_out
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        (patch, stat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{IsolationStrategy, WorkspaceProvisionRequest};
    use crate::orchestrator::WorkspaceManager;
    use tempfile::TempDir;

    fn init_test_git_repo() -> TempDir {
        let dir = TempDir::new().expect("create temp dir");
        let path = dir.path();

        let run_cmd = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(path)
                .args(args)
                .status()
                .expect("run git command");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_cmd(&["init", "-b", "main"]);
        run_cmd(&["config", "user.name", "Tester"]);
        run_cmd(&["config", "user.email", "tester@example.com"]);

        std::fs::write(path.join("README.md"), "# Test Repo\n").unwrap();
        run_cmd(&["add", "README.md"]);
        run_cmd(&["commit", "-m", "Initial commit on main"]);

        // Create dev branch
        run_cmd(&["checkout", "-b", "dev"]);
        std::fs::write(path.join("DEV.md"), "# Dev Branch\n").unwrap();
        run_cmd(&["add", "DEV.md"]);
        run_cmd(&["commit", "-m", "Add dev doc"]);

        dir
    }

    #[test]
    fn test_serde_execution_receipt() {
        let receipt = ExecutionReceipt {
            schema: AGENT_RECEIPT_SCHEMA.to_string(),
            task_id: "task-01".to_string(),
            workspace: PathBuf::from("/tmp/repo/.worktrees/task-01"),
            verdict: Verdict::Success,
            checks: ChecksReceipt {
                build: Some(CommandCheckResult {
                    status: CheckStatus::Passed,
                    command: "cargo check".to_string(),
                    exit_code: Some(0),
                    duration_ms: 120,
                    stdout: "ok".to_string(),
                    stderr: "".to_string(),
                }),
                test: Some(CommandCheckResult {
                    status: CheckStatus::Passed,
                    command: "cargo test".to_string(),
                    exit_code: Some(0),
                    duration_ms: 250,
                    stdout: "test result: ok".to_string(),
                    stderr: "".to_string(),
                }),
                linter: None,
                scope_guard: Some(ScopeGuardResult {
                    status: CheckStatus::Passed,
                    modified_files: vec!["src/lib.rs".to_string()],
                    unauthorized_files: Vec::new(),
                }),
            },
            artifacts: ArtifactsReceipt {
                git_patch_sha256: Some("abcdef123456".to_string()),
                git_diff_summary: "1 file changed, 5 insertions(+)".to_string(),
                receipt_timestamp: "2026-10-12T00:00:00Z".to_string(),
            },
            error_message: None,
        };

        let json = serde_json::to_string_pretty(&receipt).expect("serialize");
        assert!(json.contains("\"$schema\": \"https://k0maru.dev/schemas/agent-receipt-v1.json\""));
        assert!(json.contains("\"verdict\": \"success\""));
        assert!(json.contains("\"status\": \"passed\""));

        let deserialized: ExecutionReceipt = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(receipt, deserialized);
    }

    #[test]
    fn test_verifier_config_builder() {
        let config = VerifierConfig::new()
            .with_build_cmd("cargo check")
            .with_test_cmd("cargo test")
            .with_linter_cmd("cargo clippy")
            .with_allowed_files(vec!["src/".into(), "Cargo.toml".into()])
            .with_auto_merge(true)
            .with_auto_rollback(true)
            .with_target_branch("dev")
            .with_squash(true);

        assert_eq!(config.build_command.as_deref(), Some("cargo check"));
        assert_eq!(config.test_command.as_deref(), Some("cargo test"));
        assert_eq!(config.linter_command.as_deref(), Some("cargo clippy"));
        assert_eq!(config.allowed_files.len(), 2);
        assert!(config.auto_merge_on_pass);
        assert!(config.auto_rollback_on_fail);
        assert_eq!(config.target_branch.as_deref(), Some("dev"));
        assert!(config.squash);
    }

    #[test]
    fn test_file_matches_allowed() {
        assert!(WorkspaceVerifier::file_matches_allowed(
            "src/lib.rs",
            "src/lib.rs"
        ));
        assert!(WorkspaceVerifier::file_matches_allowed(
            "./src/lib.rs",
            "src/lib.rs"
        ));
        assert!(WorkspaceVerifier::file_matches_allowed(
            "src/lib.rs",
            "src/"
        ));
        assert!(WorkspaceVerifier::file_matches_allowed(
            "src/sub/mod.rs",
            "src"
        ));
        assert!(WorkspaceVerifier::file_matches_allowed(
            "src/lib.rs",
            "*.rs"
        ));
        assert!(WorkspaceVerifier::file_matches_allowed(
            "docs/readme.md",
            "*"
        ));
        assert!(!WorkspaceVerifier::file_matches_allowed(
            "secret.key",
            "src/"
        ));
        assert!(!WorkspaceVerifier::file_matches_allowed(
            "secret.key",
            "*.rs"
        ));
    }

    #[tokio::test]
    async fn test_run_command_success_and_failure() {
        let temp = TempDir::new().unwrap();
        let pass_res = WorkspaceVerifier::run_command(temp.path(), "echo hello").await;
        assert_eq!(pass_res.status, CheckStatus::Passed);
        assert_eq!(pass_res.exit_code, Some(0));
        assert!(pass_res.stdout.contains("hello"));

        let fail_res = WorkspaceVerifier::run_command(temp.path(), "sh -c 'exit 42'").await;
        assert_eq!(fail_res.status, CheckStatus::Failed);
        assert_eq!(fail_res.exit_code, Some(42));
    }

    #[tokio::test]
    async fn test_verify_sandbox_pass_no_auto_merge() {
        let repo = init_test_git_repo();
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-verify-1", repo.path())
            .with_base_branch("dev")
            .with_strategy(IsolationStrategy::Worktree);
        let handle = orchestrator.provision(&req).await.expect("provision");

        // Create a new file in the sandbox
        let new_file = handle.workspace_path.join("hello.txt");
        std::fs::write(&new_file, "hello world\n").unwrap();

        let config = VerifierConfig::new()
            .with_test_cmd("echo 'test ok'")
            .with_allowed_files(vec!["hello.txt".into()])
            .with_auto_merge(false);

        let (receipt, td) =
            WorkspaceVerifier::verify_sandbox(repo.path(), "task-verify-1", &config, &orchestrator)
                .await
                .expect("verify");

        assert_eq!(receipt.verdict, Verdict::Success);
        assert!(receipt.error_message.is_none());
        assert!(receipt.checks.test.is_some());
        assert_eq!(
            receipt.checks.test.as_ref().unwrap().status,
            CheckStatus::Passed
        );
        assert!(receipt.checks.scope_guard.is_some());
        assert_eq!(
            receipt.checks.scope_guard.as_ref().unwrap().status,
            CheckStatus::Passed
        );
        assert!(receipt.artifacts.git_patch_sha256.is_some());
        assert!(td.is_none());
        assert!(handle.workspace_path.exists());
    }

    #[tokio::test]
    async fn test_verify_sandbox_pass_with_auto_merge() {
        let repo = init_test_git_repo();
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-verify-merge", repo.path())
            .with_base_branch("dev")
            .with_strategy(IsolationStrategy::Worktree);
        let handle = orchestrator.provision(&req).await.expect("provision");

        let code_file = handle.workspace_path.join("feature.txt");
        std::fs::write(&code_file, "cool feature\n").unwrap();

        let config = VerifierConfig::new()
            .with_test_cmd("echo 'all passed'")
            .with_auto_merge(true)
            .with_target_branch("dev");

        let (receipt, td) = WorkspaceVerifier::verify_sandbox(
            repo.path(),
            "task-verify-merge",
            &config,
            &orchestrator,
        )
        .await
        .expect("verify");

        assert_eq!(receipt.verdict, Verdict::Success);
        let td_receipt = td.expect("teardown receipt");
        assert_eq!(td_receipt.action_taken, "merged");
        assert!(td_receipt.branch_deleted);
        assert!(!handle.workspace_path.exists());

        // Verify merged content on dev
        let dev_feature_file = repo.path().join("feature.txt");
        assert!(dev_feature_file.exists());
    }

    #[tokio::test]
    async fn test_verify_sandbox_fail_with_auto_rollback() {
        let repo = init_test_git_repo();
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-verify-rollback", repo.path())
            .with_base_branch("dev")
            .with_strategy(IsolationStrategy::Worktree);
        let handle = orchestrator.provision(&req).await.expect("provision");

        let bad_file = handle.workspace_path.join("bad.txt");
        std::fs::write(&bad_file, "bad code\n").unwrap();

        let config = VerifierConfig::new()
            .with_test_cmd("sh -c 'echo \"syntax error\" >&2 && exit 1'")
            .with_auto_rollback(true);

        let (receipt, td) = WorkspaceVerifier::verify_sandbox(
            repo.path(),
            "task-verify-rollback",
            &config,
            &orchestrator,
        )
        .await
        .expect("verify");

        assert_eq!(receipt.verdict, Verdict::Failed);
        assert!(receipt.error_message.is_some());
        let td_receipt = td.expect("teardown receipt");
        assert_eq!(td_receipt.action_taken, "aborted");
        assert!(!handle.workspace_path.exists());
    }

    #[tokio::test]
    async fn test_verify_sandbox_scope_guard_rejection() {
        let repo = init_test_git_repo();
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-scope-fail", repo.path())
            .with_base_branch("dev")
            .with_strategy(IsolationStrategy::Worktree);
        let handle = orchestrator.provision(&req).await.expect("provision");

        let forbidden = handle.workspace_path.join("forbidden.key");
        std::fs::write(&forbidden, "secret\n").unwrap();

        let config = VerifierConfig::new()
            .with_allowed_files(vec!["src/".into()])
            .with_auto_rollback(false);

        let (receipt, _td) = WorkspaceVerifier::verify_sandbox(
            repo.path(),
            "task-scope-fail",
            &config,
            &orchestrator,
        )
        .await
        .expect("verify");

        assert_eq!(receipt.verdict, Verdict::Failed);
        let scope = receipt.checks.scope_guard.expect("scope guard ran");
        assert_eq!(scope.status, CheckStatus::Failed);
        assert!(scope
            .unauthorized_files
            .contains(&"forbidden.key".to_string()));
    }

    #[tokio::test]
    async fn test_verify_sandbox_skipped_after_build_failure() {
        let repo = init_test_git_repo();
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-skip", repo.path())
            .with_base_branch("dev")
            .with_strategy(IsolationStrategy::Worktree);
        let _handle = orchestrator.provision(&req).await.expect("provision");

        let config = VerifierConfig::new()
            .with_build_cmd("sh -c 'exit 1'")
            .with_test_cmd("echo 'never run'")
            .with_linter_cmd("echo 'never run'");

        let (receipt, _) =
            WorkspaceVerifier::verify_sandbox(repo.path(), "task-skip", &config, &orchestrator)
                .await
                .expect("verify");

        assert_eq!(receipt.verdict, Verdict::Failed);
        assert_eq!(
            receipt.checks.build.as_ref().unwrap().status,
            CheckStatus::Failed
        );
        assert_eq!(
            receipt.checks.test.as_ref().unwrap().status,
            CheckStatus::Skipped
        );
        assert_eq!(
            receipt.checks.linter.as_ref().unwrap().status,
            CheckStatus::Skipped
        );
    }
}
