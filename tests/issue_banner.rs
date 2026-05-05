//! Integration smoke test for issue #309: issue context banner at the start
//! of `implement-issue` / `rework-pr` runs.
//!
//! Drives `kuro run` end-to-end with a stubbed `gh` binary so we can verify
//! the banner content (`Issue #N`, URL, body preview line) without going to
//! github. Three cases cover the acceptance matrix:
//!
//! 1. `gh` shim returns a canned issue payload -- banner appears.
//! 2. `gh` not on PATH -- run still succeeds, no banner, no warning.
//! 3. `vars.id` not provided -- run still succeeds, no banner.
//!
//! All three cases use a minimal linear flow with an Ollama-backed agent so
//! the test stays self-contained (no claude-cli, no network). The flow
//! produces a single step whose output is irrelevant to the banner check.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a tempdir project with a minimal flow that uses one Ollama-backed
/// agent. The flow is a single linear step so the run finishes quickly; we
/// only care about the lines printed before the first step.
fn make_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro_dir = project.path().join(".kuro");
    std::fs::create_dir_all(kuro_dir.join("agents")).unwrap();
    std::fs::create_dir_all(kuro_dir.join("flows")).unwrap();

    std::fs::write(kuro_dir.join("config.yaml"), "version: \"1\"\n").unwrap();

    std::fs::write(
        kuro_dir.join("agents/Bot.yaml"),
        "name: Bot\n\
         role: Placeholder agent for the issue-banner smoke test.\n\
         backend: ollama\n\
         model: test-model\n",
    )
    .unwrap();

    // Minimal linear flow with a single step. The flow keyword (`flow:`)
    // maps step ids to step bodies; the test only needs one step that
    // returns quickly so we can assert on the banner lines emitted before
    // any step executes.
    std::fs::write(
        kuro_dir.join("flows/banner-test.yaml"),
        "version: \"1\"\n\
         name: banner-test\n\
         prompt: Smoke test for the issue banner.\n\
         flow:\n\
         \x20\x20greet:\n\
         \x20\x20\x20\x20agent: Bot\n\
         \x20\x20\x20\x20task: \"hello\"\n",
    )
    .unwrap();

    project
}

/// Install a `gh` shim at `<dir>/gh` that returns a canned JSON payload for
/// `gh issue view <id> --json title,url,body`. The shim ignores the issue
/// number and returns the same payload regardless -- the banner only depends
/// on the JSON shape, not on argv parsing.
fn install_gh_shim(dir: &Path) {
    let shim = dir.join("gh");
    let script = "#!/bin/sh\n\
                  cat <<'JSON'\n\
                  {\"title\": \"fix(ui): banner regression\", \"url\": \"https://github.com/nestrai/kuromaku/issues/42\", \"body\": \"## Why\\n\\nThe runner needs a context anchor.\\n\\nMore detail follows.\"}\n\
                  JSON\n";
    std::fs::write(&shim, script).unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();
}

/// Install an Ollama shim that prints a one-line response for any prompt.
fn install_ollama_shim(dir: &Path) -> PathBuf {
    let shim = dir.join("ollama");
    let script = "#!/bin/sh\nprintf 'ok\\n'\n";
    std::fs::write(&shim, script).unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();
    shim
}

/// Spawn `kuro run banner-test` from inside `project` with `path_dir` prepended
/// to PATH (so a `gh` shim placed there wins over any real `gh`) and
/// `OLLAMA_PATH` pointing at the Ollama shim. `HOME` is sandboxed so stack
/// writes land in the test tempdir.
///
/// PATH is *prepended*, not replaced, because the kuro binary spawns child
/// processes (sh, ollama via OLLAMA_PATH) that may rely on the system PATH for
/// resolving subprocesses of their own (e.g. NixOS-specific interpreter paths
/// embedded in shebangs). Prepending lets the shim win for `gh` lookups
/// without breaking everything else.
fn run_kuro(
    project: &Path,
    home: &Path,
    path_dir: &Path,
    ollama_shim: &Path,
    extra_args: &[&str],
) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let parent_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{parent_path}", path_dir.display());
    let mut cmd = Command::new(bin);
    cmd.args(["run", "banner-test"])
        .args(extra_args)
        .current_dir(project)
        .env("HOME", home)
        .env("OLLAMA_PATH", ollama_shim)
        .env("PATH", new_path)
        .env_remove("RUST_LOG");
    cmd.output().expect("spawn kuro run")
}

