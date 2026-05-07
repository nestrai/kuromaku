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
    printf '{"transition": "implement", "reason": "plan ready"}\n'
    ;;
  *'state `implement`'*)
    printf '{"transition": "review", "reason": "code in"}\n'
    ;;
  *'state `review`'*)
    printf '{"transition": "verify", "reason": "criteria met"}\n'
    ;;
  *'state `create-pr`'*)
    printf '{"transition": "done", "reason": "draft pr opened"}\n'
    ;;
  *)
    printf 'unexpected prompt\n' >&2
    exit 1
    ;;
esac
"#,
    );

    // Green CI: `just` exits 0 so `verify` routes via pass.
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
    for state in ["design", "implement", "review", "verify", "create-pr"] {
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
    printf '{"transition": "implement", "reason": "plan ready"}\n'
    ;;
  *'state `implement`'*)
    printf '{"transition": "review", "reason": "code in"}\n'
    ;;
  *'state `review`'*)
    printf '{"transition": "verify", "reason": "criteria met"}\n'
    ;;
  *'state `create-pr`'*)
    printf '{"transition": "done", "reason": "draft pr opened"}\n'
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
    printf '{"transition": "aborted", "reason": "issue is too ambiguous"}\n'
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

/// Write a minimal graph flow with a human-handoff state to the project's
/// `.kuro/flows/` directory. The flow runs one agent step (`design`) then
/// hands off to a human state (`ask`). Returns the absolute path of the
/// flow file so the test can pass it via `--file`.
///
/// Kept tiny on purpose: issue #337 only locks in the pause contract, and
/// a five-state flow would dilute the test's signal. The `ask` state has
/// no `next:` -- the validator accepts that for human states (they may be
/// resumed later, but resume is #338's problem) and the driver pauses
/// before evaluating outgoing edges.
fn write_pause_flow(project: &Path) -> PathBuf {
    let flow_dir = project.join(".kuro/flows");
    std::fs::create_dir_all(&flow_dir).unwrap();
    let flow_path = flow_dir.join("pause.yaml");
    std::fs::write(
        &flow_path,
        "version: \"1\"\n\
         name: pause-smoke\n\
         prompt: \"Test the pause path.\"\n\
         initial: design\n\
         graph:\n\
         \x20\x20design:\n\
         \x20\x20\x20\x20role: developer\n\
         \x20\x20\x20\x20task: \"Write a one-line plan.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- ask: \"wait for human input\"\n\
         \x20\x20\x20\x20\x20\x20- design: \"keep iterating\"\n\
         \x20\x20ask:\n\
         \x20\x20\x20\x20human: true\n",
    )
    .unwrap();
    flow_path
}

#[test]
fn human_state_pauses_run_with_status_paused_in_manifest() {
    // Acceptance (issue #337):
    // - exit code 0 (paused is not an error)
    // - manifest contains `status: paused`
    // - manifest contains `paused_at_state: ask`
    // - manifest contains a `paused_at:` RFC3339 timestamp
    // - no `final_state:` (paused is mutually exclusive with terminal)
    // - one step file for `design`, none for `ask`
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = write_pause_flow(project.path());

    let (_shim_dir, shim, log) = install_shim(
        r#"case "$PROMPT" in
  *'state `design`'*)
    printf 'plan: ask the human something.\n{"transition": "ask", "reason": "need user input"}\n'
    ;;
  *)
    printf 'unexpected prompt; only design should run before pause\n' >&2
    exit 1
    ;;
