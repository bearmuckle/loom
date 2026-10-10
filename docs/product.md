# Product scope and use cases

## Product direction

The primary use case is an interactive coding-agent project. A user opens a
workspace, starts a project, selects one or more repositories for its root
agent, gives the project a goal such as "fix the failing tests" or "add
support for this API", and follows the project manager as it:

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
over at any point. A project is a durable orchestration object rooted in a
project-manager session. It owns the user's goal and may delegate bounded code
or non-code tasks to durable child agent sessions. The manager remains
accountable for results, communicates with its agents, handles blockers, and
reviews and integrates code changes. The detailed behavior and implementation
sequence are in [project sessions and coordinated sub-agents](project-sessions-design.md).

A **workspace** is a durable container for project roots and workspace-level
settings. It is not a directory, repository, clone, filesystem root, or
security boundary, and can exist before any project or repository has been
added. Every agent session belongs to a project; the root session is the
project manager and descendants are delegated agents. Each code-changing
agent owns an isolated filesystem/worktree based on its parent's branch and
revision. Non-code agents need no worktree. Sibling agents never share a
mutable checkout.

Repositories are inputs to agent work rather than identities for workspaces.
The same repository can be used by projects in different workspaces, and a
project can work across multiple repositories. Repository selection records
the source and requested revision; each code agent's checkout, files,
processes, checkpoints, and diffs remain scoped to its isolated root.

Loom is therefore an agent orchestration application with a code workspace,
not a code editor with an optional chat panel. The primary objects in the
product are projects, agent sessions, delegated tasks, runs, agent messages,
plans, tool calls, approvals, workspaces, and provider/model configurations.
The editor, terminal, source control, and
diagnostics are the agent's observable working environment as well as tools
the user can operate directly.

Current competitors and prior art are tracked in
[Known competitors and prior art](competitors.md). That document records
products that meet or approach Loom's remote-control and portable-backend
requirements, along with patterns to adopt and failure modes to avoid.

## Target workflows

Loom should support these concrete workflows:

- **Interactive coding:** start a session in a workspace, attach one or more
  repositories, and ask an agent to understand, modify, and validate them
  while observing every action.
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
  Agent and Edit modes automatically approve non-destructive actions by default,
  with a per-session opt-out; destructive actions remain denied.
- **Remote control:** connect to a backend on a workstation, server, or
  development container from a native or browser client.

## Initial end-to-end workflow

The first release should optimize for this exact loop:

1. Create or open a workspace; it does not need a folder or repository.
2. Start a named agent session with a selected provider and model.
3. Select sources for that session. Loom creates isolated checkouts for remote
   repositories; a native local directory is attached in place and uses its
   original files.
4. Enter a task in natural language and optionally attach an issue, files,
   prior sessions, or repository instructions.
5. Let the agent inspect the repositories and present a plan.
6. Approve the plan and individual high-risk actions according to policy.
7. Watch streamed model messages, tool calls, terminal output, file changes,
   diagnostics, and test results.
8. Interrupt or redirect the agent, then resume from the preserved context.
9. Review the resulting diffs, test evidence, and agent summary.
10. Continue the session, create commits, or hand the work to another agent.
11. Disconnect and reconnect from a native or browser client without losing
    the backend session.

## Feature areas

| Area | Target capability |
| --- | --- |
| Workspaces | Create, rename, configure, and navigate durable containers for sessions; no repository or directory is required |
| Session repositories | Attach one or more repository sources and revisions to a session; create isolated clones or worktrees under its filesystem root |
| Agent sessions | Create, rename, pause, resume, interrupt, retry, archive, permanently delete archived sessions, fork, and compare persistent sessions |
| Agent orchestration | Plans, steps, dependencies, child agents, parallel tasks, retries, cancellation, budgets, and durable event history |
| Model providers | Multiple hosted providers, OpenAI-compatible endpoints, local model servers, model discovery, per-session selection, fallback, and usage reporting |
| Agent context | Repository instructions, conversation history, file references, tool results, summaries, compaction, token budgets, and context inspection |
| Permissions | Approval policies for file reads/writes, commands, network access, secrets, plugins, and destructive operations |
| Tool execution | Search, file operations, patching, terminals, diagnostics, source control, language services, HTTP, and extensible tool adapters |
| Editor | Read-only file and diff previews tied to agent work; full editing remains a later surface |
| Navigation | Workspace and session navigation plus direct links into agent changes; fuzzy and symbol navigation remain later |
| Terminal | Bounded command and task output in the agent timeline; interactive terminals remain a later surface |
| Tasks | Agent-owned and user-owned build/test/lint commands with run, monitor, cancel, restart, and artifact inspection |
| Source control | Read-only status and diff review for agent work; staging and commit controls remain later |
| Diagnostics | Structured errors, warnings, locations, severity, and links back to source |
| Review | Proposed edits, diff views, checkpoints, test evidence, commit preparation, and agent session transcripts |
| Collaboration | Remote backend access, session sharing, reconnect, handoff, and a clear access/permission model |
| Extensibility | Versioned capabilities and adapters for model providers, tools, language servers, source-control providers, and task runners |
| Accessibility | Keyboard-first operation, focus visibility, scalable UI, reduced motion, and screen-reader-compatible semantics where supported |

Features should be added behind stable domain interfaces. A client must be
able to discover which capabilities a backend supports instead of assuming
that every installation has the same tools available.

The M5 vertical slice makes the orchestration experience concrete without
changing the agent/session authority. A compact GPUI shell puts workspaces
and their sessions in a small navigator, the active conversation and run
timeline in the main canvas, and the composer at the point of control.
Approvals, changed-file diffs, task results, and evidence open as focused review
surfaces. File contents and repository state are read-only projections in
M5; the client is not a general-purpose editor.

## Archived session management

Archiving retains a session permanently: it becomes terminal and immutable,
and its runs, transcripts, checkpoints, filesystem root, and child worktrees
stay in durable state. There is no restore or unarchive, and only an archived
session can be deleted, so archived sessions accumulate until a user or an
operator deletes them.

The client provides an archived management view for that. It lists archived
sessions grouped by project with each session's archive age, and offers a
delete action. Deleting a project root deletes the whole project tree,
including every descendant session and its worktrees, while deleting a
descendant removes only that session. The delete action carries a force choice
for a linked worktree that has changes or a lock: without it, the deletion is
refused and the worktree is preserved. The view displays the active retention
policy read-only, so a user can see whether the backend removes old archived
sessions automatically and whether that sweep may discard dirty or locked
worktrees.

## Guiding principles

1. **Backend owns truth.** The frontend renders state and submits commands; it
   does not become the source of truth for a workspace, session filesystem,
   agent, or task.
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
