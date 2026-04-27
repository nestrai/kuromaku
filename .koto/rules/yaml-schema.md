# YAML and Schema Best Practices

## YAML Authoring

### Structure
- Use consistent indentation (2 spaces, never tabs).
- Prefer block style over flow style for readability: `key:\n  - item` over `key: [item]`.
  Exception: short lists (1-3 scalar items) can use flow style when it improves scanning.
- Group related fields together. Separate groups with a blank line.
- Order fields by importance to the reader: identity fields first (name, title), behavior fields next (role, rules), metadata last (model, version).

### Comments
- Comment WHY, not WHAT. `# Required by the LLM orchestrator` is useful. `# The name field` is noise.
- Do not comment every field. If the field name is self-explanatory, skip the comment.
- Use comments for: non-obvious constraints, references to external decisions, warnings about gotchas.

### Multi-line strings
- Use `|` (literal block) for content that preserves newlines (prompts, markdown, scripts).
- Use `>` (folded block) for long single-paragraph descriptions.
- Never use quoted strings for multi-line content.

### Anti-patterns
- **Stringly-typed values**: `enabled: "true"` instead of `enabled: true`. Use native YAML types.
- **Deep nesting**: more than 3 levels deep is a design smell. Flatten or restructure.
- **Unnamed anchors**: anchors like `&a` are unreadable. Use descriptive names: `&default_model`.
- **Implicit defaults**: if a field has a default, document it in the schema. Users should not have to read source code to learn defaults.

## Schema Design

### Field definitions
- Every field has: type, description, and whether it is required or optional.
- Optional fields document their default value.
- Enums list all valid values. Use `additionalProperties: false` unless extensibility is intentional.
- String fields with constraints (regex, min/max length, format) must document them.

### Validation
- Validate early, fail loudly. A silent ignore of an unknown field is a silent bug.
- Error messages reference the field path AND the constraint that failed: "agent.rules[0]: rule 'nonexistent' not found in .koto/rules/" -- not just "validation error."
- Collect all errors before reporting. Do not stop at the first one.

### Evolution
- Adding optional fields is backwards-compatible. Adding required fields is not.
- When deprecating a field: accept both old and new for one version, emit a warning, remove in the next.
- Version the schema if the format changes in breaking ways.

### Documentation from schema
- The schema IS the reference doc for the config format. Keep descriptions in the schema, not in a separate doc that drifts.
- Generate reference docs from the schema where possible. Hand-written reference docs and schemas diverge.

## CI Integration

- Lint all YAML files in CI (yamllint, schema validation).
- Validate against the schema, not just syntax. A syntactically valid YAML file can still violate the schema.
- Test example configs: every example in docs should be validated in CI to prevent drift.
