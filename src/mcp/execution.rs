//! Flow-execution tools (#198): `run_flow`, `show_output`.
//!
//! These two tools wire the MCP layer to the runner library API and the
//! stack reader. They never touch run-directory paths directly -- the team
//! review (issue #198 update) forbids path parsing in `src/mcp/`. Run
//! lookups go through [`crate::stack::read_run`] and flow execution
//! through [`crate::runner::execute_flow`].
//!
//! ## Design notes
//!
//! - **Synchronous run_flow**: `run_flow` awaits the spawned execution task
//!   to completion and returns the final run id and status. The team review
//!   pinned this over the older "return-immediately + poll" sketch in the
//!   issue body. Progress notifications are follow-up work -- the server
//!   does not yet have an outbound notification channel.
//!
//! - **Flat args map**: per the team review, the `run_flow` schema accepts
//!   `args` as a flat `Map<String, String>`. Nested objects, arrays and
//!   non-string scalars are rejected at parse time so the contract stays
//!   predictable across MCP clients.
//!
//! - **Status taxonomy**: `show_output` mirrors [`crate::stack::RunStatus`]
//!   on the wire (`running | done | failed | not_found`). `failed` is
//!   reserved for a future explicit failure marker -- today the absence of
//!   `manifest.yaml` reads as `running`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::runner::{self, ExecuteFlowSpec, FlowSource};
use crate::stack::{self, RunOutputs, RunStatus};

use super::error::{McpError, McpErrorCode};
use super::session::SessionState;
use super::tools::Tool;

fn invalid_params(reason: impl Into<String>) -> McpError {
    McpError::with_details(
        McpErrorCode::InvalidParams,
        json!({"reason": reason.into()}),
    )
}

fn internal(reason: impl Into<String>) -> McpError {
    McpError::with_details(
        McpErrorCode::InternalError,
        json!({"reason": reason.into()}),
    )
}

// --- run_flow ---

#[derive(Deserialize)]
struct RunFlowArgs {
    name: String,
    #[serde(default)]
    args: Option<Value>,
}

/// `run_flow` -- trigger a flow execution by name.
///
/// Equivalent to `kuro run <name> key=value ...`. The tool blocks until the
/// flow completes and returns `{ run_id, status }`. On a setup-side error
/// (unknown flow, invalid YAML, missing template var) the call returns an
/// `McpError`; the run id is not produced for an unrunnable flow.
///
/// Holds an [`Arc<SessionState>`] so the flow's [`runner::ActiveRouter`]
/// is registered on the per-MCP-session registry while the run is in
/// flight. The `send_message` tool reads that registry to find the
/// conversation step it should inject into. Registration is scoped to the
/// `await_completion` span: nothing leaks if the call returns early on a
/// setup error, and the entry is removed unconditionally when the future
/// resolves (success, failure, panic via the runner's join error path).
pub struct RunFlow {
    session: Arc<SessionState>,
}

impl RunFlow {
    pub fn new(session: Arc<SessionState>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl Tool for RunFlow {
    fn name(&self) -> &'static str {
        "run_flow"
    }
    fn description(&self) -> &'static str {
        "Run a flow by name and wait for it to finish. Returns the run id and final status \
         (\"done\"). Args is a flat map of string-to-string used as `key=value` overrides for \
         template vars and roles, mirroring `kuro run`. Use show_output afterwards to read step \
         outputs. Example: run_flow {\"name\":\"dev\",\"args\":{\"task\":\"refactor\"}}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Flow name as registered under .kuro/flows/<name>.yaml in the seed cascade."
                },
                "args": {
                    "type": "object",
                    "description": "Flat map of template variables and role overrides. Values must be strings.",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["name"]
        })
    }
    async fn call(&self, arguments: Value) -> Result<Value, McpError> {
        let parsed: RunFlowArgs = serde_json::from_value(arguments)
            .map_err(|e| invalid_params(format!("arguments: {e}")))?;
        let name = parsed.name.trim().to_string();
        if name.is_empty() {
            return Err(invalid_params("name must not be empty"));
        }
        let bare_args = match parsed.args {
            Some(v) => parse_string_map(v)?,
            None => HashMap::new(),
        };
        do_run_flow(name, bare_args, Arc::clone(&self.session)).await
    }
}

