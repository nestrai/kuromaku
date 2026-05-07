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

/// One comment fetched from a GitHub issue, normalised so callers do not
/// have to know the `gh issue view --json comments` JSON shape.
///
/// `created_at` is kept as the verbatim RFC3339 string `gh` emits (UTC, `Z`
/// suffix). The filter in [`fetch_new_comments_since`] parses it via
/// [`chrono::DateTime::parse_from_rfc3339`] rather than string-comparing so
/// a future shape drift -- offsets like `+02:00` instead of `Z` -- does not
/// silently re-route which comments count as "new".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueComment {
    pub author: String,
    pub created_at: String,
    pub body: String,
}

/// Function-shaped fetcher: takes the issue number and returns the full list
/// of comments on that issue (or a stringly-typed error). Boxed so the
/// runner can swap in the default `gh`-CLI implementation in production
/// (issue #340) and a closure (canned comments, simulated failure) in
/// tests, mirroring the [`Poster`] seam.
pub type CommentsFetcher =
    Box<dyn Fn(u64) -> std::result::Result<Vec<IssueComment>, String> + Send + Sync>;

/// The production fetcher: shells out to `gh` via [`fetch_issue_comments`].
pub fn gh_comments_fetcher() -> CommentsFetcher {
    Box::new(fetch_issue_comments)
}

/// Fetch all comments on a GitHub issue via `gh issue view <id> --json comments`.
///
/// Uses the `--json` shape rather than the human `--comments` view because the
/// human renderer drops the machine-readable `createdAt` timestamps that
/// [`fetch_new_comments_since`] needs to filter on. Returns a stringly-typed
/// error so callers can soft-fail without unwrapping a typed error tree --
/// the resume contract for #340 is "any failure on the comment-fetch path
/// degrades to empty input + a warning, never aborts the resume".
///
/// Tolerant JSON parser: missing `author`/`author.login` collapses to an
/// empty author rather than dropping the comment. The only required fields
/// are `body` (silently treated as empty when missing) and `createdAt`
/// (comments without a timestamp are dropped, since the timestamp filter
/// would have nothing to compare).
pub fn fetch_issue_comments(id: u64) -> std::result::Result<Vec<IssueComment>, String> {
    use std::process::{Command, Stdio};

    let output = Command::new("gh")
        .arg("issue")
        .arg("view")
        .arg(id.to_string())
        .arg("--json")
        .arg("comments")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to spawn gh: {e}"))?;

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

    parse_issue_comments_json(&output.stdout)
}

/// Parse the `gh issue view <id> --json comments` JSON shape into a
/// normalised vector. Split out so the JSON-shape tolerance is unit-testable
/// without spawning `gh` -- the network path stays in [`fetch_issue_comments`].
fn parse_issue_comments_json(bytes: &[u8]) -> std::result::Result<Vec<IssueComment>, String> {
    let json: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("failed to parse gh JSON: {e}"))?;
    let arr = json
        .get("comments")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "gh JSON has no `comments` array".to_string())?;

    let mut out = Vec::with_capacity(arr.len());
    for c in arr {
        // `createdAt` is the only hard requirement: dropping a comment with
        // no timestamp is safer than picking an arbitrary fallback that
        // would silently route it past or before the pause.
        let Some(created_at) = c.get("createdAt").and_then(|v| v.as_str()) else {
            continue;
        };
        let body = c
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let author = c
            .get("author")
            .and_then(|v| v.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        out.push(IssueComment {
            author,
            created_at: created_at.to_string(),
            body,
        });
    }
    Ok(out)
}

/// Fetch comments on `id` via `fetcher` and keep only those with
/// `created_at >= paused_at`.
///
/// Soft-fail: any error from `fetcher`, an unparsable `paused_at`, or a
/// comment whose `created_at` is unparsable collapses to an empty Vec plus
/// a `[warn]` line on stderr. The resume contract (#340 acceptance) is
/// explicit that `gh` outages must degrade to "empty prior_context", not
/// abort the resume.
///
/// Comparison is done on parsed [`chrono::DateTime<Utc>`] values rather than
/// string compare. RFC3339 with a `Z` suffix is lexicographically sortable,
/// so for today's `gh` output the two compare modes agree -- but a future
/// `gh` change emitting offsets like `+02:00` would break naive string
/// compare while leaving parsed compare correct. Belt and braces: cheap.
///
/// The `>=` boundary is intentional. The pause timestamp is captured
/// _before_ the human is told to comment; a comment whose `created_at`
/// equals `paused_at` to the second is far more likely to be the human's
/// reply than an older comment that happens to share the timestamp.
pub fn fetch_new_comments_since(
    id: u64,
    paused_at: &str,
    fetcher: &CommentsFetcher,
) -> Vec<IssueComment> {
    let cutoff = match chrono::DateTime::parse_from_rfc3339(paused_at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => {
            eprintln!(
                "[warn] resume: paused_at '{paused_at}' is not RFC3339 ({e}); skipping human-input fetch"
            );
            return Vec::new();
        }
    };

    let comments = match fetcher(id) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[warn] resume: failed to fetch comments for issue #{id} ({e}); continuing without human input"
            );
            return Vec::new();
        }
    };

    let mut filtered: Vec<IssueComment> = comments
        .into_iter()
        .filter(
            |c| match chrono::DateTime::parse_from_rfc3339(&c.created_at) {
                Ok(dt) => dt.with_timezone(&chrono::Utc) >= cutoff,
                Err(_) => false,
            },
        )
        .collect();

    // Sort chronologically so the rendered body always reads top-to-bottom
    // in the order the human typed. `gh` returns comments in chronological
    // order today, but pinning the order here means a future `gh` change
    // does not silently flip the layout the next agent sees.
    filtered.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    filtered
}

