//! Terminal UI output module.
//!
//! Implements the visual language from the kuromaku terminal design mockups:
//! - Semantic color system (tokyonight-inspired dark theme)
//! - Step pills, banners, and summary tables
//! - Streaming output with spinners
//! - Error diagnostics with source pointers
//!
//! All output goes to stdout using ANSI escape codes via crossterm.
//! This is the foundation for a future ratatui-based TUI.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crossterm::style::{self, Attribute, Color, Stylize};

use crate::config::Backend;
use crate::notify::github::IssueSummary;

// --- Theme ---

/// Semantic terminal colors matching the design's tokyonight-ish palette.
/// These map to the user's actual terminal colors when possible.
pub struct Theme {
    pub fg: Color,
    pub dim: Color,
    pub muted: Color,
    pub cyan: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub magenta: Color,
    pub blue: Color,
}

pub const DARK: Theme = Theme {
    fg: Color::Reset,
    dim: Color::DarkGrey,
    muted: Color::DarkGrey,
    cyan: Color::Cyan,
    green: Color::Green,
    yellow: Color::Yellow,
    red: Color::Red,
    magenta: Color::Magenta,
    blue: Color::Blue,
};

// --- Step state ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepState {
    fn marker(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Running => "●",
            Self::Failed => "✗",
            Self::Pending => "○",
            Self::Skipped => "○",
        }
    }

    fn color(self, t: &Theme) -> Color {
        match self {
            Self::Done => t.green,
            Self::Running => t.cyan,
            Self::Failed => t.red,
            Self::Pending => t.muted,
            Self::Skipped => t.muted,
        }
    }
}

// --- Step pill info ---

pub struct StepPill {
    pub label: String,
    pub state: StepState,
}

// --- Output functions ---

/// Print the flow header: "✓ loaded flow <name> (<file>)  (<n> steps, <m> agents)"
pub fn print_flow_start(name: &str, file: &str, step_count: usize, agent_count: usize) {
    let t = &DARK;
    println!(
        "  {} loaded flow {} ({})  ({} steps, {} agents)",
        style::style("✓").with(t.green),
        style::style(name).with(t.fg),
        style::style(file).with(t.dim),
        step_count,
        agent_count,
    );
}

/// Print the issue context banner shown after `print_flow_start` when a flow
/// is launched with `--var id=<n>` and `gh` succeeds (issue #309).
///
/// Layout mirrors the indent and rule glyphs of [`print_step_banner`] so the
/// banner reads as a sibling header rather than a step. The body preview
/// renders dim and indented; an empty preview suppresses the body line
/// entirely so flows with empty issue bodies don't show a stray blank row.
pub fn print_issue_banner(summary: &IssueSummary) {
    let t = &DARK;
    let marker = style::style("▶").with(t.cyan).attribute(Attribute::Bold);
    println!();
    println!(
        "  {}  {} {}  {}",
        style::style("──").with(t.muted),
        marker,
        style::style(format!("Issue #{}", summary.id))
            .with(t.fg)
            .attribute(Attribute::Bold),
        style::style(&summary.title).with(t.fg),
    );
    println!("       {}", style::style(&summary.url).with(t.dim),);
    if !summary.body_preview.is_empty() {
        for line in summary.body_preview.lines() {
            println!("       {}", style::style(line).with(t.dim));
        }
    }
}

/// Print backend resolution: "✓ resolved backends [api] ok [claude-cli v0.8.2] ok"
pub fn print_backends_ok(backends: &[(&str, &str)]) {
    let t = &DARK;
    let mut parts = String::new();
    for (name, version) in backends {
        parts.push_str(&format!(
            " [{}] {}",
            style::style(name).with(t.dim),
            style::style("ok").with(t.green),
        ));
        if !version.is_empty() {
            parts.push_str(&format!(" {}", style::style(version).with(t.dim)));
        }
    }
    println!(
        "  {} resolved backends{}",
        style::style("✓").with(t.green),
        parts,
    );
}

