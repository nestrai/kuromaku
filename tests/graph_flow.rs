//! End-to-end coverage for issue #240: the graph-flow state-machine driver.
//!
//! Drives the real `kuro` binary against a fake `ollama` shim that hands
//! back canned JSON decisions per state. We pick `ollama` rather than
//! `claude-cli` because Ollama uses `OutputFormat::Raw`: stdout is
//! returned to the caller verbatim, so the JSON envelope our shim writes
//! goes straight into [`runner::decision::parse_agent_decision`] without
//! any stream-json layer in the middle. (`claude-cli` would wrap the
//! reply in NDJSON, which is its own moving target.)
//!
//! The shim is wired through the `OLLAMA_PATH` env var that
//! `executor::build_ollama_command` reads. Each test installs its own
//! shim with custom logic (3-state success, malformed retry, unknown-edge
//! retry, runaway loop) so the failure mode lives one `case` away in the
//! shim instead of behind orchestration in Rust.
//!
//! Unit-level coverage of the menu format, retry-note shape, and
//! decision parser lives next to the implementation in
//! `src/runner/graph.rs::tests` and `src/runner/decision.rs::tests`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a project tempdir with the layout the graph driver expects:
/// `.kuro/config.yaml` mapping role names to agent files, plus the agent
/// files themselves. `agents` is a list of `(agent_id, role_name)` pairs;
/// each gets a YAML file with the Ollama backend so our shim catches the
/// invocation.
fn make_project(agents: &[(&str, &str)]) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro_dir = project.path().join(".kuro");
    std::fs::create_dir_all(kuro_dir.join("agents")).unwrap();

    let mut config = String::from("version: \"1\"\nroles:\n");
    for (agent_id, role) in agents {
        config.push_str(&format!("  {role}:\n    agent: {agent_id}\n"));
    }
    std::fs::write(kuro_dir.join("config.yaml"), config).unwrap();

    for (agent_id, _) in agents {
        std::fs::write(
            kuro_dir.join(format!("agents/{agent_id}.yaml")),
            format!(
                "name: {agent_id}\n\
                 role: You are {agent_id}, a placeholder persona for the graph-flow integration test.\n\
                 backend: ollama\n\
                 model: test-model\n"
            ),
        )
        .unwrap();
    }

    project
}

/// Install a shell shim at `<dir>/ollama` that:
/// 1. Logs every invocation to `<dir>/log.txt`.
/// 2. Maintains a call counter at `<dir>/calls`.
/// 3. Echoes whatever JSON `body` produces for the given prompt and call
///    number. The body is interpolated into a `case`/conditional script.
///
/// Returned tuple: (the dir owning the shim, the shim path, the log file).
/// Drop the dir to clean up. The shim is `chmod +x` so it can be invoked
/// directly via `OLLAMA_PATH`.
fn install_shim(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("shim tempdir");
    let log = dir.path().join("log.txt");
    let calls = dir.path().join("calls");
    let shim = dir.path().join("ollama");
    let script = format!(
        "#!/usr/bin/env bash\n\
         set -eu\n\
         LOG='{}'\n\
         CALLS='{}'\n\
         # Track call count so callers can branch on attempt number.\n\
         if [ -f \"$CALLS\" ]; then\n\
             N=$(cat \"$CALLS\")\n\
         else\n\
             N=0\n\
         fi\n\
         N=$((N + 1))\n\
         printf '%s' \"$N\" > \"$CALLS\"\n\
         # Last positional arg is the user prompt (build_ollama_command\n\
         # appends it after `run <model>`).\n\
         PROMPT=\"${{@: -1}}\"\n\
         {{\n\
             printf '\\n--- call %s ---\\n' \"$N\"\n\
             printf 'argv: '\n\
             for a in \"$@\"; do printf '%s|' \"$a\"; done\n\
             printf '\\n'\n\
         }} >> \"$LOG\"\n\
         {body}\n",
        log.display(),
        calls.display(),
    );
    std::fs::write(&shim, script).unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();
    (dir, shim, log)
}

/// Spawn `kuro run --file <flow>` from inside `project`, with HOME pointed
/// at `home_dir` so `~/.koto/stacks/...` writes land inside the test
/// sandbox. `OLLAMA_PATH` is wired to our shim. We strip `RUST_LOG` so the
/// existing assertions that grep stderr aren't polluted by debug logs the
/// developer might have exported.
fn run_kuro(project: &Path, flow: &Path, shim: &Path, home_dir: &Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    Command::new(bin)
        .args(["run", "--file"])
        .arg(flow)
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", shim)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn kuro run")
}

