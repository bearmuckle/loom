# Implementation audit - agent execution quality

This document records a point-in-time audit of the implementation against the
specification in [product.md](product.md), [architecture.md](architecture.md),
and [roadmap.md](roadmap.md). It is a findings document, not a specification.
Once a finding is resolved, remove it here and update the specification
document it refers to.

A previous audit (findings A1-A13) covered structural and protocol concerns and
was removed once addressed. This audit uses a `B` prefix and covers a different
question: why the agent loop feels slow and unreliable in use compared to
comparable tools.

**Audited revision:** `01165c3` (shared LoomView browser client).

**Scope:** 24,744 lines across 15 crates.

## Summary

The infrastructure is sound. Crate boundaries are sensible, the dependency
direction runs leaf-ward, the protocol and persistence layers are real
implementations, and the reconnect/resume machinery works. None of the findings
below are about those layers.

The problem is that agent *execution quality* was never the subject of a
milestone. M0-M4 built durable orchestration, transport, and provider
plumbing; M5 built the client. The agent loop and tool surface were written
once during M1 and have not been revisited since, and they are where the
user-visible slowness and unreliability come from.

| ID | Severity | Area | Finding |
| --- | --- | --- | --- |
| B1 | Critical | Agent | Any failed tool result terminates the entire run |
| B2 | High | Agent, tools | Tool calls execute strictly sequentially |
| B3 | High | Providers, tools | Blocking IO throughout precludes concurrency |
| B4 | High | Tools | The tool surface is minimal and naively implemented |
| B5 | Medium | Project | Effort is concentrated away from agent quality |

## Findings

### B1 - Any failed tool result terminates the entire run

In `handle_stream_event`, a tool call is executed and its result inspected
(`crates/loom-agent/src/lib.rs:1401-1410`):

```rust
let (tool_events, result) = self.execute_tool(&call);
ctx.events.extend(tool_events);
if result.success {
    self.last_failed_call = None;
} else {
    self.last_failed_call = Some(call);
    ctx.events.extend(self.finish_failed(result.output));
    ctx.finished = true;
    return Ok(StreamFlow::Stop);
}
```

A tool returning `success: false` fails the run. The same pattern is repeated
on the approval path (`crates/loom-agent/src/lib.rs:663-668`, in
`approve_entry_inner`) and on the retry path (`:924-931`, in
`retry_entry_inner`) — so retrying a failed tool call cannot recover the run
either, which removes the one obvious escape hatch a user would reach for.
`retry_entry_inner` replays `last_failed_call` with identical arguments and no
intervening model turn, so for a deterministic failure — a path that does not
exist, a patch whose context does not match, a test that genuinely fails — the
retry is guaranteed to fail again.

`ToolResult::failure` is returned for entirely ordinary conditions: a
`read_file` on a path that does not exist, an `apply_patch` whose context does
not match, a `run_command` that exits non-zero, a malformed argument object.
These are the normal texture of agent work.

The `run_command` case is worth stating explicitly, because it is the most
damaging. `run_command` maps process exit status straight onto tool success
(`crates/loom-tools/src/lib.rs:268-272`):

```rust
if output.status.success() {
    ToolResult::success(call, text)
} else {
    ToolResult::failure(call, text)
}
```

Combined with B1, a failing test suite, a compile error, a non-zero linter, or
a `grep` that finds no matches terminates the agent run. Running a test in
order to read its failure output and fix it — the single most common thing an
agent does on purpose — cannot complete by construction.

Three adjacent paths terminate the run for equally recoverable reasons:

- An unrecognised tool name (`:1205-1234`) fails the run rather than returning
  "unknown tool" to the model.
- A policy `Deny` decision (`:1252-1272`) fails the run rather than telling the
  model the action is not permitted so it can choose another route.
- A model stream that yields no tool call and no completion (`:1134-1137`)
  fails the run with "model returned an empty stream".

Comparable agents treat all of these as observations to feed back into the
conversation. The model reads the error and adapts, which is most of what makes
an agent appear competent. Loom instead surfaces them as run failures, so the
user must restart and re-establish context by hand. This is the dominant
source of perceived unreliability and it is a small, contained fix.

### B2 - Tool calls execute strictly sequentially

For the chat-completions path, the stream decoder accumulates tool calls and
emits them only once the response body is fully read, in `finish()`
(`crates/loom-providers/src/lib.rs:2281-2300`). It loops over them, emitting one
at a time, and each `emit` synchronously runs the tool to completion before the
next is emitted.