/// Print the step pills row: "✓ design  -  ● implement  -  ○ review"
pub fn print_step_pills(steps: &[StepPill]) {
    let t = &DARK;
    let mut out = String::new();
    for (i, s) in steps.iter().enumerate() {
        let color = s.state.color(t);
        out.push_str(&format!(
            "{} {}",
            style::style(s.state.marker()).with(color),
            style::style(&s.label).with(color),
        ));
        if i < steps.len() - 1 {
            out.push_str(&format!("  {}  ", style::style("─").with(t.muted)));
        }
    }
    println!("{out}");
}

/// Print a step banner:
/// ```text
///   -- > Step 1/3  design -- Levi
///      model claude-sonnet-4-5  backend claude-cli
///      task "Implement the feature"
/// ```
/// Print the step banner for a shell step (issue #23).
///
/// Mirrors [`print_step_banner`] but shows the rendered command instead of
/// model/backend metadata. Long commands are truncated for the banner; the
/// full text is still written to the stack via the `prompt` field.
pub fn print_shell_step_banner(
    n: usize,
    total: usize,
    step_id: &str,
    command: &str,
    input: &[String],
) {
    let t = &DARK;
    let marker = style::style("▶").with(t.cyan).attribute(Attribute::Bold);
    println!();
    println!(
        "  {}  {} Step {}/{}  {}  {}  {}",
        style::style("──").with(t.muted),
        marker,
        n,
        total,
        style::style(step_id).with(t.fg).attribute(Attribute::Bold),
        style::style("──").with(t.muted),
        style::style("shell").with(t.magenta),
    );

    // Truncate the command to keep the banner readable; the full command is
    // preserved in the saved StepOutput. Slice on a char boundary so commands
    // containing multi-byte UTF-8 (emoji, non-ASCII paths) cannot panic the
    // banner print on a misaligned byte index.
    const MAX_LEN: usize = 80;
    let display_cmd = if command.chars().count() > MAX_LEN {
        let cut = command
            .char_indices()
            .nth(MAX_LEN)
            .map(|(i, _)| i)
            .unwrap_or(command.len());
        format!("{}…", &command[..cut])
    } else {
        command.to_string()
    };
    let mut meta = format!(
        "      {} {}",
        style::style("$").with(t.dim),
        style::style(display_cmd).with(t.yellow),
    );
    if !input.is_empty() {
        meta.push_str(&format!(
            "   {} [{}]",
            style::style("input").with(t.dim),
            input.join(", "),
        ));
    }
    println!("{meta}");
}

/// Banner for a conversation step (issue #170).
///
/// Mirrors [`print_shell_step_banner`] but shows the participant list and
/// labels the kind as `conversation`. The participants are surfaced eagerly
/// so the user can spot a misconfigured agents list before the underlying
/// CLIs are spawned.
pub fn print_conversation_step_banner(
    n: usize,
    total: usize,
    step_id: &str,
    agents: &[String],
    input: &[String],
) {
    let t = &DARK;
    let marker = style::style("▶").with(t.cyan).attribute(Attribute::Bold);
    println!();
    println!(
        "  {}  {} Step {}/{}  {}  {}  {}",
        style::style("──").with(t.muted),
        marker,
        n,
        total,
        style::style(step_id).with(t.fg).attribute(Attribute::Bold),
        style::style("──").with(t.muted),
        style::style("conversation").with(t.magenta),
    );

    let mut meta = format!(
        "      {} {}",
        style::style("agents").with(t.dim),
        style::style(agents.join(", ")).with(t.yellow),
    );
    if !input.is_empty() {
        meta.push_str(&format!(
            "   {} [{}]",
            style::style("input").with(t.dim),
            input.join(", "),
        ));
    }
    println!("{meta}");
}

