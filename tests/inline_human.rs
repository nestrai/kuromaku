//! Integration test for issue #361: inline interactive HITL on TTY.
//!
//! Acceptance (paraphrasing the issue):
//! 1. On a TTY, `kuro run <flow>` reaching a `human:` state opens an
//!    inline prompt; the operator types a message; the run continues
//!    to the next state in the same invocation -- no `kuro resume`.
//! 2. The synthetic `kind: human` step record on disk is identical
//!    between the inline-TTY and `kuro resume` paths (same body shape,
//!    same audit metadata).
//! 3. `--no-interactive` forces today's pause-and-exit path even when
//!    the streams happen to be TTYs.
//!
//! Strategy: spawn `kuro run` with `KURO_FORCE_INTERACTIVE_HITL=1` (a
//! documented test seam in `runner::flow_api::detect_interactive_tty_status`
//! that lets piped stdin / stdout impersonate a TTY -- otherwise the
//! integration test would need a real PTY emulator). Pipe the human
//! body on stdin terminated by a blank line; assert the run reaches
//! `final_state: done` in one invocation and the synthetic step
//! body matches the documented format.
//!
//! The scaffold (ollama shim, project tempdir, flow shape) mirrors
//! `tests/resume_human_input.rs` so the two files stay readable next
//! to each other.

#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Project dir with the same role bindings as `tests/resume_human_input.rs`:
/// the pre-handoff `design` step uses developer, the post-handoff
/// `review` step uses reviewer, both via an ollama-backed shim.
fn make_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro_dir = project.path().join(".kuro");
    std::fs::create_dir_all(kuro_dir.join("agents")).unwrap();

    std::fs::write(
        kuro_dir.join("config.yaml"),
        "version: \"1\"\n\
         roles:\n\
         \x20\x20developer:\n\
         \x20\x20\x20\x20agent: Developer\n\
         \x20\x20reviewer:\n\
         \x20\x20\x20\x20agent: Reviewer\n",
    )
    .unwrap();

    for agent in ["Developer", "Reviewer"] {
        std::fs::write(
            kuro_dir.join(format!("agents/{agent}.yaml")),
            format!(
                "name: {agent}\n\
                 role: Placeholder persona for the inline-HITL integration test.\n\
                 backend: ollama\n\
                 model: test-model\n"
            ),
        )
        .unwrap();
    }

    project
}

/// `design (developer) -> ask (human, next: review) -> review (reviewer)
///   -> done`. Same shape as the resume integration test so the
/// scaffolding feels familiar. `vars.id` is non-numeric so the GH path
/// is irrelevant here -- the inline arm is the only human-input
/// channel.
fn write_flow(project: &Path) -> PathBuf {
    let flow_dir = project.join(".kuro/flows");
    std::fs::create_dir_all(&flow_dir).unwrap();
    let flow_path = flow_dir.join("inline-human-test.yaml");
    std::fs::write(
        &flow_path,
        "version: \"1\"\n\
         name: inline-human-test\n\
         prompt: \"Test the inline human-input path.\"\n\
         initial: design\n\
         graph:\n\
         \x20\x20design:\n\
         \x20\x20\x20\x20role: developer\n\
         \x20\x20\x20\x20task: \"Plan something.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- ask: \"wait for human\"\n\
         \x20\x20\x20\x20\x20\x20- design: \"keep iterating\"\n\
         \x20\x20ask:\n\
         \x20\x20\x20\x20human: true\n\
         \x20\x20\x20\x20task: \"Review the design and respond.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- review: \"after human\"\n\
         \x20\x20review:\n\
         \x20\x20\x20\x20role: reviewer\n\
         \x20\x20\x20\x20task: \"Review what happened.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- done: \"approved\"\n\
         \x20\x20\x20\x20\x20\x20- aborted: \"rejected\"\n\
         \x20\x20done:\n\
         \x20\x20\x20\x20final: \"inline complete\"\n\
         \x20\x20aborted:\n\
         \x20\x20\x20\x20final: \"rejected\"\n",
    )
    .unwrap();
    flow_path
}

