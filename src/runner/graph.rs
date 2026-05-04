//! State-machine driver for graph flows (issue #240).
//!
//! Walks a [`GraphFlow`] starting from `initial:`, asks the agent assigned
//! to the current state to pick exactly one outgoing edge, and jumps to the
//! target state. Terminates when a state with `kind: final` is reached, when
//! the malformed/unknown-edge retry budget runs out, or when the global
//! step counter exceeds [`DEFAULT_MAX_STEPS`].
//!
//! Per-state output is persisted into the run's `steps/` directory using the
//! same layout the linear runner writes (`NN-<state-id>.md` content +
//! `NN-<state-id>.meta.yaml` metadata) so `kuro show-output` and the MCP
//! `show_output` tool keep working without a graph-specific reader.
//!
//! Out of scope here:
//! - guards on edges
//! - artifact tracking (produces/consumes)
//! - resume / fork from a previous run
//! - the runtime knobs `max_visits_per_node` / `on_limit_exceeded`
//! - MCP-tool integration during graph runs
//!
//! See the issue for the full IN/OUT scope.
//!
//! Re-prompting strategy: a malformed JSON reply or an unknown edge gets
//! exactly one retry against the same state with an explicit error appended
//! to the prompt. A second failure aborts the run with [`RunError::GraphRuntime`].

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use indexmap::IndexMap;

use crate::config::{Agent, Backend, GraphEdge, GraphFlow, StateKind};
use crate::executor::{self, ExecutionTask, ExecutorBoxed, OutputFormat};
use crate::stack::{self, GraphDecision, StepRecord};
use crate::ui::{self, StepInfo, StepState};

use super::decision::{
    DecisionError, DecisionParseError, parse_agent_decision, validate_transition,
};
use super::{
    RunContext, RunError, StepRunResult, backend_name, build_system_prompt, format_duration,
    llm_output_filename,
};

/// Hard cap on the number of state transitions before the driver aborts.
///
/// The issue's prototype scope explicitly fixes this rather than reading
/// `runtime.max_visits_per_node` from the YAML; making it `pub` so the
/// integration test can reference the same constant the driver enforces.
pub const DEFAULT_MAX_STEPS: usize = 30;

/// Hard cap on how often a single state may be entered before the driver
/// aborts with a "stuck in a loop" error.
///
/// The global `DEFAULT_MAX_STEPS` only catches runaway flows after dozens of
/// transitions -- that is a coarse backstop. Ping-pong between two states
/// (e.g. `design <-> steer_design`) reaches the global cap only after each
/// state has been visited ~15 times, which wastes minutes of agent time on
/// the wrong problem. A per-state cap catches the loop after a few rounds:
/// if the same state is entered five times the agents are not converging
/// and the run should fail loud rather than burn through the global budget.
///
/// The number is intentionally small. Production flows that legitimately
/// need more rounds should mark themselves so explicitly via a future
/// `runtime.max_visits_per_node:` YAML knob (out of scope for this fix).
pub const DEFAULT_MAX_VISITS_PER_STATE: usize = 5;

/// Build the deterministic edge-menu suffix appended to every state's task
/// prompt.
///
/// The earlier wording ("Reply only with the JSON object") forced agents to
/// collapse the entire artifact into the JSON `reason` field, which then
/// made downstream states (steer / review / implement) operate on routing
/// metadata instead of a real design / review / patch artifact. The current
/// wording asks the agent to produce the artifact as the main reply and
/// append the routing JSON as the last line; [`parse_agent_decision`]
/// already accepts JSON embedded in surrounding prose so the parser does
/// not need to change. The downstream context-handoff (see [`run_graph_flow`])
/// reads the saved file verbatim, so prose + JSON survives intact across
/// the next state's prompt.
pub fn build_menu(state_id: &str, edges: &IndexMap<String, GraphEdge>) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "You are at state `{state_id}`. Complete the task above as your main reply -- produce the full artifact (design plan, review, code summary, etc.) so the next agent in the graph can build on your work. After the artifact, end your reply with a single JSON object choosing one transition:\n\n",
    ));
    s.push_str(
        "{\"transition\": \"<edge-name>\", \"reason\": \"<one to two sentences explaining why>\"}\n\n",
    );
    s.push_str("Available transitions:\n");
    for (name, edge) in edges {
        s.push_str(&format!("- `{name}`: {}\n", edge.description));
    }
    s.push('\n');
    s.push_str("The JSON object must appear exactly once, on its own line at the end of your reply. Pick one of the transitions listed -- do not invent new ones.");
    s
}

