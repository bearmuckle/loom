#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo fmt --all -- --check
# `--all-features` is deliberately omitted, matching CI: it would turn on the
# `loom-ui` `visual-tests` feature, whose render tests capture real frames
# through GPUI's headless renderer and need a GPU or a software Vulkan/GL
# driver. CI exercises that feature in its own
# `cargo test -p loom-ui --features visual-tests` step instead.
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo llvm-cov --workspace --locked --lcov --output-path lcov.info
cargo build --workspace --locked

# Patch coverage: changed non-test lines must stay covered. Only runs when
# diff-cover is available and the base branch is present locally.
if command -v diff-cover >/dev/null 2>&1 && git rev-parse --verify --quiet origin/main >/dev/null; then
  diff-cover lcov.info \
    --compare-branch=origin/main \
    --fail-under=75 \
    --exclude 'crates/*/src/tests.rs' \
    --exclude 'crates/*/src/tests/*' \
    --exclude 'crates/*/src/*/tests.rs' \
    --exclude 'crates/*/src/*/tests/*' \
    --exclude 'crates/*/tests/*'
else
  echo "skipping patch coverage (diff-cover or origin/main unavailable)"
fi