/// Ollama shim that prints the canned JSON envelope for each agent
/// state. Logs every prompt so the test can grep for `prior_context`
/// after the run finishes.
fn install_ollama_shim() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("ollama-shim tempdir");
    let log = dir.path().join("log.txt");
    let shim = dir.path().join("ollama");
    let body = r#"case "$PROMPT" in
  *'state `design`'*)
    printf '{"transition": "ask", "reason": "ready for human"}\n'
    ;;
  *'state `review`'*)
    printf '{"transition": "done", "reason": "looks good"}\n'
    ;;
  *)
    printf 'unexpected prompt\n' >&2
    exit 1
    ;;
esac
"#;
    let script = format!(
        "#!/usr/bin/env bash\n\
         set -eu\n\
         LOG='{}'\n\
         PROMPT=\"${{@: -1}}\"\n\
         {{\n\
             printf '\\n--- ollama call ---\\n'\n\
             printf 'PROMPT:\\n%s\\n' \"$PROMPT\"\n\
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

/// Spawn `kuro run` with the test seam env var set so the inline arm
/// fires even though stdin is piped. `stdin_body` is written into the
/// child's stdin pipe before the parent waits on completion.
fn run_kuro_inline(
    project: &Path,
    flow: &Path,
    ollama_shim: &Path,
    home_dir: &Path,
    stdin_body: &str,
    extra_args: &[&str],
) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let host_path = std::env::var("PATH").unwrap_or_default();

    let mut cmd = Command::new(bin);
    cmd.args(["run", "--file"])
        .arg(flow)
        .args(["--var", "id=local-only"])
        .args(extra_args)
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", ollama_shim)
        .env("PATH", &host_path)
        // Test seam: ask the runner to treat piped stdin/stdout as
        // TTYs so the inline policy selector picks `Inline`. The env
        // var is exclusively for tests -- production callers see real
        // `IsTerminal` answers.
        .env("KURO_FORCE_INTERACTIVE_HITL", "1")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn kuro run");
    {
        let mut stdin_handle = child.stdin.take().expect("stdin piped");
        stdin_handle
            .write_all(stdin_body.as_bytes())
            .expect("write stdin body");
        // Drop -> close pipe so the inline reader sees the body
        // followed by EOF (acts as the Ctrl-D terminator).
    }
    child.wait_with_output().expect("wait kuro run")
}

