use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;

mod config;
mod dag;
#[allow(dead_code)]
mod executor;
#[allow(dead_code)]
mod llm;
mod runner;
mod skills;
mod stack;
#[allow(dead_code)]
mod ui;

use crate::runner::RunContext;

const KOTO_DIR: &str = ".koto";
const FLOWS_DIR: &str = ".koto/flows";

#[derive(Parser)]
#[command(name = "koto", about = "Reproducible AI agent teams", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the agent team
    Up {
        /// Flow name (looks in .koto/flows/<name>.yaml)
        flow: Option<String>,

        /// Task prompt or template arguments (key=value pairs fill {{key}} placeholders in the flow prompt)
        #[arg(short = 't', long)]
        task: Option<String>,

        /// Template arguments as key=value pairs (e.g. pr=67 branch=main)
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,

        /// Path to the flow config file (overrides flow name lookup)
        #[arg(short, long)]
        file: Option<String>,
    },
    /// Run an ad-hoc task with one or more agents (no flow needed)
    Task {
        /// Agent name(s) from .koto/agents/ (repeatable, executed in order)
        #[arg(short, long, required = true)]
        agent: Vec<String>,

        /// Task prompt
        #[arg(short = 't', long, required = true)]
        task: String,
    },
    /// Fetch skills from remote sources pinned in .koto/skills.lock
    Pull,
    /// Stop the agent team
    Down,
    /// Show running agents and stack
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Up {
            flow,
            task,
            args,
            file,
        } => run_up(flow.as_deref(), task.as_deref(), &args, file.as_deref()).await?,
        Command::Task { agent, task } => run_task(&agent, &task).await?,
        Command::Pull => run_pull()?,
        Command::Down => {
            println!("koto down: not yet implemented");
        }
        Command::Status => {
            println!("koto status: not yet implemented");
        }
    }

    Ok(())
}

/// Resolve the flow config file path from CLI arguments.
fn resolve_flow_path(flow: Option<&str>, file: Option<&str>) -> Result<PathBuf> {
    // --file takes precedence
    if let Some(f) = file {
        let path = PathBuf::from(f);
        if !path.exists() {
            return Err(eyre!("config file '{}' not found", f));
        }
        return Ok(path);
    }

    // If a flow name is given, look in .koto/flows/
    if let Some(name) = flow {
        let path = PathBuf::from(FLOWS_DIR).join(format!("{name}.yaml"));
        if !path.exists() {
            return Err(eyre!(
                "flow '{name}' not found at {}\n\nhint: create {0} or use --file <path>",
                path.display()
            ));
        }
        return Ok(path);
    }

    // No args: auto-select if only one flow exists
    let flows_dir = Path::new(FLOWS_DIR);
    if !flows_dir.exists() {
        return Err(eyre!(
            "no .koto/flows/ directory found\n\nhint: create .koto/flows/<name>.yaml, or use --file <path>"
        ));
    }

    let mut flows: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(flows_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".yaml") || name_str.ends_with(".yml") {
                flows.push(
                    name_str
                        .trim_end_matches(".yaml")
                        .trim_end_matches(".yml")
                        .to_string(),
                );
            }
        }
    }

    if flows.is_empty() {
        return Err(eyre!(
            "no flows found in .koto/flows/\n\nhint: create .koto/flows/<name>.yaml"
        ));
    }

    // Auto-select if only one flow
    if flows.len() == 1 {
        let name = &flows[0];
        return Ok(PathBuf::from(FLOWS_DIR).join(format!("{name}.yaml")));
    }

    flows.sort();
    let list = flows
        .iter()
        .map(|f| format!("  - {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(eyre!(
        "multiple flows found, specify one:\n\n{list}\n\nusage: koto up <flow-name> -t \"task\""
    ))
}

/// Resolve the stack path: explicit config > default (~/.koto/stacks/<project>/).
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

