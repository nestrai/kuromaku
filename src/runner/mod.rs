use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

// Submodules.
//
// `decision` is the standalone parser+validator for the JSON object an agent
// emits at the end of a graph-flow state-step. Consumed by `graph::run_graph_flow`
// (issue #240).
pub mod decision;
// `graph` is the state-machine driver that walks a `Flow::Graph` from
// `initial:` to a `kind: final` state, asking the assigned agent to pick an
// outgoing edge per visit (issue #240).
pub mod graph;

use crate::config::{Agent, Backend, Step};
use crate::executor::{self, ExecutionTask, ExecutorBoxed, OutputFormat};
use crate::koto_config::Seeds;
use crate::llm::{self, LlmRequest, Message, Role};
use crate::notify::github::{self, PostOutcome};
use crate::skills;
use crate::stack::{self, StepRecord};
use crate::ui::{self, StepInfo, StepState};

/// Immutable context for a single flow run.
/// Constructed once in main::run_up(), passed to run_steps() and internal helpers.
pub struct RunContext {
    /// Run-ID, format `<flow>-<YYYYMMDD-HHmmss>` (issue #31).
    /// Sortable, human-readable, embeds the flow name so multiple flow types
    /// can share a project's `~/.koto/stacks/<project>/` directory.
    pub run_id: String,
    pub flow_name: String,
    pub task: String,
    /// Project-level stack directory (`~/.koto/stacks/<project>/`). Kept for
    /// backward compat -- legacy flat-file callers and tests still reference
    /// it. New runs write into [`RunContext::run_path`].
    pub stack_path: PathBuf,
    /// Per-run directory: `<stack_path>/<run_id>/`. Every artifact for this
    /// run -- step content, step metadata, manifest, resolution audit -- lives
    /// here. Created on construction so callers can write to it without
    /// further mkdir bookkeeping.
    pub run_path: PathBuf,
    /// UTC start timestamp for the run, captured at construction so the
    /// manifest's `started_at` matches the run-id timestamp segment exactly.
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub guide: Option<String>,
    pub rules_cache: HashMap<String, String>,
    pub skills_cache: HashMap<String, String>,
    /// Effective template vars (`vars:` from the project config merged with
    /// CLI `--var` and bare `key=value` args). The runner reads `id` from
    /// this map when a step declares `post_comment:` to determine which PR
    /// or issue to post on.
    pub template_vars: HashMap<String, String>,
    /// Sink used to post step output as a GitHub comment when a step declares
    /// `post_comment:`. Defaults to the `gh` CLI poster; tests substitute a
    /// closure to exercise the soft-fail behavior without touching gh.
    pub poster: github::Poster,
}

impl RunContext {
    pub fn new(
        flow_name: String,
        task: String,
        stack_path: PathBuf,
        guide: Option<String>,
        rules_cache: HashMap<String, String>,
        skills_cache: HashMap<String, String>,
        template_vars: HashMap<String, String>,
    ) -> Self {
        // Single source of truth for both run_id and started_at. The local
        // timezone is used for the human-readable id so it lines up with the
        // user's wall clock; UTC is recorded separately for the manifest.
        let started_at_utc = chrono::Utc::now();
        let local = started_at_utc.with_timezone(&chrono::Local);
        let ts = local.format("%Y%m%d-%H%M%S").to_string();
        // Two `kuro run` calls in the same wall-clock second would otherwise
        // share a run_id and clobber each other's outputs. Bump a numeric
        // suffix until we find a free directory so the timestamp stays
        // human-readable in the common case and only collisions get `-2`,
        // `-3`, ... appended.
        let (run_id, run_path) = unique_run_path(&stack_path, &format!("{flow_name}-{ts}"));

        Self {
            run_id,
            flow_name,
            task,
            stack_path,
            run_path,
            started_at: started_at_utc,
            guide,
            rules_cache,
            skills_cache,
            template_vars,
            poster: github::gh_poster(),
        }
    }
}

/// Pick a run directory that does not already exist, appending `-2`, `-3`, ...
/// when the timestamp-based base name collides. The base name format
/// (`<flow>-YYYYMMDD-HHmmss`) only resolves to seconds, so two runs started in
/// the same wall-clock second would otherwise share a directory and overwrite
/// each other -- which defeats the per-run audit promise behind issue #31.
///
/// There is a TOCTOU window between the `exists()` check and the directory
/// being created later in the run, but `kuro run` invocations are user-driven
/// (not a service loop), so the race is bounded by how fast a human can press
/// Enter twice. The overwrite bug, by contrast, hits any back-to-back run.
/// Print the issue context banner if the run was launched with `--var id=<n>`
/// and `gh` returns a usable summary (issue #309).
///
/// All silent-skip conditions (missing var, non-numeric value, `gh` not on
/// PATH or returning non-zero) collapse to "no banner, no warning" so flows
/// that don't follow the issue convention stay quiet. Returns `()` regardless;
/// the banner is opportunistic and never aborts a run.
fn try_print_issue_banner(template_vars: &HashMap<String, String>) {
    let Some(raw) = template_vars.get("id").map(String::as_str) else {
        return;
    };
    let Ok(id) = raw.parse::<u64>() else {
        return;
    };
    let Some(summary) = github::fetch_issue_summary(id) else {
        return;
    };
    ui::print_issue_banner(&summary);
}

fn unique_run_path(stack_path: &Path, base: &str) -> (String, PathBuf) {
    let direct = stack_path.join(base);
    if !direct.exists() {
        return (base.to_string(), direct);
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        let path = stack_path.join(&candidate);
        if !path.exists() {
            return (candidate, path);
        }
        n += 1;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("step '{step}' references unknown agent '{agent}'")]
    UnknownAgent { step: String, agent: String },

    #[error("step '{step}' failed: {source}")]
    LlmFailed { step: String, source: llm::LlmError },

    #[error("step '{step}' execution failed: {source}")]
    ExecutorFailed {
        step: String,
        source: executor::ExecutorError,
    },

    #[error("stack error in step '{step}': {source}")]
    Stack {
        step: String,
        source: stack::StackError,
    },

    #[error("rules file not found: {0}")]
    RulesNotFound(String),

    #[error("skill error: {0}")]
    Skill(#[from] skills::SkillsError),

    /// A conversation step (#170) referenced a backend other than
    /// `claude-cli`. Other backends do not yet implement the [`Transport`]
    /// trait, so the Router cannot drive them. Surfaced explicitly rather
    /// than silently falling back to the executor path because the user's
    /// agent file is what would need to change.
    #[error(
        "step '{step}' is a conversation but agent '{agent}' uses backend '{backend:?}' -- only claude-cli is supported for conversation steps"
    )]
    ConversationUnsupportedBackend {
        step: String,
        agent: String,
        backend: Backend,
    },

    /// Spawning a participant transport failed (process did not start, pipes
    /// could not be captured, etc).
    #[error("step '{step}' failed to spawn agent '{agent}': {source}")]
    ConversationSpawn {
        step: String,
        agent: String,
        source: executor::transport::TransportError,
    },

    /// State-graph runtime error (issue #240). Surfaced by the graph driver
    /// for malformed/unknown-edge double-failures, max_steps overflow, and
    /// unsupported state kinds in the prototype runtime. The `state` field
    /// is the offending state ID so the user can locate it in the YAML.
    #[error("graph runtime error at state '{state}': {reason}")]
    GraphRuntime { state: String, reason: String },
}

/// Result of running a single step, used for the summary table and the run
/// manifest (issue #31).
///
/// `backend` is a string rather than the [`Backend`] enum so shell steps
/// (issue #23) can report `"shell"` without polluting the LLM-backend enum.
#[derive(Debug)]
pub struct StepRunResult {
    pub step_id: String,
    pub agent_name: String,
    pub backend: String,
    pub duration: std::time::Duration,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    /// Path of the content file relative to [`RunContext::stack_path`]
    /// (e.g. `dev-20260429-100000/01-design.md`). The summary table prints
    /// this and `main` joins it with `stack_path` to render output via
    /// termimad when `print_output: true`.
    pub output_file: String,
    pub print_output: bool,
    /// Per-step record assembled while the step ran. Used by `main` to build
    /// the run manifest. Cloned from the same data written to
    /// `<step_num>-<step_id>.meta.yaml` so the manifest and the per-step file
    /// are guaranteed to match.
    pub record: StepRecord,
}

/// Output filename for shell steps inside a run directory: `NN-<step_id>.txt`.
///
/// Uses `.txt` rather than `.md` because shell stdout isn't markdown -- a
/// downstream `print_output: true` would render terminal escapes through
/// termimad otherwise. The numbering matches the topo order of the run.
fn shell_output_filename(step_num: usize, step_id: &str) -> String {
    stack::step_content_filename(step_num, step_id, "txt")
}

/// Output filename for LLM steps inside a run directory: `NN-<step_id>.md`.
pub(crate) fn llm_output_filename(step_num: usize, step_id: &str) -> String {
    stack::step_content_filename(step_num, step_id, "md")
}

/// Load `Guide.md` from a single project config directory. Test-only
/// single-dir variant; the production loader is [`load_guide_from_seeds`].
#[cfg(test)]
pub fn load_guide(koto_dir: &Path) -> Option<String> {
    let guide_path = koto_dir.join("Guide.md");
    std::fs::read_to_string(&guide_path).ok()
}

/// Load `Guide.md` from the first seed that has it. Returns `None` when no
/// seed contains a Guide -- callers treat the guide as optional context.
///
/// Errors only when the seed walk hits a remote seed (issue #130 v1 limit).
pub fn load_guide_from_seeds(seeds: &Seeds) -> Result<Option<String>, RunError> {
    let rel = std::path::Path::new("Guide.md");
    match seeds.find(rel) {
        Ok(Some((_, path))) => Ok(std::fs::read_to_string(&path).ok()),
        Ok(None) => Ok(None),
        Err(e) => Err(RunError::RulesNotFound(e.message())),
    }
}

/// Gate on whether to load `Guide.md` for repo-agnostic commands (`kuro task`,
/// `kuro chat`). Returns `None` unless `include_project_context` is true so
/// that, by default, an agent run via `kuro task` does not inherit the cwd
/// project's identity (issue #245). `kuro run` keeps its own unconditional
/// guide load -- flow runs ARE repo-specific by design.
pub fn load_guide_for_task(
    seeds: &Seeds,
    include_project_context: bool,
) -> Result<Option<String>, RunError> {
    if include_project_context {
        load_guide_from_seeds(seeds)
    } else {
        Ok(None)
    }
}

/// Pre-load rules files for all agents that reference them.
/// Test-only single-dir variant; the production loader is
/// [`load_rules_for_agents_with_seeds`].
#[cfg(test)]
pub fn load_rules_for_agents(
    agents: &[Agent],
    koto_dir: &Path,
) -> Result<HashMap<String, String>, RunError> {
    let seeds = Seeds {
        seeds: vec![crate::koto_config::Seed {
            source: crate::koto_config::SeedSource::Local {
                display: koto_dir.display().to_string(),
                path: koto_dir.to_path_buf(),
            },
        }],
    };
    load_rules_for_agents_with_seeds(agents, &seeds)
}

/// Pre-load rules files for all agents, resolving each rule through the seed
/// list. First match wins, so a project-level seed can override a rule shipped
/// with a downstream seed.
///
/// Errors when a referenced rule isn't found in any seed -- the message lists
/// every seed that was searched so the user can fix the typo or add a seed.
pub fn load_rules_for_agents_with_seeds(
    agents: &[Agent],
    seeds: &Seeds,
) -> Result<HashMap<String, String>, RunError> {
    let mut cache: HashMap<String, String> = HashMap::new();

    for agent in agents {
        for rules_name in &agent.rules {
            if cache.contains_key(rules_name) {
                continue;
            }
            let rel = std::path::Path::new("rules").join(format!("{rules_name}.md"));
            let (_idx, rules_path) = seeds
                .find(&rel)
                .map_err(|e| RunError::RulesNotFound(e.message()))?
                .ok_or_else(|| {
                    RunError::RulesNotFound(seeds.not_found_message("rules", rules_name))
                })?;
            let content = std::fs::read_to_string(&rules_path).map_err(|_| {
                RunError::RulesNotFound(format!(
                    "rules file '{}' could not be read (path: {})",
                    rules_name,
                    rules_path.display()
                ))
            })?;
            cache.insert(rules_name.clone(), content);
        }
    }

    Ok(cache)
}

/// Build the full system prompt: Guide > Rules > Skills > Role.
pub(crate) fn build_system_prompt(
    agent: &Agent,
    guide: &Option<String>,
    rules_cache: &HashMap<String, String>,
    skills_cache: &HashMap<String, String>,
) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if let Some(guide_content) = guide {
        parts.push(guide_content);
    }

    // Append all rules in order
    for rules_name in &agent.rules {
        if let Some(content) = rules_cache.get(rules_name) {
            parts.push(content);
        }
    }

    // Append skills content in order
    for skill_name in &agent.skills {
        if let Some(content) = skills_cache.get(skill_name) {
            parts.push(content);
        }
    }

    parts.push(&agent.role);
    parts.join("\n\n")
}

/// Build the user-facing prompt with context from prior steps.
///
/// Resolve the effective `extra_args` slice for a step (#236).
///
/// Cascade is replace-not-merge: a non-empty step-level map fully shadows
/// the agent map, even when it has no entry for the effective backend (in
/// which case no override tokens are emitted). This matches the issue's
/// "step replaces agent" semantics and keeps the lookup a single
/// `HashMap::get` per layer.
///
/// Lifted into a free function so the conversation-step path (which has no
/// step-level extra_args by construction -- the conversation validator
/// rejects them) and the agent-step path share identical resolution logic
/// without copy-pasting the cascade.
fn resolve_extra_args<'a>(
    step: &'a Step,
    agent: &'a Agent,
    effective_backend: Backend,
) -> &'a [String] {
    let map = if !step.extra_args.is_empty() {
        &step.extra_args
    } else {
        &agent.extra_args
    };
    map.get(&effective_backend)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// Reads the prior step content from the per-run directory (issue #31). The
/// runner writes both an LLM `.md` and a shell `.txt` body keyed by step id;
/// this lookup doesn't care about the kind because it just splices the body
/// text into the next agent's prompt.
fn build_user_prompt(task: &str, step: &Step, run_path: &Path) -> Result<String, RunError> {
    let mut context_parts: Vec<String> = Vec::new();
    for input_id in &step.input {
        let body =
            stack::read_run_step_content(run_path, input_id).map_err(|e| RunError::Stack {
                step: step.id.clone(),
                source: e,
            })?;
        ui::print_context_injection(input_id, input_id, "");
        context_parts.push(format!(
            "--- Output from step '{input_id}' ---\n{body}\n---"
        ));
    }

    let mut user_content = task.to_string();

    // If step has its own task, append it
    if let Some(ref step_task) = step.task {
        user_content = format!("{user_content}\n\nYour task: {step_task}");
    }

    if !context_parts.is_empty() {
        user_content = format!(
            "{user_content}\n\nContext from previous steps:\n\n{}\n\nIMPORTANT: The above is work already completed by other team members. Build on their output -- do not repeat or rephrase what they already covered. Add your own perspective, analysis, or implementation.",
            context_parts.join("\n\n")
        );
    }
    Ok(user_content)
}

/// Run a step via the Executor (CLI backends: claude-cli, ollama).
///
/// `output_path`, when set, is passed to the executor so stdout streams to
/// that file line-by-line during execution -- the file fills up live and a
/// concurrent `tail -f` sees output without waiting for the agent to finish
/// (issue #16).
///
/// `extra_args` is the resolved per-step backend-keyed override slice
/// (#236). The runner already cascades step → agent → empty before calling
/// here, so the slice is always the right one for the backend in use.
#[allow(clippy::too_many_arguments)]
async fn run_step_via_executor(
    executor: &dyn ExecutorBoxed,
    step: &Step,
    flow_name: &str,
    system_prompt: &str,
    user_content: &str,
    model: &str,
    backend: Backend,
    extra_args: &[String],
    output_path: &Path,
) -> Result<(String, Option<llm::Usage>), RunError> {
    // Build unique session name: kuro-<project>-<flow>-<step>-<short-id>
    let project = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let short_id = &chrono::Utc::now().timestamp_millis().to_string()[8..];
    let task_id = format!("kuro-{project}-{flow_name}-{}-{short_id}", step.id);

    let command = match backend {
        Backend::ClaudeCli => {
            executor::build_claude_command(model, Some(system_prompt), user_content, extra_args)
        }
        Backend::Codex => {
            executor::build_codex_command(model, Some(system_prompt), user_content, extra_args)
        }
        Backend::Ollama => {
            let mut prompt = String::new();
            prompt.push_str(&format!("System: {system_prompt}\n\n"));
            prompt.push_str(&format!("User: {user_content}"));
            executor::build_ollama_command(model, &prompt, extra_args)
        }
        Backend::Api => unreachable!("API backend does not use executor"),
    };

    // Claude CLI emits structured NDJSON (issue #156); other backends speak
    // plain text. The executor parses stream-json back into readable text in
    // the artifact file and uses the `result` event for the canonical step
    // output.
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
            step: step.id.clone(),
            source: e,
        })?;

    let output = executor
        .wait_boxed(&handle)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            step: step.id.clone(),
            source: e,
        })?;

    Ok((output.stdout, None))
}

/// Run a step via the API client directly (no executor needed).
async fn run_step_via_api(
    request: LlmRequest,
    step_id: &str,
) -> Result<(String, Option<llm::Usage>), RunError> {
    let client = llm::ApiClient::from_env();
    let response = client
        .send(request)
        .await
        .map_err(|e| RunError::LlmFailed {
            step: step_id.to_string(),
            source: e,
        })?;
    Ok((response.content, response.usage))
}

