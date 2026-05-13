//! Integration test for issue #340: inject human response as `prior_context`
//! on resume.
//!
//! Acceptance:
//! 1. On resume, `gh issue view <id> --json comments` is called.
//! 2. Comments with `created_at >= paused_at` land in the next agent's
//!    `prior_context`.
//! 3. No new comments -> empty prior_context, no error.
//! 4. `gh` errors fall back to empty prior_context with a warning.
//!
//! Strategy: install a `gh` shim that prints canned JSON, run `kuro run`
//! to pause at a human state, then `kuro resume` -- which uses the real
//! `gh_comments_fetcher` and therefore picks up the shim. The next agent
//! state runs through the ollama shim, which logs every prompt; the test
//! greps that log for the formatted human-input body.
//!
//! Pattern mirrors `tests/graph_smoke.rs` so the scaffolding stays
//! recognisable. Distinct file so #340-specific failures surface in the
//! test output without having to disambiguate from #338's resume tests.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Project dir with developer + reviewer role bindings -- the pre-pause
/// `design` step uses developer, the post-resume `review` step uses
/// reviewer. Both bind to ollama-backed shim agents so the same shim
/// catches every state's invocation.
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
                 role: Placeholder persona for the human-input integration test.\n\
                 backend: ollama\n\
                 model: test-model\n"
            ),
        )
        .unwrap();
    }

    project
}

/// Resumable flow: `design (developer) -> ask (human, next: review) ->
/// review (reviewer) -> done`. The `review` state is what reads the
/// synthetic human-input prior_context, so its prompt is what the test
/// asserts on. Filename matches the `name:` field so the seed resolver
/// finds it on resume (which has no `--file`).
fn write_flow(project: &Path) -> PathBuf {
    let flow_dir = project.join(".kuro/flows");
    std::fs::create_dir_all(&flow_dir).unwrap();
    let flow_path = flow_dir.join("human-input-test.yaml");
    std::fs::write(
        &flow_path,
        "version: \"1\"\n\
         name: human-input-test\n\
         prompt: \"Test the human-input injection path.\"\n\
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
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- review: \"after human\"\n\
         \x20\x20review:\n\
         \x20\x20\x20\x20role: reviewer\n\
         \x20\x20\x20\x20task: \"Review what happened.\"\n\
         \x20\x20\x20\x20next:\n\
         \x20\x20\x20\x20\x20\x20- done: \"approved\"\n\
         \x20\x20\x20\x20\x20\x20- aborted: \"rejected\"\n\
         \x20\x20done:\n\
         \x20\x20\x20\x20final: \"resumed terminal\"\n\
         \x20\x20aborted:\n\
         \x20\x20\x20\x20final: \"rejected\"\n",
    )
    .unwrap();
    flow_path
}

/// Install the ollama shim. Branches on the state name in the prompt
/// and prints the canned `{transition, reason}` envelope. Logs every
/// prompt to `<shim_dir>/log.txt` so the test can later grep for the
/// human-input body.
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

