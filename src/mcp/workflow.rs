//! Workflow tools (#196 split): `implement_issue` (#213), `review_pr` (#214).
//!
//! `rework_pr` (#215) is tracked as a follow-up and will land alongside so
//! all three workflow wrappers share the flow-runner glue and the verdict /
//! url / findings parsing helpers.
//!
//! ## Design notes
//!
//! - **Thin wrapper.** The `implement-issue` flow already owns the
//!   `gh issue view`, branch creation and PR-opening behaviour described in
//!   the issue body. The MCP tool's job is parameter validation, flow
//!   invocation and surfacing a structured result. The tool intentionally
//!   does **not** shell out to `gh` or `git` itself -- duplicating those
//!   commands here would split the source of truth between the runner and
//!   the MCP layer.
//!
//! - **Result shape.** Per the issue: `{ branch, pr_url, verdict }`. We add
//!   `run_id` so callers can reach `show_output` for full step content
//!   (mirrors `run_flow` and keeps the audit trail discoverable).
//!
//! - **Verdict parsing.** The `review` step in the flow ends with
//!   `FINAL_VERDICT: PASS | FAIL`. We match the exact token and fall back
//!   to `unclear` so an oddly-formatted review still produces a structured
//!   response instead of a tool error -- the verdict is a *finding*, not a
//!   tool failure.
//!
//! - **PR URL extraction.** The `pr` step prints the GitHub PR URL on PASS.
//!   On FAIL it prints blocking issues and no URL. We extract the first
//!   `https://github.com/<owner>/<repo>/pull/<n>` and return `null` when
//!   absent.
//!
//! - **Branch derivation.** Read `git rev-parse --abbrev-ref HEAD` after the
//!   flow completes. The flow's first step does the checkout, so HEAD ends
//!   up on the new branch by the time the call returns. If HEAD is detached
//!   or `git` is unavailable, `branch` is `null` -- the tool still succeeds
//!   so callers can act on `verdict` and `pr_url`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex_lite::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::runner::{self, ExecuteFlowSpec, FlowResult, FlowSource};
use crate::stack;

use super::error::{McpError, McpErrorCode};
use super::execution::classify_setup_error;
use super::tools::Tool;

/// Flow name resolved through the seed cascade. Pinned as a `const` so a
/// rename of the seed file surfaces here as a single edit -- the team
/// review on the parent epic (#196) flagged hidden flow names as a recipe
/// for silent breakage.
const IMPLEMENT_ISSUE_FLOW: &str = "implement-issue";

/// Step ids that `build_result` keys off when extracting the verdict and PR
/// URL from the run output. Validated against the resolved flow before
/// `execute_flow` is called -- a renamed step otherwise produces a
/// successful tool call with `verdict: "unclear"` and `pr_url: null`,
/// which is the silent-failure mode flagged in the #213 team review.
const REQUIRED_STEP_IDS: &[&str] = &["review", "pr"];

fn invalid_params(reason: impl Into<String>) -> McpError {
    McpError::with_details(
        McpErrorCode::InvalidParams,
        json!({"reason": reason.into()}),
    )
}

fn internal(reason: impl Into<String>) -> McpError {
    McpError::with_details(
        McpErrorCode::InternalError,
        json!({"reason": reason.into()}),
    )
}

// --- implement_issue ---

#[derive(Deserialize)]
struct ImplementIssueArgs {
    issue: i64,
}

/// `implement_issue` -- run the canonical implement-issue flow against a
/// GitHub issue number and return the structured outcome.
pub struct ImplementIssue;

#[async_trait]
impl Tool for ImplementIssue {
    fn name(&self) -> &'static str {
        "implement_issue"
    }
    fn description(&self) -> &'static str {
        "Implement a GitHub issue end-to-end: runs the `implement-issue` flow which fetches the \
         issue, creates a feature branch off main, implements the change, reviews it, and opens \
         a draft PR on PASS. Returns { run_id, branch, pr_url, verdict } where verdict is \
         \"PASS\" | \"FAIL\" | \"unclear\". Use show_output with the returned run_id for the \
         full per-step transcript. Requires `implement-issue.yaml` in the seed cascade and a \
         `gh` CLI authenticated for the repo. Example: \
         implement_issue {\"issue\": 213}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "issue": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "GitHub issue number (positive integer)."
                }
            },
            "required": ["issue"]
        })
    }
    async fn call(&self, arguments: Value) -> Result<Value, McpError> {
        let parsed: ImplementIssueArgs = serde_json::from_value(arguments)
            .map_err(|e| invalid_params(format!("arguments: {e}")))?;
        if parsed.issue < 1 {
            return Err(invalid_params(format!(
                "issue must be a positive integer, got {}",
                parsed.issue
            )));
        }
        let issue = parsed.issue as u64;
        let result = run_implement_issue(issue).await?;
        Ok(serde_json::to_value(&result).map_err(|e| internal(format!("serialize: {e}")))?)
    }
}

