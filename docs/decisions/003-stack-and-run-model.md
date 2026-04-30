# ADR-003: Stack and Run Model for Audit-Grade Architecture

**Status:** accepted
**Date:** 2026-04-29
**Supersedes:** parts of ADR-002 (the run-ID and stack layout sections, where applicable)
**Tracking:** epic #161, sub-issues #162 -- #167

## Context

koto's positioning rests on a single differentiator: **audit-grade reproducibility of multi-agent runs**. A user must be able to ask "how was issue 42 solved over time?" and reconstruct the answer from the persisted state alone -- which flows ran, in what order, with what inputs, producing which outputs.

The current architecture cannot deliver this:

- Run directories are flat: every `koto up` creates `<stack>/<flow>-YYYYMMDD-HHmmss/`. Runs that belong to the same logical thread (implement-issue, then review-pr, then rework-pr for the same issue) live next to each other but have no persistent identity tying them together. There is no first-class concept of "the work on issue 42".
- Run IDs are timestamps in local time with a TOCTOU collision-loop. They are not globally unique and not safely sortable across DST or across machines.
- Storage is hardcoded to `std::fs::*` calls scattered across `stack`, `runner`, and `main`. Hosted koto (#37) and S3 backends (or any cloud storage) are blocked at the foundation.
- The provider layer is GitHub-only by convention, with `gh` CLI calls hardcoded in `src/notify/github.rs`. Cross-provider work (GitLab, Jira, Azure DevOps, Linear) would require finding every leak and rewriting it.
- `koto task` accepts free-form text. There is no audit binding from a task to an issue; if a user writes "fix the bug in issue 42" in the task prompt, koto cannot reconstruct that linkage later without unreliable text parsing.
- No project-level guarantee that runs are audit-capable. Users who care about audit must remember the right invocation every time.
- Manifests are unversioned and silently editable, so an audit trail can be tampered with or, more commonly, become unreadable after a schema change.

These are not separate problems. They share a root cause: the current architecture treats a koto run as a leaf of the filesystem, when it should be a node in a structured, versioned, provenance-tracked graph.

## Decision

### 1. Two-layer Stack/Run model (Pulumi-style)

Replace the flat run directory with a two-layer model:

- **Stack** is the persistent identity. It corresponds to a logical thread of work: an issue, a PR, a free-form task, a named flow run.
- **Run** is a single execution under a Stack. Multiple Runs accumulate over a Stack's lifetime.

```
<stacks-root>/
  <stack-id>/
    metadata.yaml                # stack identity, source ref, status
    runs/
      <ulid>/
        manifest.yaml            # what flow ran, when, by whom, audit policy snapshot
        steps/                   # per-step content + meta
        messages/                # inter-agent messages (#153)
        resolution-audit.txt
```

Stack-ID format depends on the kind:

| Kind  | Format                                  | Example                          |
|-------|-----------------------------------------|----------------------------------|
| issue | `issue-<provider>-<scope-flat>-<id>`    | `issue-gh-nestrai-koto-42`       |
| pr    | `pr-<provider>-<scope-flat>-<id>`       | `pr-gh-nestrai-koto-160`         |
| task  | `task-<localdate>-<localtime>`          | `task-20260429-143000`           |
| flow  | `flow-<flow-name>-<localdate>-<localtime>` | `flow-research-20260429-143000` |

The full canonical reference (with provider semantics) lives in `metadata.yaml`. Stack-IDs are filesystem-safe.

### 2. ULID for Run identity

Run IDs are [ULIDs](https://github.com/ulid/spec): 26 characters, lexicographically sortable by time, globally unique without coordination, no TOCTOU loop.

Human-readable timestamps (local timezone, with flow name) are rendered in the UI from `manifest.yaml.started_at`. The ID itself is the ULID.

### 3. Storage abstraction trait

A `Storage` trait abstracts persistence. `LocalStorage` is the only implementation initially; S3, GCS, hosted-koto-cloud, and Pulumi-Cloud-style HTTP backends become alternative implementations without core code changes.

All stack and run operations go through `Storage`. Direct `std::fs::*` calls outside the `LocalStorage` impl become a CI-enforced rule.

The trait operates on `StorageKey` (forward-slash separated, no parent traversal) rather than `&Path`, so cloud-native key schemes work without translation.

### 4. Provider abstraction with canonical reference schema

Issue and PR references use a canonical schema: `<provider>:<scope>#<id>` for issues, `<provider>:<scope>!<id>` for PRs / merge requests. Examples:

- `gh:nestrai/koto#42`
- `gh:nestrai/koto!160`
- `gl:group/project#42`
- `jira:PROJ-42`
- `linear:TEAM-42`

A `Provider` trait dispatches on the prefix. `GitHubProvider` is the only implementation initially. Other providers' parsers exist for round-trip tests but their fetch/post operations are explicitly out of scope until concrete demand surfaces.

The reference schema lives in metadata.yaml. Stack-IDs derive a filesystem-safe form, but the canonical reference is the source of truth.

### 5. Explicit `--issue` flag, no heuristic detection

`koto task` does not parse free-form text for issue references. Two paths:

- With `--issue <ref>`: the run lands as a Run under the corresponding issue Stack. Provider resolves the reference and verifies the issue exists before the run starts.
- Without `--issue`: the run lands under a fresh `task-<timestamp>` Stack and koto prints a warning that the run is not audit-capable.

The free-text parsing approach (looking for `#42`, URLs, Jira keys in the prompt) was rejected: false positives produce wrong audit bindings, which are worse than no binding at all. Audit must be deterministic.

### 6. `audit-trail` enforcement mode

Project-level switch in `koto.yaml`:

```yaml
audit-trail: warn      # off | warn | enforce
```

- `off`: no warnings, runs without issue binding land in `task-*` stacks silently
- `warn` (default): warning printed when no issue binding, run still proceeds
- `enforce`: koto refuses to start a run that would not be audit-capable

The active mode at run start is recorded in the run's `manifest.yaml` as `audit_policy_active`. This is the snapshot at the moment the run started -- if `koto.yaml` changes later, past runs still show what policy was in force when they ran.

### 7. Manifest immutability and schema versioning

All koto-written YAML carries `schema_version: 1`. Writers always emit current; readers support known versions; unknown future versions error clearly.

Once a run is finalized, koto refuses to overwrite its manifest, metadata, or step files. A second `koto up` always produces a new run ID; finalized runs are read-only from the writer's perspective.

Optional, opt-in stronger guarantees (filesystem-readonly chmod, hash-chain across runs in a stack) are configurable but off by default. Cryptographic signing is out of scope for this ADR.

## Alternatives considered

### Alternative A: Single-layer flat runs (status quo, refined)

Keep the flat layout, just add a `metadata.yaml` per run that mentions the issue and a query command that scans all runs. Rejected because:

- Reconstruction requires scanning every run for a match -- O(N) on every audit query, with no index
- No first-class "issue 42 status" -- the status would have to be derived from scanning all runs
- Tasks and issues live in the same flat namespace, making categorization implicit

### Alternative B: Run ID = Issue ID

Use the issue ID directly as the run ID. The user proposed this. Rejected because:

- One issue produces multiple runs over time (implement, review, rework). Single-ID-per-issue collapses this and forces a suffix scheme that recreates the two-layer model unnamed
- Issue IDs are repo-scoped (`#42` is ambiguous without a repo); cross-repo work breaks
- Tasks and ad-hoc flows have no issue ID; they would need a parallel scheme

### Alternative C: Heuristic detection of issue references in task text

Parse free-form task prompts for `#42`, URLs, Jira-style keys, and auto-attach to the corresponding stack. Rejected because:

- False positives ("issue 42 is irrelevant, ignore" -> wrong binding) corrupt the audit trail
- LLM-based detection (have an agent classify the text) makes the audit itself dependent on a non-deterministic AI output
- Conservative interactive confirmation works for a human at a terminal but breaks in CI / non-interactive contexts

The chosen approach (explicit flag with optional warning) sacrifices some convenience for audit determinism. This is the right trade-off for koto's positioning.

### Alternative D: GitHub-only, hardcoded

Keep `gh` CLI calls scattered across the codebase, defer cross-provider work to "when someone asks for it". Rejected because:

- Stack-IDs are persisted to disk and embedded in run manifests; changing the schema later is a data migration, not a code change
- The provider abstraction is small enough to design correctly now; retrofitting it later costs more
- Cross-provider in implementation is explicitly out of scope -- only the schema and trait need to be cross-provider-shaped now

### Alternative E: UUID instead of ULID for run IDs

UUIDs (v4) are unsorted; UUIDs (v7) are sorted but newer and less universally supported. ULID was chosen for sortability + simplicity + ecosystem maturity. UUID v7 was a near-tie; ULID won on existing crate quality and brevity in directory names.

## Consequences

### Positive

- "How was issue 42 solved?" becomes a one-line query: `ls stacks/issue-gh-nestrai-koto-42/runs/`
- Cloud storage and hosted koto unblock at the foundation
- Cross-provider users can adopt without forking
- Audit-critical projects can hard-enforce binding via `audit-trail: enforce`
- Manifest format can evolve without breaking readers of older runs
- Tampering with a finalized run is detectable (writer refuses; optional hash-chain catches external edits)

### Negative

- Migration: existing repos with flat runs need a one-shot `koto migrate stacks` command. Back-compat reader handles un-migrated state but new layout is the default.
- Onboarding cost: users must learn the Stack vs Run distinction. Mitigated by good defaults (most users never see Stack-IDs in normal use; the CLI handles them).
- More directories on disk per run: fine for filesystem; potentially noisy for cloud storage that meters list operations -- mitigated by manifest-as-index rather than directory scans.
- ULID directory names are 26 chars and not human-readable. Mitigated by always rendering human-readable timestamps in UI; ULIDs only show in audit output and absolute paths.

### Implementation order

Tracked as sub-tasks under epic #161:

1. #162 Storage abstraction trait
2. #163 Provider abstraction + reference schema
3. #164 Two-layer stack/run model with ULIDs (depends on 1+2)
4. #165 `--issue` flag for `koto task` (depends on 3)
5. #166 `audit-trail` enforcement (depends on 3+4)
6. #167 Manifest immutability and schema versioning (refines 3)

The broader refactoring tracked in RFC #143 (Backend trait, workspace split, model identification) is independent of this ADR and proceeds on its own timeline.
