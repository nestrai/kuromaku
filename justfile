default:
    @just --list

build:
    cargo build

release:
    cargo build --release

run *args:
    cargo run -- {{args}}

test:
    cargo test

lint:
    cargo clippy -- -D warnings
    cargo fmt -- --check

fmt:
    cargo fmt

check:
    cargo check

# Smoke-test the shipped graph flow (issue #241):
# 1. Validate the example file via `kuro validate` (AC1).
# 2. Run the graph_smoke integration test, which drives the same file
#    against an Ollama shim and asserts a terminal state is reached (AC2).
smoke-graph:
    cargo run --quiet -- validate seeds/rust/flows/implement-issue-graph.yaml
    cargo test --test graph_smoke