/// Convert a JSON object into a flat `Map<String, String>`. Rejects any
/// non-object input and any non-string value -- the team review pinned the
/// schema to flat string maps so clients cannot accidentally pass a nested
/// object that the runner would silently flatten.
fn parse_string_map(v: Value) -> Result<HashMap<String, String>, McpError> {
    let Value::Object(obj) = v else {
        return Err(invalid_params("args must be an object"));
    };
    let mut out: HashMap<String, String> = HashMap::with_capacity(obj.len());
    for (k, val) in obj {
        let s = match val {
            Value::String(s) => s,
            _ => {
                return Err(invalid_params(format!(
                    "args.{k}: expected string, got {}",
                    type_name(&val)
                )));
            }
        };
        out.insert(k, s);
    }
    Ok(out)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

async fn do_run_flow(
    name: String,
    bare_args: HashMap<String, String>,
    session: Arc<SessionState>,
) -> Result<Value, McpError> {
    let spec = ExecuteFlowSpec {
        flow: FlowSource::Name(name.clone()),
        bare_args,
        // Quiet mode -- the CLI banner is meaningless on stdout-protocol
        // transports. Stdout must stay reserved for the JSON-RPC framing.
        suppress_command_banner: true,
        ..ExecuteFlowSpec::default()
    };
    let handle = runner::execute_flow(spec)
        .await
        .map_err(|e| classify_setup_error(&name, e))?;
    let run_id = handle.run_id.clone();

    // Pull the cloneable router view BEFORE await_completion consumes the
    // handle. The slot is registered for the lifetime of the await; the
    // RAII guard below removes it on every exit path (success, error,
    // panic) so the registry never accumulates stale entries.
    let active_router = handle.active_router();
    let _slot_guard = SlotGuard::register(&session, active_router);

    match handle.await_completion().await {
        // `flow` rides along so the caller can pass it back to `show_output`
        // and reach a custom `stack.path` (the default-stack assumption was
        // the regression the team review flagged).
        Ok(_) => Ok(json!({
            "run_id": run_id,
            "status": RunStatus::Done.as_str(),
            "flow": name,
        })),
        // The execution task itself returned an error -- the flow ran
        // partially. Surface the run id so the caller can `show_output` and
        // see what landed before the failure. `internal_error` is the
        // closest fit in the catalog; a dedicated `flow_failed` code is
        // future work alongside the failure marker on disk.
        Err(e) => Err(McpError::with_details(
            McpErrorCode::InternalError,
            json!({
                "reason": format!("flow execution failed: {e}"),
                "run_id": run_id,
                "flow": name,
            }),
        )),
    }
}

/// RAII helper around [`SessionState::register`] / [`SessionState::deregister`].
/// Drops the registration when the guard goes out of scope -- in particular
/// across the `await_completion` await, where neither a manual `deregister`
/// before the `?` nor a panic in the spawned task could leave the registry
/// stale. The session is held by `Arc`, so dropping the guard is safe even
/// if the server is shutting down concurrently.
struct SlotGuard {
    session: Arc<SessionState>,
    slot: super::session::RunSlot,
}

impl SlotGuard {
    fn register(session: &Arc<SessionState>, ar: runner::ActiveRouter) -> Self {
        let slot = session.register(ar);
        Self {
            session: Arc::clone(session),
            slot,
        }
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.session.deregister(self.slot);
    }
}

/// Map setup-time eyre errors from `execute_flow` onto the stable catalog.
/// The runner uses `eyre!` strings rather than typed errors, so we pattern-
/// match on substrings. The matched markers (`config file '...' not found`,
/// `flows/<name>.yaml`) are stable across the runner's history -- a rename
/// would surface in the unit tests below.
///
/// Exposed `pub(super)` so sibling MCP tool modules (workflow.rs for
/// `implement_issue` etc.) reuse the same classifier instead of growing
/// their own drift-prone copies.
pub(super) fn classify_setup_error(name: &str, err: color_eyre::Report) -> McpError {
    let msg = format!("{err:#}");
    let lower = msg.to_lowercase();
    if lower.contains("not found")
        && (lower.contains(&format!("flows/{name}.yaml")) || lower.contains("flow"))
    {
        return McpError::with_details(
            McpErrorCode::FlowMissing,
            json!({"name": name, "reason": msg}),
        );
    }
    if lower.starts_with("missing vars:") || lower.contains("missing vars:") {
        return McpError::with_details(McpErrorCode::InvalidParams, json!({"reason": msg}));
    }
    internal(msg)
}

// --- show_output ---

#[derive(Deserialize)]
struct ShowOutputArgs {
    run_id: String,
    #[serde(default)]
    step: Option<String>,
    /// Optional flow name. When provided, the stack path is resolved
    /// through that flow's `stack.path` -- which is what `run_flow` writes
    /// to. Without it, the default `~/.kuro/stacks/<project>/` is used,
    /// which only matches runs from flows that did not override `stack.path`.
    #[serde(default)]
    flow: Option<String>,
}

/// `show_output` -- read step outputs from a previous (or in-flight) run.
pub struct ShowOutput;

#[async_trait]
impl Tool for ShowOutput {
    fn name(&self) -> &'static str {
        "show_output"
    }
    fn description(&self) -> &'static str {
        "Read step outputs for a run by id. With `step`, returns only that step's content; \
         without it, returns every recorded step in execution order. Pass `flow` (the flow name \
         that produced the run, as returned by run_flow) so the stack path matches a flow's \
         `stack.path` override; without `flow` the default project stack is used. The response \
         always carries a status: \"running\" while the flow is still executing, \"done\" once \
         it finished, \"not_found\" if no run with that id exists. Example: \
         show_output {\"run_id\":\"dev-20260501-100000\",\"flow\":\"dev\",\"step\":\"build\"}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "Run id returned by run_flow (e.g. 'dev-20260501-100000')."
                },
                "step": {
                    "type": "string",
                    "description": "Optional step id. Omit to return every recorded step."
                },
                "flow": {
                    "type": "string",
                    "description": "Optional flow name (same value as passed to run_flow). When set, the stack path is resolved via that flow's `stack.path`, so runs written under a custom stack path are visible. Omit only when no flow in the project overrides `stack.path`."
                }
            },
            "required": ["run_id"]
        })
    }
    async fn call(&self, arguments: Value) -> Result<Value, McpError> {
        let parsed: ShowOutputArgs = serde_json::from_value(arguments)
            .map_err(|e| invalid_params(format!("arguments: {e}")))?;
        let run_id = parsed.run_id.trim().to_string();
        if run_id.is_empty() {
            return Err(invalid_params("run_id must not be empty"));
        }
        let step = parsed
            .step
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let flow = parsed
            .flow
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let stack_path = match flow.as_deref() {
            Some(name) => runner::resolve_stack_path_for_flow_name(name)
                .map_err(|e| classify_setup_error(name, e))?,
            None => runner::resolve_stack_path(""),
        };
        do_show_output(&stack_path, &run_id, step.as_deref())
    }
}

