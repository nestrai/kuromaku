//! Integration tests for `kuro init --seeds ROOT` and `KURO_SEEDS` (#386).
//!
//! These tests exercise the full binary (via assert_cmd) so they cover the
//! CLI parsing, the env-var fallback, the planning errors, and the round-trip
//! through `kuro context` that proves the written cascade is loadable.

#![cfg(unix)]

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

/// Build a minimal seed library tree under `root`.
/// `buckets` is a list of relative bucket paths (e.g. `["coding/rust", "github"]`).
/// Each bucket gets an `agents/` subdirectory so `is_seed_dir` considers it usable.
fn make_seed_library(root: &Path, buckets: &[&str]) {
    for bucket in buckets {
        let agents_dir = root.join(bucket).join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        // Write a minimal agent so the seed is non-trivially usable.
        std::fs::write(
            agents_dir.join("Sage.yaml"),
            "name: Sage\ntitle: Sage\nrole: |\n  You are a helpful assistant.\n",
        )
        .unwrap();
    }
}

fn kuro() -> Command {
    Command::cargo_bin("kuro").unwrap()
}

// --- AC1: Rust project + full seed library → documented cascade ---

#[test]
fn init_seeds_rust_full_cascade_written() {
    let dir = tempfile::tempdir().unwrap();
    let seeds = tempfile::tempdir().unwrap();
    make_seed_library(seeds.path(), &["coding/rust", "github", "coding/common"]);

    // Plant a Cargo.toml so the project is detected as Rust.
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .arg(seeds.path())
        .env("PATH", "")
        .env_remove("CLAUDE_CLI_PATH")
        .env_remove("CODEX_CLI_PATH")
        .env_remove("OLLAMA_PATH")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    assert!(config.contains("seeds:"), "missing seeds: section");
    assert!(config.contains("- path: .kuro/"), "missing .kuro/ entry");
    assert!(config.contains("coding/rust/"), "missing Rust bucket");
    assert!(config.contains("github/"), "missing github bucket");
    assert!(config.contains("coding/common/"), "missing common bucket");
}

// --- AC2: language bucket absent → cascade without it ---

#[test]
fn init_seeds_language_bucket_absent_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let seeds = tempfile::tempdir().unwrap();
    // Only github present, no coding/rust.
    make_seed_library(seeds.path(), &["github"]);
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .arg(seeds.path())
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    assert!(
        config.contains("github/"),
        "github bucket must be in cascade"
    );
    assert!(
        !config.contains("coding/rust/"),
        "absent bucket must not appear"
    );
}

// --- AC3: no usable buckets → non-zero exit, no .kuro/ ---

#[test]
fn init_seeds_no_usable_buckets_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let seeds = tempfile::tempdir().unwrap();
    // Empty seed library -- no bucket subdirs.

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .arg(seeds.path())
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no usable seed buckets"));

    assert!(
        !dir.path().join(".kuro").exists(),
        "failed init must leave no .kuro/ behind"
    );
}

// --- AC4: missing root → non-zero exit, no .kuro/ ---

#[test]
fn init_seeds_missing_root_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds", "/nonexistent/kuro-seeds-xyz"])
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));

    assert!(
        !dir.path().join(".kuro").exists(),
        "failed init must leave no .kuro/ behind"
    );
}

// --- AC4 variant: root is a file, not a directory ---

#[test]
fn init_seeds_root_is_file_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, "").unwrap();

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .arg(&file)
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));

    assert!(
        !dir.path().join(".kuro").exists(),
        "failed init must leave no .kuro/ behind"
    );
}

// --- AC5: KURO_SEEDS env var works; flag wins over env ---

#[test]
fn init_seeds_env_var_works() {
    let dir = tempfile::tempdir().unwrap();
    let seeds = tempfile::tempdir().unwrap();
    make_seed_library(seeds.path(), &["github"]);

    kuro()
        .current_dir(dir.path())
        .arg("init")
        .env("PATH", "")
        .env("KURO_SEEDS", seeds.path())
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    assert!(
        config.contains("seeds:"),
        "KURO_SEEDS must inject seeds section"
    );
}