/// Append-only retry guidance: malformed JSON case.
fn malformed_retry_note(parse_err: &DecisionParseError) -> String {
    format!(
        "\n\nYour previous reply was malformed and could not be parsed: {parse_err}.\nReply again with a JSON object exactly as specified above. No prose, no fenced code blocks -- just the JSON.",
    )
}

/// Append-only retry guidance: unknown-edge case.
fn unknown_edge_retry_note(picked: &str, allowed: &[String]) -> String {
    let allowed_list = allowed
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\n\nYour previous reply chose unknown transition `{picked}`. Valid choices: {allowed_list}. Reply again with a JSON object choosing one of the listed transitions."
    )
}

/// Outcome of a successful graph-flow run.
///
/// Carries both the per-step results (for the manifest's `steps:` and the
/// summary table) and the terminal state ID the run ended in. The terminal
/// state lives on this struct rather than on a `StepRunResult` because the
/// final state has no step record -- the driver does not run an agent for
/// `kind: final` states (see the early return in [`run_graph_flow`]).
///
/// Audit consumers (`kuro show-output`, the MCP `show_output` tool, log
/// parsers) want to know which `kind: final` state was reached -- that is
/// what tells `done` apart from `aborted`. Surfacing it here lets the
/// caller (`runner::execute_flow`) thread it into the manifest's
/// `final_state` field per issue #257.
pub struct GraphRunOutcome {
    pub steps: Vec<StepRunResult>,
    pub final_state: String,
}

