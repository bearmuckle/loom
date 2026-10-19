# 0005. Automatic project titles, pinned by a manual rename

- Status: Accepted

## Decision

A project's row in the sidebar is rendered from its root agent session's `name`,
so that name is the project title. The backend chooses it automatically instead
of asking a person to name every project:

- The naming policy lives in the neutral domain crate
  (`crates/loom-core/src/title.rs`) so it is unit-testable without a provider:
  sanitising, the deterministic goal fallback, placeholder detection, and the
  divergence test that decides whether a candidate is worth applying.
- The candidate is the plan title the model proposes (`propose_plan`'s optional
  `title` field), or a title derived from the run's goal when no plan title
  exists. A plan title is a naming hint for the proposal moment and is not
  persisted, so a restored plan carries none.
- `crates/loom-server/src/backend/project.rs` (`maybe_auto_title`) applies the
  policy from the run observer in `crates/loom-server/src/backend/runs.rs`,
  after the existing event recording, using the cached run state rather than
  the runtime lock the run loop holds. Only a project root is titled: a session
  with no project record, or a hierarchy row at depth 1 without a parent.
- `SessionTitleSource` records who chose the name. A manual rename
  (`SessionManager::rename`) pins the title as `Manual`, and automation then
  skips the session; the automatic rename uses `rename_with_source(.., Auto)`.
  A client can change the source without renaming through
  `SetAgentSessionTitleSource`.
- The behaviour is per workspace: `WorkspaceConfig::auto_title_projects`
  defaults to true, so an upgraded workspace titles itself, and a workspace that
  turns it off keeps the names it has.

## Consequences

- A new project starts with its created name (for example "New project") until
  its first run plans or completes, and is re-titled silently when the work
  changes enough (`titles_diverge`).
- Titles never contain paths or URLs, are bounded to six words and 48
  characters, and start with an upper-case letter, so a project row stays
  readable.
- A person's name always wins: automation cannot replace a pinned title, and
  re-enabling automation for a session is an explicit request.
- The session snapshot, the rename event, and the session catalog carry the
  source, so a client renders state after a reconnect without replaying the
  journal.

## References

- `crates/loom-core/src/title.rs`: the policy and its unit tests.
- `crates/loom-core/src/session.rs`: `SessionTitleSource`, the snapshot field,
  and the rename/title-source events.
- `crates/loom-session/src/lib.rs`: `rename`, `rename_with_source`, and
  `set_title_source`.
- `crates/loom-server/src/backend/project.rs` and
  `crates/loom-server/src/backend/runs.rs`: where a candidate is applied.
- `crates/loom-tools/src/lib.rs` and `crates/loom-agent/src/activity.rs`: the
  `propose_plan` schema and the guidance that asks for a title.
- `crates/loom-persistence/src/schema.rs` and
  `crates/loom-persistence/src/catalog.rs`: the `sessions.title_source` column
  and its migration.
- `docs/protocol.md`: protocol 11.11, the request, the events, and the
  workspace setting.
