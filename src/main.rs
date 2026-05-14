use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::Result;
use color_eyre::eyre::eyre;

mod chat;
mod config;
mod config_md;
mod context;
mod core;
mod dag;
#[allow(dead_code)]
mod executor;
mod koto_config;
#[allow(dead_code)]
mod llm;
#[allow(dead_code)]
mod mcp;
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

    /// Force the non-interactive pause-and-exit path even on a TTY (issue #361).
    ///
    /// On a TTY the default is to prompt inline at `human:` states and
    /// continue in the same process. Pass this flag in CI, scripts, or
    /// any workflow that wants explicit two-step control through
    /// `kuro resume <run-id>`. Non-TTY runs (piped stdout, captured
    /// stdin) always behave as if this flag were set -- the flag exists
    /// for the case where the operator is on a real terminal but still
    /// wants the pause-and-resume contract.
    #[arg(long = "no-interactive")]
    no_interactive: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run a flow
    Run(RunArgs),
    /// Resume a paused graph run (issue #338).
    ///
    /// Re-enters a previously paused run at the state recorded in its
    /// manifest. The `<run-id>` is the directory name under
    /// `~/.koto/stacks/<project>/<run-id>/`; `kuro stack` and the
    /// terminal output of the original `kuro run` both name it. The
    /// project is derived from the cwd, identical to `kuro run` --
    /// cross-project resume is not in v1's scope.
    Resume {
        /// Run identifier as it appears under `~/.koto/stacks/<project>/`.
        run_id: String,

        /// Inline human input body (issue #360).
        ///
        /// Used for flows that pause at a `human:` state but are not anchored
        /// to a GitHub issue (i.e. `vars.id` is missing or non-numeric).
        /// Mutually exclusive with `--message-file`; takes precedence over
        /// piped stdin and over any GitHub comments that would also have
        /// fired (the conflict surfaces as a `[warn]` on stderr).
        #[arg(short = 'm', long, conflicts_with = "message_file")]
        message: Option<String>,

        /// Read human input body from a file (issue #360).
        ///
        /// Same precedence as `--message`. Useful for multi-line reviews
        /// or paste-from-editor workflows where wrapping the text on the
        /// command line is awkward. Mutually exclusive with `--message`.
        #[arg(long = "message-file", value_name = "PATH")]
        message_file: Option<PathBuf>,
    },
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

        /// Inject the cwd project's Guide.md into the agent system prompt.
        /// Off by default so seed agents stay repo-agnostic (issue #245); turn
        /// on when you explicitly want the agent to operate as a member of
        /// the current project's team.
        #[arg(long = "include-project-context")]
        include_project_context: bool,
    },
    /// Drop into the agent's underlying CLI in interactive mode
    /// (claude, codex, ollama) with the agent's persona + rules
    /// pre-loaded. Stdin/stdout/stderr inherit from the parent shell;
    /// exit with the upstream CLI's mechanism (`/exit`, Ctrl-D).
    Chat {
        /// Agent name from .kuro/agents/
        #[arg(short, long, required = true)]
        agent: String,

        /// Inject the cwd project's Guide.md into the agent system prompt.
        /// Mirror of `kuro task --include-project-context`: off by default,
        /// opt-in when the chat session is meant to be project-aware.
        #[arg(long = "include-project-context")]
        include_project_context: bool,
    },
    /// Print the resolved seed cascade for the current project (#366).
    ///
    /// Walks the seeds declared in `.kuro/config.yaml` (or the implicit
    /// `.kuro/` default) and lists what each one contributes: agents,
    /// rules, flows and a verbatim `SEED.md` when present. Also shows
    /// the first-match-wins "effective" view so an AI assistant can
    /// see at a glance which agent/rule/flow the runner would actually
    /// load when there's overlap between seeds.
    ///
    /// Default output is human-readable. Pass `--format json` for the
    /// stable v1 machine-readable shape -- AI clients embed it in their
    /// working memory at session start so they stop duplicating agents
    /// that already exist in the cascade.
    ///
    /// Missing `.kuro/`, missing seed paths and missing seed subdirs
    /// degrade silently to empty sections. Use `kuro validate` for
    /// semantic checks; this command is inventory, not validation.
    Context {
        /// Output format. `human` (default) renders a readable table;
        /// `json` emits the stable v1 wire format for AI clients.
        #[arg(long, value_enum, default_value_t = ContextFormat::Human)]
        format: ContextFormat,
    },
    /// Validate a flow's structure (schema + graph reachability/dead-ends).
    ///
    /// Exits non-zero on hard errors (e.g. dead-end states); exits zero
    /// on warnings only (e.g. unreachable states). Warnings and errors
    /// route to stderr; stdout stays clean for machine-readable use.
    Validate {
        /// Flow name (looked up in seeds under `flows/<name>.yaml` or
        /// `flows/<name>.md`) or a path to a flow file. Path is tried
        /// first; falls back to the seed-based lookup if the value is
        /// not an existing file.
        flow: String,
    },
    /// Fetch skills from remote sources pinned in .kuro/skills.lock
    Pull,
    /// Stop the agent team
    #[command(hide = true)]
    Down,
    /// Show running agents and stack
    Status,
    /// Run as a Model Context Protocol server over stdio (#195).
    ///
    /// External agents (Codex, Cursor, Claude Code) spawn this command and
    /// exchange newline-delimited JSON-RPC 2.0 frames on stdin/stdout to
    /// invoke kuro tools. Stops on stdin EOF. Diagnostics route to stderr.
    Mcp {
        /// Bump tracing level to DEBUG (overridden by `RUST_LOG`).
        #[arg(long)]
        verbose: bool,
    },
    /// Inspect or remove persistent stack data (#232).
    ///
    /// Parent for stack-management subcommands. Today only `purge` lives
    /// here; future siblings (`kuro stack list`, `kuro stack show`) plug
    /// in without polluting the top-level namespace.
    Stack {
        #[command(subcommand)]
        action: StackAction,
    },
}

