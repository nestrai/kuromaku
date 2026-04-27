# Clean Code and Engineering Discipline

## Code is read more than it is written

Every line of code is read dozens of times after it's written once. Optimize for the reader, not the writer.

### Language idioms

Write idiomatic code for the language at hand. Do not import patterns from other languages.
- Go: accept interfaces, return structs. Short variable names in small scopes. Error values, not exceptions.
- Python: generators, comprehensions, context managers. Type hints. snake_case.
- Rust: iterators and combinators over manual loops. Ownership, not garbage collection patterns.
- YAML: block style, 2-space indent, comments explain WHY not WHAT.

If the code looks like Java written in Python, it's wrong. If the Go code has try/catch patterns, it's wrong.

### Functions

- Small. One thing. If it needs "and" in the description, split it.
- Descriptive names that don't need comments. `calculate_monthly_revenue` not `calc` or `process_data`.
- Few parameters. No boolean flags that change behavior -- split into two functions instead.
- No magic numbers or strings. Named constants or it's a bug waiting to happen.

### Formatting

Formatting is a tool's job, not a human's.
- Every project has a formatter configured: rustfmt, gofmt, black, prettier.
- The formatter runs in pre-commit hooks AND in CI. No exceptions.
- Style discussions in code reviews are banned. The formatter decided. Move on.
- If the language has a linter with auto-fix (clippy, golangci-lint, ruff), use it.

### Automation

If you do it manually more than once, automate it.

Preferred toolchain (in order):
1. **just** -- task runner for all projects. `just build`, `just test`, `just lint`, `just fmt`.
2. **nix** -- reproducible environments and builds. `nix develop` for dev shells.
3. **make** -- acceptable fallback if just/nix aren't available.
4. **shell scripts** -- last resort. Must be shellcheck-clean.

CI and local must run identical commands. If `just test` passes locally but CI runs something different, one of them is lying.

### Reproducibility

- Same inputs = same outputs, regardless of machine.
- Dev environments via nix flakes or devcontainers, not "install these 12 things manually."
- Pinned dependencies. Lock files committed. No `latest` tags in production.
- Dotfiles managed (chezmoi), configs versioned, nothing lives only on one machine.

### Design for change

- Depend on abstractions, not concretions.
- Separate concerns: business logic, I/O, configuration, presentation.
- Design for N instances from the start. If it works with 1, adding a second should be a config change, not a code change.
- But: don't over-abstract. Three similar lines are better than a premature abstraction. The right complexity is the minimum needed for the current task.
