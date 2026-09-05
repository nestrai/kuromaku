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

# Version-consistency check (#398): Cargo.toml is the single source of
# truth (flake.nix reads it via fromTOML), so this asserts the two
# toolchains actually resolve to the same string -- covering the seam a
# broken fromTOML expression or a stale flake edit would reopen. Run by
# its own CI step; needs nix on PATH.
check-version:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_version="$(cargo pkgid | sed 's/.*[#@]//')"
    nix_version="$(nix eval --raw .#kuro.version)"
    if [ "$cargo_version" != "$nix_version" ]; then
        echo "version mismatch: Cargo.toml=$cargo_version flake.nix=$nix_version" >&2
        exit 1
    fi
    echo "version OK: $cargo_version"

# Release-tag guard (#398), called by release.yml before creating a
# release: the pushed tag must match the crate version. Reads Cargo.toml
# directly (no cargo needed) so the guard job stays toolchain-free.
check-release-tag tag:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
    if [ "{{tag}}" != "v$cargo_version" ]; then
        echo "tag {{tag}} does not match Cargo.toml version v$cargo_version" >&2
        exit 1
    fi
    echo "tag OK: {{tag}}"

# Smoke-test the shipped graph flow (issue #241):
# 1. Validate the example file via `kuro validate` (AC1).
# 2. Run the graph_smoke integration test, which drives the same file
#    against an Ollama shim and asserts a terminal state is reached (AC2).
smoke-graph:
    cargo run --quiet -- validate seeds/rust/flows/implement-issue.yaml
    cargo test --test graph_smoke
