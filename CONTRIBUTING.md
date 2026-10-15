# Contributing

Thanks for taking an interest in Loom. The project is under active development
and pre-1.0, so APIs, features, and architecture may change as the design
develops.

## Before opening a pull request

- Search existing issues and pull requests for related work.
- For a substantial change, open an issue first to discuss the proposed scope.
- Keep changes focused and describe user-visible behavior, security impact,
  and any compatibility changes in the pull request.
- Do not include real credentials, private repository data, or other secrets
  in source, fixtures, logs, or screenshots.

## Development

Install Rust 1.95 or newer and the native system dependencies listed in the
[CI workflow](.github/workflows/ci.yml). Use the locked workspace checks for local
verification:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --locked
```

Full workspace coverage is an optional local diagnostic:

```sh
cargo llvm-cov --workspace --locked --lcov --output-path lcov.info
```

Visual render tests are also optional local checks. They require the native UI
libraries, fonts, and software rendering dependencies documented in the CI
workflow:

```sh
cargo test -p loom-ui --features visual-tests --locked --bins visual_render_tests
```

CI does not run full coverage or visual render tests. Coverage should be
focused on changed code when needed; there is no workspace-wide coverage gate.
`./scripts/ci-check.sh` runs routine formatting, Clippy, and build checks. CI
runs formatting, Clippy, and workspace tests for pull requests and pushes to
`main`; tests provide the workspace compilation check, so CI does not run a
separate `cargo build --workspace` step.

### Faster builds with sccache

The dependency tree (GPUI, wgpu) is large, so builds across worktrees benefit
from a shared compiler cache:

1. Install it: `cargo install --locked sccache`, or use a package manager
   (`apt-get install sccache`, `brew install sccache`).
2. Point Cargo at it in `~/.cargo/config.toml`:

   ```toml
   [build]
   rustc-wrapper = "sccache"
   ```

   or export `RUSTC_WRAPPER=sccache` for a single shell session.
3. Optionally cap the local cache, for example with
   `SCCACHE_CACHE_SIZE="20G"`.
4. Verify it is being used with `sccache --show-stats`.

CI does not use sccache; the workflow relies on `Swatinem/rust-cache` instead.

## Security reports

Do not use a public issue for a suspected vulnerability. Follow the
instructions in [SECURITY.md](SECURITY.md).
