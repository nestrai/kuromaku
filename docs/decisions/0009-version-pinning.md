# ADR-0009: Artifact audit by content hash, seed pinning deferred to git

**Status:** proposed
**Date:** 2026-05-26
**Issue:** [#227](https://github.com/nestrai/kuromaku/issues/227)
**Related:** [#225](https://github.com/nestrai/kuromaku/issues/225) (regulatory readiness epic), [#161](https://github.com/nestrai/kuromaku/issues/161) (audit architecture), [#164](https://github.com/nestrai/kuromaku/issues/164) (stack/run model), [#166](https://github.com/nestrai/kuromaku/issues/166) (audit-trail enforce mode), [#167](https://github.com/nestrai/kuromaku/issues/167) (manifest immutability), [#37](https://github.com/nestrai/kuromaku/issues/37) (hosted koto / v2 ecosystem)

## Context

Seeds are referenced from `.kuro/config.yaml`, today only by local path.
The seed schema already carries a remote form: `RawSeed` parses `repo:`
and `ref:` into `SeedSource::Remote`, but resolution is a deliberate stub
that returns `remote seeds not yet implemented` (`src/koto_config.rs:445`).

#227 asks for "version pinning" so the same flow behaves the same over
time, and so a run records exactly which artifact versions produced it
(EU AI Act Art. 11 + Annex IV, Art. 15 -- see #225). Before designing a
mechanism it is worth being precise about the failure this is meant to
prevent, because the triggering incident was misdiagnosed.

The triggering incident was the `generic/ -> coding/common/` and
`rust/flows/ -> github/flows/` seed restructure, which left
`ai-topic-digest` pointing at a `seeds/rust/` path that no longer existed.
That does **not** fail silently. `from_raw_entries` checks
`expanded.exists()` at parse time and returns a hard
`seed path "..." does not exist` validation error before any agent runs
(`src/koto_config.rs:398`). That is the correct, safe behaviour. So the
reported pain is "config load errored with a clear message," not a silent
behaviour change.

Separating the real concerns:

1. **Loud missing path** -- already handled. Not a bug, no work needed.
2. **Silent wrong-winner** -- a bucket is renamed but a *same-named*
   artifact still exists elsewhere in the cascade, so a different agent
   wins via "earlier entry wins" with no error. This is the genuinely
   silent case. It is detected after the fact by recording what actually
   resolved, not prevented by a remote pin.
3. **Reproducibility and audit** -- "run against the seed library as it
   was at commit X" and "record which exact artifact ran." A real want,
   but distinct from the incident, and only this one needs remote
   sourcing.

The cheap, regulatory-relevant half (audit) serves concerns 2 and 3 for
*every* source. The expensive half (a git fetch/checkout/cache resolver)
serves only concern 3, only for remote seeds, which do not exist at
launch. This ADR ships the first and defers the second.

## Decision

### 1. Audit by content hash (v1, ship now)

Every agent, rule, and flow loaded during a run is hashed (SHA-256 of
file bytes) and the hash is recorded in the run record, regardless of
source (project `.kuro/`, local `path:` seed, or future remote). `sha2`
is already a dependency (`Cargo.toml:30`); the run/stack record already
exists (`src/stack.rs`). The manifest schema and immutability are owned
by #164 / #167; this ADR only requires that the per-artifact hash is
captured.

This answers "which exact artifact ran?" deterministically for the
Annex IV record, and it is the detection mechanism for concern 2
(a `kuro context` of two checkouts shows a changed hash where a silent
wrong-winner swapped in).

Surface it through the existing command, not a new one. `kuro context`
already resolves which artifact each name points at
(`src/context.rs` `resolve()`), and `--format json` is the stable v1
wire format (per CLAUDE.md). Extend that output with the content hash and
the resolved source per artifact. Do **not** add a separate `kuro resolve`
command -- that would be two commands that both print the resolved
configuration.

### 2. Interim reproducibility with git's own machinery (v1, zero code)

Until remote sourcing is built, pinning a shared seed library against a
known revision is done with git, not with kuro: add the seed repo as a
**submodule**, or point a `path:` entry at a **checked-out tag** of it.
Git pins the tree; a restructure on the seed repo's `main` cannot reach
the consumer until it deliberately updates the submodule. This needs no
new kuro code and is documented as the recommended pattern.

### 3. Remote seed resolver + pinning (deferred to its own ADR, v2 ecosystem)

The existing `repo:` / `ref:` schema is the seam. Implementing it means a
git fetch/checkout into a SHA-keyed cache, `subdir` extraction,
credential handling for private repos, and -- only if floating refs
(`ref: main`, `ref: v1.2.0`) prove necessary -- a lock file recording the
resolved SHA. That is a subsystem, it is unscheduled relative to the
publish milestone, and `#227` carries no v1 milestone. It belongs with
the hosted/registry work (#37), in a dedicated ADR.

When it is built, the design intent recorded here: prefer a full SHA in
`ref:` (immutable, committed in `config.yaml`, no lock file) for the
simple case; introduce a lock file only to pin floating refs. Reuse the
content hash from decision 1 for tamper detection of the fetched tree.

### Design questions from #227, answered

- **Versioning model:** content hash for audit (now); git ref for remote
  pinning (deferred). No hand-maintained semver as the source of truth --
  dozens of tiny files make hand-semver pure drift and toil. An optional
  advisory `version:` frontmatter field MAY exist for human changelogs but
  is never resolved or enforced.
- **Where does version live for in-repo `.kuro/` artifacts:** nowhere as a
  field. They are versioned by the project's own git history; their
  content hash goes in the run record.
- **Lock file:** not in v1. A full SHA in `ref:` is already immutable and
  committed. A lock file earns its keep only for floating refs against
  remote repos, which is deferred work.
- **Override semantics:** unchanged. Earlier `seeds:` entry wins; the
  project `.kuro/` shadows a same-named seed artifact. The run record's
  content hash captures whichever artifact actually won.
- **No semver constraint solver.** The cascade is an ordered override
  list, not a transitive dependency graph; a solver is over-engineering.

## Consequences

- The audit requirement is met for all artifact sources in v1, cheaply,
  with a dependency already present. This is the load-bearing,
  trust-and-adoption-relevant piece, and it fits the publish window.
- The silent wrong-winner case becomes detectable (hash diff across
  checkouts) rather than invisible.
- No git-resolver subsystem is built on the publish clock; the launch
  audience (local `path:` users, `kuro init`) is unaffected by its
  absence, and the existing stub error keeps remote configs honest.
- Reproducibility against a shared seed lib is available immediately via
  submodule / pinned `path:`, with no new code to maintain.

## Reversibility and out of scope

- Schema is untouched: keep `repo:` / `ref:` as already parsed. This ADR
  does not introduce a `git:` key (that would be a needless rename of an
  existing field).
- Not the run manifest schema itself (#164 / #167) -- this ADR only
  requires a per-artifact hash is recorded there.
- Not the remote resolver, cache, credentials, or lock file -- deferred
  to a dedicated ADR (decision 3).
- Reverting decision 1 is trivial: stop recording the hash. It adds no
  config surface and no new dependency.

## Review

This ADR was rewritten after a devil's-advocate pass against the codebase.
The original draft claimed the seed restructure "silently breaks"
consumers and proposed a git-source resolver + lock file + cache as v1.
The review established three corrections, all code-grounded: (a) a missing
`path:` fails loudly at parse time, so the headline justification was
false; (b) the resolver is unscheduled scope on a 5-day publish clock and
does not help the local-`path:` launch audience; (c) the schema already
has `repo:`/`ref:`, and `kuro context` already does the resolve work, so
`git:` and a new `kuro resolve` command were needless additions. The scope
was cut to the cheap, load-bearing half (content-hash audit) with remote
pinning deferred. The semver-solver rejection and content-hash-for-audit
choice survived the review unchanged.

## Open questions for sign-off

- Confirm the v1/deferred split: is shipping audit-by-hash now, and
  deferring the resolver, the right call against the publish goal?
- Should the interim "submodule or pinned `path:`" pattern be promoted in
  the README / reference card as the supported way to pin seeds?
- Naming for the deferred lock file, when it lands: `kuro.lock` at repo
  root vs `.kuro/lock.yaml`.
