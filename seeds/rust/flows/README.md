# In-tree example flows

This directory ships example flow files alongside the kuromaku binary.
They demonstrate the supported flow shapes. Together with the agents in
`seeds/rust/agents/` they form the in-repo `seeds/rust/` bucket of the
tracked cascade (see `.kuro/config.yaml`), so a fresh clone resolves
without any external seed checkout.

## Flows in this directory

- `implement-issue.yaml` -- graph flow (`states:` / edges). Drives an
  issue-to-PR loop where the reviewer routes back to `design` or
  `implement` depending on what they actually found instead of a single
  PASS/FAIL gate. The kuromaku repo itself overrides this flow with the
  higher-priority copy in `.kuro/flows/`.

## Linear vs graph

See `docs/graph-flows.md` for the full picture. Short version: pick a
linear `flow:` for fixed pipelines (no rework loops), pick a graph
`states:` when an agent needs to choose between multiple recovery
paths.