#[derive(Debug, serde::Serialize)]
struct ImplementIssueResult {
    run_id: String,
    /// Current git branch after the flow finished. `None` when HEAD is
    /// detached, `git` is unavailable, or the working directory is not a
    /// repo. The flow's `implement` step is responsible for the checkout;
    /// this field reports what the runner observed afterwards rather than
    /// re-deriving it from agent output.
    branch: Option<String>,
    /// First GitHub PR URL parsed out of the `pr` step's output. `None`
    /// when the verdict was FAIL/unclear or the agent did not emit a URL.
    pr_url: Option<String>,
    /// `"PASS"`, `"FAIL"` or `"unclear"`. Mirrors the `FINAL_VERDICT:` token
    /// the reviewer agent emits. `unclear` is reserved for outputs that
    /// neither token matched -- the call still succeeds so the caller can
    /// inspect the run via `show_output`.
    verdict: String,
}

async fn run_implement_issue(issue: u64) -> Result<ImplementIssueResult, McpError> {
    // Pre-flight: verify the resolved flow defines the step ids that
    // `build_result` keys off. Cheap (one YAML parse) and fails fast with
    // a clear error instead of running an end-to-end flow only to surface
    // an empty/unclear payload -- see #213 review feedback.
    let missing = runner::verify_flow_step_ids(IMPLEMENT_ISSUE_FLOW, REQUIRED_STEP_IDS)
        .map_err(|e| classify_setup_error(IMPLEMENT_ISSUE_FLOW, e))?;
    if !missing.is_empty() {
        return Err(internal(format!(
            "flow '{IMPLEMENT_ISSUE_FLOW}' is missing required step ids [{}]; \
             implement_issue keys off these step ids to extract the review \
             verdict and PR url. Rename the steps in the flow or update the \
             tool's REQUIRED_STEP_IDS to match.",
            missing.join(", ")
        )));
    }

    let mut bare_args = HashMap::new();
    bare_args.insert("id".to_string(), issue.to_string());

    let spec = ExecuteFlowSpec {
        flow: FlowSource::Name(IMPLEMENT_ISSUE_FLOW.to_string()),
        bare_args,
        // MCP transports treat stdout as the JSON-RPC channel -- mirrors the
        // run_flow design (#198). Without this the runner banner corrupts
        // the response framing.
        suppress_command_banner: true,
        ..ExecuteFlowSpec::default()
    };

    let handle = runner::execute_flow(spec)
        .await
        .map_err(|e| classify_setup_error(IMPLEMENT_ISSUE_FLOW, e))?;
    let run_id = handle.run_id.clone();
    let stack_path = handle.stack_path.clone();

    let flow_result = handle.await_completion().await.map_err(|e| {
        // Partial run: we still have a run_id the caller can show_output on.
        // Surface internal_error with both reason and run_id so the caller
        // can route by code and inspect the run.
        McpError::with_details(
            McpErrorCode::InternalError,
            json!({
                "reason": format!("flow execution failed: {e}"),
                "run_id": run_id,
                "flow": IMPLEMENT_ISSUE_FLOW,
            }),
        )
    })?;

    Ok(build_result(run_id, &stack_path, &flow_result))
}

