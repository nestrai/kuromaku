# AGENTS.md

Instructions for AI agents working in this repository. Read this file fully before starting any task.

## Project

kuromaku (CLI: `kuro`) -- reproducible AI agent teams. Rust, tokio, clap.
Repo: `nestrai/kuromaku` on GitHub.

Legacy naming: the config directory is `.kuro/` (formerly `.koto/`). Both names appear in the codebase.

## Session start: discover the seed cascade

Run `kuro context` once at the start of any session in a kuromaku-managed repo. This prints the resolved seed cascade -- which seeds are active, which agents/rules/flows each one contributes, and which version wins after conflict resolution.

```
kuro context                 # human-readable inventory
kuro context --format json   # stable v1 wire format for AI clients
```

Do NOT invent new agents or rules without checking whether the cascade already provides them. The kuromaku ecosystem is built around reuse across seeds; duplicating agents here defeats that. If `kuro context` is unavailable (older binary, kuro not on PATH), read each seed's root `SEED.md` directly -- those summarise what the seed offers.

## Build

Requires Rust stable (see `rust-toolchain.toml`). Optionally use `nix develop` for a pinned toolchain.

```
just build      # cargo build
just test       # cargo test
just lint       # cargo clippy -D warnings + cargo fmt --check
just fmt        # cargo fmt
just release    # cargo build --release (produces ./target/release/kuro)
```

Run a specific test: `cargo test <test_name>`.
Fix all lint issues before pushing. `just lint` runs both clippy and format checks.

After code changes that affect CLI behavior, run `just release` so the local `kuro` binary reflects the latest code. This is the dogfooding workflow -- the maintainer uses the locally built binary to run flows against the repo itself.

## Workflow

The full lifecycle for every issue is:

```
implement-issue  ->  review-pr  ->  rework-pr (if needed)  ->  merge
```

1. **implement-issue**: Create a branch, implement the change, open a draft PR.
2. **review-pr**: Review the PR against the issue's acceptance criteria. Post a verdict.
3. **rework-pr**: If the review says REQUEST_CHANGES, fix the feedback as fixup commits.
4. **merge**: After APPROVE, the maintainer squashes and merges.

This cycle repeats until the review passes. Every PR gets reviewed before merge, no exceptions.

### Implementing an issue

1. Read the issue: `gh issue view <N> --comments`. Understand the full description and acceptance criteria before writing any code.
2. Start from main: `git fetch origin && git checkout origin/main`.
3. Create a feature branch: `git checkout -b <type>/<short-slug>-<N>` (e.g. `feat/transport-trait-168`).
4. Implement the change. Keep commits small and focused.
5. Run `just lint` and `just test`. Fix any failures.
6. Push and open a draft PR: `gh pr create --draft --title "<type>(<scope>): <description>" --body "Closes #<N>"`.

### Reviewing a PR

1. Read the PR diff, all comments, and the linked issue.
2. Check the implementation against the issue's acceptance criteria. Missing criteria are blocking.
3. Review for correctness, edge cases, test coverage.
4. Post findings as a PR review. End with `VERDICT: APPROVE`, `REQUEST_CHANGES`, or `COMMENT`.

### Reworking a PR after review

1. Read all review comments.
2. Fix each issue as a fixup commit: `git commit --fixup=<original-SHA>`.
3. Push. Do not amend, force-push, or squash. The maintainer squashes after approval.

## Git rules

- Never commit directly to main. Always use a feature branch.
- Conventional Commits: `<type>(<scope>): <description>`.
- Fixup commits for review fixes, not amends.

### Commit message examples

Good: `feat(resolver): add cycle detection for DAG edges`
Good: `fix(config): handle missing seeds path gracefully`
Good: `test(stack): add coverage for empty manifest`
Bad: `update code`
Bad: `fix stuff`
Bad: `feat: implement the thing from issue 42`

## Code style

- Functional/procedural Rust, no OOP patterns.
- `thiserror` for library errors, `color-eyre` for application.
- Iterators and combinators over manual loops.
- Small, focused functions.
- No emojis in code, docs, commits, or output.

## Seeds

Shared agent definitions, flows, and rules come from an external seeds repository. The local `.kuro/` directory can override any seed file. Local takes priority.

You do not need to modify seeds to work on kuromaku itself.

## Stack

Run outputs are written to `~/.koto/stacks/<project>/<run-id>/`. The `.koto/` home root is intentional legacy (pinned, see #176) even though the config directory is `.kuro/`. You rarely need to inspect these directly.

## Boundaries

- Never close, reopen, or reassign issues.
- Never force-push unless explicitly asked.
- Never modify AGENTS.md, CLAUDE.md, or CI config unless the issue requires it.
- Never add dependencies to Cargo.toml without explicit approval.
- If the issue is ambiguous, ask in a PR comment rather than guessing.
