//! Seed cascade introspection (`kuro context`, #366).
//!
//! Resolves the project's seed list and walks each seed's `agents/`,
//! `rules/` and `flows/` directories so an AI assistant entering a
//! consumer repo can run one command and see the full inventory: which
//! seeds are active, which artefacts each seed contributes and -- with
//! conflicts resolved -- which artefact each name actually points at.
//!
//! ## Why this module exists
//!
//! The walkers used to live in [`crate::mcp::discovery`] and were
//! pulled up here in #366. Two consumers now sit at the same level
//! (`kuro context` and `kuro mcp`) and both need to enumerate seeds
//! the same way. Putting the shared helpers under `mcp/` would invert
//! the dependency direction (a CLI command importing from the MCP
//! server module); lifting them up keeps both consumers siblings of
//! a single seed-walker source of truth.
//!
//! ## Output shape
//!
//! [`resolve`] returns a [`ResolvedContext`] with two views:
//!
//! * `seeds[]` -- per-seed contributions in declaration order. Each
//!   entry lists the agents, rules and flows present in that seed
//!   regardless of priority. Useful for "what does this seed provide?".
//! * `effective` -- first-match-wins cascade. Each name appears once,
//!   tagged with the winning seed and the lower-priority seeds where
//!   the same name also lives (`shadows`, see field docs). This
//!   is what the runner would actually load.
//!
//! ## Empty-input discipline
//!
//! Missing `.kuro/`, missing seed paths, missing `agents/`/`rules/`/
//! `flows/` subdirs all degrade silently to empty contributions. The
//! command's job is *inventory*, not validation -- `kuro validate`
//! covers semantic checks. Only a malformed `.kuro/config.yaml`
//! surfaces an error (via [`KotoConfigError`]).

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::warn;

use crate::config::{
    Flow, RawAgentFile, graph_unresolved_roles, load_flow_any_from_str_with_project,
    parse_linear_flow_inventory,
};
use crate::koto_config::{KotoConfig, KotoConfigError, Seed, SeedSource, Seeds};

/// Maximum description length in Unicode scalars (characters, not bytes).
/// Pinned by the team review for MCP `list_agents`; re-used here so the
/// CLI and the MCP tool show identical strings for the same agent.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 200;

/// Stable JSON schema version. Consumers (AI clients embedding the
/// output in their prompts) check this to detect breaking changes.
/// Bump only when an existing field's shape changes; additive fields
/// (new optional keys, new array elements) leave the version alone.
pub const CONTEXT_SCHEMA_VERSION: &str = "1";

// --- Errors ---

/// Errors surfaced by [`resolve`]. Only a malformed project config
/// produces an error today; everything else degrades to empty.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("project config invalid: {0}")]
    Config(String),
}

impl From<KotoConfigError> for ContextError {
    fn from(e: KotoConfigError) -> Self {
        ContextError::Config(e.message())
    }
}

// --- Data shape (public, serialised as JSON v1) ---

/// Top-level inventory result. Carries enough information for an AI
/// assistant to know what's available in the current cwd's cascade.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedContext {
    /// Schema version. [`CONTEXT_SCHEMA_VERSION`].
    pub version: &'static str,
    /// Absolute cwd the inventory was taken from.
    pub cwd: PathBuf,
    /// Per-seed contributions, declaration order preserved.
    pub seeds: Vec<SeedContribution>,
    /// First-match-wins view: what the runner would actually load.
    pub effective: EffectiveCascade,
}

/// One seed's contribution. Listed regardless of priority -- duplicates
/// across seeds are NOT removed here. The shadowing relationship is
/// captured separately in [`EffectiveCascade`].
#[derive(Debug, Clone, Serialize)]
pub struct SeedContribution {
    /// Audit string from [`Seed::display`] (e.g. ".kuro/" or
    /// "github.com/foo/bar@main").
    pub display: String,
    /// `"local"` or `"remote"`.
    pub kind: &'static str,
    /// Absolute filesystem path for local seeds, `None` for remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// `true` for local seeds whose directory exists on disk, `false`
    /// for local seeds that resolve to a missing path. Always `false`
    /// for remote seeds (not fetched in v1).
    pub exists: bool,
    /// Verbatim contents of the seed-root `SEED.md` if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_md: Option<String>,
    /// Agents under this seed's `agents/` directory.
    pub agents: Vec<AgentEntry>,
    /// Rules under this seed's `rules/` directory.
    pub rules: Vec<RuleEntry>,
    /// Flows under this seed's `flows/` directory.
    pub flows: Vec<FlowEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowEntry {
    pub name: String,
    pub description: String,
    pub steps: Vec<String>,
    /// Role names this flow references that resolve through neither a
    /// flow-local `roles:` block nor the project-level bindings in
    /// `.kuro/config.yaml` (#389). Empty (and absent from the JSON wire
    /// format -- additive to schema v1) for flows that are runnable from
    /// on-disk configuration alone. Non-empty marks the flow as "listed
    /// but not runnable without extra binding" -- e.g. a CLI `--role`
    /// override at run time. The entry stays in the inventory because a
    /// role-broken local flow still shadows a same-named seed flow.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_roles: Vec<String>,
}

/// First-match-wins view of the cascade.
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveCascade {
    pub agents: Vec<EffectiveItem>,
    pub rules: Vec<EffectiveItem>,
    pub flows: Vec<EffectiveItem>,
}

/// One name in the effective cascade.
///
/// `shadows` is read from the **winner's** perspective: it lists the
/// display strings of lower-priority seeds where the same name also
/// appears and which the winner therefore shadows. Empty for names
/// that only appear in one seed. The naming is locked into the v1
/// JSON wire format -- consumers can rely on the field meaning
/// "seeds the winner suppresses" for the lifetime of schema v1.
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveItem {
    pub name: String,
    pub seed: String,
    pub shadows: Vec<String>,
}

// --- Public API ---

/// Resolve the cascade visible from `cwd`. See module docs for the
/// degradation rules.
pub fn resolve(cwd: &Path) -> Result<ResolvedContext, ContextError> {
    // Load the project config once and derive BOTH the seed list and the
    // project-level role bindings from it (#389). The roles feed into
    // flow enumeration so a flow whose `role:` is bound only in
    // `.kuro/config.yaml` lists exactly like it runs.
    let koto_config = KotoConfig::load_optional(cwd)?;
    let project_roles = koto_config
        .as_ref()
        .map(|c| c.role_agent_map())
        .unwrap_or_default();
    let seeds = koto_config
        .map(|c| c.seeds)
        .unwrap_or_else(Seeds::default_local);
    let seeds = rebase_seeds_for_cwd(seeds, cwd);
    let contributions: Vec<SeedContribution> = seeds
        .seeds
        .iter()
        .map(|s| seed_contribution(s, &project_roles))
        .collect();
    let effective = build_effective(&contributions);
    Ok(ResolvedContext {
        version: CONTEXT_SCHEMA_VERSION,
        cwd: cwd.to_path_buf(),
        seeds: contributions,
        effective,
    })
}

/// Render `ctx` as JSON v1 to `out`.
pub fn render_json(ctx: &ResolvedContext, out: &mut dyn Write) -> serde_json::Result<()> {
    serde_json::to_writer_pretty(&mut *out, ctx)?;
    // Trailing newline so the output is friendly to terminals and pipes.
    let _ = out.write_all(b"\n");
    Ok(())
}

/// Render `ctx` as a human-readable inventory to `out`. Intended for
/// terminal consumption; the JSON renderer is the wire format.
pub fn render_human(ctx: &ResolvedContext, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "kuro context (cwd: {})", ctx.cwd.display())?;
    writeln!(out)?;
    writeln!(out, "Seeds (top wins on conflict):")?;
    for (i, seed) in ctx.seeds.iter().enumerate() {
        let idx = i + 1;
        let exists_tag = if seed.exists { "exists" } else { "missing" };
        writeln!(
            out,
            "  [{idx}] {display:<38} ({kind}, {exists_tag})",
            idx = idx,
            display = seed.display,
            kind = seed.kind,
            exists_tag = exists_tag,
        )?;
        match &seed.seed_md {
            Some(text) => {
                let lines = text.lines().count();
                writeln!(out, "       SEED.md: {lines} lines")?;
            }
            None => writeln!(out, "       SEED.md: (none)")?,
        }
        writeln!(
            out,
            "       agents: {}",
            list_or_none(seed.agents.iter().map(|a| a.name.as_str()))
        )?;
        writeln!(
            out,
            "       rules:  {}",
            list_or_none(seed.rules.iter().map(|r| r.name.as_str()))
        )?;
        // Flows carry an inline `(unresolved role: ...)` suffix when
        // their roles resolve through neither the flow nor the project
        // config (#389) -- listed, but not runnable as configured.
        let flow_names: Vec<String> = seed
            .flows
            .iter()
            .map(|f| format!("{}{}", f.name, unresolved_roles_suffix(&f.unresolved_roles)))
            .collect();
        writeln!(
            out,
            "       flows:  {}",
            list_or_none(flow_names.iter().map(String::as_str))
        )?;
    }
    writeln!(out)?;
    writeln!(out, "Effective cascade (what agents see this session):")?;
    let no_suffixes = HashMap::new();
    write_effective_section(out, "agents", &ctx.effective.agents, &no_suffixes)?;
    write_effective_section(out, "rules ", &ctx.effective.rules, &no_suffixes)?;
    // Mirror the per-seed `(unresolved role: ...)` suffix on the winning
    // flow entries so both views of the inventory tell the same story.
    let flow_suffixes: HashMap<(&str, &str), String> = ctx
        .seeds
        .iter()
        .flat_map(|s| {
            s.flows
                .iter()
                .filter(|f| !f.unresolved_roles.is_empty())
                .map(|f| {
                    (
                        (s.display.as_str(), f.name.as_str()),
                        unresolved_roles_suffix(&f.unresolved_roles),
                    )
                })
        })
        .collect();
    write_effective_section(out, "flows ", &ctx.effective.flows, &flow_suffixes)?;
    writeln!(out)?;
    writeln!(
        out,
        "Hint: re-run with --format json for machine-readable output."
    )?;
    Ok(())
}