fn latest_run_dir(home: &Path, project_name: &str) -> PathBuf {
    let stacks = home.join(".kuro/stacks").join(project_name);
    std::fs::read_dir(&stacks)
        .unwrap_or_else(|e| panic!("read stacks dir {}: {e}", stacks.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max()
        .unwrap_or_else(|| panic!("no run directories under {}", stacks.display()))
}

#[test]
fn inline_human_completes_run_in_one_invocation() {
    // AC1 + AC4: the run reaches `final_state: done` in a single
    // `kuro run` invocation; the synthetic `kind: human` step body
    // matches the byte-shape the resume path produces (same header,
    // same source-tag format). Stdin carries the operator body and a
    // blank-line terminator; the inline arm reads it, writes
    // 02-ask.md, and continues into `review` -> `done`.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = write_flow(project.path());
    let home = tempfile::tempdir().expect("home tempdir");
    let (_ollama_dir, ollama_shim, ollama_log) = install_ollama_shim();

    let out = run_kuro_inline(
        project.path(),
        &flow,
        &ollama_shim,
        home.path(),
        "approve everything\n\n",
        &[],
    );
    assert!(
        out.status.success(),
        "kuro run with inline body must succeed; status={:?}\nstdout={}\nstderr={}\nollama log:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&ollama_log).unwrap_or_default(),
    );

    let run_dir = latest_run_dir(home.path(), &project_name);
    let manifest = std::fs::read_to_string(run_dir.join("manifest.yaml")).unwrap();
    assert!(
        manifest.contains("final_state: done"),
        "inline run must reach `done` in one invocation; manifest:\n{manifest}"
    );
    assert!(
        !manifest.contains("paused_at_state"),
        "inline run must NOT record a pause; manifest:\n{manifest}"
    );

    // Steps directory: pre-handoff design, synthetic ask, post-handoff
    // review. Pin the exact filenames so a future refactor that drifts
    // the step-numbering convention gets caught.
    let steps_dir = run_dir.join("steps");
    let entries: Vec<String> = std::fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|n| n == "01-design.md"),
        "expected 01-design.md, got: {entries:?}"
    );
    assert!(
        entries.iter().any(|n| n == "02-ask.md"),
        "expected synthetic 02-ask.md (inline human input), got: {entries:?}"
    );
    assert!(
        entries.iter().any(|n| n == "03-review.md"),
        "expected 03-review.md, got: {entries:?}"
    );

    // Synthetic step body shape: same as the resume path
    // (`format_local_human_input`). The source tag is the inline-only
    // value so a downstream auditor can tell which channel produced
    // the body.
    let body = std::fs::read_to_string(steps_dir.join("02-ask.md")).unwrap();
    assert!(
        body.contains("Human input received since pause"),
        "synthetic body must use the documented header, got:\n{body}"
    );
    assert!(
        body.contains("(via stdin (interactive))"),
        "synthetic body must surface the inline source, got:\n{body}"
    );
    assert!(
        body.contains("approve everything"),
        "synthetic body must carry the stdin payload verbatim, got:\n{body}"
    );

    // Meta yaml: synthetic step record carries `type: human` and
    // `step_id: ask` -- byte-identical with what the resume path
    // produces (AC4).
    let meta = std::fs::read_to_string(steps_dir.join("02-ask.meta.yaml")).unwrap();
    assert!(
        meta.contains("type: human"),
        "synthetic step must record `type: human`, got:\n{meta}"
    );
    assert!(
        meta.contains("step_id: ask"),
        "synthetic step must record `step_id: ask`, got:\n{meta}"
    );

    // The reviewer agent (ollama shim) saw the synthetic body via the
    // standard `prior_context` header -- exactly the path the resume
    // flow uses. Without this the on-disk artefact would be the only
    // evidence of the inline handoff; the downstream agent's prompt
    // would not actually see the operator's words.
    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    assert!(
        log.contains("approve everything"),
        "reviewer prompt must include the inline body; ollama log:\n{log}"
    );
    assert!(
        log.contains("Context from previous state 'ask'"),
        "reviewer prompt must use the standard prior_context header; ollama log:\n{log}"
    );
}

#[test]
fn no_interactive_flag_forces_pause_even_with_force_env_set() {
    // AC3: `--no-interactive` overrides the TTY detection (and the
    // test-seam env var) so the run pauses instead of prompting
    // inline. The flow exits with a paused manifest and the pause
    // banner shows the resume hint footer.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let flow = write_flow(project.path());
    let home = tempfile::tempdir().expect("home tempdir");
    let (_ollama_dir, ollama_shim, ollama_log) = install_ollama_shim();

    let out = run_kuro_inline(
        project.path(),
        &flow,
        &ollama_shim,
        home.path(),
        // Body would be read if inline fired; with --no-interactive
        // the runner ignores stdin and pauses instead.
        "this should NOT be read\n\n",
        &["--no-interactive"],
    );
    assert!(
        out.status.success(),
        "kuro run --no-interactive must exit cleanly (paused = success); status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let run_dir = latest_run_dir(home.path(), &project_name);
    let manifest = std::fs::read_to_string(run_dir.join("manifest.yaml")).unwrap();
    assert!(
        manifest.contains("paused_at_state: ask"),
        "--no-interactive must pause at `ask`; manifest:\n{manifest}"
    );

    // Reviewer must not have run -- the pause stops the run before
    // the post-handoff state.
    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    assert!(
        !log.contains("state `review`"),
        "reviewer must not run when --no-interactive pauses the flow; log:\n{log}"
    );

    // No synthetic `02-ask.md` on disk -- the pause arm writes
    // nothing for the human state.
    let steps_dir = run_dir.join("steps");
    let entries: Vec<String> = std::fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().all(|n| n != "02-ask.md"),
        "--no-interactive must NOT write a synthetic human step; got: {entries:?}"
    );

    // The resume hint footer surfaces in stdout (the pause banner
    // uses `println!`). AC2: the operator sees the exact resume
    // command even on the no-interactive path.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let run_id = run_dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        stdout.contains(&format!("kuro resume {run_id}")),
        "pause banner must spell out the resume command; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("--message"),
        "pause banner must mention --message as a channel; stdout:\n{stdout}"
    );
}
