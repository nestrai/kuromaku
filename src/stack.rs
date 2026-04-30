use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StackError {
    #[error("failed to read stack: {0}")]
    Read(std::io::Error),

    #[error("failed to write stack: {0}")]
    Write(std::io::Error),

    #[error("failed to create stack directory: {0}")]
    CreateDir(std::io::Error),

    #[error("failed to serialize stack: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("failed to serialize manifest: {0}")]
    SerializeYaml(#[from] serde_yaml::Error),

    #[error("step '{0}' not found in run directory")]
    StepNotFound(String),
}

// --- Legacy flat-file layout (kept for backward compat with pre-#31 stacks).
//
// The runner no longer writes these files; new runs use the run-directory
// layout below. The legacy reader stays so future `kuro show` (#92) can
// surface old stacks without rewriting them.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutput {
    pub step_id: String,
    pub agent_id: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: String,
}

/// Ensure a stack/run directory exists. Used by both layouts.
pub fn ensure_dir(stack_path: &Path) -> Result<(), StackError> {
    if !stack_path.exists() {
        std::fs::create_dir_all(stack_path).map_err(StackError::CreateDir)?;
    }
    Ok(())
}

/// Legacy: write a step result as JSON to `<stack_path>/<step_id>.json`.
/// Retained so old test fixtures and the legacy reader keep working.
#[allow(dead_code)]
pub fn write_step(stack_path: &Path, output: &StepOutput) -> Result<(), StackError> {
    ensure_dir(stack_path)?;
    let file_path = stack_path.join(format!("{}.json", output.step_id));
    let json = serde_json::to_string_pretty(output)?;
    std::fs::write(&file_path, json).map_err(StackError::Write)
}

/// Legacy: read a step from a flat-file stack (`<stack_path>/<step_id>.json`).
/// New runs use [`read_run_step_content`] against a run directory instead.
#[allow(dead_code)]
pub fn read_step(stack_path: &Path, step_id: &str) -> Result<StepOutput, StackError> {
    let file_path = stack_path.join(format!("{step_id}.json"));
    let contents = std::fs::read_to_string(&file_path).map_err(StackError::Read)?;
    let output: StepOutput = serde_json::from_str(&contents)?;
    Ok(output)
}

// --- New run-directory layout (issue #31).

/// Subdirectory under a run directory that holds per-step content and meta
/// files. Pinned by the issue #31 spec, so it's a constant rather than
/// configuration.
pub const STEPS_SUBDIR: &str = "steps";

/// Subdirectory under a run directory reserved for inter-agent messages
/// (issue #153). Created empty at run start so consumers can rely on its
/// presence even before any messages exist.
pub const MESSAGES_SUBDIR: &str = "messages";

/// Initialise the on-disk layout for a run directory: the run dir itself,
/// the `steps/` subdir, and the `messages/` subdir. Idempotent -- safe to
/// call from multiple sites (main.rs at run start, run_steps when the task
/// flow path skips main's setup).
pub fn init_run_layout(run_path: &Path) -> Result<(), StackError> {
    ensure_dir(run_path)?;
    ensure_dir(&run_path.join(STEPS_SUBDIR))?;
    ensure_dir(&run_path.join(MESSAGES_SUBDIR))?;
    Ok(())
}