/// Locate the run directory the binary just produced. Graph-flow runs
/// land at `<HOME>/.koto/stacks/<project-basename>/<run-id>/`. There is
/// exactly one entry per test invocation because each test uses a fresh
/// HOME.
fn find_run_dir(home: &Path, project_name: &str) -> PathBuf {
    let stacks = home.join(".koto/stacks").join(project_name);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&stacks)
        .unwrap_or_else(|e| panic!("read stacks dir {}: {e}", stacks.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries
        .into_iter()
        .next_back()
        .unwrap_or_else(|| panic!("no run directories under {}", stacks.display()))
}

const GRAPH_3STATE: &str = r#"version: "1"
name: three-state
prompt: "drive the test graph"
initial: start
graph:
  start:
    role: dev
    task: "say hi"
    next:
      - middle: "Move to the middle state."
      - done: "Skip to done."
  middle:
    role: dev
    task: "look around"
    next:
      - done: "Move to the final state."
      - start: "Go back."
  done:
    final: "Three-state graph reached its terminal state."
"#;

#[test]
fn three_state_graph_runs_to_completion() {
    // AC1: start -> middle -> done with each agent picking a single
    // allowed edge runs to `done` and exits 0. Per-step files land under
    // `<run>/steps/` and the manifest gets written.
    let project = make_project(&[("Dev", "dev")]);
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = project.path().join("flow.yaml");
    std::fs::write(&flow, GRAPH_3STATE).unwrap();

    // Shim: branch on the state name in the menu line. Both states get
    // a single valid pick.
    let (_shim_dir, shim, log) = install_shim(
        r#"case "$PROMPT" in
  *'state `start`'*)
    printf '{"transition": "middle", "reason": "moving to middle as instructed"}\n'
    ;;
  *'state `middle`'*)
    printf '{"transition": "done", "reason": "wrapping up"}\n'
    ;;
  *)
    printf 'unknown prompt\n' >&2
    exit 1
    ;;
esac
"#,
    );

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path());

    assert!(
        out.status.success(),
        "graph run must exit 0; status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    let run_dir = find_run_dir(home.path(), &project_name);
    let steps_dir = run_dir.join("steps");
    assert!(
        steps_dir.is_dir(),
        "steps/ must exist under {}",
        run_dir.display()
    );

    // AC5: per-step files match the linear runner layout.
    let start_md = steps_dir.join("01-start.md");
    let middle_md = steps_dir.join("02-middle.md");
    assert!(start_md.is_file(), "missing {}", start_md.display());
    assert!(middle_md.is_file(), "missing {}", middle_md.display());
    assert!(
        steps_dir.join("01-start.meta.yaml").is_file(),
        "missing 01-start.meta.yaml"
    );
    assert!(
        steps_dir.join("02-middle.meta.yaml").is_file(),
        "missing 02-middle.meta.yaml"
    );
    assert!(
        run_dir.join("manifest.yaml").is_file(),
        "manifest.yaml must be written"
    );

    // The shim's JSON reply lands in the content file verbatim so the
    // audit trail shows what the agent actually said.
    let body = std::fs::read_to_string(&start_md).unwrap();
    assert!(
        body.contains("\"transition\": \"middle\""),
        "start step content must contain agent's reply, got:\n{body}"
    );

    let meta = std::fs::read_to_string(steps_dir.join("01-start.meta.yaml")).unwrap();
    assert!(
        meta.contains("graph_decision:"),
        "01-start.meta.yaml must carry graph_decision block, got:\n{meta}"
    );
    assert!(
        meta.contains("transition: middle"),
        "graph_decision must carry the picked transition, got:\n{meta}"
    );
}

#[test]
fn malformed_first_reply_retries_and_succeeds() {
    // AC2: a graph where the agent's first reply is malformed retries
    // once and continues if the second reply is valid. The shim emits
    // garbage on call 1 (malformed for state `start`) and the canonical
    // JSON on calls 2..n.
    let project = make_project(&[("Dev", "dev")]);
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = project.path().join("flow.yaml");
    std::fs::write(&flow, GRAPH_3STATE).unwrap();

    let (_shim_dir, shim, log) = install_shim(
        r#"if [ "$N" = "1" ]; then
    printf 'oops, not json at all\n'
    exit 0
fi
case "$PROMPT" in
  *'state `start`'*)
    printf '{"transition": "middle", "reason": "now valid"}\n'
    ;;
  *'state `middle`'*)
    printf '{"transition": "done", "reason": "wrapping up"}\n'
    ;;