pub fn print_step_banner(n: usize, total: usize, step: &StepInfo) {
    let t = &DARK;
    let (marker, color) = match step.state {
        StepState::Done => ("✓", t.green),
        StepState::Running => ("▶", t.cyan),
        StepState::Failed => ("✗", t.red),
        _ => ("·", t.fg),
    };
    println!();
    println!(
        "  {}  {} Step {}/{}  {}  {}  {}{}",
        style::style("──").with(t.muted),
        style::style(marker).with(color).attribute(Attribute::Bold),
        n,
        total,
        style::style(&step.id).with(t.fg).attribute(Attribute::Bold),
        style::style("──").with(t.muted),
        style::style(&step.agent).with(t.magenta),
        step.title.as_deref().map_or(String::new(), |t_str| {
            format!("  {}", style::style(t_str).with(t.dim))
        }),
    );
    let mut meta = format!(
        "      {} {}   {} {}",
        style::style("model").with(t.dim),
        style::style(&step.model).with(t.yellow),
        style::style("backend").with(t.dim),
        style::style(step.backend_name()).with(t.blue),
    );
    if !step.input.is_empty() {
        meta.push_str(&format!(
            "   {} [{}]",
            style::style("input").with(t.dim),
            step.input.join(", "),
        ));
    }
    println!("{meta}");
}