/// Per-step metadata, serialised as `NN-<step-id>.meta.yaml` next to the
/// content file. Captures everything needed to reconstruct what ran without
/// re-parsing the content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_id: String,
    /// `"llm"` for agent steps, `"shell"` for `run:` steps.
    #[serde(rename = "type")]
    pub kind: String,
    /// LLM steps: agent name. Shell steps: empty string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// LLM steps: requested model identifier. Shell steps: `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requested: Option<String>,
    /// What the executor actually used. Today this mirrors `model_requested`
    /// because no backend reports a server-side concrete model id back to us.
    /// Kept as a separate field so the audit format is stable when that
    /// information becomes available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_actual: Option<String>,
    /// Backend label (`api`, `claude-cli`, `codex`, `ollama`, `shell`).
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u32>,
    pub duration_ms: u128,
    pub started_at: String,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_steps: Vec<String>,
    /// Filename of the content file inside the run's `steps/` subdirectory
    /// (e.g. `01-fetch.md`). The `steps/` segment is mandated by the spec
    /// (issue #31) and added by readers/writers, not stored here.
    pub output_file: String,
    /// Per-agent breakdown for `kind: conversation` steps (issue #170
    /// acceptance criterion: "list of agents, turns taken, tokens per
    /// agent"). Empty for non-conversation steps; `skip_serializing_if`
    /// keeps `meta.yaml` for agent/shell steps unchanged so existing audit
    /// consumers stay backward compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<ParticipantStat>,
    /// Total agent turns for `kind: conversation` steps (issue #172
    /// manifest summary: matches the sum of `participants[].turns`). Kept
    /// as a separate field so audit consumers do not have to re-aggregate
    /// the participants vector for the headline number. `None` for
    /// non-conversation steps so existing meta.yaml output stays
    /// unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    /// Total messages written to the audit log for `kind: conversation`
    /// steps (issue #172). The count comes from the
    /// [`MessageLogWriter`](crate::messaging::audit::MessageLogWriter), so
    /// it reflects what was actually flushed to disk -- a write failure
    /// would not inflate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<u32>,
    /// Termination reason for `kind: conversation` steps, rendered via
    /// [`TerminationReason::Display`](crate::messaging::router::TerminationReason)
    /// so the on-disk string stays frozen across variant renames
    /// (`max_turns`, `convergence`, `timeout`, `human_closed`,
    /// `all_agents_closed`). `None` for non-conversation steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminated_by: Option<String>,
}

/// One row of conversation-step participant statistics.
///
/// Filled by the runner from the router's log entries (turns) plus the
/// agent definition (model). Tokens are an `Option` because per-agent token
/// accounting is not yet wired through the messaging Router; recording
/// `None` is honest about that gap, recording `0` would lie.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantStat {
    /// Agent ID, matching the entry in the step's `agents:` list.
    pub agent: String,
    /// Model identifier as configured in the agent file. Mirrors
    /// `StepRecord::model_requested` for agent steps but lives per-row here
    /// since a conversation can mix models across participants.
    pub model: String,
    /// Number of `MessageKind::Final` results this agent emitted during the
    /// conversation. Tool-use entries and streaming partials do not count.
    pub turns: usize,
    /// Per-agent input tokens. `None` until the executor/transport surfaces
    /// per-message token counts to the router.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u32>,
    /// Per-agent output tokens. Same caveat as `tokens_in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u32>,
}

