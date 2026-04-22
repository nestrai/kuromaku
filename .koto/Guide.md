# koto -- Agent Guide

You are working on **koto**, a Rust CLI tool for reproducible AI agent teams with persistent shared state. Users define agent teams in YAML, run `koto up`, and get structured outputs.

## Repository

- Language: Rust (edition 2024, stable toolchain)
- Build: `just build` / `just test` / `just lint` / `just fmt`
- Dev shell: `nix develop` (optional) or `rustup` via `rust-toolchain.toml`
- Async runtime: tokio
- CLI: clap (derive API)
- Error handling: color-eyre
- Config: serde + serde_yaml

## Architecture

Single crate. Key modules:

```
src/
  main.rs      -- entry, CLI (clap), flow resolution, run_up orchestration
  config.rs    -- YAML parsing for flows and agents (.koto/flows/, .koto/agents/)
  dag.rs       -- DAG validation and topological sort of steps
  runner.rs    -- step execution loop, prompt assembly, context injection
  executor/    -- process spawning (local.rs), command building per backend
  stack.rs     -- persistent output storage (~/.koto/stacks/<project>/)
  skills.rs    -- skill fetching and injection from .koto/skills/
  ui.rs        -- terminal output, spinner, formatting (crossterm)
```

## Key Patterns

- Flows define steps, steps reference agents by name
- Agents live in `.koto/agents/<Name>.yaml` with role, model, backend, rules
- Steps can declare `input: [other-step]` to receive prior output as context
- Guide.md provides project context injected into every agent prompt
- Rules are markdown files in `.koto/rules/` referenced by agent config
- Stack outputs go to `~/.koto/stacks/<project>/` (never in the repo)

## Design Principles

- Reproducible: same flow + same input = same agent behavior
- Declarative: YAML config, no imperative scripting
- Transparent: show what agents do, how long, what they cost
- Local-first: everything runs on the user's machine
- Backend-agnostic: claude-cli, api, ollama as interchangeable backends

## Code Style

- Functional/procedural, no OOP patterns
- `thiserror` for library errors, `color-eyre` for application errors
- Iterators and combinators over manual loops
- Small, focused functions
- No emojis or icons in code, docs, or output

## Decisions

ADRs live in `docs/decisions/`. They are for internal use by the maintainer.

## Output Style

- No emojis or icons in any output
- Plain, clean markdown

## Git Workflow

Before any work:
1. `git checkout main && git pull` -- always start from up-to-date main
2. Create a feature branch (`feat/...`, `fix/...`)
3. Work, commit, push

If you cannot check out main or create a branch (dirty worktree, conflicts, etc.), STOP and report the problem. Do not work on a stale or wrong branch.

Never commit directly to main. Never close, reopen, or reassign issues -- that is the maintainer's decision.

## Review Guidelines

When reviewing PRs:
- Keep suggestions practical and actionable
- Be constructive -- suggest improvements, don't demand rewrites
- Never suggest creating ADRs or decision records