/// Start an animated spinner that shows elapsed time.
/// Returns a handle that stops the spinner when dropped or `.stop()` is called.
pub fn start_spinner() -> SpinnerHandle {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let start = Instant::now();

    let handle = std::thread::spawn(move || {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let t = &DARK;
        let mut i = 0;
        while running_clone.load(Ordering::Relaxed) {
            let elapsed = start.elapsed().as_secs();
            let time_str = if elapsed >= 60 {
                format!("{}m{:02}s", elapsed / 60, elapsed % 60)
            } else {
                format!("{elapsed}s")
            };
            eprint!(
                "\x1b[2K\r      {} {} {}",
                style::style(frames[i % frames.len()]).with(t.cyan),
                style::style("flowing").with(t.cyan),
                style::style(&time_str).with(t.dim),
            );
            let _ = std::io::stderr().flush();
            i += 1;
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        // Clear the spinner line
        eprint!("\x1b[2K\r");
        let _ = std::io::stderr().flush();
    });

    SpinnerHandle {
        running,
        thread: Some(handle),
    }
}

pub struct SpinnerHandle {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SpinnerHandle {
    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SpinnerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Print "thinking" status (static, used when spinner is not needed).
pub fn print_thinking(task_description: &str) {
    let t = &DARK;
    eprintln!(
        "      {} {}   {} \"{}\"",
        style::style("⠋").with(t.cyan),
        style::style("thinking").with(t.cyan),
        style::style("task").with(t.dim),
        style::style(task_description).with(t.fg),
    );
}

/// Print reasoning block (the thinking/reasoning lines).
pub fn print_reasoning_block(lines: &[&str]) {
    let t = &DARK;
    println!();
    println!(
        "      {} {} {}",
        style::style("┌─").with(t.muted),
        style::style("reasoning").with(t.dim),
        style::style("──────────────────────────────────────────────────").with(t.muted),
    );
    for line in lines {
        println!(
            "      {} {}",
            style::style("│").with(t.muted),
            style::style(line).with(t.dim),
        );
    }
    println!(
        "      {}",
        style::style("└────────────────────────────────────────────────────────────────")
            .with(t.muted),
    );
}

/// Print "writing <file>" with cursor placeholder.
pub fn print_writing(filename: &str) {
    let t = &DARK;
    println!();
    println!(
        "      {} {}",
        style::style("writing").with(t.cyan),
        style::style(filename).with(t.fg),
    );
}

/// Print step completion line.
pub fn print_step_done(duration: &str, tokens_in: &str, tokens_out: &str, output: &str) {
    let t = &DARK;
    println!(
        "      {} {} {}   {} {} in / {} out   {} {}",
        style::style("✓").with(t.green),
        style::style("completed in").with(t.dim),
        style::style(duration).with(t.fg),
        style::style("tokens").with(t.dim),
        tokens_in,
        tokens_out,
        style::style("wrote").with(t.dim),
        style::style(output).with(t.fg),
    );
}

/// Print the chosen transition for a graph state visit.
///
/// Mirrors the visual rhythm of [`print_step_done`] but carries the
/// graph-specific information: the edge name the agent picked, the
/// resolved next state, and the agent's reason. Lives next to the linear
/// helpers because graph runs reuse the rest of this module today --
/// #269 will redesign this once the broader graph UI brief lands.
pub fn print_graph_transition(transition: &str, next_state: &str, reason: &str) {
    let t = &DARK;
    println!(
        "      {} {} {} {} {}",
        style::style("→").with(t.cyan),
        style::style("transition").with(t.dim),
        style::style(transition).with(t.cyan),
        style::style("→").with(t.dim),
        style::style(next_state)
            .with(t.fg)
            .attribute(Attribute::Bold),
    );
    // The reason is dimmed because it is auxiliary -- a user scanning a
    // run wants the picked transition to pop, with the reason available
    // for context but not competing for attention.
    println!(
        "        {} {}",
        style::style("reason").with(t.dim),
        style::style(reason).with(t.dim),
    );
}

/// Print the banner announcing a graph flow has started executing.
///
/// Emitted on stderr at the top of [`runner::graph::run_graph_flow`] so
/// that test harnesses (and humans tailing logs) can confirm execution
/// routed through the graph runtime rather than silently falling back
/// to the linear DAG loader. Stderr is the right stream for run-level
/// banners: stdout is reserved for structured per-step artifacts that
/// callers may want to pipe into other tools. The companion
/// [`print_graph_final`] uses the same stream for symmetry.
pub fn print_graph_run_start(flow_name: &str) {
    let t = &DARK;
    eprintln!(
        "  {} running graph flow {}",
        style::style("→").with(t.cyan),
        style::style(format!("'{flow_name}'"))
            .with(t.fg)
            .attribute(Attribute::Bold),
    );
}

/// Print the line announcing a graph reached a `kind: final` state.
///
/// Distinct from [`print_flow_complete`] because the graph driver still
/// needs to write the manifest and emit the standard summary table after
/// this; this helper just marks the terminal hop. #269 will fold this
/// into a richer graph-aware summary.
///
/// Output goes to stderr so the marker stays out of any structured
/// stdout artifacts a caller may pipe downstream (matches the
/// stderr discipline of [`print_graph_run_start`]).
pub fn print_graph_final(state_id: &str) {
    let t = &DARK;
    eprintln!();
    eprintln!(
        "      {} {} {}",
        style::style("◆").with(t.green).attribute(Attribute::Bold),
        style::style("reached final state").with(t.dim),
        style::style(state_id)
            .with(t.green)
            .attribute(Attribute::Bold),
    );
}

/// Print context injection line (handoff between steps).
pub fn print_context_injection(from_step: &str, from_file: &str, tokens: &str) {
    let t = &DARK;
    println!(
        "      {} {}",
        style::style("→").with(t.cyan),
        style::style("injecting context").with(t.cyan),
    );
    println!(
        "        {} {} ({})  {} {} → prompt",
        style::style("from").with(t.dim),
        style::style(from_step).with(t.green),
        style::style(from_file).with(t.dim),
        style::style(tokens).with(t.fg),
        style::style("tokens").with(t.dim),
    );
}

/// Print streaming status line.
pub fn print_streaming(backend: &str, pid: Option<u32>) {
    let t = &DARK;
    let pid_str = pid
        .map(|p| format!("   {} {}", style::style("pid").with(t.dim), p))
        .unwrap_or_default();
    println!(
        "      {} {} {} {}{}",
        style::style("⠋").with(t.cyan),
        style::style("streaming").with(t.cyan),
        style::style("via").with(t.dim),
        style::style(backend).with(t.blue),
        pid_str,
    );
}

// --- Flow complete ---

pub struct StepResult {
    pub id: String,
    pub agent: String,
    pub backend: String,
    pub duration: String,
    pub tokens_in: String,
    pub tokens_out: String,
    pub output: String,
    pub state: StepState,
}

/// Print the flow complete summary table.
///
/// Column alignment uses pre-padded plain strings before applying ANSI colors,
/// so invisible escape bytes don't affect visual alignment.
pub fn print_flow_complete(
    steps: &[StepResult],
    total_elapsed: &str,
    total_in: &str,
    total_out: &str,
    cost: &str,
    stack_path: &str,
) {
    let t = &DARK;
    println!();
    println!(
        "      {}{}",
        style::style("✓").with(t.green).attribute(Attribute::Bold),
        style::style(" flow complete")
            .with(t.green)
            .attribute(Attribute::Bold),
    );
    println!(
        "      {}",
        style::style(format!("{} steps, 0 failures", steps.len())).with(t.dim),
    );
    println!();

    // Table header
    println!(
        "      {}",
        style::style(
            "Step             Agent         Backend       Duration      Tokens (in/out)     Output"
        )
        .with(t.dim),
    );
    println!(
        "      {}",
        style::style("─────────────────────────────────────────────────────────────────────────────────────────").with(t.muted),
    );

    // Format plain strings first with padding, then colorize.
    // This avoids ANSI escape codes messing up column widths.
    #[allow(clippy::format_in_format_args)]
    for s in steps {
        let state_marker = match s.state {
            StepState::Done => style::style("✓").with(t.green).to_string(),
            StepState::Failed => style::style("✗").with(t.red).to_string(),
            _ => style::style("○").with(t.muted).to_string(),
        };
        let tokens = format!("{} / {}", s.tokens_in, s.tokens_out);
        println!(
            "      {} {} {} {} {} {} {}",
            state_marker,
            style::style(format!("{:<16}", s.id)).with(t.fg),
            style::style(format!("{:<14}", s.agent)).with(t.magenta),
            style::style(format!("{:<14}", s.backend)).with(t.blue),
            format!("{:<14}", s.duration),
            format!("{:<20}", tokens),
            style::style(&s.output).with(t.cyan),
        );
    }

    println!();
    println!(
        "      {} {} {}   {} {}   {} {}",
        style::style("Totals").with(t.dim),
        style::style("wall").with(t.dim),
        style::style(total_elapsed).with(t.fg),
        style::style("tokens").with(t.dim),
        style::style(format!("{total_in} in / {total_out} out")).with(t.fg),
        style::style("cost").with(t.dim),
        style::style(cost).with(t.yellow),
    );

    // Stack path
    println!();
    println!(
        "      {} {}",
        style::style("Artifacts written to").with(t.dim),
        style::style(stack_path).with(t.fg),
    );
}

/// Print a single artifact in tree format.
pub fn print_artifact(name: &str, detail: &str, is_last: bool) {
    let t = &DARK;
    let prefix = if is_last { "└─" } else { "├─" };
    println!(
        "        {} {} {}",
        style::style(prefix).with(t.dim),
        style::style(name).with(t.fg),
        style::style(detail).with(t.dim),
    );
}

/// Print the "next" hint line.
pub fn print_next_hint(commands: &[(&str, &str)]) {
    let t = &DARK;
    println!();
    let mut out = format!("      {} ", style::style("next ›").with(t.dim));
    for (i, (cmd, _desc)) in commands.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!("   {}   ", style::style("or").with(t.dim)));
        }
        out.push_str(&format!("{}", style::style(cmd).with(t.fg)));
    }
    println!("{out}");
}

