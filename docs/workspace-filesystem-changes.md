# Lightweight workspace filesystem listing and change tracking

Status: implemented (see "Implementation notes" at the end for the one deliberate
deviation from the proposal below).

## Problem

`loom-workspace` modelled a session filesystem as one recursive tree: the
workspace root **plus every mounted directory**. Almost every operation called
`Workspace::snapshot()`, which:

- walked the root **and every mount source** (`collect_entries`,
  `crates/loom-workspace/src/lib.rs`), and
- called `fs::read()` on every file to compute a SHA‑256 revision.

So "listing files" meant "read and hash every byte under the workspace and all
mounts". A session with a large attached directory (for example `~/Downloads`,
about 2 GB) made startup, review refresh, tool calls and run start all block.

`snapshot()` was on every path:

| Caller | When |
| --- | --- |
| `restore_state` | workspace open / session load |
| `poll_changes` | review refresh |
| `create_checkpoint` | every agent run start |
| `context_files` | run start |
| `list_files` / search / glob tools | each tool call |
| `GetSessionFilesystemSnapshot` | Files tab |

## Decisions

Agreed with the maintainer:

1. **Tracking Loom's own edits is enough.** No requirement to detect changes
   made by other processes/editors, so no filesystem notification watcher is
   needed.
2. **Per-file undo is sufficient.** Global "before agent run" checkpoints are
   not required for recovery. (See the deviation note: the run-retry feature
   still depends on them, so they are kept but are now cheap.)
3. **Project root only.** Generic enumeration is scoped to the workspace root;
   attached directories are opaque entries and are not traversed implicitly.
4. Native-only behaviour is acceptable.

## Architecture

Split the single expensive `snapshot()` into three cheap concerns:

### 1. Listing is metadata-only

`WorkspaceEntry.revision` is a cheap fingerprint derived from file metadata
(kind, size, modification time in nanoseconds). Listing never reads file
contents. Content hashes are still computed when a caller actually reads a file
(`read_file`) or captures edit/checkpoint content.

### 2. Enumeration is root-scoped

- `Workspace::snapshot()` walks only the workspace root and emits each mount as
  a single opaque directory entry. It no longer descends into mount sources.
- `Workspace::list(relative, depth, limit)` is a metadata-only, bounded walk of
  an explicitly requested subtree. It resolves through mounts, so a caller that
  asks for `sources/local` can still list that project; the walk is limited to
  what was requested.
- Tools (`list_files`, search, `glob`) and `context_files` use `list` instead of
  a global `snapshot`.

### 3. Changes are event-sourced from edits

The change log is driven by explicit workspace edits:

- `apply_edit` / `apply_user_edit` / `write_file` append a
  `SessionFilesystemChange` (`Created` / `Modified` / `Deleted`) and advance the
  sequence.
- `poll_changes()` no longer diffs snapshots; it simply reports changes recorded
  since the last cursor via the existing `changes_since` log.
- `restore_state` no longer captures a baseline snapshot, so opening a workspace
  does not walk or hash anything.

This removes `watcher_snapshot`, the repeated full-tree diffs, and the 2 GB
content reads. The per-file `EditRecord.before` content captured at edit time
continues to back diffs, undo and revert.

## Behaviour changes

- Review "changed files" reflects files Loom created/modified/deleted. Files
  changed on disk by other tools are no longer detected.
- Generic listing (Files tab, JSON snapshot) shows the root plus mount points,
  not mount contents. Requesting a path inside a mount lists that subtree.
- Repository instruction files are discovered under the workspace root only.

## Non-goals

- OS file notifications / inotify. Judged unnecessary because Loom edits are the
  source of truth.
- Cross-process change detection.
- Reworking the root/mount identity of a session.

## Implementation notes

- `loom-workspace`: metadata-only entries, root-scoped `snapshot`, bounded
  `list`, edit-sourced changes, no restore-time baseline.
- `loom-tools`: list/search/glob use the scoped walker.
- `loom-server`: review changes come from the edit-sourced log; the Files tab
  snapshot is root-scoped.
- Checkpoints are **kept**. `retry_from_checkpoint`
  (`crates/loom-server/src/connection/runs.rs`) reverts the filesystem before
  retrying a run, which is a distinct feature from per-file undo. Because
  `snapshot()` is now root-scoped and metadata-only, capturing a checkpoint no
  longer reads attached directory contents, so the run-start checkpoint is
  cheap. Removing checkpoints entirely would be a separate, larger change.
