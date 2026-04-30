//! Integration test for the `kuro mcp` subcommand (#195).
//!
//! Spawns the binary with stdin/stdout piped, sends two JSON-RPC frames
//! (`initialize`, `tools/list`), reads two response frames, then closes
//! stdin so the server exits cleanly on EOF -- which is the only shutdown
//! path #195 promises.
//!
//! End-to-end coverage that the unit tests in `src/mcp/server.rs` do not
//! provide: real process boundary, real stdio piping, real Tokio runtime,
//! real argv parsing.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

#[test]
fn mcp_subcommand_handshake_and_lists_discovery_tools() {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kuro mcp");

    // Feed two requests; close stdin so the server hits EOF and exits.
    {
        let stdin = child.stdin.as_mut().expect("stdin pipe");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","clientInfo":{{"name":"itest","version":"0"}}}}}}"#
        )
        .unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    }
    // Drop stdin to send EOF.
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);

    let mut line1 = String::new();
    let mut line2 = String::new();
    reader.read_line(&mut line1).expect("read frame 1");
    reader.read_line(&mut line2).expect("read frame 2");

    let resp1: Value = serde_json::from_str(line1.trim()).expect("frame 1 is valid JSON");
    let resp2: Value = serde_json::from_str(line2.trim()).expect("frame 2 is valid JSON");

    // Initialize: pinned protocol version, server identifies itself, tools
    // capability advertised. The exact server version comes from
    // CARGO_PKG_VERSION so we don't pin it here.
    assert_eq!(resp1["jsonrpc"], "2.0");
    assert_eq!(resp1["id"], 1);
    assert_eq!(resp1["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(resp1["result"]["serverInfo"]["name"], "kuromaku");
    assert!(resp1["result"]["serverInfo"]["version"].is_string());
    assert_eq!(
        resp1["result"]["capabilities"]["tools"]["listChanged"],
        false
    );

    // tools/list: discovery tools registered (#197). Order is deterministic
    // (registry is a BTreeMap) -- assert names plus required-fields shape so
    // future tools landing alongside don't churn this test.
    assert_eq!(resp2["jsonrpc"], "2.0");
    assert_eq!(resp2["id"], 2);
    let tools = resp2["result"]["tools"]
        .as_array()
        .expect("tools is an array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name is string"))
        .collect();
    assert!(
        names.contains(&"list_agents"),
        "list_agents missing: {names:?}"
    );
    assert!(
        names.contains(&"list_flows"),
        "list_flows missing: {names:?}"
    );
    assert!(
        names.contains(&"load_agent"),
        "load_agent missing: {names:?}"
    );
    // Flow execution tools (#198) -- run_flow and show_output land in the
    // same registry. Asserting their presence here keeps the wire-level
    // capability advertisement honest.
    assert!(names.contains(&"run_flow"), "run_flow missing: {names:?}");
    assert!(
        names.contains(&"show_output"),
        "show_output missing: {names:?}"
    );
    // Workflow tools (#196 split). `implement_issue` (#213) lands first;
    // `review_pr` (#214) follows.
    assert!(
        names.contains(&"implement_issue"),
        "implement_issue missing: {names:?}"
    );
    let implement_issue = tools
        .iter()
        .find(|t| t["name"] == "implement_issue")
        .expect("implement_issue descriptor");
    assert_eq!(
        implement_issue["inputSchema"]["required"],
        serde_json::json!(["issue"])
    );
    assert_eq!(
        implement_issue["inputSchema"]["properties"]["issue"]["type"],
        "integer"
    );
    assert!(names.contains(&"review_pr"), "review_pr missing: {names:?}");
    let review_pr = tools
        .iter()
        .find(|t| t["name"] == "review_pr")
        .expect("review_pr descriptor");
    assert_eq!(
        review_pr["inputSchema"]["required"],
        serde_json::json!(["pr"])
    );
    assert_eq!(
        review_pr["inputSchema"]["properties"]["pr"]["type"],
        "integer"
    );
    // rework_pr (#215) -- same wire shape as review_pr (single `pr` integer).
    // Asserting the descriptor here keeps the capability advertisement honest
    // and catches schema drift before clients hit it.
    assert!(names.contains(&"rework_pr"), "rework_pr missing: {names:?}");
    let rework_pr = tools
        .iter()
        .find(|t| t["name"] == "rework_pr")
        .expect("rework_pr descriptor");
    assert_eq!(
        rework_pr["inputSchema"]["required"],
        serde_json::json!(["pr"])
    );
    assert_eq!(
        rework_pr["inputSchema"]["properties"]["pr"]["type"],
        "integer"
    );
    let run_flow = tools
        .iter()
        .find(|t| t["name"] == "run_flow")
        .expect("run_flow descriptor");
    // Per the team review on #198, args is a flat string-to-string map.
    // The schema must say so explicitly so MCP clients can validate before
    // calling.
    assert_eq!(
        run_flow["inputSchema"]["properties"]["args"]["additionalProperties"]["type"],
        "string"
    );
    assert_eq!(
        run_flow["inputSchema"]["required"],
        serde_json::json!(["name"])
    );
    let show_output = tools
        .iter()
        .find(|t| t["name"] == "show_output")
        .expect("show_output descriptor");
    assert_eq!(
        show_output["inputSchema"]["required"],
        serde_json::json!(["run_id"])
    );
    let load_agent = tools
        .iter()
        .find(|t| t["name"] == "load_agent")
        .expect("load_agent descriptor");
    // load_agent must declare `name` as required so clients can validate
    // before sending.
    assert_eq!(
        load_agent["inputSchema"]["required"],
        serde_json::json!(["name"])
    );

    // Server must exit cleanly after stdin EOF -- no SIGTERM in the
    // scaffold (per team review #195 comments).
    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success(), "kuro mcp exited non-zero: {status:?}");
}

