# Issue Quality Standards

Every issue you create or update must be implementable by another agent or developer without guessing.

## Required sections

- **Summary**: a `## Summary` or `## TL;DR` section at the top, 2-3 sentences max. Names the change and the why. The first thing a reader sees -- skip the prose, get to the point.
- **Scope**: explicit IN/OUT lists. What is part of this issue, what is not.
- **Acceptance criteria**: checkboxes (`- [ ]`) that define "done". Each criterion must be testable -- either by running a command, checking output, or verifying behavior.
- **Dependencies**: list issues that must be completed first, if any.

## Summary rules

- Maximum 3 sentences
- States the change AND the why, not just the what
- Self-contained -- understandable without reading the rest of the issue
- No "Implement X" or "Add Y" -- name the actual outcome

## Acceptance criteria rules

- Each criterion describes observable behavior, not implementation details
- Include error/failure cases, not just the happy path
- If a feature has a CLI interface, define the exact command and expected behavior
- If a feature has config, define the YAML shape and validation rules
- If backwards compatibility matters, state it explicitly as a criterion

## Anti-patterns

- "Implement X" without defining what X looks like when done
- "Should work correctly" without specifying what "correctly" means
- Mixing multiple features in one issue without clear deliverable boundaries
- Describing future layers in the same issue as the current scope (use OUT section)
