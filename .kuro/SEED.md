# kuromaku seed

This seed lives inside the kuromaku repo itself and overlays the
generic `nestrai/seeds/rust/` cascade with kuromaku-specific
artefacts. AI assistants working in the kuromaku repo see this seed
as the highest-priority source -- run `kuro context` for the full
picture.

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
  its own pipeline without bumping the shared seed.
- **validate-issues** (graph) -- batch-validates open issues against
  the `issue-quality` rule and reports which ones miss criteria.

## What it does NOT provide

- Generic developer / reviewer / architect / facilitator personas
  (`Noah`, `Bella`, `Levi`, `Mika`, ...) -- those come from the
  `nestrai/seeds/rust/` seed at lower priority.
- Generic rules (`code-review`, `clean-code`, `extensibility`, ...) --
  same story.
- Plan / review / rework flows (`plan-feature`, `review-pr`,
  `rework-pr`) -- inherited from the rust seed.

## Entry points

- Project config: `.kuro/config.yaml` (declares seeds, tiers, roles)
- Project guide: `.kuro/Guide.md` (injected via `--include-project-context`)
- Inventory: `kuro context` (or `kuro context --format json`)
