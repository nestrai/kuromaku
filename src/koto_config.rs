//! Project-level config (`.kuro/config.yaml`).
//!
//! Optional file under `.kuro/` that defines capability tiers, project
//! defaults and template variables. When the file is absent everything works
//! as before -- callers receive `None` from [`KotoConfig::load_optional`].
//!
//! Resolution cascade and roles are handled in #129; seeds in #130.
//!
//! For one migration cycle [`KotoConfig::load_optional`] also accepts two
//! legacy locations and emits a deprecation warning when it falls back:
//!   1. `.koto/config.yaml` -- former canonical path before the `.kuro/` rename
//!   2. `koto.yaml` in the repo root -- pre-`.koto/` layout from earlier still

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Backend;
use crate::core::Version;

/// Canonical project directory inside the repository. Houses agents, flows,
/// rules, skills, and the project config. Exported so non-CLI callers (the
/// runner library API, tests) can locate the directory without hard-coding
/// the string in multiple places.
pub const KOTO_DIR: &str = ".kuro";

/// Canonical relative path to the project config file. Single source of
/// truth -- runtime messages and lookups must use this constant rather than
/// hard-coding the string.
pub const KOTO_CONFIG_FILE: &str = ".kuro/config.yaml";

/// Legacy path: former canonical config location before the `.koto/` -> `.kuro/`
/// rename. Loading from this path triggers a deprecation warning suggesting
/// `mv .koto .kuro`. Removed once the migration window closes.
pub const KOTO_CONFIG_FILE_LEGACY_KOTO: &str = ".koto/config.yaml";

/// Legacy path: pre-`.koto/` layout where the config sat in the repo root.
/// Still loaded for one extra migration cycle and triggers its own deprecation
/// warning pointing at [`KOTO_CONFIG_FILE`].
pub const KOTO_CONFIG_FILE_LEGACY_ROOT: &str = "koto.yaml";

/// Errors surfaced when parsing or loading [`KotoConfig`].
///
/// `Display` is implemented manually so error messages reference
/// [`KOTO_CONFIG_FILE`] rather than a hard-coded literal -- that way renaming
/// the config file is a one-line change.
#[derive(Debug)]
pub enum KotoConfigError {
    Io(std::io::Error),
    Parse(serde_yaml::Error),
    Validation(String),
}

impl std::fmt::Display for KotoConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read {KOTO_CONFIG_FILE}: {e}"),
            Self::Parse(e) => write!(f, "failed to parse {KOTO_CONFIG_FILE}: {e}"),
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
        }
    }
}

impl std::error::Error for KotoConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::Validation(_) => None,
        }
    }
}

impl From<std::io::Error> for KotoConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_yaml::Error> for KotoConfigError {
    fn from(e: serde_yaml::Error) -> Self {
        Self::Parse(e)
    }
}

impl KotoConfigError {
    /// The error payload without any thiserror prefix.
    ///
    /// Use this when forwarding the error into another error type that adds
    /// its own prefix (for example [`crate::config::ConfigError::Validation`]).
    /// Avoids the duplicated "validation error: validation error: ..." you'd
    /// get from `to_string()` plus a wrapping `Validation` variant.
    pub fn message(&self) -> String {
        match self {
            Self::Io(e) => e.to_string(),
            Self::Parse(e) => e.to_string(),
            Self::Validation(msg) => msg.clone(),
        }
    }
}

/// Project-level backend choice. Distinct from the runtime [`crate::config::Backend`]
/// enum: this is the operator's policy (spawn a subprocess vs call the HTTP API),
/// while `Backend` names a concrete provider/transport pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KotoBackend {
    Cli,
    Api,
}

impl KotoBackend {
    /// Stable label for audit/log output.
    pub fn as_str(&self) -> &'static str {
        match self {
            KotoBackend::Cli => "cli",
            KotoBackend::Api => "api",
        }
    }

    /// Parse from a CLI-supplied string (`cli` or `api`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cli" => Some(KotoBackend::Cli),
            "api" => Some(KotoBackend::Api),
            _ => None,
        }
    }
}

// --- Raw deserialization ---

