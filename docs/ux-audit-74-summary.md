# UX Audit Summary: .koto/ Content and CLI Design

**Issue:** #74  
**Auditor:** Luna (UX Engineer agent)  
**Date:** 2026-04-23

## Overview

This audit evaluated koto's user experience from the perspective of naming consistency, discoverability, and CLI ergonomics. The focus was on how users interact with flows, agents, and roles through the command-line interface.

## Audit Scope

**IN:**
- Flow naming consistency and overlap
- Role naming patterns across flows
- Step naming consistency
- CLI subcommand design (up, task, pull, down, status)
- Discoverability mechanisms (how users find flows/agents/roles)

**OUT:**
- Prompt quality or AI behavior (covered by #62)
- Rust implementation details
- Future features not yet implemented

## Key Findings

### High Priority (Blocks basic workflows)

1. **Role/agent terminology confusion** (#77)
   - Error messages don't explain the difference between roles (abstract) and agents (concrete)
   - Users typing `implementer=Noah` vs `Noah=true` get unhelpful errors
   - Fix: Improve error message to list available roles and suggest correct syntax

2. **No flow discovery mechanism** (#78)
   - Users must trigger an error to see available flows
   - `koto --help` doesn't explain what flows are
   - Fix: Add `koto list` command to show flows and agents with descriptions

3. **Inconsistent role names** (#83)
   - Same job has different names: `developer` vs `implementer`
   - Breaks muscle memory when switching between flows
   - Fix: Standardize to `developer` everywhere, document in GUIDE.md

### Medium Priority (Creates friction)

4. **Step naming inconsistency** (#79)
   - Steps have unpredictable names (`implement` vs `fix`, `review` vs `verify`)
   - Forces memorization instead of pattern recognition
   - Fix: Establish standard vocabulary, rename `verify` -> `review`

5. **Flow name overlap** (#81)
   - `fix-issue` and `fix-pr` both use "fix" but mean different things
   - Ambiguous which flow to use for "fixing issue 35"
   - Fix: Rename to `issue` and `review-fixes`

6. **No role inspection** (#82)
   - Users must read YAML to discover what roles a flow has
   - No way to see what agents are available for override
   - Fix: Add `koto inspect <flow>` command

7. **"up/down" metaphor mismatch** (#80)
   - Borrowed from Docker Compose but koto flows are batch jobs
   - `koto down` is a stub because there's nothing to stop
   - Fix: Rename `up` -> `run`, remove or repurpose `down`

### Low Priority (Polish)

8. **"task" terminology overload** (#84)
   - `koto task --task "task"` repeats "task" with different meanings
   - Subcommand name and flag name are the same
   - Fix: Rename `--task` -> `--prompt`

9. **Missing workflow documentation** (#85)
   - Four flows but no explanation of when to use which
   - No indication that flows represent a progression (feature -> PR -> review -> fixes)
   - Fix: Add workflow guide to GUIDE.md, group flows in `koto list` output

## Issues Created

All findings have been filed as separate issues:

- #77: UX: Improve role/agent terminology in error messages (High, cli)
- #78: UX: Add 'koto list' command for flow/agent discovery (High, cli, enhancement)
- #79: UX: Standardize step naming across flows (Medium, cli, documentation)
- #80: UX: Rename 'koto up' to 'koto run' for clearer semantics (Medium, cli, breaking)
- #81: UX: Rename flows to avoid 'fix-' prefix overlap (Medium, cli, breaking)
- #82: UX: Add 'koto inspect' command to show flow details (Medium, cli, enhancement)
- #83: UX: Standardize role names across all flows (High, cli, config)
- #84: UX: Rename --task flag to --prompt for clarity (Low, cli, breaking)
- #85: UX: Document workflow progression and flow relationships (Medium, documentation)

## Quick Wins

These issues have high impact and low implementation effort:

1. **#77** - Improve role/agent error messages (15 min)
2. **#79** - Rename `verify` -> `review` in fix-pr.yaml (5 min)
3. **#83** - Rename `implementer` -> `developer` in fix-issue.yaml (5 min)
4. **#85** - Add workflow guide to GUIDE.md (30 min)

Total quick win time: ~1 hour to address major user pain points.

## Breaking Changes

Three issues involve breaking changes to the CLI:

- #80: `koto up` -> `koto run`
- #81: Flow renaming (`fix-issue` -> `issue`, `fix-pr` -> `review-fixes`)
- #84: `--task` -> `--prompt`

Recommendation: Bundle these into a single release (v0.2?) with deprecation warnings for one version cycle before removing old names.

## Dependencies

- #85 depends on #78 (list command) and #81 (flow renaming)
- All issues are otherwise independent and can be implemented in any order

## Recommendation

Prioritize in this order:

1. Quick wins (#77, #79, #83, #85) - 1 hour total, high user impact
2. Discoverability (#78, #82) - Critical for onboarding
3. Breaking changes (#80, #81, #84) - Bundle into v0.2 release
4. Polish (#85 CLI grouping) - After list command exists

## Files Analyzed

- `.koto/flows/development.yaml`
- `.koto/flows/fix-issue.yaml`
- `.koto/flows/fix-pr.yaml`
- `.koto/flows/review-pr.yaml`
- `src/main.rs` (CLI definition, lines 25-92, 154-161, 425-428)

## Notes

This audit focused on user-facing UX. No AI engineering or prompt quality issues were evaluated (those were covered in issue #62: Neo audit).

The findings reflect real user confusion patterns documented in UX research:
- Inconsistent naming breaks pattern recognition (Don Norman, "Design of Everyday Things")
- Discoverability failures force users to read documentation (anti-pattern for CLI tools)
- Metaphor mismatches create false mental models (Docker Compose "up/down" doesn't fit batch jobs)

---

*This audit was conducted by Luna (UX Engineer agent) as part of issue #74.*
