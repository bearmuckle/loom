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

**Audited revision:** `c384f64` (current PR head, after PR #5 and the follow-up
tool-recovery and workspace-tool changes).

**Scope:** 26,554 lines across 15 crates.

## Summary

PR #5 improved the activity timeline, approval controls, continuation/retry
handling, transcript repair, and session UX. The follow-up implementation also
changed the most visible reliability failure: ordinary tool errors now become
tool messages for the next model turn instead of failing the run.

The remaining problem is execution quality. There is still no explicit
milestone gate for task completion, tool latency, or recovery rate. Tool calls
are still emitted and executed one at a time; provider streaming is blocking;
and the current tool surface still lacks structured limits and diagnostics.

## Resolved since the previous audit

### B1 - Tool failures no longer terminate the run

This finding is resolved at the audited revision and is not included in the
remaining-findings table below.

`execute_tool` appends every result, including failures, as a model tool
message (`crates/loom-agent/src/lib.rs:1553-1585`). The event handler now stops
the current model stream without calling `finish_failed` when a tool fails
(`:1471-1477`), allowing the runtime to start the next model turn with the
error in context. Unknown tools and policy denials follow the same pattern by
writing tool messages before stopping the current stream
(`:1229-1257`, `:1274-1301`).

This is the right recovery shape for a failing test, a bad patch context, a
missing path, or an invalid tool argument. Provider failures, cancellation,
limits, and an empty model stream remain genuine run-level failures.

| ID | Severity | Area | Remaining finding |
| --- | --- | --- | --- |
| B2 | High | Agent, tools | Tool calls execute strictly sequentially |
| B3 | Medium | Providers | Blocking provider IO limits scalability |
| B4 | Medium | Tools | Workspace tools retain bounded exact-search limitations |
| B5 | Medium | Project | No measurable task-completion quality gate |

## Findings

### B2 - Tool calls execute strictly sequentially

For the chat-completions path, the stream decoder accumulates tool calls and
emits them only once the response body is fully read, in `finish()`
(`crates/loom-providers/src/lib.rs:2295-2315`). It emits one call, waits for
the callback to finish, then emits the next.

The executor itself has a synchronous API:
`ToolExecutor::execute(&self, call: &ToolCall) -> ToolResult`
(`crates/loom-tools/src/lib.rs:117`). An `execute_many` helper was added at
`crates/loom-tools/src/lib.rs:136-144`, but it is unused by the agent and its
lazy iterator spawns one worker and immediately joins it before spawning the
next. It therefore preserves request order but does not overlap execution.

A model that requests five independent file reads in one turn gets five
serialized operations. The round-trip count is correct — one model request per
turn, not per tool — but wall-clock latency scales with the sum of tool times.

Concurrency also needs a safety policy. Read-only operations can usually
overlap; patches, commands, approvals, and operations whose inputs depend on a
previous result must remain serialized. Preserving result order alone is not a
sufficient correctness rule.

### B3 - Blocking provider IO limits scalability

Every provider uses `ureq`, a blocking HTTP client, and the SSE loop is a
blocking line iterator (`crates/loom-providers/src/lib.rs:2087`):

```rust
for line in reader.lines() {
```

This ties a run worker to a blocking provider call and makes cancellation,
resource accounting, and many simultaneous sessions less efficient. It does
not, by itself, prevent safe read-only tool concurrency: Loom already runs
each agent run on a worker thread, and tools can use additional bounded worker
threads.

Async provider transport is therefore not a prerequisite for B2. It should
follow measurements showing that blocked provider workers or connection
scalability are material bottlenecks. `tokio` is already a workspace dependency
used by `loom-server`, so an eventual async migration would fit the existing
runtime model.

### B4 - Workspace tools retain bounded exact-search limitations

`loom-tools` exposes eight tools, two of which
(`propose_plan`, `ask_user`) are control tools handled by the runtime rather
than workspace capabilities. The six workspace tools are `list_files`,
`read_file`, `search_text`, `web_search`, `apply_patch`, and `run_command`.

The follow-up implementation improved the surface:

- `read_file` supports line ranges (`crates/loom-tools/src/lib.rs:165-189`).
- `search_text` supports a glob filter (`:197-220`, `:418-420`).
- File walks use the ripgrep `ignore` and `globset` crates for repository
  ignore rules and glob matching (`:359-451`).
- Output truncation preserves UTF-8 boundaries and has regression coverage
  (`:463-468`, `:757-764`).
- `web_search` uses a provider interface with a bounded HTML adapter, bounded
  result counts and fields, optional hostname filtering, and structured
  citation output. It is classified as a network action so the default policy
  requires approval.

The remaining limitations are:

- `search_text` performs exact `line.contains(query)` matching
  (`:421-437`); it has no regex mode, index, or structured match records.
- `list_files` still walks the entire selected workspace without depth,
  pagination, or a structured result limit.
- `read_file` and other output paths retain a fixed 64 KiB output limit
  (`:90`, `:463-468`), with only a textual truncation marker.
- `web_search` defaults to a public server-rendered search endpoint and accepts
  an operator-configured `LOOM_WEB_SEARCH_ENDPOINT`; it does not provide
  arbitrary URL fetching or page content retrieval. HTML markup and bot
  protection can change independently of Loom, so parser fixtures and explicit
  provider errors are preferred over silent fallback.

The practical effect is bounded but still lower-signal repository exploration
than a full ripgrep-style search API.

### B5 - No measurable task-completion quality gate

The project has a substantial and useful transport, persistence, provider, and
UI implementation, but no implemented acceptance benchmark for the behavior
users feel. The roadmap names time to first streamed output, search latency,
terminal throughput, reconnect time, and resource use; there are no task-level
measurements for turns per task, tool-error recovery rate, or successful
completion.

This is a milestone-ordering problem, not evidence that the infrastructure
should be discarded. Without a representative task fixture and latency
thresholds, further optimization risks improving internal metrics while
leaving the agent experience unchanged.

## Recommended order

These are ordered by user-visible benefit and confidence, not by layer:

1. Add an agent-level batching acceptance test and implementation. Issue two
   deliberately slow read-only calls in one model turn, prove they overlap
   while results retain model order, and serialize writes, commands, approvals,
   and dependent calls.
2. Add regex or indexed search, depth/pagination controls, and structured
   result limits while retaining ranged reads.
3. Add representative task fixtures and measure turns per task, time to first
   token, search latency, tool-error recovery rate, and successful completion.
4. Consider an async provider client only if those measurements show blocked
   provider workers or connection scalability are limiting factors.

A full rewrite is not indicated. The remaining work is concentrated in
`loom-agent`, `loom-tools`, and provider execution. The existing protocol,
workspace, VCS, process, persistence, server, and UI foundations can support
these changes without introducing a second orchestration model.
