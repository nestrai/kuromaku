//! Integration tests for `kuro init` (issue #385).
//!
//! Runs the binary in a tempdir cwd. Backend detection is pinned by
//! setting `PATH` explicitly (empty = no backend found) and removing the
//! executor env overrides, so the tests do not depend on what happens to
//! be installed on the host.
//!
//! Acceptance criteria pinned here:
//!   1. empty dir: exit 0, exactly the five scaffold files, nothing else
//!   2. `kuro context` right after init resolves the generated setup
//!   4. `Cargo.toml` present -> templates name Rust; no marker -> generic
//!   5. existing `.kuro/` -> non-zero exit, named on stderr, untouched
//!   6. no backend CLI on PATH -> exit 0 plus warning naming all three
//!
//! Criterion 3 (`kuro run hello` with a live backend) needs an installed
//! backend CLI and is verified manually before the PR -- CI has none.

#![cfg(unix)]

use std::path::Path;

use assert_cmd::Command;

/// The exact set of files `kuro init` promises to create, relative to the
/// target directory. Sorted -- comparison sites sort their walk results.
const EXPECTED_FILES: [&str; 5] = [
    ".kuro/agents/Developer.yaml",
    ".kuro/agents/Reviewer.yaml",
    ".kuro/config.yaml",
    ".kuro/flows/hello.yaml",
    ".kuro/rules/project-conventions.md",
];

/// `kuro init` command with pinned backend-detection env: `PATH` set to
/// `path` and all executor overrides removed.
fn init_cmd(dir: &Path, path: &str) -> Command {
    let mut cmd = Command::cargo_bin("kuro").unwrap();
    cmd.current_dir(dir)
        .arg("init")
        .env("PATH", path)
        .env_remove("CLAUDE_CLI_PATH")
        .env_remove("CODEX_CLI_PATH")
        .env_remove("OLLAMA_PATH")
        .env_remove("RUST_LOG");
    cmd
}

/// Recursively collect all file paths under `dir`, relative, sorted.
fn walk_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, acc: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, acc);
            } else {
                let rel = path.strip_prefix(root).expect("under root");
                acc.push(rel.display().to_string());
            }
        }
    }
    let mut acc = Vec::new();
    walk(dir, dir, &mut acc);
    acc.sort();
    acc
}

#[test]
fn empty_dir_creates_exactly_the_scaffold_files() {
    let dir = tempfile::tempdir().unwrap();

    let assert = init_cmd(dir.path(), "").assert().success();

    assert_eq!(walk_files(dir.path()), EXPECTED_FILES);

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    for file in EXPECTED_FILES {
        assert!(
            stdout.contains(file),
            "summary must list {file}, got: {stdout}"
        );
    }
    assert!(
        stdout.contains("kuro context") && stdout.contains("kuro run hello"),
        "summary must print next steps, got: {stdout}"
    );
}

#[test]
fn context_resolves_generated_setup_after_init() {
    let dir = tempfile::tempdir().unwrap();
    init_cmd(dir.path(), "").assert().success();

    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .current_dir(dir.path())
        .args(["context", "--format", "json"])
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    for name in ["Developer", "Reviewer", "hello", "project-conventions"] {
        assert!(
            stdout.contains(name),
            "context must list generated {name}, got: {stdout}"
        );
    }
}

#[test]
fn cargo_toml_dir_names_rust_in_templates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
    init_cmd(dir.path(), "").assert().success();

    for rel in [
        ".kuro/agents/Developer.yaml",
        ".kuro/agents/Reviewer.yaml",
        ".kuro/rules/project-conventions.md",
    ] {
        let contents = std::fs::read_to_string(dir.path().join(rel)).unwrap();
        assert!(
            contents.contains("Rust"),
            "{rel} must name Rust: {contents}"
        );
    }
}

#[test]
fn no_marker_dir_stays_generic() {
    let dir = tempfile::tempdir().unwrap();
    init_cmd(dir.path(), "").assert().success();

    for rel in EXPECTED_FILES {
        let contents = std::fs::read_to_string(dir.path().join(rel)).unwrap();
        for language in ["Rust", "Python", "Go", "JavaScript", "LaTeX"] {
            assert!(
                !contents.contains(language),
                "{rel} must not name {language} without a marker: {contents}"
            );
        }
    }
}

#[test]
fn existing_kuro_dir_fails_and_leaves_it_untouched() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".kuro")).unwrap();
    let sentinel = dir.path().join(".kuro/sentinel.txt");
    std::fs::write(&sentinel, "precious").unwrap();

    let assert = init_cmd(dir.path(), "").assert().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains(".kuro"),
        "stderr must name the existing path, got: {stderr}"
    );
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "precious");
    assert_eq!(walk_files(dir.path()), vec![".kuro/sentinel.txt"]);
}

#[test]
fn legacy_koto_yaml_fails_preflight() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("koto.yaml"), "version: \"1\"\n").unwrap();

    let assert = init_cmd(dir.path(), "").assert().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("koto.yaml"),
        "stderr must name koto.yaml, got: {stderr}"
    );
    assert_eq!(walk_files(dir.path()), vec!["koto.yaml"]);
}

#[test]
fn missing_backends_warn_but_succeed() {
    let dir = tempfile::tempdir().unwrap();

    let assert = init_cmd(dir.path(), "").assert().success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    for binary in ["claude", "codex", "ollama"] {
        assert!(
            stderr.contains(binary),
            "warning must name {binary}, got: {stderr}"
        );
    }
    // The fallback lands in the generated agents, not in config.yaml
    // (defaults.backend is the cli|api policy axis, not a provider).
    let agent = std::fs::read_to_string(dir.path().join(".kuro/agents/Developer.yaml")).unwrap();
    assert!(agent.contains("backend: claude-cli"), "got: {agent}");
}

#[test]
fn yes_flag_is_accepted_as_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = init_cmd(dir.path(), "");
    cmd.arg("--yes");
    cmd.assert().success();
    assert_eq!(walk_files(dir.path()), EXPECTED_FILES);
}
