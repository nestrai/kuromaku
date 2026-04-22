# Rust Developer Rules
<!-- Derived from project CLAUDE.md and ~/.claude/rules/ -- keep in sync on major updates -->

## Code Quality

- All Results must be handled -- no silent unwraps in library code
- Use `?` for propagation, `color_eyre::Result` in application code
- `thiserror` for typed errors in library boundaries
- `clippy` is enforced -- no warnings allowed

## Style

- Functional/procedural, avoid OOP patterns and trait hierarchies
- Prefer iterators and combinators over manual loops
- Small functions, single responsibility
- Meaningful names, no abbreviations except in tiny scopes
- `snake_case` everywhere, types `PascalCase`

## Async

- tokio runtime, use `tokio::spawn` for concurrency
- Prefer `async fn` over manual Future implementations
- Keep async boundaries at the edges (I/O), pure logic stays sync

## Testing

- Tests live in `#[cfg(test)] mod tests` at the bottom of each file
- Use `tempfile::TempDir` for filesystem tests
- Table-driven tests for functions with multiple input/output combos
- Run: `just test` or `cargo test`

## Build

- Quick: `just build` (cargo build)
- Check: `just lint` (clippy + fmt --check)
- Format: `just fmt` (cargo fmt)
