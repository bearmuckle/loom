# ADR 0006: M5 agent workspace and orchestration surface

## Status

Accepted for the M5 redesign. The previous full coding-workspace scope is
superseded.

## Context

The M4 backend already owns canonical workspaces, task supervision, durable
agent sessions, capability negotiation, and authenticated remote connections.
The previous M5 proposal expanded the native client into a small IDE with
editor buffers, language services, navigation, and source-control controls.
That scope makes the product center on manipulating files instead of
orchestrating an agent.

M5 should instead deliver the smallest useful agent client: a project and
session navigator, one focused run view, an input composer, human-control
actions, and a read-only review surface. The GitHub Copilot app is the
functional reference for this structure. Zed is relevant only as a reference
for compact visual tone and theme, not for editor behavior or product scope.

## Decisions

1. **Make the session the primary UI object.** The client presents projects
   and durable agent sessions, not a file tree or a set of editor buffers.
   Creating, resuming, renaming, archiving, and reconnecting sessions use the
   existing backend session model.
2. **Use a minimal shell.** The default layout is a narrow project/session
   navigator and a single active-session canvas. The canvas contains the
   conversation and run timeline, with a composer at the bottom and
   connection/session status in the shell. Approvals, changed files, diffs,
   and evidence open as focused drawers or overlays instead of permanent
   IDE-style panes.
3. **Make orchestration visible and controllable.** Plans, step state, tool
   calls, bounded output, approval prompts, questions, retries, failures, and
   completion are first-class timeline items. The user can start, pause,
   resume, interrupt, approve, reject, retry, and redirect without leaving
   the active run.
4. **Keep review read-only and task-oriented.** The client can show changed
   paths, a bounded diff, task status, artifacts, and stable evidence links.
   These are projections of backend workspace, VCS, and process state; M5
   does not add local editable buffers, autosave, staging, or commit state.
5. **Reuse the existing protocol and authority boundaries.** M5 uses the
   in-process and authenticated WebSocket connections, resumable session
   events, durable run snapshots, approval policy, and capability
   negotiation already defined by M1-M4. New requests are limited to compact
   session, run, review, and evidence projections where an existing snapshot
   is insufficient.
6. **Treat the frontend as a projection.** A client disconnect or restart
   must not lose an agent run, approval, task result, or event history.
   Reconnecting replaces stale local projections from the authoritative
   snapshot before applying resumed events.
7. **Use a restrained visual language.** Compact density, subdued separators,
   clear status color, and dark/light theme support are intentional. Zed
   informs visual tone only; its editor, panes, navigation, and interaction
   model are outside M5.

## Explicitly out of scope

- Editable file buffers, tabs, splits, autosave, encoding preservation, and
  external-edit conflict UI.
- Language-server lifecycle controls, diagnostics navigation, symbols,
  definitions, references, and project-wide fuzzy search.
- Interactive terminal panes and direct command editing outside the existing
  agent/task controls.
- VCS staging, unstaging, commit creation, branch management, and conflict
  resolution.
- Rich multi-agent graph editing, worktree comparison, and selective merge
  workflows.
- Browser/wasm-specific UI work, which remains M6.