fn build_result(
    run_id: String,
    stack_path: &Path,
    _flow_result: &FlowResult,
) -> ImplementIssueResult {
    let outputs = match stack::read_run(stack_path, &run_id, None) {
        Ok(o) => o,
        Err(e) => {
            // The flow finished -- if we cannot read its outputs the manifest
            // and step files are corrupt. Log and degrade to empty findings
            // rather than fail the whole call: the run_id is still useful for
            // a manual `show_output` follow-up.
            warn!(error = %e, run_id = %run_id, "failed to read run outputs");
            return ImplementIssueResult {
                run_id,
                branch: current_branch(),
                pr_url: None,
                verdict: VERDICT_UNCLEAR.to_string(),
            };
        }
    };

    let review_output = output_for(&outputs, "review");
    let pr_output = output_for(&outputs, "pr");

    let verdict = parse_verdict(review_output.as_deref().unwrap_or(""));
    let pr_url = parse_pr_url(pr_output.as_deref().unwrap_or(""));
    let branch = current_branch();

    ImplementIssueResult {
        run_id,
        branch,
        pr_url,
        verdict: verdict.to_string(),
    }
}

fn output_for(outputs: &stack::RunOutputs, step_id: &str) -> Option<String> {
    outputs
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .and_then(|s| s.content.clone())
}

const VERDICT_PASS: &str = "PASS";
const VERDICT_FAIL: &str = "FAIL";
const VERDICT_UNCLEAR: &str = "unclear";

static VERDICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match `FINAL_VERDICT:` at the start of a line, optionally preceded by
    // a markdown list bullet (`-`, `*`) plus surrounding indentation. The
    // reviewer prompt emits `- FINAL_VERDICT: PASS` as a bullet inside a
    // summary list, so the bullet prefix must be tolerated. We still pin
    // the token to a line start (rather than allowing it inline) so prose
    // like "the reviewer must emit FINAL_VERDICT: PASS" does not match.
    //
    // We take the first match so a stray repetition cannot flip the
    // result. The reviewer prompt forbids duplicates anyway.
    //
    // `[ \t]` rather than `\s` for the indentation class -- `\s` matches
    // newlines and would let the regex pull into the next line.
    Regex::new(r"(?m)^[ \t]*[-*]?[ \t]*FINAL_VERDICT:[ \t]*(PASS|FAIL)\b")
        .expect("verdict regex compiles")
});

fn parse_verdict(review_output: &str) -> &'static str {
    match VERDICT_RE
        .captures(review_output)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
    {
        Some("PASS") => VERDICT_PASS,
        Some("FAIL") => VERDICT_FAIL,
        _ => VERDICT_UNCLEAR,
    }
}

static PR_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Conservative GitHub PR URL shape: owner/repo are non-empty path segments
    // without `/`, the trailing PR number is digits. Trailing punctuation
    // (`.`, `,`, `)`, etc.) is excluded so the captured URL is safe to
    // hand back to a client without further sanitising.
    Regex::new(r"https://github\.com/[^/\s]+/[^/\s]+/pull/(\d+)").expect("pr url regex compiles")
});

fn parse_pr_url(pr_output: &str) -> Option<String> {
    PR_URL_RE.find(pr_output).map(|m| {
        m.as_str()
            .trim_end_matches(|c: char| !c.is_alphanumeric())
            .to_string()
    })
}

/// Best-effort `git rev-parse --abbrev-ref HEAD`. Returns `None` for any
/// failure mode (not a repo, detached HEAD, `git` missing) -- the tool
/// must not fail the whole call just because the branch name could not be
/// resolved. Callers can fall back to `show_output` to discover the branch
/// from the implement step's transcript.
fn current_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if name.is_empty() || name == "HEAD" {
        // Detached HEAD reports "HEAD" -- not useful as a branch name.
        return None;
    }
    Some(name)
}

// --- review_pr ---

/// Flow name resolved through the seed cascade. Same rationale as
/// `IMPLEMENT_ISSUE_FLOW`: pin the seed-file name to a single edit point.
const REVIEW_PR_FLOW: &str = "review-pr";

/// Step ids `review_pr` keys off when extracting the verdict and findings.
/// `consensus` is the only step with structured output we parse; the
/// `architecture`, `code-review` and `fetch` steps feed into it but their
/// content is intentionally not surfaced as structured data (it lives in the
/// run transcript -- callers reach it via `show_output`).
const REVIEW_PR_REQUIRED_STEP_IDS: &[&str] = &["consensus"];

#[derive(Deserialize)]
struct ReviewPrArgs {
    pr: i64,
}

/// `review_pr` -- run the canonical review-pr flow against a GitHub PR
/// number and return the structured outcome.
pub struct ReviewPr;

