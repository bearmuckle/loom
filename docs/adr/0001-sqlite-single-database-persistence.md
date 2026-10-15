# 0001. SQLite single-database persistence

- Status: Accepted

## Decision

Backend state lives in one SQLite database with a single typed schema, currently
version 4, instead of separate files or stores per aggregate. Versions 2 and 3
are upgraded in place by `migrate_schema`; a database written by a newer or
unknown revision is rejected unchanged and must be wiped explicitly. Large
immutable payloads are content-addressed inside the same database
(`content_objects`, `content_blobs`), and each mutation writes in its own
transaction. `FilePersistence` and its in-memory form share the schema,
repository, and serialization code path.

## References

- `crates/loom-persistence/src/lib.rs`: `FilePersistence`,
  `DATABASE_SCHEMA_VERSION` (4), `DATABASE_MIN_MIGRATABLE_VERSION` (2).
- `crates/loom-persistence/src/schema.rs`: typed schema, `migrate_schema`,
  `content_objects`/`content_blobs`, rejection of unsupported revisions.
- `docs/storage-design.md`, `docs/architecture.md`.
