//! Smoke test for issue #241: end-to-end run of the shipped graph flow.
//!
//! Drives `seeds/rust/flows/implement-issue.yaml` against an Ollama
//! shim that returns canned `{transition, reason}` JSON for each state.
//! The point is not to verify code-quality of any agent output -- only
//! that the runtime walks the graph from `initial:` to a state with
//! `kind: final` (either `done` or `aborted`).
//!
//! Reuses the shim pattern from `tests/graph_flow.rs`: `OLLAMA_PATH` is
//! set to a generated shell script that branches on the state name in
//! the user prompt and prints the JSON envelope the driver expects. The
//! Ollama backend is chosen because it returns stdout verbatim (raw
//! output format) -- no stream-json layer in front of the parser.
//!
//! Two paths are covered:
//! 1. Happy path: design -> implement -> review -> create_pr -> done
//! 2. Short path: design -> aborted (via the `blocked` edge)
//!
//! Both terminal states are valid per the issue's acceptance criteria.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve the shipped graph flow file in the kuromaku source tree.
///
/// `CARGO_MANIFEST_DIR` points at the kuromaku crate root at compile
/// time, so the test always finds the seed file regardless of the
/// caller's CWD. Joining a relative path keeps the smoke test useful
/// even if the seed gets moved -- the assertion below names the file we
/// looked for.
fn seed_flow_path() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seeds/rust/flows/implement-issue.yaml");
    assert!(
        p.is_file(),
        "expected seed flow at {} -- has it moved?",
        p.display()
    );
    p
}

/// Build a tempdir project with role bindings for `architect`,
/// `developer`, `reviewer`. Each role maps to an Ollama-backed agent
/// so the shim catches every state's invocation. Returns the project
/// dir; callers drop it for cleanup.
fn make_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro_dir = project.path().join(".kuro");
    std::fs::create_dir_all(kuro_dir.join("agents")).unwrap();

    // Project config: role -> agent_id mappings. The driver looks up
    // each non-terminal state's role here and resolves to an agent
    // file under `.kuro/agents/<id>.yaml`.
    std::fs::write(
        kuro_dir.join("config.yaml"),
        "version: \"1\"\n\
         roles:\n\
         \x20\x20architect:\n\
         \x20\x20\x20\x20agent: Architect\n\
         \x20\x20developer:\n\
         \x20\x20\x20\x20agent: Developer\n\
         \x20\x20reviewer:\n\
         \x20\x20\x20\x20agent: Reviewer\n",
    )
    .unwrap();

    for agent in ["Architect", "Developer", "Reviewer"] {
        std::fs::write(
            kuro_dir.join(format!("agents/{agent}.yaml")),
            format!(
                "name: {agent}\n\
                 role: Placeholder persona for the graph-flow smoke test.\n\
                 backend: ollama\n\
                 model: test-model\n"
            ),
        )
        .unwrap();
    }

    project
}

/// Install a shell shim at `<dir>/ollama` that branches on the state
/// name in the user prompt and prints the supplied JSON envelope. The
/// shim writes a log to `<dir>/log.txt` for failure diagnostics.
fn install_shim(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("shim tempdir");
    let log = dir.path().join("log.txt");
    let shim = dir.path().join("ollama");
    let script = format!(
        "#!/usr/bin/env bash\n\
         set -eu\n\
         LOG='{}'\n\
         PROMPT=\"${{@: -1}}\"\n\
         {{\n\
             printf '\\n--- call ---\\n'\n\
             printf 'argv: '\n\
             for a in \"$@\"; do printf '%s|' \"$a\"; done\n\
             printf '\\n'\n\
         }} >> \"$LOG\"\n\
         {body}\n",
        log.display(),
    );
    std::fs::write(&shim, script).unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();
    (dir, shim, log)
}

/// Install a `just` shim that exits with `code` regardless of arguments.
///
/// The seed flow's `verify` state (issue #310) runs `just lint && just
/// test` -- a tempdir project has no Justfile, so the real `just` would
/// error before the test could exercise routing. The shim lets the
/// happy-path test simulate green CI (exit 0) and the failure-path test
/// simulate red CI (non-zero) without a real toolchain on the runner.
fn install_just_shim(code: i32) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("just-shim tempdir");
    let shim = dir.path().join("just");
    let script = format!("#!/bin/sh\nexit {code}\n");
    std::fs::write(&shim, script).unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();
    (dir, shim)
}

