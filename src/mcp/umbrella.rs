//! Umbrella issue orchestration (#151).
//!
//! Splits a GitHub issue with a task list of sub-issue refs (`- [ ] #128`)
//! into sequential child runs. Each child runs the canonical
//! `implement-issue` flow on its own branch, the resulting PR is merged,
//! and the umbrella body's checkbox is ticked before the next child
//! starts. On a child failure the orchestrator stops, leaves the
//! already-merged children in place, and posts a summary comment on the
//! umbrella -- "loud failure" semantics per the V1 scoping comment on the
//! issue.
//!
//! ## Scope (V1)
//!
//! In scope:
//! - Body parsing for unchecked task list items (`- [ ] #NNN`).
//! - Sequential execution: one child at a time, its own branch and PR,
//!   merged before the next starts.
//! - Progress tracking: tick the umbrella checkbox after each successful
//!   child merge.
//! - Loud failure: stop on the first failing child, comment on the
//!   umbrella with the failed child and run id.
//! - Summary comment on the umbrella when all children pass or one fails.
//!
//! Out of scope (deferred -- see issue #151 / #28 / #26):
//! - Parallel execution of independent children (#28).
//! - Mixed DAG with parallel groups.
//! - Recursive umbrellas (a sub-issue that is itself an umbrella runs as
//!   a regular issue; further nesting is not detected).
//!
//! ## Design notes
//!
//! - **gh CLI as source of truth.** Body fetching and editing use `gh`
//!   directly so the orchestrator inherits the user's auth and respects
//!   the same repository the rest of the flow already operates on. No
//!   GitHub API client is added to the dependency tree for V1.
//!
//! - **Detection vs execution split.** [`parse_subissues`] is pure (text
//!   in, refs out) and unit-tested in isolation. The shelling-out helpers
//!   are thin wrappers around [`std::process::Command`] -- their behaviour
//!   is exercised by integration runs against a real repo, not by the unit
//!   tests in this file.
//!
//! - **Merging is explicit.** The canonical `implement-issue` flow ends
//!   at a *draft* PR by design (human-in-the-loop). Umbrella mode crosses
//!   that line on purpose: "merge before next" is a hard requirement of
//!   the issue scoping. If the repository has branch protection that
//!   blocks `gh pr merge`, the merge fails and the orchestrator surfaces
//!   it as a child failure -- consistent with the loud-failure mode.

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::LazyLock;

use color_eyre::Result;
use color_eyre::eyre::eyre;
use regex_lite::Regex;
use tracing::{info, warn};

/// One unchecked task list item parsed out of an umbrella issue body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubIssueRef {
    /// Issue number (positive). Comes from the `#NNN` token.
    pub issue: u64,
    /// Whether the task list item was already checked (`- [x] #NNN`).
    /// Checked items are skipped during orchestration -- they were
    /// completed by a prior run -- but we keep them in the parsed list
    /// so the summary comment can report total vs newly-completed.
    pub already_done: bool,
}

/// Outcome for a single child issue inside an umbrella run.
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct ChildResult {
    /// Sub-issue number.
    pub issue: u64,
    /// Run id of the child's `implement-issue` flow execution. `None`
    /// when the child was skipped (already done) or never attempted
    /// (orchestrator stopped earlier on a sibling failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// PR URL the child's `pr` step produced. `None` on skip / not
    /// attempted / failure-without-PR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// `"PASS"` / `"FAIL"` / `"unclear"` / `"skipped"` /
    /// `"not-attempted"` / `"merge-failed"`.
    pub verdict: String,
}

/// Aggregate result returned alongside the per-child detail when an
/// `implement_issue` call resolves an umbrella.
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct UmbrellaSummary {
    /// Umbrella issue number.
    pub parent_issue: u64,
    /// Total task list items detected (checked + unchecked).
    pub total: usize,
    /// Children that finished with `verdict == PASS` and a successful
    /// merge in this run. Excludes children that were already checked
    /// off before the run started.
    pub completed: usize,
    /// First child that failed, if any. `None` when all children passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<u64>,
    /// Per-child detail in body order. Always populated for every task
    /// list item (skipped, attempted, not-attempted) so the caller can
    /// reconstruct the umbrella state without re-fetching the body.
    pub children: Vec<ChildResult>,
}

