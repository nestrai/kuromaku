# In-tree example flows (GitHub provider)

This directory ships example flow files alongside the kuromaku binary.
They demonstrate the supported flow shapes; the canonical day-to-day
flows live in the external seeds repo at
`~/code/nestrai/seeds/github/flows/`.

The bucket is `github/` because the flows here are GitHub-specific:
they shell out to `gh` for issue and PR operations. Future provider
buckets (`gitlab/`, `jira/`) will mirror the same flow shapes against
their own CLIs.

## Flows in this directory

- `implement-issue.yaml` -- graph flow (`states:` / edges). Drives the
  issue-to-PR loop where the reviewer routes back to `design` or
  `implement` depending on what they found, instead of a single
  PASS/FAIL gate.

## Linear vs graph

See `docs/graph-flows.md` for the full picture. Short version: pick a
linear `flow:` for fixed pipelines (no rework loops), pick a graph
`states:` when an agent needs to choose between multiple recovery
paths.