#[async_trait]
impl Tool for ReviewPr {
    fn name(&self) -> &'static str {
        "review_pr"
    }
    fn description(&self) -> &'static str {
        "Review a GitHub pull request: runs the `review-pr` flow which fetches the PR \
         (description, diff, prior comments, linked issue) and produces a structured review. \
         Returns { run_id, verdict, blocking_issues, suggestions } where verdict is \
         \"APPROVE\" | \"REQUEST_CHANGES\" | \"COMMENT\" | \"unclear\". `blocking_issues` and \
         `suggestions` are arrays of strings, one per finding (multi-line bullet content \
         preserved). Use show_output with the returned run_id for the per-step transcript \
         (architecture and code-review prose). Requires `review-pr.yaml` in the seed cascade \
         and a `gh` CLI authenticated for the repo. Example: review_pr {\"pr\": 220}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pr": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Pull request number (positive integer)."
                }
            },
            "required": ["pr"]
        })
    }
    async fn call(&self, arguments: Value) -> Result<Value, McpError> {
        let parsed: ReviewPrArgs = serde_json::from_value(arguments)
            .map_err(|e| invalid_params(format!("arguments: {e}")))?;
        if parsed.pr < 1 {
            return Err(invalid_params(format!(
                "pr must be a positive integer, got {}",
                parsed.pr
            )));
        }
        let pr = parsed.pr as u64;
        let result = run_review_pr(pr).await?;
        Ok(serde_json::to_value(&result).map_err(|e| internal(format!("serialize: {e}")))?)
    }
}

#[derive(Debug, serde::Serialize)]
struct ReviewPrResult {
    run_id: String,
    /// `"APPROVE"`, `"REQUEST_CHANGES"`, `"COMMENT"` or `"unclear"`. Mirrors
    /// the `VERDICT:` token the consensus agent emits. `unclear` is reserved
    /// for outputs that did not match a known verdict -- the call still
    /// succeeds so the caller can inspect the run via `show_output`.
    verdict: String,
    /// Findings under the `### Blocking` section of the consensus output.
    /// Each entry is one bullet (continuation lines and code fences kept).
    /// Empty when the section is `None.` or absent.
    blocking_issues: Vec<String>,
    /// Findings under the `### Suggestions` section. Same shape as
    /// `blocking_issues`.
    suggestions: Vec<String>,
}

async fn run_review_pr(pr: u64) -> Result<ReviewPrResult, McpError> {
    // Pre-flight: same rationale as run_implement_issue. Without this, a
    // renamed `consensus` step would yield `verdict: "unclear"` and empty
    // findings on a successful run -- the silent-failure mode flagged in
    // the #213 review.
    let missing = runner::verify_flow_step_ids(REVIEW_PR_FLOW, REVIEW_PR_REQUIRED_STEP_IDS)
        .map_err(|e| classify_setup_error(REVIEW_PR_FLOW, e))?;
    if !missing.is_empty() {
        return Err(internal(format!(
            "flow '{REVIEW_PR_FLOW}' is missing required step ids [{}]; \
             review_pr keys off these step ids to extract the verdict and \
             findings. Rename the steps in the flow or update the tool's \
             REVIEW_PR_REQUIRED_STEP_IDS to match.",
            missing.join(", ")
        )));
    }

    let mut bare_args = HashMap::new();
    bare_args.insert("id".to_string(), pr.to_string());

    let spec = ExecuteFlowSpec {
        flow: FlowSource::Name(REVIEW_PR_FLOW.to_string()),
        bare_args,
        suppress_command_banner: true,
        ..ExecuteFlowSpec::default()
    };

    let handle = runner::execute_flow(spec)
        .await
        .map_err(|e| classify_setup_error(REVIEW_PR_FLOW, e))?;
    let run_id = handle.run_id.clone();
    let stack_path = handle.stack_path.clone();

    let flow_result = handle.await_completion().await.map_err(|e| {
        // Partial run: surface internal_error with run_id so the caller can
        // route by code and inspect the run. Same shape as implement_issue.
        McpError::with_details(
            McpErrorCode::InternalError,
            json!({
                "reason": format!("flow execution failed: {e}"),
                "run_id": run_id,
                "flow": REVIEW_PR_FLOW,
            }),
        )
    })?;

    Ok(build_review_result(run_id, &stack_path, &flow_result))
}

