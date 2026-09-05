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

use std::{collections::BTreeMap, path::Path, process::Command as ProcessCommand};

use assert_cmd::Command;

/// Recursively copy `src` into `dst` (must not exist yet).
fn copy_tracked_seed_files(repo_root: &Path, dst: &Path) {
    let output = ProcessCommand::new("git")
        .current_dir(repo_root)
        .args(["ls-files", "-z", "--", ".kuro", "seeds"])
        .output()
        .expect("git ls-files runs");
    assert!(output.status.success(), "git ls-files succeeds");

    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(path).expect("tracked path is UTF-8");
        let target = dst.join(path);
        std::fs::create_dir_all(target.parent().expect("tracked file has parent")).unwrap();
        std::fs::copy(repo_root.join(path), target).unwrap();
    }
}

/// Names of the agents the tracked `roles:` block binds, read from the
/// tracked config so this test cannot drift from `.kuro/config.yaml`.
fn role_bound_agents(config_root: &Path) -> Vec<String> {
    let config =
        std::fs::read_to_string(config_root.join(".kuro/config.yaml")).expect("tracked config");
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

fn effective_agent_rules(clone_root: &Path) -> BTreeMap<String, Vec<String>> {
    [".kuro/agents", "seeds/rust/agents", "seeds/common/agents"]
        .into_iter()
        .flat_map(|dir| {
            std::fs::read_dir(clone_root.join(dir))
                .into_iter()
                .flatten()
                .flatten()
        })
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "yaml"))
        .fold(BTreeMap::new(), |mut agents, entry| {
            let yaml = std::fs::read_to_string(entry.path()).expect("tracked agent reads");
            let agent: serde_yaml::Value =
                serde_yaml::from_str(&yaml).expect("tracked agent parses");
            let name = agent["name"].as_str().expect("agent name").to_string();
            let rules = agent["rules"]
                .as_sequence()
                .into_iter()
                .flatten()
                .map(|rule| rule.as_str().expect("rule name").to_string())
                .collect();
            agents.entry(name).or_insert(rules);
            agents
        })
}

#[test]
fn tracked_seed_cascade_resolves_without_maintainer_paths() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let clone = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    copy_tracked_seed_files(repo_root, clone.path());

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
    for agent in role_bound_agents(clone.path()) {
        assert!(
            effective.contains(&agent.as_str()),
            "role-bound agent {agent} missing from effective cascade {effective:?}"
        );
    }

    // Every effective agent rule must resolve through the same tracked seed
    // cascade. This catches dangling references before a fresh-clone flow
    // reaches its first agent invocation.
    for (agent, rules) in effective_agent_rules(clone.path()) {
        for rule in rules {
            let exists = [".kuro", "seeds/rust", "seeds/common"].iter().any(|seed| {
                clone
                    .path()
                    .join(seed)
                    .join("rules")
                    .join(format!("{rule}.md"))
                    .is_file()
            });
            assert!(exists, "agent {agent} references missing rule {rule}");
        }
    }
}
