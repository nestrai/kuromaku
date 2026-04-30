# CLAUDE.md

## What this is

kuromaku is a CLI tool for reproducible AI agent teams with persistent shared state. Define your team in a YAML file (`.kuro/config.yaml`), run `kuro run`. The binary is `kuro`; the project name is `kuromaku` (kubernetes/kubectl pattern).

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
just run run  # cargo run -- run
```

## Architecture

Single crate for now. Will split into workspace crates when complexity warrants it:
- `kuromaku-cli` -- binary, CLI interface, TUI
- `kuromaku-config` -- YAML config parsing and validation
- `kuromaku-engine` -- DAG resolution, agent orchestration
- `kuromaku-state` -- persistent state management
- `kuromaku-ui` -- shared TUI components (reusable across tools)

## Config format

See `docs/kuromaku-reference-card.html` for the visual spec. Key points:
- Config file: `.kuro/config.yaml`. Legacy `.koto/config.yaml` and `koto.yaml` in repo root still load with a deprecation warning.
- Stages are parallel by default, `needs` creates sequencing
- `flow` as the orchestration keyword (not `workflow` or `pipeline`)

## Code style

- Functional/procedural style, avoid OOP patterns
- Use `thiserror` for library errors, `color-eyre` for application errors
- Prefer iterators and combinators over manual loops
- Keep functions small and focused
