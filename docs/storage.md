# Durable storage

Loom stores persistent backend state in a SQLite database at the configured
persistence path. The database uses SQLite's WAL journal and `synchronous =
FULL`; each logical state group is a row in the `sections` table and updates
are committed in one transaction. Sessions, journals, runs, workspaces,
provider state, usage, models, policies, and idempotency records are kept in
independent sections, so changing one group does not rewrite the others.

The current schema version is stored on every section. The configured path must
be a SQLite database; other file formats are rejected rather than migrated or
retained.

SQLite checkpoints and WAL files are managed by SQLite. The application keeps
the existing event-retention and idempotency-retention limits; compaction is
performed by SQLite checkpointing and its normal page reuse. Detailed history
is loaded by section, while interrupted runs continue through the existing
recovery path and are persisted as recovery state.
