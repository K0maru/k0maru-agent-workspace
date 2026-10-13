use assert_cmd::Command as AssertCommand;
use k0maru_workspace::handle::IsolationStrategy;
use k0maru_workspace::receipt::{compute_sha256, MinimalExecutionReceipt, ReceiptArtifacts};
use k0maru_workspace::tuple::{
    tuple_claim, tuple_settle, Tuple, TupleClaimOptions, TuplePattern, TupleSettleOptions,
    TupleSpaceEngine, TUPLE_KIND_SANDBOX_READY, TUPLE_KIND_TASK_ABORTED, TUPLE_KIND_TASK_BLOCKED,
    TUPLE_KIND_TASK_COMPLETED, TUPLE_KIND_TASK_READY, TUPLE_KIND_TASK_VERIFIED,
};
use std::process::Command as StdCommand;
use tempfile::TempDir;

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
async fn test_tuple_claim_provisions_worktree_and_emits_sandbox_ready() {
    let repo = init_test_git_repo();
    let tuple_dir = TempDir::new().expect("create tuple dir");
    let engine = TupleSpaceEngine::new(tuple_dir.path());

    let task_id = "task-claim-01";
    let ready_tuple = Tuple::new(
        TUPLE_KIND_TASK_READY,
        "WORKSPACE",
        task_id,
        r#"{"intent":"feature","title":"Implement tuple claim"}"#,
    );
    engine.out(ready_tuple).expect("emit ready tuple");

    let opts = TupleClaimOptions {
        domain: "WORKSPACE".to_string(),
        task_id: Some(task_id.to_string()),
        repo_root: repo.path().to_path_buf(),
        base_branch: Some("dev".to_string()),
        strategy: IsolationStrategy::Worktree,
        caches: vec![],
        injected_configs: vec![],
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };

    let receipt = tuple_claim(opts).await.expect("tuple claim succeeds");

    // 1. Verify claimed tuple was removed from TASK_READY
    let pat_ready = TuplePattern::new()
        .with_kind(TUPLE_KIND_TASK_READY)
        .with_task_id(task_id);
    assert!(engine.rd(&pat_ready).unwrap().is_none());

    // 2. Verify sandbox handle
    assert_eq!(receipt.sandbox.task_id, task_id);
    assert!(receipt.sandbox.workspace_path.exists());
    assert!(receipt.sandbox.is_isolated);

    // 3. Verify SANDBOX_READY tuple was emitted into tuple space
    let pat_sandbox = TuplePattern::new()
        .with_kind(TUPLE_KIND_SANDBOX_READY)
        .with_task_id(task_id);
    let sandbox_tuple = engine
        .rd(&pat_sandbox)
        .unwrap()
        .expect("sandbox ready tuple");
    assert_eq!(sandbox_tuple.domain, "WORKSPACE");
    assert!(sandbox_tuple.payload.contains("workspace_path"));
    assert!(sandbox_tuple.payload.contains(task_id));
}