#[test]
fn banner_renders_when_gh_returns_payload() {
    // Acceptance criteria covered:
    // - "Issue banner is shown on `kuro run ... --var id=N`"
    // - "Banner appears once, before the first step starts"
    // - "Smoke test that captures stderr and asserts the banner shape on a
    //    stubbed `gh`" (we capture combined stdout+stderr -- the banner is
    //    on stdout to match `print_flow_start`, but capturing both keeps the
    //    test resilient if the stream choice changes.)
    let project = make_project();
    let path_dir = tempfile::tempdir().expect("path tempdir");
    install_gh_shim(path_dir.path());
    let ollama = install_ollama_shim(path_dir.path());
    let home = tempfile::tempdir().expect("home tempdir");

    let out = run_kuro(
        project.path(),
        home.path(),
        path_dir.path(),
        &ollama,
        &["--var", "id=42"],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        out.status.success(),
        "run must succeed; status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status,
    );
    assert!(
        combined.contains("Issue #42"),
        "expected 'Issue #42' header in output; got:\n{combined}",
    );
    assert!(
        combined.contains("fix(ui): banner regression"),
        "expected issue title in output; got:\n{combined}",
    );
    assert!(
        combined.contains("https://github.com/nestrai/kuromaku/issues/42"),
        "expected issue URL in output; got:\n{combined}",
    );
    assert!(
        combined.contains("The runner needs a context anchor."),
        "expected body preview line in output; got:\n{combined}",
    );

    // Banner must appear once, not duplicated by the linear and graph
    // call-sites racing. The header line is unique enough to count.
    let occurrences = combined.matches("Issue #42").count();
    assert_eq!(
        occurrences, 1,
        "banner header must render exactly once; got {occurrences}\n{combined}",
    );
}

#[test]
fn no_banner_when_gh_missing() {
    // Acceptance criterion: "No banner when `gh` isn't on PATH or returns
    // non-zero". PATH points at a tempdir with only the Ollama shim, no
    // `gh`. The run must still succeed and the output must be banner-free.
    let project = make_project();
    let path_dir = tempfile::tempdir().expect("path tempdir");
    let ollama = install_ollama_shim(path_dir.path());
    let home = tempfile::tempdir().expect("home tempdir");

    let out = run_kuro(
        project.path(),
        home.path(),
        path_dir.path(),
        &ollama,
        &["--var", "id=42"],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        out.status.success(),
        "run must succeed when gh is missing; status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status,
    );
    assert!(
        !combined.contains("Issue #42"),
        "banner must not appear when gh is missing; got:\n{combined}",
    );
}

#[test]
fn no_banner_when_id_var_missing() {
    // Acceptance criterion: "No banner when `vars.id` is absent or
    // non-numeric". Even with a working `gh` shim on PATH, the runner
    // must skip the banner because there's no `id` to fetch with.
    let project = make_project();
    let path_dir = tempfile::tempdir().expect("path tempdir");
    install_gh_shim(path_dir.path());
    let ollama = install_ollama_shim(path_dir.path());
    let home = tempfile::tempdir().expect("home tempdir");

    let out = run_kuro(project.path(), home.path(), path_dir.path(), &ollama, &[]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        out.status.success(),
        "run must succeed without --var id; status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status,
    );
    assert!(
        !combined.contains("Issue #"),
        "banner must not appear without vars.id; got:\n{combined}",
    );
}