fn list_or_none<'a, I: Iterator<Item = &'a str>>(names: I) -> String {
    let v: Vec<&str> = names.collect();
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

/// Human-render suffix for a flow with unresolved roles (#389). Empty
/// string when every role is bound.
fn unresolved_roles_suffix(roles: &[String]) -> String {
    if roles.is_empty() {
        String::new()
    } else {
        format!(" (unresolved role: {})", roles.join(", "))
    }
}

fn write_effective_section(
    out: &mut dyn Write,
    label: &str,
    items: &[EffectiveItem],
    suffixes: &HashMap<(&str, &str), String>,
) -> io::Result<()> {
    if items.is_empty() {
        writeln!(out, "  {label} (0): (none)")?;
        return Ok(());
    }
    // Inline a `[shadows: ...]` suffix on items with non-empty shadow
    // lists so the human renderer mirrors the JSON wire format. The
    // suffix is only emitted for cross-seed conflicts -- non-conflict
    // items stay compact (`name (from seed)`). `suffixes` (keyed by
    // winning seed display + item name) appends extra per-item context;
    // today only flows use it, for the unresolved-roles marker (#389).
    let rendered: Vec<String> = items
        .iter()
        .map(|it| {
            let extra = suffixes
                .get(&(it.seed.as_str(), it.name.as_str()))
                .map(String::as_str)
                .unwrap_or_default();
            if it.shadows.is_empty() {
                format!("{} (from {}){extra}", it.name, it.seed)
            } else {
                format!(
                    "{} (from {}) [shadows: {}]{extra}",
                    it.name,
                    it.seed,
                    it.shadows.join(", "),
                )
            }
        })
        .collect();
    writeln!(out, "  {label} ({}): {}", items.len(), rendered.join(", "))
}

// --- Seed discovery ---

/// Load the project's seed list, falling back to the implicit local
/// `.kuro/` seed when no config is present. Relative seed paths are
/// rebased against the caller-provided `cwd` so the function is safe
/// to call from any working directory.
pub(crate) fn discover_seeds(cwd: &Path) -> Result<Seeds, KotoConfigError> {
    let seeds = match KotoConfig::load_optional(cwd)? {
        Some(cfg) => cfg.seeds,
        None => Seeds::default_local(),
    };
    Ok(rebase_seeds_for_cwd(seeds, cwd))
}

/// Rebase any relative `Local` seed paths against `cwd`. Absolute paths
/// and remote seeds pass through unchanged. See the original docs on
/// the moved-from helper in `mcp/discovery.rs` for the #293 backstory:
/// without this rebase, downstream filesystem walks resolve relative
/// paths against the process CWD instead of the explicit `cwd` arg.
pub(crate) fn rebase_seeds_for_cwd(seeds: Seeds, cwd: &Path) -> Seeds {
    let rebased = seeds
        .seeds
        .into_iter()
        .map(|seed| match seed.source {
            SeedSource::Local { display, path } if path.is_relative() => Seed {
                source: SeedSource::Local {
                    display,
                    path: cwd.join(path),
                },
            },
            other => Seed { source: other },
        })
        .collect();
    Seeds { seeds: rebased }
}

// --- Per-seed walkers ---

fn seed_contribution(seed: &Seed, project_roles: &HashMap<String, String>) -> SeedContribution {
    match &seed.source {
        SeedSource::Local { display, path } => {
            let exists = path.is_dir();
            let seed_md = if exists { read_seed_md(path) } else { None };
            let agents = if exists {
                enumerate_agents_in_seed(path, display)
            } else {
                Vec::new()
            };
            let rules = if exists {
                enumerate_rules_in_seed(path, display)
            } else {
                Vec::new()
            };
            let flows = if exists {
                enumerate_flows_in_seed(path, display, project_roles)
            } else {
                Vec::new()
            };
            SeedContribution {
                display: display.clone(),
                kind: "local",
                path: Some(path.clone()),
                exists,
                seed_md,
                agents,
                rules,
                flows,
            }
        }
        SeedSource::Remote { .. } => {
            // Remote seeds are parsed but not fetched in v1. Surface
            // them with empty contents and a debug breadcrumb so the
            // user can tell why a remote-only seed appears blank --
            // mirrors the MCP discovery behaviour.
            warn!(seed = %seed.display(), "remote seed listed but not fetched in v1");
            SeedContribution {
                display: seed.display(),
                kind: "remote",
                path: None,
                exists: false,
                seed_md: None,
                agents: Vec::new(),
                rules: Vec::new(),
                flows: Vec::new(),
            }
        }
    }
}

fn read_seed_md(seed_path: &Path) -> Option<String> {
    let p = seed_path.join("SEED.md");
    match std::fs::read_to_string(&p) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(path = %p.display(), error = %e, "SEED.md unreadable");
            None
        }
    }
}

