//! Integration coverage for issue #245: `kuro task --agent X` must not
//! claim ownership of the cwd project. Two layers of coverage:
//!
//! * Help-output tests pin the CLI surface (the
//!   `--include-project-context` flag exists on both `kuro task` and
//!   `kuro chat` and is documented).
//! * The fake-claude tests exercise the full pipeline -- cwd-relative
//!   seed resolution, `load_guide_for_task` gate, command assembly,
//!   `sh -c` spawn -- and assert on the argv that actually reaches the
//!   backend. The shim is a tiny `#!/bin/sh` script wired in via the
//!   already-existing `CLAUDE_CLI_PATH` test hook (`src/executor/mod.rs`),
//!   so no LLM or network is required and the test fails with the exact
//!   symptom from the bug report (the cwd Guide body landing in
//!   `--system-prompt`) when the gate regresses.
//!
//! See also: src/runner.rs unit tests
//! `task_system_prompt_omits_cwd_guide_by_default` and
//! `task_system_prompt_includes_guide_when_opted_in`.

use std::process::Command;

fn run_help(subcmd: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let out = Command::new(bin)
        .args([subcmd, "--help"])
        .output()
        .unwrap_or_else(|e| panic!("spawn `kuro {subcmd} --help`: {e}"));
    assert!(
        out.status.success(),
        "`kuro {subcmd} --help` exited non-zero: status={:?}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn task_help_advertises_include_project_context_flag() {
    let help = run_help("task");
    assert!(
        help.contains("--include-project-context"),
        "`kuro task --help` must advertise the opt-in for cwd Guide injection (#245); got:\n{help}"
    );
}

#[test]
fn chat_help_advertises_include_project_context_flag() {
    let help = run_help("chat");
    assert!(
        help.contains("--include-project-context"),
        "`kuro chat --help` must advertise the opt-in for cwd Guide injection (#245); got:\n{help}"
    );
}

// --- Fake-claude integration coverage (issue #245, AC3) ---
//
// Unix-only because the shim relies on `#!/bin/sh` and POSIX file
// permissions. The kuro binary itself is portable, but a Windows host
// would need a different shim and is out of scope for the v1 fix.
#[cfg(unix)]
mod fake_claude {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Sentinel string used as the body of the cwd `Guide.md`. Any
    /// occurrence of this string in the argv that the kuro binary passes
    /// to claude-cli is, by definition, the bug from #245.
    const GUIDE_LEAK_SENTINEL: &str = "PROJECT_GUIDE_LEAK_SENTINEL_kuromaku_245";

    /// Build a project tempdir laid out the way `kuro task` expects --
    /// `.kuro/agents/Test.yaml` plus a `.kuro/Guide.md` containing the
    /// sentinel. The Guide path matches what `load_guide_from_seeds` finds
    /// via the default `.kuro/` seed.
    fn make_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().expect("project tempdir");
        let kuro_dir = project.path().join(".kuro");
        std::fs::create_dir_all(kuro_dir.join("agents")).unwrap();
        std::fs::write(
            kuro_dir.join("Guide.md"),
            format!("You are working on **{GUIDE_LEAK_SENTINEL}** -- a fictional CLI."),
        )
        .unwrap();
        std::fs::write(
            kuro_dir.join("agents/Test.yaml"),
            "name: Test\n\
             role: You are Test, a placeholder persona for the #245 regression test.\n\
             backend: claude-cli\n\
             model: claude-sonnet-4-5\n",
        )
        .unwrap();
        project
    }

    /// Drop a fake `claude` script that records every argv entry to a log
    /// file (one per line) and exits 0 immediately. The kuro binary picks
    /// the path up via `CLAUDE_CLI_PATH` (the existing override hook in
    /// `src/executor/mod.rs::build_claude_command`), so the real claude
    /// binary is never touched.
    fn install_shim() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let shim = tempfile::tempdir().expect("shim tempdir");
        let argv_log = shim.path().join("argv.txt");
        let fake_claude = shim.path().join("claude");
        std::fs::write(
            &fake_claude,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\nexit 0\n",
                argv_log.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake_claude).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_claude, perms).unwrap();
        (shim, fake_claude, argv_log)
    }

    fn run_kuro_task(
        project: &Path,
        fake_claude: &Path,
        extra_args: &[&str],
    ) -> std::process::Output {
        let bin = env!("CARGO_BIN_EXE_kuro");
        let mut cmd = Command::new(bin);
        cmd.args(["task", "--agent", "Test", "-t", "hello"])
            .args(extra_args)
            .current_dir(project)
            .env("CLAUDE_CLI_PATH", fake_claude);
        cmd.output().expect("spawn kuro task")
    }

    fn read_argv(argv_log: &Path, out: &std::process::Output) -> String {
        std::fs::read_to_string(argv_log).unwrap_or_else(|_| {
            panic!(
                "fake claude was never invoked -- kuro task exited before reaching the backend.\n\
                 status={:?}\nstdout={}\nstderr={}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }

    #[test]
    fn task_default_does_not_leak_cwd_guide_to_backend_argv() {
        // AC3 from #245: from a project directory containing `.kuro/Guide.md`,
        // running `kuro task --agent Test -t "hello"` (no opt-in) must NOT
        // pass the Guide body to claude-cli. The shim records the actual argv
        // the executor spawns, so a regression that re-introduces unconditional
        // Guide injection will fail this test with the cwd identity sitting in
        // `--system-prompt`.
        let project = make_project();
        let (_shim, fake_claude, argv_log) = install_shim();

        let no_extra: [&str; 0] = [];
        let out = run_kuro_task(project.path(), &fake_claude, &no_extra);
        let argv = read_argv(&argv_log, &out);

        assert!(
            !argv.contains(GUIDE_LEAK_SENTINEL),
            "kuro task default must not pass the cwd Guide to the backend (#245).\n\
             fake-claude argv:\n{argv}"
        );
    }

    #[test]
    fn task_opt_in_passes_cwd_guide_to_backend_argv() {
        // Symmetric coverage: when the user explicitly opts in via
        // `--include-project-context`, the same setup MUST inject the Guide
        // into `--system-prompt`. Pins the opt-in path next to the regression
        // so a future change that breaks one fails next to the other.
        let project = make_project();
        let (_shim, fake_claude, argv_log) = install_shim();

        let extra = ["--include-project-context"];
        let out = run_kuro_task(project.path(), &fake_claude, &extra);
        let argv = read_argv(&argv_log, &out);

        assert!(
            argv.contains(GUIDE_LEAK_SENTINEL),
            "kuro task with --include-project-context must inject the cwd Guide (#245).\n\
             fake-claude argv:\n{argv}"
        );
    }
}