// --- Error state ---

/// Print a step failure with rust-style diagnostic.
pub fn print_step_failed(duration: &str, retries: u32) {
    let t = &DARK;
    println!(
        "      {}{}",
        style::style("✗").with(t.red).attribute(Attribute::Bold),
        style::style(" step failed")
            .with(t.red)
            .attribute(Attribute::Bold),
    );
    println!(
        "      {} {}  ({} retries, exponential backoff)",
        style::style("after").with(t.dim),
        style::style(duration).with(t.fg),
        retries,
    );
}

/// Source location for error diagnostics.
pub struct DiagnosticSource<'a> {
    pub file: &'a str,
    pub line: usize,
    pub col: usize,
    pub context_lines: &'a [(usize, &'a str)],
    pub pointer_offset: usize,
    pub pointer_len: usize,
    pub pointer_msg: &'a str,
}

/// Print error diagnostic with source pointer (rust-compiler style).
#[allow(clippy::needless_pass_by_value)]
pub fn print_error_diagnostic(code: &str, message: &str, source: &DiagnosticSource<'_>) {
    let t = &DARK;
    println!();
    println!(
        "      {}[{}]{} {}",
        style::style("error").with(t.red),
        style::style(code).with(t.red).attribute(Attribute::Bold),
        style::style(":").with(t.red),
        message,
    );
    println!(
        "        {} {}:{}:{}",
        style::style("┌─").with(t.muted),
        source.file,
        source.line,
        source.col,
    );
    println!("        {}", style::style("│").with(t.muted));

    for (lineno, content) in source.context_lines {
        println!(
            "     {} {}",
            style::style(format!("{lineno:>3} │")).with(t.muted),
            content,
        );
    }

    // Pointer line
    let spaces = " ".repeat(source.pointer_offset);
    let arrows = "^".repeat(source.pointer_len);
    println!(
        "        {}{}{}",
        style::style("│         ").with(t.muted),
        style::style(format!("{spaces}{arrows}")).with(t.red),
        style::style(format!(" {}", source.pointer_msg)).with(t.red),
    );
}

