//! Discovery tools (#197): `list_agents`, `list_flows`, `load_agent`.
//!
//! These three tools let an MCP client (Claude Code, Codex, Cursor) ask a
//! running `kuro mcp` server "what agents and flows do you have?" and pull
//! one agent's prompt parts to inject into its current session.
//!
//! ## Design notes
//!
//! - **Seed cascade**: every enumeration walks the project config seeds in
//!   declaration order and keeps the first match per name. Mirrors the
//!   `Seeds::find` cascade used by the runner so discovery and execution see
//!   the same overlay decisions.
//! - **Stateless**: each call resolves config and seeds from the process CWD
//!   so the registry stays the empty struct it was in the scaffold (`#195`).
//!   The dependency rule (only `pub` API of `runner`/`config`/`koto_config`/
//!   `skills`) holds.
//! - **`load_agent` shape**: structured JSON `{ guide, rules: [...],
//!   skills: [...], role }` per the team review on `#197`. Field order
//!   matches the runner's prompt-assembly (`runner::build_system_prompt`):
//!   Guide > Rules > Skills > Role. A consumer that joins the non-null parts
//!   with `\n\n` reproduces what the runner builds.
//! - **Description derivation**: the team review pinned "first 200 chars of
//!   the role field, single line, frontmatter stripped" for agents and
//!   "the prompt field" for flows. Implemented in [`derive_description`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::config::{
    Backend, Defaults, RawAgentFile, load_agent_file_with_seeds, load_flow_from_str,
};
use crate::koto_config::{KOTO_DIR, KotoConfig, SeedSource, Seeds};
use crate::runner::load_guide_from_seeds;
use crate::skills::resolve_skill;

use super::error::{McpError, McpErrorCode};
use super::tools::Tool;

/// Maximum description length in Unicode scalars (characters, not bytes).
/// Pinned by the team review; longer roles are truncated.
const MAX_DESCRIPTION_CHARS: usize = 200;

// --- Seed / config helpers ---

fn discover_seeds(cwd: &Path) -> Result<Seeds, McpError> {
    match KotoConfig::load_optional(cwd) {
        Ok(Some(cfg)) => Ok(cfg.seeds),
        Ok(None) => Ok(Seeds::default_local()),
        Err(e) => Err(internal(format!("project config invalid: {}", e.message()))),
    }
}

fn project_config(cwd: &Path) -> Option<KotoConfig> {
    KotoConfig::load_optional(cwd).ok().flatten()
}

fn internal(reason: impl Into<String>) -> McpError {
    McpError::with_details(
        McpErrorCode::InternalError,
        json!({"reason": reason.into()}),
    )
}

// --- Description derivation ---

/// Strip a leading `---\n...\n---\n` YAML frontmatter block. Returns the
/// remainder. If the input does not start with `---\n`, returns the input
/// unchanged. Safety net for agents whose `role:` field contains its own
/// markdown frontmatter -- not the YAML doc level fence.
fn strip_frontmatter(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return &rest[end + 5..];
        }
        // Trailing fence at EOF without a newline after.
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

/// Collapse runs of whitespace (including newlines) into single ASCII
/// spaces, trim leading/trailing space.
fn single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true; // suppresses leading whitespace
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
/// Truncation respects char boundaries (no mid-codepoint cuts).
pub(crate) fn derive_description(role: &str) -> String {
    let stripped = strip_frontmatter(role);
    let line = single_line(stripped);
    line.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

// --- Agent enumeration ---

/// One agent file discovered through the seed cascade. The first occurrence
/// of a given `id` wins; lower-priority seeds are silently shadowed,
/// matching `Seeds::find`.
struct DiscoveredAgent {
    id: String,
    raw: RawAgentFile,
}

fn enumerate_agents(seeds: &Seeds) -> Vec<DiscoveredAgent> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<DiscoveredAgent> = Vec::new();
    for seed in &seeds.seeds {
        let SeedSource::Local {
            path,
            display: seed_display,
        } = &seed.source
        else {
            // Remote seeds are parsed but not fetched in v1. Surface a debug
            // breadcrumb so the user can tell why a remote-only seed appears
            // to contribute nothing -- not an error: discovery degrades to
            // local-only.
            warn!(seed = %seed.display(), "remote seed skipped during agent enumeration");
            continue;
        };
        let agents_dir = path.join("agents");
        if !agents_dir.is_dir() {
            continue;
        }
        walk_agents(&agents_dir, &agents_dir, seed_display, &mut seen, &mut out);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn walk_agents(
    root: &Path,
    dir: &Path,
    seed_display: &str,
    seen: &mut HashSet<String>,
    out: &mut Vec<DiscoveredAgent>,
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
        // Build the agent ID by joining segments with `/` regardless of OS.
        // Matches `agent_rel_path` in config.rs (split on `/`, push parts).
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
        out.push(DiscoveredAgent { id, raw });
    }
}

fn rel_path_to_id(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            _ => continue,
        }
    }
    parts.join("/")
}