#[derive(Debug, Deserialize)]
struct RawKotoConfig {
    version: Version,
    #[serde(default)]
    tiers: HashMap<String, String>,
    #[serde(default)]
    defaults: Option<RawKotoDefaults>,
    #[serde(default)]
    vars: HashMap<String, String>,
    #[serde(default)]
    roles: HashMap<String, RawKotoRole>,
    #[serde(default)]
    seeds: Option<Vec<RawSeed>>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

/// One seed entry as it appears in the project config. Either `path:`
/// (local) or `repo:` (remote). Validated at parse time -- exactly one must
/// be set.
#[derive(Debug, Deserialize)]
struct RawSeed {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawKotoRole {
    agent: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    backend: Option<KotoBackend>,
    /// Per-binding overlay of agent fields (issue #364). When set, the
    /// project layers these values on top of the seed agent before any
    /// agent run touches the role -- without forcing a fork of the agent
    /// YAML. Validated and merged in [`KotoConfig::from_yaml_str`] /
    /// runner setup.
    #[serde(default)]
    overlays: Option<RawKotoOverlay>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

/// Raw form of a role overlay block as it appears in
/// `.kuro/config.yaml`. The four supported fields mirror the named
/// overlay surface in issue #364 -- no general "merge any field"
/// behaviour. Unknown keys produce a warning but do not abort parsing,
/// matching the rest of the YAML schema.
#[derive(Debug, Deserialize, Default)]
struct RawKotoOverlay {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    backend: Option<KotoBackend>,
    #[serde(default)]
    extra_args: HashMap<String, Vec<String>>,
    #[serde(default)]
    rules: Vec<String>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawKotoDefaults {
    #[serde(default)]
    backend: Option<KotoBackend>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

// --- Resolved config ---

/// Project-level role binding. Maps a role name to a default agent plus
/// optional model and backend overrides that apply project-wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KotoRole {
    pub agent: String,
    pub model: Option<String>,
    pub backend: Option<KotoBackend>,
    /// Optional per-binding overlay (issue #364). Defaults to the empty
    /// overlay -- byte-identical to "no overlays declared" for callers
    /// that do not opt in.
    pub overlays: RoleOverlay,
}

/// Resolved per-binding overlay. Empty (`is_empty()`) when the role
/// declares no `overlays:` block, or declares an empty one (`overlays:
/// {}`). The runner applies this to the seed agent right after agents
/// load and before role resolution so the rest of the resolver sees the
/// overlay-mutated values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleOverlay {
    pub model: Option<String>,
    pub backend: Option<KotoBackend>,
    /// Backend-keyed extra CLI argument lists. Mirrors `Agent::extra_args`
    /// shape so a runner merge is a HashMap insert per backend key.
    pub extra_args: HashMap<Backend, Vec<String>>,
    /// Rules to append to the seed agent's rule list. Duplicates are
    /// dedup-by-name at apply time -- seed rules keep their position.
    pub rules: Vec<String>,
}

impl RoleOverlay {
    /// True when the overlay contributes nothing -- no fields set and no
    /// rules to append. Used by the runner to skip the merge work and by
    /// the audit to suppress the "overlays:" line.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.backend.is_none()
            && self.extra_args.is_empty()
            && self.rules.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KotoConfig {
    pub version: String,
    pub tiers: HashMap<String, String>,
    pub default_backend: Option<KotoBackend>,
    pub vars: HashMap<String, String>,
    pub roles: HashMap<String, KotoRole>,
    pub seeds: Seeds,
}

/// One resolved seed source. Carries both the original display string (for
/// audit output) and -- for local seeds -- the tilde-expanded filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub source: SeedSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedSource {
    /// Local directory. `display` is the user-supplied string (with `~/` if
    /// present); `path` is the expanded absolute-or-relative path used for
    /// filesystem lookups.
    Local { display: String, path: PathBuf },
    /// Remote git repo. Parsed but not fetched in v1 -- using one in a lookup
    /// surfaces a "not yet implemented" error.
    Remote {
        repo: String,
        /// Optional ref (tag, branch or SHA) -- captured for future use.
        ref_: Option<String>,
    },
}

impl Seed {
    /// Display string used in audit and error messages. Local seeds keep the
    /// user's original `~/...` path so the output is recognizable; remote seeds
    /// render as `repo[@ref]`.
    pub fn display(&self) -> String {
        match &self.source {
            SeedSource::Local { display, .. } => display.clone(),
            SeedSource::Remote { repo, ref_ } => match ref_ {
                Some(r) => format!("{repo}@{r}"),
                None => repo.clone(),
            },
        }
    }

    /// Stable kind label for audit output (`local` or `remote`).
    pub fn kind_label(&self) -> &'static str {
        match self.source {
            SeedSource::Local { .. } => "local",
            SeedSource::Remote { .. } => "remote",
        }
    }

    /// Local path if this is a local seed, otherwise `None`.
    pub fn local_path(&self) -> Option<&Path> {
        match &self.source {
            SeedSource::Local { path, .. } => Some(path),
            SeedSource::Remote { .. } => None,
        }
    }
}

/// Ordered list of seed sources. Lookups walk top-to-bottom; first match wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seeds {
    pub seeds: Vec<Seed>,
}

impl Seeds {
    /// Implicit default when no `seeds:` section is present in the project
    /// config (or the config file itself is absent). Single `.kuro/` seed,
    /// no eager existence check -- matches the pre-rename `.koto/` behavior
    /// against the new directory name.
    ///
    /// **Path contract**: the returned seed path is **relative** (`.kuro`).
    /// Callers that pair this with the process CWD (runner, main, chat,
    /// resolver) get the expected behavior because the rest of the project
    /// config also resolves against the process CWD. Callers that have an
    /// explicit `cwd: &Path` argument and pass it down to filesystem walks
    /// (e.g. the MCP discovery tools in `src/mcp/discovery.rs`) **must**
    /// rebase relative seed paths against that `cwd` -- otherwise the walks
    /// fall through to the process CWD and silently leak (#293). The
    /// canonical resolution helper for the discovery path is
    /// `discover_seeds`/`rebase_seeds_for_cwd` in `src/mcp/discovery.rs`.
    pub fn default_local() -> Self {
        Self {
            seeds: vec![Seed {
                source: SeedSource::Local {
                    display: ".kuro/".to_string(),
                    path: PathBuf::from(".kuro"),
                },
            }],
        }
    }

    /// Legacy default seed used when the project config was loaded from a
    /// pre-rename location (`.koto/config.yaml` or root `koto.yaml`) AND has
    /// no explicit `seeds:` section. Without this, projects on the legacy
    /// layout would silently start looking for agents/rules under `.kuro/`
    /// after upgrading. Removed once the migration window closes.
    pub fn legacy_koto_local() -> Self {
        Self {
            seeds: vec![Seed {
                source: SeedSource::Local {
                    display: ".koto/".to_string(),
                    path: PathBuf::from(".koto"),
                },
            }],
        }
    }

    /// Build from raw config entries. Validates each entry has exactly one
    /// of `path:` / `repo:`, and that local paths exist on disk.
    fn from_raw_entries(raw: &[RawSeed]) -> Result<Self, KotoConfigError> {
        if raw.is_empty() {
            return Err(KotoConfigError::Validation(
                "seeds list must contain at least one entry".to_string(),
            ));
        }
        let mut seeds = Vec::with_capacity(raw.len());
        for (idx, entry) in raw.iter().enumerate() {
            for key in entry.unknown.keys() {
                eprintln!("warning: unknown field '{key}' in {KOTO_CONFIG_FILE} seed[{idx}]");
            }
            let seed = match (entry.path.as_deref(), entry.repo.as_deref()) {
                (Some(_), Some(_)) => {
                    return Err(KotoConfigError::Validation(format!(
                        "seed[{idx}] has both 'path' and 'repo' -- use exactly one"
                    )));
                }
                (None, None) => {
                    return Err(KotoConfigError::Validation(format!(
                        "seed[{idx}] must specify either 'path' or 'repo'"
                    )));
                }
                (Some(path_str), None) => {
                    if path_str.trim().is_empty() {
                        return Err(KotoConfigError::Validation(format!(
                            "seed[{idx}] path must not be empty"
                        )));
                    }
                    let expanded = expand_tilde(path_str);
                    if !expanded.exists() {
                        return Err(KotoConfigError::Validation(format!(
                            "seed path \"{path_str}\" does not exist"
                        )));
                    }
                    Seed {
                        source: SeedSource::Local {
                            display: path_str.to_string(),
                            path: expanded,
                        },
                    }
                }
                (None, Some(repo)) => {
                    if repo.trim().is_empty() {
                        return Err(KotoConfigError::Validation(format!(
                            "seed[{idx}] repo must not be empty"
                        )));
                    }
                    Seed {
                        source: SeedSource::Remote {
                            repo: repo.to_string(),
                            ref_: entry.ref_.clone(),
                        },
                    }
                }
            };
            seeds.push(seed);
        }
        Ok(Self { seeds })
    }