/// Resource loaded from a seed (agent file, rules file, guide). Hashed so the
/// manifest can pin "what exactly was used".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRecord {
    /// `"agent"`, `"rules"`, `"guide"`, `"flow"`.
    pub kind: String,
    pub name: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedRecord {
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleResolution {
    pub role: String,
    pub agent: String,
    pub model: String,
    pub backend: String,
    pub model_source: String,
    pub backend_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub run_id: String,
    pub flow_name: String,
    pub flow_path: String,
    pub flow_sha256: String,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u128,
    pub total_tokens_in: u32,
    pub total_tokens_out: u32,
    /// Cost is intentionally `null` -- kuromaku does not currently price runs.
    /// Reserved so future audits can populate it without a schema bump.
    pub cost: Option<f64>,
    #[serde(default)]
    pub vars: indexmap::IndexMap<String, String>,
    pub seeds: Vec<SeedRecord>,
    pub resources: Vec<ResourceRecord>,
    pub roles: Vec<RoleResolution>,
    pub steps: Vec<StepRecord>,
}

/// Compute the SHA-256 of arbitrary bytes, returned as a lowercase hex string.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Filename for a step's content file: `01-design.md`, `02-review.md`, ...
/// `ext` should be `"md"` or `"txt"` -- callers pick based on step type.
pub fn step_content_filename(step_num: usize, step_id: &str, ext: &str) -> String {
    format!("{step_num:02}-{step_id}.{ext}")
}

/// Filename for a step's metadata file: `01-design.meta.yaml`.
pub fn step_meta_filename(step_num: usize, step_id: &str) -> String {
    format!("{step_num:02}-{step_id}.meta.yaml")
}

/// Write a step's content and metadata into the run directory. Returns the
/// content file path so the caller can stream subsequent stdout into it (the
/// path matches what was written).
///
/// The content is written verbatim; for executor backends the file is usually
/// already present from streaming, in which case we still overwrite to make
/// sure final content matches what the manifest will reference.
pub fn write_run_step(
    run_path: &Path,
    step_num: usize,
    record: &StepRecord,
    content: &str,
) -> Result<PathBuf, StackError> {
    // Defensive: callers should have called `init_run_layout`, but we ensure
    // the steps subdir here too so a forgotten init doesn't cost the user a
    // run. `output_file` is just the filename -- the `steps/` segment lives
    // here.
    let steps_dir = run_path.join(STEPS_SUBDIR);
    ensure_dir(&steps_dir)?;

    // Content extension is part of the StepRecord's output_file so the writer
    // and manifest stay in sync with no second source of truth.
    let content_path = steps_dir.join(&record.output_file);
    std::fs::write(&content_path, content).map_err(StackError::Write)?;

    let meta_path = steps_dir.join(step_meta_filename(step_num, &record.step_id));
    let yaml = serde_yaml::to_string(record)?;
    std::fs::write(&meta_path, yaml).map_err(StackError::Write)?;

    Ok(content_path)
}

/// Read a prior step's content from the run directory. Looks up the meta file
/// by step id (since step numbering depends on the topo order, not the id) and
/// returns the body of the referenced content file.
///
/// The match anchors on the `NN-` numeric prefix: a meta file is a candidate
/// only when stripping a leading run of digits and one `-` leaves
/// `<step_id>.meta.yaml` exactly. This avoids `pre-fetch.meta.yaml` matching
/// when `fetch` is requested (real collisions surfaced this in tests).
pub fn read_run_step_content(run_path: &Path, step_id: &str) -> Result<String, StackError> {
    let steps_dir = run_path.join(STEPS_SUBDIR);
    let entries = std::fs::read_dir(&steps_dir).map_err(StackError::Read)?;
    let target = format!("{step_id}.meta.yaml");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        // Strip leading digits, then a single '-'. If anything else is left,
        // skip -- this is not a per-step meta file.
        let after_digits = s.trim_start_matches(|c: char| c.is_ascii_digit());
        let Some(rest) = after_digits.strip_prefix('-') else {
            continue;
        };
        if rest != target {
            continue;
        }
        let yaml = std::fs::read_to_string(entry.path()).map_err(StackError::Read)?;
        let rec: StepRecord = serde_yaml::from_str(&yaml)?;
        let content =
            std::fs::read_to_string(steps_dir.join(&rec.output_file)).map_err(StackError::Read)?;
        return Ok(content);
    }
    Err(StackError::StepNotFound(step_id.to_string()))
}

// --- Read API for external consumers (MCP `show_output`, `kuro show`).
//
// The team review on issue #198 forbids path parsing in `src/mcp/`. Anything
// that wants to read a finished or in-flight run goes through these helpers
// so the on-disk layout stays a single-source-of-truth concern owned by
// `stack`.

/// Lifecycle of a run as observable from disk.
///
/// `Done`: a `manifest.yaml` exists at the run root -- the run finished
/// cleanly and every step that was supposed to write content did.
///
/// `Running`: the run directory exists but the manifest does not. We treat
/// this as "still in flight". A run that crashed mid-flight (process killed,
/// panic) sits in this state forever today; an explicit failure marker is
/// follow-up work tracked alongside #198.
///
/// `Failed`: reserved for the future explicit failure marker. Returned today
/// only by callers that surface the variant via API contract -- `read_run`
/// itself never produces it.
///
/// `NotFound`: no run directory at the expected path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Done,
    /// Reserved for the future explicit failure marker. The variant is
    /// public so the API surface stays stable when the marker lands; today
    /// no code path constructs it.
    #[allow(dead_code)]
    Failed,
    NotFound,
}

