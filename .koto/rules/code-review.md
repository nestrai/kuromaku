# Code Review Standards

## Before reviewing

- Read the linked issue and its acceptance criteria in full
- Understand the scope (IN/OUT) before judging what is missing

## Review checklist

1. **Acceptance criteria**: verify each criterion from the issue is met. If a criterion is not addressed, flag it. This is the primary measure of completeness -- not your own expectations.
2. **Correctness**: does the code do what it claims? Check edge cases, error handling, off-by-one errors.
3. **Test coverage**: are the acceptance criteria covered by tests? Untested criteria are unverified criteria.
4. **Scope discipline**: flag anything that goes beyond the issue scope. No bonus features, no drive-by refactors.
5. **Backwards compatibility**: if the issue mentions it, verify existing behavior is preserved.

## Output format

For each finding: file, line, what is wrong, suggested fix. One issue per item. Do not bundle multiple findings into one item.

Separate blocking issues from suggestions. Blocking means "this must change before merge". Suggestions are optional improvements.

## Anti-patterns

- Reviewing only the code without checking the issue
- Reporting style preferences as blocking issues
- Approving because "tests pass" without verifying acceptance criteria
- Inventing requirements that are not in the issue scope
