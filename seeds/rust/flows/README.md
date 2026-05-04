# In-tree example flows

This directory ships example flow files alongside the kuromaku binary.
They demonstrate the supported flow shapes; the canonical Rust-team
flows used day-to-day live in the external seeds repo at
`~/code/nestrai/seeds/rust/flows/`.

## Flows in this directory

- `implement-issue-graph.yaml` -- graph flow (`states:` / edges). Drives
  the same issue-to-PR loop as the linear `implement-issue.yaml` in the
  external seeds repo, but lets the reviewer route back to `design` or
  `implement` depending on what they actually found instead of a single
  PASS/FAIL gate.

## Linear vs graph

See `docs/graph-flows.md` for the full picture. Short version: pick a
linear `flow:` for fixed pipelines (no rework loops), pick a graph
`states:` when an agent needs to choose between multiple recovery
paths.