Multiple tool calls in a single assistant message are therefore executed one
after another, never concurrently. The executor signature makes this
structural rather than incidental — `ToolExecutor::execute(&self, call:
&ToolCall) -> ToolResult` (`crates/loom-tools/src/lib.rs:117`) is synchronous
and returns a finished result, so there is no representation of an in-flight
tool.

A model that requests five file reads in one turn, which is the normal way
these models explore a repository, gets five serialised disk walks. The
round-trip count is correct — one model request per turn, not per tool — so the
cost is wall-clock latency within the turn rather than extra completions.

### B3 - Blocking IO throughout precludes concurrency

Every provider uses `ureq`, a blocking HTTP client, and the SSE loop is a
blocking line iterator (`crates/loom-providers/src/lib.rs:2075`):

```rust
for line in reader.lines() {
```

Combined with the synchronous `ToolExecutor` in B2, there is no point in the
stack where concurrency could be introduced without changing signatures. B2
cannot be fixed without addressing this first.

`tokio` is already a workspace dependency used by `loom-server`, so moving the
provider layer to an async client does not add a new runtime to the project.

### B4 - The tool surface is minimal and naively implemented

`loom-tools` is 625 lines and exposes seven tools, two of which
(`propose_plan`, `ask_user`) are control tools handled by the runtime rather
than workspace capabilities (`crates/loom-tools/src/lib.rs:45-55`). The five
real tools are `list_files`, `read_file`, `search_text`, `apply_patch`, and
`run_command`.

The implementations are literal:

- `search_text` (`:347-395`) recursively walks the tree, `read_to_string`s
  every file, and tests each line with `line.contains(query)`. There is no
  regex, no file-type or glob filter, no line-range output, and no parallelism.
- `collect_files` (`:315-345`) walks the entire tree with no depth limit,
  pagination, or glob filter.
- Ignored directories are a hardcoded five-entry denylist —
  `.git`, `target`, `node_modules`, `.venv`, `vendor` (`:13-19`). `.gitignore`
  is not consulted, so generated output such as `crates/loom-ui/dist/` is
  walked and searched.
- `read_file` reads whole files with no line-range parameter.
- Output is truncated by byte count at 64 KiB (`:90`, `:396-400`), which can
  cut a file mid-token with no indication of what was elided.

The practical effect is that exploration is slow on any real repository and
low-signal, so the model needs more turns to orient itself. Every extra turn
costs a full completion. B4 and B2 compound: slow tools, run serially.

Comparable tools delegate search to `ripgrep` or an index, expose glob
matching, and return ranged reads with structured truncation markers.

### B5 - Effort is concentrated away from agent quality

By line count, `loom-server` (5,337), `loom-providers` (3,219), and `loom-ui`
(5,785) account for 14,341 of 24,744 lines — transport, reconnect, resume
cursors, request deduplication, capability negotiation, device-flow
authentication, and rendering.

The surface that determines whether the agent is actually good — `loom-tools`
(625) plus `loom-context` (334) — is 959 lines, under 4% of the workspace.

This is a milestone-ordering consequence rather than a coding mistake. M2-M4
specify durable orchestration, provider health, and remote control; none of
M0-M6 has an exit condition phrased in terms of task success rate, turns per
task, or latency. The roadmap's own guidance is "avoid building an entire layer
in isolation before proving the end-to-end path", and the quality bar already
lists "time to first streamed model output" and "search latency on
representative repositories" as things to measure. Neither is currently
measured.

## Recommended order

These are ordered by user-visible benefit per unit of work, not by layer.

1. **B1.** Return failed tool results to the model as tool messages and
   continue the run. Reserve `finish_failed` for genuine aborts — provider
   errors, limits, cancellation. Contained to `loom-agent`, and the single
   largest reliability improvement available.
2. **B4.** Rewrite `loom-tools`: `ripgrep`-backed search with regex and glob
   filters, `.gitignore` awareness, ranged reads, structured truncation, and an
   exact-string edit tool. Contained to one crate.
3. **B3.** Move providers to an async client on the existing `tokio` runtime
   and make `ToolExecutor::execute` async.
4. **B2.** Execute the tool calls of a turn concurrently, preserving result
   order. Depends on B3.
5. Add a task-completion benchmark before further milestone work — turns per
   task, wall-clock to first token, tool-error recovery rate — so the quality
   bar's performance section has something behind it.

A full rewrite is not indicated. Findings B1-B4 are confined to `loom-agent`,
`loom-tools`, and the provider transport. `loom-core`, `loom-protocol`,
`loom-workspace`, `loom-vcs`, `loom-process`, and `loom-persistence` are not
implicated by any finding here, and a restart would most likely reproduce them
before reaching the same defect.