#[test]
fn mcp_unknown_tool_returns_stable_catalog_code() {
    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kuro mcp");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"does_not_exist","arguments":{{}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read frame");

    let resp: Value = serde_json::from_str(line.trim()).expect("valid JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    assert_eq!(err["code"], -32000, "application errors use -32000");
    assert_eq!(err["message"], "unknown tool", "deterministic message");
    assert_eq!(err["data"]["code"], "unknown_tool", "stable wire code");
    assert_eq!(
        err["data"]["details"]["name"], "does_not_exist",
        "volatile substrings live in details"
    );

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// End-to-end run_flow: stand up a minimal `.kuro/` tree with a single
/// shell step in a tempdir, spawn `kuro mcp` there, and assert run_flow
/// returns `done` plus that show_output reads back the step's output.
///
/// Uses a shell `run:` step rather than an LLM agent so the test does not
/// depend on a backend being available. Stack path is set to a tempdir so
/// runs do not pollute the developer's `~/.koto/` cache.
#[test]
fn mcp_run_flow_executes_shell_step_and_show_output_reads_it() {
    let project = tempfile::tempdir().expect("project tempdir");
    // Redirect HOME so the runner's default stack path
    // (`<HOME>/.koto/stacks/<project>/`) lands inside the tempdir. This
    // keeps `run_flow` and `show_output` aligned on the same stack root
    // without either side knowing it -- which is exactly the behaviour
    // we want to verify (no path-knowledge leakage into the MCP layer).
    let fake_home = tempfile::tempdir().expect("home tempdir");
    let kuro = project.path().join(".kuro");
    std::fs::create_dir_all(kuro.join("flows")).unwrap();
    std::fs::write(kuro.join("config.yaml"), "version: \"1\"\n").unwrap();
    // `prompt:` is required by `resolve_task` even though no agent step
    // consumes it -- the runner enforces it at the flow level. Provide a
    // trivial prompt so the test exercises run_flow's success path.
    let flow_yaml = "version: \"1\"\n\
         name: smoke\n\
         prompt: smoke test\n\
         flow:\n  greet:\n    run: |\n      echo run-flow-marker\n";
    std::fs::write(kuro.join("flows/smoke.yaml"), flow_yaml).unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kuro mcp");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"run_flow","arguments":{{"name":"smoke"}}}}}}"#
        )
        .unwrap();
        // We don't yet know the run_id at this point, so request 2 is
        // sent after request 1's response is parsed. Hold stdin open
        // until then.
    }

    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let mut line1 = String::new();
    reader
        .read_line(&mut line1)
        .expect("read run_flow response");
    let resp1: Value = serde_json::from_str(line1.trim()).expect("run_flow JSON");
    assert_eq!(resp1["id"], 1, "{}", line1);
    assert!(
        resp1["error"].is_null(),
        "run_flow error: {}",
        resp1["error"]
    );
    let result1 = &resp1["result"];
    // tools/call wraps the tool's JSON in a `content` array with a
    // single `text` block of stringified JSON (MCP convention -- see
    // server::dispatch). Parse the inner JSON and assert.
    let inner_text = result1["content"][0]["text"]
        .as_str()
        .expect("content[0].text");
    let inner: Value = serde_json::from_str(inner_text).expect("inner JSON");
    assert_eq!(inner["status"], "done");
    let run_id = inner["run_id"].as_str().expect("run_id").to_string();
    assert!(run_id.starts_with("smoke-"), "run id shape: {run_id}");

    // Now ask for the step's output via show_output.
    {
        let stdin = child.stdin.as_mut().unwrap();
        let payload = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"show_output","arguments":{{"run_id":"{run_id}","step":"greet"}}}}}}"#
        );
        writeln!(stdin, "{payload}").unwrap();
    }
    drop(child.stdin.take());

    let mut line2 = String::new();
    reader.read_line(&mut line2).expect("read show_output");
    let resp2: Value = serde_json::from_str(line2.trim()).expect("show_output JSON");
    assert_eq!(resp2["id"], 2);
    assert!(
        resp2["error"].is_null(),
        "show_output error: {}",
        resp2["error"]
    );
    let inner_text = resp2["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text");
    let inner: Value = serde_json::from_str(inner_text).expect("inner JSON");
    assert_eq!(inner["status"], "done");
    assert_eq!(inner["step"], "greet");
    let steps = inner["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["step"], "greet");
    let body = steps[0]["output"].as_str().expect("output");
    assert!(
        body.contains("run-flow-marker"),
        "expected marker in output: {body}"
    );

    let status = wait_with_timeout(&mut child, Duration::from_secs(15));
    assert!(status.success(), "kuro mcp exited non-zero: {status:?}");
}