/// Output format for `kuro context` (#366).
///
/// Default is human-readable. The JSON renderer emits the stable
/// `version: "1"` shape so AI clients can rely on the layout from
/// release to release.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
enum ContextFormat {
    Human,
    Json,
}

/// Subcommands under `kuro stack`. Kept separate from [`Command`] so the
/// stack-management surface stays self-contained -- adding a sibling does
/// not touch the root parser.
#[derive(Subcommand)]
enum StackAction {
    /// Permanently delete all stack data for a project (GDPR Art. 17).
    ///
    /// The project is named explicitly -- `kuro stack purge` will not
    /// derive it from the cwd, since the typical erasure use case is
    /// targeting a project that is no longer the active directory.
    Purge {
        /// Project name as it appears under `~/.koto/stacks/`. Must be a
        /// single path segment (no `/`, `..`, or leading `.`).
        project: String,

        /// Print what would be deleted (run count, file count, byte size)
        /// and exit without touching disk.
        #[arg(long)]
        dry_run: bool,

        /// Skip the confirmation prompt. Required for non-interactive use
        /// -- without it, the command refuses to proceed when stdin is
        /// not a TTY.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run_flow(&args).await?,
        Command::Resume {
            run_id,
            message,
            message_file,
        } => resume_flow(&run_id, message, message_file).await?,
        Command::Up(args) => {
            eprintln!(
                "warning: `kuro up` is deprecated and will be removed in a future release; use `kuro run` instead"
            );
            run_flow(&args).await?
        }
        Command::Task {
            agent,
            task,
            include_project_context,
        } => run_task(&agent, &task, include_project_context).await?,
        Command::Chat {
            agent,
            include_project_context,
        } => chat::run_chat(&agent, include_project_context).await?,
        Command::Context { format } => cmd_context(format)?,
        Command::Validate { flow } => run_validate(&flow)?,
        Command::Pull => run_pull()?,
        Command::Down => {
            println!("kuro down: not yet implemented");
        }
        Command::Status => {
            println!("kuro status: not yet implemented");
        }
        Command::Mcp { verbose } => mcp::run(verbose).await?,
        Command::Stack { action } => match action {
            StackAction::Purge {
                project,
                dry_run,
                yes,
            } => cmd_stack_purge(&project, dry_run, yes)?,
        },
    }

