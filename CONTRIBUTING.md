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
[CI workflow](.github/workflows/ci.yml). Use the locked workspace checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo llvm-cov --workspace --locked --lcov --output-path lcov.info
cargo build --workspace --locked
```

`--all-features` is deliberately not used: it would turn on the `loom-ui`
`visual-tests` feature, whose render tests capture real frames through GPUI's
headless renderer and therefore need a GPU or a software Vulkan/GL driver. CI
runs those tests in a dedicated `cargo test -p loom-ui --features visual-tests`
step, so Clippy, coverage, and the build stay runnable without a driver.

Coverage is enforced per pull request as **patch coverage**: at least 75% of the
non-test lines changed by the PR must be covered (via `diff-cover` against the
base branch). There is no flat workspace threshold.

The same commands are collected in `./scripts/ci-check.sh`. CI runs these
checks for pull requests and pushes to `main`. It additionally checks that:

- the workspace compiles on the declared minimum Rust version (`1.95`),
- the browser client still compiles for `wasm32-unknown-unknown`, because
  `pages.yml` publishes it from `main` without a pre-merge check,
- the `loom-persistence` and `loom-server` benchmarks still run on pushes to
  `main` (they print measurements; they do not assert thresholds).

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