    /// Walk seeds top-to-bottom looking for `rel`. Returns the first seed that
    /// contains the file along with the resolved full path.
    ///
    /// If the walk falls through a remote seed -- meaning no earlier local
    /// seed had the file and we cannot inspect a remote one in v1 -- this
    /// returns an explicit "remote seeds not yet implemented" error pointing
    /// at the offending seed. Pure-local lookups never trigger this.
    pub fn find(&self, rel: &Path) -> Result<Option<(usize, PathBuf)>, KotoConfigError> {
        for (i, seed) in self.seeds.iter().enumerate() {
            match &seed.source {
                SeedSource::Local { path, .. } => {
                    let candidate = path.join(rel);
                    if candidate.exists() {
                        return Ok(Some((i, candidate)));
                    }
                }
                SeedSource::Remote { repo, ref_ } => {
                    let suffix = ref_.as_deref().map(|r| format!("@{r}")).unwrap_or_default();
                    return Err(KotoConfigError::Validation(format!(
                        "remote seeds not yet implemented (repo: {repo}{suffix})"
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Format a "not found" error listing every seed path that was searched.
    /// Remote seeds are still listed -- they explain why the lookup gave up.
    pub fn not_found_message(&self, kind: &str, name: &str) -> String {
        let displays: Vec<String> = self.seeds.iter().map(|s| s.display()).collect();
        format!(
            "{kind} \"{name}\" not found in seeds: {}",
            displays.join(", ")
        )
    }

    /// Display string for the audit "[resolve] seeds: ..." line.
    pub fn audit_line(&self) -> String {
        self.seeds
            .iter()
            .map(|s| format!("{} ({})", s.display(), s.kind_label()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Expand a leading `~/` or bare `~` to `$HOME`. Returns the input as a
/// `PathBuf` unchanged when no tilde is present or `$HOME` cannot be located.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(path)
}

impl KotoConfig {
    /// Load the project config from `dir` if it exists. Returns `Ok(None)`
    /// when the file is missing -- callers must treat that as the no-op case
    /// to keep the no-config workflow intact.
    ///
    /// Resolution order (first match wins):
    ///   1. `.kuro/config.yaml` -- canonical, silent
    ///   2. `.koto/config.yaml` -- legacy from the pre-`.kuro/` rename, warns
    ///      and suggests `mv .koto .kuro`
    ///   3. `koto.yaml` in the repo root -- earlier legacy, warns and points
    ///      at the canonical path
    ///
    /// For both legacy paths, when the loaded config has no explicit `seeds:`
    /// section the implicit seed is rewritten to [`Seeds::legacy_koto_local`]
    /// so projects on the old layout keep finding their agents/rules under
    /// `.koto/` after upgrading.
    pub fn load_optional(dir: &Path) -> Result<Option<Self>, KotoConfigError> {
        let path = dir.join(KOTO_CONFIG_FILE);
        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            return Ok(Some(Self::from_yaml_str(&contents)?));
        }
        // Legacy 1: `.koto/config.yaml` -- former canonical path before the
        // `.kuro/` rename. Falls back transparently for one migration cycle.
        let legacy_koto = dir.join(KOTO_CONFIG_FILE_LEGACY_KOTO);
        if legacy_koto.exists() {
            eprintln!("warning: {KOTO_CONFIG_FILE_LEGACY_KOTO} is deprecated, run: mv .koto .kuro");
            let contents = std::fs::read_to_string(&legacy_koto)?;
            let cfg = Self::from_yaml_str(&contents)?;
            return Ok(Some(rebase_default_seeds_for_legacy(cfg)));
        }
        // Legacy 2: pre-`.koto/` layout -- config in the repo root. Kept
        // separately because the deprecation message and the seeds default it
        // implies are different from the `.koto/` case.
        let legacy_root = dir.join(KOTO_CONFIG_FILE_LEGACY_ROOT);
        if legacy_root.exists() {
            eprintln!(
                "warning: {KOTO_CONFIG_FILE_LEGACY_ROOT} is deprecated, move config to {KOTO_CONFIG_FILE}"
            );
            let contents = std::fs::read_to_string(&legacy_root)?;
            let cfg = Self::from_yaml_str(&contents)?;
            return Ok(Some(rebase_default_seeds_for_legacy(cfg)));
        }
        Ok(None)
    }

    pub fn from_yaml_str(contents: &str) -> Result<Self, KotoConfigError> {
        let raw: RawKotoConfig = serde_yaml::from_str(contents)?;

        for key in raw.unknown.keys() {
            eprintln!("warning: unknown field '{key}' in {KOTO_CONFIG_FILE}");
        }
        if let Some(ref defaults) = raw.defaults {
            for key in defaults.unknown.keys() {
                eprintln!("warning: unknown field '{key}' in {KOTO_CONFIG_FILE} defaults");
            }
        }

        if raw.version.0 != "1" {
            return Err(KotoConfigError::Validation(format!(
                "unsupported version '{}', expected '1'",
                raw.version.0
            )));
        }

        // Iterate in sorted order so error messages for invalid tiers are
        // deterministic across runs -- HashMap iteration order is not stable.
        let mut tier_names: Vec<&String> = raw.tiers.keys().collect();
        tier_names.sort();
        for tier_name in tier_names {
            if tier_name.trim().is_empty() {
                return Err(KotoConfigError::Validation(
                    "tier name must not be empty".to_string(),
                ));
            }
            validate_model_format(&raw.tiers[tier_name])?;
        }

        // Validate roles in sorted order for deterministic error messages.
        let mut role_names: Vec<&String> = raw.roles.keys().collect();
        role_names.sort();
        let mut roles: HashMap<String, KotoRole> = HashMap::new();
        for role_name in role_names {
            if role_name.trim().is_empty() {
                return Err(KotoConfigError::Validation(
                    "role name must not be empty".to_string(),
                ));
            }
            let raw_role = &raw.roles[role_name];
            for key in raw_role.unknown.keys() {
                eprintln!(
                    "warning: unknown field '{key}' in {KOTO_CONFIG_FILE} role '{role_name}'"
                );
            }
            if raw_role.agent.trim().is_empty() {
                return Err(KotoConfigError::Validation(format!(
                    "role '{role_name}' has empty agent"
                )));
            }
            if let Some(ref m) = raw_role.model {
                validate_model_format(m)?;
            }
            let overlays = match raw_role.overlays.as_ref() {
                Some(raw_overlay) => resolve_overlay(role_name, raw_overlay)?,
                None => RoleOverlay::default(),
            };
            roles.insert(
                role_name.clone(),
                KotoRole {
                    agent: raw_role.agent.clone(),
                    model: raw_role.model.clone(),
                    backend: raw_role.backend,
                    overlays,
                },
            );
        }

        // Seeds: when the section is absent we use the `.koto/` default to
        // preserve v0 behavior. Explicit empty list (`seeds: []`) is rejected
        // because it would point lookups nowhere -- the user almost certainly
        // meant to write actual entries.
        let seeds = match raw.seeds {
            Some(entries) => Seeds::from_raw_entries(&entries)?,
            None => Seeds::default_local(),
        };

        Ok(Self {
            version: raw.version.0,
            tiers: raw.tiers,
            default_backend: raw.defaults.and_then(|d| d.backend),
            vars: raw.vars,
            roles,
            seeds,
        })
    }

    /// Resolve a tier name to its `<provider>/<model-id>` string.
    ///
    /// The error variant lists available tiers sorted alphabetically so the
    /// message is stable across runs.
    pub fn resolve_tier(&self, tier_name: &str) -> Result<&str, KotoConfigError> {
        self.tiers
            .get(tier_name)
            .map(String::as_str)
            .ok_or_else(|| {
                let mut available: Vec<&str> = self.tiers.keys().map(String::as_str).collect();
                available.sort();
                KotoConfigError::Validation(format!(
                    "tier \"{tier_name}\" not defined in {KOTO_CONFIG_FILE} (available: {})",
                    available.join(", ")
                ))
            })
    }
}

/// When a config was loaded from a legacy path (`.koto/config.yaml` or
/// `koto.yaml`), the implicit-seed default produced by [`Seeds::default_local`]
/// points at `.kuro/` -- which is wrong for projects still on the old layout.
/// Detect that exact case (a single seed equal to the new default) and swap
/// it to [`Seeds::legacy_koto_local`] so agents/rules/flows continue to
/// resolve out of `.koto/`.
///
/// Configs that explicitly declared `seeds:` are left untouched -- the user
/// asked for a specific layout and we honor it verbatim.
fn rebase_default_seeds_for_legacy(mut cfg: KotoConfig) -> KotoConfig {
    if cfg.seeds == Seeds::default_local() {
        cfg.seeds = Seeds::legacy_koto_local();
    }
    cfg
}

/// Convert a raw overlay block into a validated [`RoleOverlay`].
///
/// Mirrors the validation rules from `crate::config::validate_extra_args`
/// for the `extra_args` field so the overlay surface stays consistent with
/// the agent-file surface: unknown backend keys are rejected with the list
/// of valid names, and the `api` backend is rejected explicitly because it
/// has no argv to extend. Rule entries must be non-empty strings. Unknown
/// fields warn but parsing continues -- matching the rest of the YAML
/// schema.
fn resolve_overlay(role_name: &str, raw: &RawKotoOverlay) -> Result<RoleOverlay, KotoConfigError> {
    for key in raw.unknown.keys() {
        eprintln!(
            "warning: unknown field '{key}' in {KOTO_CONFIG_FILE} role '{role_name}' overlays"
        );
    }
    if let Some(ref m) = raw.model {
        validate_model_format(m)?;
    }
    // Validate rules: keep order, reject empty/whitespace entries early so
    // the user finds the typo at parse time rather than during a flow run.
    let mut rules = Vec::with_capacity(raw.rules.len());
    for r in &raw.rules {
        if r.trim().is_empty() {
            return Err(KotoConfigError::Validation(format!(
                "role '{role_name}' overlays.rules contains an empty entry"
            )));
        }
        rules.push(r.clone());
    }
    // extra_args keys: parse-time validation mirrors the agent-file path so
    // the overlay surface fails identically on typos and on `api` keys.
    let mut extra_args: HashMap<Backend, Vec<String>> =
        HashMap::with_capacity(raw.extra_args.len());
    let mut keys: Vec<&String> = raw.extra_args.keys().collect();
    keys.sort();
    for key in keys {
        let backend = Backend::from_yaml_name(key).ok_or_else(|| {
            KotoConfigError::Validation(format!(
                "unknown backend in extra_args: '{key}' (valid: claude-cli, codex, ollama) in role '{role_name}' overlays"
            ))
        })?;
        if backend == Backend::Api {
            return Err(KotoConfigError::Validation(format!(
                "extra_args is not supported for backend 'api' in role '{role_name}' overlays -- the api backend talks to the HTTP API, not a CLI, so there is no argv to extend"
            )));
        }
        extra_args.insert(backend, raw.extra_args[key].clone());
    }
    Ok(RoleOverlay {
        model: raw.model.clone(),
        backend: raw.backend,
        extra_args,
        rules,
    })
}

fn validate_model_format(model: &str) -> Result<(), KotoConfigError> {
    let mut parts = model.split('/');
    let provider = parts.next().unwrap_or("");
    let model_id = parts.next().unwrap_or("");
    let extra = parts.next();
    if provider.is_empty() || model_id.is_empty() || extra.is_some() {
        return Err(KotoConfigError::Validation(format!(
            "model must be <provider>/<model-id>, got \"{model}\""
        )));
    }
    Ok(())
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_KOTO_YAML: &str = r#"
version: "1"

tiers:
  reasoning: claude/opus-4-7
  general: claude/sonnet-4-6
  quick: claude/haiku-4-5

defaults:
  backend: cli

vars:
  owner: nestrai
  repo: koto
"#;

    #[test]
    fn full_config_parses() {
        let cfg = KotoConfig::from_yaml_str(FULL_KOTO_YAML).unwrap();
        assert_eq!(cfg.version, "1");
        assert_eq!(cfg.tiers.len(), 3);
        assert_eq!(cfg.tiers.get("reasoning").unwrap(), "claude/opus-4-7");
        assert_eq!(cfg.default_backend, Some(KotoBackend::Cli));
        assert_eq!(cfg.vars.get("owner").unwrap(), "nestrai");
        assert_eq!(cfg.vars.get("repo").unwrap(), "koto");
    }

    #[test]
    fn version_only_parses() {
        let cfg = KotoConfig::from_yaml_str(r#"version: "1""#).unwrap();
        assert_eq!(cfg.version, "1");
        assert!(cfg.tiers.is_empty());
        assert!(cfg.default_backend.is_none());
        assert!(cfg.vars.is_empty());
    }

    #[test]
    fn version_as_integer_parses() {
        let cfg = KotoConfig::from_yaml_str("version: 1").unwrap();
        assert_eq!(cfg.version, "1");
    }

    #[test]
    fn unsupported_version_errors() {
        let err = KotoConfig::from_yaml_str(r#"version: "2""#).unwrap_err();
        assert!(err.to_string().contains("unsupported version"));
    }

    #[test]
    fn missing_version_errors() {
        let err = KotoConfig::from_yaml_str("tiers:\n  general: claude/sonnet-4-6").unwrap_err();
        assert!(matches!(err, KotoConfigError::Parse(_)));
    }

    #[test]
    fn malformed_yaml_propagates_parse_error() {
        // Mismatched indentation / broken YAML
        let err = KotoConfig::from_yaml_str("version: \"1\"\ntiers:\n  bad: : :").unwrap_err();
        let msg = err.to_string();
        // serde_yaml errors include a position reference; just verify it's a parse error
        assert!(matches!(err, KotoConfigError::Parse(_)), "got: {msg}");
    }

    #[test]
    fn invalid_model_format_no_slash_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: opus-4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("model must be <provider>/<model-id>, got \"opus-4-7\""),
            "got: {err}"
        );
    }

    #[test]
    fn invalid_model_format_two_slashes_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: claude/opus/4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn invalid_model_format_empty_provider_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: /opus-4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn invalid_model_format_empty_id_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: claude/
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn resolve_tier_returns_model() {
        let cfg = KotoConfig::from_yaml_str(FULL_KOTO_YAML).unwrap();
        assert_eq!(cfg.resolve_tier("reasoning").unwrap(), "claude/opus-4-7");
    }

    #[test]
    fn resolve_tier_missing_errors_with_available() {
        let cfg = KotoConfig::from_yaml_str(FULL_KOTO_YAML).unwrap();
        let err = cfg.resolve_tier("phantom").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tier \"phantom\" not defined"), "got: {msg}");
        assert!(msg.contains("available:"), "got: {msg}");
        // The available list is alphabetically sorted
        assert!(msg.contains("general"), "got: {msg}");
        assert!(msg.contains("quick"), "got: {msg}");
        assert!(msg.contains("reasoning"), "got: {msg}");
    }

    #[test]
    fn defaults_backend_api_parses() {
        let yaml = r#"
version: "1"
defaults:
  backend: api
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.default_backend, Some(KotoBackend::Api));
    }

    #[test]
    fn load_optional_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = KotoConfig::load_optional(dir.path()).unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn load_optional_present_file_returns_some() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KOTO_CONFIG_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, FULL_KOTO_YAML).unwrap();
        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.tiers.len(), 3);
    }

    #[test]
    fn load_optional_legacy_root_yaml_loads_with_warning() {
        // Backward-compat: until users migrate, the pre-`.koto/` root
        // `koto.yaml` filename still loads. The warning itself is on stderr
        // and not asserted on here, but the file must parse and yield a
        // populated config.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(KOTO_CONFIG_FILE_LEGACY_ROOT),
            FULL_KOTO_YAML,
        )
        .unwrap();
        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.tiers.len(), 3);
        // No explicit seeds in FULL_KOTO_YAML -- legacy path must rebase the
        // implicit seed to `.koto/` so projects on the old layout keep
        // resolving agents/rules where they actually live.
        assert_eq!(cfg.seeds, Seeds::legacy_koto_local());
    }

    #[test]
    fn load_optional_legacy_koto_config_loads_with_warning() {
        // The `.koto/config.yaml` legacy path -- the immediate predecessor of
        // `.kuro/config.yaml`. Same rebase rule applies as for the root
        // `koto.yaml` legacy path.
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join(KOTO_CONFIG_FILE_LEGACY_KOTO);
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, FULL_KOTO_YAML).unwrap();
        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.tiers.len(), 3);
        assert_eq!(cfg.seeds, Seeds::legacy_koto_local());
    }

