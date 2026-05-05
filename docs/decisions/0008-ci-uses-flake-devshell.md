# ADR-0008: CI runs jobs inside the flake devShell

**Status:** accepted
**Date:** 2026-05-05
**Issue:** [#303](https://github.com/nestrai/kuromaku/issues/303)
**Related:** [#288](https://github.com/nestrai/kuromaku/issues/288) (CI epic), [#294](https://github.com/nestrai/kuromaku/pull/294) (the PR this replaces the toolchain setup from)

## Context

The first version of `.github/workflows/ci.yml` (added in #294) installed the Rust toolchain through `dtolnay/rust-toolchain@stable`, the cargo cache through `Swatinem/rust-cache@v2`, and `just` through `extractions/setup-just@v3`. Locally, every contributor uses `nix develop`, which loads the toolchain (rust + clippy + rustfmt + rust-analyzer) from `flake.nix` via `rust-overlay`, with the exact revision locked in `flake.lock`. `just` is part of that same devShell.

That left two different runtimes for the same `just <recipe>` invocations:

- Local: flake.lock-pinned toolchain, deterministic across machines and over time.
- CI: floating "stable" resolved by `dtolnay/rust-toolchain` at workflow-run time, plus a separately versioned `setup-just`.

Concrete failure modes this opens up:

- A new clippy lint that lands in upstream stable breaks CI without any code change on our side.
- `flake.lock` becomes meaningless for CI runs; only local builds honor it.
- Any toolchain bump has to be done in two places (flake + workflow pin).
- `just` is duplicated -- once in the devShell, once via `extractions/setup-just`.

This violates the project's workflow rule that local and CI must run identical commands against identical toolchains.

## Decision

CI runs each job inside the project's flake devShell:

```yaml
- uses: actions/checkout@v4
- uses: cachix/install-nix-action@v27
  with:
    extra_nix_config: |
      experimental-features = nix-command flakes
- uses: DeterminateSystems/magic-nix-cache-action@v8
- run: nix develop --command just <recipe>
```

Same shape for `lint`, `build`, and `test`. Three separate jobs, parallel, one recipe per job. No `dtolnay/rust-toolchain`, no `Swatinem/rust-cache`, no `extractions/setup-just`.

A header comment in `.github/workflows/ci.yml` points back at this ADR and at #303 so the next contributor doesn't "helpfully" revert to the standard Rust action.

### Why magic-nix-cache and not Swatinem/rust-cache

`magic-nix-cache-action` caches the Nix store, which is where the entire toolchain plus `just` plus any flake-built tooling lives. After a warm cache, the `nix develop` setup step is fast and the cargo target dir benefits from the runner's own incremental cache inside the Nix-store path. Layering `Swatinem/rust-cache` on top is possible if measurements show the 5-minute-per-job budget is breached, but it is not added speculatively -- the issue explicitly asks for the flake path to be confirmed first.

### Why three jobs, not one matrix

Each concern stays a separate job per the project's workflow rule: a clippy regression, a build break, and a test failure must each surface as a distinct check. Collapsing into one job with three steps would lose that signal granularity.

## Consequences

- Local and CI run the same toolchain. Reproducing a CI failure locally needs no version detective work.
- Bumping Rust is a `nix flake update` (or a flake-input pin change), tracked in `flake.lock`, in one place.
- Cold CI runs are slower than the previous setup -- the first job on a new branch evaluates the flake and fetches the toolchain from substituters. `magic-nix-cache-action` warms subsequent runs to within the issue's ~5-minute budget per job.
- CI now depends on Determinate Systems' hosted Nix cache. If that becomes a problem (availability, policy, or self-hosted-only requirement), the fallback is `cachix/cachix-action` with a project-owned cache, or accepting cold-run timings without any Nix-store cache.

## Reversibility

- Switching back to `dtolnay/rust-toolchain`: trivial mechanically, but reintroduces the local==CI drift this ADR exists to remove. Do not do it without a new ADR superseding this one.
- Replacing `magic-nix-cache-action` with `cachix/cachix-action`: easy, no other changes needed; the `nix develop --command just <recipe>` shape stays.
- Adding `Swatinem/rust-cache` on top: easy, drop the action above the `run` step; only do this once a measurement shows the cache budget is missed.

## Out of scope

- Building the binary with `nix build` (separate concern, owned by the `flake.nix` `packages` story).
- Self-hosted runners.
- Caching strategy beyond what `magic-nix-cache-action` provides.
