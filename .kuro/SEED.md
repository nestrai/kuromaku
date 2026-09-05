# kuromaku seed

This seed lives inside the kuromaku repo itself and sits at the top of
the tracked, fully in-repo cascade (`.kuro/` > `seeds/rust/` >
`seeds/common/`, declared in `.kuro/config.yaml`). AI assistants
working in the kuromaku repo see this seed as the highest-priority
source -- run `kuro context` for the full picture.

## What it contributes

### Agents

- **Babis** -- design engineer & AI team orchestrator. The
  steering/architect persona for kuromaku itself; shadows the
  generic `Babis` in lower-priority seeds if one ever lands.

### Rules

- **issue-quality** -- defines the implementation-readiness gate
  used by the `precheck` state of `implement-issue`. The rule lists
  the labels and content checks that mark a GitHub issue as ready
  to start; flows fail the precheck if those criteria are not met.

### Flows

- **implement-issue** (graph) -- canonical issue-to-PR pipeline.
  `precheck -> design -> implement -> review -> verify -> pr`. The
  verify state runs `just lint && just test`; failure loops back to
  implement with captured output. Local override of the generic
  `implement-issue` from `seeds/rust/` so kuromaku can iterate on
  its own pipeline without touching the shared seed.
- **validate-issues** (graph) -- batch-validates open issues against
  the `issue-quality` rule and reports which ones miss criteria.

## What the lower-priority in-repo seeds provide

- `seeds/rust/` -- the Rust-stack bucket: the generic developer /
  reviewer / architect personas (`Noah`, `Bella`, `Levi`), the example
  `implement-issue` graph flow, and the two canonical GitHub PR lifecycle
  flows (`review-pr`, `rework-pr`). See `seeds/rust/SEED.md`.
- `seeds/common/` -- the cross-cutting bucket: facilitator and
  fetcher personas (`Mika`, `Minion`). See `seeds/common/SEED.md`.

These are sanitized, self-contained copies (#398): the tracked
cascade resolves from a fresh clone with no external directories.
A richer private persona library can be layered on top locally via
additional seed entries or `--role` overrides.

## Entry points

- Project config: `.kuro/config.yaml` (declares seeds, tiers, roles)
- Project guide: `.kuro/Guide.md` (injected via `--include-project-context`)
- Inventory: `kuro context` (or `kuro context --format json`)