fn do_show_output(stack_path: &Path, run_id: &str, step: Option<&str>) -> Result<Value, McpError> {
    let run_path = stack::existing_run_path(&stack_path.join(run_id));
    let outputs = stack::read_run(
        run_path.parent().expect("run path always has a parent"),
        run_id,
        step,
    )
    .map_err(|e| internal(format!("read run: {e}")))?;
    if outputs.status == RunStatus::NotFound {
        return Err(McpError::with_details(
            McpErrorCode::RunNotFound,
            json!({"run_id": run_id}),
        ));
    }
    Ok(render_outputs(&outputs, step))
}

/// Build the JSON response from a `RunOutputs` snapshot. In-flight steps
/// (no content yet) report `null` for `output` so the caller can tell
/// "running, no body" from "done, body present".
fn render_outputs(outputs: &RunOutputs, step_filter: Option<&str>) -> Value {
    let mut steps: Vec<Value> = Vec::with_capacity(outputs.steps.len());
    for s in &outputs.steps {
        // The team review wants "in-progress step reported with status running,
        // no output". Per-step status defaults to the run's status; once the
        // content file exists the step is effectively `done` regardless of
        // whether the manifest has landed for the whole run.
        let step_status = match (outputs.status, s.content.as_ref()) {
            (RunStatus::Running, None) => RunStatus::Running,
            (RunStatus::Running, Some(_)) => RunStatus::Done,
            (status, _) => status,
        };
        steps.push(json!({
            "step": s.step_id,
            "status": step_status.as_str(),
            "output": s.content,
        }));
    }
    let mut obj = Map::new();
    obj.insert("run_id".to_string(), Value::String(outputs.run_id.clone()));
    obj.insert(
        "status".to_string(),
        Value::String(outputs.status.as_str().to_string()),
    );
    if let Some(name) = step_filter {
        obj.insert("step".to_string(), Value::String(name.to_string()));
    }
    obj.insert("steps".to_string(), Value::Array(steps));
    Value::Object(obj)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{
        ParticipantStat, STEPS_SUBDIR, StepRecord, ensure_dir, step_meta_filename, write_run_step,
    };
    use std::fs;
    use tempfile::TempDir;

    // ---- parse_string_map ----

    #[test]
    fn parse_string_map_accepts_flat_object() {
        let v = json!({"task": "refactor", "owner": "nestrai"});
        let m = parse_string_map(v).unwrap();
        assert_eq!(m.get("task").map(String::as_str), Some("refactor"));
        assert_eq!(m.get("owner").map(String::as_str), Some("nestrai"));
    }

    #[test]
    fn parse_string_map_rejects_non_object() {
        let err = parse_string_map(json!("hi")).unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn parse_string_map_rejects_non_string_values() {
        // Numbers, bools, nested objects, arrays all rejected -- the schema
        // is flat string-to-string. This guards against accidental coercion
        // into a nested `args` shape that the runner would not understand.
        for bad in [
            json!({"k": 1}),
            json!({"k": true}),
            json!({"k": {}}),
            json!({"k": []}),
        ] {
            let err = parse_string_map(bad).unwrap_err();
            assert_eq!(err.code, McpErrorCode::InvalidParams);
        }
    }

    // ---- classify_setup_error ----

    #[test]
    fn classify_setup_error_maps_flow_not_found() {
        let err = color_eyre::eyre::eyre!(
            "flow 'unknown' not found anywhere in the seed cascade\n\nhint: create flows/unknown.yaml in one of the seeds, or use --file <path>"
        );
        let mapped = classify_setup_error("unknown", err);
        assert_eq!(mapped.code, McpErrorCode::FlowMissing);
        let details = mapped.details.unwrap();
        assert_eq!(details["name"], "unknown");
    }

    #[test]
    fn classify_setup_error_maps_missing_vars() {
        let err = color_eyre::eyre::eyre!("missing vars: owner\n\nhint: define them ...");
        let mapped = classify_setup_error("dev", err);
        assert_eq!(mapped.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn classify_setup_error_falls_back_to_internal() {
        let err = color_eyre::eyre::eyre!("some other failure");
        let mapped = classify_setup_error("dev", err);
        assert_eq!(mapped.code, McpErrorCode::InternalError);
    }

    // ---- show_output / render_outputs ----

    fn record_for(step_num: usize, step_id: &str, ext: &str) -> StepRecord {
        StepRecord {
            step_id: step_id.to_string(),
            kind: "llm".to_string(),
            agent: Some("Levi".to_string()),
            model_requested: Some("claude-sonnet-4-5".to_string()),
            model_actual: Some("claude-sonnet-4-5".to_string()),
            backend: "claude-cli".to_string(),
            tokens_in: Some(100),
            tokens_out: Some(80),
            duration_ms: 500,
            started_at: "2026-05-01T10:00:00Z".to_string(),
            exit_code: 0,
            input_steps: vec![],
            output_file: format!("{step_num:02}-{step_id}.{ext}"),
            participants: Vec::<ParticipantStat>::new(),
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: None,
        }
    }

    fn write_minimal_manifest(run_path: &Path) {
        std::fs::write(run_path.join("manifest.yaml"), "version: 1\n").unwrap();
    }

    #[test]
    fn show_output_returns_run_not_found_for_missing_run() {
        let tmp = TempDir::new().unwrap();
        let err = do_show_output(tmp.path(), "ghost-id", None).unwrap_err();
        assert_eq!(err.code, McpErrorCode::RunNotFound);
        let details = err.details.unwrap();
        assert_eq!(details["run_id"], "ghost-id");
    }

    #[test]
    fn show_output_returns_done_with_all_steps_in_order() {
        let tmp = TempDir::new().unwrap();
        let stack_path = tmp.path();
        let run_id = "dev-20260501-100000";
        let run_path = stack_path.join(run_id);
        write_run_step(&run_path, 1, &record_for(1, "design", "md"), "DESIGN").unwrap();
        write_run_step(&run_path, 2, &record_for(2, "build", "md"), "BUILD").unwrap();
        write_minimal_manifest(&run_path);

        let v = do_show_output(stack_path, run_id, None).unwrap();
        assert_eq!(v["run_id"], run_id);
        assert_eq!(v["status"], "done");
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["step"], "design");
        assert_eq!(steps[0]["output"], "DESIGN");
        assert_eq!(steps[0]["status"], "done");
        assert_eq!(steps[1]["step"], "build");
        assert_eq!(steps[1]["output"], "BUILD");
    }

    #[test]
    fn show_output_filtered_returns_only_requested_step() {
        let tmp = TempDir::new().unwrap();
        let stack_path = tmp.path();
        let run_id = "dev-20260501-100100";
        let run_path = stack_path.join(run_id);
        write_run_step(&run_path, 1, &record_for(1, "design", "md"), "DESIGN").unwrap();
        write_run_step(&run_path, 2, &record_for(2, "build", "md"), "BUILD").unwrap();
        write_minimal_manifest(&run_path);

        let v = do_show_output(stack_path, run_id, Some("build")).unwrap();
        assert_eq!(v["status"], "done");
        assert_eq!(v["step"], "build");
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["step"], "build");
        assert_eq!(steps[0]["output"], "BUILD");
    }

    #[test]
    fn show_output_running_step_reports_no_output() {
        // Acceptance: in-progress step reported as `running` with no output.
        // Manifest absent + meta written but content file missing.
        let tmp = TempDir::new().unwrap();
        let stack_path = tmp.path();
        let run_id = "dev-20260501-100200";
        let run_path = stack_path.join(run_id);
        ensure_dir(&run_path.join(STEPS_SUBDIR)).unwrap();
        let rec = record_for(1, "design", "md");
        fs::write(
            run_path
                .join(STEPS_SUBDIR)
                .join(step_meta_filename(1, &rec.step_id)),
            serde_yaml::to_string(&rec).unwrap(),
        )
        .unwrap();

        let v = do_show_output(stack_path, run_id, None).unwrap();
        assert_eq!(v["status"], "running");
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["step"], "design");
        assert_eq!(steps[0]["status"], "running");
        assert!(steps[0]["output"].is_null());
    }

    #[test]
    fn show_output_step_filter_unknown_returns_done_with_empty_steps() {
        // The run is Done; the named step just is not part of the recorded
        // set. Distinguishing this from `run_not_found` is the whole point
        // of returning `Ok(...)` with an empty `steps` array.
        let tmp = TempDir::new().unwrap();
        let stack_path = tmp.path();
        let run_id = "dev-20260501-100300";
        let run_path = stack_path.join(run_id);
        write_run_step(&run_path, 1, &record_for(1, "design", "md"), "DESIGN").unwrap();
        write_minimal_manifest(&run_path);

        let v = do_show_output(stack_path, run_id, Some("missing")).unwrap();
        assert_eq!(v["status"], "done");
        assert_eq!(v["step"], "missing");
        assert_eq!(v["steps"].as_array().unwrap().len(), 0);
    }

    // ---- Tool::call validation ----

    #[tokio::test]
    async fn show_output_tool_rejects_empty_run_id() {
        let tool = ShowOutput;
        let err = tool.call(json!({"run_id": "  "})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn show_output_tool_rejects_missing_run_id() {
        let tool = ShowOutput;
        let err = tool.call(json!({})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    fn fresh_session() -> Arc<SessionState> {
        Arc::new(SessionState::new())
    }

    #[tokio::test]
    async fn run_flow_tool_rejects_empty_name() {
        let tool = RunFlow::new(fresh_session());
        let err = tool.call(json!({"name": "   "})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn run_flow_tool_rejects_missing_name() {
        let tool = RunFlow::new(fresh_session());
        let err = tool.call(json!({})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn run_flow_tool_rejects_non_string_arg_value() {
        let tool = RunFlow::new(fresh_session());
        let err = tool
            .call(json!({"name": "dev", "args": {"task": 42}}))
            .await
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn show_output_tool_accepts_optional_flow_argument() {
        // Schema-level guarantee: when `flow` is set the tool routes
        // resolution through `resolve_stack_path_for_flow_name`. This run
        // happens in a temp CWD so the seed cascade has nothing to find,
        // and the call surfaces a `flow_missing` error instead of silently
        // falling back to the default stack path. Catches the regression
        // the team review flagged on PR #218.
        let tmp = TempDir::new().unwrap();
        let saved = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let tool = ShowOutput;
        let result = tool
            .call(json!({"run_id": "anything", "flow": "no-such-flow"}))
            .await;
        std::env::set_current_dir(saved).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::FlowMissing);
    }

    #[tokio::test]
    async fn show_output_tool_treats_blank_flow_as_absent() {
        // Whitespace-only `flow` must not trigger a flow lookup -- otherwise
        // a careless client argument would crash the call. We expect the
        // run-not-found path because we never created any run.
        let tmp = TempDir::new().unwrap();
        let saved = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let tool = ShowOutput;
        let result = tool
            .call(json!({"run_id": "ghost-id", "flow": "   "}))
            .await;
        std::env::set_current_dir(saved).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::RunNotFound);
    }

    #[tokio::test]
    async fn execution_tools_have_valid_descriptors() {
        let mut reg = super::super::tools::ToolRegistry::new();
        reg.register(Box::new(RunFlow::new(fresh_session())))
            .unwrap();
        reg.register(Box::new(ShowOutput)).unwrap();
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["run_flow", "show_output"]);
    }
}