/// Install a `gh` shim that prints canned JSON for `issue view --json
/// comments`. `comments_json` is whatever the shim should emit for the
/// `comments` field; the rest of the JSON shape is wrapped here.
///
/// Other `gh` subcommands return success with empty stdout. The
/// production fetch_issue_summary path also calls `gh`; keeping its
/// branch silent prevents surprise output from leaking into the test
/// assertions.
fn install_gh_shim(comments_json: &str, dir_for_log: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("gh-shim tempdir");
    let log = dir_for_log.join("gh-log.txt");
    let shim = dir.path().join("gh");
    let comments_payload = format!(r#"{{"comments":{comments_json}}}"#);
    let script = format!(
        "#!/usr/bin/env bash\n\
         set -eu\n\
         LOG='{}'\n\
         {{\n\
             printf '\\n--- gh call ---\\n'\n\
             printf 'argv:'\n\
             for a in \"$@\"; do printf ' %s' \"$a\"; done\n\
             printf '\\n'\n\
         }} >> \"$LOG\"\n\
         # Only the `--json comments` shape produces our payload.\n\
         # `--json title,url,body` (banner) and `--json body` (#337 hash) get\n\
         # an empty-but-valid JSON object so they neither crash nor pollute\n\
         # the integration test fixture.\n\
         for a in \"$@\"; do\n\
         \x20\x20if [ \"$a\" = \"comments\" ]; then\n\
         \x20\x20\x20\x20printf '%s' '{}'\n\
         \x20\x20\x20\x20exit 0\n\
         \x20\x20fi\n\
         done\n\
         printf '{{}}'\n\
         exit 0\n",
        log.display(),
        comments_payload,
    );
    std::fs::write(&shim, script).unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();
    (dir, shim)
}

/// Spawn `kuro run --file <flow> --var id=139` in `project`. The fixed
/// `id=139` lets the synthetic-step path run in resume.
fn run_kuro(
    project: &Path,
    flow: &Path,
    ollama_shim: &Path,
    gh_dir: &Path,
    home_dir: &Path,
) -> std::process::Output {
    run_kuro_with_id(project, flow, ollama_shim, gh_dir, home_dir, "139")
}

/// `run_kuro` parametrised over the `id` template var (issue #360).
///
/// Non-numeric ids (e.g. `local-only`) skip the GH-comments path on
/// resume, so the local-input branch is exercised in isolation. Numeric
/// ids preserve the legacy #340 behaviour.
fn run_kuro_with_id(
    project: &Path,
    flow: &Path,
    ollama_shim: &Path,
    gh_dir: &Path,
    home_dir: &Path,
    id_var: &str,
) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let host_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{host_path}", gh_dir.display());
    Command::new(bin)
        .args(["run", "--file"])
        .arg(flow)
        .args(["--var", &format!("id={id_var}")])
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", ollama_shim)
        .env("PATH", path)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn kuro run")
}

/// Spawn `kuro resume <run-id>` in `project`. Same env shape as
/// `run_kuro`; resume is what triggers the gh-comments fetch.
fn resume_kuro(
    project: &Path,
    run_id: &str,
    ollama_shim: &Path,
    gh_dir: &Path,
    home_dir: &Path,
) -> std::process::Output {
    spawn_resume(project, run_id, ollama_shim, gh_dir, home_dir, &[], None)
}