/// `run_flow` with an unknown name surfaces the `flow_missing` catalog
/// entry. Acceptance criterion (#198): error handling for unknown flow.
#[test]
fn mcp_run_flow_unknown_flow_returns_flow_missing() {
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::create_dir_all(project.path().join(".kuro/flows")).unwrap();
    std::fs::write(project.path().join(".kuro/config.yaml"), "version: \"1\"\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"run_flow","arguments":{{"name":"nonexistent"}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    assert_eq!(err["code"], -32000);
    assert_eq!(err["data"]["code"], "flow_missing");
    assert_eq!(err["data"]["details"]["name"], "nonexistent");

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `show_output` with an unknown run id surfaces `run_not_found`.
/// Acceptance criterion (#198): error handling for run not found.
#[test]
fn mcp_show_output_unknown_run_returns_run_not_found() {
    let project = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"show_output","arguments":{{"run_id":"ghost-20260501-000000"}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let resp: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["data"]["code"], "run_not_found");
    assert_eq!(
        resp["error"]["data"]["details"]["run_id"],
        "ghost-20260501-000000"
    );

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `implement_issue` rejects non-integer args at the wire boundary.
/// Acceptance criterion (#213): "Returns MCP error for invalid issue
/// number". This catches client mistakes before the runner spins up,
/// keeping the failure fast and the error code stable (`invalid_params`).
#[test]
fn mcp_implement_issue_rejects_non_integer_argument() {
    let project = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"implement_issue","arguments":{{"issue":"not-a-number"}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    // invalid_params is JSON-RPC -32602 -- the transport-level code, not
    // the application catalog. The transport classifier is what we want
    // here; the issue body's "invalid issue number" criterion is satisfied
    // either way (reject before flow setup).
    assert_eq!(err["code"], -32602);

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `implement_issue` against a project without the `implement-issue` flow
/// surfaces `flow_missing` -- the same catalog entry `run_flow` uses for
/// the equivalent setup error. Exercises the end-to-end response shape and
/// the runner-error classifier wired through `classify_setup_error`.
#[test]
fn mcp_implement_issue_unknown_flow_returns_flow_missing() {
    let project = tempfile::tempdir().expect("project tempdir");
    // Empty `.kuro/` -- no `flows/implement-issue.yaml` anywhere in the
    // seed cascade. The runner must surface flow_missing rather than fall
    // through to a generic internal_error.
    std::fs::create_dir_all(project.path().join(".kuro/flows")).unwrap();
    std::fs::write(project.path().join(".kuro/config.yaml"), "version: \"1\"\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"implement_issue","arguments":{{"issue":213}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    assert_eq!(err["code"], -32000);
    assert_eq!(err["data"]["code"], "flow_missing");
    assert_eq!(err["data"]["details"]["name"], "implement-issue");

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `implement_issue` against a flow that exists but lacks the step ids the
/// tool keys off (`review`, `pr`) must surface a structured internal error
/// at config-time -- *not* run end-to-end and silently produce
/// `verdict: "unclear"` + `pr_url: null`. This is the fix for the silent
/// step-name mismatch flagged in #213's team review.
#[test]
fn mcp_implement_issue_missing_required_step_ids_errors_at_config_time() {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro = project.path().join(".kuro");
    std::fs::create_dir_all(kuro.join("flows")).unwrap();
    std::fs::write(kuro.join("config.yaml"), "version: \"1\"\n").unwrap();
    // Flow exists under the right name but the step ids do not match what
    // implement_issue keys off. The tool must reject this before spawning
    // the runner; otherwise the silent-failure mode returns.
    let flow_yaml = "version: \"1\"\n\
         name: implement-issue\n\
         prompt: noop\n\
         flow:\n  greet:\n    run: |\n      echo nope\n";
    std::fs::write(kuro.join("flows/implement-issue.yaml"), flow_yaml).unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"implement_issue","arguments":{{"issue":213}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    // JSON-RPC `InternalError` (transport-level): the numeric `-32603` code
    // identifies it; the volatile reason string lives in `data.details`.
    // No `data.code` field for transport-level errors.
    assert_eq!(err["code"], -32603);
    let reason = err["data"]["details"]["reason"]
        .as_str()
        .expect("reason string");
    assert!(
        reason.contains("review") && reason.contains("pr"),
        "reason should name both missing step ids, got: {reason}"
    );

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `review_pr` rejects non-integer args at the wire boundary.
/// Acceptance criterion (#214): "Returns MCP error for invalid PR number".
#[test]
fn mcp_review_pr_rejects_non_integer_argument() {
    let project = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"review_pr","arguments":{{"pr":"not-a-number"}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    // invalid_params is JSON-RPC -32602 -- the transport-level code, same
    // as implement_issue's matching test. The "invalid PR number" criterion
    // is satisfied either way (rejected before flow setup).
    assert_eq!(err["code"], -32602);

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `review_pr` against a project without the `review-pr` flow surfaces
/// `flow_missing` -- same catalog entry the rest of the workflow tools use.
/// Acceptance criterion (#214): "Returns MCP error for ... flow failure".
#[test]
fn mcp_review_pr_unknown_flow_returns_flow_missing() {
    let project = tempfile::tempdir().expect("project tempdir");
    // Empty `.kuro/` -- no `flows/review-pr.yaml` anywhere in the seed
    // cascade. The runner must surface flow_missing rather than internal_error.
    std::fs::create_dir_all(project.path().join(".kuro/flows")).unwrap();
    std::fs::write(project.path().join(".kuro/config.yaml"), "version: \"1\"\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"review_pr","arguments":{{"pr":42}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    assert_eq!(err["code"], -32000);
    assert_eq!(err["data"]["code"], "flow_missing");
    assert_eq!(err["data"]["details"]["name"], "review-pr");

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `review_pr` against a flow that exists but lacks the step ids the tool
/// keys off (`consensus`) must surface a structured internal error at
/// config-time -- *not* run end-to-end and silently produce
/// `verdict: "unclear"` + empty findings. Mirrors the silent-failure guard
/// from #213's team review.
#[test]
fn mcp_review_pr_missing_required_step_ids_errors_at_config_time() {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro = project.path().join(".kuro");
    std::fs::create_dir_all(kuro.join("flows")).unwrap();
    std::fs::write(kuro.join("config.yaml"), "version: \"1\"\n").unwrap();
    // Flow named `review-pr` but its only step is `greet` -- the `consensus`
    // step the tool keys off is absent. The tool must reject this before
    // spawning the runner.
    let flow_yaml = "version: \"1\"\n\
         name: review-pr\n\
         prompt: noop\n\
         flow:\n  greet:\n    run: |\n      echo nope\n";
    std::fs::write(kuro.join("flows/review-pr.yaml"), flow_yaml).unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"review_pr","arguments":{{"pr":42}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    // JSON-RPC InternalError (transport-level): `-32603` numeric code; the
    // volatile reason string lives in `data.details.reason`.
    assert_eq!(err["code"], -32603);
    let reason = err["data"]["details"]["reason"]
        .as_str()
        .expect("reason string");
    assert!(
        reason.contains("consensus"),
        "reason should name the missing step id, got: {reason}"
    );

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `rework_pr` rejects non-integer args at the wire boundary.
/// Acceptance criterion (#215): "Returns MCP error for invalid PR number".
#[test]
fn mcp_rework_pr_rejects_non_integer_argument() {
    let project = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"rework_pr","arguments":{{"pr":"not-a-number"}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    // invalid_params is JSON-RPC -32602 -- transport-level code, same as the
    // sibling tools' matching tests.
    assert_eq!(err["code"], -32602);

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `rework_pr` against a project without the `rework-pr` flow surfaces
/// `flow_missing` -- same catalog entry the rest of the workflow tools use.
/// Acceptance criterion (#215): "Returns MCP error for ... flow failure".
#[test]
fn mcp_rework_pr_unknown_flow_returns_flow_missing() {
    let project = tempfile::tempdir().expect("project tempdir");
    // Empty `.kuro/` -- no `flows/rework-pr.yaml` anywhere in the seed
    // cascade. The runner must surface flow_missing rather than internal_error.
    std::fs::create_dir_all(project.path().join(".kuro/flows")).unwrap();
    std::fs::write(project.path().join(".kuro/config.yaml"), "version: \"1\"\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"rework_pr","arguments":{{"pr":42}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    assert_eq!(err["code"], -32000);
    assert_eq!(err["data"]["code"], "flow_missing");
    assert_eq!(err["data"]["details"]["name"], "rework-pr");

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// `rework_pr` against a flow that exists but lacks the step ids the tool
/// keys off (`fix`, `verify`) must surface a structured internal error at
/// config-time -- *not* run end-to-end and silently produce empty fixup
/// commits and zero counts. Mirrors the silent-failure guard the sibling
/// tools enforce.
#[test]
fn mcp_rework_pr_missing_required_step_ids_errors_at_config_time() {
    let project = tempfile::tempdir().expect("project tempdir");
    let kuro = project.path().join(".kuro");
    std::fs::create_dir_all(kuro.join("flows")).unwrap();
    std::fs::write(kuro.join("config.yaml"), "version: \"1\"\n").unwrap();
    // Flow named `rework-pr` but its only step is `greet` -- the `fix` and
    // `verify` steps the tool keys off are absent. The tool must reject
    // this before spawning the runner.
    let flow_yaml = "version: \"1\"\n\
         name: rework-pr\n\
         prompt: noop\n\
         flow:\n  greet:\n    run: |\n      echo nope\n";
    std::fs::write(kuro.join("flows/rework-pr.yaml"), flow_yaml).unwrap();

    let bin = env!("CARGO_BIN_EXE_kuro");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"rework_pr","arguments":{{"pr":42}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(resp["id"], 1);
    let err = &resp["error"];
    // JSON-RPC InternalError (transport-level): `-32603` numeric code; the
    // volatile reason string lives in `data.details.reason`.
    assert_eq!(err["code"], -32603);
    let reason = err["data"]["details"]["reason"]
        .as_str()
        .expect("reason string");
    assert!(
        reason.contains("fix") && reason.contains("verify"),
        "reason should name both missing step ids, got: {reason}"
    );

    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success());
}

/// Poll `try_wait` until the child exits or the timeout elapses. Avoids
/// hanging the test suite if the server fails to honor stdin EOF -- which
/// would itself be a regression worth surfacing.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("kuro mcp did not exit within {timeout:?} after stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