fn build_review_result(
    run_id: String,
    stack_path: &Path,
    _flow_result: &FlowResult,
) -> ReviewPrResult {
    let outputs = match stack::read_run(stack_path, &run_id, None) {
        Ok(o) => o,
        Err(e) => {
            // Mirrors implement_issue: the flow finished, but the manifest
            // is unreadable. Degrade to empty findings rather than failing
            // the call -- the run_id remains useful for `show_output`.
            warn!(error = %e, run_id = %run_id, "failed to read run outputs");
            return ReviewPrResult {
                run_id,
                verdict: REVIEW_VERDICT_UNCLEAR.to_string(),
                blocking_issues: Vec::new(),
                suggestions: Vec::new(),
            };
        }
    };

    let consensus_output = output_for(&outputs, "consensus").unwrap_or_default();
    let verdict = parse_review_verdict(&consensus_output);
    let blocking_issues = parse_findings_section(&consensus_output, "Blocking");
    let suggestions = parse_findings_section(&consensus_output, "Suggestions");

    ReviewPrResult {
        run_id,
        verdict: verdict.to_string(),
        blocking_issues,
        suggestions,
    }
}

const REVIEW_VERDICT_APPROVE: &str = "APPROVE";
const REVIEW_VERDICT_REQUEST_CHANGES: &str = "REQUEST_CHANGES";
const REVIEW_VERDICT_COMMENT: &str = "COMMENT";
const REVIEW_VERDICT_UNCLEAR: &str = "unclear";

static REVIEW_VERDICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match `VERDICT:` at the start of a line, optionally preceded by a
    // markdown bullet. The consensus prompt prescribes a bare `VERDICT:`
    // line at the top of the comment, but agent output occasionally wraps
    // it in a `- ` bullet -- tolerating that mirrors the implement_issue
    // verdict regex so prompt drift doesn't flip results to `unclear`.
    //
    // We pin the token to a line start (rather than allowing inline) so
    // prose like "the consensus must emit VERDICT: APPROVE" does not match.
    // First match wins -- a stray duplicate cannot flip the result.
    //
    // `[ \t]` rather than `\s` for indentation -- `\s` would slurp newlines
    // and pull the regex into the next line.
    Regex::new(r"(?m)^[ \t]*[-*]?[ \t]*VERDICT:[ \t]*(APPROVE|REQUEST_CHANGES|COMMENT)\b")
        .expect("review verdict regex compiles")
});

fn parse_review_verdict(consensus_output: &str) -> &'static str {
    match REVIEW_VERDICT_RE
        .captures(consensus_output)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
    {
        Some("APPROVE") => REVIEW_VERDICT_APPROVE,
        Some("REQUEST_CHANGES") => REVIEW_VERDICT_REQUEST_CHANGES,
        Some("COMMENT") => REVIEW_VERDICT_COMMENT,
        _ => REVIEW_VERDICT_UNCLEAR,
    }
}

/// Extract bullet items under a markdown section heading like
/// `### Blocking` or `### Suggestions`. Returns `[]` when the section is
/// absent or contains only the literal `None.` placeholder.
///
/// A "bullet" starts with `- ` or `* ` at column zero (no leading
/// whitespace). Continuation lines (indented, empty, fenced code blocks)
/// stay attached to the preceding bullet so embedded snippets ride along
/// with the finding they belong to.
///
/// The parser is line-oriented and tolerant: it does not enforce a strict
/// markdown grammar. Unrecognised noise between bullets is dropped rather
/// than mixed into a finding -- the consensus prompt forbids freeform prose
/// in these sections anyway.
fn parse_findings_section(consensus_output: &str, section: &str) -> Vec<String> {
    let header_needle = format!("### {section}");
    let mut lines = consensus_output.lines();
    // Skip past everything until the section header.
    let mut found = false;
    for line in lines.by_ref() {
        if line.trim() == header_needle {
            found = true;
            break;
        }
    }
    if !found {
        return Vec::new();
    }
    // Collect lines until the next `### ` header (any section) or EOF.
    let mut section_lines: Vec<&str> = Vec::new();
    for line in lines {
        if line.trim_start().starts_with("### ") {
            break;
        }
        section_lines.push(line);
    }
    // "None." sentinel: section is empty when its only non-blank content is
    // exactly `None.` (per the consensus prompt's `If none: "None."` rule).
    let nonblank: Vec<&str> = section_lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if nonblank.len() == 1 && nonblank[0].trim() == "None." {
        return Vec::new();
    }
    // Group bullets: a top-level `- ` or `* ` opens a new finding;
    // everything else attaches to the current finding.
    let mut findings: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in section_lines {
        let is_bullet = line.starts_with("- ") || line.starts_with("* ");
        if is_bullet {
            if let Some(buf) = current.take() {
                push_trimmed(&mut findings, buf);
            }
            // Strip the bullet marker for cleaner output -- callers don't
            // need to know the source was markdown.
            current = Some(line[2..].to_string());
        } else if let Some(buf) = current.as_mut() {
            buf.push('\n');
            buf.push_str(line);
        }
        // Lines before the first bullet are dropped on purpose.
    }
    if let Some(buf) = current.take() {
        push_trimmed(&mut findings, buf);
    }
    findings
}

