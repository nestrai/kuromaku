# Kuromaku GraphFlow Architecture Direction

Kuromaku must treat workflow input formats as replaceable authoring frontends, not as part of the execution engine.

The stable internal contract is `GraphFlow`.

All input formats, such as YAML and Markdown, must be parsed into the same `GraphFlow` model before validation or execution starts. The validator, runtime, and manifest generation must not depend on whether the workflow originally came from YAML, Markdown, or any future input source.

## Core idea

The architecture should follow this separation:

```text
Authoring / Input Layer              Engine Layer
────────────────────────             ─────────────────────────────

.md     -> markdown parser ──┐
                              ├──> GraphFlow -> Validator -> Runtime -> Manifest
.yaml   -> yaml parser ───────┘

future:
.lua / .star / UI / API / SDKs -> GraphFlow
```

`GraphFlow` is the boundary between authoring convenience and execution behavior.

Everything left of `GraphFlow` is replaceable input handling.

Everything right of `GraphFlow` is stable engine behavior.

Adding a new input format must mean adding a new parser or adapter that emits `GraphFlow`. It must not require changes to the validator, runtime, or manifest logic.

## Current implementation scope

For now, only the existing YAML format and the new Markdown format need to be considered.

Do not implement Python, TypeScript, Lua, Starlark, UI, or API support now.

However, the structure should not block those additions later.

The current work should prepare the codebase so that future input sources can be added cleanly by implementing a `GraphFlow` producer.

## Responsibilities

### Parser / input adapters

Parsers are responsible for reading one specific input format and producing a `GraphFlow`.

They may validate syntax that is specific to the input format.

Examples:

- YAML parser validates that the YAML structure can be read.
- Markdown parser validates that the Markdown workflow syntax is well-formed.
- A future SDK adapter would build `GraphFlow` directly from language bindings.

Parsers must not contain general workflow validation rules that belong to the engine.

### GraphFlow

`GraphFlow` is the canonical in-memory representation of a workflow.

It should contain all execution-relevant information:

- workflow name
- version or format version if needed
- initial state
- global prompt / description
- states
- state type: role task, shell run, or final state
- task text
- shell command
- final message
- outgoing edges
- edge target
- edge reason

`GraphFlow` should not preserve presentation-only details from the original source format unless they are needed for diagnostics.

Markdown spacing, comments, and visual separators are authoring concerns, not engine concerns.

### Validator

The validator operates only on `GraphFlow`.

It must be format-agnostic.

It should validate semantic workflow rules such as:

- the initial state exists
- all edge targets exist
- state names are unique
- states are reachable where required
- final states have no outgoing edges
- shell states define exactly two outcomes: `pass` and `fail`
- role/task states have valid task content
- invalid cycles or terminal conditions are detected if required by the engine model

The validator must not inspect the original YAML or Markdown source.

### Runtime

The runtime executes validated `GraphFlow`.

It must not know whether the workflow came from Markdown, YAML, or any other future input.

The runtime receives a validated graph and performs state execution, routing, shell execution, context passing, and agent invocation.

### Manifest

Manifest generation records what happened during execution.

It must be based on runtime events and the validated `GraphFlow`, not on the original source format.

The manifest should remain stable across input formats.

The same workflow expressed in YAML and Markdown should produce equivalent execution behavior and equivalent manifest structure.

## Future SDK direction

Python and TypeScript should not be treated as additional "config file formats".

If Python or TypeScript support is added later, it should follow a Pulumi-like model:

- users choose a language binding or SDK
- the SDK builds a `GraphFlow`
- the same validator and runtime execute that `GraphFlow`

This means Python and TypeScript are integration APIs, not files that the CLI merely parses as configuration.

A future SDK may either:

- build `GraphFlow` and call the core engine directly
- build `GraphFlow` and submit it to a daemon/API
- build `GraphFlow` and serialize it for the CLI to execute

The important rule is that SDKs must not reimplement validation or runtime behavior.

## Recommended crate/module shape

The code should be structured so that the core engine is independent from file formats.

A possible Rust-level structure:

```text
kuromaku-core
  graph_flow.rs
  validator.rs
  runtime.rs
  manifest.rs

kuromaku-formats
  yaml.rs
  markdown.rs

kuromaku-cli
  loads input file
  selects parser by extension or explicit flag
  receives GraphFlow
  calls validator
  calls runtime
  writes manifest
```

The exact module names may differ, but the dependency direction should remain clear:

```text
formats -> core
cli -> formats
cli -> core
core -> no dependency on formats or cli
```

The core engine must not depend on YAML, Markdown, or CLI-specific code.

## Expected behavior

A workflow defined in YAML and the equivalent workflow defined in Markdown must produce the same `GraphFlow`.

After parsing, validation and runtime behavior should be identical.

Tests should be written around this expectation:

- YAML parser emits the expected `GraphFlow`
- Markdown parser emits the expected `GraphFlow`
- equivalent YAML and Markdown inputs emit equivalent `GraphFlow`
- validator tests use `GraphFlow` directly, not YAML or Markdown fixtures
- runtime tests use validated `GraphFlow` directly

## Design guardrails

Do not leak Markdown-specific assumptions into the runtime.

Do not leak YAML-specific structure into the validator.

Do not make the manifest depend on the original source format.

Do not duplicate workflow validation rules in every parser.

Do not introduce SDKs now.

Do not design the Markdown parser as the canonical model.

Do not design YAML as the canonical model.

The canonical model is `GraphFlow`.

## Summary

The goal is to make kuromaku extensible without overbuilding now.

YAML and Markdown are current input formats.

Future formats, UIs, APIs, or SDKs should be able to produce `GraphFlow` without requiring engine changes.

This keeps the runtime stable, the validator format-agnostic, and the authoring experience replaceable.
