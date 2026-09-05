# In-tree flows

This directory ships flow files alongside the kuromaku binary. Together
with the agents in `seeds/rust/agents/` they form the in-repo
`seeds/rust/` bucket of the tracked cascade (see `.kuro/config.yaml`),
so a fresh clone resolves without any external seed checkout.

The three flows here form the canonical GitHub issue-to-PR lifecycle:
`implement-issue` implements, `review-pr` reviews, `rework-pr` applies
the feedback.

## Flows in this directory

- `implement-issue.yaml` -- **graph** flow (`graph:` / edges). Drives an
  issue-to-PR loop where the reviewer routes back to `design` or
  `implement` depending on what they actually found instead of a single
  PASS/FAIL gate. The kuromaku repo overrides this with a richer copy in
  `.kuro/flows/`; this file is the generic Rust-stack version.
- `review-pr.yaml` -- **linear** flow, 4 steps. Fetches a PR, runs
  parallel architecture and code-review passes, then distils the findings
  into a single GitHub-ready comment with a VERDICT line. Step id
  `consensus` and its output tokens are pinned by the `review_pr` MCP
  tool (see `src/mcp/workflow.rs`).
- `rework-pr.yaml` -- **linear** flow, 4 steps. Checks out a PR branch,
  applies Blocking review comments as `git commit --fixup` commits,
  verifies the diffs, and gates the push on `FINAL_VERDICT: DONE`. Step
  ids `fix` and `verify` and their output tokens are pinned by the
  `rework_pr` MCP tool.

Only `implement-issue.yaml` has a companion `.md` file
(`implement-issue.md`). The markdown twin illustrates the graph-flow
prose format for documentation purposes; `review-pr` and `rework-pr` are
linear flows and do not need one.

## Linear vs graph

See `docs/graph-flows.md` for the full picture. Short version: pick a
linear `flow:` for fixed pipelines (no rework loops), pick a `graph:`
when an agent needs to choose between multiple recovery paths.

`review-pr` and `rework-pr` are linear because each step has exactly one
successor -- no agent decides a branch. `implement-issue` is a graph
because the reviewer can route back to `design` or `implement` depending
on what they found.
