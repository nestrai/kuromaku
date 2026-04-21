use serde::{Deserialize, Serialize};

use crate::config::Backend;

// --- Errors ---

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("API request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("API error ({status}): {message}")]
    ApiError { status: u16, message: String },

    #[error("missing environment variable: {0}")]
    MissingEnvVar(String),

    #[error("CLI execution failed: {0}")]
    CliExec(#[from] std::io::Error),

    #[error("CLI process exited with {code}: {stderr}")]
    CliError { code: i32, stderr: String },

    #[error("unexpected response format: {0}")]
    ResponseFormat(String),

    #[error("tmux error: {0}")]
    Tmux(String),
}

// --- Message types ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub model: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// --- Trait ---

pub trait LlmClient: Send + Sync {
    fn send(
        &self,
        request: LlmRequest,
    ) -> impl std::future::Future<Output = Result<LlmResponse, LlmError>> + Send;
}

// --- Factory ---

pub fn create_client(backend: Backend, flow_name: &str, stage_id: &str) -> Box<dyn LlmClientBoxed> {
    match backend {
        Backend::Api => Box::new(ApiClient::from_env()),
        Backend::ClaudeCli => Box::new(TmuxCliClient::new(flow_name, stage_id)),
        Backend::Ollama => Box::new(CliClient::ollama()),
    }
}

/// Object-safe wrapper so we can use `Box<dyn LlmClientBoxed>`.
pub trait LlmClientBoxed: Send + Sync {
    fn send_boxed(
        &self,
        request: LlmRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<LlmResponse, LlmError>> + Send + '_>,
    >;
}

impl<T: LlmClient> LlmClientBoxed for T {
    fn send_boxed(
        &self,
        request: LlmRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<LlmResponse, LlmError>> + Send + '_>,
    > {
        Box::pin(self.send(request))
    }
}

// --- ApiClient (Anthropic Messages API) ---

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct ApiClient {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| ANTHROPIC_API_URL.to_string()),
            http: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
        Self::new(api_key, base_url)
    }
}

// Anthropic API request/response shapes

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    model: String,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: String,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize)]
struct AnthropicError {
    error: AnthropicErrorBody,
}

#[derive(Deserialize)]
struct AnthropicErrorBody {
    message: String,
}

impl LlmClient for ApiClient {
    async fn send(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::MissingEnvVar("ANTHROPIC_API_KEY".to_string()));
        }

        let messages: Vec<AnthropicMessage> = request
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| AnthropicMessage {
                role: match m.role {
                    Role::User => "user".to_string(),
                    Role::Assistant => "assistant".to_string(),
                    Role::System => unreachable!(),
                },
                content: m.content.clone(),
            })
            .collect();

        let body = AnthropicRequest {
            model: request.model,
            max_tokens: request.max_tokens,
            system: request.system,
            messages,
        };

        let resp = self
            .http
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status != 200 {
            let error_body = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<AnthropicError>(&error_body)
                .map(|e| e.error.message)
                .unwrap_or(error_body);
            return Err(LlmError::ApiError { status, message });
        }

        let api_resp: AnthropicResponse = resp.json().await?;
        let content = api_resp
            .content
            .into_iter()
            .map(|c| c.text)
            .collect::<Vec<_>>()
            .join("");

        Ok(LlmResponse {
            content,
            model: api_resp.model,
            usage: Some(Usage {
                input_tokens: api_resp.usage.input_tokens,
                output_tokens: api_resp.usage.output_tokens,
            }),
        })
    }
}

// --- TmuxCliClient (claude CLI in a tmux session) ---

#[derive(Debug, Clone)]
pub struct TmuxCliClient {
    session_name: String,
    claude_bin: String,
}

impl TmuxCliClient {
    pub fn new(flow_name: &str, stage_id: &str) -> Self {
        Self {
            session_name: format!("koto-{flow_name}-{stage_id}"),
            claude_bin: std::env::var("CLAUDE_CLI_PATH").unwrap_or_else(|_| "claude".to_string()),
        }
    }

    fn build_claude_command(&self, request: &LlmRequest) -> String {
        let mut parts = vec![self.claude_bin.clone()];
        parts.push("--model".to_string());
        parts.push(shell_escape(&request.model));
        parts.push("--output-format".to_string());
        parts.push("text".to_string());

        if let Some(ref system) = request.system {
            parts.push("--system-prompt".to_string());
            parts.push(shell_escape(system));
        }

        if let Some(last_user) = request.messages.iter().rev().find(|m| m.role == Role::User) {
            parts.push("--prompt".to_string());
            parts.push(shell_escape(&last_user.content));
        }

        parts.join(" ")
    }
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl LlmClient for TmuxCliClient {
    async fn send(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        let model = request.model.clone();
        let output_file = std::env::temp_dir().join(format!("{}.out", self.session_name));

        // Build the command that writes output to a temp file
        let claude_cmd = self.build_claude_command(&request);
        let tmux_cmd = format!("{claude_cmd} > '{}' 2>&1", output_file.display());

        // Print tmux session info
        eprintln!("      tmux session: {}", self.session_name);
        eprintln!("      attach: tmux attach -t {}", self.session_name);

        // Kill any existing session with this name (ignore errors)
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &self.session_name])
            .output()
            .await;