esac
"#,
    );

    // No verify gate is in the pause flow, but `run_kuro` requires a
    // just_dir tempdir argument. An empty tempdir is fine.
    let (_just_dir, just_shim) = install_just_shim(0);
    let just_dir = just_shim.parent().unwrap();

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path(), just_dir);

    assert!(
        out.status.success(),
        "pause must exit 0 (it is not an error); status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    let manifest = read_manifest(home.path(), &project_name);
    assert!(
        manifest.contains("status: paused"),
        "paused run must record `status: paused`, got:\n{manifest}"
    );
    assert!(
        manifest.contains("paused_at_state: ask"),
        "paused run must record `paused_at_state: ask`, got:\n{manifest}"
    );
    // RFC3339 dates start with a 4-digit year + `-` -- check for the
    // `paused_at:` field key followed by a year-shaped prefix. This is
    // looser than parsing the timestamp but tight enough to catch a
    // missing field or an empty string.
    assert!(
        manifest.contains("paused_at: ") && manifest.contains("paused_at: 20"),
        "paused run must record an RFC3339 `paused_at:` timestamp, got:\n{manifest}"
    );
    assert!(
        !manifest.contains("final_state:"),
        "paused run must NOT record `final_state:` -- pause and terminal are mutually exclusive, got:\n{manifest}"
    );

    // Step files: exactly one for `design`, none for `ask`. The pause
    // contract is that the human state writes nothing -- mirroring the
    // final-state convention.
    let stacks = home.path().join(".koto/stacks").join(&project_name);
    let run_dir = std::fs::read_dir(&stacks)
        .unwrap_or_else(|e| panic!("read stacks dir {}: {e}", stacks.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max()
        .unwrap_or_else(|| panic!("no run directories under {}", stacks.display()));
    let steps_dir = run_dir.join("steps");
    let names: Vec<String> = std::fs::read_dir(&steps_dir)
        .unwrap_or_else(|e| panic!("read steps dir {}: {e}", steps_dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("design")),
        "expected a step file for design, got: {names:?}"
    );
    assert!(
        names.iter().all(|n| !n.contains("-ask.")),
        "no step file may exist for the paused human state, got: {names:?}"
    );
}

// --- `kuro resume` end-to-end (issue #338) ----------------------------
//
// Acceptance (#338) covered here:
// 1. Happy path: `kuro run` pauses at a human state, `kuro resume <id>`
//    advances and reaches a `kind: final`. Manifest no longer carries
//    `status: paused`; `final_state:` is set.
// 2. Error: `kuro resume <id>` with an unknown run-id exits non-zero
//    with a clear message.
// 3. Error: `kuro resume <id>` against a finished (non-paused) run exits
//    non-zero with a clear message naming the actual status.
//
// `kuro resume` looks the flow up by name through the seed cascade, so
// the test fixtures live under `.kuro/flows/<name>.yaml` (matching the
// YAML's `name:` field) -- a `kuro run --file ...` original is allowed,
// but resume does NOT see the `--file` arg and resolves by name.

/// Write a resumable graph flow at `.kuro/flows/<name>.yaml` so the
/// resume command can find it through the seed cascade. The flow walks
/// `design (agent) -> ask (human, next: done) -> done (final)` so the
/// pre-pause and post-resume phases are both exercised by a single
/// fixture.
fn write_resumable_flow(project: &Path) -> PathBuf {
    let flow_dir = project.join(".kuro/flows");
    std::fs::create_dir_all(&flow_dir).unwrap();
    // The filename MUST match the YAML's `name:` field for the seed
    // resolver to find it. resume() looks up by name.
    let flow_path = flow_dir.join("resume-smoke.yaml");
    std::fs::write(
        &flow_path,
        "version: \"1\"\n\
         name: resume-smoke\n\
         prompt: \"Test the resume path.\"\n\
         initial: design\n\
         graph:\n\
         \x20\x20design:\n\
         \x20\x20\x20\x20role: developer\n\
         \x20\x20\x20\x20task: \"Write a one-line plan.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- ask: \"wait for human input\"\n\
         \x20\x20\x20\x20\x20\x20- design: \"keep iterating\"\n\
         \x20\x20ask:\n\
         \x20\x20\x20\x20human: true\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- done: \"after human\"\n\
         \x20\x20done:\n\
         \x20\x20\x20\x20final: \"resumed terminal\"\n",
    )
    .unwrap();
    flow_path
}

/// Spawn `kuro resume <run-id>` from inside `project` with the same
/// sandbox shape `run_kuro` uses. No `--file`, no `--var`: resume
/// re-resolves the flow through the manifest and reuses the manifest's
/// vars verbatim.
fn resume_kuro(
    project: &Path,
    run_id: &str,
    shim: &Path,
    home_dir: &Path,
    just_dir: &Path,
) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let host_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{host_path}", just_dir.display());
    Command::new(bin)
        .args(["resume", run_id])
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", shim)
        .env("PATH", path)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn kuro resume")
}

/// Locate the (single) run dir for the given project under the test HOME.
/// Returns the path so callers can read step files / the manifest.
fn latest_run_dir(home: &Path, project_name: &str) -> PathBuf {
    let stacks = home.join(".koto/stacks").join(project_name);
    std::fs::read_dir(&stacks)
        .unwrap_or_else(|e| panic!("read stacks dir {}: {e}", stacks.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max()
        .unwrap_or_else(|| panic!("no run directories under {}", stacks.display()))
}

#[test]
fn resume_continues_paused_run_to_final() {
    // AC (#338): `kuro resume <run-id>` re-enters the paused run, the
    // graph driver advances past the human handoff, the manifest is
    // overwritten so `status: paused` is gone and `final_state:` is set.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = write_resumable_flow(project.path());

    // Same shim used by both phases. Only `design` is consulted -- the
    // `ask` state is human (no agent call) and `done` is final (no agent
    // call). On resume the driver advances `ask -> done` without
    // re-invoking the shim. If anything else is asked of the shim,
    // it would `exit 1` here and the test would surface the failure.
    let shim_body = r#"case "$PROMPT" in
  *'state `design`'*)
    printf 'plan: ask the human something.\n{"transition": "ask", "reason": "need user input"}\n'
    ;;
  *)
    printf 'unexpected prompt; resume must not re-invoke any agent\n' >&2
    exit 1
    ;;
esac
"#;
    let (_shim_dir, shim, log) = install_shim(shim_body);
    let (_just_dir, just_shim) = install_just_shim(0);
    let just_dir = just_shim.parent().unwrap();
    let home = tempfile::tempdir().expect("home tempdir");

    // Phase 1: original `kuro run` -- pauses at `ask`.
    let run_out = run_kuro(project.path(), &flow, &shim, home.path(), just_dir);
    assert!(
        run_out.status.success(),
        "phase 1 (kuro run) must exit 0 on pause; status={:?}\nstderr={}\nshim log:\n{}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    let run_dir = latest_run_dir(home.path(), &project_name);
    let run_id = run_dir.file_name().unwrap().to_string_lossy().to_string();
    let pre_resume_manifest = std::fs::read_to_string(run_dir.join("manifest.yaml")).unwrap();
    assert!(
        pre_resume_manifest.contains("status: paused"),
        "phase 1 must produce a paused manifest, got:\n{pre_resume_manifest}"
    );
    assert!(
        pre_resume_manifest.contains("paused_at_state: ask"),
        "phase 1 must record paused_at_state: ask, got:\n{pre_resume_manifest}"
    );

    // Phase 2: `kuro resume <run-id>` -- advances ask -> done.
    let resume_out = resume_kuro(project.path(), &run_id, &shim, home.path(), just_dir);
    assert!(
        resume_out.status.success(),
        "phase 2 (kuro resume) must exit 0; status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        resume_out.status,
        String::from_utf8_lossy(&resume_out.stdout),
        String::from_utf8_lossy(&resume_out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // Manifest after resume: status:paused gone, final_state present,
    // file path unchanged (same run dir).
    let post_resume_manifest = std::fs::read_to_string(run_dir.join("manifest.yaml")).unwrap();
    assert!(
        !post_resume_manifest.contains("status: paused"),
        "resumed manifest must NOT carry `status: paused`, got:\n{post_resume_manifest}"
    );
    assert!(
        post_resume_manifest.contains("final_state: done"),
        "resumed manifest must record `final_state: done`, got:\n{post_resume_manifest}"
    );
    // The pre-pause `design` step record is preserved across the
    // overwrite -- a resumed run that drops the pre-pause history would
    // lie about what happened.
    assert!(
        post_resume_manifest.contains("step_id: design"),
        "resumed manifest must keep the pre-pause step record, got:\n{post_resume_manifest}"
    );

    // Step files on disk: exactly the pre-pause `design` artifact.
    // `ask` (human) and `done` (final) write nothing on either phase.
    let steps_dir = run_dir.join("steps");
    let names: Vec<String> = std::fs::read_dir(&steps_dir)
        .unwrap_or_else(|e| panic!("read steps dir {}: {e}", steps_dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("design")),
        "expected a step file for design, got: {names:?}"
    );
    assert!(
        names
            .iter()
            .all(|n| !n.contains("-ask.") && !n.contains("-done.")),
        "no step file may exist for human or final states, got: {names:?}"
    );
}

#[test]
fn resume_errors_when_run_id_missing() {
    // AC (#338): an unknown run-id surfaces a clear error and a non-zero
    // exit. The message names the run-id and the stack path so an
    // operator can `ls` the directory and see what is actually there.
    let project = make_project();
    // We do not need a flow at all -- the resume command should fail
    // before trying to load any flow file.
    let (_shim_dir, shim, _log) = install_shim("printf 'unused\n'\n");
    let (_just_dir, just_shim) = install_just_shim(0);
    let just_dir = just_shim.parent().unwrap();
    let home = tempfile::tempdir().expect("home tempdir");

    let out = resume_kuro(
        project.path(),
        "ghost-20990101-000000",
        &shim,
        home.path(),
        just_dir,
    );
    assert!(
        !out.status.success(),
        "resume of an unknown run-id must exit non-zero; status={:?}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost-20990101-000000"),
        "error must name the run-id, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not found"),
        "error must include 'not found', got stderr:\n{stderr}"
    );
}

#[test]
fn resume_errors_when_run_already_done() {
    // AC (#338): a run that already reached a terminal state cannot be
    // resumed. The error must name the actual status so an operator can
    // tell apart a finished run from a still-mid-flight one.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = write_resumable_flow(project.path());

    // Walk the full happy path on the first invocation: design -> ask
    // -> design (loop back to keep iterating) is NOT what we want;
    // instead the shim picks `ask` once, and on resume the driver
    // advances ask -> done. To reach `done` in a single `kuro run`
    // (no human handoff), we need to pick a different transition --
    // but the seed flow's only non-pause exit from `design` is `ask`.
    // So we run a flow that reaches `done` directly: replace the seed
    // flow with one whose `design` state transitions to `done`.
    let direct_flow_path = project.path().join(".kuro/flows/done-direct.yaml");
    // The validator (validate_graph_flow) requires non-terminal states
    // to declare at least 2 outgoing edges. Add a second branch that the
    // shim never picks so the file passes schema validation but the run
    // still walks design -> done in one transition.
    std::fs::write(
        &direct_flow_path,
        "version: \"1\"\n\
         name: done-direct\n\
         prompt: \"Reach done directly.\"\n\
         initial: design\n\
         graph:\n\
         \x20\x20design:\n\
         \x20\x20\x20\x20role: developer\n\
         \x20\x20\x20\x20task: \"Pick done.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- done: \"directly\"\n\
         \x20\x20\x20\x20\x20\x20- design: \"keep iterating\"\n\
         \x20\x20done:\n\
         \x20\x20\x20\x20final: \"finished\"\n",
    )
    .unwrap();

    let shim_body = r#"case "$PROMPT" in
  *'state `design`'*)
    printf 'plan: done.\n{"transition": "done", "reason": "directly"}\n'
    ;;
  *)
    printf 'unexpected prompt\n' >&2
    exit 1
    ;;
esac
"#;
    let (_shim_dir, shim, _log) = install_shim(shim_body);
    let (_just_dir, just_shim) = install_just_shim(0);
    let just_dir = just_shim.parent().unwrap();
    let home = tempfile::tempdir().expect("home tempdir");

    // Phase 1: run to completion. `flow` (the resumable fixture) is
    // unused for this test path; the actual flow is the direct one
    // we just wrote. Pass the direct flow path to `run_kuro`.
    let _ = flow;
    let run_out = run_kuro(
        project.path(),
        &direct_flow_path,
        &shim,
        home.path(),
        just_dir,
    );
    assert!(
        run_out.status.success(),
        "phase 1 (kuro run) must exit 0; status={:?}\nstderr={}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stderr),
    );

    let run_dir = latest_run_dir(home.path(), &project_name);
    let run_id = run_dir.file_name().unwrap().to_string_lossy().to_string();

    // Phase 2: resume must refuse -- the run is `done`, not `paused`.
    let resume_out = resume_kuro(project.path(), &run_id, &shim, home.path(), just_dir);
    assert!(
        !resume_out.status.success(),
        "resume of a completed run must exit non-zero; status={:?}\nstderr={}",
        resume_out.status,
        String::from_utf8_lossy(&resume_out.stderr),
    );
    let stderr = String::from_utf8_lossy(&resume_out.stderr);
    assert!(
        stderr.contains("not 'paused'") || stderr.contains("not paused"),
        "error must explain the status mismatch, got stderr:\n{stderr}"
    );
}
