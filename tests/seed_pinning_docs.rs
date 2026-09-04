//! Documentation test for the seed-pinning contract (issue #379).
//!
//! `docs/seed-pinning.md` documents a repository-relative Git submodule
//! at `vendor/kuromaku-seeds/` as the supported seed-pinning pattern.
//! This test keeps the doc honest: it extracts the cascade YAML straight
//! out of the doc's first fenced block, materializes the documented
//! repository layout in a tempdir, and runs the real binary against it.
//! If someone edits the doc's YAML into something `kuro` no longer
//! accepts, this test fails -- there is no hand-copied fixture to drift.
//!
//! Deliberately out of scope: git/submodule operations. The pinning
//! mechanism is git's concern; kuromaku's contract is only "a directory
//! at this repository-relative path resolves". Exercising `git
//! submodule` here would test git, not kuromaku.

#![cfg(unix)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;

const DOC_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/seed-pinning.md");

/// The seed directories the documented cascade references below `.kuro/`,
/// relative to the project root. Kept in sync with the doc by the fence
/// assertion in `documented_cascade_resolves`.
const SEED_DIRS: [&str; 3] = [
    "vendor/kuromaku-seeds/coding/rust/",
    "vendor/kuromaku-seeds/github/",
    "vendor/kuromaku-seeds/coding/common/",
];

/// Extract the body of the first ```yaml fence in `markdown`.
fn first_yaml_fence(markdown: &str) -> String {
    let start = markdown
        .find("```yaml\n")
        .expect("docs/seed-pinning.md must contain a ```yaml fence");
    let body = &markdown[start + "```yaml\n".len()..];
    let end = body
        .find("\n```")
        .expect("yaml fence in docs/seed-pinning.md is not closed");
    body[..end].to_string()
}

/// Write a minimal valid agent file so the seed contributes a non-trivial
/// inventory entry.
fn write_agent(seed_dir: &Path, name: &str) {
    let agents = seed_dir.join("agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join(format!("{name}.yaml")),
        format!("name: {name}\nrole: |\n  Placeholder role for the seed-pinning doc test.\n"),
    )
    .unwrap();
}

/// Write a minimal rule file into the seed's `rules/` directory.
fn write_rule(seed_dir: &Path, name: &str) {
    let rules = seed_dir.join("rules");
    fs::create_dir_all(&rules).unwrap();
    fs::write(
        rules.join(format!("{name}.md")),
        "Placeholder rule for the seed-pinning doc test.\n",
    )
    .unwrap();
}

/// Acceptance criterion (issue #379): a documentation test loads the
/// documented cascade from the shown repository-relative layout and
/// `kuro context --format json` resolves it.
#[test]
fn documented_cascade_resolves() {
    let doc = fs::read_to_string(DOC_PATH).expect("docs/seed-pinning.md must exist");
    let cascade = first_yaml_fence(&doc);

    // The doc's example must be plain repository-relative `path:` syntax:
    // no absolute paths, no home-directory shortcuts, no remote entries.
    assert!(
        !cascade.contains('~') && !cascade.contains("/home/") && !cascade.contains("/Users/"),
        "documented cascade must not contain machine-specific paths:\n{cascade}"
    );
    assert!(
        !cascade.contains("repo:"),
        "documented cascade must use path-based pinning only:\n{cascade}"
    );
    for dir in SEED_DIRS {
        assert!(
            cascade.contains(dir),
            "documented cascade must reference {dir}; update SEED_DIRS if the doc changed:\n{cascade}"
        );
    }

    // Materialize the documented repository layout.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".kuro")).unwrap();
    fs::write(root.join(".kuro/config.yaml"), &cascade).unwrap();
    write_agent(&root.join(SEED_DIRS[0]), "SeedDeveloper");
    write_rule(&root.join(SEED_DIRS[1]), "github-conventions");
    write_rule(&root.join(SEED_DIRS[2]), "common-conventions");

    // Run the real binary from the project root, exactly as the doc's
    // operational note prescribes. This exercises both the parse-time
    // `exists()` validation and the cwd rebase.
    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .current_dir(root)
        .args(["context", "--format", "json"])
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("kuro context --format json must emit valid JSON");

    // All four documented seeds resolve, in cascade order, and exist.
    let seeds = v["seeds"].as_array().expect("v1 wire format has `seeds`");
    assert_eq!(seeds.len(), 4, "cascade documents four seeds:\n{stdout}");
    let displays: Vec<&str> = seeds
        .iter()
        .map(|s| s["display"].as_str().unwrap())
        .collect();
    assert_eq!(displays[0], ".kuro/");
    assert_eq!(&displays[1..], SEED_DIRS);
    for seed in seeds {
        assert_eq!(
            seed["exists"], true,
            "documented seed {} must resolve to an existing directory",
            seed["display"]
        );
    }

    // The effective inventory attributes artifacts to the submodule seeds.
    let effective_agents = v["effective"]["agents"].as_array().unwrap();
    assert!(
        effective_agents
            .iter()
            .any(|a| a["name"] == "SeedDeveloper" && a["seed"] == SEED_DIRS[0]),
        "SeedDeveloper must be attributed to {}:\n{stdout}",
        SEED_DIRS[0]
    );
    let effective_rules = v["effective"]["rules"].as_array().unwrap();
    for (name, seed) in [
        ("github-conventions", SEED_DIRS[1]),
        ("common-conventions", SEED_DIRS[2]),
    ] {
        assert!(
            effective_rules
                .iter()
                .any(|r| r["name"] == name && r["seed"] == seed),
            "rule {name} must be attributed to {seed}:\n{stdout}"
        );
    }
}