// --- Parser ---

static SUBISSUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match a markdown task list item that points at a sub-issue. The
    // `#NNN` (or the issue URL) must be the first non-whitespace token
    // after the checkbox -- this is the form GitHub itself emits when
    // it autogenerates a sub-issue task list, and it keeps the parser
    // from picking up incidental `#NNN` mentions like "fixes #99 too".
    //
    // Captures:
    //   1: checkbox state (` ` or `x`/`X`)
    //   2: issue number (from `#NNN`)
    //   3: issue number (from a full GitHub issue URL)
    //
    // Exactly one of (2) or (3) is present per match.
    Regex::new(
        r"(?m)^[ \t]*[-*][ \t]+\[([ xX])\][ \t]+(?:#(\d+)|https?://github\.com/[^/\s]+/[^/\s]+/issues/(\d+))\b",
    )
    .expect("subissue regex compiles")
});

/// Parse task list items pointing at sub-issues out of an umbrella body.
///
/// Returns one [`SubIssueRef`] per task list item in the order they
/// appear in the body. Duplicates (same issue number listed twice) are
/// removed -- the first occurrence wins -- because the orchestrator must
/// not run the same child twice.
///
/// An empty result means "not an umbrella" -- the caller should fall
/// back to the single-issue path.
pub(super) fn parse_subissues(body: &str) -> Vec<SubIssueRef> {
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut out: Vec<SubIssueRef> = Vec::new();
    for caps in SUBISSUE_RE.captures_iter(body) {
        let checkbox = caps.get(1).map(|m| m.as_str()).unwrap_or(" ");
        let already_done = matches!(checkbox, "x" | "X");
        let issue_str = caps
            .get(2)
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
        let Ok(issue) = issue_str.parse::<u64>() else {
            continue;
        };
        if issue == 0 {
            continue;
        }
        if !seen.insert(issue) {
            continue;
        }
        out.push(SubIssueRef {
            issue,
            already_done,
        });
    }
    out
}

// --- gh shell helpers ---

