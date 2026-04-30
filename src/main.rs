use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;

/// Matches `{{vars.<key>}}` placeholders. Compiled once at first use to avoid
/// re-parsing the pattern on every call to [`substitute_vars`].
static VARS_RE: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r"\{\{vars\.([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap());

/// Matches bare `{{key}}` placeholders. Compiled once at first use.
static PLACEHOLDER_RE: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r"\{\{([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap());

mod config;
mod dag;
#[allow(dead_code)]
mod executor;
mod koto_config;
#[allow(dead_code)]
mod llm;
#[allow(dead_code)]
mod messaging;
mod notify;
mod resolver;
mod runner;
mod skills;
mod stack;
#[allow(dead_code)]
mod ui;

use crate::koto_config::{KOTO_CONFIG_FILE, KotoConfig, Seeds};
use crate::resolver::{
    ResolvedRole, RoleOverride, format_audit, parse_role_override, print_audit, resolve_role,
    validate_role_overrides,
};
use crate::runner::{RunContext, StepRunResult};
use crate::stack::{Manifest, ResourceRecord, RoleResolution, SeedRecord};

const KOTO_DIR: &str = ".kuro";

#[derive(Parser)]
#[command(name = "kuro", about = "Reproducible AI agent teams", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Arguments shared by `run` and the deprecated `up` alias.
///
/// Kept in a dedicated [`clap::Args`] struct so the canonical verb and the
/// deprecation alias cannot drift -- adding a flag in one place automatically
/// applies to the other.
#[derive(clap::Args)]
struct RunArgs {
    /// Flow name (looks in .kuro/flows/<name>.yaml)
    flow: Option<String>,

    /// Task prompt or template arguments (key=value pairs fill {{key}} placeholders in the flow prompt)
    #[arg(short = 't', long)]
    task: Option<String>,

    /// Override project-config vars (repeatable, e.g. --var owner=foo --var repo=bar)
    ///
    /// Values fill `{{vars.<key>}}` placeholders in flow prompts and step
    /// task strings. Overrides any value defined in the project config.
    #[arg(long = "var", value_name = "KEY=VALUE")]
    vars: Vec<String>,

    /// Override role bindings (repeatable). Two forms:
    ///   --role developer=Kai             (rebind agent)
    ///   --role reviewer:model=ollama/x   (override model)
    ///   --role reviewer:backend=api      (override backend)
    #[arg(long = "role", value_name = "NAME[:FIELD]=VALUE")]
    role_overrides: Vec<String>,

    /// Template arguments as key=value pairs (e.g. pr=67 branch=main)
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,

    /// Path to the flow config file (overrides flow name lookup)
    #[arg(short, long)]
    file: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a flow
    Run(RunArgs),
    /// Deprecated alias for `run` -- emits a warning and dispatches to the same handler.
    /// Will be removed in a future release.
    #[command(hide = true)]
    Up(RunArgs),
    /// Run an ad-hoc task with one or more agents (no flow needed)
    Task {
        /// Agent name(s) from .kuro/agents/ (repeatable, executed in order)
        #[arg(short, long, required = true)]
        agent: Vec<String>,

        /// Task prompt
        #[arg(short = 't', long, required = true)]
        task: String,
    },
    /// Fetch skills from remote sources pinned in .kuro/skills.lock
    Pull,
    /// Stop the agent team
    #[command(hide = true)]
    Down,
    /// Show running agents and stack
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run_flow(&args).await?,
        Command::Up(args) => {
            eprintln!(
                "warning: `kuro up` is deprecated and will be removed in a future release; use `kuro run` instead"
            );
            run_flow(&args).await?
        }
        Command::Task { agent, task } => run_task(&agent, &task).await?,
        Command::Pull => run_pull()?,
        Command::Down => {
            println!("kuro down: not yet implemented");
        }
        Command::Status => {
            println!("kuro status: not yet implemented");
        }
    }

    Ok(())
}

/// Resolve the flow config file path from CLI arguments, walking the
/// configured seeds for the lookup. With multiple seeds the auto-select case
/// aggregates flow names across all local seeds (deduplicated by name; first
/// seed wins so an upstream override beats a downstream copy).
fn resolve_flow_path(flow: Option<&str>, file: Option<&str>, seeds: &Seeds) -> Result<PathBuf> {
    // --file takes precedence -- explicit user choice, never re-resolved.
    if let Some(f) = file {
        let path = PathBuf::from(f);
        if !path.exists() {
            return Err(eyre!("config file '{}' not found", f));
        }
        return Ok(path);
    }

    // If a flow name is given, look it up across all seeds. First match wins.
    if let Some(name) = flow {
        let rel = std::path::Path::new("flows").join(format!("{name}.yaml"));
        match seeds.find(&rel).map_err(|e| eyre!("{}", e.message()))? {
            Some((_, path)) => return Ok(path),
            None => {
                return Err(eyre!(
                    "{}\n\nhint: create flows/{name}.yaml in one of the seeds, or use --file <path>",
                    seeds.not_found_message("flow", name)
                ));
            }
        }
    }

    // No args: aggregate flow names from every local seed and auto-select if
    // exactly one is found. Multi-seed setups still get reproducible behavior
    // because we deduplicate on name and remember which seed contributed the
    // first occurrence -- that's the path we return.
    let mut by_name: std::collections::BTreeMap<String, PathBuf> =
        std::collections::BTreeMap::new();
    for seed in &seeds.seeds {
        let Some(seed_path) = seed.local_path() else {
            continue;
        };
        let flows_dir = seed_path.join("flows");
        let Ok(entries) = std::fs::read_dir(&flows_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            if !(name_str.ends_with(".yaml") || name_str.ends_with(".yml")) {
                continue;
            }
            let bare = name_str
                .trim_end_matches(".yaml")
                .trim_end_matches(".yml")
                .to_string();
            // First seed wins -- skip if a flow with the same name was already
            // recorded from an earlier seed.
            by_name
                .entry(bare)
                .or_insert_with(|| flows_dir.join(name_str.as_ref()));
        }
    }

    if by_name.is_empty() {
        return Err(eyre!(
            "no flows found in seeds: {}\n\nhint: create flows/<name>.yaml in one of the seed directories",
            seeds.audit_line()
        ));
    }

    if by_name.len() == 1 {
        let (_name, path) = by_name.into_iter().next().expect("len == 1");
        return Ok(path);
    }

    let list = by_name
        .keys()
        .map(|f| format!("  - {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(eyre!(
        "multiple flows found, specify one:\n\n{list}\n\nusage: kuro run <flow-name> -t \"task\""
    ))
}

/// Resolve the stack path: explicit config > default (`~/.koto/stacks/<project>/`).
///
/// The default home directory still uses `.koto/stacks/` even after the
/// project-config rename to `.kuro/` (#176). Renaming the home stack root
/// without a migration path would silently orphan every existing user's run
/// history, so it stays as a compatibility path until a dedicated migration
/// issue lands.
fn resolve_stack_path(config_path: &str) -> PathBuf {
    if !config_path.is_empty() {
        return PathBuf::from(config_path);
    }

    let project = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "default".to_string());

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".koto")
        .join("stacks")
        .join(project)
}

