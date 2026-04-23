# Test Coverage Definition: Issue #89

## Overview

Issue #89 adds pre-implementation validation to Noah's agent. Tests verify that Noah correctly identifies broken/incomplete designs before starting implementation. All tests are **manual verification** due to LLM non-determinism -- we define expected behavior, not automated assertions.

---

## AC1: Noah's YAML includes validation instructions

**What to test:**
- The `.koto/agents/Noah.yaml` file contains the 3-point validation checklist in the `role` field
- The validation logic requires Noah to check for: (1) ambiguities/missing details, (2) conflicts with conventions, (3) BLOCKED: prefix when issues found
- The `rules` field references `rust-developer` (convention source) and `git-workflow`

**How to verify:**
- Parse `.koto/agents/Noah.yaml` as valid YAML
- Check `role` field contains:
  - "Before implementing, validate the design"
  - The 3-point checklist (ambiguities, conventions, BLOCKED)
  - Instruction to stop if issues found
  - Instruction to proceed if valid (restate requirement, list assumptions)
- Check `rules: [rust-developer, git-workflow]` is present

**Edge cases:**
- YAML syntax errors (invalid indentation, unclosed quotes)
- Missing required fields (`name`, `title`, `role`, `rules`)
- Validation text present but incomplete (missing one of the three checks)
- BLOCKED: prefix mentioned but not linked to validation outcome
- Validation check present but no instruction to stop/proceed based on result

---

## AC2: Design violates rust-developer rule → BLOCKED

**What to test:**
- Noah detects when a design contradicts project conventions (rust-developer rule)
- Noah outputs `BLOCKED:` prefix followed by specific violation details
- Noah does NOT proceed to implementation when BLOCKED

**How to verify:**

Run the `development` flow with a design document containing convention violations:

**Test input example:**
```markdown
## Design: Error Handling for Config Parser

Requirement: Parse YAML config file, return error if invalid.

Design: Create a `parse_config(path: &str)` function that:
- Reads the file
- Parses YAML
- Returns `null` if parsing fails
- Returns the parsed config struct on success
```

**Expected output:**
Noah's response contains:
- `BLOCKED:` prefix at the start
- Specific violation cited: "Design violates rust-developer rule: functions cannot return null in Rust, must use Result<T, E>"
- Reference to the violated convention (line 6 of rust-developer.md: "All Results must be handled")
- NO implementation code generated
- NO test code generated

**Edge cases:**
- **Multiple violations:** Design violates both error handling AND style rules (e.g., "return null" + "use abbreviations for variable names"). Expect: Noah lists ALL violations, not just the first.
- **Subtle violation:** Design says "silently skip errors" instead of explicit null. Expect: Noah catches this as violating "no silent unwraps."
- **Violation in acceptance criteria, not design:** Requirement says "return null," but design interprets it as Result. Expect: Noah flags the discrepancy between requirement and design, suggests clarifying the requirement.
- **Borderline case:** Design uses `.unwrap()` in test code (allowed) vs library code (not allowed). Expect: Noah distinguishes context, only blocks on library code unwrap.

**What NOT to test:**
- Whether Noah's implementation is good -- we're not testing implementation quality, only validation behavior.
- Multi-turn correction -- if Noah outputs BLOCKED, the flow should stop. Retry logic is out of scope.

---

## AC3: Incomplete design → BLOCKED

**What to test:**
- Noah detects when design has missing details (undefined error handling, unspecified edge cases, unclear state transitions)
- Noah lists the specific missing details
- Noah outputs `BLOCKED:` prefix

**How to verify:**

Run the `development` flow with an incomplete design document:

**Test input example:**
```markdown
## Design: Add --verbose flag to CLI

Requirement: Users should be able to enable verbose output.

Design: Add a `verbose: bool` field to the CLI args struct. When enabled, print more information.
```

