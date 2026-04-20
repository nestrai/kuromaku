# CLAUDE.md

## What this is

koto is a CLI tool for reproducible AI agent teams with persistent shared state. Define your team in a YAML file (`koto.yaml`), run `koto up`.

## Language and tooling

- **Language:** Rust (edition 2024, stable toolchain)
- **Task runner:** just (`just build`, `just test`, `just lint`, `just fmt`)
- **Dev environment:** `nix develop` (optional) or `rustup` via `rust-toolchain.toml`
- **Async runtime:** tokio
- **CLI:** clap (derive API)
- **Error handling:** color-eyre
- **TUI:** ratatui (planned)
- **LLM integration:** genai crate (multi-provider) (planned)
- **Config:** serde + serde_yaml

## Build and test

```
just build    # cargo build
just test     # cargo test
just lint     # cargo clippy + cargo fmt --check
just fmt      # cargo fmt
just run up   # cargo run -- up
```

## Architecture

Single crate for now. Will split into workspace crates when complexity warrants it:
- `koto-cli` -- binary, CLI interface, TUI
- `koto-config` -- YAML config parsing and validation
- `koto-engine` -- DAG resolution, agent orchestration
- `koto-state` -- persistent state management
- `koto-ui` -- shared TUI components (reusable across tools)

## Config format

See `docs/koto-reference-card.html` for the visual spec. Key points:
- Config file: `koto.yaml` in repo root
- Stages are parallel by default, `needs` creates sequencing
- `flow` as the orchestration keyword (not `workflow` or `pipeline`)

## Code style

- Functional/procedural style, avoid OOP patterns
- Use `thiserror` for library errors, `color-eyre` for application errors
- Prefer iterators and combinators over manual loops
- Keep functions small and focused
