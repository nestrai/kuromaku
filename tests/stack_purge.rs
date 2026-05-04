//! Integration tests for `kuro stack purge <project>` (issue #232).
//!
//! Runs the binary in a sandboxed `HOME` so the per-project stack dir
//! lives under a tempdir. The binary derives the stack root from
//! `dirs::home_dir()`, which honours `$HOME` on Unix -- the same
//! sandbox technique `tests/graph_smoke.rs` already relies on.
//!
//! Three behaviours are pinned here:
//!   1. `--dry-run` reports what would be deleted and leaves disk alone.
//!   2. `--yes` deletes the project dir and exits zero.
//!   3. Without `--yes`, with no TTY attached (the assert_cmd default),
//!      the command refuses and exits non-zero -- the TTY safety net
//!      against accidental scripted erasure.
//!
//! Plus: rejection on invalid project names and "no such project" exit
//! code, so the error surface is contractually pinned alongside the
//! happy path.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// Build a fake project stack root under `<home>/.koto/stacks/<project>/`
/// containing one run with one step file. Returns the absolute project
/// directory so tests can assert on its presence/absence after the
/// purge call.
fn fake_project_under(home: &Path, project: &str) -> PathBuf {
    let project_dir = home.join(".koto").join("stacks").join(project);
    let steps_dir = project_dir.join("dev-20260501-100000").join("steps");
    std::fs::create_dir_all(&steps_dir).expect("create steps dir");
    std::fs::write(steps_dir.join("01-design.md"), "BODY").expect("write step content");
    project_dir
}

#[test]
fn dry_run_prints_summary_and_keeps_data() {
    let home = tempfile::tempdir().unwrap();
    let project_dir = fake_project_under(home.path(), "ikno");

    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .args(["stack", "purge", "ikno", "--dry-run"])
        .env("HOME", home.path())
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("dry-run"),
        "expected dry-run notice in stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("1 run"),
        "expected run-count summary in stderr, got: {stderr}"
    );

    // The project dir must still be there -- dry-run promised not to
    // touch disk.
    assert!(project_dir.is_dir(), "project dir must survive dry-run");
}

#[test]
fn yes_deletes_project_data() {
    let home = tempfile::tempdir().unwrap();
    let project_dir = fake_project_under(home.path(), "ikno");

    Command::cargo_bin("kuro")
        .unwrap()
        .args(["stack", "purge", "ikno", "--yes"])
        .env("HOME", home.path())
        .env_remove("RUST_LOG")
        .assert()
        .success();

    assert!(
        !project_dir.exists(),
        "project dir must be gone after --yes purge"
    );
    // Sibling projects are untouched -- the parent stacks dir stays.
    assert!(home.path().join(".koto/stacks").is_dir());
}

#[test]
fn without_yes_aborts_when_no_tty() {
    // assert_cmd does not attach a TTY to the spawned binary's stdin,
    // so this is the contract for "scripted use without --yes": the
    // command must refuse and exit non-zero. The user gets a clear
    // hint pointing at `--yes`.
    let home = tempfile::tempdir().unwrap();
    let project_dir = fake_project_under(home.path(), "ikno");

    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .args(["stack", "purge", "ikno"])
        .env("HOME", home.path())
        .env_remove("RUST_LOG")
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("--yes"),
        "expected hint about --yes in stderr, got: {stderr}"
    );

    // Failure path must not delete anything.
    assert!(
        project_dir.is_dir(),
        "project dir must survive aborted purge"
    );
}

#[test]
fn unknown_project_exits_nonzero_with_helpful_message() {
    let home = tempfile::tempdir().unwrap();
    // No project dir created -- the stacks tree may not even exist yet.

    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .args(["stack", "purge", "ghost", "--yes"])
        .env("HOME", home.path())
        .env_remove("RUST_LOG")
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("ghost"),
        "error must name the project, got: {stderr}"
    );
}

#[test]
fn rejects_invalid_project_names() {
    // `..` would let a caller climb out of the stack root if validation
    // was missing; this test pins the string-shape gate at the CLI
    // boundary.
    let home = tempfile::tempdir().unwrap();

    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .args(["stack", "purge", "..", "--yes"])
        .env("HOME", home.path())
        .env_remove("RUST_LOG")
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("invalid project name"),
        "expected validation error in stderr, got: {stderr}"
    );
}