/// Walk one seed's `agents/` directory and return entries in id order.
/// Per-seed listing -- no cross-seed dedup happens here.
pub(crate) fn enumerate_agents_in_seed(seed_path: &Path, seed_display: &str) -> Vec<AgentEntry> {
    let agents_dir = seed_path.join("agents");
    if !agents_dir.is_dir() {
        return Vec::new();
    }
    let mut acc: Vec<(String, RawAgentFile)> = Vec::new();
    let mut seen = HashSet::new();
    walk_agents(&agents_dir, &agents_dir, seed_display, &mut seen, &mut acc);
    acc.sort_by(|a, b| a.0.cmp(&b.0));
    acc.into_iter()
        .map(|(id, raw)| AgentEntry {
            name: id,
            title: raw.title.clone(),
            description: derive_description(&raw.role),
        })
        .collect()
}

/// Walk one seed's `rules/` directory and return entries in name order.
pub(crate) fn enumerate_rules_in_seed(seed_path: &Path, seed_display: &str) -> Vec<RuleEntry> {
    let rules_dir = seed_path.join("rules");
    let entries = match std::fs::read_dir(&rules_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<RuleEntry> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() || p.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let name = match p.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let contents = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %p.display(), seed = %seed_display, error = %e,
                      "rule file unreadable");
                continue;
            }
        };
        out.push(RuleEntry {
            name,
            description: rule_description(&contents),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Walk one seed's `flows/` directory and return entries in name order.
///
/// `project_roles` is the project-level `role -> agent` map (#389,
/// [`crate::koto_config::KotoConfig::role_agent_map`]). Linear flows are
/// parsed with these bindings -- the same map the runner passes -- so a
/// flow whose `role:` resolves only through `.kuro/config.yaml` lists
/// instead of being warn-dropped as invalid.
pub(crate) fn enumerate_flows_in_seed(
    seed_path: &Path,
    seed_display: &str,
    project_roles: &HashMap<String, String>,
) -> Vec<FlowEntry> {
    let flows_dir = seed_path.join("flows");
    let entries = match std::fs::read_dir(&flows_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    // Sort the directory listing before iterating: `read_dir` order is
    // filesystem-dependent, and when a seed contains e.g. both
    // `build.yaml` and `build.md` the stem-dedup below otherwise picks
    // the winner non-deterministically. Precedence is pinned to
    // yaml > yml > md via an explicit extension rank -- a plain filename
    // sort would put `.md` FIRST ('m' < 'y') and contradict the runtime,
    // where `resolve_flow_path` tries `<name>.yaml` before `<name>.md`.
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| flow_extension_rank(p).is_some())
        .collect();
    paths.sort_by_key(|p| {
        (
            p.file_stem().map(|s| s.to_os_string()),
            flow_extension_rank(p),
        )
    });
    let mut out: Vec<FlowEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in paths {
        let bare = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if bare.is_empty() || !seen.insert(bare.clone()) {
            continue;
        }
        let contents = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %p.display(), seed = %seed_display, error = %e,
                      "flow file unreadable");
                continue;
            }
        };
        if let Some(entry) = flow_entry_from_contents(&contents, &p, seed_display, project_roles) {
            out.push(entry);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Dedup / precedence rank for flow file extensions: yaml > yml > md.
/// `None` for everything else (not a flow file).
fn flow_extension_rank(p: &Path) -> Option<u8> {
    match p.extension().and_then(|s| s.to_str()) {
        Some("yaml") => Some(0),
        Some("yml") => Some(1),
        Some("md") => Some(2),
        _ => None,
    }
}

/// Parse one flow file's contents into a [`FlowEntry`], or `None` (with
/// a warning) when the file is genuinely malformed.
///
/// Uses the polymorphic loader so graph flows show up alongside linear
/// flows, and the string-only loader keeps the inventory tolerant of
/// missing sibling `task_file:` files: a broken prompt link is the
/// runner's problem, not a reason to hide the flow.
///
/// Failure handling (#389): when the strict load fails, a raw re-parse
/// classifies the failure. A linear flow that parses structurally but
/// references roles bound by neither the flow nor the project lists WITH
/// `unresolved_roles` populated -- it still shadows same-named seed flows
/// at runtime, so hiding it would misrepresent the cascade. Anything
/// else (malformed YAML/Markdown, semantic errors other than role
/// resolution) warns and skips, exactly the pre-#389 strictness -- a
/// malformed file can never be mistaken for a role miss because the raw
/// parse itself fails on it.
fn flow_entry_from_contents(
    contents: &str,
    path: &Path,
    seed_display: &str,
    project_roles: &HashMap<String, String>,
) -> Option<FlowEntry> {
    // Markdown flows are always graph-shaped -- no YAML probe.
    if path.extension().and_then(|s| s.to_str()) == Some("md") {
        return match crate::config_md::load_graph_flow_from_md(contents) {
            Ok(g) => Some(graph_flow_entry(g, project_roles)),
            Err(e) => {
                warn!(path = %path.display(), seed = %seed_display, error = %e,
                      "flow file invalid");
                None
            }
        };
    }
    match load_flow_any_from_str_with_project(contents, project_roles) {
        Ok(Flow::Linear(f)) => Some(FlowEntry {
            name: f.name,
            description: f.prompt.unwrap_or_default(),
            steps: f.steps.into_iter().map(|s| s.id).collect(),
            unresolved_roles: Vec::new(),
        }),
        Ok(Flow::Graph(g)) => Some(graph_flow_entry(g, project_roles)),
        Err(e) => {
            // Failure-path classification: does the flow raw-parse as a
            // linear flow with unbound roles? Then it is inventory-worthy.
            match parse_linear_flow_inventory(contents, project_roles) {
                Ok(inv) if !inv.unresolved_roles.is_empty() => Some(FlowEntry {
                    name: inv.name,
                    description: inv.prompt.unwrap_or_default(),
                    steps: inv.steps,
                    unresolved_roles: inv.unresolved_roles,
                }),
                _ => {
                    warn!(path = %path.display(), seed = %seed_display, error = %e,
                          "flow file invalid");
                    None
                }
            }
        }
    }
}

fn graph_flow_entry(
    g: crate::config::GraphFlow,
    project_roles: &HashMap<String, String>,
) -> FlowEntry {
    let unresolved_roles = graph_unresolved_roles(&g, project_roles);
    FlowEntry {
        name: g.name,
        description: g.prompt.unwrap_or_default(),
        steps: g.graph.keys().cloned().collect(),
        unresolved_roles,
    }
}

// --- Effective cascade (first-match-wins) ---

fn build_effective(seeds: &[SeedContribution]) -> EffectiveCascade {
    let agents = effective_for(seeds, |s| s.agents.iter().map(|a| a.name.as_str()));
    let rules = effective_for(seeds, |s| s.rules.iter().map(|r| r.name.as_str()));
    let flows = effective_for(seeds, |s| s.flows.iter().map(|f| f.name.as_str()));
    EffectiveCascade {
        agents,
        rules,
        flows,
    }
}

fn effective_for<'a, F, I>(seeds: &'a [SeedContribution], get_names: F) -> Vec<EffectiveItem>
where
    F: Fn(&'a SeedContribution) -> I,
    I: Iterator<Item = &'a str>,
{
    // Pass 1: pin the winner for each name (declaration order, first
    // wins). Pass 2: collect lower-priority seeds where the same
    // name also appears.
    let mut winner: Vec<(String, &str)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for seed in seeds {
        for name in get_names(seed) {
            if seen.insert(name.to_string()) {
                winner.push((name.to_string(), seed.display.as_str()));
            }
        }
    }
    let mut out: Vec<EffectiveItem> = winner
        .into_iter()
        .map(|(name, seed_display)| EffectiveItem {
            name,
            seed: seed_display.to_string(),
            shadows: Vec::new(),
        })
        .collect();
    // Pass 2: fill `shadows`. For each item, walk the seeds and
    // record any seed whose display is NOT the winner but that also
    // declares the same name -- those are the entries the winner
    // suppresses.
    for item in &mut out {
        for seed in seeds {
            if seed.display == item.seed {
                continue;
            }
            if get_names(seed).any(|n| n == item.name.as_str()) {
                item.shadows.push(seed.display.clone());
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// --- Filesystem walkers (lifted verbatim from mcp/discovery.rs) ---

fn walk_agents(
    root: &Path,
    dir: &Path,
    seed_display: &str,
    seen: &mut HashSet<String>,
    out: &mut Vec<(String, RawAgentFile)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "agents dir unreadable");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_agents(root, &path, seed_display, seen, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let id = rel_path_to_id(&rel.with_extension(""));
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path.display(), seed = %seed_display, error = %e,
                      "agent file unreadable");
                continue;
            }
        };
        let raw: RawAgentFile = match serde_yaml::from_str(&contents) {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %path.display(), seed = %seed_display, error = %e,
                      "agent file invalid yaml");
                continue;
            }
        };
        out.push((id, raw));
    }
}

