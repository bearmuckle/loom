# Agent instructions

- Include a PR verification section only when there are meaningful checks to report. List each relevant command once with a brief result; omit trivial hygiene checks and do not include verbose test output.
- For UI work, prefer existing `gpui-kit` components when they fit the interaction; build custom controls only when the component library does not provide a suitable option.
- Before opening a PR, check workspace line coverage locally with `cargo llvm-cov --workspace --all-features --locked --fail-under-lines 75`; coverage must be at least 75%.
- Before pushing a PR or PR update, run reasonable checks for the changed code, following `.github/workflows/ci.yml` and any relevant build workflow. For Rust changes, run formatting, Clippy, and tests for the affected scope; include relevant native or wasm builds when those targets are affected. Report checks that cannot be run.
- Use `sccache` globally to speed up builds across worktrees; install and configure it if missing.
- For Rust logging, use the existing `log` facade (`log::debug!`, `log::info!`, `log::warn!`, or `log::error!`) so output follows the configured native and wasm logger. Do not add direct `println!` or `eprintln!` logging.

# Commit attribution

When creating commits in this repository, attribute the commit to the repository
owner's GitHub account so deployment providers can match the commit to its
author:

- Name: `Björn Harrtell`
- Email: `141030+bjornharrtell@users.noreply.github.com`

Before committing, set these values in the repository-local Git config and
verify them with `git var GIT_AUTHOR_IDENT`. Do not use the generic Copilot
identity (`198982749+Copilot@users.noreply.github.com`) as the commit author.
