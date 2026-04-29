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
// layout below. The legacy reader stays so future `koto show` (#92) can
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
    /// Cost is intentionally `null` -- koto does not currently price runs.
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
            flow_path: ".koto/flows/dev.yaml".to_string(),
            flow_sha256: sha256_hex(b"contents"),
            started_at: "2026-04-29T10:00:00Z".to_string(),
            finished_at: "2026-04-29T10:05:00Z".to_string(),
            duration_ms: 300_000,
            total_tokens_in: 1200,
            total_tokens_out: 800,
            cost: None,
            vars: indexmap::IndexMap::from([("owner".to_string(), "nestrai".to_string())]),
            seeds: vec![SeedRecord {
                display: ".koto".to_string(),
                path: Some(".koto".to_string()),
                git_sha: None,
                dirty: false,
            }],
            resources: vec![ResourceRecord {
                kind: "agent".to_string(),
                name: "Levi".to_string(),
                path: ".koto/agents/Levi.yaml".to_string(),
                sha256: sha256_hex(b"agent"),
            }],
            roles: vec![RoleResolution {
                role: "developer".to_string(),
                agent: "Sage".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                backend: "claude-cli".to_string(),
                model_source: "agent".to_string(),
                backend_source: "agent".to_string(),
                seed_origin: Some(".koto".to_string()),
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

    #[test]
    fn resolution_audit_written_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let run_path = dir.path().join("run");
        write_resolution_audit(&run_path, "[resolve] seeds: .koto\n").unwrap();
        let txt = std::fs::read_to_string(run_path.join("resolution-audit.txt")).unwrap();
        assert!(txt.starts_with("[resolve] seeds:"));
    }
}
