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

use crate::config::{Agent, Backend};
use crate::core::{GraphFlow, SelectEntry};
use crate::executor::{self, ExecutionTask, ExecutorBoxed, ExecutorError, OutputFormat};
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
pub fn build_menu(state_id: &str, entries: &[SelectEntry]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "You are at state `{state_id}`. Complete the task above as your main reply -- produce the full artifact (design plan, review, code summary, etc.) so the next agent in the graph can build on your work. After the artifact, end your reply with a single JSON object choosing one transition:\n\n",
    ));
    s.push_str(
        "{\"transition\": \"<target-state>\", \"reason\": \"<one to two sentences explaining why>\"}\n\n",
    );
    s.push_str("Available transitions:\n");
    for entry in entries {
        let reason = entry
            .reason
            .as_ref()
            .map(|r| r.display())
            .unwrap_or_default();
        s.push_str(&format!("- `{}`: {}\n", entry.target, reason));
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

/// Lifecycle outcome of a graph-flow run.
///
/// Pause is not an error -- a `human: true` state suspends the run so a
/// human operator can react, then a future `kuro resume` (#338) picks it
/// back up. Mixing pause into [`RunError`] would force every caller to
/// pattern-match `Err(_)` to tell pause apart from real failures, so the
/// outcome enum carries both successful lifecycle endings:
///
/// * `Final` -- the driver reached a state with `kind: final`. The terminal
///   state ID lives here rather than on a `StepRunResult` because the
///   driver does not run an agent for `kind: final` states (see the early
///   return in [`run_graph_flow`]). Audit consumers (`kuro show-output`,
///   the MCP `show_output` tool, log parsers) read it to tell `done` apart
///   from `aborted` -- per issue #257.
///
/// * `Paused` -- the driver reached a state with `human: true`. The graph
///   position is recorded into the manifest's `status: paused` block so
///   `kuro resume` (#338) can later pick the run back up. Pausing must
///   NOT write a step record: a human-handoff state has no agent reply
///   and writing a synthetic record would muddy `kuro show-output`. The
///   driver therefore returns before incrementing `step_num`, mirroring
///   the `Final` arm's contract.
pub enum GraphRunOutcome {
    Final {
        steps: Vec<StepRunResult>,
        final_state: String,
    },
    Paused {
        steps: Vec<StepRunResult>,
        paused_at_state: String,
        paused_at: chrono::DateTime<chrono::Utc>,
    },
}

/// Re-entry point for `kuro resume` (#338).
///
/// Threaded into [`run_graph_flow`] as `Some(...)` to start the driver
/// from a previously-paused state instead of `graph.initial`. The
/// `step_num_offset` is the number of step records already on disk for
/// this run -- the driver uses it to start its local `step_num` at
/// `offset + 1` so the next persisted artifact does not collide with
/// the pre-pause `NN-<id>.md` files.
pub struct ResumeFrom {
    pub state: String,
    pub step_num_offset: usize,
}

/// Drive a graph flow from `initial:` (or, when `resume` is set, from a
/// previously-paused state) to a `kind: final` state.
///
/// `state_to_agent` maps every non-terminal, non-human state ID to the
/// agent ID that handles it. The mapping is built up-front so a missing
/// role binding fails before any state runs (mirrors the linear runner's
/// "unknown agent" semantics).
///
/// `resume` is `None` for fresh runs (existing semantics: start at
/// `graph.initial`, step counter at 0). When `Some(ResumeFrom { state,
/// step_num_offset })`, the driver starts at `state` and seeds its local
/// step counter at `step_num_offset` so resumed runs continue the
/// `NN-<id>.md` numbering of the pre-pause phase. On the FIRST iteration
/// only, a human-handoff state is treated as a transit point rather than
/// a pause: the driver advances to `next[0].target` instead of returning
/// `Paused`. This keeps the resume contract self-contained -- the CLI
/// does not have to special-case "resume into a paused state" because
/// the driver already knows it has just been re-entered. The policy
/// (first `next:` entry wins) is intentionally minimal for v1 (#338);
/// human-input routing replaces it in #340.
pub async fn run_graph_flow(
    graph: &GraphFlow,
    agents_by_id: &HashMap<String, Agent>,
    state_to_agent: &HashMap<String, String>,
    ctx: &RunContext,
    resume: Option<ResumeFrom>,
) -> Result<GraphRunOutcome, RunError> {
    // Announce dispatch into the graph runtime on stderr so callers and
    // tests can confirm we did not silently fall through to the linear
    // DAG loader. Emitted before any I/O so even an init failure leaves
    // a breadcrumb on the stream.
    ui::print_graph_run_start(&graph.name);

    stack::init_run_layout(&ctx.run_path).map_err(|e| RunError::Stack {
        step: "<run-init>".to_string(),
        source: e,
    })?;

    let executor = executor::create_executor();
    let mut results: Vec<StepRunResult> = Vec::new();
    let (mut current, mut step_num, mut skip_pause_once) = match &resume {
        // Fresh: start at the graph's declared initial state, counter at
        // 0, and never skip the pause check.
        None => (graph.initial.clone(), 0usize, false),
        // Resume: start at the named state, seed the counter so the next
        // increment lands at `offset + 1`, and arm the one-shot "advance
        // through a human state" flag for the first iteration only.
        Some(r) => (r.state.clone(), r.step_num_offset, true),
    };
    // Track the state the runtime just transitioned out of so the next
    // agent's prompt can include that step's artifact. Without this the
    // graph runs as a sequence of isolated agents, each blind to what its
    // predecessors produced. `None` for the first iteration.
    //
    // Resume note: on re-entry the prior step's artifact lives on disk
    // under the pre-pause run dir, but #338 does not thread it back into
    // the prompt -- the human state wrote nothing, so there is no
    // single "prior" body to splice. Richer context handoff lands with
    // #340 (resume input plumbing). Keeping `prior_state = None` here
    // means the first agent after resume sees no prior-context block,
    // which is honest about what is available.
    let mut prior_state: Option<String> = None;
    // Per-state visit counter. A loop between two states (the canonical
    // failure mode is `design <-> steer_design` where the steerer keeps
    // finding new issues) reaches `DEFAULT_MAX_STEPS` only after dozens of
    // transitions, by which point the run has burned through minutes of
    // agent time. Tracking visits per state lets us catch the loop after a
    // few rounds and abort with a message that names the offending state.
    let mut visits: HashMap<String, usize> = HashMap::new();

    loop {
        let state = graph
            .graph
            .get(&current)
            .ok_or_else(|| RunError::GraphRuntime {
                state: current.clone(),
                reason: format!("state '{current}' not found in graph"),
            })?;

        // Final state: terminate cleanly.
        if state.is_final() {
            ui::print_graph_final(&current);
            return Ok(GraphRunOutcome::Final {
                steps: results,
                final_state: current,
            });
        }

        // Human-handoff: suspend the run cleanly so a human operator can
        // react. The graph position lives on the outcome so the caller
        // can record `status: paused` into the manifest. No step record
        // is written -- a human state has no agent reply -- and the
        // driver returns before `step_num` increments, mirroring the
        // final-state contract. `kuro resume` (#338) will read the
        // manifest back to pick up where we stopped.
        //
        // Resume exception: on the FIRST iteration after re-entry, a
        // human state is the entry point itself -- pausing again would
        // deadlock the run. Take the first declared `next:` entry and
        // continue the loop instead. Subsequent visits to a human state
        // (after the resume's first iteration) pause normally. The
        // "first declared `next:`" policy is the v1 minimum from #338;
        // #340 replaces it with input-routing once human comments feed
        // back into the run.
        if state.is_human() {
            if skip_pause_once {
                skip_pause_once = false;
                let advance = state.select.as_ref().and_then(|s| s.first());
                let next = advance.ok_or_else(|| RunError::GraphRuntime {
                    state: current.clone(),
                    reason: format!(
                        "paused state '{current}' has no `next:` entries; cannot resume -- declare at least one outgoing edge in the flow before retrying"
                    ),
                })?;
                let next_state = next.target.clone();
                ui::print_graph_transition(
                    &next_state,
                    &next_state,
                    "resumed -- advancing past human handoff",
                );
                // The human state wrote no artifact at pause time, so
                // `prior_state` stays None for the next iteration --
                // see the comment block at the top of `run_graph_flow`.
                prior_state = None;
                current = next_state;
                continue;
            }
            let paused_at = chrono::Utc::now();
            ui::print_graph_paused(&current);
            return Ok(GraphRunOutcome::Paused {
                steps: results,
                paused_at_state: current,
                paused_at,
            });
        }

        // Non-terminal state must have select entries.
        let entries = state
            .select
            .as_ref()
            .ok_or_else(|| RunError::GraphRuntime {
                state: current.clone(),
                reason: "non-terminal state has no select entries".to_string(),
            })?;
        if entries.is_empty() {
            return Err(RunError::GraphRuntime {
                state: current.clone(),
                reason: "non-terminal state has an empty select list".to_string(),
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

        // Shell state: deterministic, exit-code-routed gate,
        // no LLM call.
        if state.is_shell() {
            let shell_outcome = run_shell_state(
                executor.as_ref(),
                &current,
                entries,
                state.run.as_deref().unwrap_or(""),
                ctx,
                step_num,
            )
            .await?;
            results.push(shell_outcome.result);
            prior_state = Some(current.clone());
            current = shell_outcome.next_state;
            continue;
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
        // task, prior-state context (if any), then the deterministic select menu.
        // ctx.task is already var-substituted by the caller.
        let menu = build_menu(&current, entries);
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
        let allowed_keys: Vec<&str> = entries.iter().map(|e| e.target.as_str()).collect();
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

        // In the new schema, the transition IS the target state name.
        // Look up the reason from the select entry for audit records.
        let next = decision.transition.clone();
        let matched_reason = entries
            .iter()
            .find(|e| e.target == decision.transition)
            .and_then(|e| e.reason.as_ref())
            .map(|r| r.display());

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
                matched_reason,
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

/// Outcome of a single `kind: shell` state execution: the recorded step
/// (so the caller can append it to `results`) plus the resolved next
/// state ID. Lives next to [`run_shell_state`] because nothing else needs
/// the shape -- the caller just splats both fields back into its own
/// loop variables.
#[derive(Debug)]
struct ShellStateOutcome {
    result: StepRunResult,
    next_state: String,
}

/// Drive a single `kind: shell` state (issue #310).
///
/// Spawns the configured `command` via `sh -c` through the local
/// executor, captures stdout / stderr / exit code / duration, persists a
/// `kind: shell` step record under `<run>/steps/NN-<state>.txt`, picks
/// the outgoing edge by exit code (`0` -> first `on: pass`, non-zero ->
/// first `on: fail`), and returns the persisted result + next state.
///
/// The on-disk artifact mirrors the synthesized framing block injected
/// into the next agent's prompt (`$ <command>`, exit code, stdout,
/// stderr) so a developer reading `kuro show-output` sees the same
/// payload the next state's agent saw via `prior_context`. Stderr is
/// also surfaced live to the user (matching the linear-shell-step
/// contract from issue #23).
///
/// Edge selection deliberately walks edges in declaration order with an
/// explicit `on:` tag check rather than positional convention: reordering
/// YAML keys would otherwise silently re-route the run. Validation in
/// `validate_shell_state_semantics` guarantees at least one `on: pass`
/// and one `on: fail` edge exist, but the runtime defends against
/// missing tags as well in case an unvalidated `GraphFlow` is built
/// programmatically.
async fn run_shell_state(
    executor: &dyn ExecutorBoxed,
    state_id: &str,
    entries: &[SelectEntry],
    command: &str,
    ctx: &RunContext,
    step_num: usize,
) -> Result<ShellStateOutcome, RunError> {
    if command.trim().is_empty() {
        return Err(RunError::GraphRuntime {
            state: state_id.to_string(),
            reason: "shell state has no command to run".to_string(),
        });
    }

    ui::print_shell_step_banner(step_num, DEFAULT_MAX_STEPS, state_id, command, &[]);

    // Output filename mirrors the linear shell-step convention (`.txt`,
    // not `.md`) so `kuro show-output` and `print_output: true` do not
    // try to render shell stdout through termimad.
    let content_filename = stack::step_content_filename(step_num, state_id, "txt");
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

    let project = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let short_id = &chrono::Utc::now().timestamp_millis().to_string()[8..];
    let task_id = format!(
        "kuro-{project}-{}-graph-{state_id}-shell-{short_id}",
        ctx.flow_name
    );

    let task = ExecutionTask {
        id: task_id,
        command: command.to_string(),
        env: HashMap::new(),
        // Stream stdout to the artifact file so users can `tail -f` a
        // long-running `just test`. We rewrite the file below to fold
        // stderr + exit code into the same body, but streaming during
        // execution still gives a live view of what the command emitted.
        stdout_file: Some(output_path.to_path_buf()),
        output_format: OutputFormat::Raw,
    };

    let start = Instant::now();
    let started_at = chrono::Utc::now();
    let spinner = ui::start_spinner();

    let handle = executor
        .spawn_boxed(task)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            step: state_id.to_string(),
            source: e,
        })?;

    // The executor's `wait` contract treats any non-zero exit as an error
    // (`ExecutorError::Failed { code, message }`) so the linear runner can
    // bail on a failing shell step. The shell-state contract is the
    // opposite: a non-zero exit is the routing signal we explicitly want
    // to fork on, so we recover the failed-but-completed case here and
    // synthesise an `ExecutionOutput` from what survives.
    //
    // What the Err path discards:
    // - stdout: still on disk because we passed `stdout_file:` -- read it
    //   back from `output_path`. Empty file is fine (commands often emit
    //   nothing when they fail with a clear error to stderr).
    // - stderr: prefixed into `message` as `"process exited with code N: <stderr>"`.
    //   Strip the prefix back off so the artifact body shows the real
    //   stderr instead of leaking the executor's internal wording.
    let (stdout, stderr, exit_code) = match executor.wait_boxed(&handle).await {
        Ok(out) => (out.stdout, out.stderr, out.exit_code),
        Err(ExecutorError::Failed { code, message }) => {
            let stdout = std::fs::read_to_string(&output_path)
                .unwrap_or_default()
                .trim()
                .to_string();
            let prefix = format!("process exited with code {code}: ");
            let stderr = message
                .strip_prefix(&prefix)
                .unwrap_or(&message)
                .to_string();
            (stdout, stderr, code)
        }
        Err(e) => {
            return Err(RunError::ExecutorFailed {
                step: state_id.to_string(),
                source: e,
            });
        }
    };

    spinner.stop();
    let duration = start.elapsed();

    if !stderr.is_empty() {
        eprintln!("{}", stderr.trim_end());
    }

    // Pick the next state from the exit code BEFORE persisting the step.
    let want = if exit_code == 0 { "pass" } else { "fail" };
    let entry = entries
        .iter()
        .find(|e| e.reason.as_ref().map(|r| r.display()).unwrap_or_default() == want)
        .ok_or_else(|| RunError::GraphRuntime {
            state: state_id.to_string(),
            reason: format!(
                "shell state exited with code {exit_code} but no select entry has reason '{want}'"
            ),
        })?;
    let transition_name = entry.target.clone();
    let next_state = entry.target.clone();

    // Synthesize the body that lands on disk AND in the next state's
    // prior_context. Includes the command, exit code, stdout, and stderr
    // so a downstream agent (e.g. Noah re-reading a failing `just test`)
    // does not have to read the manifest separately to see what failed.
    let body = render_shell_artifact(command, exit_code, &stdout, &stderr);

    let record = StepRecord {
        step_id: state_id.to_string(),
        kind: "shell".to_string(),
        agent: None,
        model_requested: None,
        model_actual: None,
        backend: "shell".to_string(),
        tokens_in: None,
        tokens_out: None,
        duration_ms: duration.as_millis(),
        started_at: started_at.to_rfc3339(),
        exit_code,
        input_steps: Vec::new(),
        output_file: content_filename.clone(),
        participants: Vec::new(),
        turns: None,
        messages: None,
        terminated_by: None,
        graph_decision: Some(GraphDecision {
            transition: transition_name.clone(),
            reason: format!("exit code {exit_code}"),
            matched_reason: Some(want.to_string()),
        }),
    };
    stack::write_run_step(&ctx.run_path, step_num, &record, &body).map_err(|e| {
        RunError::Stack {
            step: state_id.to_string(),
            source: e,
        }
    })?;

    let display_path = output_path
        .canonicalize()
        .unwrap_or(output_path.clone())
        .display()
        .to_string();
    ui::print_step_done(&format_duration(duration), "—", "—", &display_path);
    ui::print_graph_transition(
        &transition_name,
        &next_state,
        &format!("exit code {exit_code}"),
    );

    let result = StepRunResult {
        step_id: state_id.to_string(),
        agent_name: "shell".to_string(),
        backend: "shell".to_string(),
        duration,
        tokens_in: None,
        tokens_out: None,
        output_file,
        print_output: false,
        record,
    };

    Ok(ShellStateOutcome { result, next_state })
}

/// Build the on-disk + prior-context payload for a shell state.
///
/// The format is intentionally explicit so the next state's agent (e.g.
/// Noah after a failing `just test`) sees three labelled blocks rather
/// than guessing what got concatenated. Stderr is included verbatim --
/// failing test output frequently lives there, and dropping it would
/// defeat the whole point of the verify gate.
fn render_shell_artifact(command: &str, exit_code: i32, stdout: &str, stderr: &str) -> String {
    let mut out = String::new();
    out.push_str("$ ");
    out.push_str(command);
    out.push('\n');
    out.push_str(&format!("exit code: {exit_code}\n"));
    out.push_str("--- stdout ---\n");
    out.push_str(stdout);
    if !stdout.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("--- stderr ---\n");
    out.push_str(stderr);
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SelectReason;

    fn select_entries(pairs: &[(&str, &str)]) -> Vec<SelectEntry> {
        pairs
            .iter()
            .map(|(target, reason)| SelectEntry {
                target: (*target).to_string(),
                reason: if reason.is_empty() {
                    None
                } else {
                    Some(SelectReason::Single((*reason).to_string()))
                },
            })
            .collect()
    }

    #[test]
    fn build_menu_renders_canonical_format() {
        let e = select_entries(&[("merge", "All checks pass."), ("fix", "Tests are red.")]);
        let s = build_menu("review", &e);
        assert!(s.contains("You are at state `review`."));
        assert!(s.contains(
            "{\"transition\": \"<target-state>\", \"reason\": \"<one to two sentences explaining why>\"}"
        ));
        assert!(s.contains("- `merge`: All checks pass."));
        assert!(s.contains("- `fix`: Tests are red."));
        assert!(s.contains("produce the full artifact"));
        assert!(s.contains("appear exactly once"));
    }

    #[test]
    fn build_menu_preserves_select_order() {
        let e = select_entries(&[("zeta", "last in alphabet"), ("alpha", "first in alphabet")]);
        let s = build_menu("s", &e);
        let zeta_idx = s.find("`zeta`").expect("zeta listed");
        let alpha_idx = s.find("`alpha`").expect("alpha listed");
        assert!(
            zeta_idx < alpha_idx,
            "menu must list select entries in order, got:\n{s}"
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

    #[test]
    fn render_shell_artifact_labels_three_blocks() {
        // Acceptance: the artifact body must be self-describing so a
        // downstream agent reading prior_context can tell command, exit
        // code, stdout, and stderr apart without inferring from layout.
        let body = render_shell_artifact("just test", 1, "test 5 ok\n", "test 6 FAILED\n");
        assert!(body.starts_with("$ just test\n"));
        assert!(body.contains("exit code: 1\n"));
        assert!(body.contains("--- stdout ---\ntest 5 ok\n"));
        assert!(body.contains("--- stderr ---\ntest 6 FAILED\n"));
    }

    #[test]
    fn render_shell_artifact_handles_missing_trailing_newlines() {
        // Streamed shell output frequently lacks a final newline; the
        // artifact must still be readable rather than mashing the next
        // section onto the same line.
        let body = render_shell_artifact("echo hi", 0, "hi", "");
        assert!(body.contains("hi\n--- stderr ---"));
        // Empty stderr does not get an extra trailing newline appended.
        assert!(body.ends_with("--- stderr ---\n"));
    }

    // --- Shell-state driver tests (issue #310) --------------------------
    //
    // These exercise `run_shell_state` against the real `LocalExecutor`
    // because the executor's wait/error contract is half of what we are
    // verifying (non-zero exit -> ExecutorError::Failed -> recovered into
    // an Ok routing decision). A mock would just re-encode the contract
    // we want to pin.

    use crate::executor;
    use crate::runner::RunContext;
    use std::collections::HashMap;

    fn shell_state_ctx(stack_path: std::path::PathBuf) -> RunContext {
        RunContext::new(
            "test-graph".to_string(),
            "irrelevant for shell states".to_string(),
            stack_path,
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )
    }

    fn pass_fail_entries() -> Vec<SelectEntry> {
        vec![
            SelectEntry {
                target: "next_pass".to_string(),
                reason: Some(SelectReason::Single("pass".to_string())),
            },
            SelectEntry {
                target: "next_fail".to_string(),
                reason: Some(SelectReason::Single("fail".to_string())),
            },
        ]
    }

    #[tokio::test]
    async fn shell_state_routes_pass_on_exit_zero() {
        // Acceptance: exit code 0 selects the first edge tagged `on: pass`
        // and persists a step record carrying the resolved next state.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_state_ctx(dir.path().to_path_buf());
        std::fs::create_dir_all(ctx.run_path.join(stack::STEPS_SUBDIR)).unwrap();
        let executor = executor::create_executor();
        let entries = pass_fail_entries();

        let outcome = run_shell_state(executor.as_ref(), "verify", &entries, "true", &ctx, 1)
            .await
            .unwrap();

        assert_eq!(outcome.next_state, "next_pass");
        assert_eq!(outcome.result.record.exit_code, 0);
        let decision = outcome.result.record.graph_decision.as_ref().unwrap();
        assert_eq!(decision.transition, "next_pass");
    }

    #[tokio::test]
    async fn shell_state_routes_fail_on_nonzero_exit() {
        // Acceptance: non-zero exit selects the first `on: fail` edge.
        // The executor signals non-zero exits via `ExecutorError::Failed`,
        // which the shell-state driver must recover from rather than
        // propagating -- the whole point of the verify gate.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_state_ctx(dir.path().to_path_buf());
        std::fs::create_dir_all(ctx.run_path.join(stack::STEPS_SUBDIR)).unwrap();
        let executor = executor::create_executor();
        let entries = pass_fail_entries();

        let outcome = run_shell_state(
            executor.as_ref(),
            "verify",
            &entries,
            "echo boom 1>&2; exit 7",
            &ctx,
            1,
        )
        .await
        .unwrap();

        assert_eq!(outcome.next_state, "next_fail");
        assert_eq!(outcome.result.record.exit_code, 7);
        let decision = outcome.result.record.graph_decision.as_ref().unwrap();
        assert_eq!(decision.transition, "next_fail");
    }

    #[tokio::test]
    async fn shell_state_persists_artifact_with_command_and_exit() {
        // The artifact written under steps/NN-<id>.txt is the same body
        // injected into the next state's prior_context. Verify the
        // command, exit code, and captured stdout / stderr land in it.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_state_ctx(dir.path().to_path_buf());
        std::fs::create_dir_all(ctx.run_path.join(stack::STEPS_SUBDIR)).unwrap();
        let executor = executor::create_executor();
        let entries = pass_fail_entries();

        run_shell_state(
            executor.as_ref(),
            "verify",
            &entries,
            "printf out; printf err 1>&2; exit 3",
            &ctx,
            2,
        )
        .await
        .unwrap();

        let body = stack::read_run_step_content(&ctx.run_path, "verify").unwrap();
        assert!(body.contains("$ printf out; printf err 1>&2; exit 3"));
        assert!(body.contains("exit code: 3"));
        assert!(body.contains("--- stdout ---"));
        assert!(body.contains("out"));
        assert!(body.contains("--- stderr ---"));
        assert!(body.contains("err"));
    }

    #[tokio::test]
    async fn shell_state_rejects_empty_command_at_runtime() {
        // The schema validator already blocks empty commands, but the
        // driver also defends against a programmatically-built GraphFlow
        // skipping validation -- otherwise an empty command would be
        // silently passed to `sh -c` and route as success.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_state_ctx(dir.path().to_path_buf());
        std::fs::create_dir_all(ctx.run_path.join(stack::STEPS_SUBDIR)).unwrap();
        let executor = executor::create_executor();
        let entries = pass_fail_entries();

        let err = run_shell_state(executor.as_ref(), "verify", &entries, "   ", &ctx, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::GraphRuntime { .. }));
        assert!(err.to_string().contains("no command"));
    }

    #[tokio::test]
    async fn shell_state_errors_when_no_edge_matches_exit() {
        // Programmatic GraphFlow with only an `on: pass` edge: a non-zero
        // exit has nowhere to route, so the driver bails with a clear
        // GraphRuntime error rather than picking an arbitrary edge.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_state_ctx(dir.path().to_path_buf());
        std::fs::create_dir_all(ctx.run_path.join(stack::STEPS_SUBDIR)).unwrap();
        let executor = executor::create_executor();
        let entries = vec![SelectEntry {
            target: "done".to_string(),
            reason: Some(SelectReason::Single("pass".to_string())),
        }];

        let err = run_shell_state(executor.as_ref(), "verify", &entries, "exit 1", &ctx, 1)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'fail'"),
            "expected message naming 'fail', got: {msg}"
        );
    }

    // --- Human-pause driver tests (issue #337) --------------------------
    //
    // These exercise `run_graph_flow` directly so the pause contract --
    // clean Ok with `GraphRunOutcome::Paused`, no step record written,
    // pause arm carries the correct state ID and a recent timestamp --
    // is locked at the driver layer rather than only at the integration
    // level. The graph driver is the single source of truth for pause
    // semantics; downstream wiring (manifest, summary) just translates.

    use crate::core::GraphFlow;
    use crate::core::GraphState;
    use indexmap::IndexMap;

    fn graph_flow_with_human_initial() -> GraphFlow {
        // Smallest valid graph: `initial: ask`, `ask: { human: true }`.
        // The driver only consults `is_human()` on the current state, so
        // a human state with no `next:` is enough to verify pause.
        let mut graph: IndexMap<String, GraphState> = IndexMap::new();
        graph.insert(
            "ask".to_string(),
            GraphState {
                human: Some(true),
                ..Default::default()
            },
        );
        let mut flow = GraphFlow::default();
        flow.name = "human-pause-test".to_string();
        flow.initial = "ask".to_string();
        flow.graph = graph;
        flow
    }

    #[tokio::test]
    async fn human_state_returns_paused_outcome() {
        // Acceptance: reaching a `human: true` state must terminate the
        // driver cleanly with `GraphRunOutcome::Paused` carrying the
        // state ID and a timestamp. No `RunError` -- pause is not a
        // failure, mixing it into the error type would force every
        // caller to disambiguate via pattern-match.
        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "human-pause-test".to_string(),
            "test prompt".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let flow = graph_flow_with_human_initial();
        let agents_by_id: HashMap<String, crate::config::Agent> = HashMap::new();
        let state_to_agent: HashMap<String, String> = HashMap::new();

        let before = chrono::Utc::now();
        let outcome = run_graph_flow(&flow, &agents_by_id, &state_to_agent, &ctx, None)
            .await
            .expect("pause must be Ok, not Err");
        let after = chrono::Utc::now();

        match outcome {
            GraphRunOutcome::Paused {
                steps,
                paused_at_state,
                paused_at,
            } => {
                assert_eq!(paused_at_state, "ask");
                assert!(
                    steps.is_empty(),
                    "pause at the initial human state must not record any steps"
                );
                // The timestamp was captured between the test's `before`
                // and `after` -- if the driver returned a stale or future
                // timestamp the bound check fires.
                assert!(
                    paused_at >= before && paused_at <= after,
                    "paused_at {paused_at:?} must fall in [{before:?}, {after:?}]"
                );
            }
            GraphRunOutcome::Final { final_state, .. } => {
                panic!("human state must yield Paused, got Final({final_state})")
            }
        }
    }

    #[tokio::test]
    async fn human_state_does_not_write_a_step_record() {
        // Final states do not write a step record (the driver returns
        // before `step_num` increments). Pause must follow the same
        // pattern -- writing a synthetic record for a state that ran no
        // agent would muddy `kuro show-output` and lie about what
        // happened on disk.
        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "human-pause-test".to_string(),
            "test prompt".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let flow = graph_flow_with_human_initial();
        let agents_by_id: HashMap<String, crate::config::Agent> = HashMap::new();
        let state_to_agent: HashMap<String, String> = HashMap::new();

        run_graph_flow(&flow, &agents_by_id, &state_to_agent, &ctx, None)
            .await
            .expect("pause must succeed");

        // The driver always initialises `<run>/steps/`. Assert the
        // directory exists but holds no per-step files for the human
        // state. `read_dir` may legitimately be empty.
        let steps_dir = ctx.run_path.join(stack::STEPS_SUBDIR);
        assert!(
            steps_dir.is_dir(),
            "init_run_layout must create steps/ even when no step ran"
        );
        let entries: Vec<_> = std::fs::read_dir(&steps_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.is_empty(),
            "no step files must land on disk for a pause-on-initial run, got: {entries:?}"
        );
    }

    // --- Resume driver tests (issue #338) -------------------------------
    //
    // Pin the contract that `resume = Some(...)` inverts the entry-state
    // selection (start at `from_state`, not `graph.initial`) and that the
    // first iteration of a resumed human state advances through the first
    // declared `next:` entry instead of pausing again. The third test
    // locks in the explicit error path for an unresumable human state
    // (no `next:` entries) so a future refactor cannot silently fall
    // through to the normal pause arm and re-suspend the run.

    fn graph_flow_human_with_final_next() -> GraphFlow {
        // Two states: `ask` (human, advances to `done` on resume) and
        // `done` (final). Resuming from `ask` must walk to `done`.
        let mut graph: IndexMap<String, GraphState> = IndexMap::new();
        graph.insert(
            "ask".to_string(),
            GraphState {
                human: Some(true),
                select: Some(vec![SelectEntry {
                    target: "done".to_string(),
                    reason: Some(SelectReason::Single("after human input".to_string())),
                }]),
                ..Default::default()
            },
        );
        graph.insert(
            "done".to_string(),
            GraphState {
                final_desc: Some("run complete".to_string()),
                ..Default::default()
            },
        );
        let mut flow = GraphFlow::default();
        flow.name = "resume-test".to_string();
        flow.initial = "ask".to_string();
        flow.graph = graph;
        flow
    }

    #[tokio::test]
    async fn run_graph_flow_resume_from_human_state_takes_first_next() {
        // Acceptance (#338): resuming into a `human: true` state must
        // advance through the first declared `next:` entry instead of
        // pausing again. The first iteration of a resumed run never
        // pauses on a human state -- otherwise the run would deadlock
        // (resume re-enters the same state that paused).
        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "resume-test".to_string(),
            "test prompt".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let flow = graph_flow_human_with_final_next();
        let agents_by_id: HashMap<String, crate::config::Agent> = HashMap::new();
        let state_to_agent: HashMap<String, String> = HashMap::new();

        let outcome = run_graph_flow(
            &flow,
            &agents_by_id,
            &state_to_agent,
            &ctx,
            Some(ResumeFrom {
                state: "ask".to_string(),
                step_num_offset: 0,
            }),
        )
        .await
        .expect("resume from human state must succeed");

        match outcome {
            GraphRunOutcome::Final { steps, final_state } => {
                assert_eq!(
                    final_state, "done",
                    "resume from `ask` must walk to `done` via next[0]"
                );
                assert!(
                    steps.is_empty(),
                    "no agent ran (`ask` is human, `done` is final), so no step records"
                );
            }
            GraphRunOutcome::Paused {
                paused_at_state, ..
            } => panic!(
                "resume into a human state must NOT pause again; got Paused({paused_at_state})"
            ),
        }
    }

    #[tokio::test]
    async fn run_graph_flow_resume_from_human_state_no_next_errors() {
        // Acceptance (#338): a human state with no `next:` entries is
        // unresumable -- there is nowhere to go. The driver must fail
        // fast with a clear error rather than fall through to the normal
        // pause arm (which would silently re-suspend the run forever).
        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "resume-test".to_string(),
            "test prompt".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let flow = graph_flow_with_human_initial();
        let agents_by_id: HashMap<String, crate::config::Agent> = HashMap::new();
        let state_to_agent: HashMap<String, String> = HashMap::new();

        // `GraphRunOutcome` does not implement Debug, so `expect_err`
        // is unavailable; pattern-match the Result directly.
        let result = run_graph_flow(
            &flow,
            &agents_by_id,
            &state_to_agent,
            &ctx,
            Some(ResumeFrom {
                state: "ask".to_string(),
                step_num_offset: 0,
            }),
        )
        .await;

        match result {
            Err(RunError::GraphRuntime { state, reason }) => {
                assert_eq!(state, "ask");
                assert!(
                    reason.contains("no `next:` entries"),
                    "error must mention missing next: entries, got: {reason}"
                );
            }
            Err(other) => panic!("expected RunError::GraphRuntime, got: {other:?}"),
            Ok(_) => panic!("resume into an unresumable human state must Err, not Ok"),
        }
    }

    fn graph_flow_two_finals() -> GraphFlow {
        // `start` -> `done` (final). Used to verify that `resume_from`
        // overrides the declared `initial:` -- the driver enters at the
        // named state, not at `graph.initial`.
        let mut graph: IndexMap<String, GraphState> = IndexMap::new();
        graph.insert(
            "start".to_string(),
            GraphState {
                final_desc: Some("never reached when resuming from `done`".to_string()),
                ..Default::default()
            },
        );
        graph.insert(
            "done".to_string(),
            GraphState {
                final_desc: Some("resumed terminal".to_string()),
                ..Default::default()
            },
        );
        let mut flow = GraphFlow::default();
        flow.name = "resume-bypass-initial".to_string();
        flow.initial = "start".to_string();
        flow.graph = graph;
        flow
    }

    #[tokio::test]
    async fn run_graph_flow_resume_from_overrides_initial_state() {
        // Acceptance (#338): `resume = Some(...)` replaces `graph.initial`
        // as the entry point. The flow declares `initial: start` (which is
        // a final state in this fixture) but resuming from `done`
        // bypasses `start` entirely -- if the driver still consulted
        // `graph.initial`, the outcome's `final_state` would be `start`.
        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "resume-bypass-initial".to_string(),
            "test prompt".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let flow = graph_flow_two_finals();
        let agents_by_id: HashMap<String, crate::config::Agent> = HashMap::new();
        let state_to_agent: HashMap<String, String> = HashMap::new();

        let outcome = run_graph_flow(
            &flow,
            &agents_by_id,
            &state_to_agent,
            &ctx,
            Some(ResumeFrom {
                state: "done".to_string(),
                step_num_offset: 0,
            }),
        )
        .await
        .expect("resume into a final state must terminate cleanly");

        match outcome {
            GraphRunOutcome::Final { final_state, .. } => {
                assert_eq!(
                    final_state, "done",
                    "resume must enter at `done`, not at `graph.initial` (`start`)"
                );
            }
            GraphRunOutcome::Paused {
                paused_at_state, ..
            } => {
                panic!("resume into a final state must NOT pause; got Paused({paused_at_state})")
            }
        }
    }
}
