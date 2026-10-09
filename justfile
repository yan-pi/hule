alias b := build
alias c := check
alias f := fmt
alias t := test
alias p := pre-push

_default:
  @just --list

# Build the project
build:
   cargo build

# Check formatting, compilation, and linting.
check:
   cargo fmt --all --check
   cargo check --all-targets --all-features
   cargo clippy --all-targets --all-features -- -D warnings

# Format Rust code.
fmt:
   cargo fmt --all

# Run all tests.
test:
   cargo test --all-features

# Validate without automatically changing source formatting.
pre-push: check test