/// Fetch an issue body via `gh issue view <n> --json body -q .body`.
/// Returns the raw body text (no trailing newline trim -- the parser is
/// tolerant of either).
pub(super) fn fetch_issue_body(issue: u64) -> Result<String> {
    let out = Command::new("gh")
        .args([
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "body",
            "-q",
            ".body",
        ])
        .output()
        .map_err(|e| eyre!("gh issue view #{issue} failed to start: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(eyre!(
            "gh issue view #{issue} exited with {}: {}",
            out.status,
            stderr.trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| eyre!("gh issue view #{issue} non-utf8 output: {e}"))
}

/// Tick the `- [ ] #child` checkbox on the umbrella body so the GitHub
/// UI reflects progress. Best-effort: any failure logs a warning but
/// does not abort the orchestrator -- the run can still succeed and the
/// summary comment will record what happened.
pub(super) fn tick_checkbox(parent_issue: u64, child_issue: u64) -> Result<()> {
    let body = fetch_issue_body(parent_issue)?;
    let updated = check_off_in_body(&body, child_issue);
    if updated == body {
        // Nothing to do: the checkbox was already ticked or the line is
        // not in the parser's recognised shape. Return Ok rather than
        // pretending to write -- avoids a no-op gh call.
        return Ok(());
    }
    let status = Command::new("gh")
        .args([
            "issue",
            "edit",
            &parent_issue.to_string(),
            "--body-file",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(updated.as_bytes())?;
            }
            child.wait_with_output()
        })
        .map_err(|e| eyre!("gh issue edit #{parent_issue} failed: {e}"))?;
    if !status.status.success() {
        return Err(eyre!(
            "gh issue edit #{parent_issue} exited with {}: {}",
            status.status,
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(())
}

/// Replace `- [ ] #child` (or the `* [ ]` and URL variants) with the
/// checked form on the first matching line. Pure for testability; the
/// shell wrapper [`tick_checkbox`] composes it with `gh`.
fn check_off_in_body(body: &str, child_issue: u64) -> String {
    let needle_hash = format!("#{child_issue}");
    let mut out = String::with_capacity(body.len());
    let mut replaced = false;
    for (i, line) in body.split_inclusive('\n').enumerate() {
        if !replaced && line_targets_child(line, child_issue, &needle_hash) {
            // Find the `[ ]` and rewrite to `[x]`. Do not touch any other
            // bracket pair that might appear later on the line (e.g. a
            // markdown link `[text](url)`).
            if let Some(idx) = line.find("[ ]") {
                let mut buf = String::with_capacity(line.len());
                buf.push_str(&line[..idx]);
                buf.push_str("[x]");
                buf.push_str(&line[idx + 3..]);
                out.push_str(&buf);
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
        // `i` exists to keep the loop body unambiguous to readers.
        let _ = i;
    }
    out
}

/// Conservative match: the line must start with a task list marker
/// (`- [ ]` or `* [ ]`) and the first token after it must be the
/// child's issue ref. Mirrors the SUBISSUE_RE rule so the writer never
/// rewrites a different line than the parser would have picked up.
fn line_targets_child(line: &str, child_issue: u64, needle_hash: &str) -> bool {
    let trimmed = line.trim_start();
    let after_marker = trimmed
        .strip_prefix("- [ ] ")
        .or_else(|| trimmed.strip_prefix("* [ ] "));
    let Some(rest) = after_marker else {
        return false;
    };
    if let Some(after) = rest.strip_prefix(needle_hash) {
        // Guard against `#1234` matching `#12345`: require the next char
        // to be a non-digit (or end of string).
        return after
            .chars()
            .next()
            .map(|c| !c.is_ascii_digit())
            .unwrap_or(true);
    }
    // URL variant: `https://github.com/<owner>/<repo>/issues/<n>`.
    if let Some(idx) = rest.find("/issues/") {
        let suffix = &rest[idx + "/issues/".len()..];
        let num: String = suffix.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(parsed) = num.parse::<u64>() {
            return parsed == child_issue;
        }
    }
    false
}

/// Post a comment on the umbrella issue summarising the orchestration
/// outcome. Same best-effort policy as [`tick_checkbox`]: a failure
/// here does not change the structured result -- the call is logged
/// and ignored.
pub(super) fn post_comment(issue: u64, body: &str) -> Result<()> {
    let status = Command::new("gh")
        .args(["issue", "comment", &issue.to_string(), "--body-file", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(body.as_bytes())?;
            }
            child.wait_with_output()
        })
        .map_err(|e| eyre!("gh issue comment #{issue} failed: {e}"))?;
    if !status.status.success() {
        return Err(eyre!(
            "gh issue comment #{issue} exited with {}: {}",
            status.status,
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(())
}

/// Mark a draft PR ready and squash-merge it, deleting the source
/// branch. Returns Err on any failure; the caller treats that as a
/// child failure.
///
/// Why squash: the canonical flow produces one functional change per
/// child issue, so a single squashed commit per child keeps `main`'s
/// history one-commit-per-issue. `--delete-branch` keeps the
/// orchestrator from leaving stale remote branches behind.
pub(super) fn merge_pr(pr_url: &str) -> Result<()> {
    // Best-effort un-draft. If the PR is already ready (e.g. because the
    // child's `pr` step opened a non-draft), this exits non-zero with
    // "PR is not in draft state" -- which we treat as success.
    let ready = Command::new("gh")
        .args(["pr", "ready", pr_url])
        .output()
        .map_err(|e| eyre!("gh pr ready {pr_url} failed to start: {e}"))?;
    if !ready.status.success() {
        let stderr = String::from_utf8_lossy(&ready.stderr);
        if !stderr.to_lowercase().contains("not in draft") {
            // Surface the real error, but keep going so a benign
            // already-ready response does not abort the merge.
            warn!(stderr = %stderr.trim(), "gh pr ready returned non-zero");
        }
    }
    let merge = Command::new("gh")
        .args(["pr", "merge", pr_url, "--squash", "--delete-branch"])
        .output()
        .map_err(|e| eyre!("gh pr merge {pr_url} failed to start: {e}"))?;
    if !merge.status.success() {
        return Err(eyre!(
            "gh pr merge {pr_url} exited with {}: {}",
            merge.status,
            String::from_utf8_lossy(&merge.stderr).trim()
        ));
    }
    Ok(())
}

/// Fetch origin and check out detached `origin/main` so the next child
/// branches off the post-merge tip. The implement step's task already
/// does its own fetch+checkout; this is belt-and-suspenders so the
/// orchestrator is not left on a branch that `gh pr merge` just
/// deleted.
pub(super) fn sync_to_main() -> Result<()> {
    let fetch = Command::new("git")
        .args(["fetch", "origin"])
        .output()
        .map_err(|e| eyre!("git fetch origin failed to start: {e}"))?;
    if !fetch.status.success() {
        return Err(eyre!(
            "git fetch origin exited with {}: {}",
            fetch.status,
            String::from_utf8_lossy(&fetch.stderr).trim()
        ));
    }
    let checkout = Command::new("git")
        .args(["checkout", "origin/main"])
        .output()
        .map_err(|e| eyre!("git checkout origin/main failed to start: {e}"))?;
    if !checkout.status.success() {
        return Err(eyre!(
            "git checkout origin/main exited with {}: {}",
            checkout.status,
            String::from_utf8_lossy(&checkout.stderr).trim()
        ));
    }
    Ok(())
}

/// Render the summary comment posted on the umbrella when the run
/// finishes (success or first failure). The format is intentionally
/// mechanical -- the umbrella issue is consumed by both humans and the
/// next orchestrator run, so the wording is stable enough to grep.
pub(super) fn render_summary_comment(summary: &UmbrellaSummary) -> String {
    let mut buf = String::new();
    if let Some(failed) = summary.failed_at {
        buf.push_str(&format!("Umbrella orchestration stopped on #{failed}.\n\n"));
    } else {
        buf.push_str("Umbrella orchestration complete.\n\n");
    }
    buf.push_str(&format!(
        "- Total sub-issues: {}\n- Completed this run: {}\n",
        summary.total, summary.completed
    ));
    if let Some(failed) = summary.failed_at {
        buf.push_str(&format!("- Failed at: #{failed}\n"));
    }
    buf.push_str("\n### Per-child outcome\n");
    for child in &summary.children {
        let pr = child
            .pr_url
            .as_deref()
            .map(|u| format!(" -- {u}"))
            .unwrap_or_default();
        let run = child
            .run_id
            .as_deref()
            .map(|r| format!(" (run `{r}`)"))
            .unwrap_or_default();
        buf.push_str(&format!(
            "- #{} -- {}{pr}{run}\n",
            child.issue, child.verdict
        ));
    }
    buf
}

/// Log helper -- routes through `tracing` so MCP transports see it on
/// stderr without corrupting the JSON-RPC channel on stdout.
pub(super) fn log_starting_child(parent: u64, child: u64, idx: usize, total: usize) {
    info!(
        parent,
        child,
        idx = idx + 1,
        total,
        "starting umbrella child"
    );
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_subissues ----

    #[test]
    fn parse_subissues_finds_basic_unchecked_items() {
        let body = "Description.\n\n\
                    - [ ] #128\n\
                    - [ ] #129\n\
                    - [ ] #130\n";
        let refs = parse_subissues(body);
        assert_eq!(
            refs,
            vec![
                SubIssueRef {
                    issue: 128,
                    already_done: false
                },
                SubIssueRef {
                    issue: 129,
                    already_done: false
                },
                SubIssueRef {
                    issue: 130,
                    already_done: false
                },
            ]
        );
    }

    #[test]
    fn parse_subissues_distinguishes_checked_and_unchecked() {
        // Checked items are kept in the result so the summary comment
        // can report total vs newly-completed counts. The orchestrator
        // is responsible for skipping `already_done`.
        let body = "- [x] #1\n- [ ] #2\n- [X] #3\n";
        let refs = parse_subissues(body);
        assert_eq!(refs.len(), 3);
        assert!(refs[0].already_done);
        assert!(!refs[1].already_done);
        assert!(refs[2].already_done);
    }

    #[test]
    fn parse_subissues_returns_empty_for_non_umbrella() {
        // Free prose with bare `#NNN` mentions is NOT an umbrella. The
        // parser must not promote `fixes #99 too` to a sub-issue ref.
        let body = "This issue fixes #99 too. See also #100 and #101.\n";
        assert!(parse_subissues(body).is_empty());
    }

    #[test]
    fn parse_subissues_ignores_inline_hash_after_text() {
        // Task list item, but `#NNN` comes after prose -- not the form
        // GitHub auto-generates. Conservative parser drops it to avoid
        // false positives like `- [ ] write tests, then close #99`.
        let body = "- [ ] write tests, then close #99\n";
        assert!(parse_subissues(body).is_empty());
    }

    #[test]
    fn parse_subissues_accepts_asterisk_bullets() {
        // Markdown allows `*` as well as `-` for task lists.
        let body = "* [ ] #5\n* [x] #6\n";
        let refs = parse_subissues(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].issue, 5);
        assert_eq!(refs[1].issue, 6);
        assert!(refs[1].already_done);
    }

    #[test]
    fn parse_subissues_accepts_full_url_form() {
        // GitHub's auto-generated sub-issue list uses the full URL on
        // some flows. Both forms must be recognised.
        let body = "- [ ] https://github.com/foo/bar/issues/42\n\
                    - [x] https://github.com/foo/bar/issues/43\n";
        let refs = parse_subissues(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].issue, 42);
        assert_eq!(refs[1].issue, 43);
    }

    #[test]
    fn parse_subissues_deduplicates_keeping_first_occurrence() {
        // A duplicate ref must not run the same child twice. The first
        // line's checked state wins so a later `- [x] #10` does not
        // change the queued status of an earlier `- [ ] #10`.
        let body = "- [ ] #10\n- [x] #10\n- [ ] #11\n";
        let refs = parse_subissues(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].issue, 10);
        assert!(!refs[0].already_done);
        assert_eq!(refs[1].issue, 11);
    }

    #[test]
    fn parse_subissues_preserves_body_order() {
        // Order matters: the orchestrator runs children in the order
        // they appear in the body, which is also the order the user
        // intended.
        let body = "- [ ] #20\n- [ ] #5\n- [ ] #15\n";
        let refs = parse_subissues(body);
        assert_eq!(
            refs.iter().map(|r| r.issue).collect::<Vec<_>>(),
            vec![20, 5, 15]
        );
    }

    #[test]
    fn parse_subissues_handles_indented_items() {
        // Nested task lists are still task lists. We accept leading
        // whitespace before the bullet to handle issues that nest sub-
        // issues under headings.
        let body = "  - [ ] #100\n    - [ ] #101\n";
        let refs = parse_subissues(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].issue, 100);
        assert_eq!(refs[1].issue, 101);
    }

    #[test]
    fn parse_subissues_rejects_zero_issue() {
        // `#0` is not a valid GitHub issue. Parsing it would produce a
        // bogus child run; explicit rejection keeps the orchestrator
        // honest.
        let body = "- [ ] #0\n- [ ] #1\n";
        let refs = parse_subissues(body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].issue, 1);
    }

    #[test]
    fn parse_subissues_does_not_match_hash_inside_word() {
        // `release-#42` should not match -- the bullet must be a task
        // list item AND the `#NNN` must be the leading token. This
        // guards against accidental matches in body text the user
        // copy-pasted from elsewhere.
        let body = "- [ ] release-#42 needs a follow-up\n";
        assert!(parse_subissues(body).is_empty());
    }

    // ---- check_off_in_body ----

    #[test]
    fn check_off_in_body_ticks_unchecked_line() {
        let body = "- [ ] #128\n- [ ] #129\n";
        let updated = check_off_in_body(body, 128);
        assert_eq!(updated, "- [x] #128\n- [ ] #129\n");
    }

    #[test]
    fn check_off_in_body_only_ticks_first_match() {
        // A duplicate ref (against parser intent) should not cascade --
        // we tick only the first occurrence so re-runs are idempotent.
        let body = "- [ ] #50\n- [ ] #50\n";
        let updated = check_off_in_body(body, 50);
        assert_eq!(updated, "- [x] #50\n- [ ] #50\n");
    }

    #[test]
    fn check_off_in_body_leaves_already_checked_unchanged() {
        // Idempotent: ticking an already-ticked box returns the body
        // unchanged. The caller relies on this to skip the gh edit.
        let body = "- [x] #128\n- [ ] #129\n";
        let updated = check_off_in_body(body, 128);
        assert_eq!(updated, body);
    }

    #[test]
    fn check_off_in_body_handles_url_form() {
        let body = "- [ ] https://github.com/foo/bar/issues/42\n";
        let updated = check_off_in_body(body, 42);
        assert_eq!(updated, "- [x] https://github.com/foo/bar/issues/42\n");
    }

    #[test]
    fn check_off_in_body_does_not_affect_other_bracket_pairs() {
        // A markdown link further down the line must NOT be confused
        // with the checkbox. The writer only rewrites the leading
        // `[ ]`, never an arbitrary bracket pair.
        let body = "- [ ] #128 see [docs](http://x)\n";
        let updated = check_off_in_body(body, 128);
        assert_eq!(updated, "- [x] #128 see [docs](http://x)\n");
    }

    #[test]
    fn check_off_in_body_does_not_match_unrelated_issue() {
        // Targeting #128 must not tick a #1280 box -- the prefix guard
        // protects against the off-by-zero case.
        let body = "- [ ] #1280\n- [ ] #128\n";
        let updated = check_off_in_body(body, 128);
        assert_eq!(updated, "- [ ] #1280\n- [x] #128\n");
    }

    #[test]
    fn check_off_in_body_ignores_non_task_list_lines() {
        // A bare `#128` mention is not a task list item and must not
        // be rewritten.
        let body = "fixes #128\n";
        let updated = check_off_in_body(body, 128);
        assert_eq!(updated, body);
    }

    // ---- render_summary_comment ----

    #[test]
    fn render_summary_comment_success_path() {
        let summary = UmbrellaSummary {
            parent_issue: 151,
            total: 2,
            completed: 2,
            failed_at: None,
            children: vec![
                ChildResult {
                    issue: 128,
                    run_id: Some("run-A".into()),
                    pr_url: Some("https://github.com/o/r/pull/1".into()),
                    verdict: "PASS".into(),
                },
                ChildResult {
                    issue: 129,
                    run_id: Some("run-B".into()),
                    pr_url: Some("https://github.com/o/r/pull/2".into()),
                    verdict: "PASS".into(),
                },
            ],
        };
        let s = render_summary_comment(&summary);
        assert!(
            s.starts_with("Umbrella orchestration complete."),
            "headline: {s}"
        );
        assert!(s.contains("Total sub-issues: 2"));
        assert!(s.contains("Completed this run: 2"));
        assert!(s.contains("#128 -- PASS"));
        assert!(s.contains("https://github.com/o/r/pull/1"));
        assert!(s.contains("(run `run-A`)"));
        assert!(!s.contains("Failed at"));
    }

    #[test]
    fn render_summary_comment_failure_path_names_failed_child() {
        let summary = UmbrellaSummary {
            parent_issue: 151,
            total: 3,
            completed: 1,
            failed_at: Some(129),
            children: vec![
                ChildResult {
                    issue: 128,
                    run_id: Some("run-A".into()),
                    pr_url: Some("https://github.com/o/r/pull/1".into()),
                    verdict: "PASS".into(),
                },
                ChildResult {
                    issue: 129,
                    run_id: Some("run-B".into()),
                    pr_url: None,
                    verdict: "FAIL".into(),
                },
                ChildResult {
                    issue: 130,
                    run_id: None,
                    pr_url: None,
                    verdict: "not-attempted".into(),
                },
            ],
        };
        let s = render_summary_comment(&summary);
        assert!(s.starts_with("Umbrella orchestration stopped on #129."));
        assert!(s.contains("Failed at: #129"));
        assert!(s.contains("#128 -- PASS"));
        assert!(s.contains("#129 -- FAIL"));
        assert!(s.contains("#130 -- not-attempted"));
    }

    // ---- line_targets_child ----

    #[test]
    fn line_targets_child_matches_hash_form() {
        assert!(line_targets_child("- [ ] #128\n", 128, "#128"));
        assert!(line_targets_child("  * [ ] #128 -- desc", 128, "#128"));
    }

    #[test]
    fn line_targets_child_rejects_prefix_collision() {
        // `#1280` must not match when targeting `#128`.
        assert!(!line_targets_child("- [ ] #1280\n", 128, "#128"));
    }

    #[test]
    fn line_targets_child_rejects_non_task_lines() {
        assert!(!line_targets_child("fixes #128\n", 128, "#128"));
        assert!(!line_targets_child("- [x] #128\n", 128, "#128"));
    }

    #[test]
    fn line_targets_child_matches_url_form() {
        assert!(line_targets_child(
            "- [ ] https://github.com/o/r/issues/42\n",
            42,
            "#42"
        ));
    }
}