/// Print error hints (suggested next commands).
pub fn print_error_hints(hints: &[(&str, &str)]) {
    let t = &DARK;
    println!();
    println!(
        "      {}{}",
        style::style("hint")
            .with(t.yellow)
            .attribute(Attribute::Bold),
        style::style(": ").with(t.dim),
    );
    for (label, cmd) in hints {
        println!(
            "            {} {}",
            style::style(format!("• {label:<26}")).with(t.dim),
            style::style(cmd).with(t.fg),
        );
    }
}

// --- Status command ---

/// Print the status table header.
pub fn print_status_header(run_id: &str, flow_name: &str, status: StepState) {
    let t = &DARK;
    let status_str = match status {
        StepState::Done => style::style("complete")
            .with(t.green)
            .attribute(Attribute::Bold),
        StepState::Failed => style::style("failed")
            .with(t.red)
            .attribute(Attribute::Bold),
        _ => style::style("running")
            .with(t.cyan)
            .attribute(Attribute::Bold),
    };
    println!(
        "  {} {}   {}   {} {}   {}   {} {}",
        style::style("run").with(t.dim),
        style::style(run_id).with(t.fg),
        style::style("·").with(t.muted),
        style::style("flow").with(t.dim),
        style::style(flow_name).with(t.fg),
        style::style("·").with(t.muted),
        style::style("status").with(t.dim),
        status_str,
    );
}

