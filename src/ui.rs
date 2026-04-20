//! Terminal UI output module.
//!
//! Implements the visual language from the koto terminal design mockups:
//! - Semantic color system (tokyonight-inspired dark theme)
//! - Stage pills, banners, and summary tables
//! - Streaming output with spinners
//! - Error diagnostics with source pointers
//!
//! All output goes to stdout using ANSI escape codes via crossterm.
//! This is the foundation for a future ratatui-based TUI.

use crossterm::style::{self, Attribute, Color, Stylize};

use crate::config::Backend;

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

// --- Stage state ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StageState {
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

// --- Stage pill info ---

pub struct StagePill {
    pub label: String,
    pub state: StageState,
}

// --- Output functions ---

/// Print the flow header: "koto flow <name> (<file>)"
pub fn print_flow_start(name: &str, file: &str, stage_count: usize, agent_count: usize) {
    let t = &DARK;
    println!(
        "  {} loaded flow {} ({})  ({} stages, {} agents)",
        style::style("✓").with(t.green),
        style::style(name).with(t.fg),
        style::style(file).with(t.dim),
        stage_count,
        agent_count,
    );
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

/// Print the stage pills row: "✓ design  ─  ● implement  ─  ○ review"
pub fn print_stage_pills(stages: &[StagePill]) {
    let t = &DARK;
    let mut out = String::new();
    for (i, s) in stages.iter().enumerate() {
        let color = s.state.color(t);
        out.push_str(&format!(
            "{} {}",
            style::style(s.state.marker()).with(color),
            style::style(&s.label).with(color),
        ));
        if i < stages.len() - 1 {
            out.push_str(&format!("  {}  ", style::style("─").with(t.muted)));
        }
    }
    println!("{out}");
}

/// Print a stage banner: "── ▶ Stage 1/2  design  ──  architect · Software architect"
pub fn print_stage_banner(n: usize, total: usize, stage: &StageInfo) {
    let t = &DARK;
    let (marker, color) = match stage.state {
        StageState::Done => ("✓", t.green),
        StageState::Running => ("▶", t.cyan),
        StageState::Failed => ("✗", t.red),
        _ => ("·", t.fg),
    };
    println!();
    println!(
        "  {}  {} Stage {}/{}  {}  {}  {}{}",
        style::style("──").with(t.muted),
        style::style(marker).with(color).attribute(Attribute::Bold),
        n,
        total,
        style::style(&stage.id)
            .with(t.fg)
            .attribute(Attribute::Bold),
        style::style("──").with(t.muted),
        style::style(&stage.agent).with(t.magenta),
        format_args!(
            " {} {}",
            style::style("·").with(t.dim),
            style::style(&stage.role).with(t.dim),
        ),
    );
    println!(
        "      {} {}   {} {}{}   {} {}",
        style::style("model").with(t.dim),
        style::style(&stage.model).with(t.yellow),
        style::style("backend").with(t.dim),
        style::style(stage.backend_name()).with(t.blue),
        if !stage.input.is_empty() {
            format!(
                "   {} [{}]",
                style::style("input").with(t.dim),
                stage.input.join(", "),
            )
        } else {
            String::new()
        },
        style::style("output").with(t.dim),
        style::style(&stage.output).with(t.fg),
    );
}

/// Print "thinking" status with spinner placeholder (static for now).
pub fn print_thinking(task_description: &str) {
    let t = &DARK;
    println!(
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
        style::style("──────────────────���──────────────────────────────────────").with(t.muted),
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
        style::style("└─��───────────────────────────────────────────���────────────────────")
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

/// Print stage completion line.
pub fn print_stage_done(duration: &str, tokens_in: &str, tokens_out: &str, output: &str) {
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

/// Print context injection line (handoff between stages).
pub fn print_context_injection(from_stage: &str, from_file: &str, tokens: &str) {
    let t = &DARK;
    println!(
        "      {} {}",
        style::style("→").with(t.cyan),
        style::style("injecting context").with(t.cyan),
    );
    println!(
        "        {} {} ({})  {} {} → prompt",
        style::style("from").with(t.dim),
        style::style(from_stage).with(t.green),
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

pub struct StageResult {
    pub id: String,
    pub agent: String,
    pub backend: String,
    pub duration: String,
    pub tokens_in: String,
    pub tokens_out: String,
    pub output: String,
    pub state: StageState,
}

/// Print the flow complete summary table.
pub fn print_flow_complete(
    stages: &[StageResult],
    total_elapsed: &str,
    total_in: &str,
    total_out: &str,
    cost: &str,
    run_dir: &str,
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
        style::style(format!("{} stages, 0 failures", stages.len())).with(t.dim),
    );
    println!();

    // Table header
    println!(
        "      {}",
        style::style(
            "Stage            Agent         Backend       Duration      Tokens (in/out)     Output"
        )
        .with(t.dim),
    );
    println!(
        "      {}",
        style::style("────────────────────���─────────────────────────────────────────────────────────────────").with(t.muted),
    );

    for s in stages {
        let state_marker = match s.state {
            StageState::Done => style::style("✓").with(t.green).to_string(),
            StageState::Failed => style::style("✗").with(t.red).to_string(),
            _ => style::style("○").with(t.muted).to_string(),
        };
        println!(
            "      {} {:<14}{:<14}{:<14}{:<14}{} / {:<12}{}",
            state_marker,
            style::style(&s.id).with(t.fg),
            style::style(&s.agent).with(t.magenta),
            style::style(&s.backend).with(t.blue),
            &s.duration,
            &s.tokens_in,
            &s.tokens_out,
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

    // Artifacts tree
    println!();
    println!(
        "      {} {}",
        style::style("Artifacts written to").with(t.dim),
        style::style(run_dir).with(t.fg),
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

/// Print a stage failure with rust-style diagnostic.
pub fn print_stage_failed(duration: &str, retries: u32) {
    let t = &DARK;
    println!(
        "      {}{}",
        style::style("✗").with(t.red).attribute(Attribute::Bold),
        style::style(" stage failed")
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

/// Print the koto status table header.
pub fn print_status_header(run_id: &str, flow_name: &str, status: StageState) {
    let t = &DARK;
    let status_str = match status {
        StageState::Done => style::style("complete")
            .with(t.green)
            .attribute(Attribute::Bold),
        StageState::Failed => style::style("failed")
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

/// Print the status table with all stages.
pub fn print_status_table(stages: &[StageResult]) {
    let t = &DARK;
    println!();
    println!(
        "    {}",
        style::style(
            "STAGE        STATE     AGENT       BACKEND      DURATION   TOKENS          OUTPUT"
        )
        .with(t.muted),
    );
    println!(
        "    {}",
        style::style("─────────────────────────────────────────────────────────────���──────────────────────────").with(t.muted),
    );

    for s in stages {
        let state_str = match s.state {
            StageState::Done => style::style("done").with(t.green).to_string(),
            StageState::Failed => style::style("failed").with(t.red).to_string(),
            StageState::Skipped => style::style("skipped").with(t.muted).to_string(),
            StageState::Running => style::style("running").with(t.cyan).to_string(),
            StageState::Pending => style::style("pending").with(t.muted).to_string(),
        };
        let dim = matches!(s.state, StageState::Skipped | StageState::Pending);
        let id_style = if dim { t.dim } else { t.fg };
        let agent_color = if dim { t.dim } else { t.magenta };
        let backend_color = if dim { t.dim } else { t.blue };

        println!(
            "    {:<13}{:<10}{:<12}{:<13}{:<11}{:<16}{}",
            style::style(&s.id).with(id_style),
            state_str,
            style::style(&s.agent).with(agent_color),
            style::style(&s.backend).with(backend_color),
            if dim {
                style::style("—").with(t.dim).to_string()
            } else {
                s.duration.clone()
            },
            if dim {
                style::style("—").with(t.dim).to_string()
            } else {
                format!("{} / {}", s.tokens_in, s.tokens_out)
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

pub struct StageInfo {
    pub id: String,
    pub agent: String,
    pub role: String,
    pub model: String,
    pub backend: Backend,
    pub input: Vec<String>,
    pub output: String,
    pub state: StageState,
}

impl StageInfo {
    fn backend_name(&self) -> &str {
        match self.backend {
            Backend::Api => "api",
            Backend::ClaudeCli => "claude-cli",
            Backend::Ollama => "ollama",
        }
    }
}

/// Print the initial "$ koto up <file>" command echo.
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
    fn stage_state_markers() {
        assert_eq!(StageState::Done.marker(), "✓");
        assert_eq!(StageState::Running.marker(), "●");
        assert_eq!(StageState::Failed.marker(), "✗");
        assert_eq!(StageState::Pending.marker(), "○");
    }

    #[test]
    fn stage_info_backend_name() {
        let info = StageInfo {
            id: "test".into(),
            agent: "dev".into(),
            role: "developer".into(),
            model: "sonnet".into(),
            backend: Backend::ClaudeCli,
            input: vec![],
            output: "out.md".into(),
            state: StageState::Running,
        };
        assert_eq!(info.backend_name(), "claude-cli");
    }

    #[test]
    fn stage_pills_smoke() {
        // Just verify it doesn't panic
        let pills = vec![
            StagePill {
                label: "design".into(),
                state: StageState::Done,
            },
            StagePill {
                label: "impl".into(),
                state: StageState::Running,
            },
            StagePill {
                label: "review".into(),
                state: StageState::Pending,
            },
        ];
        print_stage_pills(&pills);
    }
}