        // Create new tmux session running the command
        let create_output = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &self.session_name, &tmux_cmd])
            .output()
            .await?;

        if !create_output.status.success() {
            let stderr = String::from_utf8_lossy(&create_output.stderr).to_string();
            return Err(LlmError::Tmux(format!(
                "failed to create tmux session '{}': {}",
                self.session_name, stderr
            )));
        }

        // Wait for the tmux session to finish
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let check = tokio::process::Command::new("tmux")
                .args(["has-session", "-t", &self.session_name])
                .output()
                .await?;

            if !check.status.success() {
                // Session ended
                break;
            }
        }

        // Read the output file
        let content = tokio::fs::read_to_string(&output_file).await.map_err(|e| {
            LlmError::Tmux(format!(
                "failed to read output from tmux session '{}': {}",
                self.session_name, e
            ))
        })?;

        // Clean up the temp file
        let _ = tokio::fs::remove_file(&output_file).await;

        let content = content.trim().to_string();
        Ok(LlmResponse {
            content,
            model,
            usage: None,
        })
    }
}

// --- CliClient (ollama, simple subprocess) ---

#[derive(Debug, Clone)]
pub struct CliClient {
    command: String,
}

impl CliClient {
    pub fn ollama() -> Self {
        Self {
            command: std::env::var("OLLAMA_PATH").unwrap_or_else(|_| "ollama".to_string()),
        }
    }

    fn build_command(&self, request: &LlmRequest) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.arg("run").arg(&request.model);
        // Ollama takes prompt as the final positional arg
        let mut prompt = String::new();
        if let Some(ref system) = request.system {
            prompt.push_str(&format!("System: {system}\n\n"));
        }
        for msg in &request.messages {
            match msg.role {
                Role::User => prompt.push_str(&format!("User: {}\n\n", msg.content)),
                Role::Assistant => prompt.push_str(&format!("Assistant: {}\n\n", msg.content)),
                Role::System => {}
            }
        }
        cmd.arg(prompt.trim());
        cmd
    }
}

impl LlmClient for CliClient {
    async fn send(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        let model = request.model.clone();
        let mut cmd = self.build_command(&request);
        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(LlmError::CliError {
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        let content = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(LlmResponse {
            content,
            model,
            usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_client_from_env_defaults() {
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        let client = ApiClient::from_env();
        assert!(client.api_key.is_empty());
        assert_eq!(client.base_url, ANTHROPIC_API_URL);
    }

    #[test]
    fn api_client_custom_base_url() {
        let client = ApiClient::new(
            "test-key".to_string(),
            Some("http://localhost:8080".to_string()),
        );
        assert_eq!(client.api_key, "test-key");
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[tokio::test]
    async fn api_client_missing_key_returns_error() {
        let client = ApiClient::new(String::new(), None);
        let request = LlmRequest {
            model: "claude-sonnet-4-5-20250514".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: "hello".to_string(),
            }],
            max_tokens: 100,
        };
        let err = client.send(request).await.unwrap_err();
        assert!(
            matches!(err, LlmError::MissingEnvVar(_)),
            "expected MissingEnvVar, got: {err}"
        );
    }

    #[test]
    fn tmux_client_session_name() {
        let client = TmuxCliClient::new("planning", "design");
        assert_eq!(client.session_name, "koto-planning-design");
    }

    #[test]
    fn tmux_client_builds_command() {
        let client = TmuxCliClient::new("planning", "design");
        let request = LlmRequest {
            model: "claude-sonnet-4-5-20250514".to_string(),
            system: Some("You are a dev".to_string()),
            messages: vec![Message {
                role: Role::User,
                content: "write tests".to_string(),
            }],
            max_tokens: 1000,
        };
        let cmd = client.build_claude_command(&request);
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("--system-prompt"));
        assert!(cmd.contains("--prompt"));
    }

    #[test]
    fn shell_escape_handles_quotes() {
        assert_eq!(shell_escape("hello"), "'hello'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn ollama_client_builds_command() {
        let client = CliClient::ollama();
        let request = LlmRequest {
            model: "llama3".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: "hello".to_string(),
            }],
            max_tokens: 1000,
        };
        let cmd = client.build_command(&request);
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert!(prog.contains("ollama"), "expected ollama, got: {prog}");
    }

    #[test]
    fn create_client_returns_correct_backend() {
        let _ = create_client(Backend::Api, "test", "stage");
        let _ = create_client(Backend::ClaudeCli, "test", "stage");
        let _ = create_client(Backend::Ollama, "test", "stage");
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            role: Role::User,
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[test]
    fn message_deserialization() {
        let json = r#"{"role":"assistant","content":"hi there"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "hi there");
    }
}
