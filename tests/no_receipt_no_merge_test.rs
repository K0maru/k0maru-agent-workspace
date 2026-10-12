use assert_cmd::Command as AssertCommand;
use k0maru_workspace::error::WorkspaceError;
use k0maru_workspace::handle::{IsolationStrategy, TeardownAction, WorkspaceProvisionRequest};
use k0maru_workspace::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use k0maru_workspace::receipt::{
    compute_sha256, MinimalExecutionReceipt, ReceiptArtifacts, ENV_K0MARU_RECEIPT_DIR,
    EXIT_CODE_VERIFICATION_FAILED,
};
use predicates::prelude::*;
use std::process::Command as StdCommand;
use tempfile::TempDir;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn init_test_git_repo() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path();

    let run_cmd = |args: &[&str]| {
        let status = StdCommand::new("git")
            .current_dir(path)
            .args(args)
            .status()
            .expect("run git command");
        assert!(status.success(), "git {:?} failed", args);
    };

    run_cmd(&["init", "-b", "main"]);
    run_cmd(&["config", "user.name", "K0maru Tester"]);
    run_cmd(&["config", "user.email", "tester@k0maru.dev"]);

    std::fs::write(path.join("README.md"), "# Test Repo\n").unwrap();
    run_cmd(&["add", "README.md"]);
    run_cmd(&["commit", "-m", "Initial commit on main"]);

    // Create dev branch
    run_cmd(&["checkout", "-b", "dev"]);
    std::fs::write(path.join("DEV.md"), "# Dev Base\n").unwrap();
    run_cmd(&["add", "DEV.md"]);
    run_cmd(&["commit", "-m", "Dev branch initial commit"]);
    run_cmd(&["checkout", "main"]);

    dir
}

#[tokio::test]
async fn test_merge_blocked_when_receipt_missing() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-no-receipt-01";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");
    assert!(handle.workspace_path.exists());

    // Agent introduces changes
    let feature_file = handle.workspace_path.join("feature_no_receipt.rs");
    std::fs::write(&feature_file, "pub fn unverified() {}\n").unwrap();

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "feature_no_receipt.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "feat: unverified commit"])
        .status()
        .unwrap();
    assert!(status.success());

    // Attempt teardown merge WITHOUT receipt
    let err = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: false,
            },
        )
        .await
        .unwrap_err();

    match &err {
        WorkspaceError::VerificationReceiptRequired { reason } => {
            assert!(
                reason.contains("No verification receipt found"),
                "Unexpected reason: {}",
                reason
            );
        }
        other => panic!("Expected VerificationReceiptRequired, got {:?}", other),
    }

    // Physical Invariant: The worktree must remain intact!
    assert!(handle.workspace_path.exists());
    assert!(handle.workspace_path.join("feature_no_receipt.rs").exists());

    // Main repo dev branch must NOT contain the feature
    assert!(!repo.path().join("feature_no_receipt.rs").exists());
}

#[tokio::test]
async fn test_merge_blocked_when_receipt_verdict_failed() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-verdict-fail-02";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");

    // Commit changes
    let file = handle.workspace_path.join("broken.rs");
    std::fs::write(&file, "pub fn broken() {}\n").unwrap();

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "broken.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "feat: broken implementation"])
        .status()
        .unwrap();
    assert!(status.success());

    // Compute actual diff SHA
    let diff_output = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["diff", "dev"])
        .output()
        .unwrap();
    let actual_sha = compute_sha256(&diff_output.stdout);

    // Write receipt with verdict = "failed"
    let failed_receipt = MinimalExecutionReceipt {
        schema: None,
        schema_version: Some("v1".into()),
        task_id: Some(task_id.into()),
        verdict: "failed".into(),
        artifacts: Some(ReceiptArtifacts {
            git_patch_sha256: Some(actual_sha),
        }),
    };

    let receipts_dir = repo.path().join(".receipts");
    std::fs::create_dir_all(&receipts_dir).unwrap();
    std::fs::write(
        receipts_dir.join(format!("{}.json", task_id)),
        serde_json::to_string_pretty(&failed_receipt).unwrap(),
    )
    .unwrap();

    let err = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: false,
            },
        )
        .await
        .unwrap_err();

    match &err {
        WorkspaceError::VerificationReceiptRequired { reason } => {
            assert!(
                reason.contains("verdict is 'failed'"),
                "Unexpected reason: {}",
                reason
            );
        }
        other => panic!("Expected VerificationReceiptRequired, got {:?}", other),
    }

    // Worktree preserved
    assert!(handle.workspace_path.exists());
    assert!(!repo.path().join("broken.rs").exists());
}

