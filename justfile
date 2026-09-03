default:
    @just --list

build:
    cargo build

release:
    cargo build --release

# Build a release tarball for one target platform (called by release.yml,
# runs on a native runner per platform -- no cross-compilation).
dist target: release
    mkdir -p dist
    tar -czf dist/kuro-{{target}}.tar.gz -C target/release kuro

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
    cargo run --quiet -- validate seeds/rust/flows/implement-issue.yaml
    cargo test --test graph_smoke