/// Drive a graph flow from `initial:` to a `kind: final` state.
///
/// `state_to_agent` maps every non-terminal, non-human state ID to the
/// agent ID that handles it. The mapping is built up-front so a missing
/// role binding fails before any state runs (mirrors the linear runner's
/// "unknown agent" semantics).
pub async fn run_graph_flow(
    graph: &GraphFlow,
    agents_by_id: &HashMap<String, Agent>,
    state_to_agent: &HashMap<String, String>,
    ctx: &RunContext,
) -> Result<GraphRunOutcome, RunError> {
    stack::init_run_layout(&ctx.run_path).map_err(|e| RunError::Stack {
        step: "<run-init>".to_string(),
        source: e,
    })?;

    let executor = executor::create_executor();
    let mut results: Vec<StepRunResult> = Vec::new();
    let mut current = graph.initial.clone();
    let mut step_num: usize = 0;
    // Track the state the runtime just transitioned out of so the next
    // agent's prompt can include that step's artifact. Without this the
    // graph runs as a sequence of isolated agents, each blind to what its
    // predecessors produced. `None` for the first iteration.
    let mut prior_state: Option<String> = None;
    // Per-state visit counter. A loop between two states (the canonical
    // failure mode is `design <-> steer_design` where the steerer keeps
    // finding new issues) reaches `DEFAULT_MAX_STEPS` only after dozens of
    // transitions, by which point the run has burned through minutes of
    // agent time. Tracking visits per state lets us catch the loop after a
    // few rounds and abort with a message that names the offending state.
    let mut visits: HashMap<String, usize> = HashMap::new();

    loop {
        let state = graph.states.get(&current).ok_or_else(|| {
            // Schema/reachability validation should make this unreachable;
            // surface it as a runtime error rather than panicking so the
            // user gets a clear message if they ever construct a malformed
            // GraphFlow programmatically.
            RunError::GraphRuntime {
                state: current.clone(),
                reason: format!("state '{current}' not found in graph"),
            }
        })?;

        // Final state: terminate cleanly. We do NOT count terminal states
        // toward step_num so a `start -> final` graph runs exactly one step.
        if matches!(state.kind, Some(StateKind::Final)) {
            ui::print_graph_final(&current);
            // Hand the terminal state ID back to the caller so it can land
            // in the run's `manifest.yaml` (issue #257). Audit consumers
            // pull it from there to tell `done` apart from `aborted`.
            return Ok(GraphRunOutcome {
                steps: results,
                final_state: current,
            });
        }

        // Human-handoff is accepted at the schema level but the prototype
        // runtime does not know how to drive it. Refuse loudly rather than
        // silently treating it as a final state.
        if matches!(state.kind, Some(StateKind::Human)) {
            return Err(RunError::GraphRuntime {
                state: current.clone(),
                reason:
                    "kind: human handoff is accepted by the schema but not supported by the prototype runtime"
                        .to_string(),
            });
        }

        // Non-terminal state must have edges -- the reachability validator
        // catches "no edges and no kind" as a dead end before we get here,
        // but defend against an unvalidated GraphFlow anyway.
        let edges = state.edges.as_ref().ok_or_else(|| RunError::GraphRuntime {
            state: current.clone(),
            reason: "non-terminal state has no edges".to_string(),
        })?;
        if edges.is_empty() {
            return Err(RunError::GraphRuntime {
                state: current.clone(),
                reason: "non-terminal state has an empty edge set".to_string(),
            });
        }

        step_num += 1;
        if step_num > DEFAULT_MAX_STEPS {
            return Err(RunError::GraphRuntime {
                state: current.clone(),
                reason: format!(
                    "max_steps ({DEFAULT_MAX_STEPS}) exceeded; last state was '{current}'"
                ),
            });
        }

        // Per-state visit cap. Increment on entry (counts the current
        // visit) and abort if the same state has now been entered more than
        // `DEFAULT_MAX_VISITS_PER_STATE` times. Catches design <-> steer
        // ping-pong before it eats the global step budget. The error names
        // the offending state and the cap so the user can see what looped.
        let visit = visits.entry(current.clone()).or_insert(0);
        *visit += 1;
        if *visit > DEFAULT_MAX_VISITS_PER_STATE {
            return Err(RunError::GraphRuntime {
                state: current.clone(),
                reason: format!(
                    "state '{current}' visited {visit} times (cap {DEFAULT_MAX_VISITS_PER_STATE}); flow is stuck in a loop -- agents are not converging"
                ),
            });
        }

        // Resolve agent for this state. Missing means the role had no
        // binding -- already filtered by the setup phase, but check again
        // so the error path is explicit if someone calls this driver
        // directly.
        let agent_id = state_to_agent
            .get(&current)
            .ok_or_else(|| RunError::UnknownAgent {
                step: current.clone(),
                agent: state.role.clone().unwrap_or_default(),
            })?;
        let agent = agents_by_id
            .get(agent_id)
            .ok_or_else(|| RunError::UnknownAgent {
                step: current.clone(),
                agent: agent_id.clone(),
            })?;

        // Step banner mirrors the linear runner so the visual rhythm of a
        // graph run matches a linear run (#266). `total` is the global cap
        // because a graph has no fixed step count -- visit caps land in
        // #263 and will refine this. Edge names are not included in the
        // banner; they appear in the per-step prompt and in the post-step
        // transition line so the user can see what was picked vs available.
        let step_info = StepInfo {
            id: current.clone(),
            agent: agent.name.clone(),
            title: agent.title.clone(),
            model: agent.model.clone(),
            backend: agent.backend,
            input: Vec::new(),
            state: StepState::Running,
        };
        ui::print_step_banner(step_num, DEFAULT_MAX_STEPS, &step_info);

        // Read the prior state's persisted artifact so the next agent can
        // build on it. Without this the graph runs as a chain of strangers --
        // a steering agent reviewing nothing, an implementer reading no
        // design plan. Linear flows declare this explicitly via `step.input`;
        // graph flows always thread the immediately-prior state's artifact.
        // If the file is unreadable for any reason (corruption, race, etc.),
        // we fail loud rather than silently dropping context.
        let prior_context: Option<(String, String)> = match &prior_state {
            Some(prev_id) => {
                let body = stack::read_run_step_content(&ctx.run_path, prev_id).map_err(|e| {
                    RunError::Stack {
                        step: current.clone(),
                        source: e,
                    }
                })?;
                ui::print_context_injection(prev_id, prev_id, "");
                Some((prev_id.clone(), body))
            }
            None => None,
        };

        // Build the user prompt: top-level prompt (from ctx.task), per-state
        // task, prior-state context (if any), then the deterministic edge menu.
        // ctx.task is already var-substituted by the caller.
        let menu = build_menu(&current, edges);
        let base_prompt = build_state_user_prompt(
            &ctx.task,
            state.task.as_deref(),
            prior_context
                .as_ref()
                .map(|(id, body)| (id.as_str(), body.as_str())),
            &menu,
        );
        let system_prompt =
            build_system_prompt(agent, &ctx.guide, &ctx.rules_cache, &ctx.skills_cache);

        // First attempt + at most one retry. The retry note is empty on
        // attempt 1, so the same code path produces the canonical prompt
        // first and the retry-augmented prompt second.
        let allowed_keys: Vec<&str> = edges.keys().map(String::as_str).collect();
        let allowed_owned: Vec<String> = allowed_keys.iter().map(|s| s.to_string()).collect();
        let mut retry_note: Option<String> = None;
        let attempt_start = Instant::now();
        let started_at = chrono::Utc::now();

        // Pre-compute output path so the per-step layout matches linear runs
        // (issue #31: `<run>/steps/NN-<id>.md`) -- `kuro show-output` keys off
        // this filename pattern so any deviation breaks downstream tools.
        let content_filename = llm_output_filename(step_num, &current);
        let output_path = ctx
            .run_path
            .join(stack::STEPS_SUBDIR)
            .join(&content_filename);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let output_file = format!(
            "{}/{}/{}",
            ctx.run_id,
            stack::STEPS_SUBDIR,
            content_filename
        );

        // Spinner mirrors the linear runner so the user sees movement while
        // the agent is running. We start it after path setup so the path-
        // resolution prints (if any) do not race with the spinner repaint.
        let spinner = ui::start_spinner();

        let (decision, raw_content): (super::decision::AgentDecision, String) = {
            let mut last_raw: Option<String> = None;
            let mut outcome: Option<super::decision::AgentDecision> = None;
            for attempt in 0..2 {
                let user_prompt = match &retry_note {
                    Some(note) => format!("{base_prompt}{note}"),
                    None => base_prompt.clone(),
                };

                let raw = run_state_via_executor(
                    executor.as_ref(),
                    &current,
                    &ctx.flow_name,
                    &system_prompt,
                    &user_prompt,
                    &agent.model,
                    agent.backend,
                    &output_path,
                    attempt,
                )
                .await?;

                match parse_agent_decision(&raw) {
                    Ok(d) => match validate_transition(&d, &allowed_keys) {
                        Ok(()) => {
                            last_raw = Some(raw);
                            outcome = Some(d);
                            break;
                        }
                        Err(DecisionError::UnknownTransition { value, .. }) => {
                            if attempt == 1 {
                                return Err(RunError::GraphRuntime {
                                    state: current.clone(),
                                    reason: format!(
                                        "agent picked unknown transition `{value}` twice; allowed: {allowed_owned:?}"
                                    ),
                                });
                            }
                            retry_note = Some(unknown_edge_retry_note(&value, &allowed_owned));
                        }
                    },
                    Err(parse_err) => {
                        if attempt == 1 {
                            return Err(RunError::GraphRuntime {
                                state: current.clone(),
                                reason: format!(
                                    "agent reply could not be parsed twice: {parse_err}"
                                ),
                            });
                        }
                        retry_note = Some(malformed_retry_note(&parse_err));
                    }
                }
            }

            // The loop either `break`s with Some(outcome) or `return`s on
            // double-failure. If we ever fall through with None, that's a
            // bug -- surface it loudly rather than panicking on `unwrap`.
            let raw = last_raw.ok_or_else(|| RunError::GraphRuntime {
                state: current.clone(),
                reason: "internal: retry loop exited without a decision".to_string(),
            })?;
            let d = outcome.ok_or_else(|| RunError::GraphRuntime {
                state: current.clone(),
                reason: "internal: retry loop exited without a decision".to_string(),
            })?;
            (d, raw)
        };

        spinner.stop();
        let duration = attempt_start.elapsed();

        // Resolve the next state BEFORE persisting the step record so the
        // record can include the resolved target alongside the agent's
        // raw decision -- audit consumers should not have to re-derive
        // `next_state` from the edge map.
        let next = edges
            .get(&decision.transition)
            .map(|e| e.to.clone())
            .ok_or_else(|| RunError::GraphRuntime {
                // validate_transition guarantees the key exists; this arm
                // is defensive.
                state: current.clone(),
                reason: format!(
                    "internal: transition '{}' missing from edge set",
                    decision.transition
                ),
            })?;

        // Persist the per-step output. `raw_content` is the full agent reply
        // including the JSON envelope; we keep it verbatim so the audit
        // trail shows exactly what was emitted. `graph_decision` carries the
        // structured transition data so `meta.yaml` is self-describing
        // without parsing the content markdown.
        let record = StepRecord {
            step_id: current.clone(),
            kind: "graph".to_string(),
            agent: Some(agent.name.clone()),
            model_requested: Some(agent.model.clone()),
            model_actual: Some(agent.model.clone()),
            backend: backend_name(agent.backend).to_string(),
            tokens_in: None,
            tokens_out: None,
            duration_ms: duration.as_millis(),
            started_at: started_at.to_rfc3339(),
            exit_code: 0,
            input_steps: prior_state.iter().cloned().collect(),
            output_file: content_filename.clone(),
            participants: Vec::new(),
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: Some(GraphDecision {
                transition: decision.transition.clone(),
                reason: decision.reason.clone(),
                next_state: next.clone(),
            }),
        };
        stack::write_run_step(&ctx.run_path, step_num, &record, &raw_content).map_err(|e| {
            RunError::Stack {
                step: current.clone(),
                source: e,
            }
        })?;

        let display_path = output_path
            .canonicalize()
            .unwrap_or(output_path.clone())
            .display()
            .to_string();
        ui::print_step_done(&format_duration(duration), "—", "—", &display_path);
        ui::print_graph_transition(&decision.transition, &next, &decision.reason);

        results.push(StepRunResult {
            step_id: current.clone(),
            agent_name: agent.name.clone(),
            backend: backend_name(agent.backend).to_string(),
            duration,
            tokens_in: None,
            tokens_out: None,
            output_file,
            print_output: false,
            record,
        });

        // Remember the state we just ran so the next iteration can splice
        // its artifact into the next agent's prompt. Done before reassigning
        // `current` so `current` still names the just-completed state.
        prior_state = Some(current.clone());
        current = next;
    }
}

