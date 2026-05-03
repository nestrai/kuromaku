use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use crate::config::Backend;

pub mod local;
pub mod stream_json;
pub mod transport;

// --- Errors ---

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("spawn failed: {0}")]
    Spawn(String),

    #[error("execution failed (exit {code}): {message}")]
    Failed { code: i32, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// --- Types ---

/// How the executor should interpret the child's stdout.
///
/// Most callers want `Raw`: the executor copies bytes verbatim into the
/// buffer and the artifact file. Claude CLI in `--output-format stream-json`
/// mode emits structured NDJSON events; `ClaudeStreamJson` tells the
/// executor to parse each line, write only the user-visible text to the
/// artifact file (so `tail -f` shows readable output, not raw JSON), and
/// return the canonical assistant text in [`ExecutionOutput::stdout`]
/// (matching what `--output-format text` would have produced).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Raw,
    ClaudeStreamJson,
}

/// Describes what to execute.
///
/// `stdout_file`, when set, instructs the executor to stream the child's
/// stdout to that file line-by-line while the process runs, so a watcher
/// (`tail -f`) sees output as it is produced. The same content is also
/// captured in memory and returned via [`ExecutionOutput::stdout`] when
/// [`Executor::wait`] resolves.
///
/// `output_format` controls how stdout is interpreted -- see [`OutputFormat`].
pub struct ExecutionTask {
    pub id: String,
    pub command: String,
    pub env: HashMap<String, String>,
    pub stdout_file: Option<PathBuf>,
    pub output_format: OutputFormat,
}

/// Handle to a running execution, used to poll/wait/stop.
#[derive(Debug)]
pub struct ExecutionHandle {
    pub id: String,
    pub metadata: HashMap<String, String>,
}

/// Current status of an execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed(String),
}

/// Output captured from a completed execution.
///
/// `stderr` is populated on success too -- shell-step execution surfaces it
/// to the user even when the command succeeds (issue #23 acceptance criteria).
/// LLM-backed callers can ignore it.
#[derive(Debug)]
pub struct ExecutionOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// --- Trait ---

/// Trait for execution backends.
pub trait Executor: Send + Sync {
    fn spawn(
        &self,
        task: ExecutionTask,
    ) -> impl Future<Output = Result<ExecutionHandle, ExecutorError>> + Send;