// --- Flow enumeration ---

struct DiscoveredFlow {
    name: String,
    description: String,
    steps: Vec<String>,
}

fn enumerate_flows(seeds: &Seeds) -> Vec<DiscoveredFlow> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<DiscoveredFlow> = Vec::new();
    for seed in &seeds.seeds {
        let SeedSource::Local {
            path,
            display: seed_display,
        } = &seed.source
        else {
            warn!(seed = %seed.display(), "remote seed skipped during flow enumeration");
            continue;
        };
        let flows_dir = path.join("flows");
        let entries = match std::fs::read_dir(&flows_dir) {
            Ok(e) => e,
            Err(_) => continue, // missing flows/ is normal for some seeds
        };
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
            let flow = match load_flow_from_str(&contents) {
                Ok(f) => f,
                Err(e) => {
                    warn!(path = %p.display(), seed = %seed_display, error = %e,
                          "flow file invalid");
                    continue;
                }
            };
            out.push(DiscoveredFlow {
                name: flow.name,
                description: flow.prompt.unwrap_or_default(),
                steps: flow.steps.into_iter().map(|s| s.id).collect(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// --- Tool: list_agents ---

/// `list_agents` -- enumerate agents resolved through the seeds cascade.
pub struct ListAgents;

#[async_trait]
impl Tool for ListAgents {
    fn name(&self) -> &'static str {
        "list_agents"
    }
    fn description(&self) -> &'static str {
        "List all agents resolved through the .kuro/ seed cascade. Returns name, title and a \
         short role-derived description for each. Use this before load_agent to discover what is \
         available. Example: list_agents (no parameters)."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _arguments: Value) -> Result<Value, McpError> {
        let cwd = std::env::current_dir().map_err(|e| internal(format!("cwd: {e}")))?;
        do_list_agents(&cwd)
    }
}

fn do_list_agents(cwd: &Path) -> Result<Value, McpError> {
    let seeds = discover_seeds(cwd)?;
    let agents = enumerate_agents(&seeds);
    let entries: Vec<Value> = agents
        .into_iter()
        .map(|a| {
            json!({
                "name": a.id,
                "title": a.raw.title,
                "description": derive_description(&a.raw.role),
            })
        })
        .collect();
    Ok(json!({"agents": entries}))
}

// --- Tool: list_flows ---

/// `list_flows` -- enumerate flow YAMLs across the seed cascade.
pub struct ListFlows;

#[async_trait]
impl Tool for ListFlows {
    fn name(&self) -> &'static str {
        "list_flows"
    }
    fn description(&self) -> &'static str {
        "List all flows under .kuro/flows/ across the seed cascade. Returns name (from the YAML), \
         description (the flow's prompt field) and the list of step IDs. Example: list_flows."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _arguments: Value) -> Result<Value, McpError> {
        let cwd = std::env::current_dir().map_err(|e| internal(format!("cwd: {e}")))?;
        do_list_flows(&cwd)
    }
}

fn do_list_flows(cwd: &Path) -> Result<Value, McpError> {
    let seeds = discover_seeds(cwd)?;
    let flows = enumerate_flows(&seeds);
    let entries: Vec<Value> = flows
        .into_iter()
        .map(|f| {
            json!({
                "name": f.name,
                "description": f.description,
                "steps": f.steps,
            })
        })
        .collect();
    Ok(json!({"flows": entries}))
}

// --- Tool: load_agent ---

