//! Integration tests for the documented seed-pinning pattern (issue #379).
//!
//! README section "Sharing seeds across repositories" documents a
//! repository-relative layout (`vendor/kuromaku-seeds/<bucket>/`) plus a
//! four-entry cascade as the supported interim contract until remote seed
//! resolution ships (ADR-0009). These tests pin that contract:
//!
//!   1. the *literal* documented cascade resolves from the documented
//!      layout via `kuro context --format json`
//!   2. an uninitialized submodule fails loudly with the exact error the
//!      README's recovery paragraph documents
//!   3. the README cannot silently drift from the fixture: the cascade
//!      YAML, layout paths and git commands asserted here must appear in
//!      the section, and the section must stay free of home-directory or
//!      maintainer-specific absolute paths
//!
//! Deliberately no `git submodule` invocation here: the resolver sees
//! only paths, git mechanics are git's contract, and shelling out to git
//! in CI adds flake for zero coverage. No `src/` behavior is touched by
//! this issue -- the tests consume existing stable surfaces only.

#![cfg(unix)]

use std::path::Path;

use assert_cmd::Command;

/// The cascade exactly as the README shows it. Must stay byte-identical
/// to the fenced YAML block in README.md ("Sharing seeds across
/// repositories") -- `readme_contains_the_documented_contract` enforces
/// the containment.
const DOCUMENTED_CASCADE: &str = r#"version: "1"
seeds:
  - path: .kuro/
  - path: vendor/kuromaku-seeds/coding/rust/
  - path: vendor/kuromaku-seeds/github/
  - path: vendor/kuromaku-seeds/coding/common/
"#;

/// Seed bucket directories from the documented layout, relative to the
/// project root, in cascade order (after the leading `.kuro/` entry).
const DOCUMENTED_BUCKETS: [&str; 3] = [
    "vendor/kuromaku-seeds/coding/rust",
    "vendor/kuromaku-seeds/github",
    "vendor/kuromaku-seeds/coding/common",
];

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn minimal_agent(name: &str) -> String {
    format!("name: {name}\nrole: |\n  Test agent for the seed-pinning fixture.\n")
}

/// Build the documented tree: config with the literal README cascade,
/// plus one minimal artifact per vendor bucket so each seed contributes
/// something observable to the effective cascade.
fn write_documented_layout(root: &Path) {
    write(&root.join(".kuro/config.yaml"), DOCUMENTED_CASCADE);
    write(
        &root.join("vendor/kuromaku-seeds/coding/rust/agents/RustSeed.yaml"),
        &minimal_agent("RustSeed"),
    );
    write(
        &root.join("vendor/kuromaku-seeds/github/rules/github-workflow.md"),
        "# GitHub Workflow\n\nSeed-pinning fixture rule.\n",
    );
    write(
        &root.join("vendor/kuromaku-seeds/coding/common/rules/clean-code.md"),
        "# Clean Code\n\nSeed-pinning fixture rule.\n",
    );
}

fn context_json(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("kuro").unwrap();
    cmd.current_dir(dir)
        .args(["context", "--format", "json"])
        .env_remove("RUST_LOG");
    cmd
}

/// Criterion 6: the documented cascade loads from the documented
/// repository-relative layout and `kuro context --format json` resolves
/// every seed and its artifacts.
#[test]
fn documented_cascade_resolves() {
    let dir = tempfile::tempdir().unwrap();
    write_documented_layout(dir.path());

    let assert = context_json(dir.path()).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // All four seeds present, declaration order preserved, all local
    // and existing on disk.
    let seeds = json["seeds"].as_array().expect("seeds array");
    let displays: Vec<&str> = seeds
        .iter()
        .map(|s| s["display"].as_str().unwrap())
        .collect();
    assert_eq!(
        displays,
        [
            ".kuro/",
            "vendor/kuromaku-seeds/coding/rust/",
            "vendor/kuromaku-seeds/github/",
            "vendor/kuromaku-seeds/coding/common/",
        ],
        "cascade must list the documented seeds in order"
    );
    for seed in seeds {
        assert_eq!(seed["kind"], "local", "documented seeds are path-based");
        assert_eq!(
            seed["exists"], true,
            "seed {} must resolve on disk",
            seed["display"]
        );
    }

    // Each vendor bucket's artifact lands in the effective cascade,
    // attributed to the right seed.
    let effective_agents = json["effective"]["agents"].as_array().unwrap();
    let rust_agent = effective_agents
        .iter()
        .find(|a| a["name"] == "RustSeed")
        .expect("RustSeed agent in effective cascade");
    assert_eq!(rust_agent["seed"], "vendor/kuromaku-seeds/coding/rust/");

    let effective_rules = json["effective"]["rules"].as_array().unwrap();
    for (rule, seed) in [
        ("github-workflow", "vendor/kuromaku-seeds/github/"),
        ("clean-code", "vendor/kuromaku-seeds/coding/common/"),
    ] {
        let item = effective_rules
            .iter()
            .find(|r| r["name"] == rule)
            .unwrap_or_else(|| panic!("{rule} rule in effective cascade"));
        assert_eq!(item["seed"], seed);
    }
}

