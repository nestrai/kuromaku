//! Integration coverage for issue #238: `kuro validate <flow>` and the
//! pre-flight gate inside `kuro run` for graph flows.
//!
//! These tests drive the real `kuro` binary so they pin the user-visible
//! contract:
//!
//! * exit codes (`zero` for clean / unreachable-only, non-zero for
//!   dead-ends and other errors)
//! * stdout/stderr discipline (warnings + errors land on stderr; stdout
//!   stays clean for machine-readable use)
//! * graph-aware error message when `kuro run` is pointed at a graph
//!   flow with a dead-end (no agent spawn, message names the state)
//!
//! Unit-level coverage of the validator itself lives in
//! `src/config.rs::tests::validate_*`.

use std::path::PathBuf;
use std::process::Command;

fn kuro_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kuro")
}

fn write_flow(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write flow yaml");
    path
}

const CLEAN_GRAPH: &str = r#"
version: "1"
name: clean
initial: start
states:
  start:
    role: developer
    edges:
      ok:
        to: done
        description: Looks good.
  done:
    kind: final
"#;

/// A graph YAML with a dead-end. Schema validation alone would reject
/// `dead:` (no edges, no kind), so we use a `kind: human` state that
/// has empty edges -- but the *real* dead-end is a separate state we
/// reach via the start state. To express a true post-schema dead end
/// we'd need to bypass the YAML parser; instead we lean on the linear
/// runner's existing graph-shape check by giving the validator a graph
/// that the schema parser will load but where one state lacks edges
/// and is not terminal.
///
/// Trick: a `kind: human` state with empty edges parses cleanly (and
/// is NOT a dead end -- terminal-ish). To get a true dead end we'd
/// need a state with neither edges nor kind, which the schema rejects.
/// So at the CLI level we cannot express a "schema-clean,
/// validator-failing dead-end graph" without bypassing the parser.
///
/// Solution: the integration test for the dead-end exit code uses an
/// *unknown initial state*, which the schema validator catches and
/// `kuro validate` propagates as a non-zero exit. That covers AC4
/// (non-zero exit on validation failure). The dead-end logic itself
/// is exercised by the unit tests in `src/config.rs`, where we can
/// construct the offending shape programmatically.
const SCHEMA_INVALID_GRAPH: &str = r#"
version: "1"
name: bad-initial
initial: nowhere
states:
  done:
    kind: final
"#;

const UNREACHABLE_GRAPH: &str = r#"
version: "1"
name: unreachable
initial: start
states:
  start:
    role: developer
    edges:
      ok:
        to: done
        description: Done.
  done:
    kind: final
  orphan:
    role: reviewer
    edges:
      back:
        to: done
        description: Loops back.
"#;

#[test]
fn validate_help_lists_subcommand() {
    // The `validate` subcommand must show up in `kuro --help` so users
    // discover it the same way they discover `kuro run`.
    let out = Command::new(kuro_bin())
        .arg("--help")
        .output()
        .expect("spawn kuro --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("validate"),
        "kuro --help must list `validate`; got:\n{stdout}"
    );
}

#[test]
fn validate_clean_graph_exits_zero_with_clean_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "clean.yaml", CLEAN_GRAPH);

    let out = Command::new(kuro_bin())
        .arg("validate")
        .arg(&flow)
        .output()
        .expect("spawn kuro validate");

    assert!(
        out.status.success(),
        "clean graph must exit zero; status={:?}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ok:"),
        "stdout must report ok; got:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("warning:") && !stderr.contains("error:"),
        "clean graph must produce no warnings/errors on stderr; got:\n{stderr}"
    );
}

#[test]
fn validate_unreachable_only_exits_zero_with_warning_on_stderr() {
    // AC: `kuro validate <flow.yaml>` returns zero on unreachable-only.
    // Warning must land on stderr, not stdout, so machine readers can
    // grep stdout for `ok:` without false negatives.
    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "unreachable.yaml", UNREACHABLE_GRAPH);

    let out = Command::new(kuro_bin())
        .arg("validate")
        .arg(&flow)
        .output()
        .expect("spawn kuro validate");

    assert!(
        out.status.success(),
        "unreachable-only must still exit zero; status={:?}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("warning:") && stderr.contains("'orphan'"),
        "stderr must carry the unreachable warning naming the state; got:\n{stderr}"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ok:"),
        "stdout must still report ok on warnings-only; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("warning:") && !stdout.contains("error:"),
        "stdout must stay clean of warning/error lines; got:\n{stdout}"
    );
}

#[test]
fn validate_invalid_graph_exits_nonzero() {
    // AC: validation failures cause non-zero exit. We use a
    // schema-invalid `initial:` reference because schema-clean
    // dead-ends cannot be expressed in YAML (the schema parser rejects
    // a state with neither edges nor kind). The CLI surface we want to
    // pin is "non-zero on validation failure"; that is exercised here
    // and the dead-end path is covered by the unit tests on
    // `validate_graph_reachability` directly.
    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "bad.yaml", SCHEMA_INVALID_GRAPH);

    let out = Command::new(kuro_bin())
        .arg("validate")
        .arg(&flow)
        .output()
        .expect("spawn kuro validate");

    assert!(
        !out.status.success(),
        "invalid graph must exit non-zero; status={:?}, stdout={}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn run_graph_flow_refuses_to_start_with_clear_message() {
    // AC: `kuro run <flow>` on a graph flow must not silently fall
    // through to the linear loader. The pre-flight gate produces a
    // graph-aware message, and -- critical for the issue -- no agent
    // is spawned. We check the message here; the "no spawn" property
    // is implicit: the runner returns Err before reaching the agent
    // loader, so spawning a fake claude shim is unnecessary.
    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "clean.yaml", CLEAN_GRAPH);

    let out = Command::new(kuro_bin())
        .arg("run")
        .arg("--file")
        .arg(&flow)
        .arg("-t")
        .arg("ignored")
        .output()
        .expect("spawn kuro run");

    assert!(
        !out.status.success(),
        "graph flow must not start (no runtime yet); status={:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("graph flow") || stderr.contains("state-graph runtime"),
        "stderr must explain the graph-flow situation; got:\n{stderr}"
    );
}
