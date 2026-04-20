use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::config::{Agent, Backend, FlowConfig, Stage, TaskSource};
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

    #[error("state error in stage '{stage}': {source}")]
    State {
        stage: String,
        source: state::StateError,
    },

    #[error("template tasks are not yet supported (stage '{0}')")]
    TemplateNotSupported(String),
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

/// Run stages sequentially in topological order.
pub async fn run_stages(
    config: &FlowConfig,
    stages: &[&Stage],
    state_path: &Path,
) -> Result<Vec<StageRunResult>, RunError> {
    let agents: HashMap<&str, &Agent> = config.agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let total = stages.len();
    let mut results = Vec::with_capacity(total);

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

        // Build context from input stages
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

        // Build prompt
        let mut user_content = task_text.clone();
        if !context_parts.is_empty() {
            user_content = format!(
                "{user_content}\n\nContext from previous stages:\n\n{}",
                context_parts.join("\n\n")
            );
        }

        ui::print_thinking(&task_text);

        let request = LlmRequest {
            model: effective_model.to_string(),
            system: Some(agent.role.clone()),
            messages: vec![Message {
                role: Role::User,
                content: user_content.clone(),
            }],
            max_tokens: 4096,
        };

        let client = llm::create_client(effective_backend);
        let start = Instant::now();
        let response = client
            .send_boxed(request)
            .await
            .map_err(|e| RunError::LlmFailed {
                stage: stage.id.clone(),
                source: e,
            })?;
        let duration = start.elapsed();

        // Save state
        let timestamp = chrono::Utc::now().to_rfc3339();
        let stage_output = StageOutput {
            stage_id: stage.id.clone(),
            agent_id: agent.id.clone(),
            model: response.model.clone(),
            prompt: user_content,
            response: response.content.clone(),
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
            std::fs::write(&output_path, &response.content).map_err(|e| RunError::State {
                stage: stage.id.clone(),
                source: state::StateError::Write(e),
            })?;
        }

        let tokens_in = response.usage.as_ref().map(|u| u.input_tokens);
        let tokens_out = response.usage.as_ref().map(|u| u.output_tokens);

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
}
