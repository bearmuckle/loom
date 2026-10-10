# Worker-scoped projects

Status: implemented for worker-scoped listing, navigation, creation, and
workspace ownership; see [Implementation status](#implementation-status).
Remaining items are called out below.

Code paths below are relative to `crates/loom-ui/src/`.

A visual companion lives in
[worker-projects-mockup.html](worker-projects-mockup.html) (open it in a browser
for the recommended "group projects by worker" navigation and the new-project
worker picker).

## Implementation status

- **Done — cross-worker listing.** `reload_sessions` walks every workspace a
  worker reports (`list_node_sessions`) instead of querying only the client's
  home `workspace_id`, so a worker that was used standalone shows its existing
  projects. Per-worker load errors are recorded and surfaced in the group
  header.
- **Done — grouped sidebar.** When more than one worker is connected, projects
  are grouped under a collapsible per-worker header (status dot and name, then
  resources or the last load error). A single worker keeps the original flat
  list.
- **Done — New project worker picker.** The New project pane has a "Run on"
  picker (default worker first) whenever more than one worker is connected.
  Choosing a worker scopes local-folder availability, clone caching, GitHub
  search, and the creation target.
- **Done — workspace ownership (option B).** A workspace belongs to exactly one
  worker. New projects are created in the target worker's own workspace (a
  `Default` is created on a worker that has none), the client no longer registers
  the home workspace on peers, and workspace settings are written to each
  worker's own workspace. There is no shared or federated workspace.