#[tokio::test]
async fn test_tuple_settle_verified_merge_and_teardown() {
    let repo = init_test_git_repo();
    let tuple_dir = TempDir::new().expect("create tuple dir");
    let engine = TupleSpaceEngine::new(tuple_dir.path());

    let task_id = "task-settle-02";
    let ready_tuple = Tuple::new(
        TUPLE_KIND_TASK_READY,
        "WORKSPACE",
        task_id,
        r#"{"intent":"bugfix"}"#,
    );
    engine.out(ready_tuple).expect("emit ready");

    // 1. Claim
    let claim_opts = TupleClaimOptions {
        domain: "WORKSPACE".to_string(),
        task_id: Some(task_id.to_string()),
        repo_root: repo.path().to_path_buf(),
        base_branch: Some("dev".to_string()),
        strategy: IsolationStrategy::Worktree,
        caches: vec![],
        injected_configs: vec![],
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };
    let claim_receipt = tuple_claim(claim_opts).await.expect("claim");
    let sandbox_path = claim_receipt.sandbox.workspace_path;

    // 2. Make modification in sandbox
    let new_file = sandbox_path.join("feature.rs");
    std::fs::write(&new_file, "pub fn feature() -> bool { true }\n").unwrap();

    let run_cmd = |args: &[&str]| {
        let status = StdCommand::new("git")
            .current_dir(&sandbox_path)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success());
    };
    run_cmd(&["add", "feature.rs"]);
    run_cmd(&["commit", "-m", "feat: add feature in sandbox"]);

    // Compute diff against dev
    let diff_out = StdCommand::new("git")
        .current_dir(&sandbox_path)
        .args(["diff", "dev"])
        .output()
        .expect("git diff");
    let patch_sha = compute_sha256(&diff_out.stdout);

    // 3. Write passing execution receipt to .worktrees/.receipts/<task_id>.json
    let receipts_dir = repo.path().join(".worktrees").join(".receipts");
    std::fs::create_dir_all(&receipts_dir).unwrap();
    let receipt_obj = MinimalExecutionReceipt {
        schema: Some("https://k0maru.dev/schemas/agent-receipt-v1.json".into()),
        schema_version: Some("v1".into()),
        task_id: Some(task_id.into()),
        verdict: "success".into(),
        artifacts: Some(ReceiptArtifacts {
            git_patch_sha256: Some(patch_sha),
        }),
    };
    std::fs::write(
        receipts_dir.join(format!("{}.json", task_id)),
        serde_json::to_string_pretty(&receipt_obj).unwrap(),
    )
    .unwrap();

    // 4. Emit TASK_VERIFIED tuple
    let verified_tuple = Tuple::new(
        TUPLE_KIND_TASK_VERIFIED,
        "WORKSPACE",
        task_id,
        r#"{"verdict":"success"}"#,
    );
    engine.out(verified_tuple).expect("emit verified");

    // 5. Settle
    let settle_opts = TupleSettleOptions {
        task_id: task_id.to_string(),
        repo_root: repo.path().to_path_buf(),
        target_branch: "dev".to_string(),
        squash: true,
        force: false,
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };
    let settle_receipt = tuple_settle(settle_opts).await.expect("settle succeeds");

    assert_eq!(settle_receipt.action, "merged");
    assert!(!sandbox_path.exists()); // Worktree was cleaned up

    // 6. Verify dev branch now contains feature.rs
    let cat_status = StdCommand::new("git")
        .current_dir(repo.path())
        .args(["show", "dev:feature.rs"])
        .output()
        .expect("git show");
    assert!(cat_status.status.success());

    // 7. Verify TASK_COMPLETED tuple exists
    let pat_completed = TuplePattern::new()
        .with_kind(TUPLE_KIND_TASK_COMPLETED)
        .with_task_id(task_id);
    let completed_tuple = engine.rd(&pat_completed).unwrap().expect("completed tuple");
    assert!(completed_tuple.payload.contains("\"status\":\"PASS\""));
    assert!(completed_tuple.payload.contains("merged_to"));
}

#[tokio::test]
async fn test_tuple_settle_fail_safe_abort_on_task_blocked() {
    let repo = init_test_git_repo();
    let tuple_dir = TempDir::new().expect("create tuple dir");
    let engine = TupleSpaceEngine::new(tuple_dir.path());

    let task_id = "task-blocked-03";
    let ready_tuple = Tuple::new(
        TUPLE_KIND_TASK_READY,
        "WORKSPACE",
        task_id,
        r#"{"intent":"bugfix"}"#,
    );
    engine.out(ready_tuple).expect("emit ready");

    // 1. Claim
    let claim_opts = TupleClaimOptions {
        domain: "WORKSPACE".to_string(),
        task_id: Some(task_id.to_string()),
        repo_root: repo.path().to_path_buf(),
        base_branch: Some("dev".to_string()),
        strategy: IsolationStrategy::Worktree,
        caches: vec![],
        injected_configs: vec![],
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };
    let claim_receipt = tuple_claim(claim_opts).await.expect("claim");
    let sandbox_path = claim_receipt.sandbox.workspace_path;

    // 2. Put dirty/broken edits in sandbox
    std::fs::write(sandbox_path.join("broken.rs"), "syntax error;\n").unwrap();

    // 3. Emit TASK_BLOCKED tuple
    let blocked_tuple = Tuple::new(
        TUPLE_KIND_TASK_BLOCKED,
        "WORKSPACE",
        task_id,
        r#"{"reason":"ScopeGuard security invariant violation: unauthorized file modification"}"#,
    );
    engine.out(blocked_tuple).expect("emit blocked");

    // 4. Settle triggers fail-safe abort
    let settle_opts = TupleSettleOptions {
        task_id: task_id.to_string(),
        repo_root: repo.path().to_path_buf(),
        target_branch: "dev".to_string(),
        squash: false,
        force: false,
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };
    let settle_receipt = tuple_settle(settle_opts)
        .await
        .expect("settle aborts safely");

    assert_eq!(settle_receipt.action, "aborted");
    assert!(!sandbox_path.exists()); // Worktree deleted

    // 5. Verify TASK_ABORTED tuple exists
    let pat_aborted = TuplePattern::new()
        .with_kind(TUPLE_KIND_TASK_ABORTED)
        .with_task_id(task_id);
    let aborted_tuple = engine.rd(&pat_aborted).unwrap().expect("aborted tuple");
    assert!(aborted_tuple.payload.contains("\"status\":\"ABORTED\""));

    // 6. Zero dirty files in main repo (ZERO_DIRTY_MAIN invariant)
    let git_status = StdCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    assert!(git_status.stdout.is_empty(), "main repo must remain clean");
}

