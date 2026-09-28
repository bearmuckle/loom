# Durable storage

Loom stores persistent backend state in a SQLite database at the configured
persistence path. Native UI clients use one user-level database at
`$LOOM_STATE_DIR/loom/state.db` (or the platform state-directory fallback),
so workspaces and sessions remain available regardless of the startup folder.
The database uses SQLite's WAL journal and `synchronous = FULL`. Durable state
uses typed, indexed domain tables for workspaces, sessions, runs, transcripts,
activities, filesystem metadata and history, checkpoints, provider state,
usage, policies, worker-node configuration, and idempotency records. Large
immutable payloads use a compressed, content-addressed store inside SQLite.
Run and filesystem checkpoint paths write keyed deltas for changed history
instead of replacing retained history on every update.

Workspace configuration is keyed by workspace ID and contains worker
WebSocket URLs, a monotonically increasing revision, and the session-card CPU
pulse threshold (default 5%).
Access tokens, session filesystem roots, and repository/worktree contents are
not part of workspace configuration. A session filesystem root is managed
separately from the workspace record and remains associated with its owning
session across backend restarts. Browser clients keep only the bootstrap worker
URL and bearer token in origin-scoped
`localStorage`, then fetch the full workspace configuration from the backend.
Because browser scripts can read `localStorage`, deployments must trust scripts
served from the same origin; the bootstrap token is never sent to peer nodes as
part of config distribution.

Native clients store each connected peer's access token separately in the
operating system credential store, scoped to the workspace ID and peer URL.
Configured peers are reconnected at startup when their matching credential
is available; peers without one remain offline until reauthenticated. Removing
a peer also deletes its local credential. No plaintext-file fallback is used:
if the OS store is unavailable, the peer remains connected for the current
session only and the UI warns that it will not reconnect after restart. Linux
uses Secret Service, which requires an available user session/keyring. Browser
peer-token behavior is unchanged.

The current database schema is version 49. Loom upgrades supported databases
forward from v41 through v49; unknown formats and versions are rejected
without modifying the existing file. Backend and schema downgrades are
unsupported, and there is no generic JSON section store retained for legacy
compatibility.

SQLite checkpoints and WAL files are managed by SQLite. Reconnect events,
idempotency responses, and filesystem change pages have explicit retention
bounds. Transcript, checkpoint, and edit/undo history currently have no
retention limit. Interrupted runs recover through the typed run state and
existing recovery rules. See [the storage design](storage-design.md) for the
implemented model and suggested future improvements.