#[test]
fn init_seeds_flag_wins_over_env() {
    let dir = tempfile::tempdir().unwrap();
    let flag_seeds = tempfile::tempdir().unwrap();
    let env_seeds = tempfile::tempdir().unwrap();
    // Only the flag seed has a bucket; the env seed has nothing.
    make_seed_library(flag_seeds.path(), &["github"]);

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .arg(flag_seeds.path())
        .env("PATH", "")
        .env("KURO_SEEDS", env_seeds.path())
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    // The flag seed's path appears in the cascade.
    assert!(
        config.contains(&flag_seeds.path().display().to_string()),
        "flag seed path must be in cascade"
    );
    // The env seed's path does not appear.
    assert!(
        !config.contains(&env_seeds.path().display().to_string()),
        "env seed path must not appear when flag wins"
    );
}

#[test]
fn init_seeds_empty_env_var_treated_as_unset() {
    let dir = tempfile::tempdir().unwrap();

    // Empty KURO_SEEDS="" → treated as unset → no seeds section.
    kuro()
        .current_dir(dir.path())
        .arg("init")
        .env("PATH", "")
        .env("KURO_SEEDS", "")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    assert!(
        !config.contains("seeds:"),
        "empty KURO_SEEDS must not inject a seeds section"
    );
}

// --- AC9: no flag/env → golden fixture unchanged ---

#[test]
fn init_no_seeds_golden_fixture_unchanged() {
    let dir = tempfile::tempdir().unwrap();

    kuro()
        .current_dir(dir.path())
        .arg("init")
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("CLAUDE_CLI_PATH")
        .env_remove("CODEX_CLI_PATH")
        .env_remove("OLLAMA_PATH")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    // Must match the golden fixture byte-for-byte.
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/init-golden");
    let config_actual = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    let config_expected = std::fs::read_to_string(fixture_root.join("config.yaml")).unwrap();
    assert_eq!(
        config_actual, config_expected,
        "no-seeds config.yaml must be byte-identical to the golden fixture"
    );
}

// --- AC10: written cascade resolves via kuro context ---

#[test]
fn init_seeds_context_resolves_written_cascade() {
    let dir = tempfile::tempdir().unwrap();
    let seeds = tempfile::tempdir().unwrap();
    make_seed_library(seeds.path(), &["github"]);

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .arg(seeds.path())
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    // kuro context must succeed and the github bucket must appear in the JSON
    // output -- a silently-dropped seeds: block would pass a bare .success()
    // check while making AC10 meaningless.
    let out = kuro()
        .current_dir(dir.path())
        .args(["context", "--format", "json"])
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(out.status.success(), "kuro context exited non-zero");
    let json = String::from_utf8(out.stdout).unwrap();
    assert!(
        json.contains("github"),
        "github bucket must appear in context JSON, got: {json}"
    );
}

// --- Relative --seeds path ---

#[test]
fn init_seeds_relative_path_resolves_from_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // Create vendor/seeds/github/agents/ under the project dir.
    let vendor = dir.path().join("vendor/seeds");
    std::fs::create_dir_all(vendor.join("github/agents")).unwrap();

    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds", "vendor/seeds"])
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join(".kuro/config.yaml")).unwrap();
    // Relative path is preserved in serialization.
    assert!(
        config.contains("vendor/seeds/github/"),
        "relative seed path must appear as-is in config: {config}"
    );
}

// --- --seeds CLI parses without value (clap should reject it) ---

#[test]
fn init_seeds_flag_requires_value() {
    let dir = tempfile::tempdir().unwrap();
    kuro()
        .current_dir(dir.path())
        .args(["init", "--seeds"])
        .env("PATH", "")
        .env_remove("KURO_SEEDS")
        .env_remove("RUST_LOG")
        .assert()
        .failure(); // clap rejects missing value for --seeds ROOT
}
