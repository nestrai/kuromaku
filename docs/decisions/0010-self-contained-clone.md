# ADR 0010: Self-contained public clone

- Status: accepted
- Date: 2026-09-05
- Issue: #398

## Context

A fresh public clone referenced the maintainer's filesystem (home-relative
`nestrai/seeds` checkout paths in the tracked `.kuro/config.yaml`), bound
roles to agents that only existed in a private seed library, wrote run
history to a path (`~/.koto/stacks/`) that contradicted the `.kuro/`
naming everywhere else, carried a stale hardcoded version in `flake.nix`
(2026.4.0 vs 2026.9.0 in `Cargo.toml`), and declared `license = "MIT"`
without shipping a `LICENSE` file. The `nestrai/seeds` repository is
private, so any tracked configuration depending on it fails the
fresh-clone criterion by construction.

## Decisions

### D1: Tracked seed cascade is in-repo only

`.kuro/config.yaml` declares `.kuro/` > `seeds/rust/` > `seeds/common/`.
The agents the tracked roles bind (`Noah`, `Bella`, `Levi`, `Mika`,
`Minion`) are sanitized in-repo copies under `seeds/*/agents/`; the
richer private personas stay private and can be layered back in locally
via extra seed entries or `--role` overrides. No overlay mechanism was
added -- it would be an abstraction with zero public users. Swapping back
to a submodule when `nestrai/seeds` goes public is a config-file diff
(LOW cost to undo). Locked by `tests/fresh_clone.rs` and
`tests/no_personal_paths.rs`.

### D2: Canonical stack root is `~/.kuro/stacks/`; legacy is explained, not migrated

`stack::stack_root()` returns `~/.kuro/stacks/` (supersedes the `.koto/`
pin from #176 -- pre-release is the cheapest moment this rename will ever
have). `legacy_stack_root()` (`~/.koto/stacks/`) stays readable: while it
holds data, stack-touching commands print a one-line notice with the
manual `mv` migration command, and `kuro stack purge` falls back to the
legacy root when a project only exists there. No auto-migration: moving
user data without asking violates the project's data-safety rules, and
the notice pattern mirrors the existing `.koto/` -> `.kuro/` project-dir
deprecation in `koto_config.rs`. A `kuro stack migrate` command can be
added later without breaking this contract. The project-config legacy
loaders (`.koto/config.yaml`, `koto.yaml`) stay as-is; the README's
"Legacy paths" section documents both axes.

### D3: Demo scaffolding is absent, and visibly so

`kuro-demo-python-cli/` was never tracked (excluded via the local-only
`.git/info/exclude`). The exclusion moved into the tracked `.gitignore`
so the decision travels with the repo and the directory cannot be
committed by accident. #378's recording setup uses its own clean demo
checkout.

### D4: `Cargo.toml` is the single version source

`flake.nix` reads the version via
`(builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version`,
making Cargo/nix drift structurally impossible. `kuro --version` already
follows `Cargo.toml` via clap. The remaining seams are guarded:
`just check-version` (own CI job) asserts `nix eval --raw .#kuro.version`
equals `cargo pkgid`'s version, and `just check-release-tag` (run by
`release.yml` before creating a release) refuses a `v*` tag that does not
match the crate version. Trivially revertible; permanently removes a
failure class.

## Consequences

- A fresh clone resolves `kuro context` with zero external setup.
- The "publicly accessible seed library" criterion remains repo-blocked:
  flipping `nestrai/seeds` to public is a maintainer action outside this
  repository. The README links the library and the pinning instructions
  apply to it unchanged once it is public.
- Users with pre-rename run history see a one-line notice until they
  migrate (or forever -- old data is never touched).