- **Remaining — settings ownership and polish.** `worker_nodes` still rides in
  `WorkspaceConfig`, concurrency is still a single fleet-wide value, and
  last-used-worker persistence and a flat/badge fallback are open (see
  [Open questions](#open-questions)).

The workspace model itself is discussed in
[The workspace model](#the-workspace-model).

## The workspace model

A **workspace** is a durable, named container for a worker's projects plus that
worker's `WorkspaceConfig` (fleet list, CPU pulse threshold, delegated-agent
concurrency). It is stored in one worker's database.

Earlier the client picked one workspace from the home worker and treated its id
as global, even registering that record onto every peer. With one backend that
was invisible; with several it produced incoherent state (a peer could end up
with two workspaces both named `Default`, and listing scope differed between
first paint and refresh).

**Chosen direction (option B):** a workspace belongs to exactly one worker.

- The user-facing hierarchy is **Worker → Project**. Workspaces are an internal
  backend container and are not exposed; a worker's workspaces are listed only
  to find its projects, and creation targets the most recently updated one.
- The client never registers a workspace onto another worker. `list_node_sessions`
  returns each worker's own workspaces, and creation/`persist_and_distribute_workspace_config`
  resolve a workspace per worker (`active_workspace` / `resolve_or_create_workspace`).
- Multi-workspace-per-worker remains possible in the data model but is not a
  v1 surface; it becomes a deliberate feature (with a switcher) only when there
  is a real use case.

**Rejected:** keeping a distributed singleton workspace id (hides a peer's own
workspaces and invites duplicates), and a client-owned workspace federated
across workers (larger change that fights "backend owns truth").

The remaining settings rehoming is the natural follow-up: provider credentials
are already per worker, `project_agent_concurrency` is really a worker resource
policy, and `worker_nodes`, the CPU pulse threshold, theme, and last-used worker
are client concerns. Moving `worker_nodes` out of `WorkspaceConfig` is what
fully removes the "global workspace" fiction; until then it is written per
worker and read from the home worker at startup.

## Problem

Once more than one worker is connected, the sidebar only shows the first
worker's projects, and there is no way to choose a worker when creating a
project. The intent to add a worker chooser even exists in the tests
(`new_session_node_choices_keep_the_default_backend_first`,
`order_session_nodes`) but the production UI never calls it.

This is not only a missing picker. The client currently treats all connected
workers as if they shared one logical workspace, while a workspace is a durable
container that lives inside a single worker's database. The mismatch is what
makes additional workers appear empty.

## How it works today

1. **A worker is a Loom server.** The client bootstraps against one worker
   (the in-process backend for native, or `--remote`/browser target) and records
   it as `default_backend_node_id`. It lists that worker's workspaces, uses the
   first one, or creates `"Default"`, and stores the resulting `workspace_id`
   (`view/lifecycle/init.rs`).
2. **More workers are added in Settings → Workers.** `add_worker_node`
   (`view/workers.rs`) pushes a `WorkerNodeEntry`, spawns a backend keyed by the
   worker's `node_id`, then calls `reload_sessions`.
3. **One workspace is forced onto every worker.**
   `persist_and_distribute_workspace_config` (`view/providers.rs`) registers the
   *home* workspace record on each peer and pushes the same
   `WorkspaceConfig`, so all workers are expected to answer for the single
   `workspace_id`.
4. **Sessions are merged across workers.** `reload_sessions`
   (`view/lifecycle/sessions.rs`) asks every online worker for
   `ListWorkspaceSessions { workspace_id }` and merges the results through
   `merge_node_sessions`, keeping a `session_node_ids` owner map. Nothing in the
   sidebar surfaces that owner.
5. **New project always targets the default worker.** `confirm_source_dialog`
   (`view/source.rs`) calls `create_session_on_node_with_source(self.default_backend_node_id.clone(), …)`.
   Repository discovery is just as hard-wired: `source_node_id()` returns the
   default worker for `StartSession`, so the clone cache and GitHub search query
   the default worker regardless of where the project will run.
6. **The sidebar is a flat "Projects" list** (`view/render/sidebar.rs`). There is
   no worker dimension and no workspace switcher anywhere in the UI.

## Why a second worker looks empty

The symptoms come from four compounding causes, in order of impact:

1. **Per-worker workspace identity mismatch.** A worker that has been used
   standalone already owns its own workspaces with their own `workspace_id`s.
   The client only ever asks that worker for the *home* `workspace_id`, so its
   real projects are never listed.
2. **A registration/listing race.** `add_worker_node` spawns workspace
   registration and calls `reload_sessions` immediately afterwards. Both are
   async; the session list can be answered before the peer has the workspace
   registered, returning empty, and nothing re-lists the worker afterwards.
3. **Silent empty results.** A failed or empty list is folded away in
   `reload_sessions` (errors are recorded but the sidebar just shows nothing for
   that worker). A user cannot tell "no projects" from "not loaded".
4. **No worker context in creation.** Even if the peer's projects did load,
   there is no UI to create new work *on* that peer.

## Design goals

- A connected worker's own projects are visible without re-homing them.
- Creating a project targets an explicit, visible worker.
- The user can always tell which worker a project runs on.
- No new protocol is required for the core fix; it should be a client-side
  change that degrades gracefully.
- Single-worker users see no extra chrome.

## Recommended model: worker as a first-class axis

Treat the client as a **fleet of independent workers**, each with its own
workspaces and projects, instead of one workspace projected across many
backends.

### Concepts

- **Home worker** — the backend the client bootstrapped against
  (`default_backend_node_id`). It keeps its embedded/local role and its existing
  default workspace.
- **Active worker** — the worker the sidebar is scoped to and the default target
  for New project. Selecting a project sets the active worker to its owner.
- **Workspace** — stays a durable per-worker container. It becomes visible only
  when a worker has more than one, and is no longer assumed to be a global
  singleton.

### Sidebar: group projects by worker

When one worker is connected, keep the current flat **Projects** list exactly as
it is. When two or more are connected, render one collapsible group per worker:

```
▾ ● Local backend · this machine
    CPU 12% · RAM 4.1/32 GiB
    ▸ loom                                    3 agents
    ▸ scratch
▾ ● External worker · build-01
    CPU 71% · RAM 12.4/64 GiB
    ▸ api-gateway                             1 agent
▾ ○ External worker · gpu-box
    offline · last seen 2h ago
    ▸ voice-lab                               4 agents
```

- The header shows a connection dot and the `worker_node_display_name` on one
  line, with live resources (`format_worker_node_resources`) on a second line so
  a long worker name is never clipped. A per-worker project count is redundant
  with the rows directly below it and is omitted.
- Projects nest with the existing session tree so delegated children keep
  nesting under their project as they do today.
- Offline/failed workers keep their last-known projects visible but subdued,
  with an inline **Reconnect** action, instead of vanishing.
- A **group by worker** toggle and a flat **worker badge** fallback cover users
  who prefer one list. The global project filter keeps spanning workers and adds
  a worker chip to each result.
- The active project's worker is shown as a badge in the main canvas header, so
  location is never ambiguous even in flat mode.

### New project: choose the worker first

The New project pane gains a **Run on** picker as its first control:

```
New project

Run on   [ ● Local backend · this machine ] [ ● External worker · build-01 ] [ ○ gpu-box ]

What it starts with
  [ Empty project ] [ Local folder ] [ GitHub repository ]

Run on build-01 · CPU 71% · RAM 12.4/64 GiB · 4 models available
                                       [ Cancel ]  [ Create project ]
```

- The picker lists connected workers via `order_session_nodes` (default/home
  worker first). It is promoted out of `#[cfg(test)]` and reused.
- Source choices adapt to the chosen worker:
  - **Local folder** is offered only for the local/home worker, matching the
    existing `local_source_available` rule; it is disabled with an explanation
    on remote workers.
  - **GitHub repository** reloads that worker's clone cache and searches GitHub
    through `source_node_id()`, which now reads the dialog's chosen worker.
  - **Empty project** works anywhere.
- The model list and default-model validation already run against the target
  node in `create_session_on_node_with_source`; the picker makes that target
  explicit and surfaces the "choose a model configured on this worker" error at
  the right moment.
- The picker appears only for a new project, not when adding a source to an
  existing session (there the worker is fixed to the session's owner). The
  default choice is the home worker; last-used persistence and a target summary
  on the confirm button are deferred.

### Cross-worker presentation

- The group header is the primary signal: status dot, worker name, and live
  resources or the last load error.
- **Deferred:** a flat list with per-row worker chips, a group-by-worker toggle,
  and a worker badge in the main canvas header. Grouped mode currently shows
  whenever more than one worker is connected.
- Cross-worker operations are deliberately *not* implied. A project's delegated
  agents already run on the worker that owns the project
  (`refresh_missing_project_snapshots` resolves the node per session), so there
  is no cross-worker delegation or file sharing to present. The fleet view
  answers "where does this run", not "merge these into one workspace".
- Long term, per-worker grouping is also the natural place to show a
  **workspace switcher** if a worker has several workspaces.

### Connection and workspace resolution

A worker is listed by walking every workspace it reports, and each worker owns
its workspaces:

1. `reload_sessions` gathers the online node backends and calls
   `list_node_sessions` per node.
2. `list_node_sessions` calls `ListWorkspaces` on the node, then
   `ListWorkspaceSessions` for each returned workspace and merges the results,
   returning the node's workspaces alongside its sessions. A node with no
   workspace yet returns empty; one is created on demand at first creation.
3. Each node's sessions are merged under that node's owner with
   `merge_node_sessions`; a per-node error is recorded so the group header can
   show it instead of an ambiguous empty list, and the node's workspaces are
   stored in `node_workspaces`.
4. Creating a project resolves the target worker's own workspace
   (`active_workspace`, or `resolve_or_create_workspace` which creates a
   `Default` when the worker has none) and creates the session there.
5. `persist_and_distribute_workspace_config` writes the config to the home
   workspace and to each peer's own workspace; it no longer registers a
   workspace onto another worker.

This removes the distributed-singleton assumption entirely. `worker_nodes`
still rides in the config and is read from the home worker at startup, which is
called out as the remaining settings-ownership item.

## Data model and code changes

Implemented client-side, in `crates/loom-ui`:

- `LoomView`: added `node_workspaces: BTreeMap<String, Vec<WorkspaceRecord>>` (a
  worker's own workspaces, from listing). Replaced the single `session_tree`
  with `session_trees: BTreeMap<String, Entity<TreeState>>` and
  `session_tree_entries: BTreeMap<String, Vec<SessionTreeNode>>` keyed by node
  id, plus `collapsed_worker_nodes: BTreeSet<String>` and
  `node_session_load_errors: BTreeMap<String, String>`. `default_backend_node_id`
  and `session_node_ids` are unchanged.
- `SessionSourceDialog`: added `target_node_id: String`; `source_node_id()`
  returns it. `choose_source_node` resets clone/search state and local-folder
  availability when the worker changes.
- `begin_source_dialog` defaults the target to the active session's owner for
  AddToSession and the home worker for a new project; `confirm_source_dialog`
  creates on the dialog's target for a new project.
- `local_source_available` now keys off the chosen target worker rather than the
  dialog purpose.
- `order_session_nodes` was promoted out of `#[cfg(test)]` and drives the
  picker ordering.
- `reload_sessions` / `list_node_sessions`: list every workspace per node,
  return the workspaces, and surface per-node errors.
- `create_session_on_node_with_source` creates in the target worker's own
  workspace; `resolve_or_create_workspace` creates a `Default` on a worker that
  has none. It no longer registers a workspace onto the target.
- `persist_and_distribute_workspace_config` writes config per worker
  (`resolve_workspace_id` / `resolve_workspace_id_async`) and only closes the
  connection for a retiring node. The client `register_workspace` helpers were
  removed.
- Helpers: `group_sessions_by_worker` assigns sessions to owners, keeps empty
  groups for known workers, and preserves unknown owners; `active_workspace`
  picks a worker's most recently updated workspace.
- `remove_worker_node` also drops the removed node's workspaces and load error.

## Protocol and backend considerations

- The core fix needs **no new protocol**: `ListWorkspaces`, `RegisterWorkspace`,
  `CreateWorkspace`, `SetWorkspaceConfigForWorkspace`, and
  `ListWorkspaceSessions` already exist. The client no longer sends
  `RegisterWorkspace`, but the backend keeps supporting it.
- Optional follow-ups: a per-worker "resolve workspace" response that reports
  whether a workspace was created or reused, and a way to enumerate a worker's
  workspaces with project counts, so the group header can be rendered before
  sessions load.
- Keep the existing capability negotiation; grouping must degrade to the current
  flat list when a worker or the protocol does not support a new projection.

## Alternatives considered

- **Keep the distributed singleton workspace and just fix the race.** Cheapest
  short-term, but it still hides a peer's pre-existing workspaces and keeps the
  fragile assumption that every worker can adopt one global `workspace_id`.
  Rejected as the primary direction; the awaited registration is still worth
  doing as an independent correctness fix.
- **Worker → Workspace → Project tree.** More faithful to the domain, but adds a
  navigation level most users never need and was not chosen. Kept as an
  opt-in expansion for workers that actually hold multiple workspaces.
- **A pure coordinator/aggregator service.** Would enable true cross-worker
  federation, but is a much larger change and is unnecessary while a project and
  its agents are worker-local.

## Delivery plan

### Slice 1 — correct cross-worker listing (implemented)

- List every workspace a worker reports and merge its sessions under that
  worker's owner; surface per-worker load errors.
- Reload sessions on connect/reconnect/removal as before.

**Exit met:** a second, independently-used worker's existing projects appear in
the sidebar; a worker that genuinely has none is distinguishable from one that
failed to load (its header shows the error).

### Slice 2 — grouped sidebar (implemented, minus the flat fallback)

- Group by worker when more than one is connected, with a collapsible header
  showing status dot, name, and resources (or the last load error).
- A single worker keeps the flat list.

**Deferred:** a flat mode with worker badges and a group-by-worker toggle.

**Exit met:** the sidebar shows which worker owns each project, and an offline
worker's known projects remain listed.

### Slice 3 — New project worker picker (implemented, minus persistence)

- **Run on** picker (default worker first) shown when more than one worker is
  connected; choosing a worker scopes local-folder availability, clone caching,
  GitHub search, and the creation target.

**Deferred:** model validation messaging polish and last-used-worker
persistence.

**Exit met:** a project can be created on any connected worker, and its sources
are discovered on that worker.

### Slice 4 — per-worker workspace ownership (implemented, option B)

- A workspace belongs to one worker; create projects in the worker's own
  workspace (`Default` on demand) and write config to that workspace.
- Stop registering the home workspace onto peers.

**Exit met:** a project created on a peer lives in the peer's own workspace, and
the home workspace is never registered there.

### Slice 5 — cross-worker polish (remaining)

- Active-project worker badge in the canvas header; global filter spanning
  workers with chips; flat/badge fallback; per-worker workspace switcher when a
  worker holds several workspaces; move `worker_nodes` and UI preferences out of
  `WorkspaceConfig`.

## Verification

Implemented checks live in `crates/loom-ui/src/view/tests/`:

- `worker_project_tests`: `group_sessions_by_worker` ordering and empty-group
  behavior; grouped sidebar render with two workers and collapse/expand;
  `choose_source_node` switching the target and local-folder availability plus
  the rendered "Run on" picker; `reload_sessions` loading each worker's
  projects; creation on a peer using the peer's own workspace without receiving
  the home workspace.
- `worker_node_tests`: `order_session_nodes` ordering, session aggregation,
  owner routing, and `local_source_available` target rules.
- Existing render tests continue to cover the single-worker flat list.

## Open questions

- Should grouped mode be remembered as a preference, and should the flat
  badge/filter fallback ship?
- When a worker holds several workspaces, is a per-worker workspace switcher
  enough, or should they group under the worker header?
- Should `worker_nodes` move out of `WorkspaceConfig` to a client-side fleet
  config (and `project_agent_concurrency` to a per-worker setting)?
- Do we expose "last used worker" per client or per workspace?
