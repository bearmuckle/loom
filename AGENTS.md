# Agent instructions

- Commit directly to `main` and push to `main`. Do not open pull requests and do not leave work on a feature branch.
- Include a Verification section in the commit message body only when there are meaningful checks to report. List each relevant command once with a brief result; omit trivial hygiene checks and do not include verbose test output.
- For UI work, prefer existing `gpui-kit` components when they fit the interaction; build custom controls only when the component library does not provide a suitable option.
- Before committing, run workspace coverage locally if useful and check patch coverage on changed lines; CI does not run coverage. For a targeted run use the relevant package or test selection with `cargo llvm-cov`, then `git fetch origin main` and `diff-cover lcov.info --compare-branch=origin/main --fail-under=75 --exclude 'crates/*/src/tests.rs' --exclude 'crates/*/src/tests/*' --exclude 'crates/*/src/*/tests.rs' --exclude 'crates/*/src/*/tests/*' --exclude 'crates/*/tests/*'`. New or changed non-test lines should meet the 75% patch threshold when a report is generated.
- Before committing or pushing, run reasonable checks for the changed code, following `.github/workflows/ci.yml` and any relevant build workflow. For Rust changes, run formatting, Clippy, and tests for the affected scope; include relevant native or wasm builds when those targets are affected. Report checks that cannot be run.
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
