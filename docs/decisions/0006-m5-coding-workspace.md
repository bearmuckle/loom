# ADR 0006: M5 coding workspace boundaries

## Status

Accepted for the M5 implementation.

## Context

The M4 backend already owns canonical workspace roots, revision-checked
edits, task supervision, durable agent sessions, capability negotiation, and
authenticated remote connections. A coding workspace needs editor buffers,
navigation, diagnostics, source control, and validation results without
creating a second workspace or session model.

## Decisions

1. **Buffers are a projection over `loom-workspace`.** `EditorWorkspace`
   stores tabs, panes, undo/redo history, autosave policy, normalized editor
   text, and agent markers in memory. Reads and writes go through the
   existing canonical `Workspace`; saves compare the last observed byte
   revision and return a structured external-change conflict.
2. **Preserve text representation where practical.** UTF-8, UTF-8 with BOM,
   UTF-16LE/BE, and LF/CRLF/CR styles are detected and restored on save.
   Binary/invalid text and files above the bounded editor limit are rejected.
3. **Use explicit basic language-service capabilities.** `loom-language`
   exposes lifecycle state and operation capabilities and ships a deterministic
   parser/search implementation for common languages. Unsupported languages
   are visible errors, not silent UI assumptions.
4. **Keep Git execution scoped and structured.** `loom-vcs` invokes `git`
   using direct argv arguments from the canonical workspace root. Paths,
   commit messages, status, diffs, staging, branches, and conflicts are
   represented as typed values.
5. **Reuse task artifacts as evidence.** M2/M3 task supervision remains the
   source of build/test/lint output and artifacts. Each artifact gets a stable
   `loom://task/...` evidence URI and a run may attach evidence links to its
   final snapshot.
6. **Make M5 additive in the protocol.** Editor, language, VCS, navigation,
   and evidence requests/responses are capability-gated and work through the
   existing in-process and WebSocket transports. The GPUI layout is a
   projection, not an authority.

## Deferred limitations

- The basic language service is deterministic and does not start external LSP
  processes, parse every language, or provide incremental semantic indexing.
- Search is bounded literal search rather than a full regex/index service.
- The Git service supports one repository rooted at the opened workspace and
  does not yet model worktrees, remotes, hooks, or provider-specific VCS.
- Buffer/layout state is currently process-local and is not persisted in the
  durable backend snapshot; reopening a client reconstructs it from files.
- The GPUI smoke surface is intentionally compact; richer text editing,
  keymap dispatch, and diff panes remain follow-up work.
