# ADR-0007: Per-project stack purge

**Status:** accepted
**Date:** 2026-05-04
**Issue:** [#232](https://github.com/nestrai/kuromaku/issues/232)
**Epic:** [#225](https://github.com/nestrai/kuromaku/issues/225) (regulatory readiness)

## Context

Run artifacts persist under `~/.koto/stacks/<project>/<run-id>/` and accumulate indefinitely. When a kuromaku user is acting as a GDPR controller and receives an Art. 17 erasure request, they need a documented mechanism to remove all stack data tied to a single project. The same command serves the everyday engineering use of clearing test runs without touching unrelated history.

The issue raised three open questions for design to resolve before code:

1. What if run-log entries are central rather than per-project?
2. Soft delete or hard delete?
3. Should the deletion itself be audited?

## Decision

Ship `kuro stack purge <project>` as a hard delete, with `--dry-run` for preview and `--yes` for non-interactive use. Validate the project name as a single path segment, canonicalise both root and project before removal, and refuse anything whose canonical path escapes the stack root.

### Resolutions to the open questions

**Run-log centralisation.** No central run log exists today; per-project deletion of `~/.koto/stacks/<project>/` is sufficient. If a central log lands later (tracked alongside the run-log issue), it inherits a follow-up that hooks into the same purge entry point.

**Soft vs hard delete.** Hard delete. Art. 17 expects erasure, not archival, and a `--soft` flag is straightforward to add later if a real workflow asks for it.

**Audit trail of the deletion.** Deferred. The controller's record-keeping obligations are jurisdiction- and contract-specific; baking a one-shape-fits-all `~/.koto/erasure-log.yaml` into the binary would be wrong for users whose obligations need different fields, and writing nothing is wrong for users who need anything. Today: erasure leaves no record by design, and the README points users to OS-level audit (journald, auditd, etc.) if their controller policy requires evidence. A purpose-built audit-log feature can be added behind an opt-in config flag once a concrete obligation surfaces.

### Subcommand layout

`kuro stack purge` rather than `kuro purge`. The `state` component label on the issue and the `kuromaku-state` crate split sketched in the v1 architecture (ADR-002) both point at more stack operations -- `kuro stack list`, `kuro stack show`. Putting `purge` under a `stack` parent now keeps the namespace clean when those siblings land. The cost of an extra word per invocation is modest; the cost of relocating a flat `kuro purge` later would land on every user's muscle memory.

### Validation

Project names must be a single path segment: no `/`, `\`, NUL, no `..`, no leading `.`. The string-shape gate runs first so a malformed name fails before any filesystem call. The canonical-containment check runs after, so a well-formed name pointing at a symlink that escapes the root is refused too. Both checks live in the library functions (`stack::plan_purge`, `stack::purge_project`) -- the CLI must not be the only line of defence, since future MCP exposure or scripted callers must inherit the same safety net.

### Out of scope

* Glob deletion (`kuro stack purge "test-*"`) -- a foot-gun without a real ask.
* MCP exposure of purge -- destructive operations triggered by remote agents need a separate threat model. Tracked as follow-up.
* Concurrency with in-flight runs -- a `kuro run` against the project being purged will fail mid-stream. Acceptable for v1 if documented.
* `kuro stack list` -- own design surface (sort order, columns, JSON output), separate issue.

## Consequences

* The `.koto/` home root in `~/.koto/stacks/` is now resolved through a single `stack::stack_root()` helper. The runner consumes it via `runner::resolve_stack_path`. A future relocation has one source of truth.
* Removing a project's stack is destructive and irreversible. The README documents this and the `--dry-run` flag is the recommended preview path.
* Future erasure-audit work (if it lands) can hook into the same `purge_project` call site without changing the CLI surface.

## Reversibility

* Hard-delete -> soft-delete: easy. Add `--soft` later, drop into a `<project>/.trash/` move.
* Subcommand layout: hard. Choose `kuro stack` now; do not relitigate.
* Audit-trail deferral: easy. Add an opt-in flag and writer when the obligation lands.