/// Build the user-facing prompt for a single state visit:
/// flow-level prompt (carried via `ctx.task`) + per-state `task:` (if any) +
/// optional prior-state context + the deterministic edge menu.
///
/// `prior_context` is `(state_id, body)` for the immediately-preceding state
/// in this run. The framing wrapper mirrors the linear runner's
/// `build_user_prompt` so agents see a consistent context envelope across
/// flow shapes.
fn build_state_user_prompt(
    flow_prompt: &str,
    state_task: Option<&str>,
    prior_context: Option<(&str, &str)>,
    menu: &str,
) -> String {
    let mut out = flow_prompt.to_string();
    if let Some(t) = state_task {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Your task: ");
        out.push_str(t);
    }
    if let Some((prev_id, body)) = prior_context {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "Context from previous state '{prev_id}':\n\n--- Output from state '{prev_id}' ---\n{body}\n---\n\nIMPORTANT: The above is the artifact the previous agent produced. Read it as the input to your task. Build on it -- do not repeat or rephrase what is already covered.",
        ));
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(menu);
    out
}

/// Spawn one state-step on the executor and return the raw stdout.
///
/// Almost a copy of `run_step_via_executor` from the linear runner, but
/// scoped to the graph driver so the linear path stays untouched. The
/// `attempt` index is folded into the executor task ID so retries do not
/// collide on the per-process job table.
#[allow(clippy::too_many_arguments)]
async fn run_state_via_executor(
    executor: &dyn ExecutorBoxed,
    state_id: &str,
    flow_name: &str,
    system_prompt: &str,
    user_content: &str,
    model: &str,
    backend: Backend,
    output_path: &Path,
    attempt: usize,
) -> Result<String, RunError> {
    let project = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let short_id = &chrono::Utc::now().timestamp_millis().to_string()[8..];
    let task_id = format!("kuro-{project}-{flow_name}-graph-{state_id}-a{attempt}-{short_id}");

    let command = match backend {
        Backend::ClaudeCli => {
            executor::build_claude_command(model, Some(system_prompt), user_content, &[])
        }
        Backend::Codex => {
            executor::build_codex_command(model, Some(system_prompt), user_content, &[])
        }
        Backend::Ollama => {
            // Ollama has no separate system slot, so we inline the prompt
            // the same way the linear runner does. Keeping the format
            // identical means a user comparing linear and graph runs sees
            // the same text shape land on disk.
            let mut prompt = String::new();
            prompt.push_str(&format!("System: {system_prompt}\n\n"));
            prompt.push_str(&format!("User: {user_content}"));
            executor::build_ollama_command(model, &prompt, &[])
        }
        Backend::Api => {
            return Err(RunError::GraphRuntime {
                state: state_id.to_string(),
                reason: "graph driver does not yet support the api backend; use claude-cli, codex, or ollama".to_string(),
            });
        }
    };

    let output_format = match backend {
        Backend::ClaudeCli => OutputFormat::ClaudeStreamJson,
        _ => OutputFormat::Raw,
    };

    let task = ExecutionTask {
        id: task_id,
        command,
        env: HashMap::new(),
        stdout_file: Some(output_path.to_path_buf()),
        output_format,
    };

    let handle = executor
        .spawn_boxed(task)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            step: state_id.to_string(),
            source: e,
        })?;

    let output = executor
        .wait_boxed(&handle)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            step: state_id.to_string(),
            source: e,
        })?;

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn edges(pairs: &[(&str, &str, &str)]) -> IndexMap<String, GraphEdge> {
        let mut m = IndexMap::new();
        for (name, to, desc) in pairs {
            m.insert(
                (*name).to_string(),
                GraphEdge {
                    to: (*to).to_string(),
                    description: (*desc).to_string(),
                },
            );
        }
        m
    }

    #[test]
    fn build_menu_renders_canonical_format() {
        let e = edges(&[
            ("approved", "merge", "All checks pass."),
            ("rework", "fix", "Tests are red."),
        ]);
        let s = build_menu("review", &e);
        assert!(s.contains("You are at state `review`."));
        assert!(s.contains(
            "{\"transition\": \"<edge-name>\", \"reason\": \"<one to two sentences explaining why>\"}"
        ));
        assert!(s.contains("- `approved`: All checks pass."));
        assert!(s.contains("- `rework`: Tests are red."));
        assert!(s.contains("produce the full artifact"));
        assert!(s.contains("appear exactly once"));
    }

    #[test]
    fn build_menu_preserves_edge_declaration_order() {
        // IndexMap insertion order matters: the agent must see edges in
        // the order the YAML declared them, not lexicographic.
        let e = edges(&[
            ("zeta", "z", "last in alphabet"),
            ("alpha", "a", "first in alphabet"),
        ]);
        let s = build_menu("s", &e);
        let zeta_idx = s.find("`zeta`").expect("zeta listed");
        let alpha_idx = s.find("`alpha`").expect("alpha listed");
        assert!(
            zeta_idx < alpha_idx,
            "menu must list edges in IndexMap order, got:\n{s}"
        );
    }

    #[test]
    fn build_state_user_prompt_with_flow_task_state_task_and_menu() {
        let out = build_state_user_prompt(
            "flow goal here",
            Some("review the patch"),
            None,
            "MENU GOES HERE",
        );
        assert!(out.starts_with("flow goal here"));
        assert!(out.contains("Your task: review the patch"));
        assert!(out.ends_with("MENU GOES HERE"));
        assert!(!out.contains("Context from previous state"));
    }

    #[test]
    fn build_state_user_prompt_with_only_flow_and_menu() {
        let out = build_state_user_prompt("flow goal", None, None, "MENU");
        assert!(out.starts_with("flow goal"));
        assert!(out.ends_with("MENU"));
        assert!(!out.contains("Your task:"));
        assert!(!out.contains("Context from previous state"));
    }

    #[test]
    fn build_state_user_prompt_includes_prior_state_artifact() {
        // The next agent must see the previous state's artifact, otherwise
        // graph runs collapse into a chain of strangers each blind to what
        // came before.
        let out = build_state_user_prompt(
            "flow goal",
            Some("steer the design"),
            Some(("design", "the design plan body")),
            "MENU",
        );
        assert!(out.contains("Context from previous state 'design'"));
        assert!(out.contains("the design plan body"));
        assert!(out.contains("Build on it"));
        assert!(out.ends_with("MENU"));
    }

    #[test]
    fn unknown_edge_retry_note_lists_allowed_choices() {
        let note = unknown_edge_retry_note("invented", &["approved".into(), "rework".into()]);
        assert!(note.contains("`invented`"));
        assert!(note.contains("`approved`"));
        assert!(note.contains("`rework`"));
    }

    #[test]
    fn malformed_retry_note_includes_parse_error() {
        // Force a real parse error so the Display impl is exercised.
        let err = parse_agent_decision("not json at all").expect_err("must error");
        let note = malformed_retry_note(&err);
        assert!(note.contains("malformed"));
        assert!(note.contains("Reply again with a JSON object"));
    }
}
