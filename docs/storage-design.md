# Durable state design

## Status

The project-session rollout upgrades forward through schema version 47. The
v41-to-v42 migration backfills existing sessions as project roots, v43 adds the
durable project-message delivery cursor to run execution state, and v44 stores
the per-run project-delegation grant. V45 adds separate per-run messaging and
inspection grants, defaulting them off for runs already in the database. V46
adds a separate per-run child-control grant, also defaulting off for existing
runs. Each migration records its own resulting version only after its schema
change succeeds, so an interrupted multi-step upgrade can resume at the next
missing migration. Incompatible databases are rejected without modification;
backend downgrades and schema downgrade migrations are unsupported. The
v46-to-v47 migration adds normalized project worktree records with ownership,
base/result and integration revisions, conflict paths, lifecycle status, and
cleanup disposition. Per-run code-worktree and integration grants default to
disabled. After that migration, a v46-only backend cannot open a database. If
an upgrade must be rolled back, restore a pre-upgrade backup or move forward
with a fix.

The v41-to-v42 migration adds project hierarchy, delegated-task, and addressed
message records and backfills one root project per
existing session without changing session IDs or deleting existing history.
Run the schema changes and backfill atomically, validate parent/root and
workspace ownership invariants before updating `PRAGMA user_version`, and leave
the database unchanged if migration fails. The v42-to-v43 migration adds the
per-run project-message cursor used to checkpoint inbox delivery with the
transcript. The v43-to-v44 migration adds `project_delegation_enabled` to each
run runtime configuration. The v44-to-v45 migration adds
`project_messaging_enabled` and `project_inspection_enabled`; the v45-to-v46
migration adds `project_child_control_enabled`. These grants remain separate,
so existing runs do not acquire new capabilities during an upgrade. Existing
runs default to disabled; new runs persist each project capability grant
chosen at start so restart recovery exposes the same tool set. The v46-to-v47
migration adds the independent worktree and integration grants; existing runs
receive neither grant. Each migration
should be tested for successful upgrade, rollback on failure, and safe retry
after interruption.

M7.4 is planned to advance v47 to v48 for branch-messaging authorization and
durable manager wait/join state. New per-run and delegated-task grants remain
independent and default off for existing records. The migration is forward
only: a v47-only backend must reject the upgraded database, and no backend or
schema downgrade is supported. Require protocol 8.0 clients before exposing
the new wire contract; protocol 7.x clients must upgrade.

The draft implementation currently adds the default-off delegated-task grants
and branch-messaging runtime grant to v48. Durable manager wait/join records
still need to be included in this migration before M7.4 is complete or
released; level-three delegation remains unavailable in the meantime.

## Data model

Typed, indexed tables are the source of truth for workspaces, sessions, runs, attempts, messages, activities, approvals and other interactions, provider state, usage, checkpoints, and filesystem history. Query and ownership fields are represented as columns and indexed for the operations that use them. Growing histories are stored as ordered child rows rather than arrays inside aggregate JSON documents.

Large immutable values—including message bodies, tool output, checkpoint file contents, and undo bytes—are stored in a shared content-addressed store inside SQLite. Equal content is deduplicated, small objects can be stored inline, and larger objects are compressed and chunked. Rows refer to content by hash; garbage collection removes content only after it becomes unreachable.

The generic JSON section store has been removed. JSON remains suitable for small bounded configuration or diagnostic payloads that are not used as query keys. Runtime objects, locks, provider clients, filesystem watchers, and UI caches are reconstructed and are not persisted.

The workspace-config JSON stores `project_agent_concurrency` with a serde
default of four (valid values are one through sixteen). Existing config blobs
without the field remain readable, and the setting needs no separate SQLite
schema migration; the delegated-task table already persists queued work. A
project may have at most fifty queued or active delegated tasks.

## Transactions and incremental writes

The backend uses one SQLite database with WAL journaling and `synchronous = FULL`. Persistent backend instances take an advisory owner lock, and graceful shutdown drains active run workers before final persistence and lock release.

Run checkpoints write the active run and its owning session, pending feed events, and changed filesystem data in one transaction. Transcript writes append new messages and streamed fragments; activity writes upsert stable IDs. Filesystem checkpoints, edits, and ordered changes use keyed deltas tied to a generation, so a failed write remains dirty and can be retried. Undo has stable edit IDs, and checkpoint revision updates commit with the edit that changes them. Idempotency results commit with their durable mutations and become visible to the in-memory cache only after commit.

Approval and input commands are tied to a run attempt and expected control revision. The decision is persisted before the worker resumes. Recovery does not automatically replay an uncertain in-flight tool effect; it reports the unknown outcome and requires an explicit retry. Interrupted planning, model, or evaluation runs recover paused and hydrate their runtime only when needed. Runs with pending tool intent use the recovery path that checks that intent.

SQLite transactions cannot roll back physical filesystem writes. Current rollback checks protect against conflicting file revisions and files created after a checkpoint, but a crash between a disk write and its metadata commit can leave the two out of sync. Durable, hash-guarded filesystem write intents are a suggested future improvement.

## Reads, feeds, and retention

Session and run summaries are read independently of transcript and file contents. Transcript and output APIs support bounded keyset pages and byte ranges. Filesystem change pages query SQLite without loading all retained history or opening an unloaded workspace; refreshing a selected filesystem still checks for external changes.

Reconnect notifications are disposable indexed feeds with per-session and per-workspace cursors, retention boundaries, and a backend-instance epoch. A restart invalidates old cursors and causes an authoritative resync. Session and workspace events share a sequence while retaining their own scopes. Feed retention is bounded; filesystem change history keeps the newest 2,048 sequence entries. UUIDv7 retryable requests have a seven-day expiry horizon; UUIDv4 request IDs use a bounded response cache.

Transcript, checkpoint, and edit/undo history have no automatic retention limit. Archived sessions remain available, and undo history is not silently capped. Any future retention policy must protect active rollback dependencies and make loss of rollback capability explicit.

## Performance evidence

Synthetic warm same-process restore measured 6.03 ms p50 for 10,000 sessions/runs and 55.40 ms p50 for 100,000 sessions/runs. Those fixtures contain sparse filesystem/checkpoint rows and do not measure cold disk startup, UI construction, or dense active histories.

A run-checkpoint benchmark seeded 10,000 transcript rows and 10,000 activity rows. Five one-message/one-activity delta updates measured 1.69 ms p50 and 1.85 ms p95; full replacement checkpoints measured 1.135 s p50 and 1.153 s p95. This measures run checkpoint work; filesystem delta writes have behavioral tests but no separate timing benchmark.

## Suggested future improvements (non-goals)

These items are outside the implemented design and do not block PR #85:

- Add hash-guarded write intents to reconcile physical filesystem changes with SQLite commits after a crash.
- Load canonical model context from bounded pages; transcript browsing is already paged.
- Define explicit retention, storage-budget reporting, and deletion controls for transcript, checkpoint, and edit/undo history.
- Narrow remaining catalog, settings, and idempotency writes to touched-row batches; define the content garbage-collection cadence.
- Expand hard-crash and failure-boundary tests, including ambiguous filesystem outcomes.
- Measure cold OS/disk and UI startup, resumable-run hydration, and dense filesystem/checkpoint workloads.