/// `resume_kuro` with extra args appended after `<run-id>` and an
/// optional stdin payload (issue #360).
///
/// `extra_args` carries flags like `["--message", "approve"]`; `stdin_in`
/// pipes a body in when provided. The test seam is in [`spawn_resume`]
/// so the existing GH-only tests stay readable and the local-input
/// tests do not have to copy the env wiring.
fn spawn_resume(
    project: &Path,
    run_id: &str,
    ollama_shim: &Path,
    gh_dir: &Path,
    home_dir: &Path,
    extra_args: &[&str],
    stdin_in: Option<&str>,
) -> std::process::Output {
    use std::io::Write as _;
    use std::process::Stdio;

    let bin = env!("CARGO_BIN_EXE_kuro");
    let host_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{host_path}", gh_dir.display());
    let mut cmd = Command::new(bin);
    cmd.args(["resume", run_id])
        .args(extra_args)
        .current_dir(project)
        .env("HOME", home_dir)
        .env("OLLAMA_PATH", ollama_shim)
        .env("PATH", path)
        .env_remove("RUST_LOG");

    // Pipe stdin when the test provides a body; otherwise close stdin
    // (Stdio::null) so the binary's non-TTY-stdin path observes EOF
    // rather than inheriting the test runner's stdin, which on some CI
    // setups can stall the read.
    cmd.stdin(if stdin_in.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kuro resume");
    if let Some(body) = stdin_in {
        let mut stdin_handle = child.stdin.take().expect("stdin piped");
        stdin_handle
            .write_all(body.as_bytes())
            .expect("write stdin body");
        // Drop the handle to close stdin so the child sees EOF.
        drop(stdin_handle);
    }
    child.wait_with_output().expect("wait kuro resume")
}

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
fn resume_injects_human_comments_as_prior_context() {
    // Acceptance (#340): a comment added between pause and resume must
    // surface in the next agent's prior_context using the same framing
    // shell-state outputs use. We verify that:
    // * a synthetic `kind: human` step record landed on disk for `ask`
    // * the reviewer's prompt (logged by the ollama shim) contains the
    //   formatted human-input body
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

    // Phase 1: kuro run -- pauses at `ask`. Use a gh shim that returns
    // no comments yet (so even if the run path queries gh, no human
    // input surfaces pre-pause).
    let (_gh_pre_dir, gh_pre_shim) = install_gh_shim("[]", _ollama_dir.path());
    let gh_pre_dir = gh_pre_shim.parent().unwrap();
    let run_out = run_kuro(project.path(), &flow, &ollama_shim, gh_pre_dir, home.path());
    assert!(
        run_out.status.success(),
        "phase 1 kuro run must succeed; status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr),
        std::fs::read_to_string(&ollama_log).unwrap_or_default(),
    );

    let run_dir = latest_run_dir(home.path(), &project_name);
    let run_id = run_dir.file_name().unwrap().to_string_lossy().to_string();
    let manifest = std::fs::read_to_string(run_dir.join("manifest.yaml")).unwrap();
    assert!(
        manifest.contains("paused_at_state: ask"),
        "phase 1 must record paused_at_state: ask, got:\n{manifest}"
    );

    // Phase 2: kuro resume -- gh shim now returns one canned comment
    // dated AFTER any plausible paused_at value. The synthetic step
    // gets written, the reviewer reads it as prior_context.
    //
    // The timestamp is intentionally far in the future so it is `>=
    // paused_at` regardless of the wall-clock time of phase 1.
    let comments_json = r#"[
      {"author": {"login": "human-reviewer"},
       "createdAt": "2099-01-01T00:00:00Z",
       "body": "the missing piece is FOOBAR_TOKEN"}
    ]"#;
    let (_gh_post_dir, gh_post_shim) = install_gh_shim(comments_json, _ollama_dir.path());
    let gh_post_dir = gh_post_shim.parent().unwrap();

    let resume_out = resume_kuro(
        project.path(),
        &run_id,
        &ollama_shim,
        gh_post_dir,
        home.path(),
    );
    assert!(
        resume_out.status.success(),
        "phase 2 kuro resume must succeed; status={:?}\nstdout={}\nstderr={}\nollama log:\n{}\ngh log:\n{}",
        resume_out.status,
        String::from_utf8_lossy(&resume_out.stdout),
        String::from_utf8_lossy(&resume_out.stderr),
        std::fs::read_to_string(&ollama_log).unwrap_or_default(),
        std::fs::read_to_string(_ollama_dir.path().join("gh-log.txt")).unwrap_or_default(),
    );

    // The synthetic step record must exist on disk with `type: human`
    // and `step_id: ask` -- the driver's `read_run_step_content` lookup
    // keys off step_id, not filename, so the meta yaml must say `ask`.
    let steps_dir = run_dir.join("steps");
    let entries: Vec<String> = std::fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let synth_meta = entries
        .iter()
        .find(|n| n.contains("-ask.meta.yaml"))
        .unwrap_or_else(|| {
            panic!("expected NN-ask.meta.yaml under {steps_dir:?}, got: {entries:?}")
        });
    let meta = std::fs::read_to_string(steps_dir.join(synth_meta)).unwrap();
    assert!(
        meta.contains("type: human"),
        "synthetic step must record `type: human`, got:\n{meta}"
    );
    assert!(
        meta.contains("step_id: ask"),
        "synthetic step must record `step_id: ask`, got:\n{meta}"
    );

    // The synthetic body must be the formatted human-input block.
    let body_path = entries
        .iter()
        .find(|n| n.contains("-ask.md"))
        .map(|n| steps_dir.join(n))
        .unwrap_or_else(|| panic!("expected NN-ask.md under {steps_dir:?}, got: {entries:?}"));
    let body = std::fs::read_to_string(&body_path).unwrap();
    assert!(
        body.contains("FOOBAR_TOKEN"),
        "synthetic body must include the canned comment, got:\n{body}"
    );
    assert!(
        body.contains("Human input received since pause"),
        "synthetic body must use the documented header, got:\n{body}"
    );
    assert!(
        body.contains("@human-reviewer"),
        "synthetic body must surface the comment author, got:\n{body}"
    );

    // The reviewer agent's prompt (captured by the ollama shim) must
    // include the synthetic body via the standard `prior_context`
    // header. Without this, the human-input plumbing would have written
    // a step record on disk but not actually injected anything into the
    // next agent's prompt -- which is exactly the bug #340 closes.
    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    // Find the review-state call in the log and assert it carries the
    // human-input body.
    assert!(
        log.contains("FOOBAR_TOKEN"),
        "reviewer prompt must include the human-input body; ollama log:\n{log}"
    );
    assert!(
        log.contains("Context from previous state 'ask'"),
        "reviewer prompt must use the standard prior_context header; ollama log:\n{log}"
    );
}