/// Replace `{{key}}` placeholders in a prompt with values from the map.
fn substitute_placeholders(
    prompt: &str,
    vars: &std::collections::HashMap<String, String>,
) -> Result<String> {
    let re = regex_lite::Regex::new(r"\{\{([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap();
    let mut result = prompt.to_string();
    let mut missing: Vec<String> = Vec::new();

    for cap in re.captures_iter(prompt) {
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

    // Use defaults matching flow config defaults
    let defaults = config::Defaults {
        model: "claude-sonnet-4-5".to_string(),
        backend: config::Backend::ClaudeCli,
    };

    // Load requested agents
    let mut agents = Vec::new();
    for name in agent_names {
        let agent = config::load_agent_file(koto_dir, name, &defaults)?;
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
            task: None,
            input,
            needs: vec![],
            model: None,
            backend: None,
            print_output: i == agents.len() - 1, // last step prints
        });
    }

    let step_refs: Vec<&config::Step> = steps.iter().collect();

    let task_name = if agent_names.len() == 1 {
        format!("task-{}", agent_names[0].to_lowercase())
    } else {
        "task".to_string()
    };

    ui::print_command(&format!(
        "koto task --agent {} -t \"...\"",
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

    // Load context
    let guide = runner::load_guide(koto_dir);
    let rules_cache = runner::load_rules_for_agents(&agents, koto_dir)?;

    // Skills
    let skills_dir = koto_dir.join("skills");
    let skill_names = skills::collect_skill_names(&agents);
    let skills_cache = if skill_names.is_empty() {
        std::collections::HashMap::new()
    } else {
        let missing = skills::check_skills_available(&skill_names, &skills_dir);
        if !missing.is_empty() {
            return Err(eyre!(
                "missing skills: {}\n\nhint: run `koto pull` to fetch skills",
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

async fn run_up(
    flow: Option<&str>,
    task: Option<&str>,
    args: &[String],
    file: Option<&str>,
) -> Result<()> {
    let flow_start = Instant::now();
    let path = resolve_flow_path(flow, file)?;
    let display_path = path.display().to_string();

    ui::print_command(&format!("koto up {}", flow.unwrap_or(&display_path)));

    // Parse role names from YAML to partition args
    let contents = std::fs::read_to_string(&path)?;
    let role_names = config::parse_role_names(&contents)?;

    // Partition args: role overrides vs template vars
    let all_args = parse_key_value_args(args)?;
    let (role_overrides, template_vars): (
        std::collections::HashMap<_, _>,
        std::collections::HashMap<_, _>,
    ) = all_args
        .into_iter()
        .partition(|(k, _)| role_names.contains(k));

    // Validate template vars against declared placeholders
    if !template_vars.is_empty() {
        let flow_config_temp = config::load_flow_from_str(&contents)?;
        let placeholders = flow_config_temp
            .prompt
            .as_ref()
            .map(|p| config::extract_placeholders(p))
            .unwrap_or_default();

        let provided_keys: HashSet<String> = template_vars.keys().cloned().collect();
        config::validate_template_vars(&provided_keys, &placeholders)?;
    }

    // Load flow with role overrides
    let flow_config = config::load_flow_from_str_with_overrides(&contents, &role_overrides)?;

    // Resolve task prompt: -t flag > flow default prompt with {{key}} substitution
    let resolved_task = resolve_task(task, &flow_config.prompt, &template_vars)?;

    // Derive flow name for tmux sessions
    let flow_name = flow
        .map(|s| s.to_string())
        .unwrap_or_else(|| flow_config.name.clone());

    // Load agents referenced by the flow
    let koto_dir = Path::new(KOTO_DIR);
    let agents = config::load_agents_for_flow(koto_dir, &flow_config)?;

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

    // Load guide and rules context
    let guide = runner::load_guide(koto_dir);
    let rules_cache = runner::load_rules_for_agents(&agents, koto_dir)?;

    // Check and load skills
    let skills_dir = koto_dir.join("skills");
    let skill_names = skills::collect_skill_names(&agents);
    let skills_cache = if skill_names.is_empty() {
        std::collections::HashMap::new()
    } else {
        let missing = skills::check_skills_available(&skill_names, &skills_dir);
        if !missing.is_empty() {
            return Err(eyre!(
                "missing skills: {}\n\nhint: run `koto pull` to fetch skills",
                missing.join(", ")
            ));
        }
        skills::load_skills_for_agents(&skill_names, &skills_dir)?
    };

    // Resolve stack path
    let stack_path = resolve_stack_path(&flow_config.stack.path);

    // Construct RunContext
    let ctx = RunContext::new(
        flow_name.clone(),
        resolved_task,
        stack_path.clone(),
        guide,
        rules_cache,
        skills_cache,
    );

    // Run steps
    let results = runner::run_steps(&steps, &agents, &ctx).await?;

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

fn run_pull() -> Result<()> {
    let koto_dir = Path::new(KOTO_DIR);
    let lock_path = koto_dir.join("skills.lock");

    if !lock_path.exists() {
        return Err(eyre!(
            "no .koto/skills.lock found\n\nhint: create .koto/skills.lock with your skill sources"
        ));
    }

    let lock = skills::load_skills_lock(&lock_path)?;
    if lock.skills.is_empty() {
        println!("no skills defined in .koto/skills.lock");
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
}