/// Print the status table with all steps.
pub fn print_status_table(steps: &[StepResult]) {
    let t = &DARK;
    println!();
    println!(
        "    {}",
        style::style(
            "STEP         STATE     AGENT       BACKEND      DURATION   TOKENS          OUTPUT"
        )
        .with(t.muted),
    );
    println!(
        "    {}",
        style::style(
            "─────────────────────────────────────────────────────────────────────────────────────"
        )
        .with(t.muted),
    );

    for s in steps {
        let state_str = match s.state {
            StepState::Done => style::style("done").with(t.green).to_string(),
            StepState::Failed => style::style("failed").with(t.red).to_string(),
            StepState::Skipped => style::style("skipped").with(t.muted).to_string(),
            StepState::Running => style::style("running").with(t.cyan).to_string(),
            StepState::Pending => style::style("pending").with(t.muted).to_string(),
        };
        let dim = matches!(s.state, StepState::Skipped | StepState::Pending);
        let id_style = if dim { t.dim } else { t.fg };
        let agent_color = if dim { t.dim } else { t.magenta };
        let backend_color = if dim { t.dim } else { t.blue };

        // Pad plain strings first, then colorize
        println!(
            "    {} {} {} {} {} {} {}",
            style::style(format!("{:<13}", s.id)).with(id_style),
            state_str,
            style::style(format!("{:<12}", s.agent)).with(agent_color),
            style::style(format!("{:<13}", s.backend)).with(backend_color),
            if dim {
                format!("{:<11}", style::style("—").with(t.dim))
            } else {
                format!("{:<11}", s.duration)
            },
            if dim {
                format!("{:<16}", style::style("—").with(t.dim))
            } else {
                format!("{:<16}", format!("{} / {}", s.tokens_in, s.tokens_out))
            },
            if dim {
                style::style("—").with(t.dim).to_string()
            } else {
                style::style(&s.output).with(t.cyan).to_string()
            },
        );
    }
}

/// Print the footer line with elapsed, tokens, and backends.
pub fn print_footer(elapsed: &str, tokens_in: &str, tokens_out: &str, backends: &str) {
    let t = &DARK;
    println!();
    println!(
        "  {} {}  │  {} {} in / {} out  │  {} {}",
        style::style("elapsed").with(t.dim),
        elapsed,
        style::style("tokens").with(t.dim),
        tokens_in,
        tokens_out,
        style::style("backends").with(t.dim),
        style::style(backends).with(t.cyan),
    );
}

// --- Helpers ---

pub struct StepInfo {
    pub id: String,
    pub agent: String,
    pub title: Option<String>,
    pub model: String,
    pub backend: Backend,
    pub input: Vec<String>,
    pub state: StepState,
}

impl StepInfo {
    fn backend_name(&self) -> &str {
        match self.backend {
            Backend::Api => "api",
            Backend::ClaudeCli => "claude-cli",
            Backend::Codex => "codex",
            Backend::Ollama => "ollama",
        }
    }
}

/// Print the initial "$ kuro run <file>" command echo.
pub fn print_command(cmd: &str) {
    let t = &DARK;
    println!("  {}", style::style(format!("$ {cmd}")).with(t.dim));
}

/// Print a partial-run saved notice.
pub fn print_run_saved(run_dir: &str, status: &str) {
    let t = &DARK;
    println!();
    println!(
        "      {} ({})",
        style::style(run_dir).with(t.fg),
        style::style(status).with(t.yellow),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_state_markers() {
        assert_eq!(StepState::Done.marker(), "✓");
        assert_eq!(StepState::Running.marker(), "●");
        assert_eq!(StepState::Failed.marker(), "✗");
        assert_eq!(StepState::Pending.marker(), "○");
    }

    #[test]
    fn step_info_backend_name() {
        let info = StepInfo {
            id: "test".into(),
            agent: "dev".into(),
            title: None,
            model: "sonnet".into(),
            backend: Backend::ClaudeCli,
            input: vec![],
            state: StepState::Running,
        };
        assert_eq!(info.backend_name(), "claude-cli");
    }

    #[test]
    fn step_pills_smoke() {
        // Just verify it doesn't panic
        let pills = vec![
            StepPill {
                label: "design".into(),
                state: StepState::Done,
            },
            StepPill {
                label: "impl".into(),
                state: StepState::Running,
            },
            StepPill {
                label: "review".into(),
                state: StepState::Pending,
            },
        ];
        print_step_pills(&pills);
    }
}
