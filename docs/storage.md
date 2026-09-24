# Durable storage

Loom stores persistent backend state in a SQLite database at the configured
persistence path. The database uses SQLite's WAL journal and `synchronous =
FULL`; each logical state group is a row in the `sections` table and updates
are committed in one transaction. Sessions, journals, runs, workspaces,
provider state, usage, models, policies, workspace worker-node configuration,
and idempotency records are kept in independent sections, so changing one
group does not rewrite the others.

Workspace configuration is keyed by project ID and contains worker WebSocket
URLs, a monotonically increasing revision, and the session-card CPU pulse
threshold (default 5%). Access tokens and repository/workspace files are not
part of that configuration. Browser clients keep only the bootstrap worker
URL and bearer token in origin-scoped
`localStorage`, then fetch the full workspace configuration from the backend.
Because browser scripts can read `localStorage`, deployments must trust scripts
served from the same origin; the bootstrap token is never sent to peer nodes as
part of config distribution.

Native clients store each connected peer's access token separately in the
operating system credential store, scoped to the workspace project and peer
URL. Configured peers are reconnected at startup when their matching credential
is available; peers without one remain offline until reauthenticated. Removing
a peer also deletes its local credential. No plaintext-file fallback is used:
if the OS store is unavailable, the peer remains connected for the current
session only and the UI warns that it will not reconnect after restart. Linux
uses Secret Service, which requires an available user session/keyring. Browser
peer-token behavior is unchanged.

The current schema version is stored on every section. The configured path must
be a SQLite database; other file formats are rejected rather than migrated or
retained.

SQLite checkpoints and WAL files are managed by SQLite. The application keeps
the existing event-retention and idempotency-retention limits; compaction is
performed by SQLite checkpointing and its normal page reuse. Detailed history
is loaded by section, while interrupted runs continue through the existing
recovery path and are persisted as recovery state.