#[derive(Serialize)]
struct NamedContent {
    name: String,
    content: String,
}

/// Field order matches the runner's prompt-assembly: Guide, Rules, Skills,
/// Role. A consumer that joins the non-null parts with `\n\n` reproduces
/// what `runner::build_system_prompt` builds.
#[derive(Serialize)]
struct LoadAgentResult {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    guide: Option<String>,
    rules: Vec<NamedContent>,
    skills: Vec<NamedContent>,
    role: String,
}

#[derive(Deserialize)]
struct LoadAgentArgs {
    name: String,
}

/// `load_agent` -- assemble Guide + Rules + Skills + Role for one agent.
pub struct LoadAgent;

#[async_trait]
impl Tool for LoadAgent {
    fn name(&self) -> &'static str {
        "load_agent"
    }
    fn description(&self) -> &'static str {
        "Load one agent's prompt parts (Guide, rules, skills, role) so an MCP client can inject \
         them as system context. Returns structured JSON with parts in runner-assembly order; \
         join the non-null parts with two newlines to reproduce the runner's prompt. Example: \
         load_agent {\"name\":\"Neo\"}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Agent name (e.g. 'Neo', 'Bella'). Nested IDs supported via '/' (e.g. 'coding/rust/Sage')."
                }
            },
            "required": ["name"]
        })
    }
    async fn call(&self, arguments: Value) -> Result<Value, McpError> {
        let cwd = std::env::current_dir().map_err(|e| internal(format!("cwd: {e}")))?;
        let args: LoadAgentArgs = serde_json::from_value(arguments).map_err(|e| {
            McpError::with_details(
                McpErrorCode::InvalidParams,
                json!({"reason": e.to_string()}),
            )
        })?;
        if args.name.trim().is_empty() {
            return Err(McpError::with_details(
                McpErrorCode::InvalidParams,
                json!({"reason": "name must not be empty"}),
            ));
        }
        do_load_agent(&cwd, &args.name)
    }
}

