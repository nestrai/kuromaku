//! GitHub comment sink. Wraps the `gh` CLI for posting PR or issue comments.
//!
//! Kept in its own module so the runner stays transport-agnostic -- adding a
//! Slack or webhook sink later is a sibling module, not a runner change.
//!
//! Also exposes [`fetch_issue_summary`] (issue #309) so the runner can show a
//! context banner with the issue title, URL and a body preview at the start of
//! `implement-issue` / `rework-pr` runs. The fetch reuses the same `gh`-CLI
//! pattern as [`post_comment`] and is silent on failure: the banner is a
//! convenience, not part of the flow contract.

use std::collections::HashMap;

use crate::config::PostCommentTarget;

/// Function-shaped sink: takes the target, the PR/issue number and the body,
/// returns Ok on success or a stringly-typed error otherwise. Boxed so the
/// runner can swap in the default `gh`-CLI implementation in production and
/// a synchronous closure (success or failure) in tests.
pub type Poster = Box<dyn Fn(PostCommentTarget, &str, &str) -> Result<(), String> + Send + Sync>;

/// The production poster: shells out to `gh` via [`post_comment`].
pub fn gh_poster() -> Poster {
    Box::new(post_comment)
}

/// Outcome of a single attempt to post a step's output as a comment. The
/// runner translates these into user-facing log lines; the tests assert on
/// the variant directly so we never have to scrape stderr.
#[derive(Debug)]
pub enum PostOutcome {
    /// Comment was successfully posted to the given target.
    Posted { kind: &'static str, number: String },
    /// Step asked for a comment but no usable `id` template var was provided.
    /// Empty strings (`--var id=`) and missing keys both land here.
    NoIdProvided,
    /// The poster ran but returned an error. The flow continues -- gh
    /// outages must not undo work already written to the stack.
    Failed { error: String },
}

/// Build a comment from a step output and post it via `poster`. Pure logic --
/// no I/O happens here unless the poster does some -- so the soft-fail
/// contract ("gh errors warn but never abort the flow") is testable without
/// spinning up the executor.
pub fn try_post_step_comment(
    target: PostCommentTarget,
    agent_name: &str,
    content: &str,
    input_agents: &[&str],
    template_vars: &HashMap<String, String>,
    poster: &Poster,
) -> PostOutcome {
    // Empty strings (`--var id=`) collapse to NoIdProvided so the user sees
    // the friendly warning instead of a cryptic "gh: invalid number" failure.
    let Some(number) = template_vars
        .get("id")
        .map(String::as_str)
        .filter(|s| !s.is_empty())
    else {
        return PostOutcome::NoIdProvided;
    };

    let body = build_comment_body(content, agent_name, input_agents);
    match poster(target, number, &body) {
        Ok(()) => {
            let kind = match target {
                PostCommentTarget::Pr => "PR",
                PostCommentTarget::Issue => "issue",
            };
            PostOutcome::Posted {
                kind,
                number: number.to_string(),
            }
        }
        Err(error) => PostOutcome::Failed { error },
    }
}

/// Format a list of agent names as English ("Bella", "Bella and Levi",
/// "Bella, Levi, and Tom"). Returns an empty string for an empty slice.
fn join_agent_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        rest => {
            let last = rest.last().unwrap();
            let head = rest[..rest.len() - 1].join(", ");
            format!("{head}, and {last}")
        }
    }
}

/// Build the comment body posted to GitHub: a single header line that names
/// the agents involved, followed by the step output verbatim.
///
/// Header format:
/// - With input agents: `Review by <inputs>, consensus by <current>`
/// - Without input agents: `Comment by <current>`
pub fn build_comment_body(output: &str, current_agent: &str, input_agents: &[&str]) -> String {
    let header = if input_agents.is_empty() {
        format!("Comment by {current_agent}")
    } else {
        format!(
            "Review by {}, consensus by {current_agent}",
            join_agent_names(input_agents)
        )
    };
    format!("{header}\n\n---\n\n{output}")
}

/// Post a comment on a PR or issue via the `gh` CLI.
///
/// Uses `gh pr comment <num> --body-file -` (or `gh issue comment ...`) and
/// pipes the body on stdin so we never have to escape large markdown bodies on
/// the command line. Returns a stringly-typed error so the runner can print a
/// warning and continue -- the issue spec is explicit that gh failures must
/// not fail the flow.
pub fn post_comment(
    target: PostCommentTarget,
    number: &str,
    body: &str,
) -> std::result::Result<(), String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let subcommand = match target {
        PostCommentTarget::Pr => "pr",
        PostCommentTarget::Issue => "issue",
    };

    let mut child = Command::new("gh")
        .arg(subcommand)
        .arg("comment")
        .arg(number)
        .arg("--body-file")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn gh: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body.as_bytes())
            .map_err(|e| format!("failed to write body to gh stdin: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for gh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "gh exited with status {}: {stderr}",
            output
                .status
                .code()
                .map_or("?".to_string(), |c| c.to_string()),
        ));
    }

    Ok(())
}

