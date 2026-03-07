set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
  @just --list

fmt:
  cargo fmt --all

fmt-check:
  cargo fmt --all -- --check

clippy:
  cargo clippy --workspace --all-targets -- -D warnings

test:
  cargo test --workspace

check:
  bash scripts/check.sh

fix: fmt

install-hooks:
  bash scripts/install-git-hooks.sh

coverage:
  cargo llvm-cov --workspace --lcov --output-path lcov.info
  @echo "Coverage report written to lcov.info"

coverage-html:
  cargo llvm-cov --workspace --html
  @echo "HTML report written to target/llvm-cov/html/index.html"

release *args:
  cargo release {{args}} --workspace --execute