pub(crate) fn rel_path_to_id(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            _ => continue,
        }
    }
    parts.join("/")
}

// --- Description derivation ---

/// Strip a leading `---\n...\n---\n` YAML frontmatter block. Returns the
/// remainder. If the input does not start with `---\n`, returns the input
/// unchanged.
pub(crate) fn strip_frontmatter(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return &rest[end + 5..];
        }
        if let Some(end) = rest.find("\n---") {
            let after = end + 4;
            if after >= rest.len() {
                return "";
            }
            return &rest[after..];
        }
    }
    s
}

/// Collapse runs of whitespace into single ASCII spaces, trim ends.
pub(crate) fn single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// First [`MAX_DESCRIPTION_CHARS`] Unicode chars of the normalised role.
/// Used by agent listings -- matches the MCP `list_agents` description.
pub(crate) fn derive_description(role: &str) -> String {
    let stripped = strip_frontmatter(role);
    let line = single_line(stripped);
    line.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

/// Pull a one-line description from a rule markdown file.
///
/// Steps:
/// 1. Strip leading YAML frontmatter.
/// 2. Skip blank lines.
/// 3. If the first non-blank line is a heading (`# ...`), strip
///    leading `#` runs and use the heading text.
/// 4. Otherwise take the first non-blank line as-is.
/// 5. Collapse whitespace, truncate to [`MAX_DESCRIPTION_CHARS`].
fn rule_description(contents: &str) -> String {
    let body = strip_frontmatter(contents);
    let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let stripped = first.trim_start();
    let text = stripped.trim_start_matches('#').trim_start();
    let line = single_line(text);
    line.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn minimal_agent(name: &str, title: &str, role: &str) -> String {
        format!("name: {name}\ntitle: {title}\nrole: |\n  {role}\n")
    }

    fn minimal_flow(name: &str, prompt: Option<&str>, steps: &[&str]) -> String {
        let mut s = format!("version: \"1\"\nname: {name}\n");
        if let Some(p) = prompt {
            s.push_str(&format!("prompt: {p}\n"));
        }
        s.push_str("flow:\n");
        for step in steps {
            s.push_str(&format!("  {step}:\n    agent: Dummy\n    task: t\n"));
        }
        s
    }

    fn write_project_with_seed(cwd: &Path, seed_path: &Path) {
        write(
            &cwd.join(".kuro/config.yaml"),
            &format!(
                "version: \"1\"\nseeds:\n  - path: {}\n",
                seed_path.display()
            ),
        );
    }

    /// Project config with one seed AND a project-level role binding --
    /// the #389 fixture shape.
    fn write_project_with_seed_and_role(cwd: &Path, seed_path: &Path, role: &str, agent: &str) {
        write(
            &cwd.join(".kuro/config.yaml"),
            &format!(
                "version: \"1\"\nseeds:\n  - path: {}\nroles:\n  {role}:\n    agent: {agent}\n",
                seed_path.display()
            ),
        );
    }

    /// Linear flow whose step binds via `role:` with NO flow-local
    /// `roles:` block -- resolvable only through project roles.
    fn role_only_flow(name: &str, role: &str) -> String {
        format!(
            "version: \"1\"\nname: {name}\nprompt: p\nflow:\n  work:\n    role: {role}\n    task: t\n"
        )
    }

    /// Markdown graph flow with a single agent state using `role`.
    fn role_only_md_flow(name: &str, role: &str) -> String {
        format!(
            "---\nformat: kuromaku-flow/v1\n---\n\n# {name}\n\n---\n\n## work\n*role: {role}*\n\nDo the thing.\n\n-> done: finished\n-> work: retry\n\n---\n\n## done\n*final: complete*\n"
        )
    }

    /// Pre-#389 call shape: seed contribution without project roles.
    fn seed_contribution_no_roles(seed: &Seed) -> SeedContribution {
        seed_contribution(seed, &HashMap::new())
    }

    // ---- resolve: degradation rules ----

    #[test]
    fn resolve_empty_returns_default_seed_with_empty_contributions() {
        // No project config, no `.kuro/` on disk. `resolve` must
        // surface a single local seed with empty contributions and
        // `exists: false` -- this is the AI-assistant first-run case.
        let tmp = TempDir::new().unwrap();
        let ctx = resolve(tmp.path()).unwrap();
        assert_eq!(ctx.version, "1");
        assert_eq!(ctx.cwd, tmp.path());
        assert_eq!(ctx.seeds.len(), 1);
        let seed = &ctx.seeds[0];
        assert_eq!(seed.display, ".kuro/");
        assert_eq!(seed.kind, "local");
        assert!(
            !seed.exists,
            "default .kuro/ in empty tempdir does not exist"
        );
        assert!(seed.agents.is_empty());
        assert!(seed.rules.is_empty());
        assert!(seed.flows.is_empty());
        assert!(seed.seed_md.is_none());
        assert!(ctx.effective.agents.is_empty());
        assert!(ctx.effective.rules.is_empty());
        assert!(ctx.effective.flows.is_empty());
    }

    #[test]
    fn resolve_walks_each_seed_independently_without_dedup() {
        // Two seeds both define Neo. Per-seed listing must show Neo
        // in both seeds; effective must show Neo once (winner) with
        // the other seed listed in `shadows` (the seeds the winner
        // suppresses).
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let seed_a = cwd.join("a");
        let seed_b = cwd.join("b");
        write(
            &seed_a.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "A", "from a"),
        );
        write(
            &seed_b.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "B", "from b"),
        );
        write(
            &seed_b.join("agents/Bella.yaml"),
            &minimal_agent("Bella", "Reviewer", "review"),
        );
        write(
            &cwd.join(".kuro/config.yaml"),
            &format!(
                "version: \"1\"\nseeds:\n  - path: {}\n  - path: {}\n",
                seed_a.display(),
                seed_b.display(),
            ),
        );

        let ctx = resolve(cwd).unwrap();
        assert_eq!(ctx.seeds.len(), 2);
        assert_eq!(
            ctx.seeds[0]
                .agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Neo"]
        );
        let b_names: Vec<&str> = ctx.seeds[1]
            .agents
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert!(b_names.contains(&"Bella"));
        assert!(b_names.contains(&"Neo"));

        // Effective: Neo wins from seed [0]; Bella from seed [1]; Neo's
        // `shadows` list contains seed [1] (the loser).
        let neo = ctx
            .effective
            .agents
            .iter()
            .find(|i| i.name == "Neo")
            .unwrap();
        assert_eq!(neo.seed, ctx.seeds[0].display);
        assert_eq!(neo.shadows, vec![ctx.seeds[1].display.clone()]);
        let bella = ctx
            .effective
            .agents
            .iter()
            .find(|i| i.name == "Bella")
            .unwrap();
        assert_eq!(bella.seed, ctx.seeds[1].display);
        assert!(bella.shadows.is_empty());
    }

    // ---- per-seed rules walker ----

    #[test]
    fn enumerate_rules_in_seed_returns_name_and_description() {
        // Three rule files: one with YAML frontmatter, one starting
        // with a `# heading`, one plain. All three must surface a
        // single-line description following the derivation rules.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("rules/with-frontmatter.md"),
            "---\ntags: foo\n---\n\nReal text.\n",
        );
        write(
            &seed.join("rules/with-heading.md"),
            "# Heading text\n\nBody continues.\n",
        );
        write(
            &seed.join("rules/plain.md"),
            "Plain first line\nSecond line ignored.\n",
        );
        let rules = enumerate_rules_in_seed(&seed, "seed/");
        assert_eq!(rules.len(), 3);
        let by_name: std::collections::HashMap<&str, &RuleEntry> =
            rules.iter().map(|r| (r.name.as_str(), r)).collect();
        assert_eq!(by_name["with-frontmatter"].description, "Real text.");
        assert_eq!(by_name["with-heading"].description, "Heading text");
        assert_eq!(by_name["plain"].description, "Plain first line");
    }

    #[test]
    fn enumerate_rules_in_seed_returns_empty_for_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let rules = enumerate_rules_in_seed(tmp.path(), "seed/");
        assert!(rules.is_empty());
    }

    // ---- SEED.md ----

    #[test]
    fn seed_md_loaded_when_present() {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        let body = "## seed\n\nHello.\n";
        write(&seed.join("SEED.md"), body);
        // Need an agents/ dir (or any dir) so `exists` is true and the
        // walk runs.
        fs::create_dir_all(seed.join("agents")).unwrap();
        let contribution = seed_contribution_no_roles(&Seed {
            source: SeedSource::Local {
                display: "seed/".to_string(),
                path: seed.clone(),
            },
        });
        assert_eq!(contribution.seed_md.as_deref(), Some(body));
    }

    #[test]
    fn seed_md_absent_yields_none() {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        fs::create_dir_all(seed.join("agents")).unwrap();
        let contribution = seed_contribution_no_roles(&Seed {
            source: SeedSource::Local {
                display: "seed/".to_string(),
                path: seed,
            },
        });
        assert!(contribution.seed_md.is_none());
    }

    // ---- per-seed agents walker (verifies the moved walker still works) ----

    #[test]
    fn enumerate_agents_in_seed_supports_nested_ids() {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("agents/coding/rust/Sage.yaml"),
            &minimal_agent("Sage", "Rust Sage", "rust matters"),
        );
        let agents = enumerate_agents_in_seed(&seed, "seed/");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "coding/rust/Sage");
        assert_eq!(agents[0].title.as_deref(), Some("Rust Sage"));
        assert_eq!(agents[0].description, "rust matters");
    }

    #[test]
    fn enumerate_agents_in_seed_skips_invalid_yaml() {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "P", "p"),
        );
        write(&seed.join("agents/broken.yaml"), "::not yaml::");
        let agents = enumerate_agents_in_seed(&seed, "seed/");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "Neo");
    }

    // ---- per-seed flows walker ----

    fn minimal_graph_flow(name: &str, prompt: Option<&str>, states: &[&str]) -> String {
        // Smallest graph flow that schema-validates: one initial state
        // ending in `final:` so reachability and dead-end checks pass.
        // We deliberately use `final:` for trailing states because the
        // schema requires every state to terminate either via `next:`
        // or `final:`.
        assert!(!states.is_empty());
        let mut s = format!("version: \"1\"\nname: {name}\n");
        if let Some(p) = prompt {
            s.push_str(&format!("prompt: {p}\n"));
        }
        s.push_str(&format!("initial: {}\n", states[0]));
        s.push_str("graph:\n");
        for (i, st) in states.iter().enumerate() {
            s.push_str(&format!("  {st}:\n    final: \"done at {st}\"\n"));
            // Note: a real graph would have edges between states. For
            // this fixture we only need the listing to surface the
            // state IDs, so making them all terminal keeps validation
            // happy without distracting from the test intent.
            let _ = i;
        }
        s
    }

    #[test]
    fn enumerate_flows_in_seed_includes_graph_flows() {
        // Regression for kuromaku's own seed: the previous walker used
        // the linear-only `load_flow_from_str` and silently dropped
        // graph-shaped flows. Pin the broader contract here so the
        // inventory keeps showing both shapes.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/build.yaml"),
            &minimal_flow("build", Some("linear"), &["plan"]),
        );
        write(
            &seed.join("flows/implement.yaml"),
            &minimal_graph_flow("implement", Some("graph"), &["precheck"]),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &HashMap::new());
        let by_name: std::collections::HashMap<&str, &FlowEntry> =
            flows.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name.len(), 2, "both linear and graph flows must list");
        assert_eq!(by_name["build"].steps, vec!["plan"]);
        assert_eq!(by_name["implement"].description, "graph");
        assert_eq!(by_name["implement"].steps, vec!["precheck"]);
    }

    #[test]
    fn enumerate_flows_in_seed_picks_yaml_over_yml_on_stem_collision() {
        // Two files share the same stem. `read_dir` order is filesystem
        // dependent so without sorting the winner would flap between
        // runs. Sorting by full filename gives `.yaml` precedence over
        // `.yml` -- pin that contract so the inventory is reproducible.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/build.yaml"),
            &minimal_flow("build-yaml", Some("yaml wins"), &["plan"]),
        );
        write(
            &seed.join("flows/build.yml"),
            &minimal_flow("build-yml", Some("yml loses"), &["other"]),
        );
        // Run multiple times to defeat any per-run randomness from
        // read_dir iteration order on resilient filesystems.
        for _ in 0..5 {
            let flows = enumerate_flows_in_seed(&seed, "seed/", &HashMap::new());
            assert_eq!(flows.len(), 1);
            assert_eq!(flows[0].name, "build-yaml");
            assert_eq!(flows[0].description, "yaml wins");
        }
    }

    #[test]
    fn enumerate_flows_in_seed_collects_yaml_and_yml() {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/build.yaml"),
            &minimal_flow("build", Some("Build the thing"), &["plan", "code"]),
        );
        write(
            &seed.join("flows/test.yml"),
            &minimal_flow("test", None, &["run"]),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &HashMap::new());
        assert_eq!(flows.len(), 2);
        let by_name: std::collections::HashMap<&str, &FlowEntry> =
            flows.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["build"].description, "Build the thing");
        assert_eq!(by_name["build"].steps, vec!["plan", "code"]);
        assert_eq!(by_name["test"].description, "");
        assert_eq!(by_name["test"].steps, vec!["run"]);
    }

    // ---- project-level role bindings in flow enumeration (#389) ----

    fn dev_roles() -> HashMap<String, String> {
        [("developer".to_string(), "Noah".to_string())].into()
    }

    #[test]
    fn enumerate_flows_in_seed_resolves_role_via_project_config() {
        // AC1 core regression: a linear flow whose `role:` binds only
        // through the project config must list, with no unresolved
        // marker. Before #389 the walker parsed with an empty roles map
        // and warn-dropped the flow as invalid.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/ship.yaml"),
            &role_only_flow("ship", "developer"),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &dev_roles());
        assert_eq!(flows.len(), 1, "project-bound role must not drop the flow");
        assert_eq!(flows[0].name, "ship");
        assert_eq!(flows[0].steps, vec!["work"]);
        assert!(flows[0].unresolved_roles.is_empty());
    }

    #[test]
    fn enumerate_flows_in_seed_lists_markdown_flow_with_project_role() {
        // AC3: markdown flows were not enumerated at all before #389
        // (extension filter accepted only yaml/yml). The runtime resolves
        // `<name>.md`, so the inventory must too.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/ship.md"),
            &role_only_md_flow("ship", "developer"),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &dev_roles());
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].name, "ship");
        assert_eq!(flows[0].steps, vec!["work", "done"]);
        assert!(flows[0].unresolved_roles.is_empty());
    }

    #[test]
    fn enumerate_flows_in_seed_flow_local_role_needs_no_project_binding() {
        // AC4 (flow side of the precedence): a flow-local `roles:`
        // default keeps working with zero project roles, and a project
        // binding for the same role does not break the flow either --
        // flow-level wins at runtime, both load here.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/local.yaml"),
            "version: \"1\"\nname: local\nprompt: p\nroles:\n  developer:\n    default: FlowDev\nflow:\n  work:\n    role: developer\n    task: t\n",
        );
        for roles in [HashMap::new(), dev_roles()] {
            let flows = enumerate_flows_in_seed(&seed, "seed/", &roles);
            assert_eq!(flows.len(), 1, "roles: {roles:?}");
            assert!(flows[0].unresolved_roles.is_empty());
        }
    }

    #[test]
    fn enumerate_flows_in_seed_marks_unresolved_roles_all_shapes() {
        // AC5: a genuinely unresolved role must not hide the flow --
        // it stays listed (it still shadows same-named seed flows) with
        // `unresolved_roles` populated. Covers all three on-disk shapes.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/linear.yaml"),
            &role_only_flow("linear", "ghost"),
        );
        write(
            &seed.join("flows/graph.yaml"),
            "version: \"1\"\nname: graph\ninitial: work\ngraph:\n  work:\n    role: ghost\n    task: t\n    next:\n      - done: finished\n      - work: retry\n  done:\n    final: \"complete\"\n",
        );
        write(
            &seed.join("flows/mark.md"),
            &role_only_md_flow("mark", "ghost"),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &dev_roles());
        assert_eq!(flows.len(), 3, "unresolved roles must not drop flows");
        for f in &flows {
            assert_eq!(
                f.unresolved_roles,
                vec!["ghost".to_string()],
                "flow {}",
                f.name
            );
        }
    }

    #[test]
    fn enumerate_flows_in_seed_skips_malformed_files_not_role_misses() {
        // AC6: malformed YAML / Markdown still warns and skips -- it
        // must never be classified as a role miss. A healthy flow next
        // to the broken ones keeps listing.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(&seed.join("flows/broken.yaml"), "::not yaml::");
        write(&seed.join("flows/broken-md.md"), "# no frontmatter\n");
        // Structurally fine but semantically broken beyond roles
        // (references an unknown step) -- pre-#389 strictness applies.
        write(
            &seed.join("flows/badstep.yaml"),
            "version: \"1\"\nname: badstep\nprompt: p\nflow:\n  work:\n    role: developer\n    task: t\n    needs: [nonexistent]\n",
        );
        write(
            &seed.join("flows/good.yaml"),
            &role_only_flow("good", "developer"),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &dev_roles());
        assert_eq!(
            flows.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["good"]
        );
    }

    #[test]
    fn enumerate_flows_in_seed_stem_precedence_yaml_over_yml_over_md() {
        // Dedup precedence pinned yaml > yml > md, matching the runtime
        // (`resolve_flow_path` tries .yaml before .md). A naive filename
        // sort would put .md first ('m' < 'y') -- regression guard.
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("flows/build.yaml"),
            &minimal_flow("build-yaml", Some("yaml wins"), &["plan"]),
        );
        write(
            &seed.join("flows/build.md"),
            &role_only_md_flow("build-md", "developer"),
        );
        write(
            &seed.join("flows/test.yml"),
            &minimal_flow("test-yml", Some("yml wins"), &["run"]),
        );
        write(
            &seed.join("flows/test.md"),
            &role_only_md_flow("test-md", "developer"),
        );
        let flows = enumerate_flows_in_seed(&seed, "seed/", &dev_roles());
        let names: Vec<&str> = flows.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["build-yaml", "test-yml"]);
    }

    #[test]
    fn resolve_lists_project_role_only_flow_in_both_views() {
        // End-to-end AC1: `resolve` derives the project roles from the
        // same config that declares the seeds, so a role-only flow shows
        // in the per-seed listing AND the effective cascade.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let seed = cwd.join("seed");
        write(
            &seed.join("flows/ship.yaml"),
            &role_only_flow("ship", "developer"),
        );
        write_project_with_seed_and_role(cwd, &seed, "developer", "Noah");
        let ctx = resolve(cwd).unwrap();
        let contribution = ctx
            .seeds
            .iter()
            .find(|s| s.display == seed.display().to_string())
            .expect("seed listed");
        assert_eq!(contribution.flows.len(), 1);
        assert_eq!(contribution.flows[0].name, "ship");
        assert!(
            ctx.effective.flows.iter().any(|f| f.name == "ship"),
            "flow must appear in the effective cascade"
        );
    }

    #[test]
    fn render_json_omits_unresolved_roles_when_empty_and_lists_when_set() {
        // AC2 + wire-format guard: healthy flows serialize WITHOUT the
        // `unresolved_roles` key (additive to schema v1); role-starved
        // flows carry it.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let seed = cwd.join("seed");
        write(
            &seed.join("flows/ship.yaml"),
            &role_only_flow("ship", "developer"),
        );
        write(
            &seed.join("flows/stuck.yaml"),
            &role_only_flow("stuck", "ghost"),
        );
        write_project_with_seed_and_role(cwd, &seed, "developer", "Noah");
        let ctx = resolve(cwd).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_json(&ctx, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["version"], "1");
        let seed_entry = v["seeds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["display"] == seed.display().to_string())
            .unwrap();
        let flows = seed_entry["flows"].as_array().unwrap();
        let ship = flows.iter().find(|f| f["name"] == "ship").unwrap();
        assert!(
            ship.get("unresolved_roles").is_none(),
            "healthy flow must not carry the key: {ship}"
        );
        let stuck = flows.iter().find(|f| f["name"] == "stuck").unwrap();
        assert_eq!(stuck["unresolved_roles"], serde_json::json!(["ghost"]));
        // Both flows stay in the effective cascade.
        let effective: Vec<&str> = v["effective"]["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(effective, vec!["ship", "stuck"]);
    }

    #[test]
    fn render_human_marks_unresolved_roles_in_both_sections() {
        // AC5 human side: the suffix appears in the per-seed listing and
        // on the winning entry in the effective cascade, and healthy
        // flows stay unmarked.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let seed = cwd.join("seed");
        write(
            &seed.join("flows/ship.yaml"),
            &role_only_flow("ship", "developer"),
        );
        write(
            &seed.join("flows/stuck.yaml"),
            &role_only_flow("stuck", "ghost"),
        );
        write_project_with_seed_and_role(cwd, &seed, "developer", "Noah");
        let ctx = resolve(cwd).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_human(&ctx, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("stuck (unresolved role: ghost)"),
            "per-seed suffix missing: {s}"
        );
        assert!(
            s.contains(&format!(
                "stuck (from {}) (unresolved role: ghost)",
                seed.display()
            )),
            "effective-cascade suffix missing: {s}"
        );
        assert!(
            !s.contains("ship (unresolved"),
            "healthy flow must not be marked: {s}"
        );
    }

    // ---- rendering ----

    #[test]
    fn render_json_round_trips_through_serde() {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        write(
            &seed.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "T", "role"),
        );
        write_project_with_seed(tmp.path(), &seed);
        let ctx = resolve(tmp.path()).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_json(&ctx, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["version"], "1");
        // Two seeds are present: the project's `.kuro/` declared from
        // the config and the explicit `seed` path. Verify the explicit
        // one is listed and exposes Neo.
        let seeds = v["seeds"].as_array().unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s["display"] == seed.display().to_string())
        );
        // Effective agents include Neo.
        let agents = v["effective"]["agents"].as_array().unwrap();
        assert!(agents.iter().any(|a| a["name"] == "Neo"));
    }

    #[test]
    fn render_json_uses_shadows_field_for_v1_wire_format() {
        // Pin the v1 JSON schema: the field must be spelled `shadows`
        // (not `shadowed_by`) and read from the winner's perspective.
        // AI clients embed this output at session start and the field
        // semantics need to match the field name -- the previous name
        // was inverted and would have anchored a misreading of the
        // cascade into every downstream consumer.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let seed_a = cwd.join("a");
        let seed_b = cwd.join("b");
        write(
            &seed_a.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "A", "winner"),
        );
        write(
            &seed_b.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "B", "loser"),
        );
        write(
            &cwd.join(".kuro/config.yaml"),
            &format!(
                "version: \"1\"\nseeds:\n  - path: {}\n  - path: {}\n",
                seed_a.display(),
                seed_b.display(),
            ),
        );
        let ctx = resolve(cwd).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_json(&ctx, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        let neo = v["effective"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "Neo")
            .unwrap();
        // The field exists under the new name and points at the loser
        // seed -- proving "shadows" reads as "what the winner suppresses".
        let shadows = neo["shadows"].as_array().expect("shadows is an array");
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0], seed_b.display().to_string());
        // And the legacy name is gone -- consumers must migrate to
        // `shadows` before any other downstream code touches v1.
        assert!(neo.get("shadowed_by").is_none());
    }

    #[test]
    fn render_human_appends_shadows_suffix_when_cascade_conflicts() {
        // The human renderer must surface shadow relationships too --
        // otherwise an operator reading the human inventory misses
        // that a name lives in multiple seeds and a non-default seed
        // would supply a different version. Inline `[shadows: ...]`
        // suffix only for items with non-empty conflict lists.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let seed_a = cwd.join("a");
        let seed_b = cwd.join("b");
        write(
            &seed_a.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "A", "winner"),
        );
        write(
            &seed_b.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "B", "loser"),
        );
        write(
            &seed_a.join("agents/Solo.yaml"),
            &minimal_agent("Solo", "S", "no conflict"),
        );
        write(
            &cwd.join(".kuro/config.yaml"),
            &format!(
                "version: \"1\"\nseeds:\n  - path: {}\n  - path: {}\n",
                seed_a.display(),
                seed_b.display(),
            ),
        );
        let ctx = resolve(cwd).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_human(&ctx, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Neo wins from seed_a and shadows seed_b -- suffix must show
        // immediately after Neo's "(from ...)" segment.
        let neo_with_suffix = format!(
            "Neo (from {}) [shadows: {}]",
            seed_a.display(),
            seed_b.display(),
        );
        assert!(
            s.contains(&neo_with_suffix),
            "expected Neo's shadows suffix in render: {s}"
        );
        // Solo lives in one seed only -- the rendered string must contain
        // its plain "(from ...)" form WITHOUT an immediately following
        // `[shadows:` segment. The effective cascade is on a single
        // comma-separated line so we cannot split by lines.
        let solo_plain = format!("Solo (from {})", seed_a.display());
        assert!(
            s.contains(&solo_plain),
            "Solo plain entry missing from render: {s}"
        );
        let solo_with_shadow_prefix = format!("{} [shadows:", solo_plain);
        assert!(
            !s.contains(&solo_with_shadow_prefix),
            "Solo has no conflicts but got shadows suffix: {s}"
        );
    }

    #[test]
    fn render_human_lists_each_seed_with_index_and_kind() {
        let tmp = TempDir::new().unwrap();
        let seed_a = tmp.path().join("a");
        let seed_b = tmp.path().join("b");
        write(
            &seed_a.join("agents/Neo.yaml"),
            &minimal_agent("Neo", "T", "role"),
        );
        write(&seed_b.join("rules/style.md"), "Style guide first line.\n");
        write(
            &seed_b.join("flows/build.yaml"),
            &minimal_flow("build", Some("Build"), &["plan"]),
        );
        // Project config declares both seeds in priority order so the
        // numbered listing covers more than a single index.
        write(
            &tmp.path().join(".kuro/config.yaml"),
            &format!(
                "version: \"1\"\nseeds:\n  - path: {}\n  - path: {}\n",
                seed_a.display(),
                seed_b.display(),
            ),
        );
        let ctx = resolve(tmp.path()).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_human(&ctx, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Header.
        assert!(s.contains("kuro context"));
        // Each seed numbered from 1.
        assert!(s.contains("[1]"));
        assert!(s.contains("[2]"));
        // Kind labels appear on each seed line.
        assert!(s.contains("local"));
        // Per-seed sections include the labels.
        assert!(s.contains("agents:"));
        assert!(s.contains("rules:"));
        assert!(s.contains("flows:"));
        // Effective cascade section.
        assert!(s.contains("Effective cascade"));
        // The Neo agent should appear somewhere in the output.
        assert!(s.contains("Neo"));
        // The format-json hint.
        assert!(s.contains("--format json"));
    }

    // ---- remote seeds ----

    #[test]
    fn remote_seed_is_listed_with_empty_contents() {
        let seed = Seed {
            source: SeedSource::Remote {
                repo: "github.com/foo/bar".to_string(),
                ref_: Some("main".to_string()),
            },
        };
        let contribution = seed_contribution_no_roles(&seed);
        assert_eq!(contribution.kind, "remote");
        assert!(contribution.path.is_none());
        assert!(!contribution.exists);
        assert!(contribution.agents.is_empty());
        assert!(contribution.rules.is_empty());
        assert!(contribution.flows.is_empty());
        assert!(contribution.seed_md.is_none());
        assert!(contribution.display.contains("foo/bar"));
    }

    // ---- description derivation (lifted from mcp/discovery.rs) ----

    #[test]
    fn description_collapses_whitespace_and_truncates() {
        let role = "First line.\n\nSecond  line   with   gaps.\n";
        assert_eq!(
            derive_description(role),
            "First line. Second line with gaps."
        );
    }

    #[test]
    fn description_strips_leading_frontmatter() {
        let role = "---\ntitle: Neo\n---\nThe actual role text.";
        assert_eq!(derive_description(role), "The actual role text.");
    }

    #[test]
    fn description_truncates_at_200_chars() {
        let role = "x".repeat(500);
        let d = derive_description(&role);
        assert_eq!(d.chars().count(), 200);
    }

    #[test]
    fn description_truncation_respects_unicode_boundaries() {
        let mut role = "a".repeat(199);
        role.push('🦀');
        role.push_str(" trailing tail that must not appear");
        let d = derive_description(&role);
        assert_eq!(d.chars().count(), 200);
        assert!(d.ends_with('🦀'));
        assert!(!d.contains("trailing"));
    }

    // ---- rebase_seeds_for_cwd (regression coverage for #293) ----

    #[test]
    fn rebase_seeds_for_cwd_leaves_absolute_paths_untouched() {
        let tmp = TempDir::new().unwrap();
        let abs = tmp.path().join("seed-abs");
        let seeds = Seeds {
            seeds: vec![Seed {
                source: SeedSource::Local {
                    display: abs.display().to_string(),
                    path: abs.clone(),
                },
            }],
        };
        let other = TempDir::new().unwrap();
        let rebased = rebase_seeds_for_cwd(seeds, other.path());
        match &rebased.seeds[0].source {
            SeedSource::Local { path, .. } => assert_eq!(path, &abs),
            other => panic!("expected local, got {other:?}"),
        }
    }

    #[test]
    fn discover_seeds_rebases_relative_default_against_cwd() {
        let tmp = TempDir::new().unwrap();
        let seeds = discover_seeds(tmp.path()).unwrap();
        assert_eq!(seeds.seeds.len(), 1);
        match &seeds.seeds[0].source {
            SeedSource::Local { path, .. } => {
                assert_eq!(path, &tmp.path().join(".kuro"));
                assert!(path.is_absolute());
            }
            other => panic!("expected local, got {other:?}"),
        }
    }

    #[test]
    fn rebase_seeds_for_cwd_passes_remote_seeds_through() {
        // Remote seeds carry no path; rebase must round-trip them
        // identically. Lifted from the removed mcp/discovery.rs test
        // so the regression guard survives the #366 refactor.
        let seeds = Seeds {
            seeds: vec![Seed {
                source: SeedSource::Remote {
                    repo: "github.com/foo/bar".to_string(),
                    ref_: Some("main".to_string()),
                },
            }],
        };
        let tmp = TempDir::new().unwrap();
        let rebased = rebase_seeds_for_cwd(seeds, tmp.path());
        match &rebased.seeds[0].source {
            SeedSource::Remote { repo, ref_ } => {
                assert_eq!(repo, "github.com/foo/bar");
                assert_eq!(ref_.as_deref(), Some("main"));
            }
            other => panic!("expected remote, got {other:?}"),
        }
    }
}
