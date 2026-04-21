use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::config::{Agent, Backend, Step};
use crate::executor::{self, ExecutionTask, ExecutorBoxed};
use crate::llm::{self, LlmRequest, Message, Role};
use crate::skills;
use crate::stack::{self, StepOutput};
use crate::ui::{self, StepInfo, StepState};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("step '{step}' references unknown agent '{agent}'")]
    UnknownAgent { step: String, agent: String },

    #[error("step '{step}' failed: {source}")]
    LlmFailed {
        step: String,
        source: llm::LlmError,
    },

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
pub struct StepRunResult {
    pub step_id: String,
    pub agent_name: String,
    pub backend: Backend,
    pub duration: std::time::Duration,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    pub output_file: String,
}

/// Generate an auto-named output filename: `<flow>-<timestamp>-<step>-<agent>.md`
fn auto_output_filename(flow_name: &str, step_id: &str, agent_name: &str) -> String {
    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d-%H%M");
    format!("{flow_name}-{ts}-{step_id}-{agent_name}.md")
}

/// Load `.koto/Guide.md` if it exists.
pub fn load_guide(koto_dir: &Path) -> Option<String> {
    let guide_path = koto_dir.join("Guide.md");
    std::fs::read_to_string(&guide_path).ok()
}

/// Pre-load rules files for all agents that reference them.
/// Returns a map from rules name -> content.
pub fn load_rules_for_agents(
    agents: &[Agent],
    koto_dir: &Path,
) -> Result<HashMap<String, String>, RunError> {
    let mut cache: HashMap<String, String> = HashMap::new();
    let rules_dir = koto_dir.join("rules");

    for agent in agents {
        for rules_name in &agent.rules {
            if cache.contains_key(rules_name) {
                continue;
            }
            let rules_path = rules_dir.join(format!("{rules_name}.md"));
            let content = std::fs::read_to_string(&rules_path).map_err(|_| {
                RunError::RulesNotFound(format!(
                    "rules file '{}' not found (expected at {})",
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
fn build_user_prompt(
    task: &str,
    step: &Step,
    stack_path: &Path,
) -> Result<String, RunError> {
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
async fn run_step_via_executor(
    executor: &dyn ExecutorBoxed,
    step: &Step,
    flow_name: &str,
    system_prompt: &str,
    user_content: &str,
    model: &str,
    backend: Backend,
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
        Backend::Ollama => {
            let mut prompt = String::new();
            prompt.push_str(&format!("System: {system_prompt}\n\n"));
            prompt.push_str(&format!("User: {user_content}"));
            executor::build_ollama_command(model, &prompt)
        }
        Backend::Api => unreachable!("API backend does not use executor"),
    };

    let task = ExecutionTask {
        id: task_id,
        command,
        env: HashMap::new(),
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

/// Run steps sequentially in topological order.
#[allow(clippy::too_many_arguments)]
pub async fn run_steps(
    steps: &[&Step],
    agents: &[Agent],
    task: &str,
    stack_path: &Path,
    flow_name: &str,
    guide: &Option<String>,
    rules_cache: &HashMap<String, String>,
    skills_cache: &HashMap<String, String>,
) -> Result<Vec<StepRunResult>, RunError> {
    let agent_map: HashMap<&str, &Agent> = agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let total = steps.len();
    let mut results = Vec::with_capacity(total);

    let executor = executor::create_executor();

    for (i, step) in steps.iter().enumerate() {
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
        let step_task = task.to_string();

        let user_content = build_user_prompt(&step_task, step, stack_path)?;

        // Pre-compute output path and show it immediately so user can tail -f
        let output_file = auto_output_filename(flow_name, &step.id, &agent.name);
        let output_path = stack_path.join(&output_file);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        ui::print_thinking(&step_task);
        eprintln!(
            "      output: {}",
            output_path.canonicalize().unwrap_or(output_path.clone()).display()
        );

        let system_prompt = build_system_prompt(agent, guide, rules_cache, skills_cache);

        let start = Instant::now();

        let (content, usage) = if executor::backend_needs_executor(effective_backend) {
            run_step_via_executor(
                executor.as_ref(),
                step,
                flow_name,
                &system_prompt,
                &user_content,
                effective_model,
                effective_backend,
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
        stack::write_step(stack_path, &step_output).map_err(|e| RunError::Stack {
            step: step.id.clone(),
            source: e,
        })?;

        // Write artifact to pre-computed output path
        std::fs::write(&output_path, &content).map_err(|e| RunError::Stack {
            step: step.id.clone(),
            source: stack::StackError::Write(e),
        })?;

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

        results.push(StepRunResult {
            step_id: step.id.clone(),
            agent_name: agent.name.clone(),
            backend: effective_backend,
            duration,
            tokens_in,
            tokens_out,
            output_file,
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
            backend: backend_name(r.backend).to_string(),
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
        assert_eq!(backend_name(Backend::Ollama), "ollama");
    }

    #[test]
    fn build_summary_maps_fields() {
        let results = vec![StepRunResult {
            step_id: "design".to_string(),
            agent_name: "Levi".to_string(),
            backend: Backend::Api,
            duration: std::time::Duration::from_secs(5),
            tokens_in: Some(1200),
            tokens_out: Some(800),
            output_file: "dev-20260421-1052-design-Levi.md".to_string(),
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
}