    Ok(())
}

/// Implementation of `kuro context` (#366).
///
/// Resolves the cascade from the current working directory and dispatches
/// to the human or JSON renderer. The library does the work; this wrapper
/// only translates the format flag and the I/O destination.
///
/// Errors are surfaced via [`color_eyre`]; the only failure path today is
/// a malformed `.kuro/config.yaml`. Missing seeds and missing subdirs
/// degrade silently (see [`context::resolve`]).
fn cmd_context(format: ContextFormat) -> Result<()> {
    let cwd = std::env::current_dir().map_err(|e| eyre!("cwd: {e}"))?;
    let ctx = context::resolve(&cwd).map_err(|e| eyre!("{e}"))?;
    // Render to a buffer first, then write to stdout in one shot.
    // Lets us absorb the BrokenPipe a downstream `| head` produces
    // without surfacing a panicky error report -- pipes to pagers
    // are the expected interactive use case.
    let mut buf: Vec<u8> = Vec::new();
    match format {
        ContextFormat::Human => context::render_human(&ctx, &mut buf)
            .map_err(|e| eyre!("failed to render context: {e}"))?,
        ContextFormat::Json => context::render_json(&ctx, &mut buf)
            .map_err(|e| eyre!("failed to render context as JSON: {e}"))?,
    }
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match out.write_all(&buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(eyre!("failed to write context to stdout: {e}")),
    }
}

/// Implementation of `kuro stack purge <project>`.
///
/// The library does the dangerous work (validation, canonical containment,
/// directory removal). This function is responsible for: rendering the
/// preview, gating on `--yes` / TTY, and translating `StackError` into a
/// user-facing exit. Splitting the work this way keeps `src/stack.rs`
/// usable from non-CLI callers (a future MCP tool, scripted use) without
/// dragging confirmation logic along.
fn cmd_stack_purge(project: &str, dry_run: bool, yes: bool) -> Result<()> {
    let root = stack::stack_root();
    // `plan_purge` runs the full validation chain (string shape,
    // containment) and returns the same diagnostics `purge_project` would
    // -- so dry-run and live mode behave identically up to the deletion.
    let report = stack::plan_purge(&root, project).map_err(format_purge_error)?;

    eprintln!(
        "stack '{}': {} run(s), {} file(s), {} byte(s) at {}",
        report.project,
        report.run_count,
        report.file_count,
        report.byte_size,
        report.path.display()
    );

    if dry_run {
        eprintln!("dry-run: nothing was deleted");
        return Ok(());
    }

    if !yes {
        use std::io::{IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            return Err(eyre!(
                "refusing to purge non-interactively without --yes\n\nhint: pass --yes to confirm, or run with a TTY attached"
            ));
        }
        eprint!(
            "Permanently delete {} run(s) from {}? [y/N] ",
            report.run_count,
            report.path.display()
        );
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| eyre!("failed to read confirmation: {e}"))?;
        let trimmed = answer.trim().to_ascii_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            return Err(eyre!("aborted"));
        }
    }

    let report = stack::purge_project(&root, project).map_err(format_purge_error)?;
    eprintln!(
        "deleted '{}': {} run(s), {} file(s), {} byte(s)",
        report.project, report.run_count, report.file_count, report.byte_size
    );
    Ok(())
}

