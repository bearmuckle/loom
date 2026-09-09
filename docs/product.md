# Product scope and use cases

## Product direction

The primary use case is an interactive coding-agent session. A user opens a
repository, gives an agent a goal such as "fix the failing tests" or "add
support for this API", and follows the agent as it:

1. Inspects the repository, existing instructions, history, and relevant
   files.
2. Forms and presents a plan before making consequential changes.
3. Reads and edits files through explicit tools.
4. Runs searches, terminals, builds, tests, linters, and other configured
   tools.
5. Streams user-visible messages, tool calls, output, progress, and errors.
6. Revises its approach when checks fail or the user provides feedback.
7. Presents a reviewable diff and a concise final result.

The user must be able to interrupt, approve, deny, redirect, retry, or take
over at any point. The agent is not a one-shot prompt wrapper: a session is a
durable orchestration object containing conversation history, plans, tool
invocations, approvals, artifacts, child tasks, model usage, and the
associated workspace.

Loom is therefore an agent orchestration application with a code workspace,
not a code editor with an optional chat panel. The primary objects in the
product are agent sessions, runs, plans, tool calls, approvals, workspaces,
and provider/model configurations. The editor, terminal, source control, and
diagnostics are the agent's observable working environment as well as tools
the user can operate directly.

## Target workflows

Loom should support these concrete workflows:

- **Interactive coding:** ask an agent to understand, modify, and validate a
  repository while observing every action.
- **Issue-driven work:** start a session from an issue or task, keep the issue
  context available, and produce a reviewable change.
- **Parallel work:** run multiple agents against isolated worktrees or
  branches, compare their results, and selectively merge or discard them.
- **Long-running work:** leave agents running tests, builds, indexing, or
  background investigation while the frontend disconnects and reconnects.
- **Provider choice:** use hosted models, organization-provided endpoints,
  OpenAI-compatible APIs, or local models without changing the session UI or
  tool semantics.
- **Human-in-the-loop automation:** configure approval policies for reads,
  writes, commands, network access, credentials, and destructive operations.
- **Remote control:** connect to a backend on a workstation, server, or
  development container from a native or browser client.

## Initial end-to-end workflow

The first release should optimize for this exact loop:

1. Create or open a repository-backed project.
2. Start a named agent session with a selected provider and model.
3. Enter a task in natural language and optionally attach an issue, files,
   prior sessions, or repository instructions.
4. Let the agent inspect the repository and present a plan.
5. Approve the plan and individual high-risk actions according to policy.
6. Watch streamed model messages, tool calls, terminal output, file changes,
   diagnostics, and test results.
7. Interrupt or redirect the agent, then resume from the preserved context.
8. Review the resulting diff, test evidence, and agent summary.
9. Continue the session, create a commit, or hand the work to another agent.
10. Disconnect and reconnect from a native or browser client without losing
    the backend session.

## Feature areas

| Area | Target capability |
| --- | --- |
| Projects | Open local directories, clone repositories, configure workspace roots, and remember recent projects |
| Agent sessions | Create, rename, pause, resume, interrupt, retry, archive, fork, and compare persistent sessions |
| Agent orchestration | Plans, steps, dependencies, child agents, parallel tasks, retries, cancellation, budgets, and durable event history |
| Model providers | Multiple hosted providers, OpenAI-compatible endpoints, local model servers, model discovery, per-session selection, fallback, and usage reporting |
| Agent context | Repository instructions, conversation history, file references, tool results, summaries, compaction, token budgets, and context inspection |
| Permissions | Approval policies for file reads/writes, commands, network access, secrets, plugins, and destructive operations |
| Tool execution | Search, file operations, patching, terminals, diagnostics, source control, language services, HTTP, and extensible tool adapters |
| Editor | Multiple files, tabs, splits, undo/redo, autosave policy, encoding/newline preservation, large-file safeguards, and agent change markers |
| Navigation | Fuzzy file open, project search, symbol outline, go-to-definition/references through language services |
| Terminal | Multiple persistent sessions, streaming output, resize, input, exit status, cancellation, and environment selection |
| Tasks | Agent-owned and user-owned build/test/lint commands with run, monitor, cancel, restart, and artifact inspection |
| Source control | Status, diff, staging, commit creation, branch awareness, and conflict visibility |
| Diagnostics | Structured errors, warnings, locations, severity, and links back to source |
| Review | Proposed edits, diff views, checkpoints, test evidence, commit preparation, and agent session transcripts |
| Collaboration | Remote backend access, session sharing, reconnect, handoff, and a clear access/permission model |
| Extensibility | Versioned capabilities and adapters for model providers, tools, language servers, source-control providers, and task runners |
| Accessibility | Keyboard-first operation, focus visibility, scalable UI, reduced motion, and screen-reader-compatible semantics where supported |

Features should be added behind stable domain interfaces. A client must be
able to discover which capabilities a backend supports instead of assuming
that every installation has the same tools available.

## Guiding principles

1. **Backend owns truth.** The frontend renders state and submits commands; it
   does not become the source of truth for an agent, workspace, or task.
2. **Local should feel local.** A local backend uses the cheapest available
   transport and must not pay remote-mode costs for ordinary interactions.
3. **Remote should be a first-class mode.** Reconnection, resumable events,
   capability negotiation, and authentication are part of the protocol rather
   than frontend-specific workarounds.
4. **Human control.** Automation must be interruptible, permissioned, and
   reviewable. The user can always see what the agent is doing and why an
   action is waiting.
5. **Portable core, thin platform shells.** Workspace logic belongs in Rust
   crates with platform-specific code isolated behind interfaces.
6. **Observable actions.** Every mutation, task, and long-running process has a
   status, an owner, and a user-visible result or error.
