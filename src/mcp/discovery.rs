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
//! - **Guide gate (#245)**: by default `guide` is `null`. The cwd's
//!   `Guide.md` only loads when the caller passes `include_guide: true`.
//!   Mirrors the `kuro task --include-project-context` gate so the same
//!   "agents stay repo-agnostic by default" invariant holds across CLI and
//!   MCP invocation paths.
//! - **Description derivation**: the team review pinned "first 200 chars of
//!   the role field, single line, frontmatter stripped" for agents and
//!   "the prompt field" for flows. Implemented in [`derive_description`].

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Backend, Defaults, load_agent_file_with_seeds};
use crate::context::{
    self, AgentEntry, FlowEntry, enumerate_agents_in_seed, enumerate_flows_in_seed,
};
use crate::koto_config::{KOTO_DIR, KotoConfig, Seeds};
use crate::runner::load_guide_from_seeds;
use crate::skills::resolve_skill;

use super::error::{McpError, McpErrorCode};
use super::tools::Tool;

// --- Seed / config helpers ---

fn discover_seeds(cwd: &Path) -> Result<Seeds, McpError> {
    context::discover_seeds(cwd)
        .map_err(|e| internal(format!("project config invalid: {}", e.message())))
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

// --- Agent / flow enumeration ---
//
// Both helpers walk the seed list and apply first-match-wins to keep the
// MCP `list_agents` / `list_flows` shape stable. The per-seed walkers
// live in [`crate::context`] (lifted in #366); this module composes them
// with deduplication.

fn enumerate_agents_for_cascade(seeds: &Seeds) -> Vec<AgentEntry> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<AgentEntry> = Vec::new();
    for seed in &seeds.seeds {
        let display = seed.display();
        let Some(path) = seed.local_path() else {
            continue;
        };
        for a in enumerate_agents_in_seed(path, &display) {
            if seen.insert(a.name.clone()) {
                out.push(a);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn enumerate_flows_for_cascade(seeds: &Seeds) -> Vec<FlowEntry> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<FlowEntry> = Vec::new();
    for seed in &seeds.seeds {
        let display = seed.display();
        let Some(path) = seed.local_path() else {
            continue;
        };
        for f in enumerate_flows_in_seed(path, &display) {
            if seen.insert(f.name.clone()) {
                out.push(f);
            }
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
    let agents = enumerate_agents_for_cascade(&seeds);
    let entries: Vec<Value> = agents
        .into_iter()
        .map(|a| {
            json!({
                "name": a.name,
                "title": a.title,
                "description": a.description,
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
    let flows = enumerate_flows_for_cascade(&seeds);
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
    /// Issue #245: opt-in for cwd `Guide.md` injection. Default false keeps
    /// the response repo-agnostic so MCP clients cannot accidentally inherit
    /// the kuromaku-server cwd's project identity. Mirrors the
    /// `kuro task --include-project-context` flag.
    #[serde(default)]
    include_guide: bool,
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
         join the non-null parts with two newlines to reproduce the runner's prompt. The cwd \
         Guide is omitted unless `include_guide: true` is passed, so agents stay repo-agnostic \
         by default (issue #245). Example: load_agent {\"name\":\"Neo\"}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Agent name (e.g. 'Neo', 'Bella'). Nested IDs supported via '/' (e.g. 'coding/rust/Sage')."
                },
                "include_guide": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, inject the cwd project's Guide.md (mirrors `kuro task --include-project-context`). Default false keeps the response repo-agnostic so the kuromaku-server cwd's project identity does not leak into the agent."
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
        do_load_agent(&cwd, &args.name, args.include_guide)
    }
}

fn do_load_agent(cwd: &Path, name: &str, include_guide: bool) -> Result<Value, McpError> {
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
    // Gate (#245): same principle as `kuro task`/`kuro chat` -- the cwd
    // Guide.md only loads when the caller explicitly asks for project
    // context. Default off keeps an MCP server started in any project
    // directory from leaking that project's identity into the agent it
    // hands back to the client.
    let guide = if include_guide {
        load_guide_from_seeds(&seeds).map_err(|e| internal(format!("guide load: {e}")))?
    } else {
        None
    };

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
    use crate::koto_config::{Seed, SeedSource};
    use std::fs;
    use tempfile::TempDir;

    // Walker-level coverage (per-seed agent/flow/rule enumeration, the
    // description derivation helpers, frontmatter and unicode handling)
    // lives in `crate::context::tests` after the #366 refactor. The
    // tests below cover the MCP-specific composition: first-match-wins
    // dedup across seeds, the JSON tool responses, and `load_agent`.

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn minimal_agent_yaml(name: &str, title: &str, role: &str) -> String {
        format!("name: {name}\ntitle: {title}\nrole: |\n  {role}\n")
    }

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

    // ---- cascade-level enumerators (first-match-wins) ----

    #[test]
    fn enumerate_agents_for_cascade_keeps_first_match() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
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
                Seed {
                    source: SeedSource::Local {
                        display: "a/".to_string(),
                        path: root.join("a"),
                    },
                },
                Seed {
                    source: SeedSource::Local {
                        display: "b/".to_string(),
                        path: root.join("b"),
                    },
                },
            ],
        };
        let agents = enumerate_agents_for_cascade(&seeds);
        let by_name: std::collections::HashMap<&str, &AgentEntry> =
            agents.iter().map(|a| (a.name.as_str(), a)).collect();
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name["Neo"].title.as_deref(), Some("From A"));
        assert_eq!(by_name["Bella"].title.as_deref(), Some("Reviewer"));
    }

    #[test]
    fn enumerate_flows_for_cascade_keeps_first_match() {
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
                Seed {
                    source: SeedSource::Local {
                        display: "hi/".to_string(),
                        path: root.join("hi"),
                    },
                },
                Seed {
                    source: SeedSource::Local {
                        display: "lo/".to_string(),
                        path: root.join("lo"),
                    },
                },
            ],
        };
        let flows = enumerate_flows_for_cascade(&seeds);
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

    // Cwd-rebase regression coverage (#293) lives in
    // `crate::context::tests::discover_seeds_rebases_relative_default_against_cwd`
    // and the rebase tests next to it; the MCP wrapper above is a
    // straight pass-through with error-type conversion.

    #[test]
    fn do_list_agents_empty_with_relative_seed_in_project_config() {
        // The user's `.kuro/config.yaml` writes a relative seed path
        // (`path: .kuro`). The discovery layer must resolve it against the
        // config's parent (`cwd`), not the process CWD.
        //
        // Note: `Seeds::from_raw_entries` performs an `exists()` existence
        // check on the literal relative path, which resolves against the
        // process CWD at validation time. During `cargo test` that CWD is
        // the kuromaku repo root and `.kuro/` exists there, so validation
        // passes. (That existence check is a separate latent issue, out of
        // scope for #293.) The seed's `path` after validation is still
        // relative (`PathBuf::from(".kuro")`), and `discover_seeds` rebases
        // it -- which is the behavior under test.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            "version: \"1\"\nseeds:\n  - path: .kuro\n",
        );
        // Tempdir has a `.kuro/` (because we wrote config.yaml into it) but
        // no `.kuro/agents/`. After the rebase, enumeration walks
        // `tmp/.kuro/agents` which does not exist -- expect zero agents.
        let v = do_list_agents(cwd).unwrap();
        assert_eq!(
            v["agents"].as_array().unwrap().len(),
            0,
            "relative seed in project config must rebase against cwd, not process CWD; got {v}"
        );
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
        // Opt in to the cwd Guide so the byte-equality assertion below still
        // holds; the default-off path is covered by
        // `do_load_agent_default_omits_cwd_guide` (#245 regression).
        let v = do_load_agent(cwd, "Neo", true).unwrap();

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
            description: None,
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
        let err = do_load_agent(cwd, "Nonexistent", false).unwrap_err();
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
        let err = do_load_agent(cwd, "Neo", false).unwrap_err();
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
        let v = do_load_agent(cwd, "Mini", true).unwrap();
        assert_eq!(v["rules"].as_array().unwrap().len(), 0);
        assert_eq!(v["skills"].as_array().unwrap().len(), 0);
        assert!(v["guide"].is_null());
    }

    #[test]
    fn do_load_agent_default_omits_cwd_guide() {
        // Regression for #245: even when the cwd has a `Guide.md`, the MCP
        // `load_agent` tool must omit it unless the caller explicitly opts
        // in via `include_guide: true`. Mirrors `kuro task` /
        // `load_guide_for_task_skips_guide_by_default` so the same
        // "agents stay repo-agnostic by default" invariant holds across CLI
        // and MCP invocation paths.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            &cwd.join(".kuro/config.yaml"),
            &project_config_yaml(&cwd.join(".kuro")),
        );
        write(
            &cwd.join(".kuro/Guide.md"),
            "You are working on **kuromaku** -- a CLI tool for reproducible AI agents.",
        );
        write(
            &cwd.join(".kuro/agents/Neo.yaml"),
            "name: Neo\nrole: |\n  ROLE-CONTENT\n",
        );

        // Default (false) -- Guide must NOT appear in the response.
        let v = do_load_agent(cwd, "Neo", false).unwrap();
        assert!(
            v["guide"].is_null(),
            "MCP load_agent default must not return cwd Guide (#245); got: {v}"
        );

        // Opt-in (true) -- Guide is loaded as before. Symmetric coverage so
        // a future regression that breaks the opt-in path surfaces here next
        // to the regression test.
        let v = do_load_agent(cwd, "Neo", true).unwrap();
        assert!(
            v["guide"].as_str().is_some_and(|s| s.contains("kuromaku")),
            "include_guide:true must inject the cwd Guide; got: {v}"
        );
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
