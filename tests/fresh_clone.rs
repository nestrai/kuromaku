//! Fresh-clone smoke test (issue #398).
//!
//! The tracked seed configuration must be self-contained: a clone of this
//! repository on a machine with no maintainer-local seed library (and an
//! empty `$HOME`) must resolve `kuro context` successfully, with every
//! declared seed present on disk and every role bound in
//! `.kuro/config.yaml` resolvable to an agent file in the cascade.
//!
//! The test copies the *tracked* `.kuro/` and `seeds/` trees into a
//! tempdir -- not the whole working tree -- so it exercises exactly what a
//! fresh clone ships, isolated from anything else on the host.

#![cfg(unix)]

use std::path::Path;

use assert_cmd::Command;

/// Recursively copy `src` into `dst` (must not exist yet).
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Names of the agents the tracked `roles:` block binds, read from the
/// tracked config so this test cannot drift from `.kuro/config.yaml`.
fn role_bound_agents(repo_root: &Path) -> Vec<String> {
    let config =
        std::fs::read_to_string(repo_root.join(".kuro/config.yaml")).expect("tracked config");
    let yaml: serde_yaml::Value = serde_yaml::from_str(&config).expect("tracked config parses");
    let roles = yaml["roles"].as_mapping().expect("roles block present");
    roles
        .values()
        .map(|role| {
            role["agent"]
                .as_str()
                .expect("role binds an agent")
                .to_string()
        })
        .collect()
}

#[test]
fn tracked_seed_cascade_resolves_without_maintainer_paths() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let clone = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    copy_tree(&repo_root.join(".kuro"), &clone.path().join(".kuro"));
    copy_tree(&repo_root.join("seeds"), &clone.path().join("seeds"));

    let assert = Command::cargo_bin("kuro")
        .unwrap()
        .current_dir(clone.path())
        .args(["context", "--format", "json"])
        // Empty HOME: nothing outside the clone may contribute to
        // resolution -- the acceptance criterion is "no ~/code/nestrai/
        // seeds directory".
        .env("HOME", home.path())
        .env_remove("RUST_LOG")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Every declared seed must exist inside the clone.
    let seeds = json["seeds"].as_array().expect("seeds array");
    assert!(!seeds.is_empty(), "cascade must declare at least one seed");
    for seed in seeds {
        assert_eq!(
            seed["kind"], "local",
            "seed {} must be local",
            seed["display"]
        );
        assert_eq!(
            seed["exists"], true,
            "seed {} must resolve inside the clone",
            seed["display"]
        );
    }

    // Every enumerated flow must be runnable from on-disk configuration:
    // no unresolved roles anywhere.
    for seed in seeds {
        for flow in seed["flows"].as_array().into_iter().flatten() {
            let unresolved = flow["unresolved_roles"].as_array();
            assert!(
                unresolved.is_none_or(|u| u.is_empty()),
                "flow {} in seed {} has unresolved roles: {:?}",
                flow["name"],
                seed["display"],
                unresolved
            );
        }
    }

    // Every role-bound agent from the tracked config must resolve to an
    // agent file somewhere in the cascade. `unresolved_roles` alone does
    // not cover this: it checks role->binding, not binding->agent-file.
    let effective: Vec<&str> = json["effective"]["agents"]
        .as_array()
        .expect("effective agents")
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    for agent in role_bound_agents(repo_root) {
        assert!(
            effective.contains(&agent.as_str()),
            "role-bound agent {agent} missing from effective cascade {effective:?}"
        );
    }
}
