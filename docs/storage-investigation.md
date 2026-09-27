# SQLite state and startup investigation

Investigated 2026-09-26 against source revision `b08879f` and the local user-level
state database. Measurements below use decimal MB. The database was read through
a read-only SQLite connection; the compaction experiment used a disposable backup.
No application behavior or stored user state was changed.

The design needs revision. SQLite is being used as a transactional container for
serialized in-memory state. The main problems are eager restoration, whole-state
saves, full-file checkpoints, and missing lifecycle policies. Changing JSON to a
binary encoding would leave these problems in place.

## What is actually stored

| Item | Measured size or count |
| --- | ---: |
| Database file | 26.05 MB |
| Free pages inside the database | 10.42 MB, 40% of file |
| Live JSON section payloads | 15.58 MB |
| Session filesystems | 10.415 MB |
| Server event journal | 3.721 MB |
| Runs | 1.304 MB |
| Other sections combined | 0.140 MB |
| Sessions | 33, including 29 archived |
| Runs | 10: 8 completed and 2 failed |
| Journal | 3,874 events across all 33 sessions |

The payload column is declared `BLOB`, but its contents are UTF-8 JSON produced
by `serde_json::to_vec`. There are 13 section rows, rather than individual rows
for sessions, runs, events, or checkpoints.

Vacuuming the disposable backup reduced its file size to 15.61 MB. This confirms
that much of the apparent growth is reusable free space. It does not resolve the
live-data growth or startup work. A SQLite WAL checkpoint and application-history
retention are separate concerns; checkpointing does not delete application history.

Source: [persistence implementation](../crates/loom-persistence/src/lib.rs),
`load_section`, `save_sections`, and `connection_for_write`.

## Journal scope and retention

The native UI intentionally uses one user-level database, independent of the
startup directory. The server journal has a global sequence and survives process
restarts. Its events carry session IDs, and session-specific reads filter by that
ID. A shared database and global reconnect cursor can be reasonable choices.

The server journal is **bounded by event count**, with a default of 4,096, rather
than growing indefinitely in event count. This database has not yet reached that
limit. There is no aggregate byte limit. One busy session can evict another
session's events because retention is global. In this sample, 3,846 of the 3,874
events belong to archived sessions; they occupy about 3.708 MB.

There is also a separate lifecycle-event vector in `SessionManager`, persisted
inside `sessions`, which has no retention bound. Filesystem changes, edit history,
checkpoints, completed runs, and provider usage records likewise lack lifecycle
cleanup in their current managers. Archiving changes the session state without
unloading or pruning its data. Archive should preserve user history, but should
not require keeping its execution machinery and rollback data active indefinitely.

The journal duplicates information stored elsewhere. Its largest event groups
are activity records (0.914 MB), context inspections (0.741 MB), tool completions
(0.564 MB), output chunks (0.554 MB), and assistant deltas (0.509 MB). The runtime
also stores messages and activity results. Some duplication is useful during
streaming, but its retention should be explicit.

Forking currently copies retained source-session journal events. That makes
forked history depend on what the global reconnect buffer still contains. Durable
conversation history needs its own authoritative representation, independent of
reconnect retention.

Sources: [server](../crates/loom-server/src/lib.rs), `EventJournal`,
`archive_session`, and the `ForkAgentSession` request handler;
[sessions](../crates/loom-session/src/lib.rs), `SessionManager::record`;
[workspace](../crates/loom-workspace/src/lib.rs), `WorkspaceState`;
[providers](../crates/loom-providers/src/lib.rs), `UsageLedger`.

## Checkpoints dominate live storage

The filesystem section contains 10 checkpoints with 536 file entries. Checkpoints
alone occupy 10.156 MB serialized. A checkpoint captures the full contents of
every eligible text file in the session filesystem, including mounted sources.
Starting an agent run automatically creates a checkpoint before the run.

The stored checkpoint contents contain 9.518 MB of text, of which 7.681 MB is
distinct by exact content. Deduplication would help, but these measurements show
that deduplication alone is insufficient. Capturing entire source trees when a run
may change only a handful of files is the larger problem. Archived sessions own
10.324 MB of the 10.415 MB filesystem section.

Store checkpoint manifests of content-addressed blob references rather than
repeated embedded file contents. Before-images for only modified files are a
possible later optimization, but cannot replace full checkpoints while arbitrary
shell commands can modify files outside an intercepted write API. Preserve
conflict-safe rollback and explicitly define how external edits are handled.
Do not rely exclusively on Git:
sessions can contain mounted directories, untracked files, and non-Git data.

Sources: [workspace](../crates/loom-workspace/src/lib.rs),
`create_checkpoint` and `revert_checkpoint`;
[server](../crates/loom-server/src/lib.rs), `start_run_with_options`.

## Startup loads and scans historical state

`restore_persisted` loads every section, including all archived sessions,
filesystem histories, and completed runs. Most sections are decoded to
`serde_json::Value` before conversion into their typed structures. Every persisted
run gets a runtime and a cloned cached runtime state.

For every stored session filesystem, `Workspace::open` scans its root. After
mounts are restored, `restore_state` scans it again, including mounted sources.
These scans read and hash file contents; they are not just directory listings.
Checkpoint restoration also validates paths and hashes stored file contents.
Repository handles and providers are restored for historical sessions/runs too.

The UI calls this synchronously before opening its window. A missing filesystem
root or invalid repository belonging to an archived session can fail startup
for the whole application.

Measurements distinguish database cost from filesystem cost:

- Reading all section payloads through Python's SQLite binding took about 19 ms;
  JSON decoding took about 74 ms in total in that sample.
