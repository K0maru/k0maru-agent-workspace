use assert_cmd::Command;
use predicates::prelude::*;
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
    run_cmd(&["config", "user.name", "Tester"]);
    run_cmd(&["config", "user.email", "tester@example.com"]);

    std::fs::write(path.join("README.md"), "# Verifier Test Repo\n").unwrap();
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
fn test_cli_verify_help() {
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.args(["verify", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--task-id"))
        .stdout(predicate::str::contains("--test-cmd"))
        .stdout(predicate::str::contains("--build-cmd"))
        .stdout(predicate::str::contains("--linter-cmd"))
        .stdout(predicate::str::contains("--allowed-files"))
        .stdout(predicate::str::contains("--auto-merge"))
        .stdout(predicate::str::contains("--auto-rollback"))
        .stdout(predicate::str::contains("--target-branch"))
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn test_cli_verify_pass_and_json_receipt() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Provision sandbox
    let mut prov_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    prov_cmd
        .args([
            "provision",
            "--task-id",
            "task_v1",
            "--repo",
            repo_path,
            "--base",
            "dev",
        ])
        .assert()
        .success();

    let worktree_path = repo.path().join(".worktrees").join("task_v1");
    assert!(worktree_path.exists());

    // 2. Make modification in sandbox
    let feature_file = worktree_path.join("feature.rs");
    std::fs::write(&feature_file, "pub fn compute() -> i32 { 42 }\n").unwrap();

    // 3. Run verify with json output
    let mut verify_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = verify_cmd
        .args([
            "verify",
            "--task-id",
            "task_v1",
            "--repo",
            repo_path,
            "--build-cmd",
            "echo 'build ok'",
            "--test-cmd",
            "echo 'test ok'",
            "--linter-cmd",
            "echo 'linter ok'",
            "--allowed-files",
            "feature.rs",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    assert!(json_str.contains("\"$schema\": \"https://k0maru.dev/schemas/agent-receipt-v1.json\""));
    assert!(json_str.contains("\"verdict\": \"success\""));
    assert!(json_str.contains("\"status\": \"passed\""));
    assert!(json_str.contains("git_patch_sha256"));
    assert!(json_str.contains("feature.rs"));
}

#[test]
fn test_cli_verify_auto_merge() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Provision sandbox
    let mut prov_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    prov_cmd
        .args([
            "provision",
            "--task-id",
            "task_automerge",
            "--repo",
            repo_path,
            "--base",
            "dev",
        ])
        .assert()
        .success();

    let worktree_path = repo.path().join(".worktrees").join("task_automerge");
    assert!(worktree_path.exists());

    // 2. Make modification in sandbox
    let service_file = worktree_path.join("service.rs");
    std::fs::write(&service_file, "pub fn run_service() {}\n").unwrap();

    // 3. Verify with auto-merge into dev
    let mut verify_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    verify_cmd
        .args([
            "verify",
            "--task-id",
            "task_automerge",
            "--repo",
            repo_path,
            "--test-cmd",
            "echo 'unit tests pass'",
            "--auto-merge",
            "--target-branch",
            "dev",
            "--json",
        ])
        .assert()
        .success();

    // 4. Sandbox worktree should be removed and branch deleted
    assert!(!worktree_path.exists());

    // 5. File should exist on dev in repo_root
    let merged_service = repo.path().join("service.rs");
    assert!(merged_service.exists());
}

#[test]
fn test_cli_verify_auto_rollback_on_test_failure() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Provision sandbox
    let mut prov_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    prov_cmd
        .args([
            "provision",
            "--task-id",
            "task_autorollback",
            "--repo",
            repo_path,
            "--base",
            "dev",
        ])
        .assert()
        .success();

    let worktree_path = repo.path().join(".worktrees").join("task_autorollback");
    assert!(worktree_path.exists());

    // 2. Add bad file
    let bad_file = worktree_path.join("broken.rs");
    std::fs::write(&bad_file, "fn syntax_err()\n").unwrap();

    // 3. Verify with auto-rollback on fail
    let mut verify_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = verify_cmd
        .args([
            "verify",
            "--task-id",
            "task_autorollback",
            "--repo",
            repo_path,
            "--test-cmd",
            "sh -c 'echo \"tests failed\" >&2 && exit 1'",
            "--auto-rollback",
            "--json",
        ])
        .assert()
        .failure() // exit code 1 on verification failure
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    assert!(json_str.contains("\"verdict\": \"failed\""));

    // 4. Sandbox worktree should be rolled back and cleaned up
    assert!(!worktree_path.exists());
}

#[test]
fn test_cli_verify_scope_guard_violation_and_rollback() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Provision sandbox
    let mut prov_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    prov_cmd
        .args([
            "provision",
            "--task-id",
            "task_scope_guard",
            "--repo",
            repo_path,
            "--base",
            "dev",
        ])
        .assert()
        .success();

    let worktree_path = repo.path().join(".worktrees").join("task_scope_guard");

    // 2. Create unauthorized file
    let secret_file = worktree_path.join("secret.key");
    std::fs::write(&secret_file, "supersecret\n").unwrap();

    // 3. Verify with whitelist allowing only src/
    let mut verify_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    verify_cmd
        .args([
            "verify",
            "--task-id",
            "task_scope_guard",
            "--repo",
            repo_path,
            "--allowed-files",
            "src/",
            "--auto-rollback",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Scope Guard:"))
        .stdout(predicate::str::contains("FAILED"))
        .stdout(predicate::str::contains("unauthorized"));

    // 4. Rolled back
    assert!(!worktree_path.exists());
}

#[test]
fn test_cli_verify_human_output_reproduction() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Provision sandbox
    let mut prov_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    prov_cmd
        .args([
            "provision",
            "--task-id",
            "task_human",
            "--repo",
            repo_path,
            "--base",
            "dev",
        ])
        .assert()
        .success();

    // 2. Verify human output
    let mut verify_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    verify_cmd
        .args([
            "verify",
            "--task-id",
            "task_human",
            "--repo",
            repo_path,
            "--test-cmd",
            "echo 'unit tests passed'",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Verification receipt for task 'task_human':",
        ))
        .stdout(predicate::str::contains("Verdict:          SUCCESS"))
        .stdout(predicate::str::contains("Test Check:       PASSED"))
        .stdout(predicate::str::contains("Diff Summary:"));
}
