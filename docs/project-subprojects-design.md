# Project sub projects and quieter sub agents

> Status: design investigation for
> [GitHub issue #257](https://github.com/bearmuckle/loom/issues/257),
> “Project sub agent / project strategies.” The **sub-agent de-emphasis**,
> **auto-archive**, and **sub projects** parts are implemented. This document
> frames the decisions; the companion mockup is
> [`project-subprojects-mockup.html`](project-subprojects-mockup.html).
>
> ## Implementation status
>
> - **Sub projects (implemented).** A sub project is a project of its own — its
>   own root session, agents, model choice, and checkouts in the parent's
>   workspace — plus one durable link, `project_parents`, to the project it was
>   branched out of. Creating one is a single name field, opened from the
>   **New sub project** shortcut in the project header (or the same action's
>   dialog); the client attaches the parent's sources to the new session so the
>   branch starts where the project is, and the sidebar lists the branch directly
>   beneath its parent with a branch mark. See *A sub project is its own project*
>   below for how this differs from the nested-membership sketch.
> - **Archive from the header (implemented).** The project header offers
>   **Archive project**, which archives the root session of the project on
>   screen, so selecting an agent of a project still archives the whole tree.
> - **Sub agents de-emphasized (implemented).** The sidebar nests each project's
>   delegated agents under one collapsed summary row (`N agents · x running ·
>   y done`) that expands for inspection. A sub agent's context menu opens its
>   transcript and keeps manager-scoped controls, and no longer offers manual
>   rename or archive.
> - **Auto-archive (implemented).** `WorkspaceConfig.auto_archive_completed_agents`
>   gates it, off by default and toggled from Settings → Agents. When on, a
>   delegated agent is archived once its task is `Completed` and any code result
>   is integrated; failures and un-integrated code stay visible.

## A sub project is its own project

The original sketch below made a sub project a *nested membership* of its parent
project. Implementation showed that rework buys nothing the tree needs: a
session belongs to one project, and every project-level invariant (depth,
concurrency, retention, integration) is expressed per project. A sub project is
therefore a separate project that records its parent:

- `project_parents(project_id, parent_project_id)` holds the link, and
  `ProjectSnapshot.parent_project_id` reports it. Deleting either project drops
  the link alone.
- The parent must be a project root in the same workspace; a delegated agent is
  managed by its project and cannot own one.
- The branch is created by `ProjectRequest::CreateSubProject`, which writes the
  session row, its project root row, and the link in one transaction. Owner role
  and `create_agent_session` are required.
- Sources and the model are **not** copied by the worker: the client attaches the
  parent's repositories (reusing the worker's clone) and directories to the new
  session, so a project that was branched out and a project created on its own
  share one attach path.
- The link only groups the row in a client. A branch whose parent is not listed
  (archived, on another worker, not yet fetched) is listed as a project of its
  own rather than disappearing.

## Problem

Today a project is a root agent session that owns a goal and attaches to a git
repository (`docs/project-sessions-design.md`). When a user wants several
related streams of work on the *same* repository they create several top-level
projects that all point at that repository. The sidebar then fills with sibling
roots that share a checkout, and it is hard to tell they belong together.

At the same time, delegated sub agents are rendered as first-class rows in the
same tree as projects. They carry the same context menu as a project, including
Archive, even though a user rarely wants to interact with or archive an
individual sub agent — the manager owns that lifecycle.

Three changes are wanted:

1. Track **sub projects** inside a project instead of creating several top-level
   projects on one repository.
2. Make **sub agents less prominent**: not interesting to interact with
   manually, and not something to archive by hand.
3. Let the **task delegation flow auto-archive completed sub agents**.

## Proposed model

> Superseded: sub projects are implemented as their own project plus a parent
> link (see *A sub project is its own project*). The nested-membership model
> below is kept as the record of what was investigated.

### A sub project is a nested project with its own manager

A sub project is a real project (`ProjectRecord`) whose root session is a child
agent session of a parent project. It owns a goal, its own manager agent, and
its own delegated sub agents, and it shares the parent project's attached
repository instead of creating a second top-level project.

- The parent project is the **repository owner**; a sub project never attaches
  its own repository. It works in a branch/worktree derived from the parent's
  project branch, the same isolation rules the current code-task path already
  uses.