/// Spawn `kuro run --file <flow>` from inside `project` with a sandboxed
/// HOME so stack writes land in the test tempdir. `OLLAMA_PATH` is wired
/// to the shim. `RUST_LOG` is stripped to keep stderr predictable.
///
/// `just_dir` is prepended to PATH so the seed flow's `verify` state
/// resolves to the `just` shim installed by [`install_just_shim`]
/// instead of whatever (if anything) the host has.
fn run_kuro(
    project: &Path,
    flow: &Path,
    shim: &Path,
    home_dir: &Path,
    just_dir: &Path,
) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let host_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{host_path}", just_dir.display());
    Command::new(bin)
        .args(["run", "--file"])
        .arg(flow)
        .args(["--var", "id=99"])
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", shim)
        .env("PATH", path)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn kuro run")
}

/// Locate the manifest the binary just produced under the test HOME.
fn read_manifest(home: &Path, project_name: &str) -> String {
    let stacks = home.join(".koto/stacks").join(project_name);
    let entries: Vec<PathBuf> = std::fs::read_dir(&stacks)
        .unwrap_or_else(|e| panic!("read stacks dir {}: {e}", stacks.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let run_dir = entries
        .into_iter()
        .max()
        .unwrap_or_else(|| panic!("no run directories under {}", stacks.display()));
    std::fs::read_to_string(run_dir.join("manifest.yaml"))
        .unwrap_or_else(|e| panic!("read manifest in {}: {e}", run_dir.display()))
}

#[test]
fn happy_path_walks_design_to_done() {
    // AC2 (issue #241): smoke test runs the flow with a canned issue and
    // lands at a terminal state. This case verifies the longest valid
    // walk through the graph: design -> implement -> review -> verify
    // (shell, exit 0, issue #310) -> create_pr -> done.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = seed_flow_path();

    let (_shim_dir, shim, log) = install_shim(
        r#"case "$PROMPT" in
  *'state `design`'*)
    printf '{"transition": "design_complete", "reason": "plan ready"}\n'
    ;;
  *'state `implement`'*)
    printf '{"transition": "implementation_complete", "reason": "code in"}\n'
    ;;
  *'state `review`'*)
    printf '{"transition": "approved", "reason": "criteria met"}\n'
    ;;
  *'state `create_pr`'*)
    printf '{"transition": "pr_created", "reason": "draft pr opened"}\n'
    ;;
  *)
    printf 'unexpected prompt\n' >&2
    exit 1
    ;;
esac
"#,
    );

    // Green CI: `just` exits 0 so `verify` routes via `on: pass`.
    let (_just_dir, just_shim) = install_just_shim(0);
    let just_dir = just_shim.parent().unwrap();

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path(), just_dir);

    assert!(
        out.status.success(),
        "happy-path graph run must exit 0; status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // Manifest must have been written and reference all five non-terminal
    // states in the happy walk. Final states (`done`, `aborted`) are not
    // expected to appear in `steps:` because the driver does not persist a
    // step record for them; instead, the terminal state lands in the
    // top-level `final_state:` field (issue #257) so audit consumers can
    // tell `done` apart from `aborted` without grepping stderr. `verify`
    // is included to lock in that the deterministic gate (#310) shows up
    // alongside agent steps in the manifest.
    let manifest = read_manifest(home.path(), &project_name);
    for state in ["design", "implement", "review", "verify", "create_pr"] {
        assert!(
            manifest.contains(state),
            "manifest must reference state '{state}', got:\n{manifest}"
        );
    }
    assert!(
        manifest.contains("final_state: done"),
        "happy path must record `final_state: done` in the manifest, got:\n{manifest}"
    );
}