#[test]
fn resume_with_no_new_comments_keeps_today_behavior() {
    // Acceptance (#340): "If no new comments exist, prior_context is
    // empty (not an error)." Resume must succeed with no synthetic step
    // on disk; the reviewer's prompt must NOT carry a prior_context
    // block referencing `ask`.
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

    // gh shim returns no comments at all -- both pre and post pause.
    let (_gh_dir, gh_shim) = install_gh_shim("[]", _ollama_dir.path());
    let gh_dir = gh_shim.parent().unwrap();

    let run_out = run_kuro(project.path(), &flow, &ollama_shim, gh_dir, home.path());
    assert!(run_out.status.success());

    let run_dir = latest_run_dir(home.path(), &project_name);
    let run_id = run_dir.file_name().unwrap().to_string_lossy().to_string();

    let resume_out = resume_kuro(project.path(), &run_id, &ollama_shim, gh_dir, home.path());
    assert!(
        resume_out.status.success(),
        "resume with empty comments must succeed; stderr={}\nollama log:\n{}",
        String::from_utf8_lossy(&resume_out.stderr),
        std::fs::read_to_string(&ollama_log).unwrap_or_default(),
    );

    // No synthetic ask step record was written.
    let steps_dir = run_dir.join("steps");
    let entries: Vec<String> = std::fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries
            .iter()
            .all(|n| !n.ends_with("-ask.md") && !n.ends_with("-ask.meta.yaml")),
        "no synthetic step must land on disk when there are no new comments; got: {entries:?}"
    );

    // The reviewer's prompt must NOT contain a prior_context header
    // for `ask` -- empty is empty, not a placeholder.
    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    assert!(
        !log.contains("Context from previous state 'ask'"),
        "reviewer prompt must NOT carry an `ask` prior_context block when no comments fired; got:\n{log}"
    );
}

// --- #360: local human input (--message / stdin / hard error) ------------

/// Phase 1 helper for the #360 tests: spin up a project pinned to a
/// non-numeric `vars.id` so the GH-comments path on resume short-circuits.
/// Returns the captured run_id.
fn pause_run_with_non_numeric_id(
    project: &tempfile::TempDir,
    home: &tempfile::TempDir,
    ollama_shim: &Path,
    ollama_log: &Path,
    gh_dir: &Path,
) -> String {
    let flow = write_flow(project.path());
    let run_out = run_kuro_with_id(
        project.path(),
        &flow,
        ollama_shim,
        gh_dir,
        home.path(),
        "local-only",
    );
    assert!(
        run_out.status.success(),
        "phase 1 kuro run must succeed; status={:?}\nstdout={}\nstderr={}\nshim log:\n{}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr),
        std::fs::read_to_string(ollama_log).unwrap_or_default(),
    );
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let run_dir = latest_run_dir(home.path(), &project_name);
    let manifest = std::fs::read_to_string(run_dir.join("manifest.yaml")).unwrap();
    assert!(
        manifest.contains("paused_at_state: ask"),
        "phase 1 must record paused_at_state: ask, got:\n{manifest}"
    );
    run_dir.file_name().unwrap().to_string_lossy().to_string()
}

/// Walk a run's `steps/` directory and return the on-disk body of the
/// synthetic step that records the paused state's human input. Panics if
/// no body file exists -- callers use this only on success paths.
fn read_synthetic_human_body(home: &Path, project_name: &str, state_id: &str) -> String {
    let stacks = home.join(".koto/stacks").join(project_name);
    let run_dir = std::fs::read_dir(&stacks)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max()
        .expect("at least one run dir");
    let steps_dir = run_dir.join("steps");
    let suffix = format!("-{state_id}.md");
    let body_path = std::fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a NN-{state_id}.md under {}",
                steps_dir.display()
            )
        });
    std::fs::read_to_string(body_path).unwrap()
}