impl RunStatus {
    /// Stable wire string. Used by MCP `show_output` and any other consumer
    /// that needs to surface the status as a JSON value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::NotFound => "not_found",
        }
    }
}

/// One step's content as exposed by [`read_run`]. `content` is `None` for
/// steps that have a meta file but no readable output file yet (the meta is
/// written first; in-progress conversation steps live in this state for a
/// while).
#[derive(Debug, Clone)]
pub struct RunStepOutput {
    pub step_id: String,
    pub content: Option<String>,
}

/// Aggregated read of a run directory. `status` is always set; `steps` is
/// empty for `NotFound` and may be partial for `Running`.
#[derive(Debug, Clone)]
pub struct RunOutputs {
    pub run_id: String,
    pub status: RunStatus,
    pub steps: Vec<RunStepOutput>,
}

/// Read a run directory by id, optionally filtering to a single step.
///
/// `stack_path` is the project's stack root (`~/.koto/stacks/<project>/`).
/// The function joins `run_id` and walks the steps subdirectory. It never
/// surfaces the run's filesystem path to callers -- that is the team review
/// rule for #198.
///
/// Errors only on truly broken on-disk state (corrupt meta YAML, unreadable
/// content file). Missing run dir is `NotFound`, missing manifest is
/// `Running` -- both return `Ok`.
pub fn read_run(
    stack_path: &Path,
    run_id: &str,
    step: Option<&str>,
) -> Result<RunOutputs, StackError> {
    let run_path = stack_path.join(run_id);
    if !run_path.is_dir() {
        return Ok(RunOutputs {
            run_id: run_id.to_string(),
            status: RunStatus::NotFound,
            steps: Vec::new(),
        });
    }
    let status = if run_path.join("manifest.yaml").is_file() {
        RunStatus::Done
    } else {
        RunStatus::Running
    };
    let steps = collect_run_steps(&run_path, step)?;
    Ok(RunOutputs {
        run_id: run_id.to_string(),
        status,
        steps,
    })
}

