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
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo llvm-cov --workspace --all-features --locked --lcov --output-path lcov.info
cargo build --workspace --locked
```

Coverage is enforced per pull request as **patch coverage**: at least 75% of the
non-test lines changed by the PR must be covered (via `diff-cover` against the
base branch). There is no flat workspace threshold.

The same commands are collected in `./scripts/ci-check.sh`. CI runs these
checks for pull requests and builds the workspace on pushes to `main`.

## Security reports

Do not use a public issue for a suspected vulnerability. Follow the
instructions in [SECURITY.md](SECURITY.md).