#[tokio::test]
async fn test_tuple_settle_blocked_when_receipt_missing() {
    let repo = init_test_git_repo();
    let tuple_dir = TempDir::new().expect("create tuple dir");
    let engine = TupleSpaceEngine::new(tuple_dir.path());

    let task_id = "task-no-receipt-04";
    let ready_tuple = Tuple::new(
        TUPLE_KIND_TASK_READY,
        "WORKSPACE",
        task_id,
        r#"{"intent":"bugfix"}"#,
    );
    engine.out(ready_tuple).expect("emit ready");

    // Claim
    let claim_opts = TupleClaimOptions {
        domain: "WORKSPACE".to_string(),
        task_id: Some(task_id.to_string()),
        repo_root: repo.path().to_path_buf(),
        base_branch: Some("dev".to_string()),
        strategy: IsolationStrategy::Worktree,
        caches: vec![],
        injected_configs: vec![],
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };
    let _ = tuple_claim(claim_opts).await.expect("claim");

    // Settle without receipt and without force should fail
    let settle_opts = TupleSettleOptions {
        task_id: task_id.to_string(),
        repo_root: repo.path().to_path_buf(),
        target_branch: "dev".to_string(),
        squash: false,
        force: false,
        tuple_dir: Some(tuple_dir.path().to_path_buf()),
    };
    let res = tuple_settle(settle_opts).await;
    assert!(res.is_err());
    let err_str = res.unwrap_err().to_string();
    assert!(err_str.contains("Verification receipt required"));
}

#[tokio::test]
async fn test_cli_tuple_claim_and_settle() {
    let repo = init_test_git_repo();
    let tuple_dir = TempDir::new().expect("create tuple dir");
    let engine = TupleSpaceEngine::new(tuple_dir.path());

    let task_id = "task-cli-05";
    let ready_tuple = Tuple::new(
        TUPLE_KIND_TASK_READY,
        "WORKSPACE",
        task_id,
        r#"{"intent":"feature"}"#,
    );
    engine.out(ready_tuple).expect("emit ready");

    // 1. Run CLI tuple-claim
    let mut cmd = AssertCommand::cargo_bin("k0maru-workspace").expect("binary");
    cmd.args([
        "tuple-claim",
        "--domain",
        "WORKSPACE",
        "--task-id",
        task_id,
        "--repo",
        repo.path().to_str().unwrap(),
        "--base",
        "dev",
        "--tuple-dir",
        tuple_dir.path().to_str().unwrap(),
        "--json",
    ]);
    cmd.assert().success();

    let worktree_path = repo.path().join(".worktrees").join(task_id);
    assert!(worktree_path.exists());

    // 2. Modify and commit in sandbox
    let run_cmd = |args: &[&str]| {
        let status = StdCommand::new("git")
            .current_dir(&worktree_path)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success());
    };
    std::fs::write(worktree_path.join("cli_feat.rs"), "// test\n").unwrap();
    run_cmd(&["add", "cli_feat.rs"]);
    run_cmd(&["commit", "-m", "feat: cli commit"]);

    let diff_out = StdCommand::new("git")
        .current_dir(&worktree_path)
        .args(["diff", "dev"])
        .output()
        .expect("git diff");
    let patch_sha = compute_sha256(&diff_out.stdout);

    // 3. Write receipt
    let receipts_dir = repo.path().join(".worktrees").join(".receipts");
    std::fs::create_dir_all(&receipts_dir).unwrap();
    let receipt_obj = MinimalExecutionReceipt {
        schema: Some("https://k0maru.dev/schemas/agent-receipt-v1.json".into()),
        schema_version: Some("v1".into()),
        task_id: Some(task_id.into()),
        verdict: "success".into(),
        artifacts: Some(ReceiptArtifacts {
            git_patch_sha256: Some(patch_sha),
        }),
    };
    std::fs::write(
        receipts_dir.join(format!("{}.json", task_id)),
        serde_json::to_string_pretty(&receipt_obj).unwrap(),
    )
    .unwrap();

    // 4. Emit TASK_VERIFIED
    let verified_tuple = Tuple::new(
        TUPLE_KIND_TASK_VERIFIED,
        "WORKSPACE",
        task_id,
        r#"{"verdict":"success"}"#,
    );
    engine.out(verified_tuple).expect("emit verified");

    // 5. Run CLI tuple-settle
    let mut settle_cmd = AssertCommand::cargo_bin("k0maru-workspace").expect("binary");
    settle_cmd.args([
        "tuple-settle",
        "--task-id",
        task_id,
        "--repo",
        repo.path().to_str().unwrap(),
        "--target-branch",
        "dev",
        "--tuple-dir",
        tuple_dir.path().to_str().unwrap(),
        "--json",
    ]);
    settle_cmd.assert().success();

    assert!(!worktree_path.exists());
}