fn do_load_agent(cwd: &Path, name: &str) -> Result<Value, McpError> {
    let seeds = discover_seeds(cwd)?;
    let project = project_config(cwd);

    // Synthetic defaults -- discovery never inspects the resolved Agent's
    // model or backend, but `load_agent_file_with_seeds` requires a Defaults
    // for the model fallback path.
    let defaults = Defaults {
        model: "claude-sonnet-4-5".to_string(),
        backend: Backend::ClaudeCli,
    };

    let (agent, _seed_idx, _sha) =
        load_agent_file_with_seeds(&seeds, name, &defaults, project.as_ref()).map_err(|e| {
            McpError::with_details(
                McpErrorCode::AgentMissing,
                json!({"name": name, "reason": e.to_string()}),
            )
        })?;

    // --- Guide ---
    let guide = load_guide_from_seeds(&seeds).map_err(|e| internal(format!("guide load: {e}")))?;

    // --- Rules: load each by name in agent declaration order ---
    let mut rules: Vec<NamedContent> = Vec::with_capacity(agent.rules.len());
    for rules_name in &agent.rules {
        let rel: PathBuf = std::path::Path::new("rules").join(format!("{rules_name}.md"));
        let path = match seeds.find(&rel).map_err(|e| internal(e.message()))? {
            Some((_idx, p)) => p,
            None => {
                return Err(McpError::with_details(
                    McpErrorCode::InternalError,
                    json!({
                        "reason": "rules file not found",
                        "rule": rules_name,
                    }),
                ));
            }
        };
        let content =
            std::fs::read_to_string(&path).map_err(|e| internal(format!("rules read: {e}")))?;
        rules.push(NamedContent {
            name: rules_name.clone(),
            content,
        });
    }

    // --- Skills: read from KOTO_DIR/skills (matches runner) ---
    let skills_dir = cwd.join(KOTO_DIR).join("skills");
    let mut skills: Vec<NamedContent> = Vec::with_capacity(agent.skills.len());
    for skill_name in &agent.skills {
        let content = resolve_skill(skill_name, &skills_dir).map_err(|e| {
            McpError::with_details(
                McpErrorCode::InternalError,
                json!({
                    "reason": format!("skill load failed: {e}"),
                    "skill": skill_name,
                }),
            )
        })?;
        skills.push(NamedContent {
            name: skill_name.clone(),
            content,
        });
    }

    let result = LoadAgentResult {
        name: agent.id,
        title: agent.title,
        guide,
        rules,
        skills,
        role: agent.role,
    };
    serde_json::to_value(result).map_err(|e| internal(format!("serialize: {e}")))
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ---- description derivation ----

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
        // 199 ASCII + a 4-byte char that should still fit at char 200.
        let mut role = "a".repeat(199);
        role.push('🦀'); // 4 bytes, 1 char
        role.push_str(" trailing tail that must not appear");
        let d = derive_description(&role);
        assert_eq!(d.chars().count(), 200);
        assert!(d.ends_with('🦀'));
        assert!(!d.contains("trailing"));
    }

    #[test]
    fn description_handles_only_frontmatter() {
        let role = "---\nfoo: bar\n---\n";
        // Stripped to empty, single_line returns empty.
        assert_eq!(derive_description(role), "");
    }

    // ---- enumerate_agents ----

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn minimal_agent_yaml(name: &str, title: &str, role: &str) -> String {
        format!("name: {name}\ntitle: {title}\nrole: |\n  {role}\n")
    }

    #[test]
    fn enumerate_agents_walks_seed_cascade_with_first_match_wins() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Two seeds: a/ (high priority) and b/ (low). Both define Neo; only
        // a/'s should appear. b/Bella appears unshadowed.
        write(
            &root.join("a/agents/Neo.yaml"),
            &minimal_agent_yaml("Neo", "From A", "from a"),
        );
        write(
            &root.join("b/agents/Neo.yaml"),
            &minimal_agent_yaml("Neo", "From B", "from b"),
        );
        write(
            &root.join("b/agents/Bella.yaml"),
            &minimal_agent_yaml("Bella", "Reviewer", "review code"),
        );
        let seeds = Seeds {
            seeds: vec![
                crate::koto_config::Seed {
                    source: SeedSource::Local {
                        display: "a/".to_string(),
                        path: root.join("a"),
                    },
                },
                crate::koto_config::Seed {
                    source: SeedSource::Local {
                        display: "b/".to_string(),
                        path: root.join("b"),
                    },
                },
            ],
        };
        let agents = enumerate_agents(&seeds);
        let by_id: std::collections::HashMap<&str, &DiscoveredAgent> =
            agents.iter().map(|a| (a.id.as_str(), a)).collect();
        assert_eq!(by_id.len(), 2);
        assert_eq!(by_id["Neo"].raw.title.as_deref(), Some("From A"));
        assert_eq!(by_id["Bella"].raw.title.as_deref(), Some("Reviewer"));
    }

    #[test]
    fn enumerate_agents_supports_nested_ids() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("seed/agents/coding/rust/Sage.yaml"),
            &minimal_agent_yaml("Sage", "Rust Sage", "rust matters"),
        );
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: SeedSource::Local {
                    display: "seed/".to_string(),
                    path: root.join("seed"),
                },
            }],
        };
        let agents = enumerate_agents(&seeds);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "coding/rust/Sage");
    }

    #[test]
    fn enumerate_agents_skips_invalid_yaml_without_failing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("seed/agents/Neo.yaml"),
            &minimal_agent_yaml("Neo", "Prompt", "prompts"),
        );
        write(&root.join("seed/agents/broken.yaml"), "::not yaml::");
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: SeedSource::Local {
                    display: "seed/".to_string(),
                    path: root.join("seed"),
                },
            }],
        };
        let agents = enumerate_agents(&seeds);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "Neo");
    }

    // ---- enumerate_flows ----

    fn minimal_flow_yaml(name: &str, prompt: Option<&str>, steps: &[&str]) -> String {
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

    #[test]
    fn enumerate_flows_collects_yaml_and_yml_with_step_ids_and_prompt() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("seed/flows/build.yaml"),
            &minimal_flow_yaml("build", Some("Build the thing"), &["plan", "code"]),
        );
        write(
            &root.join("seed/flows/test.yml"),
            &minimal_flow_yaml("test", None, &["run"]),
        );
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: SeedSource::Local {
                    display: "seed/".to_string(),
                    path: root.join("seed"),
                },
            }],
        };
        let flows = enumerate_flows(&seeds);
        assert_eq!(flows.len(), 2);
        let by_name: std::collections::HashMap<&str, &DiscoveredFlow> =
            flows.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["build"].description, "Build the thing");
        assert_eq!(by_name["build"].steps, vec!["plan", "code"]);
        assert_eq!(by_name["test"].description, ""); // missing prompt
        assert_eq!(by_name["test"].steps, vec!["run"]);
    }

    #[test]
    fn enumerate_flows_first_match_wins_across_seeds() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("hi/flows/build.yaml"),
            &minimal_flow_yaml("build", Some("HI"), &["plan"]),
        );
        write(
            &root.join("lo/flows/build.yaml"),
            &minimal_flow_yaml("build", Some("LO"), &["plan", "extra"]),
        );
        let seeds = Seeds {
            seeds: vec![
                crate::koto_config::Seed {
                    source: SeedSource::Local {
                        display: "hi/".to_string(),
                        path: root.join("hi"),
                    },
                },
                crate::koto_config::Seed {
                    source: SeedSource::Local {
                        display: "lo/".to_string(),
                        path: root.join("lo"),
                    },
                },
            ],
        };
        let flows = enumerate_flows(&seeds);
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].description, "HI");
    }

    // ---- do_list_agents (end-to-end on tempdir) ----

    /// Build a project config YAML pointing at one seed path. We pass an
    /// absolute path in tests so seed resolution does not depend on the
    /// process CWD (which is shared between parallel tests).
    fn project_config_yaml(seed: &Path) -> String {
        format!("version: \"1\"\nseeds:\n  - path: {}\n", seed.display())
    }

    #[test]
    fn do_list_agents_returns_sorted_descriptions_via_project_config() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        // Project config points at .kuro and a remote seed (which we'll skip).
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        write(
            &cwd.join(".kuro/agents/Zeb.yaml"),
            &minimal_agent_yaml("Zeb", "Last", "zeb role"),
        );
        write(
            &cwd.join(".kuro/agents/Aaron.yaml"),
            &minimal_agent_yaml("Aaron", "First", "aaron role"),
        );
        let v = do_list_agents(cwd).unwrap();
        let names: Vec<&str> = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        // Sorted by id.
        assert_eq!(names, vec!["Aaron", "Zeb"]);
        assert_eq!(v["agents"][0]["title"], "First");
        assert_eq!(v["agents"][0]["description"], "aaron role");
    }

    #[test]
    fn do_list_agents_empty_when_no_kuro_dir() {
        let tmp = TempDir::new().unwrap();
        let v = do_list_agents(tmp.path()).unwrap();
        assert_eq!(v["agents"].as_array().unwrap().len(), 0);
    }

    // ---- do_list_flows ----

    #[test]
    fn do_list_flows_returns_sorted_with_steps_and_prompt() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        write(
            &cwd.join(".kuro/flows/build.yaml"),
            &minimal_flow_yaml("build", Some("desc"), &["plan", "code"]),
        );
        let v = do_list_flows(cwd).unwrap();
        let arr = v["flows"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "build");
        assert_eq!(arr[0]["description"], "desc");
        assert_eq!(arr[0]["steps"], json!(["plan", "code"]));
    }

    // ---- do_load_agent ----

    #[test]
    fn do_load_agent_returns_structured_parts_in_assembly_order() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        write(&cwd.join(".kuro/Guide.md"), "GUIDE-CONTENT");
        write(&cwd.join(".kuro/rules/style.md"), "STYLE-CONTENT");
        write(&cwd.join(".kuro/skills/skill-x/01.md"), "SKILL-X-CONTENT");
        write(
            &cwd.join(".kuro/agents/Neo.yaml"),
            "name: Neo\n\
             title: Prompt Engineer\n\
             role: |\n  ROLE-CONTENT\n\
             rules:\n  - style\n\
             skills:\n  - skill-x\n",
        );
        let v = do_load_agent(cwd, "Neo").unwrap();

        // Each part is present and looked up by key.
        assert_eq!(v["name"], "Neo");
        assert_eq!(v["title"], "Prompt Engineer");
        assert_eq!(v["guide"], "GUIDE-CONTENT");
        assert_eq!(v["rules"][0]["name"], "style");
        assert_eq!(v["rules"][0]["content"], "STYLE-CONTENT");
        assert_eq!(v["skills"][0]["name"], "skill-x");
        assert_eq!(v["skills"][0]["content"], "SKILL-X-CONTENT");
        assert_eq!(v["role"].as_str().unwrap().trim(), "ROLE-CONTENT");

        // Acceptance criterion: a consumer who joins the non-null parts in
        // assembly order (Guide > Rules > Skills > Role) reproduces what
        // `runner::build_system_prompt` would emit. Verify by calling the
        // runner directly with the same inputs and comparing byte-for-byte.
        let mut parts: Vec<String> = Vec::new();
        if let Some(g) = v["guide"].as_str() {
            parts.push(g.to_string());
        }
        for r in v["rules"].as_array().unwrap() {
            parts.push(r["content"].as_str().unwrap().to_string());
        }
        for s in v["skills"].as_array().unwrap() {
            parts.push(s["content"].as_str().unwrap().to_string());
        }
        parts.push(v["role"].as_str().unwrap().to_string());
        let joined = parts.join("\n\n");

        let agent = crate::config::Agent {
            id: "Neo".to_string(),
            name: "Neo".to_string(),
            title: Some("Prompt Engineer".to_string()),
            role: v["role"].as_str().unwrap().to_string(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            rules: vec!["style".to_string()],
            skills: vec!["skill-x".to_string()],
            env: std::collections::HashMap::new(),
            extra_args: std::collections::HashMap::new(),
        };
        let mut rules_cache = std::collections::HashMap::new();
        rules_cache.insert("style".to_string(), "STYLE-CONTENT".to_string());
        let mut skills_cache = std::collections::HashMap::new();
        skills_cache.insert("skill-x".to_string(), "SKILL-X-CONTENT".to_string());
        let runner_prompt = crate::runner::build_system_prompt(
            &agent,
            &Some("GUIDE-CONTENT".to_string()),
            &rules_cache,
            &skills_cache,
        );
        assert_eq!(joined, runner_prompt);
    }

    #[test]
    fn do_load_agent_unknown_returns_agent_missing() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        let err = do_load_agent(cwd, "Nonexistent").unwrap_err();
        assert_eq!(err.code, McpErrorCode::AgentMissing);
        let details = err.details.unwrap();
        assert_eq!(details["name"], "Nonexistent");
        assert!(details["reason"].is_string());
    }

    #[test]
    fn do_load_agent_missing_rule_returns_internal_error() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        write(
            &cwd.join(".kuro/agents/Neo.yaml"),
            "name: Neo\nrole: |\n  r\nrules:\n  - missing-rule\n",
        );
        let err = do_load_agent(cwd, "Neo").unwrap_err();
        assert_eq!(err.code, McpErrorCode::InternalError);
        let details = err.details.unwrap();
        assert_eq!(details["rule"], "missing-rule");
    }

    #[test]
    fn do_load_agent_no_rules_no_skills_returns_empty_arrays() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        write(
            &cwd.join(".kuro/agents/Mini.yaml"),
            "name: Mini\nrole: |\n  short\n",
        );
        let v = do_load_agent(cwd, "Mini").unwrap();
        assert_eq!(v["rules"].as_array().unwrap().len(), 0);
        assert_eq!(v["skills"].as_array().unwrap().len(), 0);
        assert!(v["guide"].is_null());
    }

    // ---- Tool::call: input validation ----

    #[tokio::test]
    async fn load_agent_tool_rejects_empty_name() {
        let tool = LoadAgent;
        let err = tool.call(json!({"name": "  "})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn load_agent_tool_rejects_missing_name() {
        let tool = LoadAgent;
        let err = tool.call(json!({})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn list_tools_have_valid_descriptors() {
        // Sanity that the static metadata satisfies the registry's invariants.
        let mut reg = super::super::tools::ToolRegistry::new();
        reg.register(Box::new(ListAgents)).unwrap();
        reg.register(Box::new(ListFlows)).unwrap();
        reg.register(Box::new(LoadAgent)).unwrap();
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["list_agents", "list_flows", "load_agent"]);
    }
}
