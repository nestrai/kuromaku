//! Smoke test for issue #241: end-to-end run of the shipped graph flow.
//!
//! Drives `seeds/rust/flows/implement-issue-graph.yaml` against an Ollama
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
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("seeds/rust/flows/implement-issue-graph.yaml");
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
        "#!/bin/sh\n\
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

/// Spawn `kuro run --file <flow>` from inside `project` with a sandboxed
/// HOME so stack writes land in the test tempdir. `OLLAMA_PATH` is wired
/// to the shim. `RUST_LOG` is stripped to keep stderr predictable.
fn run_kuro(project: &Path, flow: &Path, shim: &Path, home_dir: &Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    Command::new(bin)
        .args(["run", "--file"])
        .arg(flow)
        .args(["--var", "id=99"])
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", shim)
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
    // walk through the graph: design -> implement -> review -> create_pr
    // -> done.
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

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path());

    assert!(
        out.status.success(),
        "happy-path graph run must exit 0; status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // Manifest must have been written and reference all four agent-bearing
    // states in declaration order. Final states (`done`, `aborted`) are not
    // expected to appear in `steps:` because the driver does not persist a
    // step record for them; instead, the terminal state lands in the
    // top-level `final_state:` field (issue #257) so audit consumers can
    // tell `done` apart from `aborted` without grepping stderr.
    let manifest = read_manifest(home.path(), &project_name);
    for state in ["design", "implement", "review", "create_pr"] {
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

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path());

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