fn push_trimmed(findings: &mut Vec<String>, buf: String) {
    let trimmed = buf.trim_end().to_string();
    if !trimmed.is_empty() {
        findings.push(trimmed);
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_verdict ----

    #[test]
    fn parse_verdict_recognises_pass() {
        let s = "blah\n### Summary\n- Blocking issues: none\n- FINAL_VERDICT: PASS\n";
        assert_eq!(parse_verdict(s), "PASS");
    }

    #[test]
    fn parse_verdict_recognises_fail() {
        let s = "review prose\n- FINAL_VERDICT: FAIL\n";
        assert_eq!(parse_verdict(s), "FAIL");
    }

    #[test]
    fn parse_verdict_unclear_when_token_missing() {
        let s = "review prose with no verdict line";
        assert_eq!(parse_verdict(s), "unclear");
    }

    #[test]
    fn parse_verdict_unclear_when_token_unknown() {
        // Token present but value is not in the {PASS, FAIL} set -- we do
        // not promote unknown values to PASS or FAIL silently.
        let s = "FINAL_VERDICT: MAYBE\n";
        assert_eq!(parse_verdict(s), "unclear");
    }

    #[test]
    fn parse_verdict_takes_first_match() {
        // Two verdict lines: we pin to the first so a stray duplicate cannot
        // flip the result. The reviewer prompt forbids duplicates anyway.
        let s = "- FINAL_VERDICT: PASS\n\nLater note:\n- FINAL_VERDICT: FAIL\n";
        assert_eq!(parse_verdict(s), "PASS");
    }

    #[test]
    fn parse_verdict_ignores_inline_substrings() {
        // The token must start a line (per prompt). An embedded mention like
        // "the FINAL_VERDICT: PASS sentinel" should not match -- avoids
        // false positives in agents that quote the rule back at us.
        let s = "the reviewer rule says emit FINAL_VERDICT: PASS once.\n";
        assert_eq!(parse_verdict(s), "unclear");
    }

    // ---- parse_pr_url ----

    #[test]
    fn parse_pr_url_extracts_full_url() {
        let s = "PR opened: https://github.com/nestrai/kuromaku/pull/220\n";
        assert_eq!(
            parse_pr_url(s).as_deref(),
            Some("https://github.com/nestrai/kuromaku/pull/220")
        );
    }

    #[test]
    fn parse_pr_url_strips_trailing_punctuation() {
        // Markdown-flavoured agent prose often wraps the URL in parens or
        // ends a sentence with `.` -- the trim guard keeps the captured
        // string usable as-is in clients.
        let s = "Done (https://github.com/owner/repo/pull/42).";
        assert_eq!(
            parse_pr_url(s).as_deref(),
            Some("https://github.com/owner/repo/pull/42")
        );
    }

    #[test]
    fn parse_pr_url_none_when_absent() {
        assert!(parse_pr_url("Review verdict unclear.").is_none());
        assert!(parse_pr_url("https://github.com/owner/repo/issues/42").is_none());
    }

    #[test]
    fn parse_pr_url_picks_first_when_multiple() {
        // Reviewer might link the issue and the PR. We pin to the first PR
        // URL so the result is deterministic.
        let s = "before https://github.com/a/b/pull/1 and https://github.com/c/d/pull/2 after";
        assert_eq!(
            parse_pr_url(s).as_deref(),
            Some("https://github.com/a/b/pull/1")
        );
    }

    // ---- Tool::call validation ----

    #[tokio::test]
    async fn implement_issue_rejects_missing_arg() {
        let tool = ImplementIssue;
        let err = tool.call(json!({})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn implement_issue_rejects_zero() {
        let tool = ImplementIssue;
        let err = tool.call(json!({"issue": 0})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn implement_issue_rejects_negative() {
        let tool = ImplementIssue;
        let err = tool.call(json!({"issue": -7})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn implement_issue_rejects_non_integer() {
        let tool = ImplementIssue;
        let err = tool.call(json!({"issue": "abc"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn implement_issue_descriptor_registers() {
        // Registry invariants (verb_noun, non-empty description, unique). The
        // schema must declare `issue` as required; otherwise MCP clients
        // would silently pass nothing and we would key off a default zero.
        let mut reg = super::super::tools::ToolRegistry::new();
        reg.register(Box::new(ImplementIssue)).unwrap();
        let descs = reg.descriptors();
        assert_eq!(descs.len(), 1);
        let desc = &descs[0];
        assert_eq!(desc.name, "implement_issue");
        assert_eq!(desc.input_schema["required"], json!(["issue"]));
        assert_eq!(desc.input_schema["properties"]["issue"]["type"], "integer");
    }

    // ---- parse_review_verdict ----

    #[test]
    fn parse_review_verdict_recognises_approve() {
        let s = "VERDICT: APPROVE\n\n### Blocking\nNone.\n";
        assert_eq!(parse_review_verdict(s), "APPROVE");
    }

    #[test]
    fn parse_review_verdict_recognises_request_changes() {
        let s = "VERDICT: REQUEST_CHANGES\n\n### Blocking\n- foo\n";
        assert_eq!(parse_review_verdict(s), "REQUEST_CHANGES");
    }

    #[test]
    fn parse_review_verdict_recognises_comment() {
        // The flow defines three verdicts; COMMENT must not silently degrade
        // to APPROVE because the issue body listed only two -- the flow is
        // the source of truth.
        let s = "VERDICT: COMMENT\n\n### Blocking\nNone.\n";
        assert_eq!(parse_review_verdict(s), "COMMENT");
    }

    #[test]
    fn parse_review_verdict_unclear_when_token_missing() {
        let s = "Some review prose without a verdict line.";
        assert_eq!(parse_review_verdict(s), "unclear");
    }

    #[test]
    fn parse_review_verdict_unclear_when_token_unknown() {
        // VERDICT line present but value outside {APPROVE, REQUEST_CHANGES,
        // COMMENT}. We do not promote unknown values -- protects against
        // typo'd verdicts being silently accepted.
        let s = "VERDICT: MAYBE\n";
        assert_eq!(parse_review_verdict(s), "unclear");
    }

    #[test]
    fn parse_review_verdict_takes_first_match() {
        // First match wins so a stray duplicate cannot flip the result.
        // The consensus prompt forbids duplicates anyway.
        let s = "VERDICT: APPROVE\n\nLater:\nVERDICT: REQUEST_CHANGES\n";
        assert_eq!(parse_review_verdict(s), "APPROVE");
    }

    #[test]
    fn parse_review_verdict_ignores_inline_substrings() {
        // Token must start a line. Embedded mention like "the VERDICT:
        // APPROVE token" should not match -- avoids false positives in
        // agents that quote the rule back at us.
        let s = "the consensus rule says emit VERDICT: APPROVE once.\n";
        assert_eq!(parse_review_verdict(s), "unclear");
    }

    #[test]
    fn parse_review_verdict_tolerates_bullet_prefix() {
        // Mirrors the implement_issue verdict regex's bullet tolerance.
        // The consensus prompt prescribes a bare `VERDICT:` line, but agent
        // drift sometimes wraps it in `- VERDICT:`; we accept that rather
        // than degrading to `unclear`.
        let s = "- VERDICT: APPROVE\n";
        assert_eq!(parse_review_verdict(s), "APPROVE");
    }

    // ---- parse_findings_section ----

    #[test]
    fn parse_findings_section_returns_empty_when_section_absent() {
        let s = "VERDICT: APPROVE\n\nNo sections at all.\n";
        assert!(parse_findings_section(s, "Blocking").is_empty());
        assert!(parse_findings_section(s, "Suggestions").is_empty());
    }

    #[test]
    fn parse_findings_section_returns_empty_for_none_sentinel() {
        // `### Blocking\nNone.\n` is the prescribed empty form.
        let s = "VERDICT: APPROVE\n\n### Blocking\nNone.\n\n### Suggestions\nNone.\n";
        assert!(parse_findings_section(s, "Blocking").is_empty());
        assert!(parse_findings_section(s, "Suggestions").is_empty());
    }

    #[test]
    fn parse_findings_section_extracts_single_bullet() {
        let s = "### Blocking\n- one finding here\n\n### Suggestions\nNone.\n";
        let f = parse_findings_section(s, "Blocking");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0], "one finding here");
    }

    #[test]
    fn parse_findings_section_keeps_continuation_lines_with_bullet() {
        // Multi-line bullet with embedded code fence -- the snippet must
        // ride along with its finding.
        let s = "### Blocking\n\
                 - `src/dag.rs:124` -- cycle detection bug.\n  \
                 ```rust\n  \
                 if visited.contains(&node.id) { return Err(Cycle); }\n  \
                 ```\n\n\
                 ### Suggestions\nNone.\n";
        let f = parse_findings_section(s, "Blocking");
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("cycle detection bug"));
        assert!(
            f[0].contains("if visited.contains"),
            "snippet must stay with finding: {}",
            f[0]
        );
    }

    #[test]
    fn parse_findings_section_splits_multiple_bullets() {
        let s = "### Suggestions\n\
                 - first finding\n\
                 - second finding\n\
                 - third finding\n\n\
                 ### Blocking\nNone.\n";
        let f = parse_findings_section(s, "Suggestions");
        assert_eq!(f.len(), 3);
        assert_eq!(f[0], "first finding");
        assert_eq!(f[1], "second finding");
        assert_eq!(f[2], "third finding");
    }

    #[test]
    fn parse_findings_section_stops_at_next_header() {
        // Suggestions section must not bleed into a following section.
        let s = "### Blocking\n\
                 - blocker A\n\
                 - blocker B\n\n\
                 ### Suggestions\n\
                 - sugg X\n";
        let blocking = parse_findings_section(s, "Blocking");
        let sugg = parse_findings_section(s, "Suggestions");
        assert_eq!(blocking.len(), 2);
        assert_eq!(sugg.len(), 1);
        assert_eq!(sugg[0], "sugg X");
    }

    #[test]
    fn parse_findings_section_drops_prose_before_first_bullet() {
        // If the agent puts freeform prose before the first bullet (against
        // prompt rules), drop it -- mixing it into a finding would be worse
        // than losing it. The prose lives in the run transcript regardless.
        let s = "### Blocking\n\
                 some preamble that should not be in a finding\n\
                 - real finding\n";
        let f = parse_findings_section(s, "Blocking");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0], "real finding");
    }

    #[test]
    fn parse_findings_section_accepts_asterisk_bullets() {
        // Markdown allows `*` as well as `-`. Agents occasionally use it.
        let s = "### Blocking\n* asterisk finding\n";
        let f = parse_findings_section(s, "Blocking");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0], "asterisk finding");
    }

    // ---- Tool::call validation ----

    #[tokio::test]
    async fn review_pr_rejects_missing_arg() {
        let tool = ReviewPr;
        let err = tool.call(json!({})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn review_pr_rejects_zero() {
        let tool = ReviewPr;
        let err = tool.call(json!({"pr": 0})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn review_pr_rejects_negative() {
        let tool = ReviewPr;
        let err = tool.call(json!({"pr": -3})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn review_pr_rejects_non_integer() {
        let tool = ReviewPr;
        let err = tool.call(json!({"pr": "abc"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn review_pr_descriptor_registers() {
        // Registry invariants: verb_noun, non-empty description, unique.
        // Schema must declare `pr` as required so MCP clients validate
        // before sending -- otherwise a missing field would key off a
        // default zero.
        let mut reg = super::super::tools::ToolRegistry::new();
        reg.register(Box::new(ReviewPr)).unwrap();
        let descs = reg.descriptors();
        assert_eq!(descs.len(), 1);
        let desc = &descs[0];
        assert_eq!(desc.name, "review_pr");
        assert_eq!(desc.input_schema["required"], json!(["pr"]));
        assert_eq!(desc.input_schema["properties"]["pr"]["type"], "integer");
    }
}
