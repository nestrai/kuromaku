use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::{Agent, Backend, Step};
use crate::executor::{self, ExecutionTask, ExecutorBoxed, OutputFormat};
use crate::koto_config::Seeds;
use crate::llm::{self, LlmRequest, Message, Role};
use crate::notify::github::{self, PostOutcome};
use crate::skills;
use crate::stack::{self, StepOutput};
use crate::ui::{self, StepInfo, StepState};

/// Immutable context for a single flow run.
/// Constructed once in main::run_up(), passed to run_steps() and internal helpers.
pub struct RunContext {
    #[allow(dead_code)] // Placeholder for Phase 1.2 (run-ID stack)
    pub run_id: String,
    pub flow_name: String,
    pub task: String,
    pub stack_path: PathBuf,
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
        let now = chrono::Local::now();
        let ts = now.format("%Y%m%d-%H%M%S").to_string();
        // Short hash from timestamp nanos for uniqueness
        let hash = format!("{:03x}", now.timestamp_subsec_nanos() & 0xFFF);
        let run_id = format!("{ts}-{hash}");

        Self {
            run_id,
            flow_name,
            task,
            stack_path,
            guide,
            rules_cache,
            skills_cache,
            template_vars,
            poster: github::gh_poster(),
        }
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

/// Result of running a single step, used for the summary table.
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
    pub output_file: String,
    pub print_output: bool,
}

/// Generate an auto-named output filename: `<flow>-<timestamp>-<step>-<agent>.md`
fn auto_output_filename(flow_name: &str, step_id: &str, agent_name: &str) -> String {
    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d-%H%M");
    format!("{flow_name}-{ts}-{step_id}-{agent_name}.md")
}

/// Output filename for shell steps: `<flow>-<timestamp>-<step>-shell.txt`.
///
/// Uses `.txt` rather than `.md` because shell stdout isn't markdown -- a
/// downstream `print_output: true` would render terminal escapes through
/// termimad otherwise.
fn shell_output_filename(flow_name: &str, step_id: &str) -> String {
    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d-%H%M");
    format!("{flow_name}-{ts}-{step_id}-shell.txt")
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
fn build_user_prompt(task: &str, step: &Step, stack_path: &Path) -> Result<String, RunError> {
    let mut context_parts: Vec<String> = Vec::new();
    for input_id in &step.input {
        let prior = stack::read_step(stack_path, input_id).map_err(|e| RunError::Stack {
            step: step.id.clone(),
            source: e,
        })?;
        let output_label = prior.step_id.clone();
        ui::print_context_injection(&output_label, &format!("{input_id}.json"), "");
        context_parts.push(format!(
            "--- Output from step '{output_label}' ---\n{}\n---",
            prior.response
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
    // even if the command takes a while.
    let output_file = shell_output_filename(&ctx.flow_name, &step.id);
    let output_path = ctx.stack_path.join(&output_file);
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

    // Write to stack: agent_id is empty (no agent), model is "shell" so
    // downstream tooling can tell shell steps apart.
    let timestamp = chrono::Utc::now().to_rfc3339();
    let step_output = StepOutput {
        step_id: step.id.clone(),
        agent_id: String::new(),
        model: "shell".to_string(),
        prompt: command.to_string(),
        response: stdout.clone(),
        timestamp,
    };
    stack::write_step(&ctx.stack_path, &step_output).map_err(|e| RunError::Stack {
        step: step.id.clone(),
        source: e,
    })?;

    // The artifact file was streamed to during execution (issue #16) so no
    // post-hoc write is needed here. The on-disk content matches `stdout`
    // modulo a trailing newline preserved from the raw stream.

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
    })
}

/// Run steps sequentially in topological order.
pub async fn run_steps(
    steps: &[&Step],
    agents: &[Agent],
    ctx: &RunContext,
) -> Result<Vec<StepRunResult>, RunError> {
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

        let user_content = build_user_prompt(&step_task, step, &ctx.stack_path)?;

        // Pre-compute output path and show it immediately so user can tail -f
        let output_file = auto_output_filename(&ctx.flow_name, &step.id, &agent.name);
        let output_path = ctx.stack_path.join(&output_file);
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

        // Save to stack
        let timestamp = chrono::Utc::now().to_rfc3339();
        let step_output = StepOutput {
            step_id: step.id.clone(),
            agent_id: agent.id.clone(),
            model: effective_model.to_string(),
            prompt: user_content,
            response: content.clone(),
            timestamp,
        };
        stack::write_step(&ctx.stack_path, &step_output).map_err(|e| RunError::Stack {
            step: step.id.clone(),
            source: e,
        })?;

        // Executor backends stream stdout to the artifact file during
        // execution (issue #16). Only the API backend, which has no live
        // process to read from, still needs a post-hoc write.
        if !executor::backend_needs_executor(effective_backend) {
            std::fs::write(&output_path, &content).map_err(|e| RunError::Stack {
                step: step.id.clone(),
                source: stack::StackError::Write(e),
            })?;
        }

        let tokens_in = usage.as_ref().map(|u| u.input_tokens);
        let tokens_out = usage.as_ref().map(|u| u.output_tokens);

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
            output_file: "dev-20260421-1052-design-Levi.md".to_string(),
            print_output: false,
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
    fn auto_output_filename_format() {
        let name = auto_output_filename("development", "design", "Levi");
        assert!(name.starts_with("development-"));
        assert!(name.contains("-design-Levi.md"));
        assert!(name.ends_with(".md"));
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
        let name = shell_output_filename("dev", "fetch");
        assert!(name.starts_with("dev-"), "got: {name}");
        assert!(name.contains("-fetch-shell."), "got: {name}");
        assert!(name.ends_with(".txt"), "got: {name}");
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
        // the stack same as LLM outputs.
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

        // Stack record uses the same StepOutput schema as LLM steps so
        // downstream `input:` consumers don't need to know the difference.
        let saved = stack::read_step(dir.path(), "greet").unwrap();
        assert_eq!(saved.response, "hello-from-shell");
        assert_eq!(saved.agent_id, "");
        assert_eq!(saved.model, "shell");
        assert_eq!(saved.prompt, "echo hello-from-shell");
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

        // The next step would call stack::read_step("fetch") -- mirror that.
        let prior = stack::read_step(dir.path(), "fetch").unwrap();
        assert_eq!(prior.response, "diff content");
    }
}
