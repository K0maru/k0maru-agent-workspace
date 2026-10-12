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

    std::fs::write(path.join("README.md"), "# CLI Test Repo\n").unwrap();
    run_cmd(&["add", "README.md"]);
    run_cmd(&["commit", "-m", "Initial commit on main"]);

    dir
}

#[test]
fn test_cli_help() {
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Git Worktree isolation and ephemeral sandbox lifecycle engine",
        ));
}

#[test]
fn test_cli_provision_and_lifecycle_json() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Provision with json
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "provision",
            "--task-id",
            "task_01",
            "--repo",
            repo_path,
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    assert!(json_str.contains("\"task-01\"") || json_str.contains("\"task_01\""));
    assert!(json_str.contains("\"is_isolated\": true"));

    // 2. List with json
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let list_output = cmd
        .args(["list", "--repo", repo_path, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let list_str = String::from_utf8(list_output).unwrap();
    assert!(list_str.contains("task_01") || list_str.contains("task-01"));

    // 3. Teardown with json
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let teardown_output = cmd
        .args([
            "teardown",
            "--task-id",
            "task_01",
            "--repo",
            repo_path,
            "--action",
            "abort",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let td_str = String::from_utf8(teardown_output).unwrap();
    assert!(td_str.contains("\"action_taken\": \"aborted\""));
    assert!(td_str.contains("\"branch_deleted\": true"));

    // 4. Prune with json
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.args(["prune", "--repo", repo_path, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"pruned_task_ids\""));
}

#[test]
fn test_cli_human_output_reproduction_snippet() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // Exact snippet from reproduction details:
    // k0maru-workspace provision --task-id task_01 --repo /path/to/repo
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.args(["provision", "--task-id", "task_01", "--repo", repo_path])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Provisioned sandbox for task 'task_01'",
        ))
        .stdout(predicate::str::contains("Workspace Path:"))
        .stdout(predicate::str::contains("Branch:"));
}
