use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::config::{Agent, Backend, FlowConfig, Stage, TaskSource};
use crate::executor::{self, ExecutionTask, ExecutorBoxed};
use crate::llm::{self, LlmRequest, Message, Role};
use crate::state::{self, StageOutput};
use crate::ui::{self, StageInfo, StageState};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("stage '{stage}' references unknown agent '{agent}'")]
    UnknownAgent { stage: String, agent: String },

    #[error("stage '{stage}' failed: {source}")]
    LlmFailed {
        stage: String,
        source: llm::LlmError,
    },

    #[error("stage '{stage}' execution failed: {source}")]
    ExecutorFailed {
        stage: String,
        source: executor::ExecutorError,
    },

    #[error("state error in stage '{stage}': {source}")]
    State {
        stage: String,
        source: state::StateError,
    },

    #[error("template tasks are not yet supported (stage '{0}')")]
    TemplateNotSupported(String),

    #[error("rules file not found: {0}")]
    RulesNotFound(String),
}

/// Result of running a single stage, used for the summary table.
pub struct StageRunResult {
    pub stage_id: String,
    pub agent_id: String,
    pub backend: Backend,
    pub duration: std::time::Duration,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    pub output_file: String,
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
        if let Some(ref rules_name) = agent.rules {
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

/// Build the full system prompt: Guide + Rules + Role.
fn build_system_prompt(
    agent: &Agent,
    guide: &Option<String>,
    rules_cache: &HashMap<String, String>,
) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if let Some(guide_content) = guide {
        parts.push(guide_content);
    }

    let rules_content;
    if let Some(ref rules_name) = agent.rules
        && let Some(content) = rules_cache.get(rules_name)
    {
        rules_content = content.clone();
        parts.push(&rules_content);
    }

    parts.push(&agent.role);
    parts.join("\n\n")
}

/// Build the user-facing prompt with context from prior stages.
fn build_user_prompt(
    task_text: &str,
    stage: &Stage,
    state_path: &Path,
) -> Result<String, RunError> {
    let mut context_parts: Vec<String> = Vec::new();
    for input_id in &stage.input {
        let prior = state::read_stage(state_path, input_id).map_err(|e| RunError::State {
            stage: stage.id.clone(),
            source: e,
        })?;
        let output_label = prior.stage_id.clone();
        ui::print_context_injection(&output_label, &format!("{input_id}.json"), "");
        context_parts.push(format!(
            "--- Output from stage '{output_label}' ---\n{}\n---",
            prior.response
        ));
    }

    let mut user_content = task_text.to_string();
    if !context_parts.is_empty() {
        user_content = format!(
            "{user_content}\n\nContext from previous stages:\n\n{}",
            context_parts.join("\n\n")
        );
    }
    Ok(user_content)
}

/// Run a stage via the Executor (CLI backends: claude-cli, ollama).
async fn run_stage_via_executor(
    executor: &dyn ExecutorBoxed,
    stage: &Stage,
    flow_name: &str,
    system_prompt: &str,
    user_content: &str,
    model: &str,
    backend: Backend,
) -> Result<(String, Option<llm::Usage>), RunError> {
    let task_id = format!("koto-{flow_name}-{}", stage.id);

    let command = match backend {
        Backend::ClaudeCli => {
            executor::build_claude_command(model, Some(system_prompt), user_content)
        }
        Backend::Ollama => {
            // For ollama, fold system prompt into user content
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
            stage: stage.id.clone(),
            source: e,
        })?;

    let output = executor
        .wait_boxed(&handle)
        .await
        .map_err(|e| RunError::ExecutorFailed {
            stage: stage.id.clone(),
            source: e,
        })?;

    Ok((output.stdout, None))
}

/// Run a stage via the API client directly (no executor needed).
async fn run_stage_via_api(
    request: LlmRequest,
    stage_id: &str,
) -> Result<(String, Option<llm::Usage>), RunError> {
    let client = llm::ApiClient::from_env();
    let response = client
        .send(request)
        .await
        .map_err(|e| RunError::LlmFailed {
            stage: stage_id.to_string(),
            source: e,
        })?;
    Ok((response.content, response.usage))
}

/// Run stages sequentially in topological order.
pub async fn run_stages(
    config: &FlowConfig,
    stages: &[&Stage],
    state_path: &Path,
    flow_name: &str,
    guide: &Option<String>,
    rules_cache: &HashMap<String, String>,
) -> Result<Vec<StageRunResult>, RunError> {
    let agents: HashMap<&str, &Agent> = config.agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let total = stages.len();
    let mut results = Vec::with_capacity(total);

    // Create executor for CLI backends
    let executor = executor::create_executor(config);

    for (i, stage) in stages.iter().enumerate() {
        let agent = agents
            .get(stage.agent.as_str())
            .ok_or_else(|| RunError::UnknownAgent {
                stage: stage.id.clone(),
                agent: stage.agent.clone(),
            })?;

        let effective_model = stage.model.as_deref().unwrap_or(&agent.model);
        let effective_backend = stage.backend.unwrap_or(agent.backend);

        let stage_info = StageInfo {
            id: stage.id.clone(),
            agent: agent.id.clone(),
            role: agent.role.clone(),
            model: effective_model.to_string(),
            backend: effective_backend,
            input: stage.input.clone(),
            output: stage.output.clone().unwrap_or_else(|| "stdout".to_string()),
            state: StageState::Running,
        };

        ui::print_stage_banner(i + 1, total, &stage_info);

        let task_text = match &stage.task {
            TaskSource::Inline(text) => text.clone(),
            TaskSource::Template { .. } => {
                return Err(RunError::TemplateNotSupported(stage.id.clone()));
            }
        };

        let user_content = build_user_prompt(&task_text, stage, state_path)?;

        ui::print_thinking(&task_text);

        let system_prompt = build_system_prompt(agent, guide, rules_cache);

        let start = Instant::now();

        let (content, usage) = if executor::backend_needs_executor(effective_backend) {
            run_stage_via_executor(
                executor.as_ref(),
                stage,
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
            run_stage_via_api(request, &stage.id).await?
        };

        let duration = start.elapsed();

        // Save state
        let timestamp = chrono::Utc::now().to_rfc3339();
        let stage_output = StageOutput {
            stage_id: stage.id.clone(),
            agent_id: agent.id.clone(),
            model: effective_model.to_string(),
            prompt: user_content,
            response: content.clone(),
            timestamp,
        };
        state::write_stage(state_path, &stage_output).map_err(|e| RunError::State {
            stage: stage.id.clone(),
            source: e,
        })?;

        // Write output file if specified
        let output_file = stage.output.clone().unwrap_or_else(|| "stdout".to_string());
        if let Some(ref path) = stage.output {
            let output_path = state_path.join(path);
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&output_path, &content).map_err(|e| RunError::State {
                stage: stage.id.clone(),
                source: state::StateError::Write(e),
            })?;
        }

        let tokens_in = usage.as_ref().map(|u| u.input_tokens);
        let tokens_out = usage.as_ref().map(|u| u.output_tokens);

        ui::print_stage_done(
            &format_duration(duration),
            &tokens_in.map_or("—".to_string(), |t| t.to_string()),
            &tokens_out.map_or("—".to_string(), |t| t.to_string()),
            &output_file,
        );

        results.push(StageRunResult {
            stage_id: stage.id.clone(),
            agent_id: agent.id.clone(),
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
pub fn build_summary(results: &[StageRunResult]) -> Vec<ui::StageResult> {
    results
        .iter()
        .map(|r| ui::StageResult {
            id: r.stage_id.clone(),
            agent: r.agent_id.clone(),
            backend: backend_name(r.backend).to_string(),
            duration: format_duration(r.duration),
            tokens_in: r.tokens_in.map_or("—".to_string(), |t| t.to_string()),
            tokens_out: r.tokens_out.map_or("—".to_string(), |t| t.to_string()),
            output: r.output_file.clone(),
            state: StageState::Done,
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
        let results = vec![StageRunResult {
            stage_id: "design".to_string(),
            agent_id: "architect".to_string(),
            backend: Backend::Api,
            duration: std::time::Duration::from_secs(5),
            tokens_in: Some(1200),
            tokens_out: Some(800),
            output_file: "design.md".to_string(),
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
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: Some("rust-developer".to_string()),
        };
        let guide = Some("Project guide content".to_string());
        let mut rules_cache = HashMap::new();
        rules_cache.insert(
            "rust-developer".to_string(),
            "Rust rules content".to_string(),
        );

        let prompt = build_system_prompt(&agent, &guide, &rules_cache);
        assert!(prompt.starts_with("Project guide content"));
        assert!(prompt.contains("Rust rules content"));
        assert!(prompt.ends_with("You are a developer"));
    }

    #[test]
    fn build_system_prompt_without_guide_or_rules() {
        let agent = Agent {
            id: "dev".to_string(),
            role: "You are a developer".to_string(),
            model: "sonnet".to_string(),
            backend: Backend::ClaudeCli,
            rules: None,
        };
        let guide = None;
        let rules_cache = HashMap::new();

        let prompt = build_system_prompt(&agent, &guide, &rules_cache);
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
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: Some("rust-developer".to_string()),
        }];

        let cache = load_rules_for_agents(&agents, dir.path()).unwrap();
        assert_eq!(cache.get("rust-developer").unwrap(), "Use iterators");
    }

    #[test]
    fn load_rules_for_agents_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents = vec![Agent {
            id: "dev".to_string(),
            role: "dev".to_string(),
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
            rules: Some("nonexistent".to_string()),
        }];

        let err = load_rules_for_agents(&agents, dir.path()).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }
}
