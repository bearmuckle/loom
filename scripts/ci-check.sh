#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# A worker node can have the toolchain installed without a `PATH` entry, which
# makes every `cargo` below fail to start. Resolve it once here, so the routine
# checks do not depend on how the node was provisioned.
if ! command -v cargo >/dev/null 2>&1; then
    "$ROOT/scripts/agent-toolchain.sh" >/dev/null
fi

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --locked
