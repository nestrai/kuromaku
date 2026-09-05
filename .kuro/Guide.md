# kuromaku -- Agent Guide

You are working on **kuromaku** (CLI: `kuro`), a Rust CLI tool for reproducible AI agent teams with persistent shared state. See `AGENTS.md` in the repo root for workflow conventions and git rules.

## Architecture

Single crate. Key modules:

```
src/
  main.rs        -- entry, CLI (clap), flow resolution, run orchestration
  config.rs      -- YAML parsing for flows and agents (.kuro/flows/, .kuro/agents/)
  config_md.rs   -- markdown-format flow parser (graph flows in .md files)
  core/          -- graph flow types, validator, shared error types
  dag.rs         -- DAG validation and topological sort of linear flow steps
  runner/        -- step execution loop, prompt assembly, context injection,
                    graph flow execution (graph.rs, graph_interactive.rs)
  executor/      -- process spawning (local.rs), stream-json parsing, transport trait
  mcp/           -- MCP server, tool definitions, workflow MCP tools
                    (review_pr, rework_pr, implement_issue)
  messaging/     -- inter-agent router, broadcast, termination logic, audit log
  notify/        -- GitHub notification helpers (issue comments, labels)
  koto_config.rs -- project config loader (.kuro/config.yaml, cascade, tiers)
  resolver.rs    -- role-to-agent resolution across cascade tiers
  context.rs     -- `kuro context` command: seed inventory, effective cascade
  init.rs        -- `kuro init` scaffold generator
  stack.rs       -- persistent output storage (~/.kuro/stacks/<project>/)
  skills.rs      -- skill fetching and injection from .kuro/skills/
  llm.rs         -- LLM backend abstraction (claude-cli, api, ollama)
  chat.rs        -- interactive chat mode
  ui.rs          -- terminal output, spinner, formatting (crossterm)
```

## Key Patterns

- Flows define steps, steps reference agents by name
- Agents live in `.kuro/agents/<Name>.yaml` with role, model, backend, rules
- Steps can declare `input: [other-step]` to receive prior output as context
- Guide.md provides project context injected into every agent prompt
- Rules are markdown files in `.kuro/rules/` referenced by agent config
- Stack outputs go to `~/.kuro/stacks/<project>/` (never in the repo)

## Design Principles

- Reproducible: same flow + same input = same agent behavior
- Declarative: YAML config, no imperative scripting
- Transparent: show what agents do, how long, what they cost
- Local-first: everything runs on the user's machine
- Backend-agnostic: claude-cli, api, ollama as interchangeable backends

## Decisions

ADRs live in `docs/decisions/`. They are for internal use by the maintainer.