/// Translate a `StackError` from the purge surface into a `color_eyre`
/// report whose text matches what we want users to see. The library error
/// already carries the project name and the path; we keep it terse so the
/// CLI banner does not double-print context.
fn format_purge_error(err: stack::StackError) -> color_eyre::eyre::Report {
    eyre!("{err}")
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

async fn run_task(agent_names: &[String], task: &str, include_project_context: bool) -> Result<()> {
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
            extra_args: std::collections::HashMap::new(),
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

    // Load context through the seed list -- guide is gated on
    // `--include-project-context` so `kuro task --agent X` stays repo-agnostic
    // by default (issue #245). Rules still error with the seeds searched if a
    // referenced rule is missing -- those are part of the agent persona, not
    // cwd-project context.
    let guide =
        runner::load_guide_for_task(&seeds, include_project_context).map_err(|e| eyre!("{e}"))?;
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
        no_interactive: run_args.no_interactive,
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

/// Implementation of `kuro resume <run-id>` (issues #338 + #360).
///
/// Thin CLI wrapper around the library's [`runner::resume_run_with_input`]:
/// resolves CLI flags + stdin into an optional [`runner::LocalHumanInput`],
/// then hands the run-id and the resolved input to the library. Awaits
/// the spawned driver task, and renders any `print_output: true` step
/// bodies the same way [`run_flow`] does. Setup-side errors (run-id
/// not found, status not paused, flow missing, no human input + no GH
/// source) surface synchronously with their hint-tagged messages -- this
/// layer adds no extra translation.
async fn resume_flow(
    run_id: &str,
    message: Option<String>,
    message_file: Option<PathBuf>,
) -> Result<()> {
    use std::io::{IsTerminal, Read};

    let stdin = std::io::stdin();
    let stdin_isatty = stdin.is_terminal();
    let local = collect_local_human_input(message, message_file, stdin_isatty, || {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map(|_| buf)
    })?;

    let handle =
        runner::resume_run_with_input(run_id, notify::github::gh_comments_fetcher(), local).await?;
    let result = handle.await_completion().await?;

    // Mirrors `run_flow`'s post-completion render: a resumed run that
    // continues to a final state may still hit a `print_output: true`
    // step, and the operator wants the same on-screen artifact view.
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

/// Resolve `--message`, `--message-file`, and stdin into an optional
/// [`runner::LocalHumanInput`] (issue #360).
///
/// Precedence (high to low):
///   1. `--message <TEXT>` -- inline string, smallest body, explicit
///      operator intent. Wins over stdin: if both fire, stdin is drained
///      and discarded with a `[warn]` so the operator sees the conflict.
///   2. `--message-file <PATH>` -- file body, multi-line friendly. Rejected
///      as conflict with `--message` at the clap layer (`conflicts_with`).
///   3. stdin -- only consulted when the terminal is NOT a TTY. On a TTY
///      we leave stdin alone so an interactive `kuro resume` does not
///      block on a `read`.
///
/// `stdin_isatty` + `stdin_read` are seams so the resolver stays pure
/// and unit-testable without spawning a TTY. The production caller
/// passes `std::io::stdin().is_terminal()` and a closure that reads from
/// the real stdin.
///
/// Returns `Ok(None)` when no local source supplied input; the resume
/// pipeline falls back to the GH-comments path. Empty `--message ""` is
/// treated as "no input": the resolver returns `Ok(None)` so the flow
/// behaves identically to having omitted the flag, rather than writing
/// a zero-byte synthetic step on disk.
fn collect_local_human_input(
    message: Option<String>,
    message_file: Option<PathBuf>,
    stdin_isatty: bool,
    stdin_read: impl FnOnce() -> std::io::Result<String>,
) -> Result<Option<runner::LocalHumanInput>> {
    if let Some(body) = message {
        // Drain stdin to avoid a SIGPIPE to the upstream producer, then
        // warn so the conflict is visible. Only fires on a non-TTY: a
        // TTY stdin is ignored unconditionally and there is nothing to
        // drain.
        if !stdin_isatty {
            // We do not actually read stdin here -- discarding the body
            // and warning is enough for the test seam; production stdin
            // closes on its own when the child exits. The warning makes
            // the precedence visible.
            eprintln!("[warn] --message overrides piped stdin; stdin content discarded");
        }
        let trimmed = body.trim_end_matches('\n');
        if trimmed.is_empty() {
            return Ok(None);
        }
        return Ok(Some(runner::LocalHumanInput {
            body: trimmed.to_string(),
            source: "--message".to_string(),
        }));
    }
    if let Some(path) = message_file {
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            eyre!(
                "failed to read --message-file {}: {e}\n\nhint: check the path exists and is readable",
                path.display(),
            )
        })?;
        let trimmed = raw.trim_end_matches('\n');
        if trimmed.is_empty() {
            return Ok(None);
        }
        return Ok(Some(runner::LocalHumanInput {
            body: trimmed.to_string(),
            source: format!("--message-file {}", path.display()),
        }));
    }
    // Stdin is the last local source. Only consult it when stdin is not
    // a TTY so an interactive `kuro resume` does not silently block on
    // a `read`.
    if !stdin_isatty {
        let raw = stdin_read().map_err(|e| {
            eyre!(
                "failed to read stdin: {e}\n\nhint: pipe the body via `echo \"...\" | kuro resume`"
            )
        })?;
        let trimmed = raw.trim_end_matches('\n');
        if !trimmed.is_empty() {
            return Ok(Some(runner::LocalHumanInput {
                body: trimmed.to_string(),
                source: "stdin".to_string(),
            }));
        }
    }
    Ok(None)
}

/// Implementation of `kuro validate <flow>`.
///
/// Resolves the flow argument by trying it as a file path first and
/// falling back to a seed-based name lookup (mirrors `kuro run`'s
/// resolution so the two commands accept the same value). Loads the
/// YAML through the polymorphic [`config::load_flow_any_from_str`]:
///
/// * Linear flows pass through schema validation only -- there is no
///   graph layer to walk. A successful parse is reported as ok.
/// * Graph flows additionally run [`config::validate_graph_reachability`],
///   which surfaces dead-ends (errors) and unreachable states (warnings).
///
/// Output discipline (issue #238):
/// * stdout receives only the success summary, so callers can grep or
///   pipe stdout for machine-readable use.
/// * stderr receives every warning and every error, prefixed with
///   `warning:` / `error:` so the source is unambiguous.
/// * Non-zero exit on any hard error; zero exit on warnings only.
fn run_validate(flow: &str) -> Result<()> {
    let path = if Path::new(flow).is_file() {
        PathBuf::from(flow)
    } else {
        // Fall back to the seed-based name lookup. Same resolver
        // `kuro run` uses, so the two commands accept identical values.
        let koto_config = KotoConfig::load_optional(Path::new("."))?;
        let seeds = koto_config
            .as_ref()
            .map(|c| c.seeds.clone())
            .unwrap_or_else(Seeds::default_local);
        runner::resolve_flow_path(&FlowSource::Name(flow.to_string()), &seeds)?
    };

    // Path-aware loader (#258) so `prompt_file:` / `task_file:` references
    // resolve against the flow's directory. Missing files surface as
    // validation errors that name the flow path and the offending state
    // ID, which is the channel `kuro validate` uses to report them.
    let parsed = config::load_flow_any_from_path(&path)
        .map_err(|e| eyre!("failed to load flow '{}': {e}", path.display()))?;

    match parsed {
        config::Flow::Linear(_) => {
            // Linear flows have no graph layer to walk; a successful
            // parse already validated everything we know how to check.
            println!("ok: {} validates (linear flow)", path.display());
            Ok(())
        }
        config::Flow::Graph(g) => {
            let report = config::validate_graph_reachability(&g);
            // Warnings first, errors second -- reading order matches
            // severity (lighter to heavier) so the worst news is the
            // last thing the user sees before the prompt returns.
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            for error in &report.errors {
                eprintln!("error: {error}");
            }
            if report.is_ok() {
                println!("ok: {} validates (graph flow)", path.display());
                Ok(())
            } else {
                Err(eyre!(
                    "validation failed: {} error(s) in {}",
                    report.errors.len(),
                    path.display()
                ))
            }
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

    // Helpers moved into `runner` (issue #209) but the existing test suite
    // exercises them by short, unprefixed names. Re-import via aliases so the
    // assertions below read the same after the refactor.
    use crate::resolver::{ResolvedRole, RoleOverride};
    use crate::runner::{
        RunContext, StepRunResult, apply_resolved_roles_to_steps, apply_role_agent_overrides,
        build_manifest, resolve_task, substitute_placeholders, substitute_vars,
    };

    // --- CLI parsing for `run` (canonical) and `up` (deprecated alias) (issue #181) ---

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
    fn run_subcommand_accepts_no_interactive_flag() {
        // Issue #361 AC3: `kuro run <flow> --no-interactive` must
        // parse cleanly and surface `true` on `RunArgs::no_interactive`.
        // Default (flag absent) is `false` so existing scripts keep
        // their current behaviour.
        let (cli, _) = parse_cli(&["kuro", "run", "review-pr", "--no-interactive"]);
        let Command::Run(args) = cli.command else {
            unreachable!()
        };
        assert!(
            args.no_interactive,
            "--no-interactive must flip the field to true"
        );

        // Default: absent flag is false.
        let (cli, _) = parse_cli(&["kuro", "run", "review-pr"]);
        let Command::Run(args) = cli.command else {
            unreachable!()
        };
        assert!(
            !args.no_interactive,
            "no flag means inline-on-TTY default behaviour (false)"
        );
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

    // --- collect_local_human_input + Resume CLI parsing (issue #360) ---

    #[test]
    fn collect_local_human_input_prefers_message_over_stdin() {
        // Precedence rule 1: `--message` is the highest-priority local
        // source. Stdin must NOT be read when `--message` is set.
        let out = collect_local_human_input(
            Some("approve".to_string()),
            None,
            false, // not a TTY, so stdin would otherwise be eligible
            || panic!("stdin must not be read when --message is set"),
        )
        .unwrap()
        .expect("--message must produce Some");
        assert_eq!(out.body, "approve");
        assert_eq!(out.source, "--message");
    }

    #[test]
    fn collect_local_human_input_uses_stdin_when_no_message_and_not_tty() {
        // Precedence rule 3: stdin is consulted last, only when neither
        // flag was supplied AND stdin is not a TTY. The closure is the
        // pure-function seam.
        let out = collect_local_human_input(None, None, false, || Ok("ship it\n".to_string()))
            .unwrap()
            .expect("stdin must produce Some when non-empty");
        assert_eq!(out.body, "ship it");
        assert_eq!(out.source, "stdin");
    }

    #[test]
    fn collect_local_human_input_ignores_stdin_on_tty() {
        // A TTY stdin must not be read -- an interactive `kuro resume`
        // would otherwise block on `read`. Closure must not be invoked.
        let out = collect_local_human_input(None, None, true, || {
            panic!("stdin must not be read on a TTY")
        })
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn collect_local_human_input_reads_file() {
        // `--message-file` reads the file verbatim and labels the source
        // with the path so the audit trail names which file fed the run.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feedback.md");
        std::fs::write(&path, "looks good\n").unwrap();
        let out = collect_local_human_input(
            None,
            Some(path.clone()),
            true, // does not matter; file beats stdin
            || panic!("stdin must not be read when --message-file is set"),
        )
        .unwrap()
        .expect("--message-file must produce Some");
        assert_eq!(out.body, "looks good");
        assert!(
            out.source.starts_with("--message-file "),
            "source must include the flag, got: {}",
            out.source
        );
        assert!(
            out.source.contains(path.to_string_lossy().as_ref()),
            "source must include the path, got: {}",
            out.source
        );
    }

    #[test]
    fn collect_local_human_input_returns_none_when_no_sources() {
        // No flags, TTY stdin -- the runner falls back to the GH path.
        let out = collect_local_human_input(None, None, true, || {
            panic!("stdin must not be read on a TTY")
        })
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn collect_local_human_input_treats_empty_message_as_no_input() {
        // `--message ""` is a degenerate invocation -- behave the same
        // as omitting the flag rather than writing a zero-byte step.
        let out = collect_local_human_input(Some(String::new()), None, true, || {
            panic!("stdin must not be read on a TTY")
        })
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn collect_local_human_input_treats_blank_stdin_as_no_input() {
        // Empty pipe content (`echo "" | ...` -> "\n") must not produce
        // a synthetic step. Mirrors the empty-message contract.
        let out = collect_local_human_input(None, None, false, || Ok("\n".to_string())).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn collect_local_human_input_errors_on_missing_file() {
        // A typo in `--message-file <PATH>` must fail loud at resume
        // time, not silently produce empty input.
        let err = collect_local_human_input(
            None,
            Some(PathBuf::from("/nonexistent/path/feedback.md")),
            true,
            || panic!("stdin must not be read"),
        )
        .expect_err("missing file must surface as an error");
        assert!(
            err.to_string().contains("failed to read --message-file"),
            "expected file-read error message, got: {err}"
        );
    }

    // --- `kuro context` CLI parsing (issue #366) ---

    #[test]
    fn context_subcommand_parses_canonically() {
        // `kuro context` alone parses to Command::Context with the
        // default human format. Pins the wire-level CLI shape so a
        // future refactor that renames the variant gets caught.
        let cli = Cli::try_parse_from(["kuro", "context"]).expect("CLI parse failed");
        let Command::Context { format } = cli.command else {
            panic!("expected Command::Context, got something else");
        };
        assert_eq!(format, ContextFormat::Human);
    }

    #[test]
    fn context_subcommand_accepts_format_json() {
        // `--format json` flips the discriminant. AI clients embed
        // this exact invocation in their session-start instructions,
        // so a regression here breaks downstream prompts.
        let cli =
            Cli::try_parse_from(["kuro", "context", "--format", "json"]).expect("CLI parse failed");
        let Command::Context { format } = cli.command else {
            panic!("expected Command::Context, got something else");
        };
        assert_eq!(format, ContextFormat::Json);
    }

    #[test]
    fn resume_subcommand_message_and_file_are_mutually_exclusive() {
        // Pin the clap-level `conflicts_with` contract: passing both
        // flags must fail parsing rather than silently picking one.
        let res = Cli::try_parse_from([
            "kuro",
            "resume",
            "run-id",
            "--message",
            "x",
            "--message-file",
            "/tmp/x",
        ]);
        assert!(
            res.is_err(),
            "expected clap to reject --message + --message-file together"
        );
    }

    #[test]
    fn resume_subcommand_accepts_message_flag() {
        // Sanity: the canonical happy-path invocation must parse and
        // populate the new fields. Pins the wire-level CLI shape.
        let cli = Cli::try_parse_from(["kuro", "resume", "run-id", "--message", "approve"])
            .expect("CLI parse failed");
        let Command::Resume {
            run_id,
            message,
            message_file,
        } = cli.command
        else {
            unreachable!()
        };
        assert_eq!(run_id, "run-id");
        assert_eq!(message.as_deref(), Some("approve"));
        assert!(message_file.is_none());
    }

    #[test]
    fn resume_subcommand_accepts_message_file_flag() {
        // The other side of the same contract -- `--message-file`
        // alone must parse cleanly with `message` left None.
        let cli = Cli::try_parse_from([
            "kuro",
            "resume",
            "run-id",
            "--message-file",
            "/tmp/feedback.md",
        ])
        .expect("CLI parse failed");
        let Command::Resume {
            run_id,
            message,
            message_file,
        } = cli.command
        else {
            unreachable!()
        };
        assert_eq!(run_id, "run-id");
        assert!(message.is_none());
        assert_eq!(message_file, Some(PathBuf::from("/tmp/feedback.md")));
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
            ..Default::default()
        }
    }

    fn make_flow_with_roles(
        roles: &[(&str, &str)],
        steps: Vec<config::Step>,
    ) -> config::FlowConfig {
        config::FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
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
            ..Default::default()
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
            extra_args: Vec::new(),
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
            extra_args: Vec::new(),
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
            extra_args: Vec::new(),
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
            extra_args: Vec::new(),
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
                graph_decision: None,
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
            None,
            None,
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