    #[test]
    fn load_optional_prefers_canonical_over_legacy_koto() {
        // When `.kuro/config.yaml` and `.koto/config.yaml` both exist, the
        // canonical path wins so the migration is unambiguous. We arrange a
        // deliberately-broken legacy file: if the loader picked it up we'd
        // get a parse error instead of a populated config.
        let dir = tempfile::tempdir().unwrap();

        let canonical = dir.path().join(KOTO_CONFIG_FILE);
        std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        std::fs::write(&canonical, FULL_KOTO_YAML).unwrap();

        let legacy = dir.path().join(KOTO_CONFIG_FILE_LEGACY_KOTO);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "version: \"1\"\n: : :\n").unwrap();

        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.tiers.len(), 3);
        // Canonical config has no explicit seeds, so the implicit seed must
        // be the new default `.kuro/` -- not rebased to `.koto/`.
        assert_eq!(cfg.seeds, Seeds::default_local());
    }

    #[test]
    fn load_optional_prefers_legacy_koto_over_root_yaml() {
        // The `.koto/config.yaml` legacy path takes priority over the older
        // root `koto.yaml`. Same broken-file trick: if the loader picked the
        // root file we'd see a parse error.
        let dir = tempfile::tempdir().unwrap();

        let legacy = dir.path().join(KOTO_CONFIG_FILE_LEGACY_KOTO);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, FULL_KOTO_YAML).unwrap();

        std::fs::write(
            dir.path().join(KOTO_CONFIG_FILE_LEGACY_ROOT),
            "version: \"1\"\n: : :\n",
        )
        .unwrap();

        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.tiers.len(), 3);
    }

    #[test]
    fn load_optional_legacy_preserves_explicit_seeds() {
        // Legacy paths only rebase the implicit-seed default. When the legacy
        // config explicitly declares `seeds:`, the user's choice wins -- we
        // do not silently rewrite it to `.koto/`.
        let dir = tempfile::tempdir().unwrap();
        let custom_seed = tempfile::tempdir().unwrap();
        let yaml = format!(
            "version: \"1\"\nseeds:\n  - path: {}\n",
            custom_seed.path().display()
        );

        let legacy = dir.path().join(KOTO_CONFIG_FILE_LEGACY_KOTO);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, yaml).unwrap();

        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.seeds.seeds.len(), 1);
        assert_ne!(cfg.seeds, Seeds::legacy_koto_local());
        assert_ne!(cfg.seeds, Seeds::default_local());
    }

    #[test]
    fn unknown_top_level_field_does_not_fail() {
        // Unknown fields warn but parsing succeeds, matching flow YAML behavior.
        let yaml = r#"
version: "1"
something_new: hello
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.version, "1");
    }

    #[test]
    fn invalid_model_format_error_is_deterministic() {
        // Regression: HashMap iteration order is non-deterministic, so
        // multiple invalid tiers used to produce different error messages
        // across runs. Validation iterates sorted by tier name -- the first
        // invalid tier alphabetically should always be the one reported.
        let yaml = r#"
version: "1"
tiers:
  zeta: missing-slash
  alpha: also-missing
  middle: fine/model
"#;
        // Run several times: alphabetic order means "alpha" is checked first,
        // so the message must always quote "also-missing".
        for _ in 0..20 {
            let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
            assert!(
                err.to_string().contains("\"also-missing\""),
                "expected first-by-name tier in error, got: {err}"
            );
        }
    }

    #[test]
    fn empty_tier_name_errors() {
        // YAML allows an empty key only via explicit quoting.
        let yaml = "version: \"1\"\ntiers:\n  \"\": claude/opus-4-7\n";
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("tier name"), "got: {err}");
    }

    // --- roles (issue #129) ---

    const FULL_ROLES_YAML: &str = r#"
