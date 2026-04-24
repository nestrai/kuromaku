# Requirement Verification Pattern

## Problem

Multi-step flows can drift from the original requirement through a telephone-game effect. When requirements pass through multiple agents and intermediate artifacts (design docs, interpretations, summaries), each step introduces the risk of divergence from what the user actually asked for.

**Real-world failure:**
User runs `koto up development -t "Add caching for step outputs"`. The architect interprets "caching" as in-memory only. The implementer builds in-memory caching correctly. The reviewer confirms implementation matches the design. All agents succeed, but the user wanted disk-based caching.

The problem is undetectable when agents validate against intermediate artifacts instead of the original requirement.

## Pattern

Every agent that produces or validates output must have access to the original requirement and be explicitly instructed to verify against it, not just against intermediate artifacts.

### For Flow Authors

When designing multi-step flows:

1. **Pass original requirements to all downstream steps**
   - Use `input: [fetch]` or equivalent to ensure agents receive the raw issue text
   - The flow-level `{{task}}` is automatically injected into every step's prompt via `ctx.task`
   - Don't rely on agents to "remember" the original task from earlier in the chain

2. **Step tasks should reference the original requirement**
   - BAD: "Review the implementation against the design"
   - GOOD: "Review the implementation against the ORIGINAL task (shown at the top of this prompt), not just the design doc"
   - Be explicit about what constitutes the source of truth

3. **Implementation steps should restate before coding**
   - Include instruction: "Before implementing, restate the requirement in your own words"
   - For ambiguous requirements: "If anything is ambiguous, list your assumptions explicitly"
   - For design-based implementation: "Compare the design to the original task. If the design diverged, flag it with DIVERGENCE:"

4. **Review steps must verify against the original, not just intermediate artifacts**
   - Primary check: does the implementation solve the original problem?
   - Secondary check: does it match the design/spec?
   - If design diverged from original requirement, the review should catch this

### For Agent Role Design

Verification instructions belong in the agent role definition (system prompt), not just in individual step tasks. This ensures the behavior applies across all flows using that agent.

**Implementer agents (Noah, Kai, etc.):**
```yaml
role: "You are a senior developer. Before implementing, restate the requirement in your own words. List any assumptions you are making. If the requirement is ambiguous or the design doc diverges from the original requirement, flag it explicitly and explain the discrepancy."
```

**Reviewer agents (Bella, etc.):**
```yaml
role: "You are a code reviewer. Your primary check: does the implementation meet the ORIGINAL requirement (issue text or task description), not just the design doc or intermediate artifacts? If the design diverged from the original requirement, flag it -- a correct implementation of a wrong design is still wrong. Secondary checks: correctness, edge cases, test coverage, project conventions."
```

When an agent role includes verification behavior, flow-specific step tasks can reference it but don't need to repeat the full instruction. The role provides the baseline behavior; the step task provides flow-specific context (e.g., "the original task is shown at the top of this prompt").

## Reference Implementation

The `implement-issue` flow demonstrates this pattern:

- `fetch` step retrieves raw issue text
- `implement` step receives `input: [fetch]` and includes restatement instruction
- `review` step receives `input: [fetch, implement]` to access both the original issue and implementation
- `verify` step explicitly checks whether the implementation satisfies the issue's acceptance criteria

When creating new flows or modifying existing ones, use `implement-issue` as the reference for requirement-verification behavior.

## Anti-Patterns

### Validating against intermediate artifacts only

```yaml
review:
  role: reviewer
  input: [design, implement]
  task: "Review the implementation against the design"
```

This creates a validation chain where the design becomes the source of truth. If the design misinterpreted the original requirement, the error is invisible.

### Missing original requirement in later steps

```yaml
implement:
  role: developer
  # Missing: input: [fetch] or equivalent
  task: "Implement the feature based on the design doc"
```

The implementer has no access to the original requirement and cannot detect divergence.

### Implicit assumptions without flagging

```yaml
implement:
  role: developer
  task: "Implement the feature based on the design doc"
  # Missing: instruction to restate and flag ambiguity
```

The implementer makes assumptions silently. When the user's intent differed, there's no record of what was assumed or when the divergence occurred.

## Validation Checklist

When reviewing a flow for requirement-verification compliance:

- [ ] Do implementation steps have access to the original requirement (via `input` or `ctx.task`)?
- [ ] Do implementation step tasks include restatement instruction?
- [ ] Do review steps have access to the original requirement?
- [ ] Do review step tasks prioritize original-requirement verification over design-doc validation?
- [ ] Are agent role definitions aligned with this behavior (not contradicting it)?
- [ ] If a design/planning step exists, does the implementation step explicitly compare design to original task?

If any of these are missing, the flow is vulnerable to requirement drift.

## BLOCKED Output Pattern

Implementer agents (Noah, Kai, etc.) include pre-implementation validation that can block execution when designs are not implementable.

### When BLOCKED is emitted

An implementer outputs `BLOCKED: [category]` when:

- **AMBIGUOUS**: Design has ambiguities that require clarification (missing error handling spec, unclear scope boundaries, unspecified edge cases)
- **INCOMPLETE**: Design is missing required details to implement (no function signatures, missing data structures, unspecified dependencies)
- **CONFLICTS_WITH_CONVENTIONS**: Design violates project conventions or rules (e.g., rust-developer rule says "no silent unwraps" but design specifies returning null on error)
- **DIVERGED_FROM_REQUIREMENT**: Design does not solve the original problem (telephone-game drift detected)

### Output format

```
BLOCKED: CONFLICTS_WITH_CONVENTIONS
- Design specifies returning Option<T> for errors, but rust-developer rule requires Result<T, E>
- Design uses manual loops where iterators would be idiomatic
```

The implementer lists all issues found, not just the first one. This allows the design step to address everything in one iteration.

### Flow handling

When a step outputs BLOCKED, the orchestrator should:

1. **Do not proceed to downstream steps** -- the implementation did not happen
2. **Route back to design** -- pass the BLOCKED output as feedback to the design agent
3. **Re-run implementation** after design is revised

Example flow structure:

```yaml
design:
  agent: Levi
  task: "Design the feature..."

implement:
  agent: Noah
  input: [design]
  task: "Implement the design..."
  # Noah validates design before implementing
  # If BLOCKED, output contains the issues

review:
  agent: Bella
  input: [implement]
  # Only runs if implement produced code, not BLOCKED
```

Current limitation: koto does not yet support conditional routing based on output content. For now, flows that receive BLOCKED output will fail at the review step (no code to review). The BLOCKED output in the step's result indicates what needs to be fixed.

### Noah's Validation Checklist

Before implementing, Noah verifies:

1. **Completeness**
   - Are error cases specified?
   - Are edge cases covered?
   - Are function signatures and return types clear?
   - Is the scope bounded?

2. **Convention compliance**
   - Does the design follow rust-developer rules?
   - Does it match existing project patterns?
   - Are there contradictions with the codebase?

3. **Requirement alignment**
   - Does the design solve the original problem?
   - Did interpretation drift introduce divergence?

If all checks pass, Noah proceeds to implementation. If any check fails, Noah outputs BLOCKED with the category and specific issues.
