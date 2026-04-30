use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::eyre;

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

use crate::koto_config::{KOTO_DIR, KotoConfig, Seeds};
use crate::resolver::parse_role_override;
use crate::runner::{ExecuteFlowSpec, FlowSource};

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

/// Parse `key=value` pairs from trailing CLI args. Stays in main.rs because
/// it's CLI-shaped: bumps a syntax error to the user with a hint about the
/// expected form. Library callers construct `HashMap` directly.
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

    let stack_path = runner::resolve_stack_path("");

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

/// Translate the clap-parsed [`RunArgs`] into an [`ExecuteFlowSpec`] and
/// drive the run via [`runner::execute_flow`]. The CLI takes responsibility
/// for parsing `key=value` arguments and for printing the flow's `print_output`
/// step bodies after completion -- everything else (config loading, role
/// resolution, audit, run-context construction, step execution, manifest write,
/// summary print) lives in the library API so MCP and other harnesses share it.
async fn run_flow(run_args: &RunArgs) -> Result<()> {
    let role_overrides: Vec<resolver::RoleOverride> = run_args
        .role_overrides
        .iter()
        .map(|s| parse_role_override(s))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| eyre!("{e}"))?;

    let vars = parse_key_value_args(&run_args.vars)?;
    let bare_args = parse_key_value_args(&run_args.args)?;

    let flow_source = match (run_args.flow.as_deref(), run_args.file.as_deref()) {
        (_, Some(f)) => FlowSource::File(PathBuf::from(f)),
        (Some(name), None) => FlowSource::Name(name.to_string()),
        (None, None) => FlowSource::Auto,
    };

    let spec = ExecuteFlowSpec {
        flow: flow_source,
        task: run_args.task.clone(),
        vars,
        role_overrides,
        bare_args,
        suppress_command_banner: false,
    };

    let handle = runner::execute_flow(spec).await?;
    let result = handle.await_completion().await?;

    // CLI-only post-step display: render any `print_output: true` step bodies
    // to the terminal via termimad. The library API leaves this to callers
    // because MCP and other quiet harnesses do not want markdown rendered to
    // their stdout.
    for step in &result.step_results {
        if step.print_output {
            let output_path = result.stack_path.join(&step.output_file);
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

    // Helpers moved into `runner` (issue #209) but the existing test suite
    // exercises them by short, unprefixed names. Re-import via aliases so the
    // assertions below read the same after the refactor.
    use crate::resolver::{ResolvedRole, RoleOverride};
    use crate::runner::{
        RunContext, StepRunResult, apply_resolved_roles_to_steps, apply_role_agent_overrides,
        build_manifest, resolve_task, substitute_placeholders, substitute_vars,
    };

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