#[test]
fn resume_injects_message_flag_as_prior_context() {
    // Acceptance criterion: `kuro resume <run-id> --message "..."` writes
    // the body as a `kind: human` step and surfaces it to the next agent
    // as prior_context. The flow's `vars.id` is non-numeric so the GH
    // path short-circuits -- the only source of human input is --message.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let home = tempfile::tempdir().expect("home tempdir");
    let (_ollama_dir, ollama_shim, ollama_log) = install_ollama_shim();
    // Benign gh shim -- non-numeric id means the runner never asks for
    // comments, but having a working shim on PATH defends against
    // accidental calls polluting the assertion.
    let (_gh_dir, gh_shim) = install_gh_shim("[]", _ollama_dir.path());
    let gh_dir = gh_shim.parent().unwrap();

    let run_id =
        pause_run_with_non_numeric_id(&project, &home, &ollama_shim, &ollama_log, gh_dir);

    let resume_out = spawn_resume(
        project.path(),
        &run_id,
        &ollama_shim,
        gh_dir,
        home.path(),
        &["--message", "ship it"],
        None,
    );
    assert!(
        resume_out.status.success(),
        "kuro resume --message must succeed; status={:?}\nstdout={}\nstderr={}\nollama log:\n{}",
        resume_out.status,
        String::from_utf8_lossy(&resume_out.stdout),
        String::from_utf8_lossy(&resume_out.stderr),
        std::fs::read_to_string(&ollama_log).unwrap_or_default(),
    );

    // Synthetic body persisted with the documented header shape -- the
    // `(via --message)` tag is the audit signal that distinguishes a
    // local body from a GH-comment body.
    let body = read_synthetic_human_body(home.path(), &project_name, "ask");
    assert!(
        body.contains("Human input received since pause"),
        "synthetic body must use the documented header, got:\n{body}"
    );
    assert!(
        body.contains("(via --message)"),
        "synthetic body must surface the local source, got:\n{body}"
    );
    assert!(
        body.contains("ship it"),
        "synthetic body must carry the message verbatim, got:\n{body}"
    );

    // The reviewer's prompt carries the body via the standard
    // prior_context header, identical to the GH path.
    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    assert!(
        log.contains("ship it"),
        "reviewer prompt must include the --message body; ollama log:\n{log}"
    );
    assert!(
        log.contains("Context from previous state 'ask'"),
        "reviewer prompt must use the standard prior_context header; ollama log:\n{log}"
    );
}

#[test]
fn resume_injects_stdin_as_prior_context() {
    // Acceptance criterion: `echo "approve" | kuro resume <run-id>`
    // delivers the piped body the same way `--message` does. The stdin
    // path is the one operators reach for by default in shell pipelines,
    // so it gets its own end-to-end assertion rather than only the
    // resolver unit test.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let home = tempfile::tempdir().expect("home tempdir");
    let (_ollama_dir, ollama_shim, ollama_log) = install_ollama_shim();
    let (_gh_dir, gh_shim) = install_gh_shim("[]", _ollama_dir.path());
    let gh_dir = gh_shim.parent().unwrap();

    let run_id =
        pause_run_with_non_numeric_id(&project, &home, &ollama_shim, &ollama_log, gh_dir);

    let resume_out = spawn_resume(
        project.path(),
        &run_id,
        &ollama_shim,
        gh_dir,
        home.path(),
        &[],
        Some("approve\n"),
    );
    assert!(
        resume_out.status.success(),
        "kuro resume via stdin must succeed; status={:?}\nstdout={}\nstderr={}\nollama log:\n{}",
        resume_out.status,
        String::from_utf8_lossy(&resume_out.stdout),
        String::from_utf8_lossy(&resume_out.stderr),
        std::fs::read_to_string(&ollama_log).unwrap_or_default(),
    );

    let body = read_synthetic_human_body(home.path(), &project_name, "ask");
    assert!(
        body.contains("(via stdin)"),
        "synthetic body must surface stdin as the source, got:\n{body}"
    );
    assert!(
        body.contains("approve"),
        "synthetic body must carry the stdin payload, got:\n{body}"
    );

    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    assert!(
        log.contains("approve"),
        "reviewer prompt must include the stdin body; ollama log:\n{log}"
    );
    assert!(
        log.contains("Context from previous state 'ask'"),
        "reviewer prompt must carry the prior_context header; ollama log:\n{log}"
    );
}