esac
"#,
    );

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path());
    assert!(
        out.status.success(),
        "malformed-then-valid must succeed; status={:?}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    // The shim was called at least 3 times: malformed reply for start,
    // retry for start, and one for middle.
    let n: u32 = std::fs::read_to_string(_shim_dir.path().join("calls"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        n >= 3,
        "expected at least 3 shim calls (malformed + retry + middle), got {n}"
    );

    let run_dir = find_run_dir(home.path(), &project_name);
    assert!(run_dir.join("steps/02-middle.md").is_file());
}

#[test]
fn unknown_edge_twice_aborts_with_named_state() {
    // AC3: a graph where the agent picks an edge that is not in the
    // current state's set is retried once with an explicit error
    // message; second wrong pick aborts with non-zero exit and an error
    // that names the offending state.
    let project = make_project(&[("Dev", "dev")]);
    let flow = project.path().join("flow.yaml");
    std::fs::write(&flow, GRAPH_3STATE).unwrap();

    // Shim always answers with an edge that doesn't exist on the start
    // state. Two consecutive failures trip the retry budget.
    let (_shim_dir, shim, log) = install_shim(
        r#"printf '{"transition": "totally_made_up", "reason": "bad pick"}\n'
"#,
    );

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path());
    assert!(
        !out.status.success(),
        "double unknown-edge must fail; status={:?}\nstderr={}\nshim log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("'start'"),
        "stderr must name the offending state; got:\n{stderr}"
    );
    assert!(
        stderr.contains("totally_made_up"),
        "stderr must include the rejected transition; got:\n{stderr}"
    );

    // Exactly two attempts on the start state -- the first is the
    // initial pick, the second is the single allowed retry.
    let n: u32 = std::fs::read_to_string(_shim_dir.path().join("calls"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(n, 2, "must abort after exactly one retry, got {n} calls");
}

const GRAPH_LOOP: &str = r#"version: "1"
name: looping
prompt: "drive the loop graph"
initial: a
graph:
  a:
    role: dev
    next:
      - b: "go to b"
      - a: "stay"
  b:
    role: dev
    next:
      - a: "back to a"
      - b: "stay"
"#;

#[test]
fn per_state_visits_aborts_with_clear_error() {
    // The per-state visit cap (DEFAULT_MAX_VISITS_PER_STATE = 5, see
    // src/runner/graph.rs) trips before the global max_steps cap on a
    // tight `a <-> b` ping-pong. The error must name the looping state
    // and the cap value so the user can see what got stuck. The graph
    // above has no `final` state; the agent obediently bounces a <-> b
    // until the driver gives up.
    let project = make_project(&[("Dev", "dev")]);
    let flow = project.path().join("flow.yaml");
    std::fs::write(&flow, GRAPH_LOOP).unwrap();

    let (_shim_dir, shim, log) = install_shim(
        r#"case "$PROMPT" in
  *'state `a`'*)
    printf '{"transition": "b", "reason": "loop"}\n'
    ;;
  *'state `b`'*)
    printf '{"transition": "a", "reason": "loop"}\n'
    ;;
esac
"#,
    );

    let home = tempfile::tempdir().expect("home tempdir");
    let out = run_kuro(project.path(), &flow, &shim, home.path());
    assert!(
        !out.status.success(),
        "runaway loop must abort; status={:?}\nstderr={}\nshim log tail (last 500 bytes):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&log)
            .map(|s| s.chars().rev().take(500).collect::<String>())
            .unwrap_or_default()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("visited")
            && stderr.contains("cap 5")
            && stderr.contains("stuck in a loop"),
        "stderr must name the per-state cap and the loop diagnosis; got:\n{stderr}"
    );
    // The looping state named in the abort message is whichever side of
    // the a <-> b ping-pong tripped the cap first. With start=`a`, that
    // is `a` on its 6th entry, but keep both alternatives so the test
    // does not encode driver entry-order beyond what the contract gives.
    assert!(
        stderr.contains("'a'") || stderr.contains("'b'"),
        "stderr must name the looping state; got:\n{stderr}"
    );

    // With cap=5 and a 2-state ping-pong starting at `a`, the abort
    // fires when `a` is entered for the 6th time, *before* the agent
    // runs. Sequence of agent invocations: a,b,a,b,a,b,a,b,a,b -> 10
    // calls, then the 11th entry to `a` aborts.
    let n: u32 = std::fs::read_to_string(_shim_dir.path().join("calls"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        n, 10,
        "per-state cap (5) must abort on 6th entry to 'a'; expected 10 shim calls, got {n}"
    );
}