- A standalone optimized Rust harness using the current `loom-workspace` source
  parsed the analysis JSON to `Value` in about 39 ms.
- Calling the real workspace scan code over the stored roots and mounts, counting
  two root scans and one mounted-source scan as startup does, read approximately
  252 MB across 4,097 entry visits. Three warm-cache optimized runs took
  215, 195, and 197 ms. Archived sessions accounted for about 169 ms per run.
- The same harness in an unoptimized debug build took 9.89, 10.26, and 10.47
  seconds for the corresponding scans; archived sessions accounted for 8.46,
  8.79, and 8.87 seconds. JSON-to-`Value` decoding took about 398 ms. This
  reproduces a multi-second component cost when running an unoptimized build.

These are component measurements, not end-to-end UI startup timings. The harness
does not include checkpoint validation, Git/provider setup, recovery persistence,
or the remainder of UI initialization. It does not establish the exact cause of
the reported multi-second launch time. The server now logs catalog/feed load time
and total restore time, including the count of resumable runs and filesystems left
lazy. More granular recovery phases and actual UI initialization still need
measurement in representative debug and optimized builds before claiming a full
startup fix.

Sources: [server](../crates/loom-server/src/lib.rs), `restore_persisted`;
[workspace](../crates/loom-workspace/src/lib.rs), `open`, `restore_state`,
`snapshot`, and `collect_entries`; [UI startup](../crates/loom-ui/src/main.rs).

## Saves scale with all retained history

`persist_state` clones all runs and filesystem state, exports the other managers,
converts every section to `Value`, and submits all 13 sections for serialization
and UPSERT in one transaction. This happens after successful durable mutation
requests and after each completed agent step. A small rename therefore traverses
and serializes unrelated checkpoints, archived runs, and journal history.

This is roughly 15.58 MB of serialized input per save in this sample, plus typed
clones and intermediate JSON allocations. It is not a measurement of physical
disk bytes written: SQLite can avoid some unchanged-page writes. The save path
nevertheless pays the application-level serialization cost for every section.
A new connection is opened for each save, with WAL/schema setup repeated.

The claim in [storage documentation](storage.md) that changing one section does
not rewrite the others is inconsistent with the server's current caller.

There is a correctness concern as well: the database transaction is atomic, but
the in-memory snapshot is assembled under separate locks. Concurrent workers
and requests can contribute state from different instants, and there is no
single ordered persistence writer covering snapshot capture and commit. An older
captured snapshot can potentially commit after a newer one. Multiple independent
backends opening the same user-level database also have no application ownership
guard. These are code-level risks; this investigation did not reproduce a
concurrent data-loss incident.

Streaming events enter the in-memory journal immediately, but the observer does
not persist every event. Step-end persistence and durable mutation requests are
separate from live publication. The redesign must explicitly define when an
event/cursor is durable and what can be lost on a crash.

Source: [server](../crates/loom-server/src/lib.rs), `persist_state`,
`spawn_run_worker`, `InProcessConnection::request`, and `run_observer`.

## Recommended direction and order

Keep SQLite. Use it for indexed records and transactional updates, with bounded
JSON payloads where flexibility is useful. A separate database per session is
not necessary to fix these issues.

1. **Make startup proportional to visible/current state.** Load workspace and
   session summaries first, render the window, and hydrate a selected session on
   demand. Recover unfinished runs deliberately. Completed and archived runs
   should not instantiate runtimes or scan filesystems on launch. Isolate failures
   to the affected session. Avoid the duplicate scan and repeated scans of the
   same mounted source.
2. **Introduce ordered incremental persistence.** Use a backend-owned writer and
   a long-lived connection. Update only affected records in a transaction, keeping
   state changes, durable events, and idempotency records consistent. Coalesce
   suitable streaming updates without weakening required crash recovery. Enforce
   single backend ownership of a local database, or design explicit concurrency.
3. **Separate durable history from reconnect traffic.** Store messages, activities,
   and run summaries by session/run ID, with keyset pagination. Keep a reconnect
   feed bounded by bytes, count, and age, with an explicit expired-cursor response
   that fetches authoritative state. Large tool outputs should be referenced
   once, rather than repeated in chunks, completion events, and activity records.
4. **Replace checkpoint payloads and define lifetimes.** Introduce checkpoint
   manifests and blob references, preserving full checkpoint semantics. Keep
   rollback dependencies for live/retryable runs; define retention for old
   checkpoints, filesystem changes, and usage detail. Support explicit deletion
   and reference-aware garbage collection. Archiving should unload data without
   silently deleting conversation history.
5. **Start clean and measure scaling.** This implementation does not import the
   section database. It must reject unsupported existing state without changing
   it; state reset is an explicit operator action outside this implementation.
   Add byte/time metrics and a deliberate maintenance policy for reclaiming free
   database pages.

A suitable schema separates `workspaces`, `sessions`, `runs`, `messages`,
`activities`, `events`, `checkpoints`, `checkpoint_files`, `blobs`, and expiring
`idempotency` records. Index events by `(session_id, sequence)` and conversation
records by their session/run and stable order. Keep small flexible event metadata
as JSON; put large content in actual binary/text blobs referenced by ID. Compression
can then be evaluated on cold blobs without making every startup decode everything.

Acceptance checks should include startup with thousands of archived sessions,
renaming one session without serializing unrelated history, bounded reconnect
storage under large outputs, concurrent writes, crash recovery around durability
boundaries, and rollback after checkpoint garbage collection. Startup and
single-session operations should be insensitive to the volume of unrelated
archived content.

The concrete replacement proposal is in [storage design](storage-design.md).