/// Render comments as the body that lands on disk and feeds the next
/// agent's `prior_context` (issue #340).
///
/// Empty input collapses to an empty string -- the caller decides whether
/// to skip writing a synthetic step at all rather than persisting a
/// content file with only a header. Format mirrors the framing block
/// [`crate::runner::graph::render_shell_artifact`] uses for shell states
/// (header line + `---` separators + verbatim body) so the on-disk
/// artifact reads the same shape in `kuro show-output` regardless of
/// whether the prior step was a shell command or a human handoff.
pub fn format_human_input(comments: &[IssueComment], paused_at: &str) -> String {
    if comments.is_empty() {
        return String::new();
    }
    let n = comments.len();
    let plural = if n == 1 { "comment" } else { "comments" };
    let mut out = format!("Human input received since pause at {paused_at} ({n} new {plural}):\n");
    for c in comments {
        out.push_str("\n---\n");
        out.push_str(&format!("@{} at {}:\n\n", c.author, c.created_at));
        out.push_str(&c.body);
        if !c.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
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

    // --- Comment fetcher + formatter (issue #340) ---------------------------

    fn comment(author: &str, created_at: &str, body: &str) -> IssueComment {
        IssueComment {
            author: author.to_string(),
            created_at: created_at.to_string(),
            body: body.to_string(),
        }
    }

    fn canned_fetcher(comments: Vec<IssueComment>) -> CommentsFetcher {
        Box::new(move |_id: u64| Ok(comments.clone()))
    }

    #[test]
    fn fetch_new_comments_since_filters_by_paused_at_inclusive() {
        // Acceptance (#340): the boundary is `>= paused_at`. A comment
        // whose timestamp equals the pause moment is far more likely to be
        // the human's reply than a coincidence -- include it. Comments
        // strictly before pause are dropped.
        let fetcher = canned_fetcher(vec![
            comment("alice", "2026-05-07T09:00:00Z", "way before"),
            comment("alice", "2026-05-07T09:59:59Z", "just before"),
            comment("bob", "2026-05-07T10:00:00Z", "right at pause"),
            comment("carol", "2026-05-07T10:00:01Z", "after pause"),
        ]);
        let kept = fetch_new_comments_since(42, "2026-05-07T10:00:00Z", &fetcher);
        let bodies: Vec<&str> = kept.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["right at pause", "after pause"]);
    }

    #[test]
    fn fetch_new_comments_since_returns_empty_when_none_new() {
        // Pause came after every recorded comment -> no human input. Empty
        // is the contract, not an error: the caller decides whether to
        // skip the synthetic step.
        let fetcher = canned_fetcher(vec![
            comment("alice", "2026-05-06T09:00:00Z", "stale"),
            comment("bob", "2026-05-06T10:00:00Z", "also stale"),
        ]);
        let kept = fetch_new_comments_since(42, "2026-05-07T00:00:00Z", &fetcher);
        assert!(kept.is_empty());
    }

    #[test]
    fn fetch_new_comments_since_soft_fails_on_fetcher_error() {
        // Acceptance (#340): network errors fall back to empty
        // prior_context with a warning. We assert "empty Vec, no panic" --
        // the warning lands on stderr, which is the standard side channel.
        let fetcher: CommentsFetcher = Box::new(|_| Err("simulated gh outage".to_string()));
        let kept = fetch_new_comments_since(42, "2026-05-07T10:00:00Z", &fetcher);
        assert!(kept.is_empty());
    }

    #[test]
    fn fetch_new_comments_since_soft_fails_on_unparsable_paused_at() {
        // A corrupt manifest must not abort resume. The fetch path is the
        // single seam where we can quietly degrade -- the rest of the
        // resume pipeline already enforces RFC3339 on `paused_at`, so this
        // belt-and-braces check fires only if a future writer regresses.
        let fetcher = canned_fetcher(vec![comment("alice", "2026-05-07T10:00:00Z", "any")]);
        let kept = fetch_new_comments_since(42, "not-a-date", &fetcher);
        assert!(kept.is_empty());
    }

    #[test]
    fn fetch_new_comments_since_drops_comments_with_unparsable_timestamp() {
        // A `gh` shape drift that emits a non-RFC3339 timestamp must not
        // crash the filter. Drop those comments rather than treating them
        // as "since pause" or "before pause" -- both are guesses.
        let fetcher = canned_fetcher(vec![
            comment("alice", "2026-05-07T11:00:00Z", "valid"),
            comment("bob", "garbage", "will be dropped"),
        ]);
        let kept = fetch_new_comments_since(42, "2026-05-07T10:00:00Z", &fetcher);
        let bodies: Vec<&str> = kept.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["valid"]);
    }

    #[test]
    fn fetch_new_comments_since_returns_chronological_order() {
        // The next agent reads the body top-to-bottom; pinning the order
        // here keeps the layout stable even if `gh` ever returns comments
        // out of order. Input is intentionally shuffled.
        let fetcher = canned_fetcher(vec![
            comment("carol", "2026-05-07T11:00:00Z", "third"),
            comment("alice", "2026-05-07T10:00:00Z", "first"),
            comment("bob", "2026-05-07T10:30:00Z", "second"),
        ]);
        let kept = fetch_new_comments_since(42, "2026-05-07T10:00:00Z", &fetcher);
        let bodies: Vec<&str> = kept.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["first", "second", "third"]);
    }

    #[test]
    fn format_human_input_empty_collapses_to_empty_string() {
        // Caller skips writing a synthetic step on empty -- the format
        // function must reflect that contract by emitting nothing rather
        // than a lonely header.
        let s = format_human_input(&[], "2026-05-07T10:00:00Z");
        assert_eq!(s, "");
    }

    #[test]
    fn format_human_input_renders_singular_header_for_one_comment() {
        let s = format_human_input(
            &[comment("alice", "2026-05-07T10:30:00Z", "looks good")],
            "2026-05-07T10:00:00Z",
        );
        assert!(s.contains("Human input received since pause at 2026-05-07T10:00:00Z"));
        assert!(s.contains("(1 new comment)"));
        assert!(s.contains("@alice at 2026-05-07T10:30:00Z:"));
        assert!(s.contains("looks good"));
    }

    #[test]
    fn format_human_input_renders_plural_header_for_multiple_comments() {
        let s = format_human_input(
            &[
                comment("alice", "2026-05-07T10:30:00Z", "first"),
                comment("bob", "2026-05-07T11:00:00Z", "second"),
            ],
            "2026-05-07T10:00:00Z",
        );
        assert!(s.contains("(2 new comments)"));
        // Order in the rendered body must match the input order so the
        // chronological sort applied by `fetch_new_comments_since` is
        // what the agent sees.
        let first_idx = s.find("first").expect("first present");
        let second_idx = s.find("second").expect("second present");
        assert!(first_idx < second_idx);
    }

    #[test]
    fn format_human_input_preserves_markdown_verbatim() {
        // Comment bodies frequently contain code fences, headings, lists.
        // The synthesised step is read by the next agent as plain
        // markdown, so the body must round-trip untouched.
        let raw = "### Heading\n\n```rust\nfn ok() {}\n```\n- bullet";
        let s = format_human_input(
            &[comment("alice", "2026-05-07T10:30:00Z", raw)],
            "2026-05-07T10:00:00Z",
        );
        assert!(s.contains(raw), "raw markdown not present in body: {s}");
    }

    #[test]
    fn parse_issue_comments_json_handles_canonical_gh_shape() {
        // Pin the `gh issue view <id> --json comments` shape so a future
        // gh change either keeps working or fails loud here, not silently
        // at run time.
        let bytes = br#"{
          "comments": [
            {"author": {"login": "alice"}, "createdAt": "2026-05-07T10:30:00Z", "body": "first"},
            {"author": {"login": "bob"}, "createdAt": "2026-05-07T11:00:00Z", "body": "second"}
          ]
        }"#;
        let parsed = parse_issue_comments_json(bytes).expect("canonical shape parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].author, "alice");
        assert_eq!(parsed[0].body, "first");
        assert_eq!(parsed[1].created_at, "2026-05-07T11:00:00Z");
    }

    #[test]
    fn parse_issue_comments_json_tolerates_missing_author_login() {
        // A comment from a deleted account or a bot without a login should
        // still surface (with empty author) rather than poisoning the
        // entire fetch. Body is what the agent ultimately needs to read.
        let bytes = br#"{"comments":[
          {"createdAt":"2026-05-07T10:30:00Z","body":"orphan"}
        ]}"#;
        let parsed = parse_issue_comments_json(bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].author, "");
        assert_eq!(parsed[0].body, "orphan");
    }

    #[test]
    fn parse_issue_comments_json_skips_comments_without_created_at() {
        // `createdAt` is the only field the timestamp filter cannot work
        // around: drop the comment rather than guess.
        let bytes = br#"{"comments":[
          {"author":{"login":"alice"},"body":"timeless"},
          {"author":{"login":"bob"},"createdAt":"2026-05-07T11:00:00Z","body":"keeps"}
        ]}"#;
        let parsed = parse_issue_comments_json(bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].body, "keeps");
    }

    #[test]
    fn parse_issue_comments_json_rejects_missing_array() {
        let err = parse_issue_comments_json(br#"{"foo": "bar"}"#).expect_err("missing array");
        assert!(err.contains("comments"));
    }
}
