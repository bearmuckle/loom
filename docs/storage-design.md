# Proposed durable state model

Status: target design, 2026-09-26. The implementation is in progress: session,
workspace, and run summaries, bounded session/workspace settings, reconnect
events, provider usage totals, and filesystem records use indexed rows. Runtime
details and filesystem snapshots are loaded on demand; filesystem payloads are
compressed and hash-checked. Large strings in the generic section store are
deduplicated and compressed. Provider configuration and health use
provider-keyed records. Idempotency uses a dedicated table, while detailed run
payloads and filesystem checkpoint/edit collections still use aggregate payloads.
Provider request-level detail is aggregated by provider/model because no
request-level usage history is exposed by the current protocol. This
design replaces the version-2 `sections` container. The release does not import
existing databases: it creates a fresh database and rejects an existing
unsupported format without changing it.
See the [investigation](storage-investigation.md) for measurements and code evidence.

## Decision

Keep one SQLite database per backend installation, but make **indexed domain
records the source of truth**. Store large immutable content separately from
those records, inside SQLite initially. Maintain a small, disposable reconnect
feed. Instantiate agent runtimes and filesystem services only when needed.

The application must be able to answer “show my sessions” without reading a
conversation, “show the last 50 timeline items” without opening a filesystem,
and “rename this session” without serializing anything belonging to another
session. Retaining a large conversation should cost storage, not startup time.

This is a state-oriented relational model with a transactional notification feed.
Replaying an event journal is not necessary to reconstruct current state. There
is no second unbounded lifecycle journal mirroring the same records.

## Three kinds of data

| Kind | Representation | Lifetime |
| --- | --- | --- |
| Application state and history | Typed, indexed SQLite rows | Retained with the owning session or explicit policy |
| File contents, messages, outputs, large arguments | Immutable content objects, deduplicated by hash | Retained while referenced |
| Reconnect notifications | Small records in bounded per-scope streams | Disposable after a cursor expires |

Runtime objects, locks, provider clients, Git handles, filesystem watcher state,
CPU samples, and UI caches are reconstructible process state. They are not
serialized into durable objects. Durable execution continuation is a separate,
small record with explicit fields.

## Physical representation

