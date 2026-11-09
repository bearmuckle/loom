# STATUS — read-only `run_command` classification

Branch: `feat/run-command-read-classification` (local only, not pushed).
Base: `f86b2f7` (this worktree was created from it; `origin/main` is 3 commits ahead).

## Goal

A `run_command` call that provably only reads should be classified as
`ActionKind::Read` under the default policy, instead of `ActionKind::Command`.
The widening must stay behind an explicit operator flag or setting: the shipped
default keeps every command a command.

## What is in place

1. `crates/loom-core/src/policy.rs`
   - `ReadOnlyCommandPolicy` (disabled by default) with `enabled()`,
     `disabled()`, `is_enabled()`, `from_setting(source, value)` and
     `classify(program, args)`.
   - `READ_ONLY_COMMANDS_ENV` (`LOOM_READ_ONLY_COMMANDS`) is the operator
     fallback; `from_setting` names the input that carried a refused value.
   - Conservative allowlist: `git` read subcommands (`log`, `status`, `diff`,
     `show`, `grep`, `blame`, `shortlog`, `ls-files`, `rev-parse`,
     `whatchanged`, `describe`), `ls`, `cat`, `head`, `tail`, `wc`, `rg`,
     `grep`, `file`, `stat`, `pwd`, `tree`, `du`, each with an explicit option
     list.
   - Refused: unlisted programs, unlisted options (`--output`, `--ext-diff`,
     `--textconv`, `--paginate`, `-c`, `-O`, `--pre`, `--pre-glob`,
     `--hostname-bin`, `tail -f`, `tree -o`, `file -C`), any shell structure
     (pipe, ampersand, semicolon, redirection, backtick, dollar, parentheses,
     braces, globs, backslash, quotes, hash, bang, newlines), and paths that
     leave the session tree (absolute, `..`, drive or UNC paths).
2. `crates/loom-tools/src/lib.rs`
   - `ToolExecutor::action_kind` classifies `run_command` from its call; a
     malformed call is never a read.
   - `ToolExecutor::with_read_only_commands` is the only way to turn it on.
   - `ToolExecutor::joins_read_batch` keeps a classified read command out of the
     concurrent read batch (a command can touch shared state, and its output
     belongs in model order); `loom-agent`'s `flush_tool_calls` uses it.
3. `crates/loom-server`
   - `InProcessBackend` owns the policy (`read_only_commands()`,
     `set_read_only_commands()`), starts disabled, and is never persisted.
   - `with_read_only_command_policy` is applied at all three places that build a
     run's `ToolExecutor` (fresh run, queued run, restored run).
   - `--read-only-commands <bool>` with the `LOOM_READ_ONLY_COMMANDS` fallback;
     a bad value fails the start, and the help text and startup log name it.
4. `docs/deployment.md` documents the opt-in beside the approval-policy section.

## Commits (local)

- `d11ab02` feat(core,tools): classify a provably read-only command as a read
  behind an opt-in
- `da3f5e5` feat(server): add --read-only-commands as the operator opt-in
- The docs/STATUS commit follows after this file.

## Tests

- `cargo test -p loom-core --lib` — 78 passed.
- `cargo test -p loom-tools --lib` — 106 passed.
- `cargo test -p loom-agent --lib` — 66 passed.
- `cargo test -p loom-server --lib` — 413 passed.
- `cargo test -p loom-server --bins` — 31 passed.
- `cargo check --workspace --all-targets`, `cargo fmt --all`,
  `cargo clippy --all-targets -p loom-core -p loom-tools -p loom-agent
  -p loom-server -- -D warnings` — clean.

Positive and negative security regression tests live in
`policy::tests::{opted_in_policy_classifies_reading_commands_as_reads,
opted_in_policy_keeps_every_writing_or_escaping_command_a_command,
the_shipped_default_policy_keeps_even_a_reading_command_a_command,
the_opt_in_only_moves_allowlisted_commands_to_the_read_decision,
the_setting_accepts_the_documented_values_and_refuses_the_rest}`, in
`loom-tools`' `tests::{the_executor_default_keeps_a_reading_command_a_command,
an_opted_in_executor_moves_only_allowlisted_commands_to_reads,
a_classified_read_command_does_not_join_the_concurrent_read_batch}`, in
`loom-server`'s `tests::read_only_commands`, and in the binary's
`read_only_commands_is_an_explicit_opt_in` and
`help_documents_the_read_only_command_opt_in`.

## Limitations / open points

- Glob metacharacters are refused everywhere, including inside an option value,
  so `rg --glob '*.rs' …` still prompts. This is deliberate: the allowlist does
  not model glob parsing. `rg --type rust …` is the allowlisted spelling.
- The allowlist is deliberately narrow. `sed`, `awk`, `jq`, `find`, `tar`, and
  `git stash list` are not covered, because each has an option that writes or
  runs another program.
- A classified read still executes as the same program with the same arguments;
  only the approval decision changes. A session whose read dimension is `Deny`
  or `RequireApproval` keeps gating it.
- Server-only wiring: the native shell's `--serve` path and the CLI do not set
  the opt-in, so they keep the shipped default.
- Not done here: push, pull request, PR babysitting, `loom-multi-model-review`.

## Overlaps with parallel sessions

- Touches `crates/loom-core/src/policy.rs` and `crates/loom-core/src/lib.rs`
  (re-export), which another session also owns. The changes there are additive:
  a new type, a new constant, one re-export line, and new tests.
- Touches `crates/loom-tools/src/lib.rs` (`action_kind`, `joins_read_batch`) and
  `crates/loom-agent/src/steps.rs` (`flush_tool_calls`).
- Touches `crates/loom-server` (`lib.rs`, `backend/accessors.rs`,
  `backend/constructors.rs`, `backend/filesystem.rs`, `connection/runs.rs`,
  `main.rs`, `src/tests/`) and `docs/deployment.md`.
- Does not touch the CLI, workspace configuration, or the UI.