version: "1"

tiers:
  reasoning: claude/opus-4-7
  general: claude/sonnet-4-6

roles:
  developer:
    agent: coding/rust/Sage
  reviewer:
    agent: review/Bella
    model: ollama/llama3-70b
    backend: api
"#;

    #[test]
    fn roles_parse() {
        let cfg = KotoConfig::from_yaml_str(FULL_ROLES_YAML).unwrap();
        assert_eq!(cfg.roles.len(), 2);

        let dev = cfg.roles.get("developer").unwrap();
        assert_eq!(dev.agent, "coding/rust/Sage");
        assert!(dev.model.is_none());
        assert!(dev.backend.is_none());

        let rev = cfg.roles.get("reviewer").unwrap();
        assert_eq!(rev.agent, "review/Bella");
        assert_eq!(rev.model.as_deref(), Some("ollama/llama3-70b"));
        assert_eq!(rev.backend, Some(KotoBackend::Api));
    }

    #[test]
    fn role_with_empty_agent_errors() {
        let yaml = r#"
version: "1"
roles:
  developer:
    agent: ""
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("empty agent"), "got: {err}");
    }

    #[test]
    fn role_missing_agent_errors() {
        // serde will reject the missing required field.
        let yaml = r#"
version: "1"
roles:
  developer:
    model: claude/opus-4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(matches!(err, KotoConfigError::Parse(_)));
    }

    #[test]
    fn role_invalid_model_format_errors() {
        let yaml = r#"
version: "1"
roles:
  developer:
    agent: Sage
    model: missing-slash
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn role_unknown_field_warns_but_succeeds() {
        let yaml = r#"
version: "1"
roles:
  developer:
    agent: Sage
    something_new: hello
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.roles.get("developer").unwrap().agent, "Sage");
    }

    #[test]
    fn role_validation_is_deterministic() {
        // Two roles with bad model formats. Validation iterates sorted, so the
        // alphabetically-first invalid role's model must be reported every run.
        let yaml = r#"
version: "1"
roles:
  zeta:
    agent: Z
    model: bad-zeta
  alpha:
    agent: A
    model: bad-alpha
"#;
        for _ in 0..20 {
            let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
            assert!(
                err.to_string().contains("\"bad-alpha\""),
                "expected first-by-name role in error, got: {err}"
            );
        }
    }

    #[test]
    fn no_roles_section_yields_empty_map() {
        let cfg = KotoConfig::from_yaml_str(r#"version: "1""#).unwrap();
        assert!(cfg.roles.is_empty());
    }

    #[test]
    fn koto_backend_round_trip() {
        assert_eq!(KotoBackend::Cli.as_str(), "cli");
        assert_eq!(KotoBackend::Api.as_str(), "api");
        assert_eq!(KotoBackend::parse("cli"), Some(KotoBackend::Cli));
        assert_eq!(KotoBackend::parse("api"), Some(KotoBackend::Api));
        assert_eq!(KotoBackend::parse("nope"), None);
    }

    // --- seeds (issue #130) ---

    #[test]
    fn no_seeds_section_yields_default_local() {
        // A canonical project config without `seeds:` defaults to a single
        // implicit `.kuro/` seed. Configs loaded from legacy paths get
        // rebased separately via [`rebase_default_seeds_for_legacy`] -- this
        // test pins the default for the canonical-path case only.
        let cfg = KotoConfig::from_yaml_str(r#"version: "1""#).unwrap();
        assert_eq!(cfg.seeds.seeds.len(), 1);
        assert_eq!(cfg.seeds.seeds[0].display(), ".kuro/");
        assert_eq!(cfg.seeds.seeds[0].kind_label(), "local");
    }

    #[test]
    fn seeds_local_path_validates_existence() {
        // Eager validation surfaces typos at parse time rather than during a
        // mid-run lookup.
        let yaml = r#"
version: "1"
seeds:
  - path: /definitely/does/not/exist/12345
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn seeds_remote_repo_parses_without_fetching() {
        // Remote seeds parse but do not fetch in v1. The error fires only when
        // a lookup actually walks past the remote entry.
        let yaml = r#"
version: "1"
seeds:
  - repo: nestrai/seeds
    ref: v0.1
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.seeds.seeds.len(), 1);
        assert_eq!(cfg.seeds.seeds[0].kind_label(), "remote");
        assert_eq!(cfg.seeds.seeds[0].display(), "nestrai/seeds@v0.1");
    }

    #[test]
    fn seeds_remote_repo_without_ref_displays_repo_only() {
        let yaml = r#"
version: "1"
seeds:
  - repo: nestrai/seeds
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.seeds.seeds[0].display(), "nestrai/seeds");
    }

    #[test]
    fn seed_with_both_path_and_repo_errors() {
        let yaml = r#"
version: "1"
seeds:
  - path: ./somewhere
    repo: nestrai/seeds
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("has both 'path' and 'repo'"),
            "got: {err}"
        );
    }

    #[test]
    fn seed_with_neither_path_nor_repo_errors() {
        let yaml = r#"
version: "1"
seeds:
  - {}
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("must specify either 'path' or 'repo'"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_seeds_list_errors() {
        let yaml = r#"
version: "1"
seeds: []
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("at least one entry"), "got: {err}");
    }

    #[test]
    fn seeds_find_walks_in_order() {
        // First seed wins for whole-file overlay -- matches issue #130 spec.
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir1.path().join("agents")).unwrap();
        std::fs::create_dir_all(dir2.path().join("agents")).unwrap();
        std::fs::write(dir1.path().join("agents/Sage.yaml"), "from-dir1").unwrap();
        std::fs::write(dir2.path().join("agents/Sage.yaml"), "from-dir2").unwrap();
        std::fs::write(dir2.path().join("agents/OnlyInDir2.yaml"), "x").unwrap();

        let seeds = Seeds {
            seeds: vec![
                Seed {
                    source: SeedSource::Local {
                        display: dir1.path().display().to_string(),
                        path: dir1.path().to_path_buf(),
                    },
                },
                Seed {
                    source: SeedSource::Local {
                        display: dir2.path().display().to_string(),
                        path: dir2.path().to_path_buf(),
                    },
                },
            ],
        };

        // Both seeds have Sage.yaml -- first seed wins.
        let (idx, path) = seeds
            .find(std::path::Path::new("agents/Sage.yaml"))
            .unwrap()
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "from-dir1");

        // OnlyInDir2 falls through to the second seed.
        let (idx, path) = seeds
            .find(std::path::Path::new("agents/OnlyInDir2.yaml"))
            .unwrap()
            .unwrap();
        assert_eq!(idx, 1);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "x");
    }

    #[test]
    fn seeds_find_returns_none_when_missing_in_all() {
        let dir = tempfile::tempdir().unwrap();
        let seeds = Seeds {
            seeds: vec![Seed {
                source: SeedSource::Local {
                    display: dir.path().display().to_string(),
                    path: dir.path().to_path_buf(),
                },
            }],
        };
        let result = seeds
            .find(std::path::Path::new("agents/Missing.yaml"))
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn seeds_find_errors_when_walk_reaches_remote() {
        // Remote seeds in v1 are parse-only. If the walk falls through a
        // remote seed because no earlier local seed had the file, we error
        // explicitly so the user knows why the lookup failed.
        let dir = tempfile::tempdir().unwrap();
        let seeds = Seeds {
            seeds: vec![
                Seed {
                    source: SeedSource::Local {
                        display: dir.path().display().to_string(),
                        path: dir.path().to_path_buf(),
                    },
                },
                Seed {
                    source: SeedSource::Remote {
                        repo: "nestrai/seeds".to_string(),
                        ref_: Some("v0.1".to_string()),
                    },
                },
            ],
        };
        let err = seeds
            .find(std::path::Path::new("agents/Missing.yaml"))
            .unwrap_err();
        assert!(
            err.to_string().contains("remote seeds not yet implemented"),
            "got: {err}"
        );
        assert!(err.to_string().contains("nestrai/seeds@v0.1"), "got: {err}");
    }

    #[test]
    fn seeds_find_succeeds_before_remote_when_local_has_file() {
        // Local-first lookups must succeed even if a remote seed is declared
        // later -- otherwise the user couldn't keep remote seeds in their
        // project config until the implementation lands.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/Sage.yaml"), "ok").unwrap();
        let seeds = Seeds {
            seeds: vec![
                Seed {
                    source: SeedSource::Local {
                        display: dir.path().display().to_string(),
                        path: dir.path().to_path_buf(),
                    },
                },
                Seed {
                    source: SeedSource::Remote {
                        repo: "nestrai/seeds".to_string(),
                        ref_: None,
                    },
                },
            ],
        };
        let (idx, _) = seeds
            .find(std::path::Path::new("agents/Sage.yaml"))
            .unwrap()
            .unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn seeds_audit_line_includes_kind_labels() {
        let dir = tempfile::tempdir().unwrap();
        let seeds = Seeds {
            seeds: vec![
                Seed {
                    source: SeedSource::Local {
                        display: ".kuro/".to_string(),
                        path: dir.path().to_path_buf(),
                    },
                },
                Seed {
                    source: SeedSource::Remote {
                        repo: "nestrai/seeds".to_string(),
                        ref_: Some("v0.1".to_string()),
                    },
                },
            ],
        };
        let line = seeds.audit_line();
        assert_eq!(line, ".kuro/ (local), nestrai/seeds@v0.1 (remote)");
    }

    #[test]
    fn seeds_not_found_message_lists_all_searched() {
        let dir = tempfile::tempdir().unwrap();
        let seeds = Seeds {
            seeds: vec![
                Seed {
                    source: SeedSource::Local {
                        display: ".kuro/".to_string(),
                        path: dir.path().to_path_buf(),
                    },
                },
                Seed {
                    source: SeedSource::Local {
                        display: "~/other-seed/".to_string(),
                        path: dir.path().to_path_buf(),
                    },
                },
            ],
        };
        let msg = seeds.not_found_message("agent", "coding/rust/Sage");
        assert!(
            msg.contains("agent \"coding/rust/Sage\" not found"),
            "got: {msg}"
        );
        assert!(msg.contains(".kuro/"), "got: {msg}");
        assert!(msg.contains("~/other-seed/"), "got: {msg}");
    }

    #[test]
    fn seeds_tilde_expansion_uses_home() {
        // We don't want to depend on dirs::home_dir() being settable, so test
        // expand_tilde directly with a non-tilde path that should pass through
        // unchanged plus the simple "~" case.
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        }
        assert_eq!(expand_tilde("./local"), std::path::PathBuf::from("./local"));
        assert_eq!(
            expand_tilde("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn full_seeds_config_parses() {
        // The full project-config example from the issue: seeds + tiers +
        // roles round-trips through the parser. We use real temp dirs so the
        // eager existence check passes.
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"version: "1"

seeds:
  - path: {}
  - path: {}
  - repo: nestrai/seeds
    ref: v0.1

tiers:
  reasoning: claude/opus-4-7
"#,
            dir1.path().display(),
            dir2.path().display(),
        );
        let cfg = KotoConfig::from_yaml_str(&yaml).unwrap();
        assert_eq!(cfg.seeds.seeds.len(), 3);
        assert_eq!(cfg.seeds.seeds[0].kind_label(), "local");
        assert_eq!(cfg.seeds.seeds[1].kind_label(), "local");
        assert_eq!(cfg.seeds.seeds[2].kind_label(), "remote");
    }

    // --- overlays (issue #364) ---

    #[test]
    fn role_without_overlays_defaults_to_empty() {
        // AC6 anchor: roles that do not declare an `overlays:` block must
        // resolve to the empty overlay so the no-overlay path is
        // byte-identical to today's behaviour.
        let cfg = KotoConfig::from_yaml_str(FULL_ROLES_YAML).unwrap();
        let dev = cfg.roles.get("developer").unwrap();
        assert!(
            dev.overlays.is_empty(),
            "expected empty overlay, got: {:?}",
            dev.overlays
        );
    }

    #[test]
    fn overlays_parse_all_fields() {
        // AC1: the four supported fields round-trip through the parser into
        // the resolved RoleOverlay.
        let yaml = r#"
version: "1"
roles:
  writer:
    agent: Babis
    overlays:
      model: claude/opus-4-7
      backend: api
      rules:
        - senior-cover-letter
        - customer-anonymization
      extra_args:
        codex:
          - --sandbox
          - workspace-write
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        let writer = cfg.roles.get("writer").unwrap();
        let o = &writer.overlays;
        assert!(!o.is_empty());
        assert_eq!(o.model.as_deref(), Some("claude/opus-4-7"));
        assert_eq!(o.backend, Some(KotoBackend::Api));
        assert_eq!(
            o.rules,
            vec![
                "senior-cover-letter".to_string(),
                "customer-anonymization".to_string()
            ]
        );
        assert_eq!(
            o.extra_args
                .get(&Backend::Codex)
                .cloned()
                .unwrap_or_default(),
            vec!["--sandbox".to_string(), "workspace-write".to_string()]
        );
    }

    #[test]
    fn overlays_empty_block_is_no_op() {
        // Declaring `overlays: {}` accepts cleanly and behaves identically
        // to omitting the block -- useful when scaffolding tools want to
        // pre-write the key.
        let yaml = r#"
version: "1"
roles:
  writer:
    agent: Babis
    overlays: {}
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        let writer = cfg.roles.get("writer").unwrap();
        assert!(writer.overlays.is_empty());
    }

    #[test]
    fn overlays_unknown_field_warns_but_parses() {
        // Mirror role_unknown_field_warns_but_succeeds: typos in the
        // overlay block emit a stderr warning, parse succeeds.
        let yaml = r#"
version: "1"
roles:
  writer:
    agent: Babis
    overlays:
      model: claude/opus-4-7
      something_new: oops
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(
            cfg.roles.get("writer").unwrap().overlays.model.as_deref(),
            Some("claude/opus-4-7")
        );
    }

    #[test]
    fn overlays_bad_model_format_errors() {
        let yaml = r#"
version: "1"
roles:
  writer:
    agent: Babis
    overlays:
      model: missing-slash
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn overlays_extra_args_unknown_backend_errors() {
        // Reuses the same validation message shape as the agent file so
        // the user gets a consistent error regardless of where they wrote
        // the typo.
        let yaml = r#"
version: "1"
roles:
  writer:
    agent: Babis
    overlays:
      extra_args:
        not-a-backend:
          - --flag
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown backend in extra_args"), "got: {msg}");
        assert!(msg.contains("not-a-backend"), "got: {msg}");
    }

    #[test]
    fn overlays_extra_args_api_key_errors() {
        // Same v1 reject as agent-file `extra_args`: api has no argv to
        // extend, so accepting the key would silently drop the tokens.
        let yaml = r#"
version: "1"
roles:
  writer:
    agent: Babis
    overlays:
      extra_args:
        api:
          - --flag
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("not supported for backend 'api'"),
            "got: {err}"
        );
    }

    #[test]
    fn overlays_empty_rule_entry_errors() {
        let yaml = "
version: \"1\"
roles:
  writer:
    agent: Babis
    overlays:
      rules:
        - good-rule
        - \"\"
";
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("empty entry"), "got: {err}");
    }
}
