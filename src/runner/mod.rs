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
    /// Pre-rendered overlay summaries keyed by role name (issue #364).
    /// Filled by the runner setup after `apply_role_overlays`; consumed by
    /// the step-banner construction sites to surface the overlay
    /// contribution next to the model/backend cells. Empty when no role
    /// in the flow had overlays, in which case the banner is byte-
    /// identical to today's output.
    pub overlay_summaries: HashMap<String, String>,
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
            overlay_summaries: HashMap::new(),
        }
    }

    /// Construct a [`RunContext`] for a resumed run (issue #338).
    ///
    /// Differs from [`RunContext::new`] in three ways:
    /// 1. The caller supplies the existing `run_id` / `run_path` /
    ///    `started_at` instead of generating fresh values. The pause /
    ///    resume contract says the run keeps its original identity --
    ///    same directory under `~/.koto/stacks/<project>/`, same
    ///    timestamp on the manifest -- so that operators see one run,
    ///    not two related ones.
    /// 2. No call to [`unique_run_path`]: the directory already exists
    ///    on disk from the original run.
    /// 3. No directory creation: layout was set up at the original
    ///    `kuro run`. Resume reuses it verbatim so step files from
    ///    before the pause survive next to the ones written after.
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        flow_name: String,
        task: String,
        stack_path: PathBuf,
        run_id: String,
        run_path: PathBuf,
        started_at: chrono::DateTime<chrono::Utc>,
        guide: Option<String>,
        rules_cache: HashMap<String, String>,
        skills_cache: HashMap<String, String>,
        template_vars: HashMap<String, String>,
    ) -> Self {
        Self {
            run_id,
            flow_name,
            task,
            stack_path,
            run_path,
            started_at,
            guide,
            rules_cache,
            skills_cache,
            template_vars,
            poster: github::gh_poster(),
            overlay_summaries: HashMap::new(),
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

        // #364: pull the overlay summary for this step's role from
        // RunContext, populated by the runner setup right after
        // `apply_role_overlays`. Direct-agent steps (no `role:`) and
        // steps whose role had no overlays produce `None`, which
        // `print_step_banner` suppresses entirely.
        let overlay_summary = step
            .role
            .as_deref()
            .and_then(|r| ctx.overlay_summaries.get(r).cloned());
        let step_info = StepInfo {
            id: step.id.clone(),
            agent: agent.name.clone(),
            title: agent.title.clone(),
            model: effective_model.to_string(),
            backend: effective_backend,
            input: step.input.clone(),
            state: StepState::Running,
            overlay_summary,
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
                // Try .yaml first, then .md (issue #320: markdown flow format).
                let rel_yaml = std::path::Path::new("flows").join(format!("{name}.yaml"));
                if let Some((_, path)) = seeds
                    .find(&rel_yaml)
                    .map_err(|e| eyre!("{}", e.message()))?
                {
                    return Ok(path);
                }
                let rel_md = std::path::Path::new("flows").join(format!("{name}.md"));
                match seeds.find(&rel_md).map_err(|e| eyre!("{}", e.message()))? {
                    Some((_, path)) => Ok(path),
                    None => Err(eyre!(
                        "{}\n\nhint: create flows/{name}.yaml (or .md) in one of the seeds, or use --file <path>",
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
                        if !(name_str.ends_with(".yaml")
                            || name_str.ends_with(".yml")
                            || name_str.ends_with(".md"))
                        {
                            continue;
                        }
                        let bare = name_str
                            .trim_end_matches(".yaml")
                            .trim_end_matches(".yml")
                            .trim_end_matches(".md")
                            .to_string();
                        // YAML takes precedence over .md for the same bare name
                        by_name
                            .entry(bare)
                            .or_insert_with(|| flows_dir.join(name_str.as_ref()));
                    }
                }
                if by_name.is_empty() {
                    return Err(eyre!(
                        "no flows found in seeds: {}\n\nhint: create flows/<name>.yaml (or .md) in one of the seed directories",
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
                let ids: Vec<String> = graph.graph.keys().cloned().collect();
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

    /// Summary of what an overlay contributed to a single role's bound
    /// agent. Used for the run banner ("overlays: rules+=2, model") and
    /// the audit ("[resolve] overlays: rules+=2, model"). Issue #364.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct OverlayApplied {
        /// Number of overlay rules that were appended after dedup.
        pub rule_delta: usize,
        pub model_replaced: bool,
        pub backend_replaced: bool,
        /// Backend keys whose `extra_args` were replaced. Ordered by the
        /// `Backend` enum's natural order so the summary is deterministic.
        pub extra_args_backends: Vec<Backend>,
    }

    impl OverlayApplied {
        /// One-line summary used in both the banner and the audit. Returns
        /// `None` when nothing was applied -- callers suppress the line.
        pub(crate) fn summary(&self) -> Option<String> {
            let mut parts: Vec<String> = Vec::new();
            if self.rule_delta > 0 {
                parts.push(format!("rules+={}", self.rule_delta));
            }
            if self.model_replaced {
                parts.push("model".to_string());
            }
            if self.backend_replaced {
                parts.push("backend".to_string());
            }
            if !self.extra_args_backends.is_empty() {
                let names: Vec<&str> = self
                    .extra_args_backends
                    .iter()
                    .map(|b| match b {
                        Backend::Api => "api",
                        Backend::ClaudeCli => "claude-cli",
                        Backend::Codex => "codex",
                        Backend::Ollama => "ollama",
                    })
                    .collect();
                parts.push(format!("extra_args[{}]", names.join(",")));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(", "))
            }
        }
    }

    /// Layer the project-level role overlays onto the seed agents
    /// (issue #364).
    ///
    /// `roles_in_use` is the list of `(role_name, agent_id)` pairs the
    /// caller wants overlayed -- typically every role referenced by the
    /// current flow. For each pair we:
    ///   1. Look up the project config's `overlays:` block for the role.
    ///   2. If non-empty, merge it onto the seed agent in `agents` whose
    ///      `id` matches `agent_id`. `model`/`backend` replace; `rules`
    ///      append-then-dedup; `extra_args` replace per backend key.
    ///   3. Record an [`OverlayApplied`] keyed by role name so the
    ///      runner can surface the summary in the banner and audit.
    ///
    /// v1 constraint: if the same `agent_id` is bound to more than one
    /// role and the per-role overlays differ, we return a validation
    /// error. This keeps the mutate-in-place model honest -- supporting
    /// "same agent, different overlays per role" would require a per-
    /// binding effective agent map, which is deferred until a real use
    /// case turns up.
    pub(crate) fn apply_role_overlays(
        agents: &mut [config::Agent],
        roles_in_use: &[(String, String)],
        koto_config: Option<&crate::koto_config::KotoConfig>,
    ) -> std::result::Result<HashMap<String, OverlayApplied>, String> {
        use crate::koto_config::RoleOverlay;

        // No project config => no overlays to apply.
        let Some(kc) = koto_config else {
            return Ok(HashMap::new());
        };

        // Collect per-agent overlays first so we can catch the v1 collision
        // case before mutating anything. Empty overlays are skipped so a
        // role without an `overlays:` block does not collide with one that
        // has them.
        let mut overlays_by_agent: HashMap<String, Vec<(&str, &RoleOverlay)>> = HashMap::new();
        for (role_name, agent_id) in roles_in_use {
            let Some(kr) = kc.roles.get(role_name) else {
                continue;
            };
            if kr.overlays.is_empty() {
                continue;
            }
            overlays_by_agent
                .entry(agent_id.clone())
                .or_default()
                .push((role_name.as_str(), &kr.overlays));
        }
        // Collision rule (#364 v1): if two roles bind the same agent_id
        // with non-identical overlays, refuse. The exit ramp is documented
        // in the v1 design notes -- a future "per-binding effective agent"
        // refactor lifts the restriction without changing YAML.
        for (agent_id, entries) in &overlays_by_agent {
            if entries.len() < 2 {
                continue;
            }
            let first_overlay = entries[0].1;
            for (other_role, other_overlay) in entries.iter().skip(1) {
                if other_overlay != &first_overlay {
                    // Sort role names so the error message is deterministic
                    // -- otherwise HashMap iteration order would leak.
                    let mut role_names: Vec<&str> = entries.iter().map(|(r, _)| *r).collect();
                    role_names.sort();
                    return Err(format!(
                        "agent '{agent_id}' is bound to roles {} with differing overlays \
(roles: {}, conflict on '{other_role}') -- the v1 overlay model mutates the agent in place. \
Either drop overlays on one binding or fork the agent into separate IDs.",
                        role_names.join(", "),
                        role_names.join(", ")
                    ));
                }
            }
        }

        // Apply overlays. Each unique agent gets mutated at most once --
        // when multiple roles bind the same agent with identical overlays,
        // we apply the overlay once and surface the OverlayApplied summary
        // for every role that asked for it.
        let mut applied: HashMap<String, OverlayApplied> = HashMap::new();
        let mut mutated: HashSet<String> = HashSet::new();
        let agents_by_id: HashMap<String, usize> = agents
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.clone(), i))
            .collect();

        for (agent_id, entries) in &overlays_by_agent {
            let Some(&idx) = agents_by_id.get(agent_id) else {
                // Agent wasn't loaded -- typically because the flow does
                // not reference the role. Skip silently; the overlay was
                // never going to apply to a running step.
                continue;
            };
            let overlay = entries[0].1;
            let agent = &mut agents[idx];

            let mut summary = OverlayApplied {
                rule_delta: 0,
                model_replaced: false,
                backend_replaced: false,
                extra_args_backends: Vec::new(),
            };

            if !mutated.contains(agent_id) {
                if let Some(ref m) = overlay.model {
                    agent.model = m.clone();
                    summary.model_replaced = true;
                }
                if let Some(b) = overlay.backend {
                    agent.backend = crate::resolver::project_backend_to_runtime(b);
                    summary.backend_replaced = true;
                }
                // Per-backend replace -- mirrors AC4 "extra_args.codex
                // replaces the seed agent's extra_args.codex list entirely
                // (no token-level merge)".
                if !overlay.extra_args.is_empty() {
                    let mut backends: Vec<Backend> = overlay.extra_args.keys().copied().collect();
                    // Deterministic order so the banner summary is stable.
                    backends.sort_by_key(|b| match b {
                        Backend::Api => 0,
                        Backend::ClaudeCli => 1,
                        Backend::Codex => 2,
                        Backend::Ollama => 3,
                    });
                    summary.extra_args_backends = backends.clone();
                    for b in backends {
                        let val = overlay.extra_args.get(&b).cloned().unwrap_or_default();
                        agent.extra_args.insert(b, val);
                    }
                }
                // Append + dedup-by-name preserving seed order (AC2, AC7).
                if !overlay.rules.is_empty() {
                    let mut seen: HashSet<String> = agent.rules.iter().cloned().collect();
                    let mut added = 0usize;
                    for r in &overlay.rules {
                        if seen.insert(r.clone()) {
                            agent.rules.push(r.clone());
                            added += 1;
                        }
                    }
                    summary.rule_delta = added;
                }
                mutated.insert(agent_id.clone());
            } else {
                // Already mutated by an earlier role binding with identical
                // overlays. Re-derive the summary so every role gets one
                // even though the mutation happened once.
                if overlay.model.is_some() {
                    summary.model_replaced = true;
                }
                if overlay.backend.is_some() {
                    summary.backend_replaced = true;
                }
                if !overlay.extra_args.is_empty() {
                    let mut backends: Vec<Backend> = overlay.extra_args.keys().copied().collect();
                    backends.sort_by_key(|b| match b {
                        Backend::Api => 0,
                        Backend::ClaudeCli => 1,
                        Backend::Codex => 2,
                        Backend::Ollama => 3,
                    });
                    summary.extra_args_backends = backends;
                }
                if !overlay.rules.is_empty() {
                    // We do not know how many new rules survived dedup for
                    // this second-round role, but the agent rule list is
                    // already final -- so we approximate by counting
                    // overlay rules that landed in the agent's final list
                    // beyond what the seed had. Cheaper and more honest:
                    // record the overlay's rule count, which is what the
                    // user wrote. The banner is informational, not load-
                    // bearing.
                    summary.rule_delta = overlay.rules.len();
                }
            }
            for (role_name, _) in entries {
                applied.insert(role_name.to_string(), summary.clone());
            }
        }

        Ok(applied)
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

    /// Pause metadata threaded from the graph driver into [`build_manifest`]
    /// (issue #337).
    ///
    /// Constructed only on the [`super::graph::GraphRunOutcome::Paused`] arm
    /// of `execute_graph_flow_setup`; absent for linear runs and for graph
    /// runs that reached a `kind: final` state. Field semantics mirror the
    /// manifest fields they populate so the wiring stays one-to-one.
    pub(crate) struct PauseRecord {
        pub paused_at_state: String,
        /// RFC3339 string -- the manifest stores a string for symmetry with
        /// `started_at` / `finished_at`, so the conversion happens here
        /// once at the boundary.
        pub paused_at: String,
        /// SHA-256 hex of the referenced GitHub issue body. Best-effort:
        /// `None` when no `id` var is set, when `gh` is unavailable, or
        /// when the issue lookup fails. The pause itself does not depend
        /// on it.
        pub issue_body_sha256: Option<String>,
    }

    /// Build the run manifest. Pure -- no I/O happens here; the caller is
    /// expected to write the result to `<run_path>/manifest.yaml`.
    ///
    /// Format-agnostic: the inputs are already parsed (`FlowConfig` /
    /// `GraphFlow` materialise upstream) and the only path-shaped values
    /// the builder consumes are `flow_path` plus `flow_contents`, which it
    /// stores verbatim and hashes. The same workflow expressed as YAML or
    /// Markdown therefore produces a structurally identical manifest --
    /// only `flow_path`, `flow_sha256`, and the flow's own
    /// `ResourceRecord` differ by construction. This contract is locked
    /// in by `tests::manifest_structure_identical_for_yaml_and_md_sources`
    /// (issue #329); do not branch on `flow_path.extension()` here.
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
        pause: Option<PauseRecord>,
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

        // Lifecycle fields: a paused run records the pause shape on the
        // manifest so `kuro resume` (#338) can read it back. A non-paused
        // run leaves all four absent so existing manifests keep their
        // bytes -- the `skip_serializing_if` on each field locks the
        // back-compat contract on disk; tests pin the wire string.
        let (status, paused_at_state, paused_at, paused_issue_body_sha256) = match pause {
            Some(p) => (
                Some("paused".to_string()),
                Some(p.paused_at_state),
                Some(p.paused_at),
                p.issue_body_sha256,
            ),
            None => (None, None, None, None),
        };

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
            status,
            paused_at_state,
            paused_at,
            paused_issue_body_sha256,
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
        let (mut agents, agent_origins, agent_hashes) =
            config::load_agents_for_flow_with_seeds(&seeds, &flow_config, koto_config.as_ref())?;

        // #364: apply project-level role overlays right after agents load
        // and before role resolution. This way the resolver, the rules
        // loader, and the manifest all see the overlay-mutated values
        // automatically -- overlays sit one layer above the agent file and
        // one layer below the CLI/`--role` override surface.
        let roles_in_use: Vec<(String, String)> = flow_config
            .steps
            .iter()
            .filter_map(|s| s.role.clone().map(|r| (r, s.agent.clone())))
            .collect();
        let overlays_by_role =
            apply_role_overlays(&mut agents, &roles_in_use, koto_config.as_ref())
                .map_err(|msg| eyre!("{msg}"))?;

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
        // #364: render the overlay summaries into the audit alongside
        // the existing model/backend/extra_args lines so the run record
        // explains every layer that touched the effective agent. Empty
        // map = no overlays, audit is byte-identical to pre-#364.
        let overlay_summary_map: HashMap<String, String> = overlays_by_role
            .iter()
            .filter_map(|(role, applied)| applied.summary().map(|s| (role.clone(), s)))
            .collect();
        let audit_text = format_audit(
            &seeds,
            &resolved_roles,
            &cli_vars_for_audit,
            &overlay_summary_map,
        );
        print_audit(
            &seeds,
            &resolved_roles,
            &cli_vars_for_audit,
            &overlay_summary_map,
        );

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
        let mut ctx = RunContext::new(
            flow_name.clone(),
            resolved_task,
            stack_path.clone(),
            guide,
            rules_cache,
            skills_cache,
            effective_vars.clone(),
        );
        // #364: render once into a role-keyed summary map so the step
        // loop can join it without re-running summary derivation per
        // step. Empty when no role had overlays.
        ctx.overlay_summaries = overlays_by_role
            .iter()
            .filter_map(|(role, applied)| applied.summary().map(|s| (role.clone(), s)))
            .collect();

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
            // Linear flows have no `human: true` states; pause is graph-
            // only (issue #337). The field stays absent on disk so old
            // manifests are byte-equivalent to new ones.
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
        use crate::config::{Defaults, load_agent_file_with_seeds};
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
            graph.graph.values().all(|s| s.task_file.is_none()),
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
        for (state_id, state) in &graph.graph {
            // Skip terminal, human, and shell states -- they have no agent.
            if state.is_final() || state.is_human() || state.is_shell() {
                continue;
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
            if let Some(state) = graph.graph.get(state_id)
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
        for (state_id, state) in graph.graph.iter_mut() {
            if let Some(task) = state.task.as_mut() {
                *task = super::flow_api::substitute_vars(task, &effective_vars)?;
                let ctx = format!("state '{state_id}'");
                *task = super::flow_api::substitute_roles(task, &roles_map, &ctx)?;
            }
            // Shell-state `run:` commands get the same var substitution
            // as `task:`. Roles do not apply -- a shell command does not
            // address an agent.
            if let Some(run_cmd) = state.run.as_mut() {
                *run_cmd = super::flow_api::substitute_vars(run_cmd, &effective_vars)?;
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

        // #364: apply project-level role overlays to the loaded agents.
        // The graph driver looks up agents through `agents_by_id` keyed
        // by agent ID -- because overlays mutate in place, the driver
        // sees the overlay values without further wiring. The role map
        // we build here is one `(role_name, agent_id)` pair per
        // non-terminal state that has a role binding.
        let roles_in_use_graph: Vec<(String, String)> = state_to_agent
            .iter()
            .filter_map(|(state_id, agent_id)| {
                graph
                    .graph
                    .get(state_id)
                    .and_then(|s| s.role.as_deref())
                    .map(|r| (r.to_string(), agent_id.clone()))
            })
            .collect();
        // `agents_by_id` is a HashMap; we need a contiguous &mut slice for
        // `apply_role_overlays`. Move into a Vec, mutate, then rebuild.
        let mut agents_vec_mut: Vec<config::Agent> = agents_by_id.drain().map(|(_, a)| a).collect();
        let overlays_by_role_graph =
            apply_role_overlays(&mut agents_vec_mut, &roles_in_use_graph, koto_config)
                .map_err(|msg| eyre!("{msg}"))?;
        // Rebuild the by-id map so the rest of the setup keeps reading
        // through the same handle.
        let mut agents_by_id: HashMap<String, config::Agent> = HashMap::new();
        for a in agents_vec_mut {
            agents_by_id.insert(a.id.clone(), a);
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
        let mut ctx = RunContext::new(
            graph.name.clone(),
            resolved_task,
            stack_path,
            guide,
            rules_cache,
            HashMap::new(),
            effective_vars.clone(),
        );
        ctx.overlay_summaries = overlays_by_role_graph
            .iter()
            .filter_map(|(role, applied)| applied.summary().map(|s| (role.clone(), s)))
            .collect();
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
            graph.graph.len(),
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
            // `None` -> fresh run, drives from `graph.initial`. Resume
            // is wired through `execute_graph_flow_resume_setup` (#338),
            // which builds its own `ResumeFrom` from the persisted
            // manifest before calling the driver.
            let outcome =
                super::graph::run_graph_flow(&graph, &agents_by_id, &state_to_agent, &ctx, None)
                    .await?;
            let total_elapsed = flow_start.elapsed();
            // Lifecycle outcome decomposition. The graph driver returns
            // either a `Final` (run reached a `kind: final` state) or a
            // `Paused` (run reached a `human: true` state, issue #337).
            // We carry both shapes through the same downstream wiring
            // -- manifest, summary table, FlowResult -- but populate the
            // pause-shaped fields only on the `Paused` arm.
            let (results, final_state, pause) = match outcome {
                super::graph::GraphRunOutcome::Final { steps, final_state } => {
                    (steps, Some(final_state), None)
                }
                super::graph::GraphRunOutcome::Paused {
                    steps,
                    paused_at_state,
                    paused_at,
                } => {
                    // Best-effort issue-body snapshot. Hashed at pause time
                    // so `kuro resume` (#338) can detect mid-pause edits to
                    // the referenced issue. Skipped silently when no `id`
                    // var is set, when `gh` is unavailable, or when the
                    // lookup fails: the pause itself is the contract,
                    // drift detection is a future-facing convenience.
                    let issue_body_sha256 = effective_vars
                        .get("id")
                        .and_then(|s| s.parse::<u64>().ok())
                        .and_then(crate::notify::github::fetch_issue_body)
                        .map(|body| stack::sha256_hex(body.as_bytes()));
                    let pause = PauseRecord {
                        paused_at_state,
                        paused_at: paused_at.to_rfc3339(),
                        issue_body_sha256,
                    };
                    (steps, None, Some(pause))
                }
            };

            // Manifest: reuse the linear builder so `kuro show-output`
            // and `read_run` see the same shape regardless of flow
            // type. Resolved roles are empty for graph flows in this
            // prototype -- a richer audit lands with the role/state
            // resolution pass. `final_state` is populated for runs that
            // reached a terminal state (#257); `pause` is populated for
            // runs that suspended at a human handoff (#337). The two
            // are mutually exclusive at the source -- the driver
            // returns one outcome variant -- so the manifest cannot
            // record both.
            let pause_state_marker = pause.as_ref().map(|p| p.paused_at_state.clone());
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
                final_state.as_deref(),
                pause,
            );
            stack::write_manifest(&ctx.run_path, &manifest)
                .map_err(|e| eyre!("failed to write manifest.yaml: {e}"))?;

            // Summary: paused runs use a distinct headline so an operator
            // tailing the run sees that it suspended rather than completed.
            // The per-step table itself is identical -- a paused run still
            // walked some agent states and the operator wants the same
            // shape recap. Token totals are dropped on pause because they
            // are partial by definition (the run continues on resume).
            let summary = super::build_summary(&results);
            match &pause_state_marker {
                Some(state_id) => {
                    ui::print_flow_paused(
                        &summary,
                        state_id,
                        &ctx.stack_path.display().to_string(),
                    );
                }
                None => {
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
                }
            }

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

    /// Library entry point for `kuro resume <run-id>` (issue #338).
    ///
    /// Re-enters a previously paused graph run: reads the manifest, walks
    /// the same project / seed / agent / role resolution path that the
    /// fresh `kuro run` used, then spawns the graph driver from the
    /// recorded `paused_at_state`.
    ///
    /// Setup-side errors (run-id missing, manifest unreadable, status not
    /// paused, flow file no longer resolvable, agent file gone) surface
    /// synchronously with operator-shaped messages so a `RunHandle` is
    /// never produced for an unresumable run.
    ///
    /// Out of scope for v1 (see #338's IN/OUT block):
    /// * body-hash drift detection (#342)
    /// * human-input plumbing into prior_context (#340)
    /// * timeout enforcement (#343)
    /// * automatic resume triggers (#341 `kuro watch`)
    #[allow(dead_code)]
    pub async fn resume_run(run_id: &str) -> Result<RunHandle> {
        // Production fetcher: shells out to `gh issue view --json comments`.
        // The `_with` variants are the test seam (issue #340 + #360).
        //
        // The binary calls `resume_run_with_input` directly to thread
        // local-input plumbing through (#360); this convenience entry
        // point stays on the public surface for external callers (MCP,
        // SDK) that only need the basic resume contract.
        resume_run_with_input(run_id, crate::notify::github::gh_comments_fetcher(), None).await
    }

    /// Test-seam variant of [`resume_run`] (issue #340).
    ///
    /// Same contract as `resume_run` but lets the caller swap the
    /// GH-comments fetcher. Production wires `gh_comments_fetcher()`;
    /// integration tests inject a closure that returns canned
    /// `IssueComment`s (or a simulated network failure) so the
    /// human-input synthesis path is testable without spawning `gh`.
    ///
    /// Thin wrapper around [`resume_run_with_input`] for callers that
    /// only care about the GH path (existing #340 tests). Passes
    /// `local: None` so the local-input plumbing collapses to the
    /// pre-existing behaviour. Kept on the public surface as a stable
    /// back-compat seam for downstream callers that wired against the
    /// #340 signature; the binary crate itself no longer uses it.
    #[allow(dead_code)]
    pub async fn resume_run_with(
        run_id: &str,
        fetcher: crate::notify::github::CommentsFetcher,
    ) -> Result<RunHandle> {
        resume_run_with_input(run_id, fetcher, None).await
    }

    /// Test-seam variant of [`resume_run`] that additionally accepts a
    /// local human-input source (issue #360).
    ///
    /// `local` carries feedback supplied via `--message`,
    /// `--message-file`, or stdin. Precedence rules:
    ///
    /// 1. Local wins over GH. When both produce a body, the local body
    ///    becomes the synthetic step's content and a `[warn]` line on
    ///    stderr surfaces the conflict so the operator notices.
    /// 2. GH falls through when `local` is `None`. The existing #340
    ///    path runs verbatim.
    /// 3. Neither path producing input AND no GH source (numeric `id` var
    ///    parses, even if comments are empty) raises a hard error so the
    ///    flow stops with an actionable hint instead of silently routing
    ///    to `next[0]`.
    pub async fn resume_run_with_input(
        run_id: &str,
        fetcher: crate::notify::github::CommentsFetcher,
        local: Option<LocalHumanInput>,
    ) -> Result<RunHandle> {
        let flow_start = Instant::now();

        // ---- 1. Resolve project + run path -----------------------------
        // CWD-derived project name -- mirrors `kuro run`. Cross-project
        // resume (`--project <name>`) is intentionally out of scope for
        // v1 (#338); the v1 invocation is `kuro resume <run-id>` from
        // the same project the original run was started in.
        let stack_path = resolve_stack_path("");
        let run_path = stack_path.join(run_id);
        if !run_path.is_dir() {
            return Err(eyre!(
                "run-id '{run_id}' not found under {}\n\nhint: run `ls {}` to see available runs",
                stack_path.display(),
                stack_path.display(),
            ));
        }

        // ---- 2. Read manifest ------------------------------------------
        // No manifest = run is mid-flight (the runner writes the manifest
        // post-loop). Treat it as "in flight, not resumable" rather than
        // "missing": resume can only continue from a clean Paused state,
        // and a still-running run does not have one persisted yet.
        let manifest = match stack::read_manifest(&run_path) {
            Ok(m) => m,
            Err(stack::StackError::Read(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(eyre!(
                    "run '{run_id}' has no manifest yet (still in flight or aborted before pause); cannot resume"
                ));
            }
            Err(e) => return Err(eyre!("failed to read manifest for run '{run_id}': {e}")),
        };

        // ---- 3. Validate manifest is in `Status::Paused` ---------------
        // The wire string `paused` is locked by `manifest_roundtrips_pause_fields_for_human_handoff`
        // in stack.rs -- if it ever drifts, that test fires before this code
        // ships, and the comparison here keeps reading the literal.
        let status = manifest.status.as_deref();
        if status != Some("paused") {
            let actual = status.unwrap_or("done");
            return Err(eyre!(
                "run '{run_id}' has status '{actual}', not 'paused'; cannot resume\n\nhint: `kuro resume` only re-enters paused runs -- a completed run is final"
            ));
        }
        let paused_at_state = manifest.paused_at_state.clone().ok_or_else(|| {
            eyre!(
                "run '{run_id}' is paused but has no `paused_at_state` recorded -- the manifest is incomplete"
            )
        })?;
        // `paused_at` is the cutoff for the human-input fetch (#340).
        // A paused manifest without it is incomplete: the timestamp
        // filter has nothing to compare against, and degrading silently
        // would let stale comments leak in as "human input". Fail loud
        // so the operator surfaces the missing field instead.
        let paused_at = manifest.paused_at.clone().ok_or_else(|| {
            eyre!(
                "run '{run_id}' is paused but has no `paused_at` recorded -- the manifest is incomplete"
            )
        })?;

        // Decode the original `started_at` so the resumed run keeps the
        // same wall-clock identity. RFC3339 by construction (the writer
        // uses `to_rfc3339`); a parse failure here means the manifest is
        // corrupt, which is fail-fast territory.
        let started_at = chrono::DateTime::parse_from_rfc3339(&manifest.started_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .map_err(|e| {
                eyre!("manifest started_at is not RFC3339: {e}; cannot resume run '{run_id}'")
            })?;

        // ---- 4. Re-resolve the flow ------------------------------------
        // Walk the seed cascade for the manifest's `flow_name`. We do
        // NOT verify `manifest.flow_sha256` against the on-disk file
        // contents -- drift detection is #342's job. A flow that was
        // moved or renamed between pause and resume produces an "unknown
        // flow" error here, which is the right surface for v1.
        let koto_config = KotoConfig::load_optional(Path::new("."))?;
        let seeds = koto_config
            .as_ref()
            .map(|c| c.seeds.clone())
            .unwrap_or_else(Seeds::default_local);
        let flow_path = resolve_flow_path(&FlowSource::Name(manifest.flow_name.clone()), &seeds)?;
        let contents = std::fs::read_to_string(&flow_path)?;

        // Linear flows cannot pause (they have no human handoff state),
        // so a paused manifest pointing at a Linear flow means the flow
        // was switched between runs. Reject explicitly rather than
        // panic later when the linear runner sees a paused manifest.
        let flow_loaded = config::load_flow_any_from_path(&flow_path)?;
        let graph = match flow_loaded {
            config::Flow::Graph(g) => g,
            config::Flow::Linear(_) => {
                return Err(eyre!(
                    "run '{run_id}' was paused on graph flow '{}', but the flow at {} is now a linear flow; cannot resume",
                    manifest.flow_name,
                    flow_path.display(),
                ));
            }
        };

        // ---- 5. Hand off to resume setup -------------------------------
        // The setup function builds vars + state_to_agent + agents from
        // the manifest's recorded vars and the project config (re-resolved
        // each resume -- role overrides given on the original run are NOT
        // stored in the manifest as overrides, only the resolved bindings
        // are; this is consistent with how the rest of the system treats
        // project config). It then constructs a `RunContext::resume` that
        // adopts the existing run dir and spawns the driver with
        // `Some(ResumeFrom { state: paused_at_state, ... })`.
        let resume_ctx = ResumeContext {
            run_id: run_id.to_string(),
            run_path,
            started_at,
            paused_at_state,
            paused_at,
            prior_steps: manifest.steps.clone(),
            vars: manifest
                .vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };

        execute_graph_flow_resume_setup(
            graph,
            koto_config.as_ref(),
            &seeds,
            flow_path,
            contents,
            flow_start,
            resume_ctx,
            fetcher,
            local,
        )
        .await
    }

    /// Build the synthetic `kind: human` step record + body for a resumed
    /// run (issue #340).
    ///
    /// Returns `Some((record, body))` only when there is human input to
    /// inject:
    /// * `vars["id"]` resolves to a `u64` issue number, AND
    /// * `fetch_new_comments_since` returns at least one comment.
    ///
    /// All other paths -- missing/non-numeric `id`, fetcher error, no new
    /// comments -- collapse to `None` so the caller skips writing a
    /// synthetic step. This keeps the "empty prior_context is not an
    /// error" acceptance criterion local to one place rather than
    /// scattering soft-fail logic across the resume setup.
    ///
    /// The returned record's `output_file` is `step_content_filename(step_num,
    /// paused_at_state, "md")` so the `prior_context` reader (which infers
    /// the extension from the meta yaml) finds the body on disk. `step_id`
    /// is the paused state ID, so the driver's `skip_pause_once` arm can
    /// reuse the same name as the `human_input_step_id` it threads back
    /// into `prior_state`.
    pub(crate) fn synthesize_human_step(
        step_num: usize,
        paused_at_state: &str,
        paused_at: &str,
        vars: &HashMap<String, String>,
        fetcher: &crate::notify::github::CommentsFetcher,
    ) -> Option<(stack::StepRecord, String)> {
        let id = vars.get("id").and_then(|s| s.parse::<u64>().ok())?;
        let comments = crate::notify::github::fetch_new_comments_since(id, paused_at, fetcher);
        if comments.is_empty() {
            return None;
        }
        let body = crate::notify::github::format_human_input(&comments, paused_at);
        let output_file = stack::step_content_filename(step_num, paused_at_state, "md");
        let record = stack::StepRecord {
            step_id: paused_at_state.to_string(),
            kind: "human".to_string(),
            agent: None,
            model_requested: None,
            model_actual: None,
            // `backend: "human"` keeps the audit shape self-describing
            // (consumers grep `kind == "human"` AND `backend == "human"`
            // when partitioning step types). No agent ran, so leaving
            // it empty would lie about the row.
            backend: "human".to_string(),
            tokens_in: None,
            tokens_out: None,
            duration_ms: 0,
            // Mirror the pause moment so audit consumers can correlate
            // the synthetic record with the manifest's `paused_at`
            // without re-deriving it from the run timeline.
            started_at: paused_at.to_string(),
            exit_code: 0,
            input_steps: Vec::new(),
            output_file,
            participants: Vec::new(),
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: None,
        };
        Some((record, body))
    }

    /// Local human input collected at `kuro resume` invocation time
    /// (issue #360).
    ///
    /// Represents a single body of feedback supplied via `--message`,
    /// `--message-file <path>`, or stdin. Holds the resolved body alongside
    /// a human-readable `source` label so the on-disk synthetic step and
    /// any conflict warning can name the channel the operator used.
    ///
    /// Constructed exclusively by [`collect_local_human_input`] -- the
    /// invariants (non-empty body, sensible source label) live there so
    /// the resume pipeline can treat any `Some(LocalHumanInput)` as
    /// already-validated.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct LocalHumanInput {
        /// Resolved body. Already trimmed of a trailing newline if one was
        /// present on stdin or in the file, so [`crate::notify::github::format_local_human_input`]
        /// does not double-print and the synthetic step's body shape is
        /// stable regardless of where it came from.
        pub body: String,
        /// Human-readable label for the channel: `"--message"`, `"stdin"`,
        /// or `"--message-file <path>"`. Lands in the synthetic step's
        /// header and in the local-vs-GH conflict warning so auditors can
        /// tell which channel produced the body.
        pub source: String,
    }

    /// Build the synthetic `kind: human` step record + body from a local
    /// input source (issue #360).
    ///
    /// Sibling of [`synthesize_human_step`] for the case where the operator
    /// supplied feedback through `--message`, `--message-file`, or stdin
    /// instead of GitHub comments. Returns `Some((record, body))` for any
    /// non-empty body so the resume pipeline can write the step and route
    /// it as `prior_context` to the next agent.
    ///
    /// Empty bodies collapse to `None`, mirroring [`synthesize_human_step`]'s
    /// "empty input is not an error" contract. `collect_local_human_input`
    /// rejects empty input earlier, but the synthesiser stays consistent so
    /// a future caller that constructs `LocalHumanInput` from a different
    /// path cannot end up with a zero-byte synthetic step on disk.
    pub(crate) fn synthesize_human_step_from_local(
        step_num: usize,
        paused_at_state: &str,
        paused_at: &str,
        local: &LocalHumanInput,
    ) -> Option<(stack::StepRecord, String)> {
        if local.body.is_empty() {
            return None;
        }
        let body =
            crate::notify::github::format_local_human_input(&local.body, &local.source, paused_at);
        let output_file = stack::step_content_filename(step_num, paused_at_state, "md");
        let record = stack::StepRecord {
            step_id: paused_at_state.to_string(),
            kind: "human".to_string(),
            agent: None,
            model_requested: None,
            model_actual: None,
            // Match `synthesize_human_step`: the audit row reads
            // `kind == "human"` AND `backend == "human"` so consumers can
            // partition on either. Leaving backend empty would lie about
            // the row's provenance.
            backend: "human".to_string(),
            tokens_in: None,
            tokens_out: None,
            duration_ms: 0,
            // Mirror the pause moment so audit consumers can correlate
            // the synthetic record with the manifest's `paused_at` without
            // re-deriving it from the run timeline. Exactly the contract
            // `synthesize_human_step` enforces.
            started_at: paused_at.to_string(),
            exit_code: 0,
            input_steps: Vec::new(),
            output_file,
            participants: Vec::new(),
            turns: None,
            messages: None,
            terminated_by: None,
            graph_decision: None,
        };
        Some((record, body))
    }

    /// Carries the manifest-derived inputs the resume setup needs but the
    /// fresh-run setup does not. Fields are owned -- the setup function
    /// moves them into the spawned task and onto the `RunContext::resume`
    /// constructor.
    pub(crate) struct ResumeContext {
        pub run_id: String,
        pub run_path: PathBuf,
        pub started_at: chrono::DateTime<chrono::Utc>,
        pub paused_at_state: String,
        /// RFC3339 timestamp of the original pause, transcribed verbatim
        /// from the manifest. Used as the `>=` cutoff for the human-input
        /// fetch (#340) so only comments added at or after the pause
        /// land in the synthetic `kind: human` step record.
        pub paused_at: String,
        /// Pre-pause step records, hydrated verbatim into the resumed
        /// run's manifest so the `steps:` history spans the full lifecycle
        /// rather than only the post-resume tail.
        pub prior_steps: Vec<stack::StepRecord>,
        /// Effective vars from the manifest (project-config + CLI vars
        /// merged at original-run time). Used to substitute placeholders
        /// in the re-loaded flow and to populate the resumed manifest's
        /// `vars:` field unchanged.
        pub vars: HashMap<String, String>,
    }

    /// Resume sibling of [`execute_graph_flow_setup`].
    ///
    /// Mirrors the fresh-run setup: vars + role substitution, agent load,
    /// guide / rules cache, `RunContext` build, driver spawn. Diverges in
    /// three places only:
    /// 1. Constructs [`RunContext::resume`] (existing dir, original
    ///    `run_id` and `started_at`) instead of [`RunContext::new`].
    /// 2. Passes `Some(ResumeFrom { ... })` to [`super::graph::run_graph_flow`]
    ///    so the driver enters at `paused_at_state` and seeds its step
    ///    counter past the pre-pause artefacts on disk.
    /// 3. Pre-seeds the spawned task's results vector with the manifest's
    ///    pre-pause step records so the resumed manifest's `steps:` list
    ///    is contiguous across the pause boundary.
    ///
    /// Code paths shared with the fresh setup (substitute_vars,
    /// substitute_roles, agent loading via `load_agent_file_with_seeds`,
    /// guide / rules caching) keep their fresh-run home -- duplicated only
    /// where the divergent ResumeContext shape forces it. The two functions
    /// stay readable independently rather than threading a `ResumeMode`
    /// enum through every line; #338's design notes flag the trade-off.
    #[allow(clippy::too_many_arguments)]
    async fn execute_graph_flow_resume_setup(
        graph: config::GraphFlow,
        koto_config: Option<&crate::koto_config::KotoConfig>,
        seeds: &Seeds,
        flow_path: PathBuf,
        flow_contents: String,
        flow_start: Instant,
        mut resume: ResumeContext,
        fetcher: crate::notify::github::CommentsFetcher,
        local: Option<LocalHumanInput>,
    ) -> Result<RunHandle> {
        use crate::config::{Defaults, load_agent_file_with_seeds};
        use std::sync::Arc;

        // Resume invariant: external-prompt resolution must already have
        // run. `load_flow_any_from_path` (the only caller that produces
        // the `graph: GraphFlow` we receive here) folds `prompt_file:` /
        // `task_file:` into the in-memory strings before returning.
        debug_assert!(
            graph.prompt_file.is_none(),
            "graph.prompt_file should be resolved before execute_graph_flow_resume_setup"
        );
        debug_assert!(
            graph.graph.values().all(|s| s.task_file.is_none()),
            "every state's task_file should be resolved before execute_graph_flow_resume_setup"
        );

        // ---- Effective vars: from the manifest (no CLI overrides) ------
        // The resume command does not accept `--var`; the run's vars are
        // pinned to what the original `kuro run` recorded. This is
        // intentional: changing vars mid-pause would silently re-route
        // the run.
        let effective_vars = resume.vars.clone();

        // ---- Resolve role -> agent_id for every non-terminal state -----
        // Same shape as fresh setup. Final / human / shell states are
        // skipped (no agent runs there).
        let project_roles = koto_config.map(|c| &c.roles);
        let mut state_to_agent: HashMap<String, String> = HashMap::new();
        for (state_id, state) in &graph.graph {
            if state.is_final() || state.is_human() || state.is_shell() {
                continue;
            }
            let role_name = state.role.as_deref().ok_or_else(|| {
                eyre!(
                    "graph state '{state_id}' is non-terminal but has no `role:` -- declare a role or mark the state as `kind: final`"
                )
            })?;
            let project_role = project_roles.and_then(|m| m.get(role_name));
            // No CLI overrides on resume: the cascade reduces to
            // project-config + flow defaults. If the project config has
            // changed between pause and resume, the resumed run uses
            // the new bindings -- consistent with how the rest of the
            // system treats project config.
            let agent_id = resolver::resolve_role_agent(role_name, None, project_role, &[])
                .ok_or_else(|| {
                    eyre!(
                        "graph state '{state_id}' uses role '{role_name}' but no agent is bound -- set a project-config role binding"
                    )
                })?;
            state_to_agent.insert(state_id.clone(), agent_id);
        }

        // ---- Build role -> agent_id map for {{roles.X}} substitution ---
        let mut roles_map: HashMap<String, String> = HashMap::new();
        if let Some(pr) = project_roles {
            for (role_name, kr) in pr {
                roles_map.insert(role_name.clone(), kr.agent.clone());
            }
        }
        for (state_id, agent_id) in &state_to_agent {
            if let Some(state) = graph.graph.get(state_id)
                && let Some(role_name) = state.role.as_deref()
            {
                roles_map.insert(role_name.to_string(), agent_id.clone());
            }
        }

        // ---- Var + role substitution in graph prompt + per-state tasks -
        let mut graph = graph;
        if let Some(prompt) = graph.prompt.as_mut() {
            *prompt = substitute_vars(prompt, &effective_vars)?;
            *prompt = substitute_roles(prompt, &roles_map, "graph prompt")?;
        }
        for (state_id, state) in graph.graph.iter_mut() {
            if let Some(task) = state.task.as_mut() {
                *task = substitute_vars(task, &effective_vars)?;
                let ctx = format!("state '{state_id}'");
                *task = substitute_roles(task, &roles_map, &ctx)?;
            }
            if let Some(run_cmd) = state.run.as_mut() {
                *run_cmd = substitute_vars(run_cmd, &effective_vars)?;
            }
        }

        // ---- Top-level task (no `-t` on resume; falls back to prompt) --
        let resolved_task = graph.prompt.clone().unwrap_or_default();

        // ---- Load each unique agent ------------------------------------
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

        // #364: apply project-level role overlays on resume too, so a
        // resumed run sees the same overlay-merged agents as a fresh
        // run. If the project config was edited between pause and
        // resume, the new overlays take effect -- same policy as the
        // rest of the resume code path (`state_to_agent` re-resolves
        // through the current project config).
        let roles_in_use_graph: Vec<(String, String)> = state_to_agent
            .iter()
            .filter_map(|(state_id, agent_id)| {
                graph
                    .graph
                    .get(state_id)
                    .and_then(|s| s.role.as_deref())
                    .map(|r| (r.to_string(), agent_id.clone()))
            })
            .collect();
        let mut agents_vec_mut: Vec<config::Agent> = agents_by_id.drain().map(|(_, a)| a).collect();
        let overlays_by_role_graph =
            apply_role_overlays(&mut agents_vec_mut, &roles_in_use_graph, koto_config)
                .map_err(|msg| eyre!("{msg}"))?;
        let mut agents_by_id: HashMap<String, config::Agent> = HashMap::new();
        for a in agents_vec_mut {
            agents_by_id.insert(a.id.clone(), a);
        }

        // ---- Guide / rules cache ---------------------------------------
        let agents_vec: Vec<config::Agent> = agents_by_id.values().cloned().collect();
        let guide = super::load_guide_from_seeds(seeds).map_err(|e| eyre!("{e}"))?;
        let rules_cache = super::load_rules_for_agents_with_seeds(&agents_vec, seeds)
            .map_err(|e| eyre!("{e}"))?;

        // ---- RunContext: ADOPT the existing run dir --------------------
        // No `unique_run_path`, no `init_run_layout`. The directory was
        // created on the original run; reuse it so step files written
        // before and after the pause share one location.
        let stack_path = resolve_stack_path("");
        let mut ctx = RunContext::resume(
            graph.name.clone(),
            resolved_task,
            stack_path,
            resume.run_id.clone(),
            resume.run_path.clone(),
            resume.started_at,
            guide,
            rules_cache,
            HashMap::new(),
            effective_vars.clone(),
        );
        ctx.overlay_summaries = overlays_by_role_graph
            .iter()
            .filter_map(|(role, applied)| applied.summary().map(|s| (role.clone(), s)))
            .collect();

        // ---- Synthesise human-input step (issues #340 + #360) ----------
        // The driver's `skip_pause_once` arm routes `prior_state` to this
        // step on the first iteration after resume, so the next agent
        // reads human feedback through the same `prior_context` path
        // every other state uses.
        //
        // Two sources can produce a synthetic step:
        //   * Local: `--message`, `--message-file`, or stdin (#360),
        //     carried in `local: Option<LocalHumanInput>`.
        //   * GitHub: comments added to `vars.id` since pause (#340),
        //     fetched via `fetcher` and filtered by `paused_at`.
        //
        // Precedence (high to low):
        //   1. Local wins. Explicit operator intent beats ambient GH
        //      activity. When both produce a body, emit a `[warn]` on
        //      stderr so the conflict is visible but the resume proceeds.
        //   2. GH falls through when `local` is None.
        //   3. Neither path producing a body AND no GH source (no numeric
        //      `vars.id`) raises a hard error so the flow stops with a
        //      hint instead of silently routing to `next[0]`.
        let step_num = resume.prior_steps.len() + 1;
        let gh_id_is_numeric = effective_vars
            .get("id")
            .and_then(|s| s.parse::<u64>().ok())
            .is_some();
        let synth = match local {
            Some(local_input) => {
                // Local wins. Only consult GH to detect the conflict
                // case and warn -- the actual body comes from `local`.
                if gh_id_is_numeric
                    && synthesize_human_step(
                        step_num,
                        &resume.paused_at_state,
                        &resume.paused_at,
                        &effective_vars,
                        &fetcher,
                    )
                    .is_some()
                {
                    eprintln!(
                        "[warn] both local input ({}) and GitHub comments are present; using local input",
                        local_input.source,
                    );
                }
                synthesize_human_step_from_local(
                    step_num,
                    &resume.paused_at_state,
                    &resume.paused_at,
                    &local_input,
                )
            }
            None => synthesize_human_step(
                step_num,
                &resume.paused_at_state,
                &resume.paused_at,
                &effective_vars,
                &fetcher,
            ),
        };

        let human_input_step_id: Option<String> = match synth {
            Some((record, body)) => {
                stack::write_run_step(&resume.run_path, step_num, &record, &body)
                    .map_err(|e| eyre!("failed to persist human-input step: {e}"))?;
                // Keep the manifest's `steps:` history contiguous: the
                // synthetic step appears between pre-pause and
                // post-resume records, exactly as it lives on disk.
                let step_id = record.step_id.clone();
                resume.prior_steps.push(record);
                Some(step_id)
            }
            None => {
                // No body from either source. If there is also no GH
                // source (numeric `vars.id`), the operator has no path
                // to feed feedback into the run -- fail loud so the
                // run does not silently take `next[0]`. The check fires
                // regardless of whether the paused state's `next:`
                // branches on intent; #360 only adds the input mechanism,
                // not the routing logic.
                if !gh_id_is_numeric {
                    return Err(eyre!(
                        "no human input provided for resume of run '{}' paused at '{}'\n\nhint: pass --message \"...\", pipe via stdin, or run from a flow whose vars.id is a GitHub issue number",
                        resume.run_id,
                        resume.paused_at_state,
                    ));
                }
                None
            }
        };

        // Resume banner so the operator sees we adopted an existing run
        // rather than starting a new one. Mirrors `print_command` shape
        // used by the fresh-run path.
        let display_path_str = flow_path.display().to_string();
        ui::print_command(&format!("kuro resume {}", resume.run_id));
        ui::print_run_resume(&resume.run_id, &resume.paused_at_state);
        ui::print_flow_start(
            &graph.name,
            &display_path_str,
            graph.graph.len(),
            agents_by_id.len(),
        );

        // ---- Spawn driver task -----------------------------------------
        let state = Arc::new(RunState::default());
        let task_state = Arc::clone(&state);
        let run_id = ctx.run_id.clone();
        let run_id_for_task = run_id.clone();
        let run_path = ctx.run_path.clone();
        let stack_path_for_handle = ctx.stack_path.clone();
        let flow_name_for_handle = graph.name.clone();
        let flow_name = graph.name.clone();
        let seeds_owned = seeds.clone();
        let paused_state_for_resume = resume.paused_at_state.clone();
        let prior_steps_count = resume.prior_steps.len();
        let prior_steps = resume.prior_steps;

        let join: JoinHandle<Result<FlowResult>> = tokio::spawn(async move {
            if task_state.is_cancelled() {
                return Err(eyre!("run cancelled before graph driver resumed"));
            }
            let resume_from = super::graph::ResumeFrom {
                state: paused_state_for_resume,
                step_num_offset: prior_steps_count,
                human_input_step_id,
            };
            let outcome = super::graph::run_graph_flow(
                &graph,
                &agents_by_id,
                &state_to_agent,
                &ctx,
                Some(resume_from),
            )
            .await?;
            let total_elapsed = flow_start.elapsed();

            // Hydrate the manifest's `steps:` history across the pause
            // boundary: prior records + new records. Synthesised
            // StepRunResults so build_manifest can reuse the same
            // results-driven path it already takes for fresh runs.
            // Fields not on `StepRecord` (e.g. `print_output`) are
            // irrelevant downstream of build_manifest -- the manifest
            // reads `.record.clone()` and totals from `tokens_in/out`.
            let mut all_results: Vec<StepRunResult> = prior_steps
                .into_iter()
                .map(|rec| StepRunResult {
                    step_id: rec.step_id.clone(),
                    agent_name: rec.agent.clone().unwrap_or_default(),
                    backend: rec.backend.clone(),
                    duration: std::time::Duration::from_millis(rec.duration_ms as u64),
                    tokens_in: rec.tokens_in,
                    tokens_out: rec.tokens_out,
                    // Keep a stack-relative path shape consistent with
                    // fresh runs (`<run_id>/<steps>/<filename>`); the
                    // manifest builder does not consume `output_file`
                    // but the sibling fields on `StepRunResult` are
                    // public, so honour the convention.
                    output_file: format!(
                        "{}/{}/{}",
                        run_id_for_task,
                        stack::STEPS_SUBDIR,
                        rec.output_file,
                    ),
                    print_output: false,
                    record: rec,
                })
                .collect();

            let (post_resume_results, final_state, pause) = match outcome {
                super::graph::GraphRunOutcome::Final { steps, final_state } => {
                    (steps, Some(final_state), None)
                }
                super::graph::GraphRunOutcome::Paused {
                    steps,
                    paused_at_state,
                    paused_at,
                } => {
                    // Re-pause: a resumed run can pause again at a
                    // later human state. Re-snapshot the issue body so
                    // a future #342 drift check still has fresh data.
                    let issue_body_sha256 = effective_vars
                        .get("id")
                        .and_then(|s| s.parse::<u64>().ok())
                        .and_then(crate::notify::github::fetch_issue_body)
                        .map(|body| stack::sha256_hex(body.as_bytes()));
                    let pause = PauseRecord {
                        paused_at_state,
                        paused_at: paused_at.to_rfc3339(),
                        issue_body_sha256,
                    };
                    (steps, None, Some(pause))
                }
            };
            all_results.extend(post_resume_results);

            let pause_state_marker = pause.as_ref().map(|p| p.paused_at_state.clone());
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
                &all_results,
                total_elapsed,
                final_state.as_deref(),
                pause,
            );
            // Overwrites the previous Paused manifest at the same path
            // (issue #338's tech note: "overwrite the previous Paused
            // manifest with the new run state").
            stack::write_manifest(&ctx.run_path, &manifest)
                .map_err(|e| eyre!("failed to write manifest.yaml: {e}"))?;

            let summary = super::build_summary(&all_results);
            match &pause_state_marker {
                Some(state_id) => {
                    ui::print_flow_paused(
                        &summary,
                        state_id,
                        &ctx.stack_path.display().to_string(),
                    );
                }
                None => {
                    let total_in: u32 = all_results.iter().filter_map(|r| r.tokens_in).sum();
                    let total_out: u32 = all_results.iter().filter_map(|r| r.tokens_out).sum();
                    ui::print_flow_complete(
                        &summary,
                        &format_elapsed(total_elapsed),
                        &total_in.to_string(),
                        &total_out.to_string(),
                        "—",
                        &ctx.stack_path.display().to_string(),
                    );
                }
            }

            Ok(FlowResult {
                run_id: ctx.run_id.clone(),
                run_path: ctx.run_path.clone(),
                stack_path: ctx.stack_path.clone(),
                flow_name,
                manifest,
                step_results: all_results,
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

    #[cfg(test)]
    mod synth_tests {
        //! Unit tests for `synthesize_human_step` (issue #340).
        //!
        //! Cover the four soft-fail paths called out in the design plan:
        //! missing `id`, fetch error, no new comments, and the happy path
        //! (record + body shape). Lives inside `flow_api` so it can call
        //! the private helper directly without widening visibility.
        use super::synthesize_human_step;
        use crate::notify::github::{CommentsFetcher, IssueComment};
        use std::collections::HashMap;

        fn comment(author: &str, created_at: &str, body: &str) -> IssueComment {
            IssueComment {
                author: author.to_string(),
                created_at: created_at.to_string(),
                body: body.to_string(),
            }
        }

        fn fetcher_returning(comments: Vec<IssueComment>) -> CommentsFetcher {
            Box::new(move |_id: u64| Ok(comments.clone()))
        }

        fn vars_with_id(id: &str) -> HashMap<String, String> {
            let mut v = HashMap::new();
            v.insert("id".to_string(), id.to_string());
            v
        }

        #[test]
        fn returns_none_when_no_id_var() {
            // Acceptance (#340): "no id template var" must collapse to
            // None so the resume continues without a synthetic step.
            let fetcher: CommentsFetcher = Box::new(|_| {
                panic!("fetcher must not be called when there is no id");
            });
            let out =
                synthesize_human_step(2, "ask", "2026-05-07T10:00:00Z", &HashMap::new(), &fetcher);
            assert!(out.is_none());
        }

        #[test]
        fn returns_none_when_id_is_not_numeric() {
            // `--var id=PR-123` is invalid as an issue number; degrade
            // rather than crash.
            let fetcher: CommentsFetcher = Box::new(|_| {
                panic!("fetcher must not be called for non-numeric id");
            });
            let out = synthesize_human_step(
                2,
                "ask",
                "2026-05-07T10:00:00Z",
                &vars_with_id("PR-123"),
                &fetcher,
            );
            assert!(out.is_none());
        }

        #[test]
        fn returns_none_when_no_new_comments() {
            // Acceptance (#340): "If no new comments exist, prior_context
            // is empty (not an error)." The helper must return None so
            // no synthetic step lands on disk and the next agent runs
            // without prior_context.
            let fetcher =
                fetcher_returning(vec![comment("alice", "2026-05-06T09:00:00Z", "stale")]);
            let out = synthesize_human_step(
                2,
                "ask",
                "2026-05-07T10:00:00Z",
                &vars_with_id("139"),
                &fetcher,
            );
            assert!(out.is_none());
        }

        #[test]
        fn returns_none_when_fetcher_errors() {
            // Acceptance (#340): "Network errors fall back to empty
            // prior_context with a warning." The fetch error degrades
            // through `fetch_new_comments_since` to an empty Vec, which
            // here surfaces as None.
            let fetcher: CommentsFetcher = Box::new(|_| Err("simulated outage".to_string()));
            let out = synthesize_human_step(
                2,
                "ask",
                "2026-05-07T10:00:00Z",
                &vars_with_id("139"),
                &fetcher,
            );
            assert!(out.is_none());
        }

        #[test]
        fn returns_record_with_human_kind_and_body_for_one_comment() {
            // Happy path: one comment after the pause produces a record
            // with `kind: "human"`, `agent: None`, `step_id` set to the
            // paused state, and an output_file matching the on-disk
            // naming convention. The body is the formatted human-input
            // block, which the next agent reads via `prior_context`.
            let fetcher = fetcher_returning(vec![comment(
                "alice",
                "2026-05-07T10:30:00Z",
                "looks good, ship it",
            )]);
            let out = synthesize_human_step(
                3,
                "ask",
                "2026-05-07T10:00:00Z",
                &vars_with_id("139"),
                &fetcher,
            )
            .expect("happy path returns Some");
            let (record, body) = out;
            assert_eq!(record.kind, "human");
            assert_eq!(record.step_id, "ask");
            assert_eq!(record.backend, "human");
            assert!(record.agent.is_none());
            assert_eq!(record.output_file, "03-ask.md");
            assert_eq!(record.exit_code, 0);
            assert_eq!(record.duration_ms, 0);
            assert_eq!(record.started_at, "2026-05-07T10:00:00Z");
            assert!(body.contains("Human input received since pause"));
            assert!(body.contains("looks good, ship it"));
            assert!(body.contains("@alice"));
        }

        // --- synthesize_human_step_from_local (issue #360) ---

        use super::{LocalHumanInput, synthesize_human_step_from_local};

        #[test]
        fn local_returns_record_for_non_empty_body() {
            // Happy path: any non-empty local body produces a synthetic
            // step with the same `kind: "human"` / `backend: "human"` shape
            // the GH path emits, so downstream `prior_context` plumbing is
            // source-agnostic.
            let local = LocalHumanInput {
                body: "approve".to_string(),
                source: "--message".to_string(),
            };
            let out = synthesize_human_step_from_local(2, "ask", "2026-05-13T10:00:00Z", &local)
                .expect("non-empty body must return Some");
            let (record, body) = out;
            assert_eq!(record.kind, "human");
            assert_eq!(record.backend, "human");
            assert_eq!(record.step_id, "ask");
            assert!(record.agent.is_none());
            assert_eq!(record.output_file, "02-ask.md");
            assert_eq!(record.exit_code, 0);
            assert_eq!(record.duration_ms, 0);
            assert_eq!(record.started_at, "2026-05-13T10:00:00Z");
            assert!(
                body.contains(
                    "Human input received since pause at 2026-05-13T10:00:00Z (via --message)"
                ),
                "body must carry the source-tagged header, got:\n{body}"
            );
            assert!(body.contains("approve"), "body must carry verbatim text");
        }

        #[test]
        fn local_returns_none_for_empty_body() {
            // Defensive: `collect_local_human_input` rejects empty bodies
            // earlier, but the synthesiser must agree with the GH-side
            // contract that empty input is not an error.
            let local = LocalHumanInput {
                body: String::new(),
                source: "stdin".to_string(),
            };
            let out = synthesize_human_step_from_local(2, "ask", "2026-05-13T10:00:00Z", &local);
            assert!(out.is_none());
        }

        #[test]
        fn local_record_output_file_matches_step_num_filename() {
            // The on-disk filename embeds `step_num` so the synthetic
            // record sorts between pre-pause and post-resume artifacts.
            // Same convention as the GH path -- pin it so downstream
            // listing tools (`kuro show-output`) cannot drift.
            let local = LocalHumanInput {
                body: "looks good".to_string(),
                source: "--message".to_string(),
            };
            let out = synthesize_human_step_from_local(
                7,
                "human-handoff",
                "2026-05-13T10:00:00Z",
                &local,
            )
            .expect("non-empty body returns Some");
            assert_eq!(out.0.output_file, "07-human-handoff.md");
        }

        #[test]
        fn record_output_file_uses_provided_step_num() {
            // The on-disk filename embeds `step_num` so the synthetic
            // record sorts correctly between pre-pause and post-resume
            // step files. `read_run_step_content` keys off the step_id,
            // not the step_num, but downstream tools (`kuro show-output`,
            // listing helpers) walk the directory in name order.
            let fetcher = fetcher_returning(vec![comment("alice", "2026-05-07T10:30:00Z", "ok")]);
            let out = synthesize_human_step(
                12,
                "human-handoff",
                "2026-05-07T10:00:00Z",
                &vars_with_id("139"),
                &fetcher,
            )
            .expect("happy path returns Some");
            assert_eq!(out.0.output_file, "12-human-handoff.md");
        }
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
    ActiveRouter, ExecuteFlowSpec, FlowResult, FlowSource, LocalHumanInput, RouterAccessor,
    RouterAccessorError, RunHandle, execute_flow, resume_run, resume_run_with,
    resume_run_with_input,
};

// Crate-internal re-exports so the CLI tests (and any other in-tree caller)
// can reach the orchestration helpers without poking through a private
// module path. Kept separate from the public `pub use` above so the public
// surface stays focused on the library entry point. The bin build does not
// reach for these directly (only the test build does) -- silence the lint.
#[allow(unused_imports)]
pub(crate) use flow_api::{
    apply_resolved_roles_to_steps, apply_role_agent_overrides, apply_role_overlays, build_manifest,
    resolve_flow_path, resolve_stack_path, resolve_stack_path_for_flow_name, resolve_task,
    substitute_placeholders, substitute_roles, substitute_vars, verify_flow_step_ids,
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

    // Issue #329: lock in that the manifest builder is format-agnostic.
    //
    // The same workflow expressed in YAML and in Markdown must produce a
    // structurally identical manifest. The only fields that legitimately
    // differ are the source-keyed ones (`flow_path`, `flow_sha256`, and the
    // flow's own `ResourceRecord`), plus the wall-clock `finished_at`
    // captured inside `build_manifest` at call time. Everything else is
    // shaped from already-parsed structures (`GraphFlow` materialises
    // upstream of the builder), so a future regression that branches on
    // `flow_path.extension()` -- or that pipes the source format into a
    // new field -- will fail this test.
    const EQUIVALENCE_FLOW_YAML: &str = r#"version: "1"
name: equivalence
prompt: drive the test graph
initial: start
graph:
  start:
    role: dev
    task: say hi
    next:
      - middle: "Move to the middle state."
      - done: "Skip to done."
  middle:
    role: dev
    task: look around
    next:
      - done: "Move to the final state."
      - start: "Go back."
  done:
    final: "Three-state graph reached its terminal state."
"#;

    const EQUIVALENCE_FLOW_MD: &str = r#"---
format: kuromaku-flow/v1
---

# equivalence

drive the test graph

---

## start
*role: dev*

say hi

-> middle: Move to the middle state.
-> done: Skip to done.

---

## middle
*role: dev*

look around

-> done: Move to the final state.
-> start: Go back.

---

## done
*final: Three-state graph reached its terminal state.*
"#;

    #[test]
    fn manifest_structure_identical_for_yaml_and_md_sources() {
        use crate::config::{self, Flow};
        use crate::resolver::ResolvedRole;
        use crate::stack::StepRecord;

        // Two source files for the same logical workflow. Writing them to a
        // tempdir lets `load_flow_any_from_path` exercise its real
        // extension dispatch -- the YAML probe vs the Markdown loader.
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("flow.yaml");
        let md_path = dir.path().join("flow.md");
        std::fs::write(&yaml_path, EQUIVALENCE_FLOW_YAML).unwrap();
        std::fs::write(&md_path, EQUIVALENCE_FLOW_MD).unwrap();

        let yaml_flow = match config::load_flow_any_from_path(&yaml_path).unwrap() {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("YAML fixture must parse as a graph flow"),
        };
        let md_flow = match config::load_flow_any_from_path(&md_path).unwrap() {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("MD fixture must parse as a graph flow"),
        };

        // Sanity check: the two sources lower into equal `GraphFlow`s. The
        // manifest builder never sees `GraphFlow` directly, so this is a
        // local guard rather than the central assertion -- if the parsers
        // diverge here, the manifest comparison below would still hold but
        // the failure attribution would be misleading.
        assert_eq!(
            yaml_flow.name, md_flow.name,
            "fixture name must round-trip identically across YAML and MD parsers"
        );
        assert_eq!(
            yaml_flow.initial, md_flow.initial,
            "initial state must round-trip identically across YAML and MD parsers"
        );
        assert_eq!(
            yaml_flow.graph.keys().collect::<Vec<_>>(),
            md_flow.graph.keys().collect::<Vec<_>>(),
            "state IDs and order must round-trip identically"
        );

        // Single shared RunContext keeps `run_id`, `started_at`, `stack_path`
        // and the rules/skills caches identical between the two manifest
        // builds. Anything that varies between the two builds is therefore
        // attributable to source-format leakage, not to context drift.
        let ctx = RunContext::new(
            "equivalence".to_string(),
            "format-agnostic manifest audit".to_string(),
            dir.path().to_path_buf(),
            Some("guide content".to_string()),
            HashMap::from([("rust-developer".to_string(), "Use iterators".to_string())]),
            HashMap::from([("domain-cli".to_string(), "skill content".to_string())]),
            HashMap::new(),
        );

        let seeds = Seeds::default_local();
        let agents: Vec<config::Agent> = Vec::new();
        let agent_origins: HashMap<String, usize> = HashMap::new();
        let agent_hashes: HashMap<String, String> = HashMap::new();
        let roles = vec![ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            model_source: "agent".to_string(),
            backend_source: "agent".to_string(),
            seed_origin: Some(".kuro/".to_string()),
            extra_args: Vec::new(),
        }];
        let mut vars = HashMap::new();
        vars.insert("owner".to_string(), "nestrai".to_string());

        // Canned step records. The runtime input to `build_manifest` is
        // already format-agnostic by the time the runner reaches it, so we
        // bypass the executor entirely and feed the same `StepRunResult`
        // into both calls.
        let started = ctx.started_at.to_rfc3339();
        let results: Vec<StepRunResult> = vec![StepRunResult {
            step_id: "start".to_string(),
            agent_name: "Sage".to_string(),
            backend: "api".to_string(),
            duration: std::time::Duration::from_millis(1234),
            tokens_in: Some(100),
            tokens_out: Some(50),
            output_file: format!("{}/01-start.md", ctx.run_id),
            print_output: false,
            record: StepRecord {
                step_id: "start".to_string(),
                kind: "llm".to_string(),
                agent: Some("Sage".to_string()),
                model_requested: Some("claude-sonnet-4-5".to_string()),
                model_actual: Some("claude-sonnet-4-5".to_string()),
                backend: "api".to_string(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                duration_ms: 1234,
                started_at: started.clone(),
                exit_code: 0,
                input_steps: vec![],
                output_file: "01-start.md".to_string(),
                participants: Vec::new(),
                turns: None,
                messages: None,
                terminated_by: None,
                graph_decision: None,
            },
        }];

        let total_elapsed = std::time::Duration::from_secs(2);
        let mut yaml_manifest = build_manifest(
            &ctx,
            "equivalence",
            &yaml_path,
            EQUIVALENCE_FLOW_YAML,
            &seeds,
            &agents,
            &agent_origins,
            &agent_hashes,
            &roles,
            &vars,
            &results,
            total_elapsed,
            Some("done"),
            None,
        );
        let mut md_manifest = build_manifest(
            &ctx,
            "equivalence",
            &md_path,
            EQUIVALENCE_FLOW_MD,
            &seeds,
            &agents,
            &agent_origins,
            &agent_hashes,
            &roles,
            &vars,
            &results,
            total_elapsed,
            Some("done"),
            None,
        );

        // Source-keyed values diverge by construction: the path lives in
        // `flow_path`, the bytes hash to `flow_sha256`, and both surface a
        // second time on the flow's `ResourceRecord`. Mask all four; if
        // anything else differs, that is the regression we are catching.
        let mask = |m: &mut crate::stack::Manifest| {
            m.flow_path = "<masked>".to_string();
            m.flow_sha256 = "<masked>".to_string();
            // `finished_at` is `chrono::Utc::now()` captured inside the
            // builder; the two calls happen microseconds apart and would
            // otherwise jitter independently of source format.
            m.finished_at = "<masked>".to_string();
            for r in m.resources.iter_mut() {
                if r.kind == "flow" {
                    r.path = "<masked>".to_string();
                    r.sha256 = "<masked>".to_string();
                }
            }
        };
        mask(&mut yaml_manifest);
        mask(&mut md_manifest);

        // Compare via the YAML serialisation rather than via in-memory
        // `PartialEq`. The serialised form IS the manifest contract --
        // audit consumers read `manifest.yaml`, not the in-memory struct
        // -- and using a string compare gives the developer a readable
        // diff on failure without dragging `PartialEq` across the entire
        // manifest schema. A regression that grows a new format-keyed
        // slot (e.g. a `source_format:` field) or that branches on
        // `flow_path.extension()` inside the builder trips this assert.
        let yaml_serialised = serde_yaml::to_string(&yaml_manifest).unwrap();
        let md_serialised = serde_yaml::to_string(&md_manifest).unwrap();
        assert_eq!(
            yaml_serialised, md_serialised,
            "serialised manifest must be identical for YAML and MD sources"
        );
    }

    // ---- pause-arm coverage for `build_manifest` (issue #339) ----------
    //
    // The production change for #339 (manifest write at pause) shipped
    // alongside #337 in PR #350. The `match pause` block at the bottom of
    // `build_manifest` is therefore the wire that carries paused-run
    // metadata from the graph driver onto the manifest. End-to-end coverage
    // exists in `tests/graph_smoke.rs::human_state_pauses_run_with_status_paused_in_manifest`,
    // but that test pays for an Ollama shim subprocess and only fails after
    // the full setup runs. The two unit tests below pin the wiring directly:
    // a regression that drops one of the four paused-run fields trips a
    // millisecond-scale unit test instead of the e2e shim.
    //
    // The fixture mirrors `manifest_structure_identical_for_yaml_and_md_sources`
    // -- minimum viable RunContext, a single canned step, no real executor.
    // We extract it into a helper because the two tests need byte-identical
    // inputs and only diverge on the `final_state` / `pause` arguments.

    /// Build the canned arguments shared by the two pause-arm tests. Returns
    /// owned values rather than references so the tests can hold the result
    /// in a single binding without lifetime juggling -- the function inputs
    /// (`flow_contents`, `flow_path`, etc.) all live for the duration of the
    /// test.
    #[allow(clippy::type_complexity)]
    fn pause_arm_fixture() -> (
        RunContext,
        PathBuf,
        Seeds,
        Vec<crate::config::Agent>,
        HashMap<String, usize>,
        HashMap<String, String>,
        Vec<crate::resolver::ResolvedRole>,
        HashMap<String, String>,
        Vec<StepRunResult>,
    ) {
        use crate::resolver::ResolvedRole;
        use crate::stack::StepRecord;

        let dir = tempfile::tempdir().unwrap();
        let flow_path = dir.path().join("flow.yaml");
        let ctx = RunContext::new(
            "pause-arm".to_string(),
            "fixture for pause-arm unit tests".to_string(),
            dir.path().to_path_buf(),
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let seeds = Seeds::default_local();
        let roles = vec![ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            model_source: "agent".to_string(),
            backend_source: "agent".to_string(),
            seed_origin: Some(".kuro/".to_string()),
            extra_args: Vec::new(),
        }];
        let started = ctx.started_at.to_rfc3339();
        let results = vec![StepRunResult {
            step_id: "design".to_string(),
            agent_name: "Sage".to_string(),
            backend: "api".to_string(),
            duration: std::time::Duration::from_millis(500),
            tokens_in: Some(10),
            tokens_out: Some(5),
            output_file: format!("{}/01-design.md", ctx.run_id),
            print_output: false,
            record: StepRecord {
                step_id: "design".to_string(),
                kind: "llm".to_string(),
                agent: Some("Sage".to_string()),
                model_requested: Some("claude-sonnet-4-5".to_string()),
                model_actual: Some("claude-sonnet-4-5".to_string()),
                backend: "api".to_string(),
                tokens_in: Some(10),
                tokens_out: Some(5),
                duration_ms: 500,
                started_at: started,
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

        (
            ctx,
            flow_path,
            seeds,
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            roles,
            HashMap::new(),
            results,
        )
    }

    /// Pause arm: when `build_manifest` receives `Some(PauseRecord)`, it
    /// must transcribe all four lifecycle fields (`status`, `paused_at_state`,
    /// `paused_at`, `paused_issue_body_sha256`) onto the manifest, leave
    /// `final_state` empty, and preserve the step history that accumulated
    /// before the pause. The wire spelling `"paused"` is locked: `kuro resume`
    /// (#338) reads the manifest back as a string and matches on this value.
    #[test]
    fn build_manifest_records_pause_fields_when_pause_provided() {
        let (ctx, flow_path, seeds, agents, agent_origins, agent_hashes, roles, vars, results) =
            pause_arm_fixture();

        let pause = super::flow_api::PauseRecord {
            paused_at_state: "ask_user".to_string(),
            paused_at: "2026-05-07T09:00:01Z".to_string(),
            issue_body_sha256: Some(
                "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_string(),
            ),
        };

        let manifest = build_manifest(
            &ctx,
            "pause-arm",
            &flow_path,
            "version: \"1\"\n",
            &seeds,
            &agents,
            &agent_origins,
            &agent_hashes,
            &roles,
            &vars,
            &results,
            std::time::Duration::from_secs(1),
            None,
            Some(pause),
        );

        assert_eq!(
            manifest.status.as_deref(),
            Some("paused"),
            "status must serialise to the locked wire string `paused` so #338 can match on it"
        );
        assert_eq!(
            manifest.paused_at_state.as_deref(),
            Some("ask_user"),
            "paused_at_state must transcribe the graph state ID where the pause happened"
        );
        assert_eq!(
            manifest.paused_at.as_deref(),
            Some("2026-05-07T09:00:01Z"),
            "paused_at must transcribe the RFC3339 timestamp the driver captured"
        );
        assert_eq!(
            manifest.paused_issue_body_sha256.as_deref(),
            Some("2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"),
            "paused_issue_body_sha256 must transcribe the issue-body hash from the PauseRecord"
        );
        assert!(
            manifest.final_state.is_none(),
            "final_state and the pause fields are mutually exclusive: a paused run did not reach a terminal state"
        );
        assert_eq!(
            manifest.steps.len(),
            results.len(),
            "step history accumulated before the pause must be preserved on the manifest"
        );
        assert_eq!(
            manifest.steps[0].step_id, results[0].record.step_id,
            "the preserved step records must be the ones the runner accumulated"
        );
    }

    /// Companion of the test above: the runner-side regression guard for the
    /// `skip_serializing_if = Option::is_none` contract on the four
    /// pause fields. A non-paused run must leave all four absent so the
    /// terminal-state manifest bytes do not gain new keys; this complements
    /// the stack-layer roundtrip lock in `stack::tests::manifest_roundtrip`.
    #[test]
    fn build_manifest_omits_pause_fields_when_pause_is_none() {
        let (ctx, flow_path, seeds, agents, agent_origins, agent_hashes, roles, vars, results) =
            pause_arm_fixture();

        let manifest = build_manifest(
            &ctx,
            "pause-arm",
            &flow_path,
            "version: \"1\"\n",
            &seeds,
            &agents,
            &agent_origins,
            &agent_hashes,
            &roles,
            &vars,
            &results,
            std::time::Duration::from_secs(1),
            Some("done"),
            None,
        );

        assert!(
            manifest.status.is_none(),
            "status must stay absent on terminal runs so the manifest's existence keeps encoding `ran to completion`"
        );
        assert!(
            manifest.paused_at_state.is_none(),
            "paused_at_state must stay absent on terminal runs"
        );
        assert!(
            manifest.paused_at.is_none(),
            "paused_at must stay absent on terminal runs"
        );
        assert!(
            manifest.paused_issue_body_sha256.is_none(),
            "paused_issue_body_sha256 must stay absent on terminal runs"
        );
        assert_eq!(
            manifest.final_state.as_deref(),
            Some("done"),
            "final_state must transcribe the terminal-state ID the graph driver reported"
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
            run: Some(command.to_string()),
            ..Default::default()
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
            task: Some("Review the diff".to_string()),
            input: vec!["fetch".to_string()],
            ..Default::default()
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

    // --- agent overlays (issue #364) -----------------------------------

    /// Build a seed Agent for overlay tests. Each test mutates the
    /// returned agent through `apply_role_overlays` and asserts on the
    /// post-overlay state.
    fn babis_seed() -> crate::config::Agent {
        let mut extra_args = HashMap::new();
        extra_args.insert(
            Backend::Codex,
            vec!["--sandbox".to_string(), "read-only".to_string()],
        );
        crate::config::Agent {
            id: "Babis".to_string(),
            name: "Babis".to_string(),
            title: None,
            description: None,
            role: String::new(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["design-rigor".to_string(), "naming-discipline".to_string()],
            skills: Vec::new(),
            env: HashMap::new(),
            extra_args,
        }
    }

    /// Build a one-role KotoConfig with the given overlay attached.
    fn koto_with_overlay(
        role: &str,
        agent: &str,
        overlay: crate::koto_config::RoleOverlay,
    ) -> crate::koto_config::KotoConfig {
        use crate::koto_config::{KotoConfig, KotoRole, Seeds};
        let mut roles = HashMap::new();
        roles.insert(
            role.to_string(),
            KotoRole {
                agent: agent.to_string(),
                model: None,
                backend: None,
                overlays: overlay,
            },
        );
        KotoConfig {
            version: "1".to_string(),
            tiers: HashMap::new(),
            default_backend: None,
            vars: HashMap::new(),
            roles,
            seeds: Seeds::default_local(),
        }
    }

    #[test]
    fn overlay_appends_rules_seed_first() {
        // AC2: seed rules keep position; overlay rules come last.
        use crate::koto_config::RoleOverlay;
        let mut agents = vec![babis_seed()];
        let overlay = RoleOverlay {
            rules: vec![
                "senior-cover-letter".to_string(),
                "customer-anonymization".to_string(),
            ],
            ..RoleOverlay::default()
        };
        let kc = koto_with_overlay("writer", "Babis", overlay);
        let roles = vec![("writer".to_string(), "Babis".to_string())];
        let applied = apply_role_overlays(&mut agents, &roles, Some(&kc)).unwrap();

        assert_eq!(
            agents[0].rules,
            vec![
                "design-rigor".to_string(),
                "naming-discipline".to_string(),
                "senior-cover-letter".to_string(),
                "customer-anonymization".to_string(),
            ]
        );
        assert_eq!(applied["writer"].rule_delta, 2);
    }

    #[test]
    fn overlay_dedup_preserves_seed_order() {
        // AC7: duplicate rule names get deduplicated and the seed rule
        // keeps its position. Only the new rule lands at the end.
        use crate::koto_config::RoleOverlay;
        let mut agents = vec![babis_seed()];
        let overlay = RoleOverlay {
            rules: vec![
                "design-rigor".to_string(), // already on seed -- dedup
                "senior-cover-letter".to_string(),
            ],
            ..RoleOverlay::default()
        };
        let kc = koto_with_overlay("writer", "Babis", overlay);
        let roles = vec![("writer".to_string(), "Babis".to_string())];
        let applied = apply_role_overlays(&mut agents, &roles, Some(&kc)).unwrap();

        assert_eq!(
            agents[0].rules,
            vec![
                "design-rigor".to_string(),
                "naming-discipline".to_string(),
                "senior-cover-letter".to_string(),
            ]
        );
        // Only one new rule survived dedup, so rule_delta is 1.
        assert_eq!(applied["writer"].rule_delta, 1);
    }

    #[test]
    fn overlay_replaces_model() {
        // AC3: overlay `model:` replaces the seed agent's model wholesale.
        use crate::koto_config::RoleOverlay;
        let mut agents = vec![babis_seed()];
        let overlay = RoleOverlay {
            model: Some("claude/opus-4-7".to_string()),
            ..RoleOverlay::default()
        };
        let kc = koto_with_overlay("writer", "Babis", overlay);
        let roles = vec![("writer".to_string(), "Babis".to_string())];
        let applied = apply_role_overlays(&mut agents, &roles, Some(&kc)).unwrap();

        assert_eq!(agents[0].model, "claude/opus-4-7");
        assert!(applied["writer"].model_replaced);
    }

    #[test]
    fn overlay_replaces_extra_args_per_backend() {
        // AC4: extra_args.codex replaces the seed list entirely (no
        // token-level merge). Other backends keep their seed values.
        use crate::koto_config::RoleOverlay;
        let mut agents = vec![babis_seed()];
        let mut overlay_extra = HashMap::new();
        overlay_extra.insert(
            Backend::Codex,
            vec!["--sandbox".to_string(), "workspace-write".to_string()],
        );
        let overlay = RoleOverlay {
            extra_args: overlay_extra,
            ..RoleOverlay::default()
        };
        let kc = koto_with_overlay("writer", "Babis", overlay);
        let roles = vec![("writer".to_string(), "Babis".to_string())];
        let applied = apply_role_overlays(&mut agents, &roles, Some(&kc)).unwrap();

        // Codex slot replaced wholesale.
        assert_eq!(
            agents[0].extra_args.get(&Backend::Codex),
            Some(&vec![
                "--sandbox".to_string(),
                "workspace-write".to_string()
            ])
        );
        // Other backend keys untouched (there were none on the seed for
        // claude-cli; the assertion just confirms no spurious entries).
        assert!(!agents[0].extra_args.contains_key(&Backend::ClaudeCli));
        assert_eq!(applied["writer"].extra_args_backends, vec![Backend::Codex]);
    }

    #[test]
    fn overlay_no_overlay_is_byte_identical_to_baseline() {
        // AC6: a role without overlays produces a no-op. The agent must
        // come out the other side untouched, and the returned map carries
        // no entry for that role.
        let mut agents = vec![babis_seed()];
        let baseline = agents[0].clone();
        let kc = koto_with_overlay(
            "writer",
            "Babis",
            crate::koto_config::RoleOverlay::default(),
        );
        let roles = vec![("writer".to_string(), "Babis".to_string())];
        let applied = apply_role_overlays(&mut agents, &roles, Some(&kc)).unwrap();

        assert_eq!(agents[0], baseline);
        assert!(applied.is_empty());
    }

    #[test]
    fn overlay_none_koto_config_is_noop() {
        // Defensive: callers that have no project config (e.g. tests, MCP
        // probes) get an empty map and untouched agents. No panic, no
        // hidden mutation.
        let mut agents = vec![babis_seed()];
        let baseline = agents[0].clone();
        let roles = vec![("writer".to_string(), "Babis".to_string())];
        let applied = apply_role_overlays(&mut agents, &roles, None).unwrap();
        assert_eq!(agents[0], baseline);
        assert!(applied.is_empty());
    }

    #[test]
    fn overlay_collision_two_roles_same_agent_errors() {
        // v1 constraint: if two roles bind the same agent_id and their
        // overlays differ, refuse. Forking the agent is the documented
        // exit ramp; the error message points at both colliding roles.
        use crate::config::Agent;
        use crate::koto_config::{KotoConfig, KotoRole, RoleOverlay, Seeds};

        let mut agents = vec![Agent {
            id: "Babis".to_string(),
            name: "Babis".to_string(),
            title: None,
            description: None,
            role: String::new(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            rules: Vec::new(),
            skills: Vec::new(),
            env: HashMap::new(),
            extra_args: HashMap::new(),
        }];
        let mut roles_cfg = HashMap::new();
        roles_cfg.insert(
            "writer".to_string(),
            KotoRole {
                agent: "Babis".to_string(),
                model: None,
                backend: None,
                overlays: RoleOverlay {
                    rules: vec!["a".to_string()],
                    ..RoleOverlay::default()
                },
            },
        );
        roles_cfg.insert(
            "reviewer".to_string(),
            KotoRole {
                agent: "Babis".to_string(),
                model: None,
                backend: None,
                overlays: RoleOverlay {
                    rules: vec!["b".to_string()],
                    ..RoleOverlay::default()
                },
            },
        );
        let kc = KotoConfig {
            version: "1".to_string(),
            tiers: HashMap::new(),
            default_backend: None,
            vars: HashMap::new(),
            roles: roles_cfg,
            seeds: Seeds::default_local(),
        };
        let roles = vec![
            ("writer".to_string(), "Babis".to_string()),
            ("reviewer".to_string(), "Babis".to_string()),
        ];
        let err = apply_role_overlays(&mut agents, &roles, Some(&kc)).unwrap_err();
        assert!(
            err.contains("Babis") && err.contains("differing overlays"),
            "got: {err}"
        );
    }

    #[test]
    fn overlay_identical_overlays_on_same_agent_allowed() {
        // Counter to the collision test: when two roles bind the same
        // agent with structurally equal overlays, no conflict exists --
        // applying once produces the right end state. This avoids
        // breaking obvious user intent (two `*-reviewer` roles sharing a
        // Babis seed with the same overlay block).
        use crate::config::Agent;
        use crate::koto_config::{KotoConfig, KotoRole, RoleOverlay, Seeds};

        let mut agents = vec![Agent {
            id: "Babis".to_string(),
            name: "Babis".to_string(),
            title: None,
            description: None,
            role: String::new(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["seed-rule".to_string()],
            skills: Vec::new(),
            env: HashMap::new(),
            extra_args: HashMap::new(),
        }];
        let overlay = RoleOverlay {
            rules: vec!["shared-rule".to_string()],
            ..RoleOverlay::default()
        };
        let mut roles_cfg = HashMap::new();
        roles_cfg.insert(
            "writer".to_string(),
            KotoRole {
                agent: "Babis".to_string(),
                model: None,
                backend: None,
                overlays: overlay.clone(),
            },
        );
        roles_cfg.insert(
            "reviewer".to_string(),
            KotoRole {
                agent: "Babis".to_string(),
                model: None,
                backend: None,
                overlays: overlay,
            },
        );
        let kc = KotoConfig {
            version: "1".to_string(),
            tiers: HashMap::new(),
            default_backend: None,
            vars: HashMap::new(),
            roles: roles_cfg,
            seeds: Seeds::default_local(),
        };
        let roles = vec![
            ("writer".to_string(), "Babis".to_string()),
            ("reviewer".to_string(), "Babis".to_string()),
        ];
        let applied =
            apply_role_overlays(&mut agents, &roles, Some(&kc)).expect("identical overlays ok");

        assert_eq!(
            agents[0].rules,
            vec!["seed-rule".to_string(), "shared-rule".to_string()]
        );
        // Both roles must see the summary so the banner shows the
        // contribution for either step.
        assert!(applied.contains_key("writer"));
        assert!(applied.contains_key("reviewer"));
    }

    #[test]
    fn overlay_summary_renders_combined() {
        // The summary string is what feeds the banner and the audit
        // line. Pin the surface so a future refactor cannot silently
        // change the format from underneath the user.
        use super::flow_api::OverlayApplied;
        let s = OverlayApplied {
            rule_delta: 2,
            model_replaced: true,
            backend_replaced: false,
            extra_args_backends: vec![Backend::Codex],
        };
        let summary = s.summary().unwrap();
        assert!(summary.contains("rules+=2"));
        assert!(summary.contains("model"));
        assert!(summary.contains("extra_args[codex]"));
    }

    #[test]
    fn overlay_summary_is_none_when_empty() {
        // Symmetric to the rendered case: empty OverlayApplied collapses
        // to None so banner and audit suppress the line entirely.
        use super::flow_api::OverlayApplied;
        let s = OverlayApplied {
            rule_delta: 0,
            model_replaced: false,
            backend_replaced: false,
            extra_args_backends: Vec::new(),
        };
        assert!(s.summary().is_none());
    }
}
