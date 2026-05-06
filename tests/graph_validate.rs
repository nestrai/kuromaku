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
    description: Happy-path exit.
"#;

/// A graph YAML with a real dead-end state: `dead:` has neither
/// `edges:` nor a terminal `kind:`. The schema parser accepts this
/// shape (the dead-end semantics live in the reachability validator,
/// not the schema -- see `validate_graph_reachability`), so this YAML
/// exercises AC5 of issue #238 end-to-end: `kuro run` must refuse to
/// start with a graph-aware error naming the dead-end state.
const DEAD_END_GRAPH: &str = r#"
version: "1"
name: dead-end
initial: start
states:
  start:
    role: developer
    edges:
      go:
        to: dead
        description: Walk into the dead end.
  dead:
    role: developer
"#;

/// A graph YAML the schema rejects up front (unknown initial state).
/// Used to pin AC4: `kuro validate` exits non-zero on schema errors
/// before reachability runs.
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
    description: Happy-path exit.
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
    // AC4: validation failures cause non-zero exit. Uses a
    // schema-invalid `initial:` reference so the schema validator
    // rejects the file before reachability runs. Dead-end YAML is
    // covered separately by `validate_dead_end_graph_exits_nonzero`.
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
fn validate_dead_end_graph_exits_nonzero() {
    // AC5: a dead-end graph must fail validation end-to-end. The
    // schema accepts the YAML (a state with neither edges nor a
    // terminal kind is structurally valid); the reachability
    // validator surfaces the dead-end as a hard error and `kuro
    // validate` exits non-zero with a message naming the state.
    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "dead.yaml", DEAD_END_GRAPH);

    let out = Command::new(kuro_bin())
        .arg("validate")
        .arg(&flow)
        .output()
        .expect("spawn kuro validate");

    assert!(
        !out.status.success(),
        "dead-end graph must exit non-zero; status={:?}, stdout={}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("'dead'") && stderr.contains("dead end"),
        "stderr must name the dead-end state and classify it; got:\n{stderr}"
    );
}

#[test]
fn run_dead_end_graph_refuses_before_spawn() {
    // AC5: `kuro run` on a graph with a dead-end state must refuse
    // to start before any agent is spawned, with a graph-aware
    // message naming the offending state. Covers the end-to-end
    // path that the unit tests on `validate_graph_reachability`
    // alone cannot prove.
    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "dead.yaml", DEAD_END_GRAPH);
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(kuro_bin())
        .arg("run")
        .arg("--file")
        .arg(&flow)
        .arg("-t")
        .arg("ignored")
        .env("HOME", home.path())
        .current_dir(tmp.path())
        .output()
        .expect("spawn kuro run");

    assert!(
        !out.status.success(),
        "dead-end graph must not start; status={:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("'dead'") && stderr.contains("dead end"),
        "stderr must name the dead-end state and classify it; got:\n{stderr}"
    );
    assert!(
        stderr.contains("graph flow") && stderr.contains("refusing to start"),
        "stderr must include the graph-aware refusal banner; got:\n{stderr}"
    );
}

#[test]
fn run_graph_flow_routes_through_graph_runtime() {
    // AC: `kuro run <flow>` on a graph flow must NOT silently fall
    // through to the linear DAG loader. With the graph runtime in
    // place (issue #240), the structural guarantee is that graph
    // flows hit a dedicated execution branch. We prove this with a
    // trivial graph whose initial state is already final: the linear
    // loader would error on the missing `agents:` and `steps:` keys,
    // while the graph runtime walks states, sees `kind: final`, and
    // exits zero without spawning any agent.
    const FINAL_ONLY_GRAPH: &str = r#"
version: "1"
name: final-only
initial: done
states:
  done:
    kind: final
    description: Trivial single-state graph for the routing smoke test.
"#;

    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "final-only.yaml", FINAL_ONLY_GRAPH);
    // Isolate the run state so we don't pollute the developer's
    // ~/.koto/stacks/ with throwaway test runs.
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(kuro_bin())
        .arg("run")
        .arg("--file")
        .arg(&flow)
        .arg("-t")
        .arg("ignored")
        .env("HOME", home.path())
        // Run from the tempdir so we don't pick up the kuromaku
        // project's own .kuro/config.yaml (its seeds reference paths
        // outside the test sandbox).
        .current_dir(tmp.path())
        .output()
        .expect("spawn kuro run");

    assert!(
        out.status.success(),
        "graph runtime must accept a final-only graph; status={:?}, stdout={}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // The graph runtime announces itself with a flow-level banner
    // and a `reached final state` line. The linear DAG loader would
    // never produce these strings -- presence proves we routed
    // through the graph branch.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("graph flow") && stderr.contains("reached final state"),
        "stderr must show graph runtime markers; got:\n{stderr}"
    );
}

/// AC6 of issue #258: `kuro validate` reports missing prompt files
/// with the flow path AND the offending state ID. The error must land
/// on stderr (not stdout), and the exit code must be non-zero so CI
/// pipelines fail loud.
#[test]
fn validate_reports_missing_task_file_with_flow_path_and_state_id() {
    const GRAPH_WITH_MISSING_TASK_FILE: &str = r#"
version: "1"
name: missing-task-file
initial: design
states:
  design:
    role: developer
    task_file: prompts/design.md
    edges:
      ok:
        to: done
        description: Move on.
  done:
    kind: final
    description: Done.
"#;

    let tmp = tempfile::tempdir().unwrap();
    let flow = write_flow(tmp.path(), "missing.yaml", GRAPH_WITH_MISSING_TASK_FILE);

    let out = Command::new(kuro_bin())
        .arg("validate")
        .arg(&flow)
        .output()
        .expect("spawn kuro validate");

    assert!(
        !out.status.success(),
        "missing task_file must exit non-zero; stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("design"),
        "stderr must name the offending state ID 'design'; got:\n{stderr}"
    );
    assert!(
        stderr.contains(&flow.display().to_string()),
        "stderr must embed the flow path; got:\n{stderr}"
    );
    assert!(
        stderr.contains("prompts/design.md"),
        "stderr must name the unresolved relative path; got:\n{stderr}"
    );
}
