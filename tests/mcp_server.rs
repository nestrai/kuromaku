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
