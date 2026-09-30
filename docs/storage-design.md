# Durable state design

## Status

Loom stores durable backend state in one SQLite database with a single
baseline schema, currently version 2. Loom is pre-1.0: there is no migration
ladder and no legacy import path. A database written by any other Loom revision
is rejected unchanged, and the operator must wipe it before starting. When the
domain model changes, the baseline version and the typed schema definitions
change together.

Per-run project-agent grants are stored as one versioned JSON payload
(`run_runtime_config.project_grants`), and delegated-task grants as
`delegated_tasks.permissions`. Both use Serde defaults: unknown keys are
ignored and missing keys default to disabled, so a new grant is a code change
rather than a schema change. JSON is otherwise reserved for small bounded
configuration or diagnostic payloads that are not used as query keys.

Wiping is always opt-in. On startup the native client and CLI inspect the
database without modifying it; an unknown or newer database is reported and
wiped only when `--reset-state` is passed or the user confirms an interactive
prompt on a terminal. The wipe removes the database and its SQLite sidecar files and
refuses to run while another backend holds the writer lock.

## Data model

Typed, indexed tables are the source of truth for workspaces, sessions, runs, attempts, messages, activities, approvals and other interactions, provider state, usage, checkpoints, and filesystem history. Query and ownership fields are represented as columns and indexed for the operations that use them. Growing histories are stored as ordered child rows rather than arrays inside aggregate JSON documents.

Transcript and tool state are deliberately shallow. A transcript message is one `run_messages` row; its model tool calls and its streamed fragment descriptors are versioned JSON payloads on that row (`tool_calls`, `fragments`). A logical tool call is one `run_tool_calls` row, and its execution attempts are a versioned JSON payload on that row (`attempts`). Message tool-call arguments and tool results are stored inline in those payloads, while streamed fragment bytes remain content-addressed so they stay compressed and deduplicated. An attempt keeps a result's contents only when they carry information the action does not, which means a successful result that exists nowhere else. Every other result—a workspace restatement, a command's stdout, or any failure—is stored without its contents, because the full output the model read is already kept as the transcript message, which is also what a resumed run replays. JSON is used here only for child collections that are always read and written with their parent; the query keys (run, message ordinal, tool-call id) remain columns. These JSON payloads tolerate added fields without a schema change.

Large immutable values—including message bodies, tool output, checkpoint file contents, and undo bytes—are stored in a shared content-addressed store inside SQLite. Equal content is deduplicated, small objects can be stored inline, and larger objects are compressed and chunked. Rows refer to content by hash; garbage collection removes content only after it becomes unreachable.

Runtime objects, locks, provider clients, filesystem watchers, and UI caches are reconstructed and are not persisted.

The workspace-config JSON stores `project_agent_concurrency` with a serde
default of four (valid values are one through sixteen). Existing config blobs
without the field remain readable, and the setting needs no separate SQLite
schema change; the delegated-task table already persists queued work. A
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

These items are outside the implemented design:

- Add hash-guarded write intents to reconcile physical filesystem changes with SQLite commits after a crash.
- Load canonical model context from bounded pages; transcript browsing is already paged.
- Define explicit retention, storage-budget reporting, and deletion controls for transcript, checkpoint, and edit/undo history.
- Narrow remaining catalog, settings, and idempotency writes to touched-row batches; define the content garbage-collection cadence.
- Expand hard-crash and failure-boundary tests, including ambiguous filesystem outcomes.
- Measure cold OS/disk and UI startup, resumable-run hydration, and dense filesystem/checkpoint workloads.
