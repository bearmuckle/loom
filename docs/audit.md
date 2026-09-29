# Implementation audit - agent execution quality

This document records outstanding findings from an audit of the implementation
against [product.md](product.md), [architecture.md](architecture.md), and
[roadmap.md](roadmap.md). It is a findings document, not a specification. Once a
finding is resolved, remove it here and update the specification document it
refers to.

## Summary

The remaining problem is measurable execution quality. There is no completed
milestone gate for task completion, tool latency, or recovery rate. Consecutive
read-only tool calls now run concurrently and provider streaming uses an async
client; workspace exploration is bounded: `search_text` supports regex, case-insensitive matching,
and context lines; `list_files` supports depth, glob filtering, and entry
limits; `glob` bounds file discovery; and oversized tool output keeps both ends
with an explicit omitted-byte report.

| ID | Severity | Area | Remaining finding |
| --- | --- | --- | --- |
| B3 | Medium | Providers | Blocking provider IO limits scalability |
| B5 | Medium | Project | No measurable task-completion quality gate |

## Findings

### B3 - Blocking provider IO limits scalability

This finding is resolved. Provider streaming, OAuth, device login, and health
checks all use the async `reqwest`/`tokio` client (driven from synchronous
workers with a runtime), and `ureq` has been removed from the workspace.

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

1. Add representative task fixtures and measure turns per task, time to first
   token, search latency, tool-error recovery rate, and successful completion.
2. Revisit remaining per-connection scalability and cancellation behavior once
   those measurements exist.

A full rewrite is not indicated. The remaining work is concentrated in
`loom-agent`, `loom-tools`, and provider execution. The existing protocol,
workspace, VCS, process, persistence, server, and UI foundations can support
these changes without introducing a second orchestration model.