Use `STRICT` tables, explicit foreign keys, `CHECK` constraints, and indexes for
the actual query paths. SQLite supports these together; enable foreign-key
enforcement explicitly on every connection. [STRICT tables](https://sqlite.org/stricttables.html),
[foreign-key enforcement](https://sqlite.org/foreignkeys.html).

| Value | Storage |
| --- | --- |
| Existing UUID identifiers | 16-byte `BLOB`, `CHECK(length(id) = 16)`; UUID strings at protocol boundaries |
| Content hash | SHA-256 of uncompressed bytes, 32-byte `BLOB` |
| Time, counters, revisions, sequence numbers | `INTEGER`; UTC milliseconds for timestamps |
| Status, role, kind, short names, paths | `TEXT`; checked stable enum strings where applicable |
| Frequently queried properties | Dedicated columns, indexed where needed |
| Small extensible configuration | Versioned JSON `TEXT`, validated and size-limited |
| Large text or binary content | Raw bytes, optionally compressed, referenced by content ID |

JSON remains appropriate for small policy/options objects and provider-specific
arguments. It must not contain sessions, messages, checkpoints, or histories as
growing arrays. Start with a 16 KiB inline metadata limit; larger values become
content references. Extract any field used for filtering or ordering into a
column. Arbitrary tool arguments can remain JSON content because the tool, not
the database, interprets them.

Do not replace section JSON with MessagePack, CBOR, or bincode. That would still
require deserializing and replacing an entire aggregate. SQLite JSONB can reduce
parsing overhead but does not supply the missing relational structure: most of
its operations still have linear complexity. Keep it out of the initial design.
[SQLite JSONB](https://sqlite.org/json1.html#jsonb).

## Relational model

The following is the target logical schema. Common `created_at`, `updated_at`,
and `revision` fields are omitted where repetitive. Ownership links use foreign
keys, and composite ownership keys prevent a child from referencing another
session's run, attempt, or checkpoint. Index child foreign keys as well as the
query indexes listed below.

| Tables | Important columns and relationships |
| --- | --- |
| `store_meta` | Stable store UUID, cursor epoch, and schema version |
| `workspaces` | `id`, `name`, `config_revision`, bounded options |
| `workspace_peers` | Workspace, peer/node ID, URL, credential reference; unique workspace + peer |
| `sessions` | `id`, `workspace_id`, `name`, `archived_at`, `deleted_at`, `last_activity_at`, `latest_local_run_id`, provenance IDs |
| `session_settings` | Session, policy revision, current approval policy, auto-approval flag, bounded options; attempts capture their effective policy |
| `runs` | `id`, `session_id`, `state`, `current_attempt_id`, model/provider IDs, task/instruction content references, `record_kind` (`local`/`inherited`) |
| `run_attempts` | Run, attempt number, state, checkpoint ID, start/end times, immutable effective policy/options and pricing |
| `execution_state` | One row per resumable local attempt: phase, step number, provider cursor, pending interaction ID, last failed call ID, active message ID, control revision |
| `steps`, `plan_steps`, `evidence` | Attempt, ordered step/plan entries and evidence links; rows rather than embedded growing vectors |
| `messages`, `message_fragments` | Session, attempt, stable message ID, role, status, ordered content references; fragments carry ordinal and byte offsets |
| `tool_calls`, `tool_attempts` | Stable logical call, name, argument reference; individual execution attempts, status, result references, timing, error and process outcome |
| `interactions` | Attempt, optional tool call, approval/input kind, pending/resolved status, prompt, decision, policy revision, resolution time |
| `activities` | Session, attempt, step, parent activity, kind/status, tool-call reference, timing; no embedded copy of the tool result |
| `timeline_entries` | `(session_id, ordinal)`, exactly one message/activity/run-boundary reference; stable display order |
| `context_summaries`, `context_inspections` | Attempt, summary content, stable source boundary and input digest; optional detailed inspections with separate retention |
| `session_filesystems`, `mounts`, `repositories` | Session, owning node, root, control mode; mount source/destination, repository origin and revision |
| `checkpoints`, `checkpoint_files` | Session, purpose, status, retention/pin metadata; `(checkpoint_id, path)`, existence, content ID, expected current revision |
| `file_operations` | Session/path, prepared/applied/conflict state, before/after content IDs and revisions, source, related tool attempt |
| `filesystem_index` | Optional disposable path/stat/hash cache and scan generation, scoped to filesystem identity |
| `contents`, `content_parts`, `blobs` | Immutable logical objects and ordered references to deduplicated raw/compressed byte chunks |
| `feed_streams`, `feed_events` | Scope ownership, sequence high-water mark and pruning boundary; small ordered change notifications |
| `idempotency` | Scope, request UUID, request hash, result reference/small response, creation/expiry times |
| `usage_records`, `usage_totals` | Attempt/step usage and prices; separately maintained per-run/provider/time aggregates |
| `provider_configs`, `model_catalog` | One provider/model per row, bounded options and credential references; catalog is refreshable cached data |
| `operations` | Durable preparation/completion state for multi-stage actions such as fork, filesystem removal, and revert |

Session archival is independent of execution state. Archiving does not erase
whether the last run completed or failed. Session state returned by the protocol
is a projection of archival state and the latest local run, rather than a second
independently maintained execution state machine.

Only `execution_state` rows represent runnable continuation. Completed history
does not instantiate an `AgentRuntime`. Retrying a failed tool adds a tool attempt;
retrying from a checkpoint adds a run attempt. Earlier messages/results remain
historical records instead of being overwritten when runtime counters reset.

Context compaction stores a summary plus the exact ordered message boundary it
covers, with the context-projection version/digest. It does not delete the
transcript or use a mutable array index as the only durable boundary. Model input
loads the summary and required suffix for that attempt. Provider-history repair
must operate on a deterministic projection with a matching boundary.

## Queries and indexes

These are representative index definitions, assuming the columns above. Use
stable tie-breakers and keyset pagination rather than large `OFFSET` scans.

```sql
CREATE INDEX sessions_visible
ON sessions(workspace_id, last_activity_at DESC, id DESC)
WHERE archived_at IS NULL AND deleted_at IS NULL;

CREATE INDEX sessions_archived
ON sessions(workspace_id, archived_at DESC, id DESC)
WHERE archived_at IS NOT NULL AND deleted_at IS NULL;

CREATE INDEX runs_by_session
ON runs(session_id, created_at DESC, id DESC);

CREATE UNIQUE INDEX one_live_local_run
ON runs(session_id)
WHERE record_kind = 'local'
  AND state IN ('planning', 'executing', 'evaluating',
                'awaiting_approval', 'needs_input', 'paused');

CREATE INDEX messages_for_context
ON messages(attempt_id, ordinal);

CREATE INDEX pending_interactions
ON interactions(session_id, created_at, id)
WHERE status = 'pending';

CREATE INDEX idempotency_expiry ON idempotency(expires_at);
```

Use composite primary keys for `timeline_entries(session_id, ordinal)`,
`message_fragments(message_id, ordinal)`, `checkpoint_files(checkpoint_id, path)`,
`content_parts(content_id, ordinal)`, and `feed_events(stream_id, sequence)`.
Evaluate `WITHOUT ROWID` for those tables; do not apply it indiscriminately.
Partial indexes can exclude archived sessions and resolved interactions from the
main access paths. [SQLite partial indexes](https://sqlite.org/partialindex.html).

For example, the session picker queries only summary rows:

```sql
SELECT id, name, last_activity_at, latest_local_run_id
FROM sessions
WHERE workspace_id = :workspace
  AND archived_at IS NULL AND deleted_at IS NULL
  AND (last_activity_at, id) < (:before_time, :before_id)
ORDER BY last_activity_at DESC, id DESC
LIMIT :page_size;
```

The first page omits the cursor predicate. Opening a conversation seeks the last
50 timeline entries, fetches their small referenced records, then loads content
only for the visible items. Loading tool output accepts byte ranges and a limit.
Checkpoint listing reads headers; revert reads its manifest and referenced blobs.

If transcript search is added, use a rebuildable FTS index of user-visible text.
Search indexing is a separate projection with its own storage budget. It must
not become a prerequisite for session listing or startup.

## Immutable content

Store content in SQLite initially. This keeps insertion of content references
and their bytes transactional and makes backup/recovery simpler than a second
filesystem object store. Large-content retrieval is separate from normal row
queries, so the database may grow without making summary queries read its bodies.

`contents` identifies an immutable byte sequence by SHA-256 and records its raw
length. `content_parts` references ordered `blobs`; short content uses one part.
Start with 256 KiB chunks for large content. `blobs` records chunk hash, raw
length, codec (`raw` or `zstd`), and encoded bytes. Hash uncompressed bytes so
deduplication is independent of compression settings. Compress chunks above
4 KiB only when they shrink meaningfully; benchmark and tune these starting values.

An assistant message or process output being produced is a sequence of committed
fragments, not an ever-growing BLOB replaced for every token. Stream fragments
reference immutable chunks and carry contiguous byte offsets. Closing a stream
freezes its manifest; any later compaction changes references transactionally.
The message, activity, tool result, and checkpoint can reference the same content
without copying it into each representation.

Garbage collection marks content reachable from retained domain rows and
in-progress operations, then deletes unreferenced objects/chunks in bounded writer
transactions. Each deletion batch rechecks reachability under writer ordering;
foreign keys restrict deleting referenced objects. Reference counts may be an
optimization, not the sole correctness
mechanism. User deletion removes ownership references; shared fork content lives
until its last owner is deleted.

Content APIs authorize access through an owning session/run reference. Knowing a
content hash alone must not grant access. This preserves workspace/session scopes
when several clients share a backend while allowing internal deduplication.

## Writes, durability, and recovery

One backend owns the local database through an OS process lock. Additional local
clients connect to that backend. Remote nodes own independent databases and
exchange protocol messages; they do not share the SQLite file. A dedicated writer
serializes short transactions, with a small set of read connections. WAL supports
concurrent readers but only one writer at a time. [SQLite WAL](https://sqlite.org/wal.html).

Use WAL and `synchronous=FULL` initially. Do not trade away acknowledged
durability to mask oversized transactions. Configure connections once, keep read
transactions short, and monitor WAL size/checkpoint progress. Queue and batch
limits provide backpressure; a stalled writer cannot accumulate unbounded memory.

Workers submit domain commands with expected revisions, not snapshots of all
managers. For each command, the writer:

1. Checks the scope, expected revision, and idempotency key.
2. Changes affected rows and inserts any required content.
3. Advances affected revisions and inserts small feed notifications.
4. Saves the idempotent result in the same transaction.
5. Commits, then acknowledges and publishes the committed change.

A rename touches its session row, stream metadata/event, and idempotency record.
A tool completion touches that tool attempt, associated activity/messages,
execution continuation, and usage. Neither visits unrelated sessions.
Worker caches advance at acknowledged durable boundaries. A persistence failure
pauses/reconciles execution instead of reporting an uncommitted transition as
successful. Usage entries have stable source keys so retrying a transaction cannot
double-count tokens or cost.

Persist user input, approval decisions, tool execution intent, pause boundaries,
and completion synchronously before acknowledging them or executing dependent
effects. Batch assistant/output fragments at a starting threshold of 50 ms or
32 KiB, whichever comes first, with a final flush on completion. These are initial
tuning values, not established performance claims.

Optional immediate UI previews are explicitly uncommitted, tagged with message
generation and byte offset. They do not advance the durable reconnect cursor.
After a crash, only that uncommitted tail may disappear. Committed tool outcomes
and acknowledged user decisions must survive.

SQLite cannot atomically commit a shell command or filesystem write. Persist
tool intent before invocation and outcome afterward. On restart, an intent with
no outcome is `outcome_unknown`; do not automatically rerun a possibly completed
external side effect. For controlled file writes, persist before/after references
and a prepared operation, perform the atomic file replacement with the required
syncs, then finalize. Recovery compares expected hashes and marks conflicts where
the outcome cannot be established. Multi-file revert uses a recoverable operation
with per-file progress; it must not pretend to be an atomic SQL transaction.

Recover pending approvals/input directly from `interactions`. Persisted runs
that were executing become paused/recovery-required through small row updates.
Opening the application never silently resumes a command or replays approvals.
Pending decisions include an attempt/control revision so stale clients cannot
approve a different tool invocation.

## Reconnect feed

Use a workspace stream for session-list/config changes and a session stream for
conversation/run changes. A cursor is `(store_id, epoch, stream_id, sequence)`.
Sequences are allocated transactionally and never derived from the largest
retained event. Stream metadata keeps the high-water mark and pruned-through
boundary even when every event has been deleted.

Events contain small immutable facts or revision notifications such as
`message_changed(id, revision, committed_bytes)` or
`tool_attempt_finished(id, revision)`. They never repeat full tool output,
checkpoint content, or runtime state. Consumers may fetch a newer revision and
coalesce intermediate notifications; the feed is not an immutable copy of every
historical object version. The durable domain tables supply history.

Initial configurable limits: 4,096 events, 1 MiB, or 24 hours per stream, plus
16 MiB total feed payload per backend. Whichever limit is reached first applies.
Enforce a 4 KiB notification maximum and prune contiguous prefixes. Global pressure
can expire a quiet stream entirely; no promise of a minimum reconnect duration is
made. This bounds payload bytes, not total SQLite file size including free pages.

An expired cursor returns `ResyncRequired`, followed by an authoritative snapshot
with a fresh cursor read in the same SQLite read transaction. Subscribe after that
cursor to close the snapshot/live-update race. Epoch changes invalidate cursors
after an explicit state reset or backup restoration.

This requires a negotiated protocol capability/version for scoped cursors,
paged history, and content-range reads. Do not silently reinterpret the existing
global `EventSequence`. Until old clients are retired, an adapter must expose
their documented global cursor semantics or reject unsupported negotiation.

## Checkpoints, undo, and forks

Preserve full checkpoint semantics first. A checkpoint is a manifest of paths,
existence, content references, and expected current revisions. Creating one still
requires observing the relevant source tree, but only new content bytes are
stored. Perform this work when the user/run needs the checkpoint, asynchronously
with progress, never while merely listing sessions or opening the application.
Mounts are explicit and checkpoint paths remain session-relative.

A “record only writes made through Loom” design would miss arbitrary shell
commands modifying files. It is therefore not a valid general replacement today.
An intercepted-write optimization can be introduced only for execution modes that
actually guarantee interception. Git snapshots alone do not cover all current
non-Git/mounted/untracked use cases.

Keep the current rollback conflict checks, including expected post-edit revisions
and files created after a checkpoint. Preserve existing text-file coverage in the
first implementation; binary support can use the same byte store but needs deliberate
rollback behavior. Symlinks and concurrent external changes require the existing
path/access checks. Without filesystem snapshot support, there is no promise of
a single atomic view of externally changing files.

Fork by copying compact relational history metadata at a committed cutoff and
sharing immutable content references. Allocate new local IDs and remap internal
relationships; keep origin IDs as provenance, not as ownership dependencies on the
source session. Inherited runs are read-only historical records with no
`execution_state` or executable pending approvals. New work creates a local run
with the inherited conversation selected as context. This favors straightforward
queries/deletion over a recursive parent-history graph. Fork metadata work scales
with history rows; large forks can run as background operations.

Filesystem fork preparation is a separate staged operation. Coordinate with the
source session's execution lease, build the destination, and publish a ready
session only after required preparation succeeds. A crash resumes or cleans up
the staged operation. Forking must not depend on the reconnect feed or preserve
mutable links into the source session's in-progress message/output fragments.

## Lifetimes and startup

| Data | Default policy |
| --- | --- |
| Sessions, transcript, final activities, referenced outputs | Retain until explicit deletion; archive only changes visibility/loading |
| Resumable state and pending interactions | Retain while the run is resumable |
| Executing/paused/pending run checkpoint | Pinned; never automatically evict a referenced rollback dependency |
| User-created checkpoint | Retain until explicit deletion |
| Automatic checkpoint for a terminal run | Initially retain 30 days, with user pinning; atomically mark rollback expired and release references before GC |
| Detailed context inspections and diagnostic traces | Opt-in or time/byte bounded; final summary and required context remain durable |
| Reconnect events/filesystem change notifications | Bounded disposable feeds with resync |
| Idempotency | Explicit retry horizon, initially 7 days; expiry indexed by time, not UUID order |
| Usage | Retain useful run totals; roll older detail into aggregates |
| Filesystem index, model discovery, provider health | Rebuildable caches; never required to read conversation history |

Expiry is part of the retry protocol: requests carry an immutable issue/deadline
time, and retries beyond the advertised horizon are rejected as expired rather
than silently treated as new commands. An intentional new action uses a new key.
Retain terminal operation identifiers longer when necessary for side-effect
reconciliation; a bounded response cache alone cannot promise eternal exactly-once
execution.

Checkpoint expiry is visible in the run's capabilities. Retrying from an expired
checkpoint is rejected explicitly; ordinary conversation continuation and history
remain available. An explicit storage budget may shorten terminal automatic
checkpoint retention under the same advertised policy, but cannot evict active
or user-pinned checkpoints. This is a deliberate product behavior change from
indefinite retention, not silent damage to a supposedly available rollback.

Terminal sessions and OS process handles remain live backend resources. Store
command identity/outcomes and optionally bounded scrollback, but never promise
that restoring a database reattaches a process that died with the host.

There is deliberately no fixed cap on user-created history. It grows with retained
useful data. Bounded transient state and lazy access prevent that growth from
turning into unbounded startup, memory, and rewrite costs. Expose storage totals
by session and category, together with deletion/checkpoint cleanup controls.

Startup opens the store, reads schema and a page of workspace/session summaries,
and opens the UI. A background query identifies interrupted local execution rows
and updates their recovery status without instantiating all runtimes. Selected
conversation history is paged independently. Filesystem mounts, Git, and provider
clients initialize only for a requested operation. Missing archived directories
must not prevent reading the session's messages or launching the application.

Watch active filesystems rather than repeatedly scanning all retained session
roots. Reuse source identity where mounts refer to the same directory, but do not
conflate separate checkouts. Cached stat/hash data accelerates observation; a
strong checkpoint must not blindly trust unchanged file size/mtime as proof of
identical content.

## Implementation

Replace generic `save_sections` with typed store queries and transactional
commands in `loom-persistence`. `loom-session` supplies domain validation rather
than a second complete database in `BTreeMap`s. `loom-agent` holds one working
execution context and emits incremental durable transitions. `loom-workspace`
owns live filesystem operations and checkpoint manifests, initialized on demand.
`loom-server` owns the writer, runtime cache, and protocol projections. Native and
remote clients use the same API; the browser does not become another state owner.

Implement in this order:

1. Establish the typed schema/store, ownership guard, and summary queries. Add
   phase timings and row/byte counters.
2. Migrate messages, tools, activities, and immutable content; add paging and
   canonical context loading. Make startup and history independent of filesystems.
3. Move execution, approvals, idempotency, and publication to transactional domain
   commands. Test crash boundaries before switching live writes.
4. Migrate checkpoint manifests and filesystem operations; make services lazy.
5. Introduce scoped feeds, retention/GC, and storage maintenance; remove section
   exports and their mirrored in-memory journals completely.

This release has a clean start only. It does not copy, import, rename, or remove
an existing state database. When the configured path contains an unsupported
database, startup reports that state is unsupported and leaves the file intact.
Any future import process is a separate product decision and is outside this
design's implementation scope.

## Acceptance criteria

- Increasing archived history from 100 MB to 10 GB does not make session listing
  read content blobs or create filesystem/provider services.
- Startup reads one bounded summary page and indexed interrupted-run metadata;
  initial rendering does not wait for recovery, catalog refresh, or scans.
- Rename/archive updates a bounded set of records and never exports all sessions.
- Transcript/output queries have bounded page/range sizes; query-plan checks
  confirm the intended indexes on populated fixtures.
- Stream pressure respects configured feed budgets and cannot evict transcript
  history or invalidate a pinned checkpoint.
- Forked history survives deleting the parent and cannot execute inherited
  approvals. Content remains deduplicated and GC preserves all surviving owners.
- Crash injection before/after tool intent, file replacement, approval commit,
  output flush, and feed publication preserves acknowledged state and reports
  ambiguous external outcomes.
- An unsupported existing database is rejected without changing its contents or
  SQLite journal mode. Performance is measured in both debug and release builds.

The key performance contract is about work performed, not a guessed launch-time
target: reading one session must not require processing the rest of the account.