/// Execute a shell step (`run:` instead of `agent:`).
///
/// Spawns the rendered command via `sh -c` through the local executor:
/// stdout becomes the step output (saved to the stack and to a `.txt`
/// artifact), stderr is surfaced to the user via stderr regardless of exit
/// code, and a non-zero exit aborts the flow with a [`RunError::ExecutorFailed`]
/// that includes both the exit code and stderr (acceptance criteria, issue #23).
///
/// `prior_results` is read-only -- shell steps care about it only when
/// `post_comment:` is set, to label the comment header with input-step agent
/// names like the LLM-step path does.
async fn run_shell_step(
    executor: &dyn ExecutorBoxed,
    step: &Step,
    ctx: &RunContext,
    step_num: usize,
    total: usize,
    prior_results: &[StepRunResult],
) -> Result<StepRunResult, RunError> {
    let command = step
        .run
        .as_deref()
        .expect("run_shell_step called on non-shell step");

    ui::print_shell_step_banner(step_num, total, &step.id, command, &step.input);

    // Pre-compute and announce the output path so users can `tail -f` it
    // even if the command takes a while. Layout from issue #31:
    // `<stack>/<run-id>/steps/NN-<step>.txt` -- numbered by execution order.
    let content_filename = shell_output_filename(step_num, &step.id);
    let output_path = ctx
        .run_path
        .join(stack::STEPS_SUBDIR)
        .join(&content_filename);
    // The summary table and `print_output:true` resolve via stack_path, so
    // the relative output_file embeds both the run id and the steps segment.
    let output_file = format!(
        "{}/{}/{}",
        ctx.run_id,
        stack::STEPS_SUBDIR,
        content_filename
    );
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    eprintln!(
        "      output: {}",
        output_path
            .canonicalize()
            .unwrap_or(output_path.clone())
            .display()
    );

    let start = Instant::now();
    // Capture the step's wall-clock start so the manifest reflects when this
    // step actually began -- not when the run started. Without this, every
    // step shares `ctx.started_at` and the audit trail collapses.
    let step_started_at = chrono::Utc::now();
    let spinner = ui::start_spinner();

    // Build a unique task ID, mirroring run_step_via_executor so log/process
    // listings stay consistent across step types.
    let project = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let short_id = &chrono::Utc::now().timestamp_millis().to_string()[8..];
    let task_id = format!(
        "kuro-{project}-{}-{}-{short_id}-shell",
        ctx.flow_name, step.id
    );

    let task = ExecutionTask {
        id: task_id,
        command: command.to_string(),
        env: HashMap::new(),
        // Streamed: shell stdout fills the artifact file live so the user
        // can `tail -f` long-running commands (issue #16).
        stdout_file: Some(output_path.to_path_buf()),
        // Shell `run:` steps emit raw text, not NDJSON.
        output_format: OutputFormat::Raw,
    };

    let handle = executor
        .spawn_boxed(task)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            step: step.id.clone(),
            source: e,
        })?;

    let exec_output = executor
        .wait_boxed(&handle)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            step: step.id.clone(),
            source: e,
        })?;

    spinner.stop();
    let duration = start.elapsed();

    // Surface stderr to the user even on success. Acceptance criterion:
    // "stderr is shown to the user but not captured as output". On failure
    // the executor already includes stderr in the error message.
    if !exec_output.stderr.is_empty() {
        eprintln!("{}", exec_output.stderr.trim_end());
    }

    let stdout = exec_output.stdout;

    // Build the per-step record, then write it alongside the content file.
    // The content was streamed during execution (issue #16); we still
    // overwrite to make sure the on-disk body matches the stdout the manifest
    // refers to (the streamed file may carry a trailing newline that the
    // collected stdout doesn't, and vice versa).
    let started_at = step_started_at.to_rfc3339();
    let record = StepRecord {
        step_id: step.id.clone(),
        kind: "shell".to_string(),
        agent: None,
        model_requested: None,
        model_actual: None,
        backend: "shell".to_string(),
        tokens_in: None,
        tokens_out: None,
        duration_ms: duration.as_millis(),
        started_at,
        exit_code: 0,
        input_steps: step.input.clone(),
        output_file: content_filename.clone(),
        participants: Vec::new(),
        turns: None,
        messages: None,
        terminated_by: None,
        graph_decision: None,
    };
    stack::write_run_step(&ctx.run_path, step_num, &record, &stdout).map_err(|e| {
        RunError::Stack {
            step: step.id.clone(),
            source: e,
        }
    })?;

    let display_path = output_path
        .canonicalize()
        .unwrap_or(output_path.clone())
        .display()
        .to_string();
    // Tokens are intentionally "—" for shell steps -- acceptance criterion
    // "report zero tokens and no model in the summary table". Reusing
    // print_step_done keeps the visual cadence consistent with LLM steps.
    ui::print_step_done(&format_duration(duration), "—", "—", &display_path);

    // post_comment is allowed on shell steps too -- e.g. post a `gh pr diff`
    // result back as a comment. Soft-fails like the LLM path.
    if let Some(target) = step.post_comment {
        let input_agents: Vec<&str> = step
            .input
            .iter()
            .filter_map(|input_id| {
                prior_results
                    .iter()
                    .find(|r| r.step_id == *input_id)
                    .map(|r| r.agent_name.as_str())
            })
            .collect();
        let outcome = github::try_post_step_comment(
            target,
            "shell",
            &stdout,
            &input_agents,
            &ctx.template_vars,
            &ctx.poster,
        );
        match outcome {
            PostOutcome::Posted { kind, number } => {
                eprintln!("      posted comment on {kind} #{number}");
            }
            PostOutcome::NoIdProvided => {
                eprintln!(
                    "warning: step '{}' declares post_comment but no 'id' template var was provided",
                    step.id
                );
            }
            PostOutcome::Failed { error } => {
                eprintln!(
                    "warning: step '{}' failed to post comment: {error}",
                    step.id
                );
            }
        }
    }

    Ok(StepRunResult {
        step_id: step.id.clone(),
        agent_name: "shell".to_string(),
        backend: "shell".to_string(),
        duration,
        tokens_in: None,
        tokens_out: None,
        output_file,
        print_output: step.print_output,
        record,
    })
}

/// Spawn a background task that reads lines from `reader` and forwards them
/// on the returned mpsc receiver. Empty / whitespace-only lines are skipped
/// so a stray Enter does not inject a no-op message into the conversation.
/// EOF on the reader drops the sender, which makes the router terminate
/// with [`TerminationReason::HumanClosed`].
///
/// Generic over `R: AsyncBufRead` so tests can drive it from a `Cursor`
/// without spawning a TTY. Production callers use [`spawn_stdin_human_reader`]
/// which wires this up to `tokio::io::stdin()` after a TTY check.
///
/// Issue #171 (stdin fallback): the conversation step attaches this reader
/// to [`Router::set_human_input`] when running interactively.
///
/// Currently only the unit tests exercise this directly; production code
/// goes through [`spawn_stdin_to_accessor`], which wraps the same channel
/// model with the [`flow_api::RouterAccessor`] so MCP injections share
/// the path. Kept as a focused helper (and tested) because it is the
/// minimal building block for the production flow.
#[allow(dead_code)]
fn spawn_line_reader<R>(reader: R) -> tokio::sync::mpsc::Receiver<String>
where
    R: tokio::io::AsyncBufRead + Send + Unpin + 'static,
{
    use tokio::io::AsyncBufReadExt;

    // Bounded channel: 8 is plenty for a human typing speed, and prevents
    // a runaway producer (shouldn't happen with stdin, but bounded is the
    // safe default).
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(8);
    tokio::spawn(async move {
        let mut lines = reader.lines();
        // EOF (Ctrl-D) or a read error ends the human session: the loop
        // exits, `tx` drops, and the channel close signals the router to
        // stop waiting for input.
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if tx.send(trimmed.to_string()).await.is_err() {
                // Router dropped the receiver (run terminated).
                break;
            }
        }
    });
    rx
}