#[test]
fn verify_failure_loops_back_to_implement() {
    // AC4 (issue #310): a non-zero `just` exit must route the run back
    // through `implement` instead of progressing to `create_pr`. The
    // visit cap (`DEFAULT_MAX_VISITS_PER_STATE`) eventually terminates
    // the loop so the test does not hang -- we assert the run exited
    // (non-zero, the cap fires `RunError::GraphRuntime`) and that the
    // manifest records at least one shell step with a non-zero exit.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = seed_flow_path();

    let (_shim_dir, shim, _log) = install_shim(
        r#"case "$PROMPT" in
  *'state `design`'*)
    printf '{"transition": "design_complete", "reason": "plan ready"}\n'
    ;;
  *'state `implement`'*)
    printf '{"transition": "implementation_complete", "reason": "code in"}\n'
    ;;
  *'state `review`'*)
    printf '{"transition": "approved", "reason": "criteria met"}\n'
    ;;
  *'state `create_pr`'*)
    printf '{"transition": "pr_created", "reason": "draft pr opened"}\n'
    ;;
  *)
    printf 'unexpected prompt\n' >&2
    exit 1
    ;;
esac
"#,
    );

    // Red CI: `just` always exits non-zero, so `verify` routes via
    // `on: fail` back to `implement`. With every agent state happy and
    // `verify` always red, the loop runs until the visit cap aborts.
    let (_just_dir, just_shim) = install_just_shim(1);
    let just_dir = just_shim.parent().unwrap();

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path(), just_dir);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "loop must abort with non-zero exit (visit cap), got status={:?}\nstderr={stderr}",
        out.status,
    );

    // GraphRuntime errors abort before the manifest is written, but the
    // per-step records are persisted live as the driver walks. Assert
    // that at least one verify-shell-step landed on disk and that its
    // meta.yaml records `type: shell` with a non-zero exit code.
    let stacks = home.path().join(".koto/stacks").join(&project_name);
    let run_dir = std::fs::read_dir(&stacks)
        .unwrap_or_else(|e| panic!("read stacks dir {}: {e}", stacks.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max()
        .unwrap_or_else(|| panic!("no run directories under {}", stacks.display()));
    let steps_dir = run_dir.join("steps");
    let verify_metas: Vec<PathBuf> = std::fs::read_dir(&steps_dir)
        .unwrap_or_else(|e| panic!("read steps dir {}: {e}", steps_dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("-verify.meta.yaml"))
        })
        .collect();
    assert!(
        !verify_metas.is_empty(),
        "expected at least one verify shell step on disk under {}\nstderr was:\n{stderr}",
        steps_dir.display()
    );
    let meta = std::fs::read_to_string(&verify_metas[0]).unwrap();
    assert!(
        meta.contains("type: shell"),
        "verify step meta.yaml must record `type: shell`, got:\n{meta}"
    );
    assert!(
        !meta.contains("exit_code: 0"),
        "verify step meta.yaml must record a non-zero exit code, got:\n{meta}"
    );
}

#[test]
fn blocked_at_design_lands_at_aborted() {
    // AC2 (issue #241): "lands at done OR aborted -- either is
    // acceptable". This case verifies the short path through the graph
    // (design -> aborted) so we know the second terminal kind also
    // works end-to-end.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = seed_flow_path();

    let (_shim_dir, shim, log) = install_shim(
        r#"case "$PROMPT" in
  *'state `design`'*)
    printf '{"transition": "blocked", "reason": "issue is too ambiguous"}\n'
    ;;
  *)
    printf 'unexpected prompt; only design should run\n' >&2
    exit 1
    ;;
esac
"#,
    );

    // No verify gate fires on this path (`design` aborts immediately),
    // but `run_kuro` still wants a `just_dir`. An empty tempdir is fine.
    let (_just_dir, just_shim) = install_just_shim(0);
    let just_dir = just_shim.parent().unwrap();

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path(), just_dir);

    assert!(
        out.status.success(),
        "blocked graph run must still exit 0 (aborted is a final state); status={:?}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // The blocked path lands at the second terminal kind (`aborted`); the
    // manifest's `final_state:` field is the structural signal -- audit
    // consumers (#257) read it to tell the two terminal kinds apart
    // without parsing stderr.
    let manifest = read_manifest(home.path(), &project_name);
    assert!(
        manifest.contains("final_state: aborted"),
        "blocked path must record `final_state: aborted` in the manifest, got:\n{manifest}"
    );
}
