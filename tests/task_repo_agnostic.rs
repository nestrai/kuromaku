//! Integration coverage for issue #245: `kuro task --agent X` must not
//! claim ownership of the cwd project. The unit tests in
//! `src/runner.rs` (task_system_prompt_omits_cwd_guide_by_default) already
//! prove the system-prompt assembly skips the Guide. This file pins the
//! CLI surface that callers see -- the `--include-project-context` flag
//! exists on both `kuro task` and `kuro chat`, and is documented in their
//! help output. End-to-end runs against real backends are out of scope
//! (they need an LLM).
//!
//! Spawning the binary with `--help` is cheap, no LLM is touched, and a
//! future change that drops the flag (regressing #245) fails this test
//! before any agent invocation gets the chance to leak project identity.
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