#[tokio::test]
async fn test_merge_blocked_when_receipt_sha256_mismatch() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-sha-mismatch-03";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");

    // Commit changes
    let file = handle.workspace_path.join("tampered.rs");
    std::fs::write(&file, "pub fn tampered() {}\n").unwrap();

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "tampered.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "feat: tampered"])
        .status()
        .unwrap();
    assert!(status.success());

    // Write receipt with bogus sha256
    let bogus_sha = "0000000000000000000000000000000000000000000000000000000000000000";
    let receipt = MinimalExecutionReceipt::new_success(task_id, bogus_sha);

    let receipt_path = repo.path().join(format!("{}.receipt.json", task_id));
    std::fs::write(
        &receipt_path,
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();

    let err = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: false,
            },
        )
        .await
        .unwrap_err();

    match &err {
        WorkspaceError::VerificationReceiptRequired { reason } => {
            assert!(
                reason.contains("Git patch SHA-256 mismatch"),
                "Unexpected reason: {}",
                reason
            );
        }
        other => panic!("Expected VerificationReceiptRequired, got {:?}", other),
    }

    assert!(handle.workspace_path.exists());
    assert!(!repo.path().join("tampered.rs").exists());
}

#[tokio::test]
async fn test_merge_succeeds_when_valid_receipt_present() {
    let _guard = ENV_LOCK.lock().await;

    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-valid-receipt-04";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");

    // Commit changes
    let file = handle.workspace_path.join("verified_feature.rs");
    std::fs::write(
        &file,
        "pub fn verified() -> &'static str { \"verified\" }\n",
    )
    .unwrap();

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "verified_feature.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "feat: verified feature"])
        .status()
        .unwrap();
    assert!(status.success());

    // Compute actual diff against target_branch (dev)
    let diff_output = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["diff", "dev"])
        .output()
        .unwrap();
    assert!(diff_output.status.success());
    let actual_sha = compute_sha256(&diff_output.stdout);

    // Place receipt in temporary K0MARU_RECEIPT_DIR
    let receipts_temp = TempDir::new().unwrap();
    std::env::set_var(ENV_K0MARU_RECEIPT_DIR, receipts_temp.path());

    let receipt = MinimalExecutionReceipt::new_success(task_id, &actual_sha);
    let receipt_file = receipts_temp.path().join(format!("{}.json", task_id));
    std::fs::write(
        &receipt_file,
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();

    // Teardown with Merge without force -> should succeed!
    let teardown_receipt = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: false,
            },
        )
        .await
        .expect("teardown merge should succeed with valid receipt");

    assert_eq!(teardown_receipt.action_taken, "merged");
    assert!(teardown_receipt.branch_deleted);
    assert!(teardown_receipt.merge_commit.is_some());
    assert!(!handle.workspace_path.exists());

    // Switch repo to dev and verify file exists
    let status = StdCommand::new("git")
        .current_dir(repo.path())
        .args(["checkout", "dev"])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(repo.path().join("verified_feature.rs").exists());

    // Clean up env
    std::env::remove_var(ENV_K0MARU_RECEIPT_DIR);
}

#[tokio::test]
async fn test_merge_succeeds_with_force_bypass_when_receipt_missing() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-force-bypass-05";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");

    // Commit changes
    let file = handle.workspace_path.join("emergency_hotfix.rs");
    std::fs::write(&file, "pub fn emergency() {}\n").unwrap();

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "emergency_hotfix.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "hotfix: emergency bypass"])
        .status()
        .unwrap();
    assert!(status.success());

    // No receipt is created. Teardown with force = true
    let teardown_receipt = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: true,
            },
        )
        .await
        .expect("teardown merge must succeed with --force");

    assert_eq!(teardown_receipt.action_taken, "merged");
    assert!(teardown_receipt.branch_deleted);
    assert!(!handle.workspace_path.exists());

    // Switch repo to dev and verify file exists
    let status = StdCommand::new("git")
        .current_dir(repo.path())
        .args(["checkout", "dev"])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(repo.path().join("emergency_hotfix.rs").exists());
}