**Expected output:**
Noah's response contains:
- `BLOCKED:` prefix
- List of missing details:
  - "What specific information should verbose mode print?"
  - "Where in the codebase should verbose logging be added?"
  - "Should verbose mode affect all commands or only some?"
  - "How should verbose output be formatted (stdout, stderr, structured logging)?"
- NO implementation code

**Edge cases:**
- **Partially complete design:** Design specifies error handling but not edge cases. Expect: Noah flags missing edge cases, proceeds if error handling is sufficient for starting implementation.
- **"TBD" placeholders:** Design says "error handling TBD." Expect: Noah outputs BLOCKED, lists error handling as missing.
- **Vague language:** Design says "handle errors appropriately." Expect: Noah flags as ambiguous, asks for specific error handling strategy.
- **Implicit details:** Design assumes reader knows the context (e.g., "use the standard pattern"). Expect: Noah either (a) infers from project patterns and lists assumption, OR (b) flags as ambiguous and asks for explicit specification.
- **Over-specification:** Design includes every edge case, error path, test scenario (1000+ words). Expect: Noah proceeds, does not flag as incomplete.

**Boundary between "missing" and "assumed":**

Noah should flag missing details when:
- Error handling is not specified
- Edge cases are not addressed
- State transitions are unclear
- Integration points are undefined

Noah should NOT flag when:
- Design relies on established project patterns (e.g., "use color_eyre::Result" without defining Result)
- Details can be inferred from the requirement and existing codebase
- Implementation details are left to the developer (e.g., "choose appropriate data structure")

When in doubt, Noah should list the assumption explicitly rather than blocking.

---

## AC4: Valid design → Proceeds to implementation

**What to test:**
- Noah proceeds to implementation when design is complete and valid
- Noah restates the requirement in his own words (existing behavior)
- Noah lists assumptions (existing behavior)
- Noah does NOT output `BLOCKED:`
- Noah generates implementation code and tests

**How to verify:**

Run the `development` flow with a complete, valid design:

**Test input example:**
```markdown
## Design: Add --dry-run flag to `up` command

Requirement: Users should be able to preview what `koto up` will do without executing.

Design:
- Add `dry_run: bool` field to `UpCommand` struct in `src/main.rs`
- Parse `--dry-run` flag via clap derive API
- In `run_up()` function (src/main.rs), check `dry_run` before executing each step
- If dry_run is true:
  - Print "DRY RUN: would execute <step name>" to stdout
  - Skip actual step execution
  - Proceed to next step
- Error handling: dry_run does not affect error parsing, only execution
- Tests:
  - Unit test: `run_up` with dry_run=true does not call executor
  - Integration test: `koto up --dry-run` outputs expected "would execute" messages

Acceptance criteria:
- `koto up --dry-run` prints all steps that would execute
- `koto up --dry-run` does not modify stack state
- `koto up --dry-run` exits with code 0 if flow is valid
```

**Expected output:**
Noah's response contains:
- Restatement of requirement: "Add a --dry-run flag that shows what koto up would do without executing steps."
- List of assumptions: "Assuming dry-run should print to stdout, not modify any files, and use the existing step ordering logic."
- NO `BLOCKED:` prefix
- Implementation code: UpCommand struct modification, clap attribute, run_up logic
- Test code: unit test and integration test as specified in design

**Edge cases:**
- **Minimal design:** Design is short but covers all necessary points (error handling, edge cases, tests). Expect: Noah proceeds.
- **Detailed design:** Design is very detailed (500+ words, diagrams, pseudocode). Expect: Noah proceeds, implementation follows design closely.
- **Design explicitly addresses validation points:** Design says "Error handling: use color_eyre::Result. Edge case: empty flow file handled by returning early. Conflicts: none, follows rust-developer rule." Expect: Noah acknowledges, proceeds.
- **Design diverges from requirement:** Requirement says "add --verbose flag," design proposes "--dry-run flag." Expect: Noah flags divergence (existing behavior per current role), asks for clarification. This is NOT a BLOCKED scenario -- it's a "design interpretation" flag.