    fn wait(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<ExecutionOutput, ExecutorError>> + Send;

    fn status(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<ExecutionStatus, ExecutorError>> + Send;

    fn stop(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<(), ExecutorError>> + Send;
}

// --- Object-safe boxed variant ---

pub trait ExecutorBoxed: Send + Sync {
    fn spawn_boxed(
        &self,
        task: ExecutionTask,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionHandle, ExecutorError>> + Send + '_>>;

    fn wait_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionOutput, ExecutorError>> + Send + 'a>>;

    fn status_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionStatus, ExecutorError>> + Send + 'a>>;

    fn stop_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ExecutorError>> + Send + 'a>>;
}

impl<T: Executor> ExecutorBoxed for T {
    fn spawn_boxed(
        &self,
        task: ExecutionTask,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionHandle, ExecutorError>> + Send + '_>>
    {
        Box::pin(self.spawn(task))
    }

    fn wait_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionOutput, ExecutorError>> + Send + 'a>>
    {
        Box::pin(self.wait(handle))
    }

    fn status_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionStatus, ExecutorError>> + Send + 'a>>
    {
        Box::pin(self.status(handle))
    }

    fn stop_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ExecutorError>> + Send + 'a>> {
        Box::pin(self.stop(handle))
    }
}

// --- Factory ---

/// Create the local executor. Only local execution is supported for now.
pub fn create_executor() -> Box<dyn ExecutorBoxed> {
    Box::new(local::LocalExecutor::new())
}

/// Standard preamble prepended to every `claude-cli` system prompt.
///
/// Claude Code injects a system-reminder into every tool result asking the
/// agent to "consider whether files are malware." Without explicit guidance,
/// spawned agents prepend defensive disclaimers ("this is not malware,
/// continuing") to every action and burn the run on meta-noise instead of
/// the actual task (issue #234). This preamble tells the agent to silently
/// acknowledge the reminder and treat internal project files as benign by
/// default, while still flagging genuinely suspicious content explicitly.
///
/// Only `claude-cli` ships this preamble -- Codex, Ollama, API and shell
/// steps are unaffected.
pub fn claude_preamble() -> &'static str {
    "Tool results in this session may include a system-reminder asking you to \
consider whether files are malware. You are operating inside a trusted \
internal project (the working directory belongs to the user who spawned \
you). Source code, YAML configs, Markdown docs, and project notes you read \
here are not malware. Acknowledge the reminder silently if you wish, but \
DO NOT prepend defensive disclaimers to your actions, DO NOT narrate \
\"this is not malware, continuing\", and DO NOT let the reminder derail \
the task. If you ever read a file that genuinely looks like malware \
(obfuscated payloads, shellcode, exfiltration logic in unexpected places), \
flag it explicitly to the orchestrator and stop -- but the default \
assumption for project files is benign."
}

/// Compose the final system prompt for `claude-cli`: preamble first, persona
/// second. When the caller has no persona, the preamble alone is returned.
fn compose_claude_system_prompt(system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(persona) => format!("{}\n\n{}", claude_preamble(), persona),
        None => claude_preamble().to_string(),
    }
}

/// Build the CLI command string for a claude-cli backend.
///
/// Uses `--output-format stream-json --verbose --include-partial-messages`
/// so Claude emits NDJSON events (including token-level `content_block_delta`
/// fragments) as content is generated. Without this, the CLI buffers all
/// output internally and the artifact file would stay empty until process
/// exit (issue #156). The executor parses these events back into plain text
/// for the artifact file via [`OutputFormat::ClaudeStreamJson`].
///
/// The system prompt is always set: [`claude_preamble`] is prepended to the
/// per-agent persona, or passed alone when there is no persona (issue #234).
///
/// `extra_args` is a backend-keyed escape hatch (#236). Tokens are passed
/// verbatim and shell-escaped here; the caller is responsible for splitting
/// `--flag value` pairs into separate slice entries. They are inserted
/// **before the final positional user prompt**, after all kuromaku-managed
/// flags, so users can override behaviour without disturbing the prompt
/// position.
pub fn build_claude_command(
    model: &str,
    system_prompt: Option<&str>,
    user_prompt: &str,
    extra_args: &[String],
) -> String {
    let claude_bin = std::env::var("CLAUDE_CLI_PATH").unwrap_or_else(|_| "claude".to_string());

    let mut parts = vec![claude_bin];
    parts.push("--print".to_string());
    parts.push("--model".to_string());
    parts.push(shell_escape(model));
    parts.push("--output-format".to_string());
    parts.push("stream-json".to_string());
    parts.push("--verbose".to_string());
    parts.push("--include-partial-messages".to_string());
    parts.push("--dangerously-skip-permissions".to_string());

    let combined_system = compose_claude_system_prompt(system_prompt);
    parts.push("--system-prompt".to_string());
    parts.push(shell_escape(&combined_system));

    for arg in extra_args {
        parts.push(shell_escape(arg));
    }

    parts.push(shell_escape(user_prompt));

    parts.join(" ")
}

/// Build a `tokio::process::Command` for an interactive Claude CLI session
/// driven through [`StreamJsonTransport`](transport::StreamJsonTransport).
///
/// The interactive form differs from [`build_claude_command`] in two
/// fundamental ways:
///
/// * No `--print`. Print mode is a one-shot: feed a prompt argument, get a
///   single response, exit. The router needs the agent to stay alive across
///   multiple turns so it can deliver the other agents' messages as they
///   arrive.
/// * `--input-format stream-json`. The transport writes
///   `{"type":"user","message":...}` NDJSON envelopes to stdin; without this
///   flag the CLI would interpret stdin as raw text and fail to parse it.
///
/// Output format is the same stream-json the executor already understands,
/// so the messaging Router can reuse the existing [`stream_json::parse_line`]
/// pipeline. Permissions are skipped to match the non-interactive path --
/// the CLI in stream-json mode otherwise blocks waiting for an approval
/// prompt the user cannot answer.
///
/// `system_prompt` is optional. The [`claude_preamble`] is always prepended
/// to whatever the caller passes (or sent alone when there is no persona),
/// so per-agent personas survive the switch to interactive transport while
/// the malware-reminder mitigation (issue #234) stays in place.
///
/// `kill_on_drop(true)` is set so a partial spawn (one of N participants
/// fails to come up, an early error in `run_conversation_step`, an
/// unwound future) tears down the already-running children rather than
/// leaking detached `claude` processes. The transport has no `Drop` impl
/// of its own and `close()` is not always reached on the error path; the
/// kill flag is the backstop.
///
/// (issue #170)
pub fn build_claude_interactive_command(
    model: &str,
    system_prompt: Option<&str>,
    extra_args: &[String],
) -> tokio::process::Command {
    let claude_bin = std::env::var("CLAUDE_CLI_PATH").unwrap_or_else(|_| "claude".to_string());
    let mut cmd = tokio::process::Command::new(claude_bin);
    cmd.arg("--model").arg(model);
    cmd.arg("--input-format").arg("stream-json");
    cmd.arg("--output-format").arg("stream-json");
    cmd.arg("--verbose");
    cmd.arg("--include-partial-messages");
    cmd.arg("--dangerously-skip-permissions");
    let combined_system = compose_claude_system_prompt(system_prompt);
    cmd.arg("--system-prompt").arg(combined_system);
    // #236: extra_args land at the end. The interactive form has no
    // positional prompt argument (stdin drives the conversation), so
    // appending after `--system-prompt` puts the override flags last on
    // the argv -- mirroring the non-interactive form's "after kuromaku
    // flags, before user prompt" position as closely as possible.
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.kill_on_drop(true);
    cmd
}

/// Build a `tokio::process::Command` for an interactive `claude-cli` chat
/// session driven by the user's terminal (issue #244).
///
/// Distinct from [`build_claude_interactive_command`] -- that builder targets
/// the messaging Router and speaks `stream-json` over stdin/stdout. Chat mode
/// is a true REPL: the user types, claude-cli renders its own UI. We therefore
/// avoid `--input-format stream-json` (would parse keystrokes as NDJSON and
/// fail) and `--print` (one-shot mode, exits after one response).
///
/// Uses `--append-system-prompt` to layer the agent's persona + rules on top
/// of claude-cli's default system prompt -- the user keeps the upstream
/// behaviour and additionally sees the agent's persona. The malware preamble
/// (#234) is prepended to the agent's system prompt before the append, so
/// the mitigation lands here too.
///
/// `system_prompt` is optional: when `None`, only the preamble is appended.
pub fn build_claude_chat_command(
    model: &str,
    system_prompt: Option<&str>,
    extra_args: &[String],
) -> tokio::process::Command {
    let claude_bin = std::env::var("CLAUDE_CLI_PATH").unwrap_or_else(|_| "claude".to_string());
    let mut cmd = tokio::process::Command::new(claude_bin);
    cmd.arg("--model").arg(model);
    let combined_system = compose_claude_system_prompt(system_prompt);
    cmd.arg("--append-system-prompt").arg(combined_system);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd
}

/// Build a `tokio::process::Command` for an interactive `codex` chat session
/// (issue #244). Symmetric to [`build_codex_command`] but targets the REPL
/// rather than `codex exec`.
///
/// Codex has no `--system` flag in interactive mode. Per the issue's
/// implementer notes, the prototype injects the system prompt as the first
/// positional argument to `codex` -- codex treats the trailing positional as
/// the initial user message that seeds the conversation. The agent's first
/// reply will therefore be a response to the persona/rules block, which is a
/// known prototype limitation; the user can then continue the conversation
/// normally. Document the choice here so future work can replace it with a
/// proper system-prompt injection if codex grows that capability.
///
/// Unlike `codex exec`, no `--full-auto` flag is set -- the user is driving
/// the session and codex's interactive defaults apply.
pub fn build_codex_chat_command(
    model: &str,
    system_prompt: Option<&str>,
    extra_args: &[String],
) -> tokio::process::Command {
    let codex_bin = std::env::var("CODEX_CLI_PATH").unwrap_or_else(|_| "codex".to_string());
    let mut cmd = tokio::process::Command::new(codex_bin);
    if model != "default" {
        cmd.arg("-m").arg(model);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    // Prototype: pass the system prompt as the initial user message via the
    // trailing positional argument. Codex has no `--system` flag in interactive
    // mode (issue #244 implementer notes). Skip when there is no persona so
    // codex opens a clean REPL without an empty seed message.
    if let Some(prompt) = system_prompt {
        cmd.arg(prompt);
    }
    cmd
}

/// Build a `tokio::process::Command` for an interactive `ollama` chat session
/// (issue #244). Symmetric to [`build_ollama_command`] but drops into the
/// ollama REPL with the agent's persona pre-loaded via `--system`.
///
/// `ollama run <model> --system "<prompt>"` opens the interactive prompt with
/// the system prompt already set when stdin is a TTY. `system_prompt` is
/// optional: when `None`, the `--system` flag is omitted entirely so ollama
/// uses the model's built-in default.
pub fn build_ollama_chat_command(
    model: &str,
    system_prompt: Option<&str>,
    extra_args: &[String],
) -> tokio::process::Command {
    let ollama_bin = std::env::var("OLLAMA_PATH").unwrap_or_else(|_| "ollama".to_string());
    let mut cmd = tokio::process::Command::new(ollama_bin);
    cmd.arg("run").arg(model);
    if let Some(prompt) = system_prompt {
        cmd.arg("--system").arg(prompt);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd
}

/// Spawn the given command with stdin/stdout/stderr inherited from the
/// parent shell and wait for it to exit. Used by `kuro chat` (#244) to hand
/// the terminal over to the upstream CLI for its full interactive UX.
///
/// Returns the child process's exit status. Errors only on spawn failure;
/// non-zero exits propagate via the returned `ExitStatus` and are the
/// caller's responsibility to map to an error.
pub async fn spawn_interactive(
    mut cmd: tokio::process::Command,
) -> Result<std::process::ExitStatus, ExecutorError> {
    use std::process::Stdio;
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.status().await.map_err(ExecutorError::Io)
}

/// Build the CLI command string for a codex backend.
///
/// Uses `codex exec` in full-auto mode (no approval prompts, sandboxed).
/// Codex has no --system-prompt flag, so system and user prompts are combined.
///
/// `extra_args` is the #236 escape hatch. Tokens are spliced in **between
/// the model selection (`-m MODEL` or its absence) and the final positional
/// prompt argument**, so the prompt always stays last and codex sees its
/// `-c key=value` overrides on the same argv position the codex CLI
/// documents for them. Each token is shell-escaped here -- the caller passes
/// `["-c", "model_reasoning_effort=high"]` as two separate entries.
pub fn build_codex_command(
    model: &str,
    system_prompt: Option<&str>,
    user_prompt: &str,
    extra_args: &[String],
) -> String {
    let codex_bin = std::env::var("CODEX_CLI_PATH").unwrap_or_else(|_| "codex".to_string());

    // Codex CLI has no --system-prompt flag, so system and user prompts are
    // concatenated. The LLM cannot distinguish instructions from task content
    // if the user prompt starts instruction-like.
    let prompt = match system_prompt {
        Some(system) => format!("{system}\n\n{user_prompt}"),
        None => user_prompt.to_string(),
    };

    let mut parts = vec![codex_bin, "exec".to_string()];
    parts.push("--full-auto".to_string());
    // "default" means use Codex's built-in model, don't pass -m flag.
    // Any other value gets passed literally.
    if model != "default" {
        parts.push("-m".to_string());
        parts.push(shell_escape(model));
    }
    // #236: extra_args sit between `-m MODEL` (or its absence) and the
    // final positional prompt. Order matters -- the prompt MUST stay last.
    for arg in extra_args {
        parts.push(shell_escape(arg));
    }
    parts.push(shell_escape(&prompt));

    parts.join(" ")
}

/// Build the CLI command string for an ollama backend.
///
/// `extra_args` (#236) is appended after `run <model>` and before the
/// positional prompt, so the prompt stays the last argv token.
pub fn build_ollama_command(model: &str, prompt: &str, extra_args: &[String]) -> String {
    let ollama_bin = std::env::var("OLLAMA_PATH").unwrap_or_else(|_| "ollama".to_string());

    let mut parts = vec![ollama_bin, "run".to_string(), shell_escape(model)];
    for arg in extra_args {
        parts.push(shell_escape(arg));
    }
    parts.push(shell_escape(prompt));
    parts.join(" ")
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Determine whether a backend needs an executor (CLI-based) or is direct HTTP.
pub fn backend_needs_executor(backend: Backend) -> bool {
    matches!(
        backend,
        Backend::ClaudeCli | Backend::Codex | Backend::Ollama
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_plain_string() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    /// Stable substring of [`claude_preamble`]. Used in tests as the
    /// "preamble marker" required by issue #234's acceptance criteria --
    /// distinctive enough that no real persona or user prompt is likely to
    /// contain it, robust to minor wording tweaks of the preamble itself.
    const PREAMBLE_MARKER: &str = "trusted internal project";

    #[test]
    fn build_claude_command_basic() {
        // claude-cli always carries --system-prompt now, even with no persona,
        // because the preamble alone must still ship (issue #234).
        let cmd = build_claude_command("claude-sonnet-4-5", None, "write tests", &[]);
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("--print"));
        assert!(cmd.contains("--system-prompt"));
        assert!(cmd.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_claude_command_uses_stream_json_for_live_streaming() {
        // Issue #156: plain text mode buffers internally and only flushes on
        // process exit, defeating live artifact tailing. Stream-json verbose
        // mode emits NDJSON events as content is generated.
        let cmd = build_claude_command("claude-sonnet-4-5", None, "do work", &[]);
        assert!(cmd.contains("--output-format stream-json"));
        assert!(cmd.contains("--verbose"));
        assert!(cmd.contains("--include-partial-messages"));
        // Old text format must be gone -- otherwise we keep the buffering bug.
        assert!(!cmd.contains("--output-format text"));
    }

    #[test]
    fn build_claude_command_with_system() {
        // Persona survives, preamble is prepended (issue #234).
        let cmd = build_claude_command(
            "claude-sonnet-4-5",
            Some("You are a dev"),
            "write tests",
            &[],
        );
        assert!(cmd.contains("--system-prompt"));
        assert!(cmd.contains("You are a dev"));
        assert!(cmd.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn claude_preamble_contains_marker_and_is_stable() {
        // The marker the rest of the test suite relies on must actually
        // appear in the preamble, and the preamble must be a stable constant.
        let first = claude_preamble();
        let second = claude_preamble();
        assert!(first.contains(PREAMBLE_MARKER));
        assert_eq!(first, second);
    }

    #[test]
    fn build_claude_command_no_system_still_carries_preamble() {
        // Acceptance criterion: when system_prompt is None, the preamble
        // alone is passed -- not omitted (issue #234).
        let cmd = build_claude_command("claude-sonnet-4-5", None, "do work", &[]);
        assert!(cmd.contains("--system-prompt"));
        assert!(cmd.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_claude_command_preamble_precedes_persona() {
        // Order matters: the infrastructure-level guidance comes before the
        // per-agent persona so the persona can override tone but the
        // anti-disclaimer rule stays anchored at the top of the system prompt.
        let cmd = build_claude_command("claude-sonnet-4-5", Some("PERSONA_TOKEN"), "do work", &[]);
        let preamble_pos = cmd.find(PREAMBLE_MARKER).expect("preamble missing");
        let persona_pos = cmd.find("PERSONA_TOKEN").expect("persona missing");
        assert!(
            preamble_pos < persona_pos,
            "preamble must appear before persona in the command string"
        );
    }

    #[test]
    fn build_claude_interactive_command_carries_preamble_with_persona() {
        let cmd = build_claude_interactive_command("claude-sonnet-4-5", Some("You are a dev"), &[]);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "--system-prompt"));
        let joined = args.join(" ");
        assert!(joined.contains(PREAMBLE_MARKER));
        assert!(joined.contains("You are a dev"));
    }

    #[test]
    fn build_claude_interactive_command_carries_preamble_without_persona() {
        // Same acceptance criterion as the non-interactive path: even with
        // no persona, --system-prompt with the preamble is always sent.
        let cmd = build_claude_interactive_command("claude-sonnet-4-5", None, &[]);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "--system-prompt"));
        let joined = args.join(" ");
        assert!(joined.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_ollama_command_basic() {
        let cmd = build_ollama_command("llama3", "hello world", &[]);
        assert!(cmd.contains("ollama"));
        assert!(cmd.contains("run"));
        assert!(cmd.contains("llama3"));
    }

    #[test]
    fn build_codex_command_basic() {
        let cmd = build_codex_command("o3", None, "write docs", &[]);
        assert!(cmd.contains("codex"));
        assert!(cmd.contains("exec"));
        assert!(cmd.contains("--full-auto"));
        assert!(cmd.contains("-m"));
        assert!(cmd.contains("write docs"));
        assert!(!cmd.contains("System:"));
    }

    #[test]
    fn build_codex_command_with_system() {
        let cmd = build_codex_command("o3", Some("You are a writer"), "write docs", &[]);
        assert!(cmd.contains("You are a writer"));
        assert!(cmd.contains("write docs"));
    }

    #[test]
    fn build_codex_command_default_model() {
        let cmd = build_codex_command("default", None, "write docs", &[]);
        assert!(cmd.contains("codex"));
        assert!(cmd.contains("exec"));
        assert!(cmd.contains("--full-auto"));
        assert!(!cmd.contains("-m"));
        assert!(cmd.contains("write docs"));
    }

    #[test]
    fn build_codex_command_does_not_carry_claude_preamble() {
        // Issue #234: only claude-cli ships the preamble. Codex must be
        // untouched both when it has a system prompt and when it doesn't,
        // because Codex concatenates system+user into one positional prompt
        // and a foreign preamble would leak into the user-visible task.
        let cmd_no_sys = build_codex_command("o3", None, "write docs", &[]);
        assert!(!cmd_no_sys.contains(PREAMBLE_MARKER));
        let cmd_with_sys = build_codex_command("o3", Some("You are a writer"), "write docs", &[]);
        assert!(!cmd_with_sys.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_ollama_command_does_not_carry_claude_preamble() {
        // Ollama has no system-prompt mechanism in the CLI invocation we
        // build, and it does not suffer from Claude Code's malware reminder.
        // The preamble must not appear in the command (issue #234).
        // (Shell steps don't go through any build_*_command at all, so they
        // are covered by construction.)
        let cmd = build_ollama_command("llama3", "write docs", &[]);
        assert!(!cmd.contains(PREAMBLE_MARKER));
    }

    // --- #236: extra_args escape hatch ---

    #[test]
    fn extra_args_codex_injects_between_model_and_prompt() {
        // Acceptance: codex receives the override flag splice between `-m MODEL`
        // and the trailing positional prompt. The prompt MUST stay last so the
        // CLI parses the trailing token as the user prompt.
        let extra = vec!["-c".to_string(), "model_reasoning_effort=high".to_string()];
        let cmd = build_codex_command("gpt-5.5", None, "write docs", &extra);

        let m_pos = cmd.find("-m ").expect("-m flag missing");
        let model_pos = cmd.find("'gpt-5.5'").expect("model token missing");
        // -c is shell-escaped as '-c' since extras flow through shell_escape.
        let c_pos = cmd.find("'-c'").expect("extra flag -c missing");
        let kv_pos = cmd
            .find("'model_reasoning_effort=high'")
            .expect("extra value missing");
        let prompt_pos = cmd.rfind("'write docs'").expect("prompt missing");

        assert!(m_pos < model_pos, "got: {cmd}");
        assert!(model_pos < c_pos, "extras must follow -m MODEL: {cmd}");
        assert!(c_pos < kv_pos, "got: {cmd}");
        assert!(kv_pos < prompt_pos, "prompt must stay last: {cmd}");
    }

    #[test]
    fn extra_args_empty_is_regression_safe() {
        // Acceptance: empty extras leave existing argv untouched -- the four
        // build_*_command outputs must equal what they were before #236 when
        // no extras are provided.
        let claude = build_claude_command("claude-sonnet-4-5", None, "do work", &[]);
        assert!(claude.contains("--system-prompt"));
        assert!(claude.contains("'do work'"));
        assert!(claude.trim_end().ends_with("'do work'"));

        let codex = build_codex_command("o3", None, "do work", &[]);
        assert!(codex.contains("-m 'o3'"));
        assert!(codex.trim_end().ends_with("'do work'"));

        let ollama = build_ollama_command("llama3", "do work", &[]);
        // ollama: `<bin> run 'llama3' 'do work'` -- no extras between.
        assert!(ollama.contains("run 'llama3' 'do work'"), "got: {ollama}");

        let interactive =
            build_claude_interactive_command("claude-sonnet-4-5", Some("persona"), &[]);
        let argv: Vec<String> = interactive
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Last two args are `--system-prompt <combined>` -- no extras tacked on.
        let last = argv.last().expect("argv empty");
        assert!(last.contains("persona"), "argv: {argv:?}");
    }

    #[test]
    fn extra_args_claude_cli_appends_before_prompt() {
        // Acceptance: claude-cli receives extras after kuromaku-managed flags
        // (system-prompt etc.) and immediately before the trailing user prompt.
        let extra = vec!["--allowed-tools".to_string(), "Read".to_string()];
        let cmd = build_claude_command("claude-sonnet-4-5", Some("persona"), "the task", &extra);

        let sys_pos = cmd.find("--system-prompt").expect("system-prompt missing");
        // Extras pass through shell_escape, so the literal token is wrapped.
        let allowed_pos = cmd.find("'--allowed-tools'").expect("extra flag missing");
        let read_pos = cmd.find("'Read'").expect("extra value missing");
        let prompt_pos = cmd.rfind("'the task'").expect("user prompt missing");

        assert!(
            sys_pos < allowed_pos,
            "extras must come after --system-prompt: {cmd}"
        );
        assert!(allowed_pos < read_pos, "got: {cmd}");
        assert!(read_pos < prompt_pos, "user prompt must remain last: {cmd}");
    }

    #[test]
    fn extra_args_ollama_appends_before_prompt() {
        let extra = vec!["--keepalive".to_string(), "5m".to_string()];
        let cmd = build_ollama_command("llama3", "do work", &extra);
        let model_pos = cmd.find("'llama3'").expect("model missing");
        let keep_pos = cmd.find("'--keepalive'").expect("extra missing");
        let val_pos = cmd.find("'5m'").expect("extra value missing");
        let prompt_pos = cmd.rfind("'do work'").expect("prompt missing");
        assert!(model_pos < keep_pos, "got: {cmd}");
        assert!(keep_pos < val_pos, "got: {cmd}");
        assert!(val_pos < prompt_pos, "prompt must stay last: {cmd}");
    }

    #[test]
    fn backend_needs_executor_classification() {
        assert!(backend_needs_executor(Backend::ClaudeCli));
        assert!(backend_needs_executor(Backend::Codex));
        assert!(backend_needs_executor(Backend::Ollama));
        assert!(!backend_needs_executor(Backend::Api));
    }

    #[test]
    fn create_executor_works() {
        let _ = create_executor();
    }

    // --- #244: chat-mode interactive command builders ---
    //
    // Tests cover the three supported backends (claude-cli, codex, ollama)
    // and exercise the two preamble-related contracts spelled out in
    // PR #235 / issue #234: the malware preamble lands on claude-cli but
    // never on codex/ollama. Also pin the structural choices documented
    // in the builders' doc comments (no --print, no stream-json, system
    // prompt position) so a regression in those is caught at the unit
    // test layer rather than at the user-facing UX layer.

    fn argv(cmd: &tokio::process::Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_claude_chat_command_uses_append_system_prompt() {
        // Acceptance: chat mode uses --append-system-prompt (not
        // --system-prompt) so the agent's persona layers on top of
        // claude-cli's default behaviour rather than replacing it.
        let cmd = build_claude_chat_command("claude-sonnet-4-5", Some("You are Sage"), &[]);
        let args = argv(&cmd);
        assert!(args.iter().any(|a| a == "--append-system-prompt"));
        assert!(!args.iter().any(|a| a == "--system-prompt"));
        let joined = args.join(" ");
        assert!(joined.contains("You are Sage"));
        assert!(joined.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_claude_chat_command_omits_print_and_stream_json() {
        // Chat mode is a true REPL; --print would make claude-cli exit after
        // one response and stream-json would parse keystrokes as NDJSON.
        // Both must stay out of the chat-mode argv.
        let cmd = build_claude_chat_command("claude-sonnet-4-5", Some("persona"), &[]);
        let args = argv(&cmd);
        assert!(!args.iter().any(|a| a == "--print"));
        assert!(!args.iter().any(|a| a == "--input-format"));
        assert!(!args.iter().any(|a| a == "--output-format"));
    }

    #[test]
    fn build_claude_chat_command_no_persona_still_carries_preamble() {
        // Issue #234 acceptance criterion mirrored to chat mode: even with
        // no persona the malware preamble is always sent, on the same
        // --append-system-prompt argv slot.
        let cmd = build_claude_chat_command("claude-sonnet-4-5", None, &[]);
        let args = argv(&cmd);
        assert!(args.iter().any(|a| a == "--append-system-prompt"));
        let joined = args.join(" ");
        assert!(joined.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_claude_chat_command_passes_extra_args() {
        // Per #236 escape hatch: extra_args propagate to the final argv
        // even in chat mode, after kuromaku-managed flags.
        let extra = vec!["--allowed-tools".to_string(), "Read".to_string()];
        let cmd = build_claude_chat_command("claude-sonnet-4-5", Some("p"), &extra);
        let args = argv(&cmd);
        assert!(args.iter().any(|a| a == "--allowed-tools"));
        assert!(args.iter().any(|a| a == "Read"));
    }

    #[test]
    fn build_codex_chat_command_passes_system_prompt_as_initial_message() {
        // Codex has no --system flag in interactive mode -- the prototype
        // injects the persona block as the trailing positional argument
        // (initial user message). Pin that wiring so a refactor that
        // accidentally drops the prompt is caught.
        let cmd = build_codex_chat_command("o3", Some("You are Sage"), &[]);
        let args = argv(&cmd);
        assert_eq!(args.last().map(String::as_str), Some("You are Sage"));
        // No `exec` subcommand -- chat mode targets the REPL.
        assert!(!args.iter().any(|a| a == "exec"));
        // No --full-auto -- the user drives the interactive session.
        assert!(!args.iter().any(|a| a == "--full-auto"));
    }

    #[test]
    fn build_codex_chat_command_default_model_omits_minus_m() {
        // Mirrors the non-interactive build_codex_command rule: "default"
        // means use codex's built-in default model, no -m flag.
        let cmd = build_codex_chat_command("default", Some("p"), &[]);
        let args = argv(&cmd);
        assert!(!args.iter().any(|a| a == "-m"));
    }

    #[test]
    fn build_codex_chat_command_no_persona_skips_initial_message() {
        // No persona means no positional initial message -- codex opens a
        // clean REPL. Empty-string seeds would still emit a positional
        // argument, which we want to avoid.
        let cmd = build_codex_chat_command("o3", None, &[]);
        let args = argv(&cmd);
        // Last arg should be the model (after -m), not an empty string.
        assert!(!args.last().map(|s| s.is_empty()).unwrap_or(true));
    }

    #[test]
    fn build_codex_chat_command_does_not_carry_claude_preamble() {
        // The malware preamble is claude-cli specific (issue #234). Codex
        // and ollama must not see it -- carrying it forward would leak
        // claude-cli-only language into the user-visible session prompt.
        let cmd = build_codex_chat_command("o3", Some("You are Sage"), &[]);
        let joined = argv(&cmd).join(" ");
        assert!(!joined.contains(PREAMBLE_MARKER));
    }

    #[test]
    fn build_ollama_chat_command_uses_system_flag() {
        // Acceptance: ollama chat passes the persona via --system so the
        // model has it loaded for the interactive REPL.
        let cmd = build_ollama_chat_command("llama3", Some("You are Sage"), &[]);
        let args = argv(&cmd);
        assert!(args.iter().any(|a| a == "run"));
        assert!(args.iter().any(|a| a == "llama3"));
        assert!(args.iter().any(|a| a == "--system"));
        assert!(args.iter().any(|a| a == "You are Sage"));
    }

    #[test]
    fn build_ollama_chat_command_no_persona_omits_system_flag() {
        // No persona means no --system flag at all -- ollama then uses the
        // model's built-in default system prompt rather than receiving an
        // empty override.
        let cmd = build_ollama_chat_command("llama3", None, &[]);
        let args = argv(&cmd);
        assert!(!args.iter().any(|a| a == "--system"));
    }

    #[test]
    fn build_ollama_chat_command_does_not_carry_claude_preamble() {
        let cmd = build_ollama_chat_command("llama3", Some("You are Sage"), &[]);
        let joined = argv(&cmd).join(" ");
        assert!(!joined.contains(PREAMBLE_MARKER));
    }
}