/// Summary of a GitHub issue for the run-start banner (issue #309).
///
/// Owned strings keep this trivially `Clone`-able and free the renderer from
/// any borrow on the fetch path. `body_preview` has already been truncated to
/// at most 3 non-empty lines on the producer side so the renderer stays a
/// pure layout function.
#[derive(Debug, Clone)]
pub struct IssueSummary {
    pub id: u64,
    pub title: String,
    pub url: String,
    pub body_preview: String,
}

/// Number of body lines the banner will show. The fetch path applies the
/// truncation in Rust (rather than via `--jq`) so we stay robust against
/// missing or null fields in the JSON.
const ISSUE_BODY_PREVIEW_LINES: usize = 3;

/// Fetch a [`IssueSummary`] via `gh issue view <id> --json title,url,body`.
///
/// Returns `None` on any failure -- `gh` not on PATH, non-zero exit (e.g. not
/// in a github repo, network issue, unknown issue), JSON parse error, or
/// missing fields. The banner is opportunistic: a failed fetch must not
/// produce noise on flows that don't follow the issue convention. Any logging
/// would defeat that contract.
pub fn fetch_issue_summary(id: u64) -> Option<IssueSummary> {
    use std::process::{Command, Stdio};

    let output = Command::new("gh")
        .arg("issue")
        .arg("view")
        .arg(id.to_string())
        .arg("--json")
        .arg("title,url,body")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let title = json.get("title")?.as_str()?.to_string();
    let url = json.get("url")?.as_str()?.to_string();
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let body_preview = truncate_body_preview(body, ISSUE_BODY_PREVIEW_LINES);

    Some(IssueSummary {
        id,
        title,
        url,
        body_preview,
    })
}

