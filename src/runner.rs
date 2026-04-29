use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
        // Two `koto up` calls in the same wall-clock second would otherwise
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
/// being created later in the run, but `koto up` invocations are user-driven
/// (not a service loop), so the race is bounded by how fast a human can press
/// Enter twice. The overwrite bug, by contrast, hits any back-to-back run.
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
fn llm_output_filename(step_num: usize, step_id: &str) -> String {
    stack::step_content_filename(step_num, step_id, "md")
}

/// Load `.koto/Guide.md` if it exists. Test-only single-dir variant; the
/// production loader is [`load_guide_from_seeds`].
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
fn build_system_prompt(
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
#[allow(clippy::too_many_arguments)]
async fn run_step_via_executor(
    executor: &dyn ExecutorBoxed,
    step: &Step,
    flow_name: &str,
    system_prompt: &str,
    user_content: &str,
    model: &str,
    backend: Backend,
    output_path: &Path,
) -> Result<(String, Option<llm::Usage>), RunError> {
    // Build unique session name: koto-<project>-<flow>-<step>-<short-id>
    let project = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let short_id = &chrono::Utc::now().timestamp_millis().to_string()[8..];
    let task_id = format!("koto-{project}-{flow_name}-{}-{short_id}", step.id);

    let command = match backend {
        Backend::ClaudeCli => {
            executor::build_claude_command(model, Some(system_prompt), user_content)
        }
        Backend::Codex => executor::build_codex_command(model, Some(system_prompt), user_content),
        Backend::Ollama => {
            let mut prompt = String::new();
            prompt.push_str(&format!("System: {system_prompt}\n\n"));
            prompt.push_str(&format!("User: {user_content}"));
            executor::build_ollama_command(model, &prompt)
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
        "koto-{project}-{}-{}-{short_id}-shell",
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

/// Run steps sequentially in topological order.
pub async fn run_steps(
    steps: &[&Step],
    agents: &[Agent],
    ctx: &RunContext,
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

        let agent = agent_map
            .get(step.agent.as_str())
            .ok_or_else(|| RunError::UnknownAgent {
                step: step.id.clone(),
                agent: step.agent.clone(),
            })?;

        let effective_model = step.model.as_deref().unwrap_or(&agent.model);
        let effective_backend = step.backend.unwrap_or(agent.backend);

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

fn format_duration(d: std::time::Duration) -> String {
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
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust-developer".to_string()],
            skills: vec!["error-handling".to_string()],
            env: HashMap::new(),
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
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust".to_string(), "cli-ux".to_string()],
            skills: vec![],
            env: HashMap::new(),
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
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec![],
            skills: vec![],
            env: HashMap::new(),
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
    fn load_rules_for_agents_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("rust-developer.md"), "Use iterators").unwrap();

        let agents = vec![Agent {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            title: None,
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust-developer".to_string()],
            skills: vec![],
            env: HashMap::new(),
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
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["rust".to_string(), "cli".to_string()],
            skills: vec![],
            env: HashMap::new(),
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
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["nonexistent".to_string()],
            skills: vec![],
            env: HashMap::new(),
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
        // Two `koto up` calls in the same wall-clock second must not collide.
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
        // Acceptance: every koto up creates a run directory with NN-<step>.md
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
}
