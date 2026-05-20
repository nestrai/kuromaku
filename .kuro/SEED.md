# kuromaku seed

This seed lives inside the kuromaku repo itself and sits at the top
of the cascade over `nestrai/seeds/{rust,github,common}/`. AI
assistants working in the kuromaku repo see this seed as the
highest-priority source -- run `kuro context` for the full inventory.

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
  Overrides the generic `issue-quality` rule from `common/` with
  kuromaku-specific label conventions and verification checks.

### Flows

- **implement-issue** (graph) -- canonical issue-to-PR pipeline.
  `precheck -> design -> implement -> review -> verify -> pr`. The
  verify state runs `just lint && just test`; failure loops back to
  implement with captured output. Local override of the generic
  `implement-issue` from `github/` so kuromaku can iterate on its
  own pipeline without bumping the shared seed.
- **validate-issues** (graph) -- batch-validates open issues against
  the `issue-quality` rule and reports which ones miss criteria.

## What it does NOT provide

- Generic developer / reviewer / architect / facilitator personas
  (`Noah`, `Bella`, `Levi`, `Mika`, ...) -- those come from
  `nestrai/seeds/rust/` at lower priority.
- Cross-cutting rules (`code-review`, `clean-code`, `extensibility`,
  `git-workflow`, ...) -- those come from `nestrai/seeds/common/`.
- Provider flows (`plan-feature`, `review-pr`, `rework-pr`) --
  inherited from `nestrai/seeds/github/`.

## Cascade summary

Priority from highest to lowest:

1. `.kuro/` (this seed) -- kuromaku-specific override.
2. `~/code/nestrai/seeds/rust/` -- stack: Rust personas, rust-developer rule.
3. `~/code/nestrai/seeds/github/` -- provider: GitHub flows.
4. `~/code/nestrai/seeds/common/` -- cross-cutting: Zero, engineering and AI rules.

## Entry points

- Project config: `.kuro/config.yaml` (declares seeds, tiers, roles)
- Project guide: `.kuro/Guide.md` (injected via `--include-project-context`)
- Inventory: `kuro context` (or `kuro context --format json`)