#[test]
fn test_cli_teardown_blocks_merge_without_receipt_and_exits_code_1() {
    let repo = init_test_git_repo();
    let task_id = "task-cli-no-receipt";

    // Provision via CLI
    let mut cmd = AssertCommand::cargo_bin("k0maru-workspace").unwrap();
    cmd.args([
        "provision",
        "--task-id",
        task_id,
        "--repo",
        repo.path().to_str().unwrap(),
        "--base",
        "dev",
    ])
    .assert()
    .success();

    // Teardown with merge and without force
    let mut teardown_cmd = AssertCommand::cargo_bin("k0maru-workspace").unwrap();
    teardown_cmd
        .args([
            "teardown",
            "--task-id",
            task_id,
            "--repo",
            repo.path().to_str().unwrap(),
            "--action",
            "merge",
            "--target-branch",
            "dev",
        ])
        .assert()
        .code(EXIT_CODE_VERIFICATION_FAILED)
        .stderr(predicate::str::contains("Verification receipt required"));

    // Teardown with merge and WITH --force bypass
    let mut force_cmd = AssertCommand::cargo_bin("k0maru-workspace").unwrap();
    force_cmd
        .args([
            "teardown",
            "--task-id",
            task_id,
            "--repo",
            repo.path().to_str().unwrap(),
            "--action",
            "merge",
            "--target-branch",
            "dev",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Teardown completed"));
}

#[tokio::test]
async fn test_inplace_strategy_merge_receipt_checks() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-inplace-receipt-06";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_strategy(IsolationStrategy::InPlace);

    orchestrator
        .provision(&req)
        .await
        .expect("provision inplace");

    // Attempt merge without force and without receipt -> error
    let err = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: false,
                force: false,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        WorkspaceError::VerificationReceiptRequired { .. }
    ));

    // Teardown with force = true -> succeeds
    let receipt = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: false,
                force: true,
            },
        )
        .await
        .expect("inplace teardown with force succeeds");

    assert_eq!(
        receipt.action_taken,
        "in-place workspace completed without deletion"
    );
}

#[tokio::test]
async fn test_corrupt_receipt_json_blocked() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-corrupt-receipt-07";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");

    // Write invalid JSON file
    let receipt_path = repo.path().join(format!("{}.json", task_id));
    std::fs::write(&receipt_path, "{ broken_json: ").unwrap();

    let err = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: false,
            },
        )
        .await
        .unwrap_err();

    match &err {
        WorkspaceError::VerificationReceiptRequired { reason } => {
            assert!(
                reason.contains("Failed to parse receipt JSON"),
                "Unexpected reason: {}",
                reason
            );
        }
        other => panic!("Expected VerificationReceiptRequired, got {:?}", other),
    }

    assert!(handle.workspace_path.exists());
}

#[tokio::test]
async fn test_receipt_located_in_worktrees_receipts_folder() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());
    let task_id = "task-wt-receipt-08";

    let req = WorkspaceProvisionRequest::new(task_id, repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");

    let file = handle.workspace_path.join("feature.rs");
    std::fs::write(&file, "pub fn feature() {}\n").unwrap();

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "feature.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "feat: feature"])
        .status()
        .unwrap();
    assert!(status.success());

    let diff_output = StdCommand::new("git")
        .current_dir(&handle.workspace_path)
        .args(["diff", "dev"])
        .output()
        .unwrap();
    let actual_sha = compute_sha256(&diff_output.stdout);

    // Place receipt inside <repo>/.worktrees/.receipts/<task_id>.json
    let wt_receipts_dir = repo.path().join(".worktrees").join(".receipts");
    std::fs::create_dir_all(&wt_receipts_dir).unwrap();
    let receipt = MinimalExecutionReceipt::new_success(task_id, &actual_sha);
    std::fs::write(
        wt_receipts_dir.join(format!("{}.json", task_id)),
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();

    let teardown_receipt = orchestrator
        .teardown(
            task_id,
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
                force: false,
            },
        )
        .await
        .expect("teardown with receipt in .worktrees/.receipts should succeed");

    assert_eq!(teardown_receipt.action_taken, "merged");
    assert!(teardown_receipt.branch_deleted);
}