/// Collect step outputs from `<run_path>/steps/`, ordered by the `NN-`
/// numeric prefix (i.e. topological execution order). When `filter` is
/// `Some`, only the matching step id is returned -- callers get an empty
/// vector if the requested step has not been recorded yet (the caller can
/// distinguish "step missing" from "run missing" via the run-level status).
fn collect_run_steps(
    run_path: &Path,
    filter: Option<&str>,
) -> Result<Vec<RunStepOutput>, StackError> {
    let steps_dir = run_path.join(STEPS_SUBDIR);
    if !steps_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut metas: Vec<(u32, StepRecord)> = Vec::new();
    for entry in std::fs::read_dir(&steps_dir)
        .map_err(StackError::Read)?
        .flatten()
    {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        // Same `NN-<id>.meta.yaml` shape as `read_run_step_content`. Steps
        // without a numeric prefix are not part of the topological run
        // record and are skipped.
        let after_digits = s.trim_start_matches(|c: char| c.is_ascii_digit());
        let digits_len = s.len() - after_digits.len();
        if digits_len == 0 {
            continue;
        }
        let Some(rest) = after_digits.strip_prefix('-') else {
            continue;
        };
        let Some(step_id) = rest.strip_suffix(".meta.yaml") else {
            continue;
        };
        if let Some(want) = filter
            && want != step_id
        {
            continue;
        }
        let order: u32 = s[..digits_len].parse().unwrap_or(u32::MAX);
        let yaml = std::fs::read_to_string(entry.path()).map_err(StackError::Read)?;
        let rec: StepRecord = serde_yaml::from_str(&yaml)?;
        metas.push((order, rec));
    }
    metas.sort_by_key(|(order, _)| *order);
    let mut out: Vec<RunStepOutput> = Vec::with_capacity(metas.len());
    for (_order, rec) in metas {
        let content_path = steps_dir.join(&rec.output_file);
        let content = match std::fs::read_to_string(&content_path) {
            Ok(c) => Some(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(StackError::Read(e)),
        };
        out.push(RunStepOutput {
            step_id: rec.step_id,
            content,
        });
    }
    Ok(out)
}

/// Write the run manifest at `<run_path>/manifest.yaml`. Called once at the
/// end of a successful run.
pub fn write_manifest(run_path: &Path, manifest: &Manifest) -> Result<(), StackError> {
    ensure_dir(run_path)?;
    let yaml = serde_yaml::to_string(manifest)?;
    let path = run_path.join("manifest.yaml");
    std::fs::write(path, yaml).map_err(StackError::Write)
}

/// Write the resolution audit text at `<run_path>/resolution-audit.txt`.
pub fn write_resolution_audit(run_path: &Path, text: &str) -> Result<(), StackError> {
    ensure_dir(run_path)?;
    std::fs::write(run_path.join("resolution-audit.txt"), text).map_err(StackError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_output() -> StepOutput {
        StepOutput {
            step_id: "design".to_string(),
            agent_id: "architect".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            prompt: "Design the system".to_string(),
            response: "Here is the design...".to_string(),
            timestamp: "2026-04-20T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn legacy_write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let stack_path = dir.path().join("stack");

        let output = sample_output();
        write_step(&stack_path, &output).unwrap();

        let loaded = read_step(&stack_path, "design").unwrap();
        assert_eq!(loaded.step_id, "design");
        assert_eq!(loaded.agent_id, "architect");
        assert_eq!(loaded.response, "Here is the design...");
    }

    #[test]
    fn legacy_read_nonexistent_step_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_step(dir.path(), "ghost").unwrap_err();
        assert!(matches!(err, StackError::Read(_)));
    }

    #[test]
    fn legacy_write_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let stack_path = dir.path().join("nested").join("stack");

        let output = sample_output();
        write_step(&stack_path, &output).unwrap();
        assert!(stack_path.join("design.json").exists());
    }

    // --- new run-directory layout ---

    #[test]
    fn sha256_hex_known_vector() {
        // Empty input -> well-known SHA-256 digest.
        let h = sha256_hex(b"");
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn step_filenames_zero_padded() {
        assert_eq!(step_content_filename(1, "fetch", "md"), "01-fetch.md");
        assert_eq!(step_content_filename(12, "review", "txt"), "12-review.txt");
        assert_eq!(step_meta_filename(3, "design"), "03-design.meta.yaml");
    }

    fn record(step_num: usize, step_id: &str, ext: &str) -> StepRecord {
        StepRecord {
            step_id: step_id.to_string(),
            kind: "llm".to_string(),
            agent: Some("Levi".to_string()),
            model_requested: Some("claude-sonnet-4-5".to_string()),
            model_actual: Some("claude-sonnet-4-5".to_string()),
            backend: "claude-cli".to_string(),
            tokens_in: Some(1200),
            tokens_out: Some(800),
            duration_ms: 4500,
            started_at: "2026-04-29T10:00:00Z".to_string(),
            exit_code: 0,
            input_steps: vec![],
            output_file: step_content_filename(step_num, step_id, ext),
            participants: Vec::new(),
            turns: None,
            messages: None,
            terminated_by: None,
        }
    }

    #[test]
    fn write_and_read_run_step_roundtrip() {
        // Issue #31 layout: step content and meta live in `<run>/steps/`,
        // not directly in the run directory.
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("dev-20260429-100000");

        let rec = record(1, "design", "md");
        let path = write_run_step(&run_path, 1, &rec, "# Design\nbody").unwrap();
        assert_eq!(path, run_path.join("steps").join("01-design.md"));
        assert!(run_path.join("steps").join("01-design.meta.yaml").exists());
        // Content must NOT live at the run root -- that was the #159 bug.
        assert!(!run_path.join("01-design.md").exists());

        let body = read_run_step_content(&run_path, "design").unwrap();
        assert_eq!(body, "# Design\nbody");
    }

    #[test]
    fn read_run_step_not_found_errors() {
        let dir = tempfile::tempdir().unwrap();
        // The reader scans the `steps/` subdir; without it the read errors
        // at the directory layer rather than reporting StepNotFound.
        init_run_layout(dir.path()).unwrap();
        let err = read_run_step_content(dir.path(), "ghost").unwrap_err();
        assert!(matches!(err, StackError::StepNotFound(_)));
    }

    #[test]
    fn init_run_layout_creates_subdirs() {
        // The run-start hook must create `steps/` and an empty `messages/`
        // (issue #159, #153 prep). Idempotent so callers can invoke it
        // defensively without checking first.
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("flow-20260429-100000");

        init_run_layout(&run_path).unwrap();
        assert!(run_path.join(STEPS_SUBDIR).is_dir());
        assert!(run_path.join(MESSAGES_SUBDIR).is_dir());

        // Idempotent: a second call must not fail.
        init_run_layout(&run_path).unwrap();
    }

    #[test]
    fn read_run_step_disambiguates_by_id_suffix() {
        // Two steps whose ids share a substring must not collide. The lookup
        // uses `-<id>.meta.yaml` so `pre-fetch` and `fetch` resolve to
        // distinct files.
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("run");
        write_run_step(&run_path, 1, &record(1, "pre-fetch", "txt"), "alpha").unwrap();
        write_run_step(&run_path, 2, &record(2, "fetch", "md"), "beta").unwrap();

        assert_eq!(read_run_step_content(&run_path, "fetch").unwrap(), "beta");
        assert_eq!(
            read_run_step_content(&run_path, "pre-fetch").unwrap(),
            "alpha"
        );
    }

    #[test]
    fn meta_yaml_has_step_fields() {
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("run");
        let rec = record(1, "design", "md");
        write_run_step(&run_path, 1, &rec, "body").unwrap();

        let meta =
            std::fs::read_to_string(run_path.join("steps").join("01-design.meta.yaml")).unwrap();
        assert!(meta.contains("step_id: design"));
        assert!(meta.contains("type: llm"));
        assert!(meta.contains("backend: claude-cli"));
        assert!(meta.contains("model_requested: claude-sonnet-4-5"));
    }

    #[test]
    fn meta_yaml_started_at_differs_per_step() {
        // Audit promise: each step records its own wall-clock start, not the
        // run's start. Two steps written with distinct `started_at` values
        // must surface those distinct values in their meta files.
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("run");

        let mut first = record(1, "design", "md");
        first.started_at = "2026-04-29T10:00:00Z".to_string();
        write_run_step(&run_path, 1, &first, "body").unwrap();

        let mut second = record(2, "build", "md");
        second.started_at = "2026-04-29T10:00:42Z".to_string();
        write_run_step(&run_path, 2, &second, "body").unwrap();

        let meta1 =
            std::fs::read_to_string(run_path.join("steps").join("01-design.meta.yaml")).unwrap();
        let meta2 =
            std::fs::read_to_string(run_path.join("steps").join("02-build.meta.yaml")).unwrap();
        assert!(meta1.contains("started_at: 2026-04-29T10:00:00Z"));
        assert!(meta2.contains("started_at: 2026-04-29T10:00:42Z"));
        assert_ne!(
            extract_started_at(&meta1),
            extract_started_at(&meta2),
            "started_at must differ across steps -- otherwise the manifest collapses every step onto the run's start time"
        );
    }

    fn extract_started_at(meta: &str) -> &str {
        meta.lines()
            .find_map(|l| l.trim().strip_prefix("started_at:"))
            .map(str::trim)
            .unwrap_or("")
    }

    #[test]
    fn manifest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("run");
        let manifest = Manifest {
            version: 1,
            run_id: "dev-20260429-100000".to_string(),
            flow_name: "dev".to_string(),
            flow_path: ".kuro/flows/dev.yaml".to_string(),
            flow_sha256: sha256_hex(b"contents"),
            started_at: "2026-04-29T10:00:00Z".to_string(),
            finished_at: "2026-04-29T10:05:00Z".to_string(),
            duration_ms: 300_000,
            total_tokens_in: 1200,
            total_tokens_out: 800,
            cost: None,
            vars: indexmap::IndexMap::from([("owner".to_string(), "nestrai".to_string())]),
            seeds: vec![SeedRecord {
                display: ".kuro".to_string(),
                path: Some(".kuro".to_string()),
                git_sha: None,
                dirty: false,
            }],
            resources: vec![ResourceRecord {
                kind: "agent".to_string(),
                name: "Levi".to_string(),
                path: ".kuro/agents/Levi.yaml".to_string(),
                sha256: sha256_hex(b"agent"),
            }],
            roles: vec![RoleResolution {
                role: "developer".to_string(),
                agent: "Sage".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                backend: "claude-cli".to_string(),
                model_source: "agent".to_string(),
                backend_source: "agent".to_string(),
                seed_origin: Some(".kuro".to_string()),
            }],
            steps: vec![record(1, "design", "md")],
        };
        write_manifest(&run_path, &manifest).unwrap();

        let yaml = std::fs::read_to_string(run_path.join("manifest.yaml")).unwrap();
        let parsed: Manifest = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.run_id, "dev-20260429-100000");
        assert_eq!(parsed.flow_sha256.len(), 64);
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.roles[0].agent, "Sage");
    }

    // --- read_run ---

    fn write_minimal_manifest(run_path: &Path) {
        // Smallest manifest YAML that satisfies serde -- enough so that
        // `read_run` flips the status to Done. The on-disk shape is not
        // parsed by `read_run`; presence is the signal.
        std::fs::write(run_path.join("manifest.yaml"), "version: 1\n").unwrap();
    }

    #[test]
    fn read_run_not_found_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = read_run(dir.path(), "ghost-20260501-000000", None).unwrap();
        assert_eq!(out.status, RunStatus::NotFound);
        assert!(out.steps.is_empty());
        assert_eq!(out.run_id, "ghost-20260501-000000");
    }

    #[test]
    fn read_run_running_when_manifest_missing() {
        // A run dir with one step written but no manifest -- the typical
        // mid-flight shape. `read_run` reports `running` and returns the
        // partial step output so a caller can render progress.
        let dir = tempfile::tempdir().unwrap();
        let run_id = "dev-20260501-100000";
        let run_path = dir.path().join(run_id);
        write_run_step(&run_path, 1, &record(1, "design", "md"), "BODY-1").unwrap();

        let out = read_run(dir.path(), run_id, None).unwrap();
        assert_eq!(out.status, RunStatus::Running);
        assert_eq!(out.steps.len(), 1);
        assert_eq!(out.steps[0].step_id, "design");
        assert_eq!(out.steps[0].content.as_deref(), Some("BODY-1"));
    }

    #[test]
    fn read_run_done_when_manifest_present_returns_steps_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let run_id = "dev-20260501-100100";
        let run_path = dir.path().join(run_id);
        // Write steps out of order to prove the reader sorts by NN- prefix.
        write_run_step(&run_path, 2, &record(2, "build", "md"), "BUILD-OUT").unwrap();
        write_run_step(&run_path, 1, &record(1, "design", "md"), "DESIGN-OUT").unwrap();
        write_minimal_manifest(&run_path);

        let out = read_run(dir.path(), run_id, None).unwrap();
        assert_eq!(out.status, RunStatus::Done);
        let ids: Vec<&str> = out.steps.iter().map(|s| s.step_id.as_str()).collect();
        assert_eq!(ids, vec!["design", "build"]);
        assert_eq!(out.steps[0].content.as_deref(), Some("DESIGN-OUT"));
        assert_eq!(out.steps[1].content.as_deref(), Some("BUILD-OUT"));
    }

    #[test]
    fn read_run_step_filter_returns_only_match() {
        let dir = tempfile::tempdir().unwrap();
        let run_id = "dev-20260501-100200";
        let run_path = dir.path().join(run_id);
        write_run_step(&run_path, 1, &record(1, "design", "md"), "D").unwrap();
        write_run_step(&run_path, 2, &record(2, "build", "md"), "B").unwrap();
        write_minimal_manifest(&run_path);

        let out = read_run(dir.path(), run_id, Some("build")).unwrap();
        assert_eq!(out.status, RunStatus::Done);
        assert_eq!(out.steps.len(), 1);
        assert_eq!(out.steps[0].step_id, "build");
        assert_eq!(out.steps[0].content.as_deref(), Some("B"));
    }

    #[test]
    fn read_run_step_filter_unknown_step_is_empty_not_error() {
        // The team review wants `show_output` to distinguish run-level vs
        // step-level "missing": the run is Done, the step just is not in
        // the recorded set. Returning Ok with empty steps lets the MCP
        // tool surface that distinction without inventing a new error.
        let dir = tempfile::tempdir().unwrap();
        let run_id = "dev-20260501-100300";
        let run_path = dir.path().join(run_id);
        write_run_step(&run_path, 1, &record(1, "design", "md"), "D").unwrap();
        write_minimal_manifest(&run_path);

        let out = read_run(dir.path(), run_id, Some("missing-step")).unwrap();
        assert_eq!(out.status, RunStatus::Done);
        assert!(out.steps.is_empty());
    }

    #[test]
    fn read_run_step_filter_disambiguates_id_suffix() {
        // Two steps with shared substring must not collide. Mirrors the
        // existing `read_run_step_content` test.
        let dir = tempfile::tempdir().unwrap();
        let run_id = "dev-20260501-100400";
        let run_path = dir.path().join(run_id);
        write_run_step(&run_path, 1, &record(1, "pre-fetch", "md"), "ALPHA").unwrap();
        write_run_step(&run_path, 2, &record(2, "fetch", "md"), "BETA").unwrap();
        write_minimal_manifest(&run_path);

        let only_fetch = read_run(dir.path(), run_id, Some("fetch")).unwrap();
        assert_eq!(only_fetch.steps.len(), 1);
        assert_eq!(only_fetch.steps[0].step_id, "fetch");
        assert_eq!(only_fetch.steps[0].content.as_deref(), Some("BETA"));
    }

    #[test]
    fn read_run_running_step_with_no_content_yet() {
        // A meta file exists (the runner writes meta then content) but the
        // content file has not landed. `content` is None so the caller can
        // surface `running` without making up output.
        let dir = tempfile::tempdir().unwrap();
        let run_id = "dev-20260501-100500";
        let run_path = dir.path().join(run_id);
        ensure_dir(&run_path.join(STEPS_SUBDIR)).unwrap();
        let rec = record(1, "design", "md");
        // Write only the meta -- no content file.
        std::fs::write(
            run_path
                .join(STEPS_SUBDIR)
                .join(step_meta_filename(1, &rec.step_id)),
            serde_yaml::to_string(&rec).unwrap(),
        )
        .unwrap();

        let out = read_run(dir.path(), run_id, None).unwrap();
        assert_eq!(out.status, RunStatus::Running);
        assert_eq!(out.steps.len(), 1);
        assert!(out.steps[0].content.is_none());
    }

    #[test]
    fn run_status_wire_strings_are_stable() {
        // The MCP `show_output` JSON contract pins these spellings. Any
        // rename here is a wire break and must show up in this test before
        // shipping.
        assert_eq!(RunStatus::Running.as_str(), "running");
        assert_eq!(RunStatus::Done.as_str(), "done");
        assert_eq!(RunStatus::Failed.as_str(), "failed");
        assert_eq!(RunStatus::NotFound.as_str(), "not_found");
    }

    #[test]
    fn resolution_audit_written_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("run");
        write_resolution_audit(&run_path, "[resolve] seeds: .kuro\n").unwrap();
        let txt = std::fs::read_to_string(run_path.join("resolution-audit.txt")).unwrap();
        assert!(txt.starts_with("[resolve] seeds:"));
    }
}