**Boundary between "valid" and "incomplete":**

A design is valid when:
- Error handling strategy is defined (even if minimal, e.g., "propagate errors with ?")
- Core logic is described (not pseudocode, but clear enough to implement)
- Edge cases are acknowledged (even if the handling is "return error")
- Tests are specified (even if just "test happy path + one error case")

A design is incomplete when ANY of the above are missing.

---

## AC5: Documentation explains checklist and BLOCKED handling

**What to test:**
- Documentation describes Noah's 3-point validation checklist
- Documentation explains what `BLOCKED:` means
- Documentation explains how flows should handle `BLOCKED:` output (human reads it, decides next step)

**How to verify:**

Check that documentation exists and is accessible:
- **Inline documentation:** Noah's `role` field in `.koto/agents/Noah.yaml` contains the validation logic (self-documenting)
- **PR description:** PR for issue #89 explains the validation behavior and provides examples
- **Optional:** User-facing guide (e.g., `.koto/docs/agent-validation.md`) if created

Documentation should cover:
- What Noah checks before implementing (the 3 validation points)
- What `BLOCKED:` output looks like (example snippet)
- What users should do when they see `BLOCKED:` in flow output (review design, fix issues, re-run)
- That `BLOCKED:` is a marker for humans, not an engine-level flow control mechanism (current limitation)

**Edge cases:**
- Documentation exists but is incomplete (mentions BLOCKED but not the checklist)
- Documentation is buried (exists in code comments but not discoverable by users)
- Documentation contradicts the implementation (says Noah checks 4 points, YAML only has 3)

**Acceptance threshold:**
- PR description is mandatory and sufficient for AC5
- Inline role text in Noah.yaml counts as documentation
- Additional standalone docs are optional (design review suggests skipping `.koto/docs/` file)

---

## Test Execution Plan

**Pre-implementation:**
1. Validate current `.koto/agents/Noah.yaml` does NOT contain validation logic (baseline)

**Post-implementation:**
1. Verify AC1: Parse YAML, check structure
2. Run AC2 test: Design with "return null" violation → expect BLOCKED
3. Run AC2 edge case: Design with multiple violations → expect all listed
4. Run AC3 test: Design missing error handling → expect BLOCKED with specific gaps
5. Run AC3 edge case: Design with "TBD" placeholders → expect BLOCKED
6. Run AC4 test: Complete, valid design → expect implementation code generated
7. Verify AC5: Read PR description, check coverage of validation behavior

**Evidence collection:**
- Capture Noah's output for each test run
- Include in PR description as "Verification results"
- Note any unexpected behavior (e.g., Noah proceeds when expected to block, or vice versa)

**Handling non-determinism:**
- If Noah's output varies across runs (sometimes blocks, sometimes proceeds on same input), this is a failure
- The validation logic should produce consistent behavior for the same input
- If inconsistency occurs, the prompt engineering needs strengthening (more explicit instructions, few-shot examples)

---

## Out of Scope

**Not testing:**
- Whether Noah's generated code is correct (implementation quality)
- Whether the flow engine routes based on BLOCKED output (structural flow changes, future issue)
- Whether other agents (Levi, Rio, Kai) validate their inputs (separate issues)
- Performance (token usage, latency)

**Why manual tests, not automated:**
- LLM output is non-deterministic (same prompt can yield different responses)
- Testing LLM behavior requires human judgment (is this "BLOCKED" output clear and actionable?)
- Automated tests would be brittle (string matching on LLM output is fragile)
- These tests verify a prompt engineering change -- the "test" is "does Noah behave as specified" not "does this function return X"

**When to automate:**
- If koto gains a test harness for agent behavior (future enhancement)
- If validation logic moves into engine code (Rust functions, not prompt text)
- If acceptance criteria require regression testing across many scenarios
