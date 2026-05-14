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
//!   the same name also lives (`shadowed_by`, see field docs). This
//!   is what the runner would actually load.
//!
//! ## Empty-input discipline
//!
//! Missing `.kuro/`, missing seed paths, missing `agents/`/`rules/`/
//! `flows/` subdirs all degrade silently to empty contributions. The
//! command's job is *inventory*, not validation -- `kuro validate`
//! covers semantic checks. Only a malformed `.kuro/config.yaml`
//! surfaces an error (via [`KotoConfigError`]).

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::warn;

use crate::config::{RawAgentFile, load_flow_from_str};
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
/// `shadowed_by` is read from the **winner's** perspective: it lists
/// the display strings of lower-priority seeds where the same name
/// also appears (and which the winner therefore shadows). The naming
/// is locked into the v1 JSON wire format.
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveItem {
    pub name: String,
    pub seed: String,
    pub shadowed_by: Vec<String>,
}

// --- Public API ---

/// Resolve the cascade visible from `cwd`. See module docs for the
/// degradation rules.
pub fn resolve(cwd: &Path) -> Result<ResolvedContext, ContextError> {
    let seeds = discover_seeds(cwd)?;
    let contributions: Vec<SeedContribution> = seeds.seeds.iter().map(seed_contribution).collect();
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
        writeln!(
            out,
            "       flows:  {}",
            list_or_none(seed.flows.iter().map(|f| f.name.as_str()))
        )?;
    }
    writeln!(out)?;
    writeln!(out, "Effective cascade (what agents see this session):")?;
    write_effective_section(out, "agents", &ctx.effective.agents)?;
    write_effective_section(out, "rules ", &ctx.effective.rules)?;
    write_effective_section(out, "flows ", &ctx.effective.flows)?;
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

fn write_effective_section(
    out: &mut dyn Write,
    label: &str,
    items: &[EffectiveItem],
) -> io::Result<()> {
    if items.is_empty() {
        writeln!(out, "  {label} (0): (none)")?;
        return Ok(());
    }
    let rendered: Vec<String> = items
        .iter()
        .map(|it| format!("{} (from {})", it.name, it.seed))
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

fn seed_contribution(seed: &Seed) -> SeedContribution {
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
                enumerate_flows_in_seed(path, display)
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
pub(crate) fn enumerate_flows_in_seed(seed_path: &Path, seed_display: &str) -> Vec<FlowEntry> {
    let flows_dir = seed_path.join("flows");
    let entries = match std::fs::read_dir(&flows_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<FlowEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in entries.flatten() {
        let p = entry.path();
        let ext = p.extension().and_then(|s| s.to_str());
        if ext != Some("yaml") && ext != Some("yml") {
            continue;
        }
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
        // Mirror the MCP discovery loop's tolerance: list flows even
        // when a sibling `task_file:` is missing -- a broken prompt
        // link is the runner's problem, not a reason to hide the
        // flow from inventory.
        let flow = match load_flow_from_str(&contents) {
            Ok(f) => f,
            Err(e) => {
                warn!(path = %p.display(), seed = %seed_display, error = %e,
                      "flow file invalid");
                continue;
            }
        };
        out.push(FlowEntry {
            name: flow.name,
            description: flow.prompt.unwrap_or_default(),
            steps: flow.steps.into_iter().map(|s| s.id).collect(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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
            shadowed_by: Vec::new(),
        })
        .collect();
    // Pass 2: fill shadowed_by. For each item, walk the seeds and
    // record any seed whose display is NOT the winner but that also
    // declares the same name.
    for item in &mut out {
        for seed in seeds {
            if seed.display == item.seed {
                continue;
            }
            if get_names(seed).any(|n| n == item.name.as_str()) {
                item.shadowed_by.push(seed.display.clone());
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
        // the other seed listed in `shadowed_by`.
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
        // shadowed_by lists seed [1].
        let neo = ctx
            .effective
            .agents
            .iter()
            .find(|i| i.name == "Neo")
            .unwrap();
        assert_eq!(neo.seed, ctx.seeds[0].display);
        assert_eq!(neo.shadowed_by, vec![ctx.seeds[1].display.clone()]);
        let bella = ctx
            .effective
            .agents
            .iter()
            .find(|i| i.name == "Bella")
            .unwrap();
        assert_eq!(bella.seed, ctx.seeds[1].display);
        assert!(bella.shadowed_by.is_empty());
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
        let contribution = seed_contribution(&Seed {
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
        let contribution = seed_contribution(&Seed {
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
        let flows = enumerate_flows_in_seed(&seed, "seed/");
        assert_eq!(flows.len(), 2);
        let by_name: std::collections::HashMap<&str, &FlowEntry> =
            flows.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["build"].description, "Build the thing");
        assert_eq!(by_name["build"].steps, vec!["plan", "code"]);
        assert_eq!(by_name["test"].description, "");
        assert_eq!(by_name["test"].steps, vec!["run"]);
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
        let contribution = seed_contribution(&seed);
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
