# Implementation audit - agent execution quality

This document records outstanding findings from an audit of the implementation
against [product.md](product.md), [architecture.md](architecture.md), and
[roadmap.md](roadmap.md). It is a findings document, not a specification. Once a
finding is resolved, remove it here and update the specification document it
refers to.

## Summary

The remaining problem is execution quality. There is no completed milestone gate
for task completion, tool latency, or recovery rate. Tool calls are still emitted
and executed one at a time, and provider streaming is blocking. Workspace
exploration is bounded: `search_text` supports regex, case-insensitive matching,
and context lines; `list_files` supports depth, glob filtering, and entry
limits; `glob` bounds file discovery; and oversized tool output keeps both ends
with an explicit omitted-byte report.

| ID | Severity | Area | Remaining finding |
| --- | --- | --- | --- |
| B2 | High | Agent, tools | Tool calls execute strictly sequentially |
| B3 | Medium | Providers | Blocking provider IO limits scalability |
| B5 | Medium | Project | No measurable task-completion quality gate |

## Findings

### B2 - Tool calls execute strictly sequentially

For the chat-completions path, the stream decoder accumulates tool calls and
emits them only once the response body is fully read. It emits one call, waits
for the callback to finish, then emits the next.

The executor itself has a synchronous API,
`ToolExecutor::execute(&self, call: &ToolCall) -> ToolResult`. An `execute_many`
helper exists but is unused by the agent, and its lazy iterator spawns one worker
and immediately joins it before spawning the next; it preserves request order
but does not overlap execution.

A model that requests five independent file reads in one turn gets five
serialized operations. The round-trip count is correct — one model request per
turn, not per tool — but wall-clock latency scales with the sum of tool times.

Concurrency also needs a safety policy. Read-only operations can usually
overlap; patches, commands, approvals, and operations whose inputs depend on a
previous result must remain serialized. Preserving result order alone is not a
sufficient correctness rule.

### B3 - Blocking provider IO limits scalability

Every provider uses `ureq`, a blocking HTTP client, and the SSE loop is a
blocking line iterator. This ties a run worker to a blocking provider call and
makes cancellation, resource accounting, and many simultaneous sessions less
efficient. It does not, by itself, prevent safe read-only tool concurrency: Loom
already runs each agent run on a worker thread, and tools can use additional
bounded worker threads.

Async provider transport is therefore not a prerequisite for B2. It should
follow measurements showing that blocked provider workers or connection
scalability are material bottlenecks. `tokio` is already a workspace dependency
used by `loom-server`, so an eventual async migration would fit the existing
runtime model.

### B5 - No measurable task-completion quality gate

A task-level acceptance test now drives a representative fixture to completion,
but the project still lacks measured turns per task, time to first streamed
output, search latency, tool-error recovery rate, and successful-completion
rates with thresholds for the behavior users feel.

This is a milestone-ordering problem, not evidence that the infrastructure
should be discarded. Without representative fixtures and latency thresholds,
further optimization risks improving internal metrics while leaving the agent
experience unchanged.

## Recommended order

These are ordered by user-visible benefit and confidence, not by layer:

1. Add an agent-level batching implementation. Issue two deliberately slow
   read-only calls in one model turn, prove they overlap while results retain
   model order, and serialize writes, commands, approvals, and dependent calls.
2. Add representative task fixtures and measure turns per task, time to first
   token, search latency, tool-error recovery rate, and successful completion.
3. Consider an async provider client only if those measurements show blocked
   provider workers or connection scalability are limiting factors.

A full rewrite is not indicated. The remaining work is concentrated in
`loom-agent`, `loom-tools`, and provider execution. The existing protocol,
workspace, VCS, process, persistence, server, and UI foundations can support
these changes without introducing a second orchestration model.
