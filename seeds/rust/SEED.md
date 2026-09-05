# seeds/rust -- Rust-stack seed bucket

This bucket contributes the Rust-stack personas and the canonical GitHub
PR lifecycle flows. It sits below `.kuro/` in the tracked cascade and
above `seeds/common/` (see `.kuro/config.yaml`).

## Agents

- **Noah** -- Senior Rust developer. Implements features and fixes with
  tests. Reads neighboring code before writing new code. Outputs a
  structured close with branch, commits, tests run, and open items.
- **Bella** -- Code reviewer. Reviews changes against the issue's
  acceptance criteria. Separates BLOCKING from SUGGESTIONS; finding
  zero issues in a first draft is flagged as suspicious.
- **Levi** -- Software architect. Designs module boundaries and
  interfaces. Names trade-offs explicitly; "no downsides" is treated as
  incomplete analysis. Output: decision, affected files, trade-offs,
  edge cases, open questions.

## Flows

- **implement-issue** (graph) -- issue-to-PR loop with state routing.
  The kuromaku repo overrides this with a richer copy in `.kuro/flows/`.
  See `seeds/rust/flows/README.md` for the graph-vs-linear comparison.
- **review-pr** (linear) -- fetch -> architecture + code-review ->
  consensus. Produces a single GitHub-ready PR comment with a verdict
  (APPROVE | REQUEST_CHANGES | COMMENT). Used by the `review_pr` MCP
  tool; step id `consensus` and its output tokens are pinned by that
  tool's parser.
- **rework-pr** (linear) -- fetch -> fix -> verify -> push. Applies
  Blocking review comments as fixup commits and gates the push on
  FINAL_VERDICT from the verify step. Used by the `rework_pr` MCP tool;
  step ids `fix` and `verify` and their output tokens are pinned by that
  tool's parser.

## Note on bucket placement

`review-pr` and `rework-pr` are GitHub-provider flows (they call `gh`).
They live here rather than in a dedicated `seeds/github/` tier because
that tier does not exist yet in this tracked cascade. Moving them to a
provider tier later is cheap; restructuring the cascade before the first
public release is not. See `.kuro/SEED.md` for the full cascade picture.

## Cross-links

- `.kuro/SEED.md` -- top-level seed inventory and cascade description
- `seeds/rust/flows/README.md` -- per-flow description and linear-vs-graph guide
- `seeds/common/SEED.md` -- cross-cutting personas (Mika, Minion)