/// Fetch the full body of a GitHub issue via `gh issue view <id> --json body`.
///
/// Returns `None` on any failure -- `gh` not on PATH, non-zero exit (no
/// repo context, network issue, unknown issue), JSON parse error, or
/// missing body field. Distinct from [`fetch_issue_summary`] in that it
/// returns the full body untouched: the body-hash snapshot recorded into
/// a paused run's manifest (issue #337) needs to hash the exact bytes
/// `kuro resume` (#338) will compare against, not a 3-line preview.
///
/// Stays best-effort: a paused run with no `vars["id"]`, no `gh`
/// installed, or a network blip simply skips the body-hash field rather
/// than failing the pause -- the manifest's `status: paused` and
/// `paused_at_state` are the contract; the hash is a future drift-
/// detection convenience.
pub fn fetch_issue_body(id: u64) -> Option<String> {
    use std::process::{Command, Stdio};

    let output = Command::new("gh")
        .arg("issue")
        .arg("view")
        .arg(id.to_string())
        .arg("--json")
        .arg("body")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("body")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Take the first `max_lines` non-empty lines and join them with `\n`.
///
/// Issue bodies tend to start with a blank line or a heading; skipping empty
/// lines means the preview shows actual content even when the markdown puts
/// whitespace at the top. Trimmed `\r` keeps Windows line endings from
/// looking ragged inside a coloured banner.
fn truncate_body_preview(body: &str, max_lines: usize) -> String {
    body.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_agent_names_handles_arity() {
        assert_eq!(join_agent_names(&[]), "");
        assert_eq!(join_agent_names(&["Mika"]), "Mika");
        assert_eq!(join_agent_names(&["Bella", "Levi"]), "Bella and Levi");
        assert_eq!(
            join_agent_names(&["Bella", "Levi", "Tom"]),
            "Bella, Levi, and Tom"
        );
    }

    #[test]
    fn build_comment_body_with_inputs_uses_consensus_header() {
        let body = build_comment_body("VERDICT: APPROVE", "Mika", &["Bella", "Levi"]);
        assert!(
            body.starts_with("Review by Bella and Levi, consensus by Mika\n\n---\n\n"),
            "got: {body}"
        );
        assert!(body.ends_with("VERDICT: APPROVE"));
    }

    #[test]
    fn build_comment_body_without_inputs_uses_comment_header() {
        let body = build_comment_body("Some output", "Bella", &[]);
        assert_eq!(body, "Comment by Bella\n\n---\n\nSome output");
    }

    #[test]
    fn build_comment_body_preserves_markdown_verbatim() {
        // Stack output is markdown; the body must pass it through untouched
        // so reviewers see the same rendering on GitHub as in the local file.
        let raw = "### Heading\n\n- item 1\n- item 2\n\n```rust\nfn x() {}\n```";
        let body = build_comment_body(raw, "Mika", &["Bella"]);
        assert!(body.contains(raw), "raw markdown not present in body");
    }

    use std::sync::{Arc, Mutex};

    fn vars_with_id(id: &str) -> HashMap<String, String> {
        let mut v = HashMap::new();
        v.insert("id".to_string(), id.to_string());
        v
    }

    #[test]
    fn try_post_step_comment_returns_failed_when_poster_errors() {
        // Acceptance criterion: gh errors are reported but never abort the
        // flow. We assert the outcome is `Failed`, not a panic, and not an
        // `Err` propagated up the call stack.
        let poster: Poster = Box::new(|_, _, _| Err("simulated gh failure".to_string()));
        let outcome = try_post_step_comment(
            PostCommentTarget::Pr,
            "Bella",
            "review body",
            &[],
            &vars_with_id("139"),
            &poster,
        );
        match outcome {
            PostOutcome::Failed { error } => assert!(error.contains("simulated gh failure")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn try_post_step_comment_returns_no_id_when_id_empty() {
        // `--var id=` resolves to an empty string. Without the filter this
        // would have called `gh pr comment ""` and produced a cryptic error
        // instead of the friendly "no id template var" warning.
        let poster: Poster = Box::new(|_, _, _| panic!("poster must not be called"));
        let outcome = try_post_step_comment(
            PostCommentTarget::Pr,
            "Bella",
            "out",
            &[],
            &vars_with_id(""),
            &poster,
        );
        assert!(matches!(outcome, PostOutcome::NoIdProvided));
    }

    #[test]
    fn try_post_step_comment_returns_no_id_when_id_missing() {
        let poster: Poster = Box::new(|_, _, _| panic!("poster must not be called"));
        let outcome = try_post_step_comment(
            PostCommentTarget::Pr,
            "Bella",
            "out",
            &[],
            &HashMap::new(),
            &poster,
        );
        assert!(matches!(outcome, PostOutcome::NoIdProvided));
    }

    #[test]
    fn try_post_step_comment_posts_pr_with_consensus_header() {
        // Capture what the poster receives so we can assert the body and
        // target the runner would have sent to `gh`.
        type Captured = Arc<Mutex<Option<(PostCommentTarget, String, String)>>>;
        let captured: Captured = Arc::new(Mutex::new(None));
        let cap_clone = Arc::clone(&captured);
        let poster: Poster = Box::new(move |t, n, b| {
            *cap_clone.lock().unwrap() = Some((t, n.to_string(), b.to_string()));
            Ok(())
        });

        let outcome = try_post_step_comment(
            PostCommentTarget::Pr,
            "Mika",
            "VERDICT: APPROVE",
            &["Bella", "Levi"],
            &vars_with_id("139"),
            &poster,
        );
        match outcome {
            PostOutcome::Posted { kind, number } => {
                assert_eq!(kind, "PR");
                assert_eq!(number, "139");
            }
            other => panic!("expected Posted, got {other:?}"),
        }
        let (target, num, body) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(target, PostCommentTarget::Pr);
        assert_eq!(num, "139");
        assert!(body.contains("Review by Bella and Levi, consensus by Mika"));
        assert!(body.ends_with("VERDICT: APPROVE"));
    }

    #[test]
    fn try_post_step_comment_uses_issue_kind_for_issue_target() {
        let poster: Poster = Box::new(|_, _, _| Ok(()));
        let outcome = try_post_step_comment(
            PostCommentTarget::Issue,
            "Bella",
            "out",
            &[],
            &vars_with_id("42"),
            &poster,
        );
        match outcome {
            PostOutcome::Posted { kind, number } => {
                assert_eq!(kind, "issue");
                assert_eq!(number, "42");
            }
            other => panic!("expected Posted, got {other:?}"),
        }
    }

    #[test]
    fn truncate_body_preview_skips_blank_lines_and_trims() {
        // Issue bodies typically start with a blank line; the preview must
        // surface actual content rather than echo whitespace.
        let body = "\n\n## Why\n\nThe runner needs a banner\n\nMore detail\nfourth line\n";
        let preview = truncate_body_preview(body, 3);
        assert_eq!(preview, "## Why\nThe runner needs a banner\nMore detail");
    }

    #[test]
    fn truncate_body_preview_handles_short_bodies() {
        assert_eq!(truncate_body_preview("", 3), "");
        assert_eq!(truncate_body_preview("only one\n", 3), "only one");
        assert_eq!(truncate_body_preview("a\nb\n", 3), "a\nb");
    }

    #[test]
    fn truncate_body_preview_strips_trailing_cr() {
        // CRLF input should not leave stray `\r` in the rendered banner.
        let body = "first\r\nsecond\r\n";
        let preview = truncate_body_preview(body, 3);
        assert_eq!(preview, "first\nsecond");
    }
}
