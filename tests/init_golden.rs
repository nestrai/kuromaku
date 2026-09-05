//! Golden-fixture test for `kuro init` output (issue #398).
//!
//! `tests/fixtures/init-golden/` holds the byte-exact scaffold `kuro init`
//! produces for a generic project with no backend CLI on PATH (the
//! fully-pinned detection case). Two contracts are locked:
//!
//!   1. the wizard's output matches the checked-in fixture byte-for-byte,
//!      so template edits are a reviewed diff instead of silent drift
//!   2. every directory tree the README presents as the generated layout
//!      lists exactly the fixture's files, so docs and wizard cannot
//!      disagree (acceptance criterion: "checked-in directory trees and
//!      generated examples match a golden `kuro init` fixture")
//!
//! To update after an intentional template change: run `kuro init` with an
//! empty PATH in a scratch dir and copy the `.kuro/` tree over the fixture.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// Recursively collect relative file paths under `dir`, sorted.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, acc: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, acc);
            } else {
                acc.push(path.strip_prefix(root).expect("under root").to_path_buf());
            }
        }
    }
    let mut acc = Vec::new();
    walk(dir, dir, &mut acc);
    acc.sort();
    acc
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/init-golden")
}

#[test]
fn init_output_matches_golden_fixture_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("kuro")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        // Pinned detection: no marker file (generic), empty PATH (no
        // backend -> claude-cli default), no executor overrides.
        .env("PATH", "")
        .env_remove("CLAUDE_CLI_PATH")
        .env_remove("CODEX_CLI_PATH")
        .env_remove("OLLAMA_PATH")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let fixture = fixture_root();
    let generated_root = dir.path().join(".kuro");
    let fixture_files = walk_files(&fixture);
    let generated_files = walk_files(&generated_root);
    assert_eq!(
        generated_files, fixture_files,
        "generated file set must match the fixture -- update tests/fixtures/init-golden/ on intentional template changes"
    );

    for rel in &fixture_files {
        let expected = std::fs::read_to_string(fixture.join(rel)).expect("fixture readable");
        let actual = std::fs::read_to_string(generated_root.join(rel)).expect("output readable");
        assert_eq!(
            actual,
            expected,
            "generated {} must match the golden fixture byte-for-byte",
            rel.display()
        );
    }
}

#[test]
fn readme_generated_tree_lists_the_fixture_files() {
    // The README's "How it works" tree presents the generated layout. It
    // must name every fixture file (and no scaffold file may be renamed
    // without the README following).
    let readme = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("README readable");
    for rel in walk_files(&fixture_root()) {
        let file_name = rel.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            readme.contains(&file_name),
            "README must mention generated file {file_name} -- align the directory tree with `kuro init` output"
        );
    }
}