/// Resolve the task prompt from CLI flag, flow default, and template vars.
///
/// Priority: `-t "prompt"` overrides flow default. If the flow has a `prompt:`
/// field with `{{key}}` placeholders, template vars fill them.
fn resolve_task(
    task_flag: Option<&str>,
    flow_prompt: &Option<String>,
    template_vars: &std::collections::HashMap<String, String>,
) -> Result<String> {
    // -t flag takes precedence
    if let Some(task) = task_flag {
        return Ok(task.to_string());
    }

    // Flow has a default prompt with placeholders
    if let Some(prompt) = flow_prompt {
        let resolved = substitute_placeholders(prompt, template_vars)?;
        return Ok(resolved);
    }

    Err(eyre!(
        "no task specified\n\nhint: use -t \"task\" or define a prompt in the flow YAML"
    ))
}

/// Parse `key=value` pairs from trailing CLI args.
fn parse_key_value_args(args: &[String]) -> Result<std::collections::HashMap<String, String>> {
    let mut map = std::collections::HashMap::new();
    for arg in args {
        let (key, value) = arg.split_once('=').ok_or_else(|| {
            eyre!(
                "invalid argument '{arg}': expected key=value format\n\nhint: e.g. pr=67 branch=main"
            )
        })?;
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

/// Replace `{{vars.<key>}}` placeholders. Used for project-level vars from
/// the project config + `--var` CLI flag. Returns an error listing every
/// missing key.
///
/// Bare `{{key}}` placeholders (CLI key=value args) are NOT touched here --
/// see [`substitute_placeholders`].
fn substitute_vars(text: &str, vars: &std::collections::HashMap<String, String>) -> Result<String> {
    let mut missing: Vec<String> = Vec::new();

    // Single pass: build the replaced string in one allocation while
    // collecting any unknown keys for the error path.
    let result = VARS_RE.replace_all(text, |caps: &regex_lite::Captures<'_>| {
        let key = &caps[1];
        match vars.get(key) {
            Some(value) => value.clone(),
            None => {
                if !missing.iter().any(|k| k == key) {
                    missing.push(key.to_string());
                }
                // Leave the placeholder intact so the error message points
                // at the original text the caller passed in.
                caps[0].to_string()
            }
        }
    });

    if !missing.is_empty() {
        return Err(eyre!(
            "missing vars: {}\n\nhint: define them in {KOTO_CONFIG_FILE} or pass --var key=value",
            missing.join(", ")
        ));
    }

    Ok(result.into_owned())
}

/// Replace `{{key}}` placeholders in a prompt with values from the map.
fn substitute_placeholders(
    prompt: &str,
    vars: &std::collections::HashMap<String, String>,
) -> Result<String> {
    let mut result = prompt.to_string();
    let mut missing: Vec<String> = Vec::new();

    for cap in PLACEHOLDER_RE.captures_iter(prompt) {
        let key = &cap[1];
        match vars.get(key) {
            Some(value) => {
                result = result.replace(&format!("{{{{{key}}}}}"), value);
            }
            None => {
                if !missing.contains(&key.to_string()) {
                    missing.push(key.to_string());
                }
            }
        }
    }

    if !missing.is_empty() {
        return Err(eyre!(
            "missing template arguments: {}\n\nhint: pass them as key=value, e.g. {}",
            missing.join(", "),
            missing
                .iter()
                .map(|k| format!("{k}=<value>"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    Ok(result)
}

async fn run_task(agent_names: &[String], task: &str) -> Result<()> {
    let task_start = Instant::now();
    let koto_dir = Path::new(KOTO_DIR);

    // Optional project-level config -- needed if any agent declares a tier
    // and to source the seeds list. Without the project config we fall back
    // to the implicit `.kuro/` seed.
    let koto_config = KotoConfig::load_optional(Path::new("."))?;
    let seeds = koto_config
        .as_ref()
        .map(|c| c.seeds.clone())
        .unwrap_or_else(Seeds::default_local);

    // Use defaults matching flow config defaults
    let defaults = config::Defaults {
        model: "claude-sonnet-4-5".to_string(),
        backend: config::Backend::ClaudeCli,
    };

    // Load requested agents through the seed list. Arbitrary-depth IDs
    // (`coding/rust/Sage`) work the same way as in flow runs.
    let mut agents = Vec::new();
    for name in agent_names {
        let (agent, _seed_idx, _sha) =
            config::load_agent_file_with_seeds(&seeds, name, &defaults, koto_config.as_ref())?;
        agents.push(agent);
    }

    // Build synthetic steps: sequential chain, each sees previous output
    let mut steps: Vec<config::Step> = Vec::new();
    for (i, agent) in agents.iter().enumerate() {
        let input = if i == 0 {
            vec![]
        } else {
            vec![format!("step-{}", i)]
        };
        steps.push(config::Step {
            id: format!("step-{}", i + 1),
            agent: agent.id.clone(),
            role: None,
            task: None,
            run: None,
            input,
            needs: vec![],
            model: None,
            backend: None,
            print_output: i == agents.len() - 1, // last step prints
            post_comment: None,
            agents: Vec::new(),
            max_turns: None,
            turn_timeout: None,
        });
    }

    let step_refs: Vec<&config::Step> = steps.iter().collect();

    let task_name = if agent_names.len() == 1 {
        format!("task-{}", agent_names[0].to_lowercase())
    } else {
        "task".to_string()
    };

    ui::print_command(&format!(
        "kuro task --agent {} -t \"...\"",
        agent_names.join(" --agent ")
    ));

    ui::print_flow_start(&task_name, "ad-hoc", steps.len(), agents.len());

    // Resolve backends
    let mut seen_backends = std::collections::HashSet::new();
    let mut backend_list: Vec<(&str, &str)> = Vec::new();
    for agent in &agents {
        let name = match agent.backend {
            config::Backend::Api => "api",
            config::Backend::ClaudeCli => "claude-cli",
            config::Backend::Codex => "codex",
            config::Backend::Ollama => "ollama",
        };
        if seen_backends.insert(name) {
            backend_list.push((name, ""));
        }
    }
    ui::print_backends_ok(&backend_list);

    // Load context through the seed list -- guide is optional, rules error
    // with the seeds searched if a referenced rule is missing.
    let guide = runner::load_guide_from_seeds(&seeds).map_err(|e| eyre!("{e}"))?;
    let rules_cache =
        runner::load_rules_for_agents_with_seeds(&agents, &seeds).map_err(|e| eyre!("{e}"))?;
    // koto_dir kept around for the skills directory below, which is not yet
    // seed-aware.
    let _ = koto_dir;

    // Skills
    let skills_dir = koto_dir.join("skills");
    let skill_names = skills::collect_skill_names(&agents);
    let skills_cache = if skill_names.is_empty() {
        std::collections::HashMap::new()
    } else {
        let missing = skills::check_skills_available(&skill_names, &skills_dir);
        if !missing.is_empty() {
            return Err(eyre!(
                "missing skills: {}\n\nhint: run `kuro pull` to fetch skills",
                missing.join(", ")
            ));
        }
        skills::load_skills_for_agents(&skill_names, &skills_dir)?
    };

    let stack_path = resolve_stack_path("");

    let ctx = runner::RunContext::new(
        task_name.clone(),
        task.to_string(),
        stack_path.clone(),
        guide,
        rules_cache,
        skills_cache,
        std::collections::HashMap::new(),
    );

    let results = runner::run_steps(&step_refs, &agents, &ctx).await?;

    // Summary
    let total_elapsed = task_start.elapsed();
    let summary = runner::build_summary(&results);
    let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
    let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();

    ui::print_flow_complete(
        &summary,
        &format_elapsed(total_elapsed),
        &total_in.to_string(),
        &total_out.to_string(),
        "—",
        &ctx.stack_path.display().to_string(),
    );

    // Print output of last step
    for result in &results {
        if result.print_output {
            let output_path = ctx.stack_path.join(&result.output_file);
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                println!();
                termimad::print_text(&content);
            }
        }
    }

    Ok(())
}

async fn run_flow(run_args: &RunArgs) -> Result<()> {
    let flow = run_args.flow.as_deref();
    let task = run_args.task.as_deref();
    let var_args: &[String] = &run_args.vars;
    let role_override_args: &[String] = &run_args.role_overrides;
    let args: &[String] = &run_args.args;
    let file = run_args.file.as_deref();

    let flow_start = Instant::now();

    // Load the project config first -- the seeds list it produces feeds
    // every later lookup (flow file, agents, rules, Guide).
    let koto_config = KotoConfig::load_optional(Path::new("."))?;
    let seeds = koto_config
        .as_ref()
        .map(|c| c.seeds.clone())
        .unwrap_or_else(Seeds::default_local);

    let path = resolve_flow_path(flow, file, &seeds)?;
    let display_path = path.display().to_string();

    ui::print_command(&format!("kuro run {}", flow.unwrap_or(&display_path)));

    // Parse `--role` flags up front. Errors here surface bad syntax (unknown
    // field, bad model format, ...) before we touch any YAML.
    let parsed_role_overrides: Vec<RoleOverride> = role_override_args
        .iter()
        .map(|s| parse_role_override(s))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| eyre!("{e}"))?;

    // Build effective vars: project config < CLI --var (CLI wins).
    let cli_vars_map = parse_key_value_args(var_args)?;
    let mut effective_vars = koto_config
        .as_ref()
        .map(|c| c.vars.clone())
        .unwrap_or_default();
    for (k, v) in &cli_vars_map {
        effective_vars.insert(k.clone(), v.clone());
    }

    // Parse role names from YAML to partition args
    let contents = std::fs::read_to_string(&path)?;
    let role_names = config::parse_role_names(&contents)?;

    // Partition positional args: role overrides vs template vars
    let all_args = parse_key_value_args(args)?;
    let (legacy_role_overrides, template_vars): (
        std::collections::HashMap<_, _>,
        std::collections::HashMap<_, _>,
    ) = all_args
        .into_iter()
        .partition(|(k, _)| role_names.contains(k));

    // Deprecation warning for legacy bare key=value template args. They
    // continue to work for now (one cycle) but encourage migration to --var.
    // Also copy them into effective_vars so new-style {{vars.<key>}}
    // placeholders work without requiring an explicit --var flag.
    if !template_vars.is_empty() {
        let mut keys: Vec<&String> = template_vars.keys().collect();
        keys.sort();
        eprintln!(
            "warning: bare key=value args are deprecated, use --var: {}",
            keys.iter()
                .map(|k| format!("--var {k}={}", template_vars[*k]))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for (k, v) in &template_vars {
            effective_vars.entry(k.clone()).or_insert_with(|| v.clone());
        }

        // Warn about template vars that aren't placeholders either
        let flow_config_temp = config::load_flow_from_str(&contents)?;
        let placeholders = flow_config_temp
            .prompt
            .as_ref()
            .map(|p| config::extract_placeholders(p))
            .unwrap_or_default();

        for key in template_vars.keys() {
            if !placeholders.contains(key) {
                eprintln!(
                    "warning: '{}' is not a declared role or template placeholder",
                    key
                );
            }
        }
    }

    // Deprecation warning for legacy bare key=value role rebinds. Same one-cycle
    // grace period as template vars: still applied below via
    // load_flow_from_str_with_overrides, but users should migrate to --role.
    if !legacy_role_overrides.is_empty() {
        let mut keys: Vec<&String> = legacy_role_overrides.keys().collect();
        keys.sort();
        eprintln!(
            "warning: bare key=value role rebinds are deprecated, use --role: {}",
            keys.iter()
                .map(|k| format!("--role {k}={}", legacy_role_overrides[*k]))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    // Load flow with legacy positional role overrides AND project-level role
    // bindings from the project config. Project roles act as inherited
    // defaults so a flow can reference a role declared only in the project
    // config without redeclaring
    // it locally.
    let project_roles: std::collections::HashMap<String, String> = koto_config
        .as_ref()
        .map(|c| {
            c.roles
                .iter()
                .map(|(k, v)| (k.clone(), v.agent.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut flow_config =
        config::load_flow_from_str_with_project(&contents, &legacy_role_overrides, &project_roles)?;

    // Substitute `{{vars.<key>}}` in the flow prompt, step task strings, and
    // the `-t` override before any further use. Bare `{{key}}` placeholders
    // are still handled downstream by `resolve_task` for CLI key=value args.
    //
    // Shell steps (`run:`, issue #23) get BOTH `{{vars.<key>}}` and bare
    // `{{key}}` substitution applied to the command up front. The example in
    // the issue uses bare placeholders (`gh pr diff {{pr}}`), so the bare
    // form must work without forcing users to write `{{vars.pr}}` in
    // shell-step commands.
    if let Some(ref mut prompt) = flow_config.prompt {
        *prompt = substitute_vars(prompt, &effective_vars)?;
    }
    for step in flow_config.steps.iter_mut() {
        if let Some(task_str) = step.task.as_mut() {
            *task_str = substitute_vars(task_str, &effective_vars)?;
        }
        if let Some(run_str) = step.run.as_mut() {
            *run_str = substitute_vars(run_str, &effective_vars)?;
            *run_str = substitute_placeholders(run_str, &effective_vars)?;
        }
    }
    let task_with_vars = task
        .map(|t| substitute_vars(t, &effective_vars))
        .transpose()?;

    // Resolve task prompt: -t flag > flow default prompt with {{key}} substitution
    let resolved_task = resolve_task(
        task_with_vars.as_deref(),
        &flow_config.prompt,
        &template_vars,
    )?;

    // Derive flow name for tmux sessions
    let flow_name = flow
        .map(|s| s.to_string())
        .unwrap_or_else(|| flow_config.name.clone());

    // Load agents referenced by the flow
    let koto_dir = Path::new(KOTO_DIR);

    // Validate that all CLI --role overrides target known roles. This must
    // happen before agent loading so the user gets a clear error early.
    validate_role_overrides(&parsed_role_overrides, &flow_config, koto_config.as_ref())
        .map_err(|e| eyre!("{e}"))?;

    // Apply the agent-cascade decision before agent loading so the right
    // agent files get loaded. The decision itself comes from
    // `resolver::resolve_role_agent` -- this call only writes the result back
    // into the flow's role map and step.agent. The full cascade (with model
    // and backend) runs below as `build_resolved_roles` and reads from the
    // already-applied flow values, so the rule lives in one place.
    apply_role_agent_overrides(
        &mut flow_config,
        koto_config.as_ref(),
        &parsed_role_overrides,
    );

    // Load agents through seeds. The origins map records which seed each
    // agent came from (used by the audit). `agent_hashes` captures the
    // SHA-256 of the bytes that were actually read from disk so the manifest
    // can record what the LLM saw, not what's on disk at manifest-write time.
    let (agents, agent_origins, agent_hashes) =
        config::load_agents_for_flow_with_seeds(&seeds, &flow_config, koto_config.as_ref())?;

    // Build the per-step role -> resolved binding map for the cascade.
    let mut resolved_roles = build_resolved_roles(
        &flow_config,
        &agents,
        koto_config.as_ref(),
        &parsed_role_overrides,
    )
    .map_err(|e| eyre!("{e}"))?;

    // Annotate each ResolvedRole with the seed display string of its agent so
    // print_audit can show "<- <seed>" lines per role. Steps that bind directly
    // (no role) are still tracked in agent_origins -- they just don't get an
    // audit block here because the audit walks roles only.
    for r in resolved_roles.iter_mut() {
        if let Some(idx) = agent_origins.get(&r.agent)
            && let Some(seed) = seeds.seeds.get(*idx)
        {
            r.seed_origin = Some(seed.display());
        }
    }

    // Audit output: show the seed list, the resolved binding for every role
    // and a CLI vars summary. Print before any execution starts so the user
    // can ctrl-C if a resolution looks wrong. The same text is captured below
    // for the run directory's `resolution-audit.txt` (issue #31), so the
    // on-disk audit matches what the user saw in the terminal.
    let audit_text = format_audit(&seeds, &resolved_roles, &cli_vars_map);
    print_audit(&seeds, &resolved_roles, &cli_vars_map);

    // Apply role-derived model and backend overrides to every step that uses
    // a role. Steps that already specified `model:` / `backend:` in the flow
    // YAML keep their explicit choice -- the cascade only fills the gaps.
    apply_resolved_roles_to_steps(&mut flow_config, &resolved_roles);

    ui::print_flow_start(
        &flow_config.name,
        &display_path,
        flow_config.steps.len(),
        agents.len(),
    );

    // Validate DAG and get execution order
    let steps = dag::validate_dag(&flow_config)?;

    // Resolve and print backends
    let mut seen_backends = std::collections::HashSet::new();
    let mut backend_list: Vec<(&str, &str)> = Vec::new();
    for agent in &agents {
        let name = match agent.backend {
            config::Backend::Api => "api",
            config::Backend::ClaudeCli => "claude-cli",
            config::Backend::Codex => "codex",
            config::Backend::Ollama => "ollama",
        };
        if seen_backends.insert(name) {
            backend_list.push((name, ""));
        }
    }
    ui::print_backends_ok(&backend_list);

    // Load guide and rules context through the seed list. Guide is optional;
    // rules error out with the searched seed list if a referenced rule is
    // missing.
    let guide = runner::load_guide_from_seeds(&seeds).map_err(|e| eyre!("{e}"))?;
    let rules_cache =
        runner::load_rules_for_agents_with_seeds(&agents, &seeds).map_err(|e| eyre!("{e}"))?;

    // Check and load skills
    let skills_dir = koto_dir.join("skills");
    let skill_names = skills::collect_skill_names(&agents);
    let skills_cache = if skill_names.is_empty() {
        std::collections::HashMap::new()
    } else {
        let missing = skills::check_skills_available(&skill_names, &skills_dir);
        if !missing.is_empty() {
            return Err(eyre!(
                "missing skills: {}\n\nhint: run `kuro pull` to fetch skills",
                missing.join(", ")
            ));
        }
        skills::load_skills_for_agents(&skill_names, &skills_dir)?
    };

    // Resolve stack path
    let stack_path = resolve_stack_path(&flow_config.stack.path);

    // Construct RunContext. The runner reads `id` from `effective_vars` when
    // a step declares `post_comment:` -- no extra plumbing needed since the
    // map is already built up at this point.
    let ctx = RunContext::new(
        flow_name.clone(),
        resolved_task,
        stack_path.clone(),
        guide,
        rules_cache,
        skills_cache,
        effective_vars.clone(),
    );

    // Per-run directory layout (issue #31): create the run_path up front --
    // including its `steps/` and `messages/` subdirs (issue #159) -- and drop
    // the resolution audit there so the on-disk audit and the per-step files
    // share the same directory regardless of whether the run later succeeds.
    // If the run fails, the audit is still recoverable for post-mortem.
    stack::init_run_layout(&ctx.run_path).map_err(|e| eyre!("failed to create run dir: {e}"))?;
    if let Err(e) = stack::write_resolution_audit(&ctx.run_path, &audit_text) {
        eprintln!("warning: failed to write resolution-audit.txt: {e}");
    }

    // Run steps
    let results = runner::run_steps(&steps, &agents, &ctx).await?;

    // Manifest write happens only on success (issue #31). On failure the
    // per-step files and the resolution audit are still on disk; the absence
    // of `manifest.yaml` is the signal that the run did not complete.
    let manifest = build_manifest(
        &ctx,
        &flow_name,
        &path,
        &contents,
        &seeds,
        &agents,
        &agent_origins,
        &agent_hashes,
        &resolved_roles,
        &effective_vars,
        &results,
        flow_start.elapsed(),
    );
    // Manifest write must propagate. The doc comment on this run path
    // promises that the absence of `manifest.yaml` signals an incomplete
    // run; swallowing a disk-full or permission failure with `eprintln!`
    // would silently violate that invariant on a successful exit code.
    // Fail the run instead so the absence is genuine.
    stack::write_manifest(&ctx.run_path, &manifest)
        .map_err(|e| eyre!("failed to write manifest.yaml: {e}"))?;

    // Print summary
    let total_elapsed = flow_start.elapsed();
    let summary = runner::build_summary(&results);
    let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
    let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();

    ui::print_flow_complete(
        &summary,
        &format_elapsed(total_elapsed),
        &total_in.to_string(),
        &total_out.to_string(),
        "—",
        &ctx.stack_path.display().to_string(),
    );

    // Print output of steps marked with print_output: true
    for result in &results {
        if result.print_output {
            let output_path = ctx.stack_path.join(&result.output_file);
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                println!();
                termimad::print_text(&content);
            }
        }
    }

    Ok(())
}

/// Build the run manifest (issue #31). Pure function so it can be tested in
/// isolation -- no I/O is performed here; the caller writes the result to
/// `<run_path>/manifest.yaml`.
///
/// Resource hashes are best-effort: a missing file (broken symlink, removed
/// after load) becomes an empty SHA. The manifest stays internally consistent
/// because every entry whose hash failed will compare unequal across runs --
/// callers that demand strict reproducibility can flag empty hashes.
#[allow(clippy::too_many_arguments)]
fn build_manifest(
    ctx: &RunContext,
    flow_name: &str,
    flow_path: &Path,
    flow_contents: &str,
    seeds: &Seeds,
    agents: &[config::Agent],
    agent_origins: &std::collections::HashMap<String, usize>,
    agent_hashes: &std::collections::HashMap<String, String>,
    roles: &[ResolvedRole],
    vars: &std::collections::HashMap<String, String>,
    results: &[StepRunResult],
    total_elapsed: std::time::Duration,
) -> Manifest {
    let finished_at = chrono::Utc::now();

    // Seed records: track display, tilde-expanded path (local seeds only), and
    // a `dirty` flag. Git SHA pinning is left for #152's full audit work and
    // intentionally None here so the field is stable for forward compat.
    let seed_records: Vec<SeedRecord> = seeds
        .seeds
        .iter()
        .map(|s| SeedRecord {
            display: s.display(),
            path: s.local_path().map(|p| p.display().to_string()),
            git_sha: None,
            dirty: false,
        })
        .collect();

    // Resources: flow + agents + rules + guide. Each entry pins the file
    // path and its SHA-256, so two runs that produced the same output can be
    // compared by hash alone.
    let mut resources: Vec<ResourceRecord> = Vec::new();
    resources.push(ResourceRecord {
        kind: "flow".to_string(),
        name: flow_name.to_string(),
        path: flow_path.display().to_string(),
        sha256: stack::sha256_hex(flow_contents.as_bytes()),
    });
    for agent in agents {
        // Re-find the agent's source file via seeds. This re-walks the seed
        // list but is cheap (a couple of stat()s) and keeps `build_manifest`
        // free of the agent loader's internal state. Nested ids
        // (`coding/rust/Sage`) are preserved verbatim.
        let rel = std::path::Path::new("agents").join(format!("{}.yaml", agent.id));
        let path_str = match seeds.find(&rel) {
            Ok(Some((_, p))) => p.display().to_string(),
            _ => agent_origins
                .get(&agent.id)
                .and_then(|idx| seeds.seeds.get(*idx))
                .map(|s| s.display())
                .unwrap_or_default(),
        };
        // Use the SHA captured when the agent file was loaded -- not a fresh
        // read of disk now. If the user edited the agent file mid-run, the
        // bytes the LLM saw are gone from disk; the manifest must record the
        // hash of what was actually used, otherwise the "same hash = same
        // run" audit promise breaks. Empty fallback only triggers when the
        // hash map is somehow missing the entry (shouldn't happen for any
        // agent in `agents`, since the loader populates both in lockstep).
        let sha = agent_hashes.get(&agent.id).cloned().unwrap_or_default();
        resources.push(ResourceRecord {
            kind: "agent".to_string(),
            name: agent.id.clone(),
            path: path_str,
            sha256: sha,
        });
    }
    // Rules: iterate `ctx.rules_cache` so we hash exactly what was injected
    // into the prompt, not whatever happens to be on disk now.
    let mut rule_names: Vec<&String> = ctx.rules_cache.keys().collect();
    rule_names.sort();
    for name in rule_names {
        let rel = std::path::Path::new("rules").join(format!("{name}.md"));
        let path_str = match seeds.find(&rel) {
            Ok(Some((_, p))) => p.display().to_string(),
            _ => String::new(),
        };
        let content = ctx.rules_cache.get(name).map(String::as_str).unwrap_or("");
        resources.push(ResourceRecord {
            kind: "rules".to_string(),
            name: name.clone(),
            path: path_str,
            sha256: stack::sha256_hex(content.as_bytes()),
        });
    }
    // Skills: hash exactly what was injected into the prompt. Two runs that
    // differ only in skill content would otherwise produce identical hashes
    // here -- breaking the "same hash = same inputs" audit promise. Skills
    // live at `.kuro/skills/<name>/` (project-local, not in seeds), so the
    // recorded path stays relative and machine-portable.
    let mut skill_names: Vec<&String> = ctx.skills_cache.keys().collect();
    skill_names.sort();
    for name in skill_names {
        let content = ctx.skills_cache.get(name).map(String::as_str).unwrap_or("");
        resources.push(ResourceRecord {
            kind: "skill".to_string(),
            name: name.clone(),
            path: format!("{KOTO_DIR}/skills/{name}"),
            sha256: stack::sha256_hex(content.as_bytes()),
        });
    }
    if let Some(guide) = ctx.guide.as_deref() {
        let path_str = seeds
            .find(std::path::Path::new("Guide.md"))
            .ok()
            .flatten()
            .map(|(_, p)| p.display().to_string())
            .unwrap_or_default();
        resources.push(ResourceRecord {
            kind: "guide".to_string(),
            name: "Guide".to_string(),
            path: path_str,
            sha256: stack::sha256_hex(guide.as_bytes()),
        });
    }

    let role_records: Vec<RoleResolution> = roles
        .iter()
        .map(|r| RoleResolution {
            role: r.name.clone(),
            agent: r.agent.clone(),
            model: r.model.clone(),
            backend: backend_label_static(r.backend).to_string(),
            model_source: r.model_source.clone(),
            backend_source: r.backend_source.clone(),
            seed_origin: r.seed_origin.clone(),
        })
        .collect();

    // Vars: sort by key for stable manifest output. IndexMap preserves
    // insertion order so YAML diffs across runs stay readable.
    let mut keys: Vec<&String> = vars.keys().collect();
    keys.sort();
    let var_map: indexmap::IndexMap<String, String> = keys
        .into_iter()
        .map(|k| (k.clone(), vars[k].clone()))
        .collect();

    let total_in: u32 = results.iter().filter_map(|r| r.tokens_in).sum();
    let total_out: u32 = results.iter().filter_map(|r| r.tokens_out).sum();

    Manifest {
        version: 1,
        run_id: ctx.run_id.clone(),
        flow_name: flow_name.to_string(),
        flow_path: flow_path.display().to_string(),
        flow_sha256: stack::sha256_hex(flow_contents.as_bytes()),
        started_at: ctx.started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        duration_ms: total_elapsed.as_millis(),
        total_tokens_in: total_in,
        total_tokens_out: total_out,
        cost: None,
        vars: var_map,
        seeds: seed_records,
        resources,
        roles: role_records,
        steps: results.iter().map(|r| r.record.clone()).collect(),
    }
}

/// Static label for a backend variant. Mirrors `resolver::backend_label` but
/// kept here to avoid making that helper public for one caller.
fn backend_label_static(b: config::Backend) -> &'static str {
    match b {
        config::Backend::Api => "api",
        config::Backend::ClaudeCli => "claude-cli",
        config::Backend::Codex => "codex",
        config::Backend::Ollama => "ollama",
    }
}

/// Apply the resolver-decided agent for every role in the flow. This walks
/// the flow's role map and every role-bound step and writes back the agent ID
/// returned by [`resolver::resolve_role_agent`] -- the single source of truth
/// for the agent cascade (CLI > flow.role.default > project-config roles[X].agent).
///
/// Must run before `load_agents_for_flow` so the right agent files are loaded.
/// This function does NOT compute the cascade itself; it only applies the
/// resolver's decision, which keeps the rule in exactly one place.
fn apply_role_agent_overrides(
    flow_config: &mut config::FlowConfig,
    koto_config: Option<&KotoConfig>,
    overrides: &[RoleOverride],
) {
    let mut decided: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for role_name in flow_config.roles.keys() {
        let flow_agent = flow_config.roles.get(role_name).map(String::as_str);
        let project_role = koto_config.and_then(|c| c.roles.get(role_name));
        if let Some(agent) =
            resolver::resolve_role_agent(role_name, flow_agent, project_role, overrides)
        {
            decided.insert(role_name.clone(), agent);
        }
    }
    for (role_name, agent_id) in flow_config.roles.iter_mut() {
        if let Some(new_agent) = decided.get(role_name) {
            *agent_id = new_agent.clone();
        }
    }
    for step in flow_config.steps.iter_mut() {
        if let Some(role) = step.role.as_deref()
            && let Some(new_agent) = decided.get(role)
        {
            step.agent = new_agent.clone();
        }
    }
}

/// Build the cascade-resolved binding for every role used in the flow. Returns
/// one [`ResolvedRole`] per role -- both flow-declared roles and project-
/// config roles that the flow inherits.
fn build_resolved_roles(
    flow_config: &config::FlowConfig,
    agents: &[config::Agent],
    koto_config: Option<&KotoConfig>,
    cli_overrides: &[RoleOverride],
) -> std::result::Result<Vec<ResolvedRole>, resolver::ResolverError> {
    use std::collections::HashSet;
    let agents_by_id: std::collections::HashMap<&str, &config::Agent> =
        agents.iter().map(|a| (a.id.as_str(), a)).collect();

    // Roles to report on: union of flow-declared and project-config-declared roles
    // referenced by any step. We only include roles actually used to keep the
    // audit output focused.
    let mut used_roles: HashSet<&str> = HashSet::new();
    for step in &flow_config.steps {
        if let Some(role) = step.role.as_deref() {
            used_roles.insert(role);
        }
    }

    let mut out = Vec::new();
    for role_name in used_roles {
        let flow_agent = flow_config.roles.get(role_name).map(String::as_str);
        let project_role = koto_config.and_then(|c| c.roles.get(role_name));

        // The agent that load_agents_for_flow used. Use the agent currently
        // resolved on the role (after CLI rebind), falling back to project.
        let agent_for_lookup = flow_agent
            .or_else(|| project_role.map(|r| r.agent.as_str()))
            .unwrap_or("");
        let agent = agents_by_id.get(agent_for_lookup).copied();

        let agent_model = agent.map(|a| a.model.as_str()).unwrap_or("");
        let agent_backend = agent
            .map(|a| a.backend)
            .unwrap_or(config::Backend::ClaudeCli);

        // The agent's tier label is read straight from disk -- cheap, and
        // keeps the audit text honest about why a model was chosen.
        let agent_tier = agent.and_then(|a| read_agent_tier(a.id.as_str()));

        let input = resolver::RoleResolveInput {
            role_name,
            agent_model,
            agent_tier: agent_tier.as_deref(),
            agent_backend,
            flow_default_model: flow_config.defaults.model.as_str(),
        };

        let resolved = resolve_role(
            &input,
            flow_agent,
            project_role,
            cli_overrides,
            koto_config.and_then(|c| c.default_backend),
        )
        .ok_or_else(|| resolver::ResolverError::UnknownRole {
            name: role_name.to_string(),
        })?;
        out.push(resolved);
    }
    Ok(out)
}

/// Read the optional `tier:` field from an agent file. Returns `None` if the
/// file is missing or has no tier -- callers treat both as "no tier", which
/// matches how the rest of the loader behaves.
fn read_agent_tier(agent_id: &str) -> Option<String> {
    let path = Path::new(KOTO_DIR)
        .join("agents")
        .join(format!("{agent_id}.yaml"));
    let contents = std::fs::read_to_string(&path).ok()?;
    #[derive(serde::Deserialize)]
    struct TierOnly {
        tier: Option<String>,
    }
    let parsed: TierOnly = serde_yaml::from_str(&contents).ok()?;
    parsed.tier
}

/// Apply role-derived model and backend overrides to every step that uses a
/// role. Steps that already specified `model:` / `backend:` keep their
/// explicit per-step choice; the cascade only fills the gaps.
fn apply_resolved_roles_to_steps(flow_config: &mut config::FlowConfig, resolved: &[ResolvedRole]) {
    let by_role: std::collections::HashMap<&str, &ResolvedRole> =
        resolved.iter().map(|r| (r.name.as_str(), r)).collect();
    for step in flow_config.steps.iter_mut() {
        let Some(role_name) = step.role.as_deref() else {
            continue;
        };
        let Some(rr) = by_role.get(role_name) else {
            continue;
        };
        if step.model.is_none() {
            step.model = Some(rr.model.clone());
        }
        if step.backend.is_none() {
            step.backend = Some(rr.backend);
        }
    }
}

fn run_pull() -> Result<()> {
    let koto_dir = Path::new(KOTO_DIR);
    let lock_path = koto_dir.join("skills.lock");

    if !lock_path.exists() {
        return Err(eyre!(
            "no .kuro/skills.lock found\n\nhint: create .kuro/skills.lock with your skill sources"
        ));
    }

    let lock = skills::load_skills_lock(&lock_path)?;
    if lock.skills.is_empty() {
        println!("no skills defined in .kuro/skills.lock");
        return Ok(());
    }

    let skills_dir = koto_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    println!("pulling {} skill(s)...", lock.skills.len());
    skills::pull_skills(&lock, &skills_dir)?;
    println!("done");

    Ok(())
}

fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CLI parsing for `run` (canonical) and `up` (deprecated alias) (issue #181) ---

    use clap::Parser as _;

    /// Helper: extract the `RunArgs` from whichever of the two variants
    /// `Cli::try_parse_from` produced. Lets the assertions below stay focused
    /// on argument values without re-doing the variant match each time.
    fn parse_cli(argv: &[&str]) -> (Cli, &'static str) {
        let cli = Cli::try_parse_from(argv).expect("CLI parse failed");
        let kind = match &cli.command {
            Command::Run(_) => "run",
            Command::Up(_) => "up",
            _ => "other",
        };
        (cli, kind)
    }

    #[test]
    fn run_subcommand_parses_canonically() {
        // `kuro run <flow>` is the canonical verb -- it must parse to
        // Command::Run, not the deprecated Up alias.
        let (cli, kind) = parse_cli(&["kuro", "run", "review-pr"]);
        assert_eq!(kind, "run");
        let Command::Run(args) = cli.command else {
            unreachable!()
        };
        assert_eq!(args.flow.as_deref(), Some("review-pr"));
    }

    #[test]
    fn up_subcommand_still_parses_for_deprecation() {
        // `kuro up <flow>` continues to work but parses to the hidden Up
        // variant so the dispatcher can emit the deprecation warning. Removing
        // this without a release cycle would break user scripts.
        let (cli, kind) = parse_cli(&["kuro", "up", "review-pr"]);
        assert_eq!(kind, "up");
        let Command::Up(args) = cli.command else {
            unreachable!()
        };
        assert_eq!(args.flow.as_deref(), Some("review-pr"));
    }

    #[test]
    fn run_and_up_share_argument_layout() {
        // The two variants must accept identical flags -- they share a single
        // RunArgs struct, so adding a flag in one place automatically applies
        // to the other. Pin the contract here so a future refactor that
        // splits them gets caught.
        let argv_common = &[
            "review-pr",
            "-t",
            "do the thing",
            "--var",
            "owner=nestrai",
            "--role",
            "developer=Sage",
            "id=42",
        ];

        let (run_cli, _) = parse_cli(
            &[&["kuro", "run"][..], &argv_common[..]]
                .concat::<&str>()
                .as_slice(),
        );
        let (up_cli, _) = parse_cli(
            &[&["kuro", "up"][..], &argv_common[..]]
                .concat::<&str>()
                .as_slice(),
        );

        let Command::Run(run_args) = run_cli.command else {
            unreachable!()
        };
        let Command::Up(up_args) = up_cli.command else {
            unreachable!()
        };

        assert_eq!(run_args.flow, up_args.flow);
        assert_eq!(run_args.task, up_args.task);
        assert_eq!(run_args.vars, up_args.vars);
        assert_eq!(run_args.role_overrides, up_args.role_overrides);
        assert_eq!(run_args.args, up_args.args);
        assert_eq!(run_args.file, up_args.file);
    }

    #[test]
    fn parse_key_value_args_valid() {
        let args = vec!["pr=67".to_string(), "branch=main".to_string()];
        let map = parse_key_value_args(&args).unwrap();
        assert_eq!(map.get("pr").unwrap(), "67");
        assert_eq!(map.get("branch").unwrap(), "main");
    }

    #[test]
    fn parse_key_value_args_empty() {
        let map = parse_key_value_args(&[]).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_key_value_args_invalid() {
        let args = vec!["nope".to_string()];
        assert!(parse_key_value_args(&args).is_err());
    }

    #[test]
    fn substitute_single_placeholder() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("pr".to_string(), "67".to_string());
        let result = substitute_placeholders("Review PR #{{pr}}", &vars).unwrap();
        assert_eq!(result, "Review PR #67");
    }

    #[test]
    fn substitute_multiple_placeholders() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("pr".to_string(), "67".to_string());
        vars.insert("repo".to_string(), "ikno".to_string());
        let result = substitute_placeholders("Review PR #{{pr}} in {{repo}}", &vars).unwrap();
        assert_eq!(result, "Review PR #67 in ikno");
    }

    #[test]
    fn substitute_no_placeholders() {
        let vars = std::collections::HashMap::new();
        let result = substitute_placeholders("Just a plain task", &vars).unwrap();
        assert_eq!(result, "Just a plain task");
    }

    #[test]
    fn substitute_missing_placeholder_errors() {
        let vars = std::collections::HashMap::new();
        let err = substitute_placeholders("Review PR #{{pr}}", &vars).unwrap_err();
        assert!(err.to_string().contains("pr"));
    }

    // --- vars substitution (issue #128) ---

    #[test]
    fn substitute_vars_replaces_namespaced_placeholders() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("owner".to_string(), "nestrai".to_string());
        vars.insert("repo".to_string(), "koto".to_string());

        let result = substitute_vars("clone {{vars.owner}}/{{vars.repo}}", &vars).unwrap();
        assert_eq!(result, "clone nestrai/koto");
    }

    #[test]
    fn substitute_vars_leaves_bare_placeholders_alone() {
        // Bare `{{id}}` must not be touched by vars substitution -- it's the
        // CLI key=value namespace handled by substitute_placeholders.
        let mut vars = std::collections::HashMap::new();
        vars.insert("owner".to_string(), "nestrai".to_string());

        let result = substitute_vars("Issue #{{id}} in {{vars.owner}}/repo", &vars).unwrap();
        assert_eq!(result, "Issue #{{id}} in nestrai/repo");
    }

    #[test]
    fn substitute_vars_missing_key_errors() {
        let vars = std::collections::HashMap::new();
        let err = substitute_vars("repo: {{vars.repo}}", &vars).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing vars: repo"), "got: {msg}");
        assert!(msg.contains("--var"), "got: {msg}");
    }

    #[test]
    fn substitute_vars_no_placeholders_passes_through() {
        let vars = std::collections::HashMap::new();
        let result = substitute_vars("plain text", &vars).unwrap();
        assert_eq!(result, "plain text");
    }

    #[test]
    fn task_flag_runs_through_substitute_vars() {
        // Regression: `-t "deploy {{vars.env}}"` used to pass the placeholder
        // through unchanged. The fix in run_up applies substitute_vars to the
        // task flag before resolve_task -- mirror that behavior here so the
        // expectation is documented next to the helper it relies on.
        let mut vars = std::collections::HashMap::new();
        vars.insert("env".to_string(), "prod".to_string());

        let task_flag = "deploy {{vars.env}}";
        let substituted = substitute_vars(task_flag, &vars).unwrap();
        let resolved =
            resolve_task(Some(&substituted), &None, &std::collections::HashMap::new()).unwrap();

        assert_eq!(resolved, "deploy prod");
    }

    #[test]
    fn substitute_vars_repeated_placeholder_replaces_all() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("repo".to_string(), "koto".to_string());
        let result = substitute_vars("{{vars.repo}} and again {{vars.repo}}", &vars).unwrap();
        assert_eq!(result, "koto and again koto");
    }

    #[test]
    fn cli_vars_override_project_config_vars() {
        // Replicates the merge logic used in run_flow: project config vars
        // get shadowed by `--var` CLI flags. Renamed from the legacy
        // koto_yaml/koto_yaml_vars wording -- the file is `.kuro/config.yaml`
        // now and the test is generic over the project-config layer regardless.
        let mut project_config_vars = std::collections::HashMap::new();
        project_config_vars.insert("owner".to_string(), "from-yaml".to_string());
        project_config_vars.insert("repo".to_string(), "from-yaml".to_string());

        let cli_args = vec!["repo=from-cli".to_string()];
        let cli_vars = parse_key_value_args(&cli_args).unwrap();

        let mut effective = project_config_vars;
        for (k, v) in cli_vars {
            effective.insert(k, v);
        }

        assert_eq!(effective.get("owner").unwrap(), "from-yaml");
        assert_eq!(effective.get("repo").unwrap(), "from-cli");
    }

    #[test]
    fn resolve_task_flag_wins() {
        let vars = std::collections::HashMap::new();
        let result = resolve_task(
            Some("manual task"),
            &Some("default {{pr}}".to_string()),
            &vars,
        )
        .unwrap();
        assert_eq!(result, "manual task");
    }

    #[test]
    fn resolve_task_flow_prompt_with_args() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("pr".to_string(), "42".to_string());
        let result = resolve_task(None, &Some("Review PR #{{pr}}".to_string()), &vars).unwrap();
        assert_eq!(result, "Review PR #42");
    }

    #[test]
    fn resolve_task_no_task_no_prompt_errors() {
        let vars = std::collections::HashMap::new();
        let err = resolve_task(None, &None, &vars).unwrap_err();
        assert!(err.to_string().contains("no task specified"));
    }

    #[test]
    fn partition_role_overrides_from_template_vars() {
        use std::collections::{HashMap, HashSet};

        let mut role_names = HashSet::new();
        role_names.insert("reviewer".to_string());

        let mut all_args = HashMap::new();
        all_args.insert("issue".to_string(), "42".to_string());
        all_args.insert("reviewer".to_string(), "Kai".to_string());

        let (role_overrides, template_vars): (HashMap<_, _>, HashMap<_, _>) = all_args
            .into_iter()
            .partition(|(k, _)| role_names.contains(k));

        assert_eq!(role_overrides.len(), 1);
        assert_eq!(role_overrides.get("reviewer").unwrap(), "Kai");
        assert_eq!(template_vars.len(), 1);
        assert_eq!(template_vars.get("issue").unwrap(), "42");
    }

    // --- role cascade integration (issue #129) ---

    fn make_step(id: &str, agent: &str, role: Option<&str>) -> config::Step {
        config::Step {
            id: id.to_string(),
            agent: agent.to_string(),
            role: role.map(String::from),
            task: None,
            run: None,
            input: vec![],
            needs: vec![],
            model: None,
            backend: None,
            print_output: false,
            post_comment: None,
            agents: Vec::new(),
            max_turns: None,
            turn_timeout: None,
        }
    }

    fn make_flow_with_roles(
        roles: &[(&str, &str)],
        steps: Vec<config::Step>,
    ) -> config::FlowConfig {
        config::FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
            prompt: None,
            defaults: config::Defaults {
                model: "claude-sonnet-4-5".to_string(),
                backend: config::Backend::ClaudeCli,
            },
            roles: roles
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            steps,
            stack: config::StackConfig {
                backend: "local".to_string(),
                path: String::new(),
            },
        }
    }

    #[test]
    fn cli_rebind_changes_step_agent() {
        let steps = vec![make_step("s1", "OldAgent", Some("dev"))];
        let mut flow = make_flow_with_roles(&[("dev", "OldAgent")], steps);

        let overrides = vec![RoleOverride::Agent {
            role: "dev".to_string(),
            agent: "NewAgent".to_string(),
        }];

        apply_role_agent_overrides(&mut flow, None, &overrides);

        assert_eq!(flow.roles.get("dev").unwrap(), "NewAgent");
        assert_eq!(flow.steps[0].agent, "NewAgent");
    }

    #[test]
    fn cli_rebind_leaves_unrelated_steps_alone() {
        let steps = vec![
            make_step("s1", "Reviewer", Some("reviewer")),
            make_step("s2", "Direct", None), // direct agent, no role
        ];
        let mut flow = make_flow_with_roles(&[("reviewer", "Reviewer")], steps);

        let overrides = vec![RoleOverride::Agent {
            role: "reviewer".to_string(),
            agent: "Bella2".to_string(),
        }];

        apply_role_agent_overrides(&mut flow, None, &overrides);
        assert_eq!(flow.steps[0].agent, "Bella2");
        // Direct-agent step is untouched
        assert_eq!(flow.steps[1].agent, "Direct");
    }

    #[test]
    fn apply_resolved_roles_fills_step_model_and_backend() {
        let steps = vec![make_step("s1", "Sage", Some("dev"))];
        let mut flow = make_flow_with_roles(&[("dev", "Sage")], steps);

        let resolved = vec![ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "ollama/codestral".to_string(),
            backend: config::Backend::Api,
            model_source: "CLI override".to_string(),
            backend_source: "CLI override".to_string(),
            seed_origin: None,
        }];

        apply_resolved_roles_to_steps(&mut flow, &resolved);

        assert_eq!(flow.steps[0].model.as_deref(), Some("ollama/codestral"));
        assert_eq!(flow.steps[0].backend, Some(config::Backend::Api));
    }

    #[test]
    fn apply_resolved_roles_does_not_override_explicit_step_model() {
        // Step explicitly set its own model -- the cascade must respect that.
        let mut step = make_step("s1", "Sage", Some("dev"));
        step.model = Some("explicit/model".to_string());
        let steps = vec![step];
        let mut flow = make_flow_with_roles(&[("dev", "Sage")], steps);

        let resolved = vec![ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "from/role".to_string(),
            backend: config::Backend::Api,
            model_source: "role override".to_string(),
            backend_source: "role override".to_string(),
            seed_origin: None,
        }];

        apply_resolved_roles_to_steps(&mut flow, &resolved);
        assert_eq!(flow.steps[0].model.as_deref(), Some("explicit/model"));
    }

    #[test]
    fn apply_resolved_roles_skips_steps_without_role() {
        let steps = vec![make_step("s1", "Direct", None)];
        let mut flow = make_flow_with_roles(&[], steps);

        let resolved = vec![ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "from/role".to_string(),
            backend: config::Backend::Api,
            model_source: "role override".to_string(),
            backend_source: "role override".to_string(),
            seed_origin: None,
        }];

        apply_resolved_roles_to_steps(&mut flow, &resolved);
        // No mutation: step has no role binding to inherit from.
        assert!(flow.steps[0].model.is_none());
        assert!(flow.steps[0].backend.is_none());
    }

    #[test]
    fn all_args_are_template_vars_when_no_roles() {
        use std::collections::{HashMap, HashSet};

        let role_names: HashSet<String> = HashSet::new(); // No roles

        let mut all_args = HashMap::new();
        all_args.insert("issue".to_string(), "42".to_string());
        all_args.insert("branch".to_string(), "main".to_string());

        let (role_overrides, template_vars): (HashMap<_, _>, HashMap<_, _>) = all_args
            .into_iter()
            .partition(|(k, _)| role_names.contains(k));

        assert!(role_overrides.is_empty());
        assert_eq!(template_vars.len(), 2);
    }

    // --- run manifest (issue #31) ---

    #[test]
    fn build_manifest_records_run_metadata_and_resources() {
        use crate::stack::StepRecord;

        let dir = tempfile::tempdir().unwrap();
        let ctx = RunContext::new(
            "review".to_string(),
            "task".to_string(),
            dir.path().to_path_buf(),
            Some("guide content".to_string()),
            std::collections::HashMap::from([(
                "rust-developer".to_string(),
                "Use iterators".to_string(),
            )]),
            // Populate a skill so the manifest skill-pinning assertion below
            // is meaningful. Without this, two runs that differ only in
            // skill content would hash identically.
            std::collections::HashMap::from([(
                "domain-cli".to_string(),
                "skill content".to_string(),
            )]),
            std::collections::HashMap::new(),
        );

        let flow_path = dir.path().join("flow.yaml");
        let flow_contents = "version: '1'\nname: review\n";
        std::fs::write(&flow_path, flow_contents).unwrap();

        let seeds = Seeds::default_local();
        let agents: Vec<config::Agent> = vec![];
        let agent_origins: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let agent_hashes: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let roles = vec![ResolvedRole {
            name: "developer".to_string(),
            agent: "Sage".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            backend: config::Backend::ClaudeCli,
            model_source: "agent".to_string(),
            backend_source: "agent".to_string(),
            seed_origin: Some(".kuro/".to_string()),
        }];
        let mut vars = std::collections::HashMap::new();
        vars.insert("owner".to_string(), "nestrai".to_string());
        let results: Vec<StepRunResult> = vec![StepRunResult {
            step_id: "design".to_string(),
            agent_name: "Sage".to_string(),
            backend: "api".to_string(),
            duration: std::time::Duration::from_millis(1234),
            tokens_in: Some(100),
            tokens_out: Some(50),
            output_file: format!("{}/01-design.md", ctx.run_id),
            print_output: false,
            record: StepRecord {
                step_id: "design".to_string(),
                kind: "llm".to_string(),
                agent: Some("Sage".to_string()),
                model_requested: Some("claude-sonnet-4-5".to_string()),
                model_actual: Some("claude-sonnet-4-5".to_string()),
                backend: "api".to_string(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                duration_ms: 1234,
                started_at: ctx.started_at.to_rfc3339(),
                exit_code: 0,
                input_steps: vec![],
                output_file: "01-design.md".to_string(),
                participants: Vec::new(),
                turns: None,
                messages: None,
                terminated_by: None,
            },
        }];

        let manifest = build_manifest(
            &ctx,
            "review",
            &flow_path,
            flow_contents,
            &seeds,
            &agents,
            &agent_origins,
            &agent_hashes,
            &roles,
            &vars,
            &results,
            std::time::Duration::from_secs(2),
        );

        // Run identification matches the context.
        assert_eq!(manifest.run_id, ctx.run_id);
        assert_eq!(manifest.flow_name, "review");
        assert_eq!(manifest.flow_path, flow_path.display().to_string());
        // Hash is a 64-char hex SHA-256.
        assert_eq!(manifest.flow_sha256.len(), 64);
        // Resources cover flow + rules + skill + guide (no agents in this fixture).
        let kinds: Vec<&str> = manifest.resources.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains(&"flow"));
        assert!(kinds.contains(&"rules"));
        assert!(kinds.contains(&"skill"));
        assert!(kinds.contains(&"guide"));
        // Each resource record carries a populated SHA-256.
        for r in &manifest.resources {
            assert_eq!(r.sha256.len(), 64, "missing hash for {:?}", r);
        }
        // Skill record uses a project-relative path (.kuro/skills/<name>) and
        // its hash matches the cached content -- confirming runs that differ
        // only in skill content produce different manifest hashes.
        let skill = manifest
            .resources
            .iter()
            .find(|r| r.kind == "skill")
            .expect("skill record present");
        assert_eq!(skill.name, "domain-cli");
        assert_eq!(skill.path, ".kuro/skills/domain-cli");
        assert_eq!(skill.sha256, stack::sha256_hex(b"skill content"));
        // Roles round-tripped with backend label normalised.
        assert_eq!(manifest.roles[0].backend, "claude-cli");
        assert_eq!(manifest.roles[0].seed_origin.as_deref(), Some(".kuro/"));
        // Steps include the per-step record verbatim and totals are summed.
        assert_eq!(manifest.steps.len(), 1);
        assert_eq!(manifest.steps[0].step_id, "design");
        assert_eq!(manifest.total_tokens_in, 100);
        assert_eq!(manifest.total_tokens_out, 50);
        // Cost not tracked yet -- field present, value None for forward compat.
        assert!(manifest.cost.is_none());
        assert_eq!(manifest.duration_ms, 2000);
    }
}