/// Pins the resolution gap the README's invocation-directory sentence
/// documents: `kuro context` resolves from the current directory with no
/// ancestor walk, so running from a subdirectory of the documented
/// layout does not fail loudly -- it silently falls back to the implicit
/// `.kuro/` default seed (missing there) and an empty effective cascade.
/// If an ancestor walk ever ships, this test and the README sentence
/// must change together.
#[test]
fn cascade_resolves_from_subdir_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    write_documented_layout(dir.path());
    let subdir = dir.path().join("subdir");
    std::fs::create_dir_all(&subdir).unwrap();

    let assert = context_json(&subdir).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // No config in the subdir and no ancestor walk: only the implicit
    // default seed remains, and it does not exist there.
    let seeds = json["seeds"].as_array().expect("seeds array");
    assert_eq!(
        seeds.len(),
        1,
        "subdir invocation must fall back to the single implicit seed"
    );
    assert_eq!(seeds[0]["display"], ".kuro/");
    assert_eq!(seeds[0]["exists"], false);

    // The documented seeds one level up are invisible: nothing resolves.
    for kind in ["agents", "rules", "flows"] {
        assert_eq!(
            json["effective"][kind].as_array().unwrap().len(),
            0,
            "effective {kind} must be empty when invoked from a subdirectory"
        );
    }
}

/// The failure mode the README's "Cloning and recovery" paragraph
/// documents: with the submodule uninitialized (no `vendor/`), config
/// load fails loudly and names the offending path.
#[test]
fn missing_submodule_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join(".kuro/config.yaml"), DOCUMENTED_CASCADE);

    let assert = context_json(dir.path()).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("seed path \"vendor/kuromaku-seeds/coding/rust/\" does not exist"),
        "error must name the missing seed path, got: {stderr}"
    );
}

/// Drift guard (criteria 1, 2, 5): the README section must contain the
/// exact contract these tests pin -- the cascade YAML verbatim, the
/// layout, the clone/recovery/update commands, the ADR link -- and no
/// home-directory or maintainer-specific absolute path.
#[test]
fn readme_contains_the_documented_contract() {
    let readme_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let readme = std::fs::read_to_string(readme_path).expect("README.md readable");

    const HEADING: &str = "## Sharing seeds across repositories";
    let start = readme.find(HEADING).expect("seed-sharing section present");
    let body = &readme[start + HEADING.len()..];
    let section = match body.find("\n## ") {
        Some(end) => &body[..end],
        None => body,
    };

    // The fixture cascade and the README block are the same bytes --
    // editing one without the other fails here.
    assert!(
        section.contains(DOCUMENTED_CASCADE),
        "README cascade YAML must match the fixture verbatim"
    );
    for bucket in DOCUMENTED_BUCKETS {
        assert!(
            section.contains(bucket),
            "README must document bucket {bucket}"
        );
    }
    for required in [
        "git clone --recurse-submodules",
        "git submodule update --init --recursive",
        "git submodule update --remote",
        "vendor/kuromaku-seeds",
        "does not exist",
        "docs/decisions/0009-version-pinning.md",
    ] {
        assert!(
            section.contains(required),
            "README seed section must contain {required:?}"
        );
    }

    // Criterion 5: repository-relative examples only.
    for forbidden in ["/home/", "/Users/", "~/"] {
        assert!(
            !section.contains(forbidden),
            "README seed section must not contain {forbidden:?}"
        );
    }
}