#[test]
fn resume_errors_when_no_input_and_no_gh_source() {
    // Acceptance criterion: with no `--message`, no piped stdin, and a
    // non-numeric `vars.id`, the operator has no input channel -- the
    // command must exit non-zero with an actionable hint, not silently
    // route through `next[0]`.
    let project = make_project();
    let home = tempfile::tempdir().expect("home tempdir");
    let (_ollama_dir, ollama_shim, ollama_log) = install_ollama_shim();
    let (_gh_dir, gh_shim) = install_gh_shim("[]", _ollama_dir.path());
    let gh_dir = gh_shim.parent().unwrap();

    let run_id =
        pause_run_with_non_numeric_id(&project, &home, &ollama_shim, &ollama_log, gh_dir);

    let resume_out = spawn_resume(
        project.path(),
        &run_id,
        &ollama_shim,
        gh_dir,
        home.path(),
        &[],
        None, // closes stdin so the non-TTY-stdin path observes EOF
    );
    assert!(
        !resume_out.status.success(),
        "kuro resume with no input + non-numeric id must fail; status={:?}\nstdout={}",
        resume_out.status,
        String::from_utf8_lossy(&resume_out.stdout),
    );
    let stderr = String::from_utf8_lossy(&resume_out.stderr);
    assert!(
        stderr.contains("no human input provided"),
        "stderr must explain the missing input, got:\n{stderr}"
    );
    assert!(
        stderr.contains("--message") && stderr.contains("stdin"),
        "stderr must hint at the recovery paths, got:\n{stderr}"
    );

    // The reviewer state must NOT have been invoked -- the run aborts
    // before routing past the human handoff.
    let log = std::fs::read_to_string(&ollama_log).unwrap_or_default();
    assert!(
        !log.contains("state `review`"),
        "reviewer must not run when resume aborts; ollama log:\n{log}"
    );
}

#[test]
fn resume_message_flag_wins_over_gh_comments() {
    // Pins design decision B: with both a GH source (numeric `vars.id`
    // + non-empty comments) AND a local `--message`, the local body
    // becomes the synthetic step content and a `[warn]` surfaces the
    // conflict on stderr. The acceptance criterion phrases this as
    // "behaviour is unchanged when GH is the only source"; this test
    // adds the converse case to lock the precedence rule.
    let project = make_project();
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let home = tempfile::tempdir().expect("home tempdir");
    let (_ollama_dir, ollama_shim, _ollama_log) = install_ollama_shim();

    // Phase 1: pause with numeric id=139 + empty pre-pause comments.
    let (_gh_pre_dir, gh_pre_shim) = install_gh_shim("[]", _ollama_dir.path());
    let gh_pre_dir = gh_pre_shim.parent().unwrap();
    let flow = write_flow(project.path());
    let run_out = run_kuro(project.path(), &flow, &ollama_shim, gh_pre_dir, home.path());
    assert!(
        run_out.status.success(),
        "phase 1 kuro run must succeed; stderr={}",
        String::from_utf8_lossy(&run_out.stderr),
    );
    let run_dir = latest_run_dir(home.path(), &project_name);
    let run_id = run_dir.file_name().unwrap().to_string_lossy().to_string();

    // Phase 2: gh shim now returns a post-pause comment AND we pass
    // --message. Local must win.
    let comments_json = r#"[
      {"author": {"login": "human-reviewer"},
       "createdAt": "2099-01-01T00:00:00Z",
       "body": "GH_COMMENT_BODY"}
    ]"#;
    let (_gh_post_dir, gh_post_shim) = install_gh_shim(comments_json, _ollama_dir.path());
    let gh_post_dir = gh_post_shim.parent().unwrap();

    let resume_out = spawn_resume(
        project.path(),
        &run_id,
        &ollama_shim,
        gh_post_dir,
        home.path(),
        &["--message", "override-body"],
        None,
    );
    assert!(
        resume_out.status.success(),
        "resume must succeed in conflict case; stderr={}",
        String::from_utf8_lossy(&resume_out.stderr),
    );

    // Local body wins -- GH body must not appear in the synthetic step.
    let body = read_synthetic_human_body(home.path(), &project_name, "ask");
    assert!(
        body.contains("override-body"),
        "synthetic body must carry local --message verbatim, got:\n{body}"
    );
    assert!(
        !body.contains("GH_COMMENT_BODY"),
        "synthetic body must NOT carry GH comment when --message is set, got:\n{body}"
    );
    assert!(
        body.contains("(via --message)"),
        "synthetic body must surface the local source, got:\n{body}"
    );

    // The `[warn]` line names the conflict on stderr so the operator
    // sees that GH comments were available but ignored.
    let stderr = String::from_utf8_lossy(&resume_out.stderr);
    assert!(
        stderr.contains("[warn]")
            && stderr.contains("local input")
            && stderr.contains("GitHub comments"),
        "stderr must surface the local-vs-GH conflict warning, got:\n{stderr}"
    );
}