/// Forward stdin lines into a [`flow_api::RouterAccessor`]. Counterpart to
/// [`spawn_line_reader`] that funnels into the same human-input channel as
/// the MCP `send_message` tool, so a conversation can take both at once.
///
/// The task exits on stdin EOF (Ctrl-D), or as soon as the accessor's
/// channel rejects a send (router has terminated). When this is the last
/// alive sender, the router observes `HumanClosed` -- preserving the legacy
/// "Ctrl-D ends the conversation" behavior from #171.
fn spawn_stdin_to_accessor(accessor: flow_api::RouterAccessor) {
    use tokio::io::AsyncBufReadExt;

    tokio::spawn(async move {
        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if accessor
                .inject_human_message(trimmed.to_string())
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

/// Run a `type: conversation` step (issue #170).
///
/// Spawns one [`StreamJsonTransport`](executor::transport::StreamJsonTransport)
/// per participant, hands them to a [`Router`](crate::messaging::router::Router),
/// and drives the conversation with the assembled task as the initial
/// broadcast prompt. The router's log entries are collected synchronously
/// through an `Arc<Mutex<...>>`; once the router terminates we render them
/// to a markdown transcript via [`render_transcript`] and persist that as the
/// step's output file.
///
/// Limitations:
///
/// * Only the `claude-cli` backend is supported. The other backends (`api`,
///   `codex`, `ollama`) do not have a [`Transport`](executor::transport::Transport)
///   implementation. We refuse to start rather than silently falling back.
/// * The step-level `model:` / `backend:` fields are rejected by the parser
///   so the per-agent settings are the single source of truth here.
async fn run_conversation_step(
    step: &Step,
    agent_map: &HashMap<&str, &Agent>,
    ctx: &RunContext,
    step_num: usize,
    total: usize,
    run_state: Option<std::sync::Arc<flow_api::RunState>>,
) -> Result<StepRunResult, RunError> {
    use std::io::IsTerminal;
    use std::sync::{Arc, Mutex};

    use crate::messaging::audit::{MessageLogWriter, message_log_path};
    use crate::messaging::router::{LogEntry, Router, RouterConfig};

    // Resolve all participant agents up front -- a missing agent must fail
    // before we spawn any transports, otherwise the user gets a partially
    // started conversation that is hard to diagnose.
    let participants: Vec<&Agent> = step
        .agents
        .iter()
        .map(|id| {
            agent_map
                .get(id.as_str())
                .copied()
                .ok_or_else(|| RunError::UnknownAgent {
                    step: step.id.clone(),
                    agent: id.clone(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Backend gate: only claude-cli has a Transport. We check before
    // spawning so we never end up with half a conversation alive.
    for agent in &participants {
        if agent.backend != Backend::ClaudeCli {
            return Err(RunError::ConversationUnsupportedBackend {
                step: step.id.clone(),
                agent: agent.id.clone(),
                backend: agent.backend,
            });
        }
    }

    let participant_names: Vec<String> = participants.iter().map(|a| a.id.clone()).collect();
    ui::print_conversation_step_banner(step_num, total, &step.id, &participant_names, &step.input);

    // Pre-compute the output path the same way agent steps do, so users can
    // `tail -f` the transcript while the conversation runs. We don't stream
    // partial rebuilds, but the path is announced for consistency.
    let content_filename = llm_output_filename(step_num, &step.id);
    let output_path = ctx
        .run_path
        .join(stack::STEPS_SUBDIR)
        .join(&content_filename);
    let output_file = format!(
        "{}/{}/{}",
        ctx.run_id,
        stack::STEPS_SUBDIR,
        content_filename
    );
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    eprintln!(
        "      output: {}",
        output_path
            .canonicalize()
            .unwrap_or(output_path.clone())
            .display()
    );

    // Build the seed prompt. The flow's task plus any `input:` outputs are
    // funneled into the Router as the initial broadcast -- exactly the same
    // semantics as agent steps, minus the per-step `task:` flavor. The
    // conversation's `task:` (if any) is appended via build_user_prompt.
    let seed_prompt = build_user_prompt(&ctx.task, step, &ctx.run_path)?;

    // Configure the router. A missing turn_timeout falls back to the
    // RouterConfig default (600s, see #169).
    let mut router_cfg = RouterConfig::default();
    if let Some(n) = step.max_turns {
        router_cfg.max_turns = n;
    }
    if let Some(secs) = step.turn_timeout {
        router_cfg.turn_timeout = std::time::Duration::from_secs(secs);
    }

    // Open the NDJSON audit log alongside the run before the conversation
    // starts (#172). Writing happens inside the logger callback so a
    // `tail -f messages/<step>.ndjson` reader sees lines appear in real
    // time. Failures here are surfaced eagerly: we will not start a
    // conversation we cannot audit.
    let messages_path = message_log_path(&ctx.run_path, &step.id);
    let writer =
        Arc::new(
            MessageLogWriter::create(&messages_path).map_err(|e| RunError::Stack {
                step: step.id.clone(),
                source: stack::StackError::Write(e),
            })?,
        );
    eprintln!(
        "      messages: {}",
        messages_path
            .canonicalize()
            .unwrap_or(messages_path.clone())
            .display()
    );

    // The logger has two jobs now:
    // 1. Stream every relevant entry to the NDJSON audit file (#172).
    // 2. Buffer every entry in memory so the post-run transcript renderer
    //    and per-agent turn counter still see the full sequence.
    //
    // Arc<Mutex<Vec<LogEntry>>> rather than mpsc because we only need the
    // entries once the router has terminated. The audit writer carries
    // its own internal locking, so the closure does not need to hold any
    // additional state.
    let log: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let log_clone = Arc::clone(&log);
    let writer_for_logger = Arc::clone(&writer);
    let logger = Arc::new(move |entry: LogEntry| {
        // Audit-write first so a panic in the buffering branch (e.g. mutex
        // poisoning) cannot drop messages from the persistent log. An I/O
        // failure on the audit file is logged and the conversation
        // continues -- aborting mid-conversation would lose more
        // information than a partial audit trail.
        if let Err(e) = writer_for_logger.record(&entry) {
            eprintln!("warning: failed to append audit message: {e}");
        }
        if let Ok(mut v) = log_clone.lock() {
            v.push(entry);
        }
    });

    let mut router = Router::new(router_cfg, logger);

    // Attach human-input sources to the router. Two senders feed the same
    // mpsc channel (`Router::set_human_input` only stores one receiver):
    //
    // 1. The MCP `send_message` tool (#199) -- only when the conversation
    //    runs under a `RunHandle` whose `state` slot we can publish to.
    // 2. Stdin lines, but only when stdin is connected to a terminal.
    //    Piped/redirected stdin is left alone so scripted runs (CI,
    //    `kuro run < file`) do not have pipeline data interpreted as
    //    human messages. On Ctrl-D the forwarder task exits, dropping its
    //    sender clone; if it was the last alive sender, the router stops
    //    with `HumanClosed` -- preserving the legacy interactive behavior.
    //
    // The accessor is created only when at least one source is wired up.
    // Without any source, the router gets no human channel at all, so the
    // human-input arm of the select stays pending forever (current
    // behaviour for non-interactive runs).
    let stdin_is_tty = std::io::stdin().is_terminal();
    let need_human_channel = run_state.is_some() || stdin_is_tty;
    if need_human_channel {
        let (accessor, rx) = flow_api::RouterAccessor::new();
        router.set_human_input(rx);
        if let Some(state) = &run_state {
            // Publishing under the state slot is what `RunHandle::router`
            // and `ActiveRouter::current` read; the MCP server snapshots
            // through `ActiveRouter` to find the right conversation when
            // dispatching `send_message`.
            state.set_router(accessor.clone());
        }
        if stdin_is_tty {
            eprintln!("      human input: type a message and press Enter to inject; Ctrl-D to end");
            spawn_stdin_to_accessor(accessor.clone());
        }
        // Drop the local accessor: keeping it alive here would prevent the
        // router from observing `HumanClosed` even after the last real
        // source (stdin / state) drops its clone. The clones live in the
        // forwarder task and the run state; both have well-defined drop
        // points.
        drop(accessor);
    }

    // Spawn each agent's transport and hand it to the router. If any spawn
    // fails we abort -- partial setups would leak processes.
    for agent in &participants {
        let system_prompt =
            build_system_prompt(agent, &ctx.guide, &ctx.rules_cache, &ctx.skills_cache);
        // Conversation steps always run via claude-cli interactive transport,
        // so we resolve extra_args from the agent's claude-cli bucket.
        let extra_args: &[String] = agent
            .extra_args
            .get(&Backend::ClaudeCli)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let cmd = executor::build_claude_interactive_command(
            &agent.model,
            Some(&system_prompt),
            extra_args,
        );
        let transport = executor::transport::StreamJsonTransport::spawn(cmd)
            .await
            .map_err(|e| RunError::ConversationSpawn {
                step: step.id.clone(),
                agent: agent.id.clone(),
                source: e,
            })?;
        router.add_agent(agent.id.clone(), transport);
    }

    let start = Instant::now();
    let step_started_at = chrono::Utc::now();
    let spinner = ui::start_spinner();
    let termination = router.run(Some(&seed_prompt)).await;
    spinner.stop();
    // The conversation has stopped; drop the published accessor so a
    // concurrent observer (e.g. MCP `send_message`) reading the run state
    // sees "no live conversation" instead of holding a stale, closed
    // RouterAccessor. Doing this before any other post-run work means a
    // racing `send_message` cannot land on a router that has already
    // returned from `run`.
    if let Some(state) = &run_state {
        state.clear_router();
    }
    let duration = start.elapsed();

    // Drain the log under the mutex; the router task has terminated so no
    // more writers exist.
    let entries = log.lock().map(|v| v.clone()).unwrap_or_default();
    let transcript = render_transcript(&entries, &termination, &participant_names);

    // Per-agent breakdown (#170 acceptance criterion). One row per
    // participant in the order they were declared on the step. Tokens stay
    // `None` until the transport surfaces per-message counts to the router.
    let participants_stats: Vec<stack::ParticipantStat> = participants
        .iter()
        .map(|agent| stack::ParticipantStat {
            agent: agent.id.clone(),
            model: agent.model.clone(),
            turns: count_agent_turns(&entries, &agent.id),
            tokens_in: None,
            tokens_out: None,
        })
        .collect();

    // Conversation summary fields for the manifest (#172). `turns` is the
    // sum of agent finals, matching the per-agent rows so the audit
    // numbers reconcile. `messages` comes from the writer's count -- a
    // failed flush would not inflate it. `terminated_by` uses the
    // [`TerminationReason`] Display impl so the on-disk string stays
    // frozen against future variant renames.
    let total_turns: u32 = participants_stats.iter().map(|p| p.turns as u32).sum();
    let total_messages: u32 = writer.message_count();
    let terminated_by = termination.to_string();

    let started_at = step_started_at.to_rfc3339();
    let record = StepRecord {
        step_id: step.id.clone(),
        kind: "conversation".to_string(),
        // No single agent for a conversation; leave None so the audit
        // schema reflects "many agents" via the participants list below.
        agent: None,
        model_requested: None,
        model_actual: None,
        // Step-type discrimination lives in `kind: "conversation"`. The
        // backend label sticks to the documented vocabulary
        // (api/claude-cli/codex/ollama/shell), so audit consumers don't
        // see a value that's missing from the schema. Conversation steps
        // are enforced to run on claude-cli upstream, so this is also
        // factually accurate.
        backend: "claude-cli".to_string(),
        tokens_in: None,
        tokens_out: None,
        duration_ms: duration.as_millis(),
        started_at,
        exit_code: 0,
        input_steps: step.input.clone(),
        output_file: content_filename.clone(),
        participants: participants_stats,
        turns: Some(total_turns),
        messages: Some(total_messages),
        terminated_by: Some(terminated_by),
        graph_decision: None,
    };
    stack::write_run_step(&ctx.run_path, step_num, &record, &transcript).map_err(|e| {
        RunError::Stack {
            step: step.id.clone(),
            source: e,
        }
    })?;

    let display_path = output_path
        .canonicalize()
        .unwrap_or(output_path.clone())
        .display()
        .to_string();
    ui::print_step_done(&format_duration(duration), "—", "—", &display_path);

    Ok(StepRunResult {
        step_id: step.id.clone(),
        agent_name: participant_names.join(", "),
        backend: "conversation".to_string(),
        duration,
        tokens_in: None,
        tokens_out: None,
        output_file,
        print_output: step.print_output,
        record,
    })
}

/// Render a router log to a markdown transcript.
///
/// Pure function over the log entries so it is unit-testable without
/// spawning processes. The transcript layout is intentionally simple:
///
/// * One `## <agent>` header per final inbound message.
/// * Body text verbatim under the header.
/// * Tool-use entries shown as a single italic line so audit consumers see
///   the activity but transcripts stay readable.
/// * Send failures appear inline as a fenced warning block.
/// * The trailing line records the termination reason and participant set.
///
/// Partial fragments are deliberately skipped -- they are streaming deltas
/// of text the same agent later emits as a final result, and including them
/// would duplicate the body.
fn render_transcript(
    entries: &[crate::messaging::router::LogEntry],
    termination: &crate::messaging::router::TerminationReason,
    participants: &[String],
) -> String {
    use crate::messaging::router::{LogKind, MessageKind};

    let mut out = String::new();
    out.push_str("# Conversation transcript\n\n");
    out.push_str(&format!("Participants: {}\n\n", participants.join(", ")));

    for entry in entries {
        match &entry.kind {
            LogKind::Inbound { content, message } => {
                // Use `Source::Display`, which is the stable on-disk
                // identifier (`"user"` for the human, agent id for agents,
                // `"router"` for router-internal entries). Acceptance #171:
                // human messages must carry `from: "user"` in the audit log.
                let speaker = entry.from.to_string();
                match message {
                    MessageKind::Final => {
                        out.push_str(&format!("## {speaker}\n\n{}\n\n", content.trim_end()));
                    }
                    MessageKind::ToolUse { name } => {
                        out.push_str(&format!("_{speaker} used tool: {name}_\n\n"));
                    }
                    MessageKind::Partial => {
                        // Streaming partial -- the final result entry will
                        // carry the canonical text. Skipping avoids
                        // duplicate output in the transcript.
                    }
                }
            }
            LogKind::Outbound { .. } => {
                // Outbound deliveries duplicate inbound text -- skip them
                // in the transcript.
            }
            LogKind::SendFailed { to, error } => {
                out.push_str(&format!(
                    "```\nwarning: failed to deliver to {to}: {error}\n```\n\n"
                ));
            }
            LogKind::Termination { .. } => {
                // Rendered explicitly below so it is always the last line.
            }
        }
    }

    // Display (not Debug) so the on-disk transcript carries the stable
    // string form ("max_turns", "convergence", ...) rather than the variant
    // identifier; renaming a TerminationReason variant in source code must
    // not silently mutate historical audit text.
    out.push_str(&format!("---\nTermination: {termination}\n"));
    out
}

/// Count how many `Final` inbound messages `agent_id` emitted in the
/// router log. Tool-use entries and streaming partials do not count -- a
/// "turn" is a canonical result from the agent. Pure function so the
/// `meta.yaml` `participants` breakdown is testable without spawning a
/// router.
fn count_agent_turns(entries: &[crate::messaging::router::LogEntry], agent_id: &str) -> usize {
    use crate::messaging::router::{LogKind, MessageKind, Source};
    entries
        .iter()
        .filter(|e| matches!(&e.from, Source::Agent(id) if id == agent_id))
        .filter(|e| {
            matches!(
                &e.kind,
                LogKind::Inbound {
                    message: MessageKind::Final,
                    ..
                }
            )
        })
        .count()
}

/// Run steps sequentially in topological order.
pub async fn run_steps(
    steps: &[&Step],
    agents: &[Agent],
    ctx: &RunContext,
) -> Result<Vec<StepRunResult>, RunError> {
    // Public, state-less entry point. Plain `kuro run` (and tests) drive the
    // runner this way -- there is no `RunHandle` to publish a router on, so
    // conversation steps fall back to "stdin only when TTY" behavior.
    run_steps_with_state(steps, agents, ctx, None).await
}

/// Internal entry point that accepts the shared run-state slot a
/// [`flow_api::RunHandle`] uses to publish its [`RouterAccessor`]. The MCP
/// server (#199) drives the runner through this path so its `send_message`
/// tool can reach the live router during a conversation step.
///
/// Kept private so the `pub(super)` `RunState` type does not leak into the
/// crate's public surface.
pub(crate) async fn run_steps_with_state(
    steps: &[&Step],
    agents: &[Agent],
    ctx: &RunContext,
    run_state: Option<std::sync::Arc<flow_api::RunState>>,
) -> Result<Vec<StepRunResult>, RunError> {
    // Ensure the run directory layout (`steps/`, `messages/`) exists before
    // any step writes. main.rs `run_up` also calls this for the audit-write
    // ordering, but the task flow path doesn't, so we do it here too.
    // Idempotent.
    stack::init_run_layout(&ctx.run_path).map_err(|e| RunError::Stack {
        step: "<run-init>".to_string(),
        source: e,
    })?;

    let agent_map: HashMap<&str, &Agent> = agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let total = steps.len();
    let mut results: Vec<StepRunResult> = Vec::with_capacity(total);

    let executor = executor::create_executor();

    for (i, step) in steps.iter().enumerate() {
        // Shell steps (issue #23) run via `sh -c`, no agent or LLM. Branch
        // here so the agent-lookup below stays valid for LLM steps.
        if step.is_shell() {
            let result =
                run_shell_step(executor.as_ref(), step, ctx, i + 1, total, &results).await?;
            results.push(result);
            continue;
        }

        // Conversation steps (issue #170) drive multiple agents through the
        // messaging Router instead of a single agent's executor invocation.
        if step.is_conversation() {
            // Pass the shared run state through so the conversation step can
            // publish its `RouterAccessor` for external observers (MCP
            // `send_message`, #199). Stateless callers pass `None`.
            let state_clone = run_state.clone();
            let result =
                run_conversation_step(step, &agent_map, ctx, i + 1, total, state_clone).await?;
            results.push(result);
            continue;
        }

        let agent = agent_map
            .get(step.agent.as_str())
            .ok_or_else(|| RunError::UnknownAgent {
                step: step.id.clone(),
                agent: step.agent.clone(),
            })?;

        let effective_model = step.model.as_deref().unwrap_or(&agent.model);
        let effective_backend = step.backend.unwrap_or(agent.backend);
        // #236: extra_args cascade is replace-not-merge. A non-empty
        // step-level map fully shadows the agent map, even if it has no
        // entry for the effective backend (in which case no override
        // tokens are used). This matches the issue text "step replaces
        // agent" and keeps the resolution explicit -- no surprise merges.
        let effective_extra_args: &[String] = resolve_extra_args(step, agent, effective_backend);

        let step_info = StepInfo {
            id: step.id.clone(),
            agent: agent.name.clone(),
            title: agent.title.clone(),
            model: effective_model.to_string(),
            backend: effective_backend,
            input: step.input.clone(),
            state: StepState::Running,
        };

        ui::print_step_banner(i + 1, total, &step_info);

        // All steps get the flow prompt; step.task is appended in build_user_prompt
        let step_task = ctx.task.to_string();

        let user_content = build_user_prompt(&step_task, step, &ctx.run_path)?;

        // Pre-compute output path and show it immediately so user can tail -f.
        // Layout: `<stack>/<run-id>/steps/NN-<step>.md`. The summary table
        // renders `<run-id>/steps/NN-<step>.md` so `print_output: true` joins
        // it with `stack_path` to find the file.
        let step_num = i + 1;
        let content_filename = llm_output_filename(step_num, &step.id);
        let output_path = ctx
            .run_path
            .join(stack::STEPS_SUBDIR)
            .join(&content_filename);
        let output_file = format!(
            "{}/{}/{}",
            ctx.run_id,
            stack::STEPS_SUBDIR,
            content_filename
        );
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        eprintln!(
            "      output: {}",
            output_path
                .canonicalize()
                .unwrap_or(output_path.clone())
                .display()
        );

        // #236: surface the resolved extra_args next to the model/backend
        // so a user looking at the run output can see what override tokens
        // their YAML produced. We only print when non-empty -- the default
        // case stays quiet so existing run logs are byte-identical.
        if !effective_extra_args.is_empty() {
            let source = if !step.extra_args.is_empty() {
                "step"
            } else {
                "agent"
            };
            eprintln!(
                "      extra_args ({source}, {}): [{}]",
                effective_backend.yaml_name(),
                effective_extra_args.join(" ")
            );
        }

        let system_prompt =
            build_system_prompt(agent, &ctx.guide, &ctx.rules_cache, &ctx.skills_cache);

        let start = Instant::now();
        // Capture per-step wall-clock start. Sharing `ctx.started_at` across
        // every step would make the manifest's `started_at` indistinguishable
        // between steps even when their durations differ.
        let step_started_at = chrono::Utc::now();
        let spinner = ui::start_spinner();

        let (content, usage) = if executor::backend_needs_executor(effective_backend) {
            run_step_via_executor(
                executor.as_ref(),
                step,
                &ctx.flow_name,
                &system_prompt,
                &user_content,
                effective_model,
                effective_backend,
                effective_extra_args,
                &output_path,
            )
            .await?
        } else {
            let request = LlmRequest {
                model: effective_model.to_string(),
                system: Some(system_prompt),
                messages: vec![Message {
                    role: Role::User,
                    content: user_content.clone(),
                }],
                max_tokens: 4096,
            };
            run_step_via_api(request, &step.id).await?
        };

        spinner.stop();
        let duration = start.elapsed();

        let tokens_in = usage.as_ref().map(|u| u.input_tokens);
        let tokens_out = usage.as_ref().map(|u| u.output_tokens);

        // Build the per-step record. `model_actual` mirrors `model_requested`
        // today because no backend reports back a server-side concrete model
        // id; the field exists so audits stay schema-stable when one does.
        let record = StepRecord {
            step_id: step.id.clone(),
            kind: "llm".to_string(),
            agent: Some(agent.name.clone()),
            model_requested: Some(effective_model.to_string()),
            model_actual: Some(effective_model.to_string()),
            backend: backend_name(effective_backend).to_string(),
            tokens_in,
            tokens_out,
            duration_ms: duration.as_millis(),
            started_at: step_started_at.to_rfc3339(),
            exit_code: 0,
            input_steps: step.input.clone(),
            output_file: content_filename.clone(),
            participants: Vec::new(),
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: None,
        };

        // Write the canonical content file plus the meta.yaml. Executor
        // backends stream stdout into the artifact file during execution
        // (issue #16); we still rewrite from `content` here so the byte-for-byte
        // body referenced by the manifest matches what `read_run_step_content`
        // returns when downstream steps consume this output.
        stack::write_run_step(&ctx.run_path, step_num, &record, &content).map_err(|e| {
            RunError::Stack {
                step: step.id.clone(),
                source: e,
            }
        })?;

        let display_path = output_path
            .canonicalize()
            .unwrap_or(output_path.clone())
            .display()
            .to_string();
        ui::print_step_done(
            &format_duration(duration),
            &tokens_in.map_or("—".to_string(), |t| t.to_string()),
            &tokens_out.map_or("—".to_string(), |t| t.to_string()),
            &display_path,
        );

        // Post the step output as a GitHub comment when the flow asks for it.
        // Failures here never abort the flow -- gh outages should not undo
        // work that's already been written to the stack. The decision logic
        // lives in `notify::github::try_post_step_comment` so the soft-fail
        // contract is testable without spinning up the executor; this match
        // is just translating the outcome to user-facing log lines.
        if let Some(target) = step.post_comment {
            // Inputs are the prior step IDs declared on this step. Map them
            // to agent names from already-finished results so the header
            // reads "Review by Bella and Levi, consensus by Mika".
            let input_agents: Vec<&str> = step
                .input
                .iter()
                .filter_map(|input_id| {
                    results
                        .iter()
                        .find(|r| r.step_id == *input_id)
                        .map(|r| r.agent_name.as_str())
                })
                .collect();
            let outcome = github::try_post_step_comment(
                target,
                &agent.name,
                &content,
                &input_agents,
                &ctx.template_vars,
                &ctx.poster,
            );
            match outcome {
                PostOutcome::Posted { kind, number } => {
                    eprintln!("      posted comment on {kind} #{number}");
                }
                PostOutcome::NoIdProvided => {
                    eprintln!(
                        "warning: step '{}' declares post_comment but no 'id' template var was provided",
                        step.id
                    );
                }
                PostOutcome::Failed { error } => {
                    eprintln!(
                        "warning: step '{}' failed to post comment: {error}",
                        step.id
                    );
                }
            }
        }

        results.push(StepRunResult {
            step_id: step.id.clone(),
            agent_name: agent.name.clone(),
            backend: backend_name(effective_backend).to_string(),
            duration,
            tokens_in,
            tokens_out,
            output_file,
            print_output: step.print_output,
            record,
        });
    }

    Ok(results)
}

pub(crate) fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs > 0 {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    } else {
        format!("{}ms", d.as_millis())
    }
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Api => "api",
        Backend::ClaudeCli => "claude-cli",
        Backend::Codex => "codex",
        Backend::Ollama => "ollama",
    }
}

/// Build the summary table from run results.
pub fn build_summary(results: &[StepRunResult]) -> Vec<ui::StepResult> {
    results
        .iter()
        .map(|r| ui::StepResult {
            id: r.step_id.clone(),
            agent: r.agent_name.clone(),
            backend: r.backend.clone(),
            duration: format_duration(r.duration),
            tokens_in: r.tokens_in.map_or("—".to_string(), |t| t.to_string()),
            tokens_out: r.tokens_out.map_or("—".to_string(), |t| t.to_string()),
            output: r.output_file.clone(),
            state: StepState::Done,
        })
        .collect()
}

// =====================================================================
// Library API: execute_flow / RunHandle (issue #209)
// =====================================================================
//
// `execute_flow` is the clap-free orchestration entry point. `kuro run` is a
// thin wrapper around it; the MCP server (#194/#199) drives the same path
// without shelling out. The setup half (config load, role resolution, agent
// loading, audit, run-context construction) runs synchronously on the caller
// so configuration errors surface before a `RunHandle` exists. Step execution
// + manifest writing + summary printing run on a spawned tokio task that the
// returned handle awaits.

mod flow_api {
    //! Implementation of [`execute_flow`] and the supporting types. The module
    //! is public-in-private so the items can be re-exported from `runner` while
    //! their internals stay scoped to one file's worth of orchestration code.
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::LazyLock;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use color_eyre::Result;
    use color_eyre::eyre::eyre;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    use crate::config::{self, Backend, FlowConfig};
    use crate::dag;
    use crate::koto_config::{KOTO_CONFIG_FILE, KOTO_DIR, KotoConfig, Seeds};
    use crate::resolver::{
        self, ResolvedRole, RoleOverride, format_audit, print_audit, resolve_role,
        validate_role_overrides,
    };
    use crate::skills;
    use crate::stack::{self, Manifest, ResourceRecord, RoleResolution, SeedRecord};
    use crate::ui;

    use super::{RunContext, StepRunResult, run_steps_with_state};

    /// Where to read the flow YAML from. The CLI partitions its `--file`
    /// flag into the explicit `File` arm; `Name` triggers a seed walk;
    /// `Auto` is the no-arg case that auto-selects when exactly one flow
    /// exists across all seeds.
    pub enum FlowSource {
        Auto,
        Name(String),
        File(PathBuf),
    }

    /// All inputs needed to run a flow. Independent of clap so non-CLI
    /// callers (MCP, tests) can build it from any source. Field semantics
    /// match the historical `kuro run` cascade (project config < CLI <
    /// per-step) -- callers do not pre-merge.
    ///
    /// Path resolution operates on the process current working directory:
    /// `.kuro/config.yaml`, the seeds it declares, agent files, rules,
    /// skills, and the manifest's resource paths are all looked up
    /// relative to CWD. Callers that need to run a flow against a
    /// different project directory must `chdir` before calling
    /// [`execute_flow`]. Threading a project root through every loader is
    /// tracked separately -- see #199.
    pub struct ExecuteFlowSpec {
        /// Where to look for the flow definition.
        pub flow: FlowSource,
        /// Task prompt override. `None` falls back to the flow's `prompt:`.
        pub task: Option<String>,
        /// CLI-style `--var key=value` overrides. Project-config vars are
        /// merged in by `execute_flow`; this map carries only what the
        /// caller passed.
        pub vars: HashMap<String, String>,
        /// Pre-parsed `--role` overrides. Use [`crate::resolver::parse_role_override`]
        /// in the CLI; library callers can construct values directly.
        pub role_overrides: Vec<RoleOverride>,
        /// Bare `key=value` arguments. After the flow YAML is read,
        /// `execute_flow` partitions these into legacy role rebinds and
        /// template-var fills, mirroring the historical CLI behavior. Pass
        /// an empty map if the caller does not want the legacy partition.
        pub bare_args: HashMap<String, String>,
        /// When true, suppress the `kuro run <flow>` banner that the CLI
        /// prints up front. MCP and other quiet callers set this to true.
        pub suppress_command_banner: bool,
    }

    impl Default for ExecuteFlowSpec {
        fn default() -> Self {
            Self {
                flow: FlowSource::Auto,
                task: None,
                vars: HashMap::new(),
                role_overrides: Vec::new(),
                bare_args: HashMap::new(),
                suppress_command_banner: false,
            }
        }
    }

    /// Outcome of a completed flow run.
    ///
    /// The CLI wrapper currently consumes only `step_results` and
    /// `stack_path`; the remaining fields are part of the public surface
    /// for the MCP server (#199) and other in-process callers (#196,
    /// #198). Allowing dead code here keeps the API stable without
    /// forcing every consumer to read every field.
    #[allow(dead_code)]
    pub struct FlowResult {
        pub run_id: String,
        pub run_path: PathBuf,
        pub stack_path: PathBuf,
        pub flow_name: String,
        pub manifest: Manifest,
        pub step_results: Vec<StepRunResult>,
        pub total_elapsed: Duration,
    }

    /// Shared mutable state held jointly by the `RunHandle` and the
    /// spawned execution task. The handle reads `active_router` and writes
    /// `cancel`; the task does the inverse.
    ///
    /// Visibility is `pub(crate)` so the conversation step in
    /// [`super::run_steps_with_state`] can call `set_router`/`clear_router`
    /// without taking a dependency on the `RunHandle` itself. The struct is
    /// not part of the crate's outward-facing API; external callers reach
    /// the slot through [`ActiveRouter`] / [`RunHandle::router`].
    #[derive(Default)]
    pub(crate) struct RunState {
        cancel: AtomicBool,
        active_router: Mutex<Option<RouterAccessor>>,
    }

    impl RunState {
        pub(super) fn is_cancelled(&self) -> bool {
            self.cancel.load(Ordering::Relaxed)
        }

        /// Conversation steps call this on entry; the matching
        /// [`Self::clear_router`] runs when the router stops. The slot is
        /// `None` between conversations so an external accessor reflects
        /// "no live conversation" honestly.
        #[allow(dead_code)]
        pub(crate) fn set_router(&self, accessor: RouterAccessor) {
            if let Ok(mut guard) = self.active_router.lock() {
                *guard = Some(accessor);
            }
        }

        #[allow(dead_code)]
        pub(crate) fn clear_router(&self) {
            if let Ok(mut guard) = self.active_router.lock() {
                *guard = None;
            }
        }

        #[allow(dead_code)]
        fn snapshot_router(&self) -> Option<RouterAccessor> {
            self.active_router.lock().ok()?.clone()
        }
    }

    /// Live accessor for the router that is currently driving a
    /// conversation step. Use [`RouterAccessor::inject_human_message`] to
    /// broadcast a message to every participant; it surfaces inside the
    /// router exactly like a stdin-typed line, complete with audit log
    /// entry.
    ///
    /// The accessor is a thin wrapper around an mpsc sender so it can be
    /// cloned and held across tasks. Cloning produces another sender to
    /// the same channel.
    #[derive(Clone)]
    pub struct RouterAccessor {
        #[allow(dead_code)]
        sender: mpsc::Sender<String>,
    }

    impl RouterAccessor {
        /// Build an accessor + receiver pair. The receiver is wired into
        /// `Router::set_human_input`; the accessor is published on the
        /// shared run state so external callers (MCP `send_message`) can
        /// inject messages while the conversation runs.
        pub(super) fn new() -> (Self, mpsc::Receiver<String>) {
            let (tx, rx) = mpsc::channel(16);
            (Self { sender: tx }, rx)
        }

        /// Inject a human-style message into the live conversation.
        /// Returns an error if the router has already terminated (channel
        /// closed) so the caller can distinguish "no live conversation"
        /// from "delivery failed".
        #[allow(dead_code)]
        pub async fn inject_human_message(
            &self,
            text: impl Into<String>,
        ) -> std::result::Result<(), RouterAccessorError> {
            self.sender
                .send(text.into())
                .await
                .map_err(|_| RouterAccessorError::Closed)
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[allow(dead_code)]
    pub enum RouterAccessorError {
        #[error("router channel closed -- conversation has terminated")]
        Closed,
    }

    /// Cloneable view onto a run's shared state, scoped to looking up the
    /// live [`RouterAccessor`]. Distinct from [`RunHandle`] because the
    /// handle owns the join future and `await_completion` consumes it; the
    /// MCP `send_message` tool needs to read the live router after the
    /// run-flow tool has already moved into `await_completion`.
    ///
    /// Returns `None` between conversation steps or before the first one
    /// starts -- so a `send_message` call that arrives during a non-
    /// conversation step honestly reports "no live conversation".
    #[derive(Clone)]
    pub struct ActiveRouter {
        state: Arc<RunState>,
    }

    impl ActiveRouter {
        /// Snapshot of the currently published accessor, if any.
        pub fn current(&self) -> Option<RouterAccessor> {
            self.state.snapshot_router()
        }
    }

    /// Handle returned by [`execute_flow`]. Holds the join handle for the
    /// background execution task plus a shared-state slot the running task
    /// uses to publish a [`RouterAccessor`] for the active conversation
    /// step (if any). The handle does not auto-cancel on drop -- callers
    /// must call [`RunHandle::cancel`] explicitly if they want to abort.
    #[allow(dead_code)]
    pub struct RunHandle {
        pub run_id: String,
        pub run_path: PathBuf,
        pub stack_path: PathBuf,
        pub flow_name: String,
        state: Arc<RunState>,
        join: JoinHandle<Result<FlowResult>>,
    }

    impl RunHandle {
        /// Run id of the spawned flow. Same value as `FlowResult::run_id`
        /// after completion; available immediately so callers can show it
        /// before awaiting.
        #[allow(dead_code)]
        pub fn run_id(&self) -> &str {
            &self.run_id
        }

        /// Snapshot of the live router accessor, if a conversation step is
        /// currently driving the run. `None` between conversations or
        /// before any have started. The returned accessor is owned (not a
        /// borrow) so the caller can hold it across awaits without
        /// blocking the runner.
        #[allow(dead_code)]
        pub fn router(&self) -> Option<RouterAccessor> {
            self.state.snapshot_router()
        }

        /// Cloneable handle that survives `await_completion`. Use it to
        /// query the live [`RouterAccessor`] from another task while the
        /// run is in progress -- needed because `await_completion` consumes
        /// `self`, so callers cannot hold both the join handle and a
        /// router-lookup at the same time without splitting them up first.
        ///
        /// Wired to the MCP `send_message` tool (#199): the server stores
        /// one [`ActiveRouter`] per active run in its session state and
        /// resolves the live accessor on each call.
        pub fn active_router(&self) -> ActiveRouter {
            ActiveRouter {
                state: Arc::clone(&self.state),
            }
        }

        /// Request cancellation. Best-effort: the flag is currently only
        /// checked once, before the first step starts. Mid-run cancellation
        /// (between steps, or interrupting an in-flight step) is not yet
        /// wired -- see the comment in `run_to_completion` and the
        /// follow-up tracked alongside #199.
        #[allow(dead_code)]
        pub fn cancel(&self) {
            self.state.cancel.store(true, Ordering::Relaxed);
        }

        /// Await the spawned execution task and return the flow result.
        /// Consumes the handle, so double-await is a compile-time error and
        /// no runtime guard is needed. Returns an error if the spawned task
        /// itself panicked.
        pub async fn await_completion(self) -> Result<FlowResult> {
            match self.join.await {
                Ok(res) => res,
                Err(e) => Err(eyre!("flow execution task panicked: {e}")),
            }
        }
    }

    static VARS_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"\{\{vars\.([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap()
    });

    static PLACEHOLDER_RE: LazyLock<regex_lite::Regex> =
        LazyLock::new(|| regex_lite::Regex::new(r"\{\{([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap());

    static ROLES_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"\{\{roles\.([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap()
    });

    /// Replace `{{vars.<key>}}` placeholders. Mirrors the helper that lived
    /// in `main.rs` -- moved here so callers without the CLI binary can
    /// reuse it. Errors list every missing key in one message.
    pub(crate) fn substitute_vars(text: &str, vars: &HashMap<String, String>) -> Result<String> {
        let mut missing: Vec<String> = Vec::new();
        let result = VARS_RE.replace_all(text, |caps: &regex_lite::Captures<'_>| {
            let key = &caps[1];
            match vars.get(key) {
                Some(value) => value.clone(),
                None => {
                    if !missing.iter().any(|k| k == key) {
                        missing.push(key.to_string());
                    }
                    caps[0].to_string()
                }
            }
        });
        if !missing.is_empty() {
            return Err(eyre!(
                "missing vars: {}\n\nhint: define them in {KOTO_CONFIG_FILE} or pass --var key=value",
                missing.join(", ")
            ));
        }
        Ok(result.into_owned())
    }

    /// Replace `{{roles.<name>}}` placeholders with the agent ID bound to
    /// that role (issue #259). The `roles` map is the cascade-resolved
    /// `role -> agent_id` table the runner has already built (linear:
    /// `flow_config.roles` after `apply_role_agent_overrides`; graph: a
    /// freshly built map from project config + CLI overrides + per-state
    /// `role:` declarations).
    ///
    /// Errors collect every unknown role referenced in `text` into a
    /// single message, naming `context` (e.g. `state 'steer_design'`,
    /// `flow prompt`, `step 'design'`) so the operator can find the bad
    /// placeholder without grepping the whole flow.
    pub(crate) fn substitute_roles(
        text: &str,
        roles: &HashMap<String, String>,
        context: &str,
    ) -> Result<String> {
        let mut missing: Vec<String> = Vec::new();
        let result = ROLES_RE.replace_all(text, |caps: &regex_lite::Captures<'_>| {
            let role = &caps[1];
            match roles.get(role) {
                Some(agent_id) => agent_id.clone(),
                None => {
                    if !missing.iter().any(|r| r == role) {
                        missing.push(role.to_string());
                    }
                    caps[0].to_string()
                }
            }
        });
        if !missing.is_empty() {
            // Match the wording the issue's AC2 spells out: the operator
            // sees which role is unbound and where the placeholder lives.
            let plural = if missing.len() == 1 { "role" } else { "roles" };
            return Err(eyre!(
                "unknown {plural} referenced in {context}: {}\n\nhint: bind the role in {KOTO_CONFIG_FILE} (`roles:` map) or pass --role {}=<agent>",
                missing.join(", "),
                missing[0]
            ));
        }
        Ok(result.into_owned())
    }

    /// Replace bare `{{key}}` placeholders. Counterpart to
    /// [`substitute_vars`] for the CLI key=value namespace.
    pub(crate) fn substitute_placeholders(
        prompt: &str,
        vars: &HashMap<String, String>,
    ) -> Result<String> {
        let mut result = prompt.to_string();
        let mut missing: Vec<String> = Vec::new();
        for cap in PLACEHOLDER_RE.captures_iter(prompt) {
            let key = &cap[1];
            match vars.get(key) {
                Some(value) => {
                    result = result.replace(&format!("{{{{{key}}}}}"), value);
                }
                None => {
                    if !missing.contains(&key.to_string()) {
                        missing.push(key.to_string());
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Err(eyre!(
                "missing template arguments: {}\n\nhint: pass them as key=value, e.g. {}",
                missing.join(", "),
                missing
                    .iter()
                    .map(|k| format!("{k}=<value>"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        Ok(result)
    }

    /// Resolve the task prompt: explicit override > flow default with bare
    /// placeholder fills.
    pub(crate) fn resolve_task(
        task_flag: Option<&str>,
        flow_prompt: &Option<String>,
        template_vars: &HashMap<String, String>,
    ) -> Result<String> {
        if let Some(task) = task_flag {
            return Ok(task.to_string());
        }
        if let Some(prompt) = flow_prompt {
            return substitute_placeholders(prompt, template_vars);
        }
        Err(eyre!(
            "no task specified\n\nhint: use -t \"task\" or define a prompt in the flow YAML"
        ))
    }

    /// Resolve the on-disk flow YAML path according to the cascade.
    pub(crate) fn resolve_flow_path(source: &FlowSource, seeds: &Seeds) -> Result<PathBuf> {
        match source {
            FlowSource::File(p) => {
                if !p.exists() {
                    return Err(eyre!("config file '{}' not found", p.display()));
                }
                Ok(p.clone())
            }
            FlowSource::Name(name) => {
                let rel = std::path::Path::new("flows").join(format!("{name}.yaml"));
                match seeds.find(&rel).map_err(|e| eyre!("{}", e.message()))? {
                    Some((_, path)) => Ok(path),
                    None => Err(eyre!(
                        "{}\n\nhint: create flows/{name}.yaml in one of the seeds, or use --file <path>",
                        seeds.not_found_message("flow", name)
                    )),
                }
            }
            FlowSource::Auto => {
                let mut by_name: std::collections::BTreeMap<String, PathBuf> =
                    std::collections::BTreeMap::new();
                for seed in &seeds.seeds {
                    let Some(seed_path) = seed.local_path() else {
                        continue;
                    };
                    let flows_dir = seed_path.join("flows");
                    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let file_name = entry.file_name();
                        let name_str = file_name.to_string_lossy();
                        if !(name_str.ends_with(".yaml") || name_str.ends_with(".yml")) {
                            continue;
                        }
                        let bare = name_str
                            .trim_end_matches(".yaml")
                            .trim_end_matches(".yml")
                            .to_string();
                        by_name
                            .entry(bare)
                            .or_insert_with(|| flows_dir.join(name_str.as_ref()));
                    }
                }
                if by_name.is_empty() {
                    return Err(eyre!(
                        "no flows found in seeds: {}\n\nhint: create flows/<name>.yaml in one of the seed directories",
                        seeds.audit_line()
                    ));
                }
                if by_name.len() == 1 {
                    let (_n, path) = by_name.into_iter().next().expect("len == 1");
                    return Ok(path);
                }
                let list = by_name
                    .keys()
                    .map(|f| format!("  - {f}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(eyre!(
                    "multiple flows found, specify one:\n\n{list}\n\nusage: kuro run <flow-name> -t \"task\""
                ))
            }
        }
    }

    /// Resolve the project's stack directory. Explicit config > default of
    /// `~/.koto/stacks/<project>/`. The `.koto/` home root is intentional
    /// (see comment trail in #176): a rename here would orphan existing
    /// users' run history without a migration path.
    ///
    /// The home root itself comes from [`crate::stack::stack_root`] so a
    /// future relocation has one source of truth -- and so `kuro stack
    /// purge` (#232) targets exactly the directory the runner writes to.
    pub(crate) fn resolve_stack_path(config_path: &str) -> PathBuf {
        if !config_path.is_empty() {
            return PathBuf::from(config_path);
        }
        let project = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "default".to_string());
        crate::stack::stack_root().join(project)
    }

    /// Resolve the stack directory for a named flow, honoring the flow's
    /// own `stack.path` override. Walks the seed cascade exactly like
    /// `execute_flow` so a run started by `run_flow` and a status query via
    /// `show_output` agree on where the artifacts live. Falls back to the
    /// default when the flow file declares no `stack.path`.
    pub(crate) fn resolve_stack_path_for_flow_name(name: &str) -> Result<PathBuf> {
        let koto_config = KotoConfig::load_optional(Path::new("."))?;
        let seeds = koto_config
            .as_ref()
            .map(|c| c.seeds.clone())
            .unwrap_or_else(Seeds::default_local);
        let path = resolve_flow_path(&FlowSource::Name(name.to_string()), &seeds)?;
        // #258: path-aware loader so a flow with `prompt_file:` /
        // `task_file:` resolves consistently with the runner. The stack
        // path itself does not depend on prompt content; using the same
        // entry point everywhere keeps "what counts as a valid flow"
        // identical across the binary.
        let flow_config = config::load_flow_from_path(&path)?;
        Ok(resolve_stack_path(&flow_config.stack.path))
    }

    /// Verify that the named flow defines all `required` step ids and return
    /// the list of those that are missing (empty when the flow is OK).
    ///
    /// Used by tool wrappers that key off specific step names to build
    /// their result (`implement_issue` keys off `review`/`pr`). Without
    /// this check, a renamed step would silently make `read_run` lookups
    /// return `None` and the caller would report a successful run with an
    /// empty/unclear payload -- the failure mode flagged in the team
    /// review on #213. Resolves the flow through the seed cascade so the
    /// check uses the same file the runner would execute.
    ///
    /// Accepts both flow shapes (#268): linear flows are matched against
    /// `steps[].id`; graph flows are matched against `states` keys. Both
    /// shapes persist per-step records under the same id, so the same
    /// required-id list works for both -- the loader just needs to handle
    /// either YAML shape rather than only the linear one.
    pub(crate) fn verify_flow_step_ids(flow_name: &str, required: &[&str]) -> Result<Vec<String>> {
        let koto_config = KotoConfig::load_optional(Path::new("."))?;
        let seeds = koto_config
            .as_ref()
            .map(|c| c.seeds.clone())
            .unwrap_or_else(Seeds::default_local);
        let path = resolve_flow_path(&FlowSource::Name(flow_name.to_string()), &seeds)?;
        // #258: path-aware loader so flows with `prompt_file:` /
        // `task_file:` references load consistently across entry
        // points. Step-id verification does not read task content, but
        // a flow with broken external prompts should fail here too --
        // not silently succeed only to blow up at execute time.
        let has_id: Box<dyn Fn(&str) -> bool> = match config::load_flow_any_from_path(&path)? {
            config::Flow::Linear(flow_config) => {
                let ids: Vec<String> = flow_config.steps.iter().map(|s| s.id.clone()).collect();
                Box::new(move |id| ids.iter().any(|s| s == id))
            }
            config::Flow::Graph(graph) => {
                let ids: Vec<String> = graph.states.keys().cloned().collect();
                Box::new(move |id| ids.iter().any(|s| s == id))
            }
        };
        Ok(required
            .iter()
            .filter(|id| !has_id(id))
            .map(|s| s.to_string())
            .collect())
    }

    /// Apply the resolver-decided agent for every role in the flow.
    pub(crate) fn apply_role_agent_overrides(
        flow_config: &mut FlowConfig,
        koto_config: Option<&KotoConfig>,
        overrides: &[RoleOverride],
    ) {
        let mut decided: HashMap<String, String> = HashMap::new();
        for role_name in flow_config.roles.keys() {
            let flow_agent = flow_config.roles.get(role_name).map(String::as_str);
            let project_role = koto_config.and_then(|c| c.roles.get(role_name));
            if let Some(agent) =
                resolver::resolve_role_agent(role_name, flow_agent, project_role, overrides)
            {
                decided.insert(role_name.clone(), agent);
            }
        }
        for (role_name, agent_id) in flow_config.roles.iter_mut() {
            if let Some(new_agent) = decided.get(role_name) {
                *agent_id = new_agent.clone();
            }
        }
        for step in flow_config.steps.iter_mut() {
            if let Some(role) = step.role.as_deref()
                && let Some(new_agent) = decided.get(role)
            {
                step.agent = new_agent.clone();
            }
        }
    }

    /// Build the cascade-resolved binding for every role used in the flow.
    pub(crate) fn build_resolved_roles(
        flow_config: &FlowConfig,
        agents: &[config::Agent],
        koto_config: Option<&KotoConfig>,
        cli_overrides: &[RoleOverride],
    ) -> std::result::Result<Vec<ResolvedRole>, resolver::ResolverError> {
        let agents_by_id: HashMap<&str, &config::Agent> =
            agents.iter().map(|a| (a.id.as_str(), a)).collect();

        let mut used_roles: HashSet<&str> = HashSet::new();
        for step in &flow_config.steps {
            if let Some(role) = step.role.as_deref() {
                used_roles.insert(role);
            }
        }

        let mut out = Vec::new();
        for role_name in used_roles {
            let flow_agent = flow_config.roles.get(role_name).map(String::as_str);
            let project_role = koto_config.and_then(|c| c.roles.get(role_name));

            let agent_for_lookup = flow_agent
                .or_else(|| project_role.map(|r| r.agent.as_str()))
                .unwrap_or("");
            let agent = agents_by_id.get(agent_for_lookup).copied();

            let agent_model = agent.map(|a| a.model.as_str()).unwrap_or("");
            let agent_backend = agent.map(|a| a.backend).unwrap_or(Backend::ClaudeCli);
            let agent_tier = agent.and_then(|a| read_agent_tier(a.id.as_str()));

            let input = resolver::RoleResolveInput {
                role_name,
                agent_model,
                agent_tier: agent_tier.as_deref(),
                agent_backend,
                flow_default_model: flow_config.defaults.model.as_str(),
                // #236: feed the agent's extra_args through so the resolver
                // can pin the slice for the resolved backend onto
                // ResolvedRole.extra_args. The audit reads it from there.
                agent_extra_args: agent.map(|a| &a.extra_args),
            };

            let resolved = resolve_role(
                &input,
                flow_agent,
                project_role,
                cli_overrides,
                koto_config.and_then(|c| c.default_backend),
            )
            .ok_or_else(|| resolver::ResolverError::UnknownRole {
                name: role_name.to_string(),
            })?;
            out.push(resolved);
        }
        Ok(out)
    }

    fn read_agent_tier(agent_id: &str) -> Option<String> {
        let path = Path::new(KOTO_DIR)
            .join("agents")
            .join(format!("{agent_id}.yaml"));
        let contents = std::fs::read_to_string(&path).ok()?;
        #[derive(serde::Deserialize)]
        struct TierOnly {
            tier: Option<String>,
        }
        let parsed: TierOnly = serde_yaml::from_str(&contents).ok()?;
        parsed.tier
    }

    pub(crate) fn apply_resolved_roles_to_steps(
        flow_config: &mut FlowConfig,
        resolved: &[ResolvedRole],
    ) {
        let by_role: HashMap<&str, &ResolvedRole> =
            resolved.iter().map(|r| (r.name.as_str(), r)).collect();
        for step in flow_config.steps.iter_mut() {
            let Some(role_name) = step.role.as_deref() else {
                continue;
            };
            let Some(rr) = by_role.get(role_name) else {
                continue;
            };
            if step.model.is_none() {
                step.model = Some(rr.model.clone());
            }
            if step.backend.is_none() {
                step.backend = Some(rr.backend);
            }
        }
    }

    fn backend_label(b: Backend) -> &'static str {
        match b {
            Backend::Api => "api",
            Backend::ClaudeCli => "claude-cli",
            Backend::Codex => "codex",
            Backend::Ollama => "ollama",
        }
    }

    /// Build the run manifest. Pure -- no I/O happens here; the caller is
    /// expected to write the result to `<run_path>/manifest.yaml`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_manifest(
        ctx: &RunContext,
        flow_name: &str,
        flow_path: &Path,
        flow_contents: &str,
        seeds: &Seeds,
        agents: &[config::Agent],
        agent_origins: &HashMap<String, usize>,
        agent_hashes: &HashMap<String, String>,
        roles: &[ResolvedRole],
        vars: &HashMap<String, String>,
        results: &[StepRunResult],
        total_elapsed: Duration,
        final_state: Option<&str>,
    ) -> Manifest {
        let finished_at = chrono::Utc::now();

        let seed_records: Vec<SeedRecord> = seeds
            .seeds
            .iter()
            .map(|s| SeedRecord {
                display: s.display(),
                path: s.local_path().map(|p| p.display().to_string()),
                git_sha: None,
                dirty: false,
            })
            .collect();

        let mut resources: Vec<ResourceRecord> = Vec::new();
        resources.push(ResourceRecord {
            kind: "flow".to_string(),
            name: flow_name.to_string(),
            path: flow_path.display().to_string(),
            sha256: stack::sha256_hex(flow_contents.as_bytes()),
        });
        for agent in agents {
            let rel = std::path::Path::new("agents").join(format!("{}.yaml", agent.id));
            let path_str = match seeds.find(&rel) {
                Ok(Some((_, p))) => p.display().to_string(),
                _ => agent_origins
                    .get(&agent.id)
                    .and_then(|idx| seeds.seeds.get(*idx))
                    .map(|s| s.display())
                    .unwrap_or_default(),
            };
            let sha = agent_hashes.get(&agent.id).cloned().unwrap_or_default();
            resources.push(ResourceRecord {
                kind: "agent".to_string(),
                name: agent.id.clone(),
                path: path_str,
                sha256: sha,
            });
        }
        let mut rule_names: Vec<&String> = ctx.rules_cache.keys().collect();
        rule_names.sort();
        for name in rule_names {
            let rel = std::path::Path::new("rules").join(format!("{name}.md"));
            let path_str = match seeds.find(&rel) {
                Ok(Some((_, p))) => p.display().to_string(),
                _ => String::new(),
            };
            let content = ctx.rules_cache.get(name).map(String::as_str).unwrap_or("");
            resources.push(ResourceRecord {
                kind: "rules".to_string(),
                name: name.clone(),
                path: path_str,
                sha256: stack::sha256_hex(content.as_bytes()),
            });
        }
        let mut skill_names: Vec<&String> = ctx.skills_cache.keys().collect();
        skill_names.sort();
        for name in skill_names {
            let content = ctx.skills_cache.get(name).map(String::as_str).unwrap_or("");
            resources.push(ResourceRecord {
                kind: "skill".to_string(),
                name: name.clone(),
                path: format!("{KOTO_DIR}/skills/{name}"),
                sha256: stack::sha256_hex(content.as_bytes()),
            });
        }
        if let Some(guide) = ctx.guide.as_deref() {
            let path_str = seeds
                .find(std::path::Path::new("Guide.md"))
                .ok()
                .flatten()
                .map(|(_, p)| p.display().to_string())
                .unwrap_or_default();
            resources.push(ResourceRecord {
                kind: "guide".to_string(),
                name: "Guide".to_string(),
                path: path_str,
                sha256: stack::sha256_hex(guide.as_bytes()),
            });
        }

        let role_records: Vec<RoleResolution> = roles
            .iter()
            .map(|r| RoleResolution {
                role: r.name.clone(),
                agent: r.agent.clone(),
                model: r.model.clone(),
                backend: backend_label(r.backend).to_string(),
                model_source: r.model_source.clone(),
                backend_source: r.backend_source.clone(),
                seed_origin: r.seed_origin.clone(),
            })
            .collect();

        let mut keys: Vec<&String> = vars.keys().collect();
        keys.sort();
        let var_map: indexmap::IndexMap<String, String> = keys
            .into_iter()
            .map(|k| (k.clone(), vars[k].clone()))
            .collect();

        let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
        let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();

        Manifest {
            version: 1,
            run_id: ctx.run_id.clone(),
            flow_name: flow_name.to_string(),
            flow_path: flow_path.display().to_string(),
            flow_sha256: stack::sha256_hex(flow_contents.as_bytes()),
            started_at: ctx.started_at.to_rfc3339(),
            finished_at: finished_at.to_rfc3339(),
            duration_ms: total_elapsed.as_millis(),
            total_tokens_in: total_in,
            total_tokens_out: total_out,
            cost: None,
            vars: var_map,
            seeds: seed_records,
            resources,
            roles: role_records,
            steps: results.iter().map(|r| r.record.clone()).collect(),
            final_state: final_state.map(str::to_string),
        }
    }

    /// Library entry point. Loads project config, parses the flow YAML,
    /// resolves the role + agent + skill cascade, builds a [`RunContext`],
    /// and spawns step execution on a tokio task. Returns immediately with
    /// a [`RunHandle`] that the caller awaits via `await_completion`.
    ///
    /// Setup-side errors (config parse, missing flow, unknown role,
    /// missing skill, ...) are returned synchronously so the CLI fails
    /// fast and a `RunHandle` is never produced for an unrunnable flow.
    pub async fn execute_flow(spec: ExecuteFlowSpec) -> Result<RunHandle> {
        let flow_start = Instant::now();

        // ---- 1. Project config + seed resolution -----------------------
        // CWD-relative: see ExecuteFlowSpec docs. Seeds, KOTO_DIR lookups
        // and skills_dir below all resolve against the process CWD too,
        // so this loader must use the same anchor for consistency.
        let koto_config = KotoConfig::load_optional(Path::new("."))?;
        let seeds = koto_config
            .as_ref()
            .map(|c| c.seeds.clone())
            .unwrap_or_else(Seeds::default_local);

        let path = resolve_flow_path(&spec.flow, &seeds)?;
        let display_path = path.display().to_string();

        if !spec.suppress_command_banner {
            let banner = match &spec.flow {
                FlowSource::Name(n) => n.clone(),
                _ => display_path.clone(),
            };
            ui::print_command(&format!("kuro run {banner}"));
        }

        // ---- 2. Var/role merge -----------------------------------------
        let mut effective_vars = koto_config
            .as_ref()
            .map(|c| c.vars.clone())
            .unwrap_or_default();
        for (k, v) in &spec.vars {
            effective_vars.insert(k.clone(), v.clone());
        }

        // Read flow YAML once -- needed for role-name partitioning and for
        // the manifest's `flow_sha256`.
        let contents = std::fs::read_to_string(&path)?;

        // Graph-flow pre-flight (issue #238). The runtime for state-graph
        // flows does not exist yet; before the linear loader trips on a
        // missing `flow:` field with a confusing serde error, probe the
        // shape and:
        //   * surface a graph-aware error if the flow is a graph,
        //   * run the reachability/dead-end validator first so dead-ends
        //     fail with a graph-aware message *before* any agent spawn.
        // Acceptance criteria #5 from the issue: a dead-end graph must
        // refuse to start before any agent is spawned.
        // #258: path-aware so graph flows with `prompt_file:` /
        // `task_file:` arrive fully resolved at
        // `execute_graph_flow_setup`. The linear branch falls through
        // to the path-aware loader below.
        match config::load_flow_any_from_path(&path) {
            Ok(config::Flow::Graph(g)) => {
                let report = config::validate_graph_reachability(&g);
                for warning in &report.warnings {
                    eprintln!("warning: {warning}");
                }
                for error in &report.errors {
                    eprintln!("error: {error}");
                }
                if !report.is_ok() {
                    return Err(eyre!(
                        "graph flow '{}' has {} validation error(s); refusing to start",
                        path.display(),
                        report.errors.len()
                    ));
                }
                // Hand off to the graph-flow setup. Returns a `RunHandle`
                // exactly like the linear path so callers (CLI, MCP) await
                // both flow shapes through the same surface.
                return execute_graph_flow_setup(
                    g,
                    spec,
                    koto_config.as_ref(),
                    &seeds,
                    &effective_vars,
                    path,
                    contents,
                    flow_start,
                )
                .await;
            }
            Ok(config::Flow::Linear(_)) => {}
            Err(e) => return Err(e.into()),
        }

        let role_names = config::parse_role_names(&contents)?;

        // Bare key=value args partition by role-name membership.
        let (legacy_role_overrides, template_vars): (
            HashMap<String, String>,
            HashMap<String, String>,
        ) = spec
            .bare_args
            .into_iter()
            .partition(|(k, _)| role_names.contains(k));

        if !template_vars.is_empty() {
            let mut keys: Vec<&String> = template_vars.keys().collect();
            keys.sort();
            eprintln!(
                "warning: bare key=value args are deprecated, use --var: {}",
                keys.iter()
                    .map(|k| format!("--var {k}={}", template_vars[*k]))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            for (k, v) in &template_vars {
                effective_vars.entry(k.clone()).or_insert_with(|| v.clone());
            }

            let flow_config_temp = config::load_flow_from_str(&contents)?;
            let placeholders = flow_config_temp
                .prompt
                .as_ref()
                .map(|p| config::extract_placeholders(p))
                .unwrap_or_default();
            for key in template_vars.keys() {
                if !placeholders.contains(key) {
                    eprintln!(
                        "warning: '{}' is not a declared role or template placeholder",
                        key
                    );
                }
            }
        }

        if !legacy_role_overrides.is_empty() {
            let mut keys: Vec<&String> = legacy_role_overrides.keys().collect();
            keys.sort();
            eprintln!(
                "warning: bare key=value role rebinds are deprecated, use --role: {}",
                keys.iter()
                    .map(|k| format!("--role {k}={}", legacy_role_overrides[*k]))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }

        // ---- 3. Load flow with project + legacy role bindings ----------
        let project_roles: HashMap<String, String> = koto_config
            .as_ref()
            .map(|c| {
                c.roles
                    .iter()
                    .map(|(k, v)| (k.clone(), v.agent.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // #258: path-aware variant so per-step `task_file:` and the
        // top-level `prompt_file:` resolve against the flow's
        // directory. After this returns, every `step.task` and the
        // flow `prompt:` already carry the file contents and the
        // subsequent `substitute_vars` pass treats them identically
        // to inline strings.
        let mut flow_config = config::load_flow_from_str_with_project_at(
            &contents,
            config::flow_base_dir_for(&path),
            &path.display().to_string(),
            &legacy_role_overrides,
            &project_roles,
        )?;

        // ---- 4a. Validate + apply role cascade BEFORE substitution -----
        // Issue #259: `{{roles.<name>}}` substitution needs the final
        // role -> agent map. Running the cascade first writes the
        // CLI/project/flow-resolved agent into `flow_config.roles`, which
        // `substitute_roles` below reads from. Validate first so a bad
        // `--role` argument errors with the resolver's typed error
        // before any substitution work happens.
        let koto_dir = Path::new(KOTO_DIR);
        validate_role_overrides(&spec.role_overrides, &flow_config, koto_config.as_ref())
            .map_err(|e| eyre!("{e}"))?;
        apply_role_agent_overrides(&mut flow_config, koto_config.as_ref(), &spec.role_overrides);

        // ---- 4b. Var + role substitution in flow + per-step task/run ---
        // Order: vars first (they may appear inside any string), roles
        // second (so a `{{roles.X}}` placeholder cannot accidentally be
        // produced by var expansion). Both run before any agent spawns
        // so AC2 holds: an unknown role aborts the run before work starts.
        if let Some(ref mut prompt) = flow_config.prompt {
            *prompt = substitute_vars(prompt, &effective_vars)?;
            *prompt = substitute_roles(prompt, &flow_config.roles, "flow prompt")?;
        }
        for step in flow_config.steps.iter_mut() {
            if let Some(task_str) = step.task.as_mut() {
                *task_str = substitute_vars(task_str, &effective_vars)?;
                let ctx = format!("step '{}'", step.id);
                *task_str = substitute_roles(task_str, &flow_config.roles, &ctx)?;
            }
            if let Some(run_str) = step.run.as_mut() {
                *run_str = substitute_vars(run_str, &effective_vars)?;
                *run_str = substitute_placeholders(run_str, &effective_vars)?;
                let ctx = format!("step '{}'", step.id);
                *run_str = substitute_roles(run_str, &flow_config.roles, &ctx)?;
            }
        }
        let task_with_vars = spec
            .task
            .as_deref()
            .map(|t| substitute_vars(t, &effective_vars))
            .transpose()?;
        let task_with_roles = task_with_vars
            .as_deref()
            .map(|t| substitute_roles(t, &flow_config.roles, "-t task"))
            .transpose()?;
        let resolved_task = resolve_task(
            task_with_roles.as_deref(),
            &flow_config.prompt,
            &template_vars,
        )?;

        let flow_name = match &spec.flow {
            FlowSource::Name(n) => n.clone(),
            _ => flow_config.name.clone(),
        };

        // ---- 6. Load agents and resolve roles --------------------------
        let (agents, agent_origins, agent_hashes) =
            config::load_agents_for_flow_with_seeds(&seeds, &flow_config, koto_config.as_ref())?;
        let mut resolved_roles = build_resolved_roles(
            &flow_config,
            &agents,
            koto_config.as_ref(),
            &spec.role_overrides,
        )
        .map_err(|e| eyre!("{e}"))?;
        for r in resolved_roles.iter_mut() {
            if let Some(idx) = agent_origins.get(&r.agent)
                && let Some(seed) = seeds.seeds.get(*idx)
            {
                r.seed_origin = Some(seed.display());
            }
        }

        // ---- 7. Audit output ------------------------------------------
        let cli_vars_for_audit = spec.vars.clone();
        let audit_text = format_audit(&seeds, &resolved_roles, &cli_vars_for_audit);
        print_audit(&seeds, &resolved_roles, &cli_vars_for_audit);

        apply_resolved_roles_to_steps(&mut flow_config, &resolved_roles);

        ui::print_flow_start(
            &flow_config.name,
            &display_path,
            flow_config.steps.len(),
            agents.len(),
        );
        super::try_print_issue_banner(&effective_vars);

        // ---- 8. DAG validation + backend list -------------------------
        let topo = dag::validate_dag(&flow_config)?;
        // Topological order produces references into `flow_config.steps`;
        // we'll need an owned copy on the spawned task, so collect step
        // ids in topo order and re-resolve once `flow_config` lives there.
        let topo_ids: Vec<String> = topo.iter().map(|s| s.id.clone()).collect();

        let mut seen_backends = HashSet::new();
        let mut backend_list: Vec<(&str, &str)> = Vec::new();
        for agent in &agents {
            let name = match agent.backend {
                Backend::Api => "api",
                Backend::ClaudeCli => "claude-cli",
                Backend::Codex => "codex",
                Backend::Ollama => "ollama",
            };
            if seen_backends.insert(name) {
                backend_list.push((name, ""));
            }
        }
        ui::print_backends_ok(&backend_list);

        // ---- 9. Guide / rules / skills --------------------------------
        let guide = super::load_guide_from_seeds(&seeds).map_err(|e| eyre!("{e}"))?;
        let rules_cache =
            super::load_rules_for_agents_with_seeds(&agents, &seeds).map_err(|e| eyre!("{e}"))?;
        let skills_dir = koto_dir.join("skills");
        let skill_names = skills::collect_skill_names(&agents);
        let skills_cache = if skill_names.is_empty() {
            HashMap::new()
        } else {
            let missing = skills::check_skills_available(&skill_names, &skills_dir);
            if !missing.is_empty() {
                return Err(eyre!(
                    "missing skills: {}\n\nhint: run `kuro pull` to fetch skills",
                    missing.join(", ")
                ));
            }
            skills::load_skills_for_agents(&skill_names, &skills_dir)?
        };

        // ---- 10. RunContext + on-disk run layout ----------------------
        let stack_path = resolve_stack_path(&flow_config.stack.path);
        let ctx = RunContext::new(
            flow_name.clone(),
            resolved_task,
            stack_path.clone(),
            guide,
            rules_cache,
            skills_cache,
            effective_vars.clone(),
        );

        stack::init_run_layout(&ctx.run_path)
            .map_err(|e| eyre!("failed to create run dir: {e}"))?;
        if let Err(e) = stack::write_resolution_audit(&ctx.run_path, &audit_text) {
            eprintln!("warning: failed to write resolution-audit.txt: {e}");
        }

        // ---- 11. Spawn the execution task -----------------------------
        let state = Arc::new(RunState::default());
        let run_id = ctx.run_id.clone();
        let run_path = ctx.run_path.clone();
        let stack_path_for_handle = ctx.stack_path.clone();
        let flow_name_for_handle = flow_name.clone();

        // Move owned data into the task. The borrowed `topo` slice can't
        // cross the spawn boundary, so we pass the topo step ids and let
        // the task re-borrow into the owned `flow_config` it now holds.
        let task_state = Arc::clone(&state);
        let join: JoinHandle<Result<FlowResult>> = tokio::spawn(async move {
            run_to_completion(
                ctx,
                flow_config,
                topo_ids,
                agents,
                agent_origins,
                agent_hashes,
                resolved_roles,
                effective_vars,
                seeds,
                path,
                contents,
                flow_name,
                flow_start,
                task_state,
            )
            .await
        });

        Ok(RunHandle {
            run_id,
            run_path,
            stack_path: stack_path_for_handle,
            flow_name: flow_name_for_handle,
            state,
            join,
        })
    }

    /// Body of the spawned execution task. Kept separate from
    /// `execute_flow` so the synchronous setup half stays readable and
    /// the task can be tested in isolation if needed.
    #[allow(clippy::too_many_arguments)]
    async fn run_to_completion(
        ctx: RunContext,
        flow_config: FlowConfig,
        topo_ids: Vec<String>,
        agents: Vec<config::Agent>,
        agent_origins: HashMap<String, usize>,
        agent_hashes: HashMap<String, String>,
        resolved_roles: Vec<ResolvedRole>,
        effective_vars: HashMap<String, String>,
        seeds: Seeds,
        flow_path: PathBuf,
        flow_contents: String,
        flow_name: String,
        flow_start: Instant,
        state: Arc<RunState>,
    ) -> Result<FlowResult> {
        // Cooperative cancellation check: if a caller flipped the cancel
        // flag between `execute_flow` returning the handle and this task
        // beginning, we abort before running the first step. This is the
        // only check today -- mid-run cancellation (between steps, or
        // interrupting an in-flight step) requires threading `state` into
        // `run_steps`, which is tracked alongside the MCP work in #199.
        if state.is_cancelled() {
            return Err(eyre!("run cancelled before steps started"));
        }

        // Re-borrow steps in topo order from the owned flow_config.
        let by_id: HashMap<&str, &config::Step> = flow_config
            .steps
            .iter()
            .map(|s| (s.id.as_str(), s))
            .collect();
        let steps: Vec<&config::Step> = topo_ids
            .iter()
            .map(|id| {
                *by_id
                    .get(id.as_str())
                    .expect("topo step id missing from flow_config")
            })
            .collect();

        // Pass `state` through so the conversation step can publish its
        // `RouterAccessor` for `RunHandle::router` / `ActiveRouter::current`
        // (#199 dependency on the #209 wiring).
        let results = run_steps_with_state(&steps, &agents, &ctx, Some(Arc::clone(&state))).await?;

        let total_elapsed = flow_start.elapsed();

        let manifest = build_manifest(
            &ctx,
            &flow_name,
            &flow_path,
            &flow_contents,
            &seeds,
            &agents,
            &agent_origins,
            &agent_hashes,
            &resolved_roles,
            &effective_vars,
            &results,
            total_elapsed,
            // Linear flows have no terminal state ID -- per issue #257 the
            // field stays absent so audit consumers can distinguish linear
            // and graph runs structurally.
            None,
        );
        stack::write_manifest(&ctx.run_path, &manifest)
            .map_err(|e| eyre!("failed to write manifest.yaml: {e}"))?;

        // Summary -- printed here so CLI behavior stays byte-equivalent.
        let summary = super::build_summary(&results);
        let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
        let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();
        ui::print_flow_complete(
            &summary,
            &format_elapsed(total_elapsed),
            &total_in.to_string(),
            &total_out.to_string(),
            "—",
            &ctx.stack_path.display().to_string(),
        );

        Ok(FlowResult {
            run_id: ctx.run_id.clone(),
            run_path: ctx.run_path.clone(),
            stack_path: ctx.stack_path.clone(),
            flow_name,
            manifest,
            step_results: results,
            total_elapsed,
        })
    }

    fn format_elapsed(d: Duration) -> String {
        let secs = d.as_secs();
        if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{}.{:01}s", secs, d.subsec_millis() / 100)
        }
    }

    /// Set up a graph-flow run and spawn the driver task (issue #240).
    ///
    /// The shape mirrors the linear setup in [`execute_flow`]: synchronous
    /// resolution + agent loading happens here, then a tokio task takes
    /// over to drive the state machine and write the manifest. Any
    /// configuration error returned from this function fails the run
    /// before a `RunHandle` is produced -- consistent with the linear
    /// path's "fast-fail on bad config" promise.
    #[allow(clippy::too_many_arguments)]
    async fn execute_graph_flow_setup(
        graph: config::GraphFlow,
        spec: ExecuteFlowSpec,
        koto_config: Option<&crate::koto_config::KotoConfig>,
        seeds: &Seeds,
        cli_vars: &HashMap<String, String>,
        flow_path: PathBuf,
        flow_contents: String,
        flow_start: Instant,
    ) -> Result<RunHandle> {
        use crate::config::{Defaults, StateKind, load_agent_file_with_seeds};
        use std::sync::Arc;

        // ---- Effective vars: project + CLI override --------------------
        // Mirrors the linear path so `{{vars.X}}` placeholders in the
        // graph's `prompt:` and per-state `task:` substitute identically.
        let mut effective_vars = cli_vars.clone();
        for (k, v) in &spec.vars {
            effective_vars.insert(k.clone(), v.clone());
        }

        // ---- Var substitution in graph prompt + per-state tasks --------
        let mut graph = graph;
        // #258 invariant: external-prompt resolution must run before
        // we get here. The path-aware loader (`load_flow_any_from_path`)
        // is the only entry point that calls this function, and it
        // resolves `prompt_file:` / `task_file:` before returning. The
        // debug_assert is a single safety net at the runtime boundary
        // so a future caller that bypasses the path-aware loader
        // tripps the check in tests instead of silently feeding the
        // runtime an unresolved flow.
        debug_assert!(
            graph.prompt_file.is_none(),
            "graph.prompt_file should be resolved before execute_graph_flow_setup"
        );
        debug_assert!(
            graph.states.values().all(|s| s.task_file.is_none()),
            "every state's task_file should be resolved before execute_graph_flow_setup"
        );
        // ---- Resolve role -> agent_id for every non-terminal state -----
        // Final and human states are skipped (no agent runs there). A
        // non-terminal state with no role:, or a role with no binding,
        // aborts the setup with a clear error before any agent file is
        // touched.
        //
        // Issue #259: this used to live AFTER var substitution, but the
        // new `{{roles.<name>}}` substitution needs the resolved map up
        // front. Doing it first does not change the validation contract
        // (the same checks fire, in the same order from the operator's
        // perspective) and lets `substitute_roles` reuse the same map.
        let project_roles = koto_config.map(|c| &c.roles);
        let mut state_to_agent: HashMap<String, String> = HashMap::new();
        for (state_id, state) in &graph.states {
            match state.kind {
                Some(StateKind::Final) => continue,
                Some(StateKind::Human) => continue,
                // Shell states are deterministic command runners with no
                // agent binding -- skip role resolution (issue #310). The
                // graph driver dispatches them through the executor
                // instead of `run_state_via_executor`.
                Some(StateKind::Shell) => continue,
                None => {}
            }
            let role_name = state.role.as_deref().ok_or_else(|| {
                eyre!(
                    "graph state '{state_id}' is non-terminal but has no `role:` -- declare a role or mark the state as `kind: final`"
                )
            })?;
            let project_role = project_roles.and_then(|m| m.get(role_name));
            let agent_id =
                resolver::resolve_role_agent(role_name, None, project_role, &spec.role_overrides)
                    .ok_or_else(|| {
                        eyre!(
                            "graph state '{state_id}' uses role '{role_name}' but no agent is bound -- set a project-config role or pass --role {role_name}=<agent>"
                        )
                    })?;
            state_to_agent.insert(state_id.clone(), agent_id);
        }

        // ---- Build role -> agent_id map for {{roles.X}} substitution --
        // Issue #259. Three sources, in the same precedence as the
        // resolver cascade (CLI override > project config > state's
        // role binding picked up via `state_to_agent`). Roles bound only
        // in the project config are included even when no state uses
        // them, so a top-level prompt can reference any role the project
        // declares without forcing a dummy state.
        let mut roles_map: HashMap<String, String> = HashMap::new();
        if let Some(pr) = project_roles {
            for (role_name, kr) in pr {
                roles_map.insert(role_name.clone(), kr.agent.clone());
            }
        }
        for (state_id, agent_id) in &state_to_agent {
            // state_to_agent is keyed by state_id, but the cascade-resolved
            // value already reflects per-state role + CLI overrides. Look
            // up the role name from the state to key the map by role.
            if let Some(state) = graph.states.get(state_id)
                && let Some(role_name) = state.role.as_deref()
            {
                roles_map.insert(role_name.to_string(), agent_id.clone());
            }
        }
        for ov in &spec.role_overrides {
            if let crate::resolver::RoleOverride::Agent { role, agent } = ov {
                roles_map.insert(role.clone(), agent.clone());
            }
        }

        // ---- Var + role substitution in graph prompt + per-state tasks -
        // Order: vars first (general namespace), then roles (so a role
        // value cannot be re-interpreted as a placeholder). Both happen
        // pre-spawn so AC2 holds: an unknown role in any prompt aborts
        // the setup before agents launch.
        if let Some(prompt) = graph.prompt.as_mut() {
            *prompt = super::flow_api::substitute_vars(prompt, &effective_vars)?;
            *prompt = super::flow_api::substitute_roles(prompt, &roles_map, "graph prompt")?;
        }
        for (state_id, state) in graph.states.iter_mut() {
            if let Some(task) = state.task.as_mut() {
                *task = super::flow_api::substitute_vars(task, &effective_vars)?;
                let ctx = format!("state '{state_id}'");
                *task = super::flow_api::substitute_roles(task, &roles_map, &ctx)?;
            }
            // Shell-state commands (issue #310) get the same var
            // substitution as `task:`. Roles do not apply -- a shell
            // command does not address an agent. Substituting now keeps
            // the runtime free of var-aware string handling on the
            // shell-dispatch path.
            if let Some(command) = state.command.as_mut() {
                *command = super::flow_api::substitute_vars(command, &effective_vars)?;
            }
        }

        // ---- Resolve top-level task (CLI -t > graph.prompt) ------------
        let task_with_vars = spec
            .task
            .as_deref()
            .map(|t| super::flow_api::substitute_vars(t, &effective_vars))
            .transpose()?;
        let task_with_roles = task_with_vars
            .as_deref()
            .map(|t| super::flow_api::substitute_roles(t, &roles_map, "-t task"))
            .transpose()?;
        let resolved_task = match (task_with_roles, graph.prompt.clone()) {
            (Some(t), _) => t,
            (None, Some(p)) => p,
            (None, None) => String::new(),
        };

        // ---- Load each unique agent ------------------------------------
        // The graph format does not declare `defaults:`, so we use the
        // crate-wide implicit defaults (claude-sonnet-4-5 / claude-cli)
        // when the agent file does not pin its own. Project-config tiers
        // are honoured because `load_agent_file_with_seeds` consults
        // `koto_config` for tier resolution.
        let defaults = Defaults {
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
        };
        let mut agents_by_id: HashMap<String, config::Agent> = HashMap::new();
        let mut agent_origins: HashMap<String, usize> = HashMap::new();
        let mut agent_hashes: HashMap<String, String> = HashMap::new();
        for agent_id in state_to_agent.values() {
            if agents_by_id.contains_key(agent_id) {
                continue;
            }
            let (agent, origin, sha) =
                load_agent_file_with_seeds(seeds, agent_id, &defaults, koto_config)?;
            agent_origins.insert(agent_id.clone(), origin);
            agent_hashes.insert(agent_id.clone(), sha);
            agents_by_id.insert(agent_id.clone(), agent);
        }

        // ---- Guide / rules cache (skills are not declared on graph yet)
        let agents_vec: Vec<config::Agent> = agents_by_id.values().cloned().collect();
        let guide = super::load_guide_from_seeds(seeds).map_err(|e| eyre!("{e}"))?;
        let rules_cache = super::load_rules_for_agents_with_seeds(&agents_vec, seeds)
            .map_err(|e| eyre!("{e}"))?;

        // ---- RunContext + on-disk run layout ---------------------------
        // Graph flows do not declare a `stack:` block in v1, so we fall
        // back to the implicit per-project stack root. Same algorithm the
        // linear runner uses when its `stack.path` is empty.
        let stack_path = resolve_stack_path("");
        let ctx = RunContext::new(
            graph.name.clone(),
            resolved_task,
            stack_path,
            guide,
            rules_cache,
            HashMap::new(),
            effective_vars.clone(),
        );
        stack::init_run_layout(&ctx.run_path)
            .map_err(|e| eyre!("failed to create run dir: {e}"))?;

        // Run banner mirroring the linear path so the visual rhythm of a
        // graph run matches a linear run (#266). `step_count` is reported
        // as the state count -- not byte-equivalent to a linear flow's
        // step count (graphs revisit), but it is the closest analogue
        // until #269 redesigns the graph-aware UI.
        let display_path_str = flow_path.display().to_string();
        ui::print_flow_start(
            &graph.name,
            &display_path_str,
            graph.states.len(),
            agents_by_id.len(),
        );
        super::try_print_issue_banner(&effective_vars);

        // ---- Spawn driver task -----------------------------------------
        let state = Arc::new(RunState::default());
        let task_state = Arc::clone(&state);
        let run_id = ctx.run_id.clone();
        let run_path = ctx.run_path.clone();
        let stack_path_for_handle = ctx.stack_path.clone();
        let flow_name_for_handle = graph.name.clone();
        let flow_name = graph.name.clone();
        // Clone the seeds list so the spawned task owns its copy --
        // tokio::spawn requires `'static` futures, and the caller's
        // borrow does not satisfy that.
        let seeds_owned = seeds.clone();

        let join: JoinHandle<Result<FlowResult>> = tokio::spawn(async move {
            // The state arc carries cancellation; honour it before
            // spawning the first agent so a caller that flipped the flag
            // between handle creation and task start does not pay for
            // a wasted state-step.
            if task_state.is_cancelled() {
                return Err(eyre!("run cancelled before graph driver started"));
            }
            let outcome =
                super::graph::run_graph_flow(&graph, &agents_by_id, &state_to_agent, &ctx).await?;
            let total_elapsed = flow_start.elapsed();
            let super::graph::GraphRunOutcome {
                steps: results,
                final_state,
            } = outcome;

            // Manifest: reuse the linear builder so `kuro show-output`
            // and `read_run` see the same shape regardless of flow
            // type. Resolved roles are empty for graph flows in this
            // prototype -- a richer audit lands with the role/state
            // resolution pass. `final_state` is populated for graph
            // runs so audit consumers can tell terminal states (`done`
            // vs `aborted`) apart without parsing stderr (issue #257).
            let manifest = build_manifest(
                &ctx,
                &flow_name,
                &flow_path,
                &flow_contents,
                &seeds_owned,
                &agents_vec,
                &agent_origins,
                &agent_hashes,
                &[],
                &effective_vars,
                &results,
                total_elapsed,
                Some(final_state.as_str()),
            );
            stack::write_manifest(&ctx.run_path, &manifest)
                .map_err(|e| eyre!("failed to write manifest.yaml: {e}"))?;

            // Reuse the linear summary table so a graph run ends with the
            // same per-step recap a linear run does (#266). Token totals
            // are 0 today because the graph driver does not collect usage
            // metadata yet -- mirrors the linear path's "—" treatment when
            // a backend declines to report tokens. #269 will design a
            // graph-native summary that also visualises the path taken.
            let summary = super::build_summary(&results);
            let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
            let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();
            ui::print_flow_complete(
                &summary,
                &format_elapsed(total_elapsed),
                &total_in.to_string(),
                &total_out.to_string(),
                "—",
                &ctx.stack_path.display().to_string(),
            );

            Ok(FlowResult {
                run_id: ctx.run_id.clone(),
                run_path: ctx.run_path.clone(),
                stack_path: ctx.stack_path.clone(),
                flow_name,
                manifest,
                step_results: results,
                total_elapsed,
            })
        });

        Ok(RunHandle {
            run_id,
            run_path,
            stack_path: stack_path_for_handle,
            flow_name: flow_name_for_handle,
            state,
            join,
        })
    }

    /// Test-only constructors for the in-tree MCP session module. We do not
    /// want a public ctor for `ActiveRouter` -- production code reaches it
    /// only through [`RunHandle::active_router`] -- but the session tests
    /// need to register and observe accessors without spinning up a real
    /// flow run. Gated on `#[cfg(test)]` so it never enters release builds.
    #[cfg(test)]
    pub mod test_support {
        use super::{ActiveRouter, RouterAccessor, RunState};
        use std::sync::Arc;

        /// `ActiveRouter` over a fresh, empty `RunState`. `current()` always
        /// returns `None` until something publishes via `set_router`.
        pub fn fresh_active_router() -> ActiveRouter {
            ActiveRouter {
                state: Arc::new(RunState::default()),
            }
        }

        /// `ActiveRouter` plus the `RouterAccessor` that has been published
        /// onto the same shared state. `current()` resolves to a clone of
        /// the accessor for as long as the state is alive; the returned
        /// accessor is the original sender and can be used to send through
        /// the same channel as `current()` does.
        pub fn active_router_with_published() -> (ActiveRouter, RouterAccessor) {
            let state = Arc::new(RunState::default());
            let (accessor, _rx) = RouterAccessor::new();
            state.set_router(accessor.clone());
            // The receiver is dropped here, so `inject_human_message` would
            // fail with `Closed`; tests that need a live channel build
            // their own pair. The published accessor is enough to make
            // `ActiveRouter::current()` resolve to `Some(...)`, which is
            // what the session-state tests assert on.
            (ActiveRouter { state }, accessor)
        }
    }
}

// Public library API entry points. Several of these (FlowResult, RunHandle,
// RouterAccessor, RouterAccessorError) are forward-looking API for the MCP
// server work tracked in #199 -- the current CLI wrapper does not name them
// directly, so the unused-import lint fires. Allow it: dropping them would
// shrink the surface promised by #209.
#[allow(unused_imports)]
pub use flow_api::{
    ActiveRouter, ExecuteFlowSpec, FlowResult, FlowSource, RouterAccessor, RouterAccessorError,
    RunHandle, execute_flow,
};

// Crate-internal re-exports so the CLI tests (and any other in-tree caller)
// can reach the orchestration helpers without poking through a private
// module path. Kept separate from the public `pub use` above so the public
// surface stays focused on the library entry point. The bin build does not
// reach for these directly (only the test build does) -- silence the lint.
#[allow(unused_imports)]
pub(crate) use flow_api::{
    apply_resolved_roles_to_steps, apply_role_agent_overrides, build_manifest, resolve_flow_path,
    resolve_stack_path, resolve_stack_path_for_flow_name, resolve_task, substitute_placeholders,
    substitute_roles, substitute_vars, verify_flow_step_ids,
};

// Test-only re-export so in-tree consumers (notably the MCP session module)
// reach helpers via `runner::test_support::...`. Production builds never see
// this module; gating mirrors the inner `#[cfg(test)] pub mod test_support`.
#[cfg(test)]
pub use flow_api::test_support;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_millis() {
        assert_eq!(
            format_duration(std::time::Duration::from_millis(450)),
            "450ms"
        );
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs_f64(3.2)),
            "3.2s"
        );
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(125)),
            "2m05s"
        );
    }

    #[test]
    fn backend_name_values() {
        assert_eq!(backend_name(Backend::Api), "api");
        assert_eq!(backend_name(Backend::ClaudeCli), "claude-cli");
        assert_eq!(backend_name(Backend::Codex), "codex");
        assert_eq!(backend_name(Backend::Ollama), "ollama");
    }

    #[test]
    fn build_summary_maps_fields() {
        let results = vec![StepRunResult {
            step_id: "design".to_string(),
            agent_name: "Levi".to_string(),
            backend: backend_name(Backend::Api).to_string(),
            duration: std::time::Duration::from_secs(5),
            tokens_in: Some(1200),
            tokens_out: Some(800),
            output_file: "dev-20260421-105200/01-design.md".to_string(),
            print_output: false,
            record: StepRecord {
                step_id: "design".to_string(),
                kind: "llm".to_string(),
                agent: Some("Levi".to_string()),
                model_requested: Some("claude-sonnet-4-5".to_string()),
                model_actual: Some("claude-sonnet-4-5".to_string()),
                backend: "api".to_string(),
                tokens_in: Some(1200),
                tokens_out: Some(800),
                duration_ms: 5000,
                started_at: "2026-04-21T10:52:00Z".to_string(),
                exit_code: 0,
                input_steps: vec![],
                output_file: "01-design.md".to_string(),
                participants: Vec::new(),
                turns: None,
                messages: None,
                terminated_by: None,
                graph_decision: None,
            },
        }];
        let summary = build_summary(&results);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].id, "design");
        assert_eq!(summary[0].backend, "api");
        assert_eq!(summary[0].tokens_in, "1200");
    }

    #[test]
    fn build_system_prompt_with_all_parts() {
        let agent = Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            description: None,
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust-developer".to_string()],
            skills: vec!["error-handling".to_string()],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        };
        let guide = Some("Project guide content".to_string());
        let mut rules_cache = HashMap::new();
        rules_cache.insert(
            "rust-developer".to_string(),
            "Rust rules content".to_string(),
        );
        let mut skills_cache = HashMap::new();
        skills_cache.insert(
            "error-handling".to_string(),
            "Error handling skill content".to_string(),
        );

        let prompt = build_system_prompt(&agent, &guide, &rules_cache, &skills_cache);
        assert!(prompt.starts_with("Project guide content"));
        assert!(prompt.contains("Rust rules content"));
        assert!(prompt.contains("Error handling skill content"));
        assert!(prompt.ends_with("You are a developer"));
    }

    #[test]
    fn build_system_prompt_multiple_rules() {
        let agent = Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            description: None,
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust".to_string(), "cli-ux".to_string()],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        };
        let guide = None;
        let mut rules_cache = HashMap::new();
        rules_cache.insert("rust".to_string(), "Rust rules".to_string());
        rules_cache.insert("cli-ux".to_string(), "CLI UX rules".to_string());
        let skills_cache = HashMap::new();

        let prompt = build_system_prompt(&agent, &guide, &rules_cache, &skills_cache);
        assert!(prompt.contains("Rust rules"));
        assert!(prompt.contains("CLI UX rules"));
        // Rules should come before role
        let rust_pos = prompt.find("Rust rules").unwrap();
        let cli_pos = prompt.find("CLI UX rules").unwrap();
        let role_pos = prompt.find("You are a developer").unwrap();
        assert!(rust_pos < cli_pos);
        assert!(cli_pos < role_pos);
    }

    #[test]
    fn build_system_prompt_without_guide_or_rules() {
        let agent = Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            description: None,
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec![],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        };
        let guide = None;
        let rules_cache = HashMap::new();
        let skills_cache = HashMap::new();

        let prompt = build_system_prompt(&agent, &guide, &rules_cache, &skills_cache);
        assert_eq!(prompt, "You are a developer");
    }

    #[test]
    fn load_guide_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_guide(dir.path()).is_none());
    }

    #[test]
    fn load_guide_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let guide_path = dir.path().join("Guide.md");
        std::fs::write(&guide_path, "# My Project\nContext here").unwrap();
        let content = load_guide(dir.path()).unwrap();
        assert!(content.contains("My Project"));
    }

    #[test]
    fn load_guide_for_task_skips_guide_by_default() {
        // Regression for #245: `kuro task` and `kuro chat` must NOT inject the
        // cwd-project's Guide.md into agent system prompts. Even with a Guide
        // sitting in the first seed, the gate returns None when the
        // include-project-context flag is off.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Guide.md"),
            "You are working on **kuromaku**...",
        )
        .unwrap();
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: crate::koto_config::SeedSource::Local {
                    display: dir.path().display().to_string(),
                    path: dir.path().to_path_buf(),
                },
            }],
        };
        let guide = load_guide_for_task(&seeds, false).unwrap();
        assert!(
            guide.is_none(),
            "kuro task must skip cwd Guide by default; got: {guide:?}"
        );
    }

    #[test]
    fn task_system_prompt_omits_cwd_guide_by_default() {
        // Regression for #245 -- the full leak path. Assemble the system
        // prompt the way `kuro task` does (load_guide_for_task ->
        // build_system_prompt) and assert the cwd Guide content does NOT
        // appear when the user has not opted in via
        // --include-project-context. The assertion targets the project name
        // the issue explicitly names ("kuromaku") so a future regression that
        // re-introduces unconditional Guide injection will fail this test
        // with the exact symptom from the bug report.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Guide.md"),
            "You are working on **kuromaku** -- a CLI tool for reproducible AI agents.",
        )
        .unwrap();
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: crate::koto_config::SeedSource::Local {
                    display: dir.path().display().to_string(),
                    path: dir.path().to_path_buf(),
                },
            }],
        };
        let agent = Agent {
            id: "neo".to_string(),
            name: "Neo".to_string(),
            title: None,
            description: None,
            role: "You are Neo, a Prompt Engineer.".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec![],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        };
        let guide = load_guide_for_task(&seeds, false).unwrap();
        let prompt = build_system_prompt(&agent, &guide, &HashMap::new(), &HashMap::new());
        assert!(
            !prompt.contains("kuromaku"),
            "kuro task system prompt must not name the cwd project; got:\n{prompt}"
        );
        assert!(
            prompt.contains("You are Neo"),
            "agent role must still be present: {prompt}"
        );
    }

    #[test]
    fn task_system_prompt_includes_guide_when_opted_in() {
        // Symmetric to the regression test above: when the user explicitly
        // opts in via --include-project-context, the Guide loads and the
        // system prompt looks exactly like a flow run's prompt would. Keeps
        // the opt-in path covered so a future change that breaks it surfaces
        // here next to the regression test.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Guide.md"),
            "You are working on **kuromaku** -- a CLI tool for reproducible AI agents.",
        )
        .unwrap();
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: crate::koto_config::SeedSource::Local {
                    display: dir.path().display().to_string(),
                    path: dir.path().to_path_buf(),
                },
            }],
        };
        let agent = Agent {
            id: "neo".to_string(),
            name: "Neo".to_string(),
            title: None,
            description: None,
            role: "You are Neo, a Prompt Engineer.".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec![],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        };
        let guide = load_guide_for_task(&seeds, true).unwrap();
        let prompt = build_system_prompt(&agent, &guide, &HashMap::new(), &HashMap::new());
        assert!(
            prompt.contains("kuromaku"),
            "opt-in must inject the Guide: {prompt}"
        );
        assert!(
            prompt.starts_with("You are working on"),
            "Guide must lead the cascade: {prompt}"
        );
    }

    #[test]
    fn load_guide_for_task_loads_when_opted_in() {
        // Symmetric to the above: when the user explicitly opts in via
        // `--include-project-context`, the Guide loads exactly as in flow runs.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Guide.md"),
            "You are working on **kuromaku**...",
        )
        .unwrap();
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: crate::koto_config::SeedSource::Local {
                    display: dir.path().display().to_string(),
                    path: dir.path().to_path_buf(),
                },
            }],
        };
        let guide = load_guide_for_task(&seeds, true).unwrap();
        assert_eq!(guide.as_deref(), Some("You are working on **kuromaku**..."));
    }

    #[test]
    fn load_rules_for_agents_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("rust-developer.md"), "Use iterators").unwrap();

        let agents = vec![Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            description: None,
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust-developer".to_string()],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        }];

        let cache = load_rules_for_agents(&agents, dir.path()).unwrap();
        assert_eq!(cache.get("rust-developer").unwrap(), "Use iterators");
    }

    #[test]
    fn load_rules_for_agents_multiple_rules() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("rust.md"), "Rust rules").unwrap();
        std::fs::write(rules_dir.join("cli.md"), "CLI rules").unwrap();

        let agents = vec![Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            description: None,
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust".to_string(), "cli".to_string()],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        }];

        let cache = load_rules_for_agents(&agents, dir.path()).unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("rust").unwrap(), "Rust rules");
        assert_eq!(cache.get("cli").unwrap(), "CLI rules");
    }

    #[test]
    fn load_rules_for_agents_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents = vec![Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            description: None,
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["nonexistent".to_string()],
            skills: vec![],
            env: HashMap::new(),
            extra_args: HashMap::new(),
        }];

        let err = load_rules_for_agents(&agents, dir.path()).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn llm_output_filename_format() {
        // Issue #31 layout: NN-<step>.md, two-digit zero-padded.
        assert_eq!(llm_output_filename(1, "design"), "01-design.md");
        assert_eq!(llm_output_filename(12, "review"), "12-review.md");
    }

    #[test]
    fn post_comment_target_deserializes_from_yaml() {
        use crate::config::PostCommentTarget;

        // Both string variants round-trip from the YAML form used in flows.
        let yaml_pr = "post_comment: pr\n";
        let yaml_issue = "post_comment: issue\n";

        #[derive(serde::Deserialize)]
        struct Wrap {
            post_comment: PostCommentTarget,
        }

        let pr: Wrap = serde_yaml::from_str(yaml_pr).unwrap();
        assert_eq!(pr.post_comment, PostCommentTarget::Pr);
        let issue: Wrap = serde_yaml::from_str(yaml_issue).unwrap();
        assert_eq!(issue.post_comment, PostCommentTarget::Issue);
    }

    #[test]
    fn shell_output_filename_uses_txt_extension() {
        // .txt rather than .md so termimad doesn't try to render shell output
        // as markdown when print_output: true is set on a shell step.
        // Layout: NN-<step>.txt under the run directory (issue #31).
        let name = shell_output_filename(1, "fetch");
        assert_eq!(name, "01-fetch.txt");
    }

    #[test]
    fn unique_run_path_uses_base_when_free() {
        // Happy path: nothing on disk yet, so the base name is taken verbatim
        // and the timestamp stays human-readable.
        let dir = tempfile::tempdir().unwrap();
        let (id, path) = unique_run_path(dir.path(), "review-20260429-100000");
        assert_eq!(id, "review-20260429-100000");
        assert_eq!(path, dir.path().join("review-20260429-100000"));
    }

    #[test]
    fn unique_run_path_bumps_suffix_when_directory_exists() {
        // Two `kuro run` calls in the same wall-clock second must not collide.
        // The first creates `<base>`, the second falls back to `<base>-2`,
        // and so on. Without this, the second run silently overwrites the
        // first run's outputs and the audit trail is destroyed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("review-20260429-100000")).unwrap();
        std::fs::create_dir_all(dir.path().join("review-20260429-100000-2")).unwrap();

        let (id, path) = unique_run_path(dir.path(), "review-20260429-100000");
        assert_eq!(id, "review-20260429-100000-3");
        assert_eq!(path, dir.path().join("review-20260429-100000-3"));
        assert!(
            !path.exists(),
            "unique_run_path must return a path that doesn't exist yet"
        );
    }

    /// Build a minimal RunContext for shell-step tests that don't need a real
    /// LLM stack. The temp dir's path is the stack path so `write_step` can
    /// land artifacts somewhere predictable.
    fn shell_test_ctx(stack_path: PathBuf) -> RunContext {
        RunContext::new(
            "test-flow".to_string(),
            "irrelevant for shell steps".to_string(),
            stack_path,
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )
    }

    fn shell_step(id: &str, command: &str) -> crate::config::Step {
        crate::config::Step {
            id: id.to_string(),
            agent: String::new(),
            role: None,
            task: None,
            run: Some(command.to_string()),
            input: vec![],
            needs: vec![],
            model: None,
            backend: None,
            print_output: false,
            post_comment: None,
            agents: Vec::new(),
            max_turns: None,
            turn_timeout: None,
            extra_args: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn shell_step_captures_stdout_to_stack() {
        // Acceptance: stdout is captured as the step output and written to
        // the run directory same as LLM outputs (issue #31).
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx(dir.path().to_path_buf());
        let executor = executor::create_executor();
        let step = shell_step("greet", "echo hello-from-shell");

        let result = run_shell_step(executor.as_ref(), &step, &ctx, 1, 1, &[])
            .await
            .unwrap();

        assert_eq!(result.step_id, "greet");
        assert_eq!(result.backend, "shell");
        assert!(result.tokens_in.is_none());
        assert!(result.tokens_out.is_none());

        // Run-directory layout: NN-<id>.txt + NN-<id>.meta.yaml. The body is
        // discoverable by step id without a reader needing to know the
        // numbering, so downstream `input:` consumers stay simple.
        let body = stack::read_run_step_content(&ctx.run_path, "greet").unwrap();
        assert_eq!(body, "hello-from-shell");
        // Per-step metadata records that this was a shell step.
        assert_eq!(result.record.kind, "shell");
        assert_eq!(result.record.backend, "shell");
        assert!(result.record.agent.is_none());
    }

    #[tokio::test]
    async fn shell_step_nonzero_exit_aborts_with_clear_error() {
        // Acceptance: non-zero exit fails the step with exit code and stderr
        // included in the error.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx(dir.path().to_path_buf());
        let executor = executor::create_executor();
        let step = shell_step("fail", "echo oops 1>&2; exit 7");

        let err = run_shell_step(executor.as_ref(), &step, &ctx, 1, 1, &[])
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(
            matches!(err, RunError::ExecutorFailed { .. }),
            "expected ExecutorFailed, got: {msg}"
        );
        // The wrapped error message contains the exit code and stderr.
        assert!(msg.contains("7"), "exit code missing from error: {msg}");
        assert!(msg.contains("oops"), "stderr missing from error: {msg}");
    }

    #[tokio::test]
    async fn shell_step_output_consumable_by_downstream_step() {
        // Acceptance: the output of a shell step is available as context to
        // downstream steps via input. We exercise the stack roundtrip that
        // build_user_prompt would use to inject the prior output.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx(dir.path().to_path_buf());
        let executor = executor::create_executor();
        let step = shell_step("fetch", "printf 'diff content'");

        run_shell_step(executor.as_ref(), &step, &ctx, 1, 1, &[])
            .await
            .unwrap();

        // The next step would call stack::read_run_step_content("fetch") via
        // build_user_prompt -- mirror that read here so the test matches the
        // live consumer path.
        let prior = stack::read_run_step_content(&ctx.run_path, "fetch").unwrap();
        assert_eq!(prior, "diff content");
    }

    // --- run-ID stack layout (issue #31) ---

    #[test]
    fn run_id_format_is_flow_then_timestamp() {
        // Acceptance: run-ID is `<flow>-<YYYYMMDD-HHmmss>`. The flow name and
        // the timestamp segment are visible in the produced id; the run_path
        // is `<stack_path>/<run_id>`.
        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "review".to_string(),
            "task".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );

        assert!(
            ctx.run_id.starts_with("review-"),
            "run_id should start with flow name: {}",
            ctx.run_id
        );
        // Trailing segment is YYYYMMDD-HHmmss = 8+1+6 = 15 chars.
        let suffix = &ctx.run_id["review-".len()..];
        assert_eq!(suffix.len(), 15, "got: {suffix}");
        assert!(
            suffix.chars().nth(8) == Some('-'),
            "missing date/time separator: {suffix}"
        );

        assert_eq!(ctx.run_path, dir.path().join(&ctx.run_id));
    }

    #[tokio::test]
    async fn build_user_prompt_reads_prior_step_from_run_dir() {
        // Acceptance: build_user_prompt resolves `input:` against the per-run
        // directory layout. We seed the run dir via the public stack helpers
        // -- same code path the runner uses.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx(dir.path().to_path_buf());
        let rec = stack::StepRecord {
            step_id: "fetch".to_string(),
            kind: "shell".to_string(),
            agent: None,
            model_requested: None,
            model_actual: None,
            backend: "shell".to_string(),
            tokens_in: None,
            tokens_out: None,
            duration_ms: 5,
            started_at: ctx.started_at.to_rfc3339(),
            exit_code: 0,
            input_steps: vec![],
            output_file: stack::step_content_filename(1, "fetch", "txt"),
            participants: Vec::new(),
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: None,
        };
        stack::write_run_step(&ctx.run_path, 1, &rec, "PR diff goes here").unwrap();

        let downstream = crate::config::Step {
            id: "review".to_string(),
            agent: "Bella".to_string(),
            role: None,
            task: Some("Review the diff".to_string()),
            run: None,
            input: vec!["fetch".to_string()],
            needs: vec![],
            model: None,
            backend: None,
            print_output: false,
            post_comment: None,
            agents: Vec::new(),
            max_turns: None,
            turn_timeout: None,
            extra_args: HashMap::new(),
        };

        let prompt = build_user_prompt("Top-level task", &downstream, &ctx.run_path).unwrap();
        // Top-level task and per-step task are both present.
        assert!(prompt.contains("Top-level task"));
        assert!(prompt.contains("Your task: Review the diff"));
        // Prior step body is spliced in.
        assert!(prompt.contains("PR diff goes here"));
        assert!(prompt.contains("Output from step 'fetch'"));
    }

    #[tokio::test]
    async fn run_steps_writes_per_step_files_and_meta_in_run_dir() {
        // Acceptance: every kuro run creates a run directory with NN-<step>.md
        // (or .txt) and NN-<step>.meta.yaml per step. Two shell steps are
        // enough to exercise the numbering and the input-handoff.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx(dir.path().to_path_buf());

        let step_one = shell_step("fetch", "printf 'one'");
        let mut step_two = shell_step("collect", "printf 'two'");
        // collect depends on fetch so we exercise the input read on the new
        // layout while we are at it.
        step_two.input = vec!["fetch".to_string()];
        let steps = vec![&step_one, &step_two];

        let results = run_steps(&steps, &[], &ctx).await.unwrap();
        assert_eq!(results.len(), 2);

        // Step content files live under `steps/` and are zero-padded and
        // numbered by topo order (issue #31, fixed in issue #159).
        let steps_dir = ctx.run_path.join("steps");
        assert!(steps_dir.join("01-fetch.txt").exists());
        assert!(steps_dir.join("01-fetch.meta.yaml").exists());
        assert!(steps_dir.join("02-collect.txt").exists());
        assert!(steps_dir.join("02-collect.meta.yaml").exists());

        // Regression for #159: nothing must land directly in the run root.
        assert!(!ctx.run_path.join("01-fetch.txt").exists());
        assert!(!ctx.run_path.join("01-fetch.meta.yaml").exists());

        // Empty `messages/` dir created at run start (#153 prep, #159).
        assert!(
            ctx.run_path.join("messages").is_dir(),
            "messages/ must exist at run start"
        );

        // Meta yaml is parseable as a StepRecord. `output_file` is just the
        // filename -- the `steps/` segment is added by readers/writers.
        let meta = std::fs::read_to_string(steps_dir.join("01-fetch.meta.yaml")).unwrap();
        let parsed: stack::StepRecord = serde_yaml::from_str(&meta).unwrap();
        assert_eq!(parsed.step_id, "fetch");
        assert_eq!(parsed.kind, "shell");
        assert_eq!(parsed.output_file, "01-fetch.txt");

        // The summary's output_file is `<run_id>/steps/NN-<id>.<ext>` so
        // print_output joins it with stack_path correctly.
        assert!(
            results[0].output_file.starts_with(&ctx.run_id),
            "got: {}",
            results[0].output_file
        );
        assert!(results[0].output_file.ends_with("/steps/01-fetch.txt"));
    }

    #[tokio::test]
    async fn run_steps_step_started_at_differs_between_steps() {
        // Acceptance criterion (issue #159): per-step `started_at` reflects
        // each step's actual wall-clock start, not the run's start. Without
        // this, every step in the manifest collapses onto `ctx.started_at`
        // and the audit promise -- "when did this step run?" -- breaks.
        //
        // Two consecutive shell steps run sequentially, each takes some
        // real time, so their captured `started_at` strings must differ.
        let dir = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx(dir.path().to_path_buf());

        let step_one = shell_step("first", "printf 'a'");
        let step_two = shell_step("second", "printf 'b'");
        let steps = vec![&step_one, &step_two];

        let results = run_steps(&steps, &[], &ctx).await.unwrap();
        assert_eq!(results.len(), 2);

        let started_one = &results[0].record.started_at;
        let started_two = &results[1].record.started_at;
        assert_ne!(
            started_one, started_two,
            "per-step started_at must differ across steps; got both = {started_one}"
        );
        // Neither should equal the run's start time -- otherwise we'd be
        // back to the bug the test guards against.
        let run_started = ctx.started_at.to_rfc3339();
        // The first step starts very close to the run start, but capturing
        // it independently must still produce a distinct nanosecond reading.
        assert_ne!(
            started_one, &run_started,
            "step 1 started_at must come from chrono::Utc::now() at step start, not ctx.started_at"
        );
    }

    // --- Conversation transcript rendering (issue #170) ---

    #[test]
    fn render_transcript_includes_participants_and_finals() {
        use crate::messaging::router::{LogEntry, LogKind, MessageKind, Source, TerminationReason};

        let entries = vec![
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: "I propose option A.".to_string(),
                    message: MessageKind::Final,
                },
            },
            LogEntry {
                from: Source::Agent("Mika".to_string()),
                kind: LogKind::Inbound {
                    content: "I disagree, option B is safer.".to_string(),
                    message: MessageKind::Final,
                },
            },
        ];
        let participants = vec!["Levi".to_string(), "Mika".to_string()];

        let out = render_transcript(&entries, &TerminationReason::MaxTurns, &participants);

        assert!(out.starts_with("# Conversation transcript\n"), "got: {out}");
        assert!(
            out.contains("Participants: Levi, Mika"),
            "missing participants line: {out}"
        );
        assert!(out.contains("## Levi"), "missing Levi heading: {out}");
        assert!(out.contains("## Mika"), "missing Mika heading: {out}");
        assert!(out.contains("I propose option A."));
        assert!(out.contains("option B is safer."));
        // Termination footer uses the stable Display string, not the
        // Debug variant identifier. Variant renames in source must not
        // mutate historical transcript text.
        assert!(
            out.contains("Termination: max_turns"),
            "expected stable Display string 'max_turns', got: {out}"
        );
        assert!(
            !out.contains("MaxTurns"),
            "transcript must not leak the Debug/variant name: {out}"
        );
    }

    #[test]
    fn render_transcript_skips_partials_and_outbound() {
        use crate::messaging::router::{LogEntry, LogKind, MessageKind, Source, TerminationReason};

        let entries = vec![
            // Streaming partial -- must not appear in transcript.
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: "thinking...".to_string(),
                    message: MessageKind::Partial,
                },
            },
            // Outbound delivery -- duplicates inbound, must be skipped.
            LogEntry {
                from: Source::Router,
                kind: LogKind::Outbound {
                    to: "Mika".to_string(),
                    content: "I propose option A.".to_string(),
                },
            },
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: "Final answer.".to_string(),
                    message: MessageKind::Final,
                },
            },
        ];
        let participants = vec!["Levi".to_string(), "Mika".to_string()];

        let out = render_transcript(&entries, &TerminationReason::Convergence, &participants);

        assert!(
            !out.contains("thinking..."),
            "partial fragments must be skipped: {out}"
        );
        assert!(
            !out.contains("I propose option A."),
            "outbound deliveries must be skipped (they duplicate inbound text): {out}"
        );
        assert!(out.contains("Final answer."));
    }

    #[test]
    fn count_agent_turns_only_counts_final_inbound() {
        // Acceptance: per-agent `turns` in meta.yaml counts canonical
        // results only. Tool-use, partials, outbound deliveries, send
        // failures, and other agents' messages must not contribute.
        use crate::messaging::router::{LogEntry, LogKind, MessageKind, Source};
        let entries = vec![
            // Final by Levi -> counts.
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: "first".to_string(),
                    message: MessageKind::Final,
                },
            },
            // Partial by Levi -> ignored.
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: "thinking".to_string(),
                    message: MessageKind::Partial,
                },
            },
            // Tool-use by Levi -> ignored (not a turn).
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: String::new(),
                    message: MessageKind::ToolUse {
                        name: "read_file".to_string(),
                    },
                },
            },
            // Final by Mika -> ignored when counting Levi.
            LogEntry {
                from: Source::Agent("Mika".to_string()),
                kind: LogKind::Inbound {
                    content: "rebut".to_string(),
                    message: MessageKind::Final,
                },
            },
            // Outbound delivery -> not an agent emission.
            LogEntry {
                from: Source::Router,
                kind: LogKind::Outbound {
                    to: "Levi".to_string(),
                    content: "first".to_string(),
                },
            },
            // Final by Levi #2 -> counts.
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: "second".to_string(),
                    message: MessageKind::Final,
                },
            },
        ];

        assert_eq!(count_agent_turns(&entries, "Levi"), 2);
        assert_eq!(count_agent_turns(&entries, "Mika"), 1);
        assert_eq!(count_agent_turns(&entries, "Nobody"), 0);
    }

    #[test]
    fn participant_stat_serializes_to_meta_yaml() {
        // Acceptance: `meta.yaml` carries per-agent rows; non-conversation
        // steps stay backward-compatible (no `participants:` key emitted).
        let convo = stack::StepRecord {
            step_id: "debate".to_string(),
            kind: "conversation".to_string(),
            agent: None,
            model_requested: None,
            model_actual: None,
            backend: "claude-cli".to_string(),
            tokens_in: None,
            tokens_out: None,
            duration_ms: 1234,
            started_at: "2026-04-30T16:00:00Z".to_string(),
            exit_code: 0,
            input_steps: vec![],
            output_file: "01-debate.md".to_string(),
            participants: vec![
                stack::ParticipantStat {
                    agent: "Levi".to_string(),
                    model: "claude-sonnet-4-5".to_string(),
                    turns: 3,
                    tokens_in: None,
                    tokens_out: None,
                },
                stack::ParticipantStat {
                    agent: "Mika".to_string(),
                    model: "claude-opus-4-5".to_string(),
                    turns: 2,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            // Conversation summary fields (#172): aggregate turns, total
            // messages logged, termination reason. Asserted below.
            turns: Some(5),
            messages: Some(7),
            terminated_by: Some("convergence".to_string()),
            graph_decision: None,
        };
        let yaml = serde_yaml::to_string(&convo).unwrap();
        assert!(
            yaml.contains("participants:"),
            "conversation meta must include participants: {yaml}"
        );
        assert!(yaml.contains("agent: Levi"));
        assert!(yaml.contains("turns: 3"));
        assert!(yaml.contains("agent: Mika"));
        assert!(yaml.contains("turns: 2"));
        // #172 manifest summary surfaces alongside participants.
        assert!(
            yaml.contains("messages: 7"),
            "conversation meta must carry total messages: {yaml}"
        );
        assert!(
            yaml.contains("terminated_by: convergence"),
            "conversation meta must carry termination reason: {yaml}"
        );

        // Backend uses the stable schema vocabulary, not "conversation".
        assert!(
            yaml.contains("backend: claude-cli"),
            "audit backend must stick to api/claude-cli/codex/ollama/shell: {yaml}"
        );

        // Non-conversation step has no participants key (skip_serializing_if).
        let llm = stack::StepRecord {
            step_id: "design".to_string(),
            kind: "llm".to_string(),
            agent: Some("Levi".to_string()),
            model_requested: Some("claude-sonnet-4-5".to_string()),
            model_actual: Some("claude-sonnet-4-5".to_string()),
            backend: "claude-cli".to_string(),
            tokens_in: None,
            tokens_out: None,
            duration_ms: 100,
            started_at: "2026-04-30T16:00:00Z".to_string(),
            exit_code: 0,
            input_steps: vec![],
            output_file: "01-design.md".to_string(),
            participants: vec![],
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: None,
        };
        let yaml = serde_yaml::to_string(&llm).unwrap();
        assert!(
            !yaml.contains("participants"),
            "non-conversation steps must omit participants for backward-compat: {yaml}"
        );
        // #172: the new conversation summary fields must also stay out of
        // non-conversation meta.yaml so existing audit consumers see no
        // schema drift.
        assert!(
            !yaml.contains("terminated_by"),
            "non-conversation steps must omit terminated_by: {yaml}"
        );
        assert!(
            !yaml.contains("messages:"),
            "non-conversation steps must omit messages summary: {yaml}"
        );
    }

    #[test]
    fn render_transcript_renders_tool_use_and_send_failures() {
        use crate::messaging::router::{LogEntry, LogKind, MessageKind, Source, TerminationReason};

        let entries = vec![
            LogEntry {
                from: Source::Agent("Levi".to_string()),
                kind: LogKind::Inbound {
                    content: String::new(),
                    message: MessageKind::ToolUse {
                        name: "read_file".to_string(),
                    },
                },
            },
            LogEntry {
                from: Source::Router,
                kind: LogKind::SendFailed {
                    to: "Mika".to_string(),
                    error: "transport closed".to_string(),
                },
            },
        ];
        let participants = vec!["Levi".to_string(), "Mika".to_string()];

        let out = render_transcript(&entries, &TerminationReason::Timeout, &participants);

        assert!(
            out.contains("_Levi used tool: read_file_"),
            "tool-use must render as italic note: {out}"
        );
        assert!(
            out.contains("failed to deliver to Mika") && out.contains("transport closed"),
            "send-failure must surface in transcript: {out}"
        );
    }

    // --- Human input (issue #171) ---

    /// Acceptance #171: human messages appear in the transcript with the
    /// `from: "user"` identifier (not `"human"` or any other label). The
    /// mapping flows through `Source::Display`, so this test pins the
    /// rendered string in the audit log.
    #[test]
    fn render_transcript_renders_human_as_user() {
        use crate::messaging::router::{LogEntry, LogKind, MessageKind, Source, TerminationReason};

        let entries = vec![LogEntry {
            from: Source::Human,
            kind: LogKind::Inbound {
                content: "focus on tests".to_string(),
                message: MessageKind::Final,
            },
        }];
        let participants = vec!["Levi".to_string()];

        let out = render_transcript(&entries, &TerminationReason::HumanClosed, &participants);

        assert!(
            out.contains("## user\n"),
            "human input must render under `## user` heading: {out}"
        );
        assert!(out.contains("focus on tests"));
        assert!(
            !out.contains("## human\n"),
            "transcript must not use the legacy `human` label: {out}"
        );
    }

    /// Sanity for the test helper: a sequence of lines arrives on the
    /// receiver in order, EOF closes the channel.
    #[tokio::test]
    async fn spawn_line_reader_forwards_lines() {
        let input = b"hello\nworld\n".to_vec();
        let mut rx = spawn_line_reader(std::io::Cursor::new(input));

        assert_eq!(rx.recv().await.as_deref(), Some("hello"));
        assert_eq!(rx.recv().await.as_deref(), Some("world"));
        // EOF: sender drops, channel closes.
        assert!(rx.recv().await.is_none(), "EOF must close the channel");
    }

    /// Empty / whitespace-only lines must not surface as messages -- they
    /// would inject zero-information content into the conversation and
    /// confuse agents. A stray Enter is the most likely cause.
    #[tokio::test]
    async fn spawn_line_reader_skips_empty_lines() {
        let input = b"\n   \n\t\nfirst\n\nsecond\n".to_vec();
        let mut rx = spawn_line_reader(std::io::Cursor::new(input));

        assert_eq!(rx.recv().await.as_deref(), Some("first"));
        assert_eq!(rx.recv().await.as_deref(), Some("second"));
        assert!(rx.recv().await.is_none());
    }

    /// EOF must drop the sender so `Router::run` can exit with
    /// `TerminationReason::HumanClosed` instead of hanging waiting for more
    /// input. This wires the line reader's contract to the router's
    /// termination logic without spawning the router itself.
    #[tokio::test]
    async fn spawn_line_reader_closes_channel_on_eof() {
        let input = b"only line\n".to_vec();
        let mut rx = spawn_line_reader(std::io::Cursor::new(input));

        // Drain the message.
        assert_eq!(rx.recv().await.as_deref(), Some("only line"));
        // Next recv returns None: closed channel signals HumanClosed to the
        // router.
        assert!(
            rx.recv().await.is_none(),
            "channel must close on EOF so the router can terminate cleanly"
        );
    }

    // --- {{roles.X}} substitution (issue #259) -------------------------

    fn roles_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn substitute_roles_replaces_namespaced_placeholders() {
        let roles = roles_map(&[("architect", "Levi"), ("developer", "Kai")]);
        let result = substitute_roles(
            "{{roles.architect}} just produced a design plan; {{roles.developer}} will implement.",
            &roles,
            "flow prompt",
        )
        .unwrap();
        assert_eq!(
            result,
            "Levi just produced a design plan; Kai will implement."
        );
    }

    #[test]
    fn substitute_roles_repeated_placeholder_replaces_all() {
        let roles = roles_map(&[("architect", "Levi")]);
        let result = substitute_roles(
            "{{roles.architect}} and again {{roles.architect}}",
            &roles,
            "flow prompt",
        )
        .unwrap();
        assert_eq!(result, "Levi and again Levi");
    }

    #[test]
    fn substitute_roles_leaves_vars_namespace_alone() {
        // The vars regex and the roles regex are siblings -- neither must
        // touch the other's namespace.
        let roles = roles_map(&[("architect", "Levi")]);
        let result =
            substitute_roles("Issue {{vars.id}} for {{roles.architect}}", &roles, "ctx").unwrap();
        assert_eq!(result, "Issue {{vars.id}} for Levi");
    }

    #[test]
    fn substitute_roles_no_placeholders_passes_through() {
        let roles = roles_map(&[("architect", "Levi")]);
        let result = substitute_roles("plain text without placeholders", &roles, "ctx").unwrap();
        assert_eq!(result, "plain text without placeholders");
    }

    #[test]
    fn substitute_roles_unknown_role_errors_with_clear_message() {
        // AC2: unknown role aborts before any agent runs. The error must
        // name the role and the context (state / step / prompt).
        let roles = roles_map(&[("architect", "Levi")]);
        let err = substitute_roles(
            "{{roles.reviewer}} should weigh in",
            &roles,
            "state 'review'",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown role"), "got: {msg}");
        assert!(msg.contains("reviewer"), "got: {msg}");
        assert!(msg.contains("state 'review'"), "got: {msg}");
        // Operator hint surfaces the fix path.
        assert!(msg.contains("--role"), "got: {msg}");
    }

    #[test]
    fn substitute_roles_collects_multiple_unknown_roles() {
        // One pass, one error -- mirrors substitute_vars' behavior so
        // operators do not have to fix-and-rerun in a loop.
        let roles = roles_map(&[("architect", "Levi")]);
        let err = substitute_roles(
            "{{roles.foo}} and {{roles.bar}} and {{roles.architect}}",
            &roles,
            "flow prompt",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("foo"), "got: {msg}");
        assert!(msg.contains("bar"), "got: {msg}");
        // The known role does not get reported as missing.
        assert!(!msg.contains("architect"), "got: {msg}");
        assert!(msg.contains("roles"), "got: {msg}");
    }

    #[test]
    fn substitute_roles_honors_cli_role_override_via_apply_cascade() {
        // Cascade integration: --role architect=Vera must beat the
        // project-config binding `architect: Levi` so `{{roles.architect}}`
        // substitutes Vera. Mirrors the order the linear path uses --
        // apply_role_agent_overrides first, substitute second.
        use crate::config::{Defaults, FlowConfig, StackConfig};
        use crate::resolver::RoleOverride;

        let mut flow = FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
            prompt: Some("{{roles.architect}} kicks off".to_string()),
            defaults: Defaults {
                model: "claude-sonnet-4-5".to_string(),
                backend: Backend::ClaudeCli,
            },
            // Project-config binding -- the loader writes this into
            // flow_config.roles before the runner gets it.
            roles: roles_map(&[("architect", "Levi")]),
            steps: Vec::new(),
            stack: StackConfig {
                backend: "local".to_string(),
                path: "/tmp/test-stack".to_string(),
            },
        };
        let overrides = vec![RoleOverride::Agent {
            role: "architect".to_string(),
            agent: "Vera".to_string(),
        }];

        apply_role_agent_overrides(&mut flow, None, &overrides);
        let prompt = flow.prompt.as_deref().unwrap();
        let substituted = substitute_roles(prompt, &flow.roles, "flow prompt").unwrap();

        assert_eq!(substituted, "Vera kicks off");
    }

    #[test]
    fn substitute_roles_ignores_dotted_non_role_namespaces() {
        // `{{agents.X.model}}` is explicitly out of scope for #259. The
        // helper must leave it untouched so a future namespace can land
        // without colliding.
        let roles = roles_map(&[("architect", "Levi")]);
        let result =
            substitute_roles("{{agents.Levi.model}} {{roles.architect}}", &roles, "ctx").unwrap();
        assert_eq!(result, "{{agents.Levi.model}} Levi");
    }
}
