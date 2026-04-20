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