- Hierarchy gains a **project nesting level** that is separate from agent
  depth. `MAX_PROJECT_AGENT_DEPTH` (3) still bounds manager → sub agent →
  sub-sub-agent *within* one project. Project nesting is bounded separately
  (proposal: two levels, project → sub project) so the tree stays readable.
- A sub project root is both a project root and a member of its parent project:
  parent↔sub-project messages, control, and integration reuse the existing
  child-edge machinery. There is no new adjacency concept.

Naming: **sub project** is the working term. *Workstream* is the friendlier
label for the goal line; *strategy* (from the issue title) is a possible
alternative. The mockup uses “workstream” as the sub project subtitle.

### Domain and storage sketch

- `ProjectRecord` / `ProjectSnapshot` gain `parent_project_id:
  Option<ProjectId>` and the linking `parent_session_id`.
- A sub project's `root_session_id` remains the project ID, so existing
  snapshot-by-session lookup keeps working; a session now resolves to the
  innermost project that claims it plus its ancestor project chain.
- Repository/attachment resolution walks up to the nearest ancestor that owns a
  repository.
- The concurrency cap, queued-task admission, and retention sweep apply per
  owning project tree, not per nested project, unless we decide otherwise.

## UI model

### Sidebar hierarchy

```
Project            (repo source, descendant count, activity)
└─ Sub project     (goal line, agent summary, activity dot)
   └─ Agents       (one collapsed summary row, expandable)
```

- A project row keeps its name and repo-source second line. It becomes a
  disclosure parent for its sub projects.
- A sub project row shows its goal/workstream line and a compact agent summary.
- Projects with direct agents and no sub projects still render an agent summary
  row directly beneath the project.
- Selecting a sub project opens the same conversation surface as any session;
  selecting the summary row toggles it.

### Creating a sub project

Because a sub project inherits the parent project's repository, worker, model,
and approval policy, creating one should not be a dialog. It is a **single
field in the sidebar**:

- Clicking **＋** on a project row (or the `New sub project` ghost row under an
  empty project, the project context-menu item, or the sidebar header menu)
  turns the row into an inline input.
- Typing a name and pressing **Enter** creates the sub project on
  `subproject/<project>/<name>`, using the project's defaults. The goal is
  editable afterwards; nothing else needs choosing.
- The existing top-level **New project** dialog stays as-is, since a top-level
  project genuinely has to choose a worker and a repository source. A sub
  project has nothing to choose.

### Sub agents de-emphasized

- Sub agents collapse into **one summary row** per project or sub project:
  `4 agents · 2 running · 2 done`. Expanding reveals read-only rows: selecting
  one still opens its transcript for inspection, but the row is visually muted
  and the context menu drops **Archive** (and other lifecycle actions) in favour
  of **Open transcript** and a **Stop** control while it runs.
- Completed agents are grouped under the summary rather than occupying their
  own persistent rows, so a finished delegation leaves no clutter.

## Manager-driven auto-archive

- The delegation tool (`delegate_project_task`) gains an `auto_archive` flag
  (default on for completed work). When a delegated task reaches a terminal
  `Completed` state **and** any code result has been integrated, the backend
  archives the sub-agent session automatically.
- A sub agent that failed, was cancelled, or has un-integrated code is *not*
  auto-archived; it stays visible so the manager or user can recover it.
- The automatic archive is the same durable archive transition as today, so
  retention-based deletion still reclaims it later.
- The behavior is gated by a workspace setting — proposal: **Auto-archive
  completed sub agents: off / immediately / after a delay** — so users who want
  to inspect finished agents can keep them around. The mockup surfaces this in
  the project detail pane.

## Open questions

- Should sub projects carry their own model/provider defaults, or always
  inherit the parent project's?
- Does a sub project add to the parent's delegated-agent concurrency budget, or
  does each nested project get its own cap?
- Should the summary row be per-project only, or also per-directory when one
  project has many agents?
- Is two levels of project nesting enough, or does a sub project need to nest
  further?
- Can a sub project attach an *additional* repository, or is repo sharing
  strictly single-owner?

## Mockup

`docs/project-subprojects-mockup.html` is an interactive, self-contained
mockup. It offers a **Proposed / Current** comparison and scenes for the
sidebar tree, the inline **Add sub project** flow, a project detail pane (with
the auto-archive setting), and the muted sub-agent context menu.
