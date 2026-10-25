# Headless client (`loom-cli`)

`loom-cli` is the headless binary of the `crates/loom-cli` package and a pure
client of the versioned protocol: it embeds no backend, opens no SQLite
database, and manages no worker, and the package does not depend on the
backend crates. The `loom` demo shell lives in the separate `loom-shell`
package:

```sh
cargo run -p loom-cli --bin loom-cli -- doctor
```

The examples below use the installed binary name. In a checkout, replace
`loom-cli` with `cargo run -p loom-cli --bin loom-cli --`.

## Connecting

- `--connect <url>` (or `LOOM_URL`) names the worker WebSocket endpoint.
- `--token-file <path>` (or `LOOM_TOKEN_FILE`) is the only credential path.
  There is deliberately no `--token` flag, so a token never appears on a
  command line, and no diagnostic or output line carries it.
- `--ca-file <pem>` trusts an additional TLS certificate.
- `--allow-insecure-remote` permits plaintext `ws://` to a non-loopback host.
- `--max-frame-bytes <n>` sets the client's frame limit; the default is 4 MiB.
- `--json` prints one JSON envelope for a call instead of text.

## Verbs

| Verb | Purpose |
| --- | --- |
| `doctor` | Negotiated protocol, capability intersection, participant, and worker node. |
| `node` | The worker node's id, name, capabilities, and health. |
| `whoami` | The participant bound to the token. |
| `ls [--workspace <id>] [--archived]` | Workspaces, or the sessions of one workspace. |
| `get <session-id> [--metadata]` | One session snapshot, or that snapshot with its active run metadata. |
| `events --session <id> [--after <seq>] [--cursor-file <path>] [--follow] [--format text\|jsonl] [--max-bytes <n>]` | Session events, followed until the run completes or the caller interrupts. |
| `peek --session <id> [--lines <n>] [--format text\|jsonl] [--max-bytes <n>]` | The tail of the session's current output; 200 lines by default. |
| `run --workspace <id> (--task <text> \| --task-file <path>) [--name <name>] [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]] [--as-directory <path>] [--approval-policy default\|auto] [--request-id <uuid>] [--new]` | Reuse or create the session named `--name`, attach the inputs, and start one run. `--new` always creates a session. |
| `send --run <id> --text <text>` | Follow-up direction to the active run. |
| `answer --run <id> --text <text>` | Answer the question the run is waiting on. |
| `approve --run <id> --tool-call <id>` | Approve the tool call that blocked the run. |
| `deny --run <id> --tool-call <id> [--reason <text>]` | Deny the blocked tool call. |
| `stop --run <id> [--session <id> --archive] [--retry-on-conflict]` | Interrupt the run, optionally archiving its session afterwards. |
| `wait --session <id> [--until blocked\|terminal] [--timeout <seconds>]` | Poll until the run blocks or reaches a terminal state; 60 seconds by default. |
| `status --session <id> [--format text\|verb] [--tree]` | The session and its latest run; `--format verb` prints one declaration line, `--tree` adds the project projection. |
| `liveness --session <id>` | `alive`, `dead`, `gone`, or `unknown`. |
| `result --session <id> [--task <id>]` | Branch, head, diffstat, changed paths, and task evidence. |
| `retention get` | The worker-wide archive-retention policy, with a disabled window as `off`. |
| `retention set [--window <duration>] [--force-discard <bool>] [--sub-agent <duration>]` | Merge the given fields into the stored worker-wide archive-retention policy. |
| `delete <session-id> [--force] [--yes] [--request-id <uuid>]` | Permanently delete an archived session, or a project root with its descendants; the same operation as `session delete`. |
| `session archive <session-id> [--yes] [--request-id <uuid>]` | Archive one session, and the live descendants of a project root that cascades; every checkout is kept. |
| `session delete <session-id> [--force] [--yes] [--request-id <uuid>]` | Permanently delete an archived session, or a project root with its descendants. |
| `project get <session-id>` | One project's members, their depth and parent, and the project it was branched from. |
| `project tree <session-id>` | The same hierarchy as an indented pre-order tree. |
| `project tasks <session-id>` | The project's delegated tasks with status, owner, target, model, dependency blocks, and intent. |
| `project children <session-id>` | The direct child agents with their task, run, pending approval, and pending question. |
| `project messages <session-id> [--after <seq>] [--limit <n>]` | One page of the project's agent message history; `--limit` is 1 to 512 and 100 by default. |
| `project child pause\|resume\|interrupt\|cancel <task-id> --session <id>` | Control one delegated child task. |
| `project archive <session-id> [--request-id <uuid>]` | Archive a project root with its idle members; the id must name such a root. |
| `project delete <session-id> [--force] [--yes] [--request-id <uuid>]` | Delete a project root and its tree; the id must name such a root. |
| `project worktrees <session-id>` | The checkouts the project's snapshot records, in snapshot order. |
| `project worktree get <session-id> --task <task-id>` | The durable worktree record of one delegated task; the checkout is not opened. |
| `project worktree diff <session-id> --task <task-id>` | The worker's bounded review of one checkout: status, diffstat, changed paths, and hunks. |
| `project worktree integrate <session-id> --task <task-id> [--expected-parent-revision <rev>] [--request-id <uuid>]` | Fast-forward the parent with the task's result revision; a conflict is reported, never retried. |
| `project worktree cleanup <session-id> --task <task-id> (--keep \| --remove \| --discard) [--yes] [--request-id <uuid>]` | Keep or remove the checkout recorded for one delegated task. |

`node`, `whoami`, `ls`, `get`, `events`, `peek`, `retention get`, and every
`project` read are read-only, issue no mutation, and work with a read-only-scoped
token. `run` starts a run; `send`, `answer`, `approve`, `deny`, and `stop`
control one; `wait`, `status`, `liveness`, and `result` read one.
`retention set`, `session archive`, `session delete` (and its `delete` alias),
`project archive`, `project delete`, `project worktree integrate`, and
`project worktree cleanup` are operator-level mutations. No verb waits for a run
except `wait`, which is always bounded by `--timeout`.

Every verb negotiates only the capabilities its own requests need. `doctor`
reports the intersection the worker granted, so its capability list is the
smallest set for that verb rather than a client-wide catalogue.

## Output

### `text`

`--format text` is the default and is the documented, byte-stable `loom-text/1`
projection: one line per event as `<seq> <kind> <body>`, with the body escaped
so it never contains a raw newline, both ends of oversized tool output kept with
an exact `[... N bytes omitted ...]` marker, and explicit `pending_approval` and
`question` markers for the states that block a run. The same journal plus the
same cursor produces identical bytes.

`events` and `peek` bound the tool payload of each rendered event to 4096 bytes
by default; `--max-bytes` changes that cap.

### `jsonl`

`--format jsonl` writes one JSON object per line that carries `seq` and
`stream_epoch`, so a caller can resume. Each object also carries the rendered
`kind`/`text` and the full `event` payload.

### `--json`

`--json` prints one envelope per call:

```json
{"ok":true,"verb":"whoami","request_id":"…","replayed":false,"data":{…}}
```

`replayed` is `true` when the worker answered this call's mutating request
from its idempotency cache instead of acting again, which happens only when the
caller repeats a `--request-id`; a repeated archive, deletion, integration, or
cleanup is recognised that way. A failure uses the same envelope with `ok: false`
and an `error` object carrying `code`, `message`, `retryable`, and `exit_code`.
`events` adds `gap { after, before, missing }` to `data` when a snapshot could
not carry every event after the cursor, and `null` otherwise.

`--json` with `--follow` keeps every followed event in that one envelope, so
prefer `--format jsonl` for a long follow.

## Resume and the cursor file

`events` starts from `--after <seq>` when it is given, otherwise from the local
cursor file when `--cursor-file <path>` is used, otherwise from the current
retained window. The cursor records the URL, the session, the last rendered
sequence, and the backend epoch as JSON, and is replaced atomically (a unique
temporary file and a rename) after each rendered batch. A cursor written for
another URL or session is ignored.

A `SessionEventsSnapshot` cannot page: it carries only the newest part of the
retained window. When the cursor sits below the first sequence that window
carries, the events in between are gone from the response and cannot be fetched
again by this verb. `events` prints the window, reports the missing range as
`warning gap <first>..<last>` on stderr and as a `gap` field in its `--json`
envelope, and resumes at the head of the window rather than claiming a complete
suffix. A first read without a cursor has no gap; starting from the current
window is its documented behaviour.

A changed `stream_epoch` invalidates the stored cursor, because the sequence
numbers no longer name the same events. The client then treats the response in
hand as a snapshot.

The polling cadence, paging, snapshot replacement, and epoch rules are stated in
[the protocol document](protocol.md#headless-polling-client).

## Starting a run

`run` reuses the workspace session whose name matches `--name` (or a name
derived from the task) and otherwise creates it. It attaches `--repo` or
`--as-directory` only when it created the session, so a replay neither
duplicates an attachment nor fails on one that already exists.

The approval policy is set before the run starts, because a run resolves its
policy once when it is created. A created session is pinned to
`--approval-policy` (`default` when the flag is absent); a reused session is
only changed when the flag is given, so a plain run never overwrites a policy a
person set.

`--request-id` is the idempotency key of the run start, so repeating the same
command returns the same run instead of starting a second one, and the `--json`
envelope reports `replayed: true` when the answer came from that cache. A
worker accepts a run only in an idle session, so reusing a session that already
ran reports `invalid_state` (exit `5`) and creates nothing.

### `run --new`

`--new` creates a session unconditionally. It never lists the workspace and
never matches a session by name, so a retrigger after a completed run does not
reach `invalid_state`. A session name is a label, not an identity: `--new` may
produce a second session with an existing name, and the printed session id and
the `created` flag are the identity. Pass a unique `--name` when the name is
meant to be a handle.

A `--new` command with `--request-id <base>` derives every mutation from that
base so the whole command is replayable: `<base>` starts the run and the
session creation and the input attachments use deterministic ids derived from
it. Repeating the command returns the same session and the same run, with
exactly one of each. The `--json` envelope reports both ids under
`data.request_ids` (`session` and `run`).

## Supervising a run

The control verbs act on a run id, and the tool call id they need comes from
the `pending_approval` event or from `wait`/`status`. They read
`GetRunRevision` first and send that revision with the request.

- `send`, `answer`, `approve`, and `deny` re-read once and retry when the
  worker answers `Conflict`, because their intent is idempotent. A conflict
  that survives the retry is reported with the protocol's `conflict` code
  (exit `1`).
- `stop` refuses a conflict by default, so a stale revision cannot interrupt a
  run whose state the caller has not seen. `--retry-on-conflict` allows the
  same single retry. A run that already finished is `invalid_state` (exit `5`)
  and nothing is archived.
- `stop --archive` archives the session only after the interrupt succeeded,
  and prints `archived <session-id>` after `stopped <run-id> <state>`.

`wait --until blocked` returns exit `3` as soon as the run is
`awaiting_approval` or `needs_input`, with the attempt id and, for an approval,
the tool call id in the `--json` envelope. It does not wait for a person to
decide. The timeout covers only a run that is still working, and a run parked
on a question or approval is likewise reported at once by `--until terminal`,
because it cannot reach a terminal state without a decision. `--until terminal`
exits `0` for `completed` and non-zero for `failed` and `cancelled`; the
envelope's `state` distinguishes them.

`status --format verb` prints the declaration grammar the rest of the ecosystem
uses: `working: …`, `blocked: …`, `paused: …`, `done: …`, or `failed: …`, each
followed by the run state and the run id. The word is derived by this client
from the run state; no worker writes it. A session with no run is
`done: idle -`.

### `status --tree`

`status --tree` returns the whole picture a supervisor needs from one command:
the requested session and its run as above, then the project it belongs to. It
reads `Project/GetProjectSnapshotForSession` and, for every project agent whose
record is `awaiting_approval` or `needs_input`, one
`Session/GetAgentSessionSnapshotMetadata` to learn which approval or question
that agent waits on. The requested session reuses the projection the verb
already read, so its own pending decision appears once, as a `decision` line,
instead of the plain `pending` line.

One line per record, fields separated by spaces, free text escaped the way the
`loom-text/1` projection escapes it:

```
project <project-id> <verdict>
parent <project-id>
root <session-id>
agent <session-id> <depth> <state> <verdict> <task-summary-or-dash>
task <task-id> <status> <child-name>
worktree <worktree-name> <status> <relative-path> <base-revision> <result-revision-or-dash> <integrated-revision-or-dash>
conflict <worktree-name> <path>
decision approval <session-id> <tool-call-id> <tool-name>
decision question <session-id> <question-text>
```

`parent` appears only for a sub project, `conflict` appears after the worktree
it belongs to, and `-` stands for an absent task summary, result revision, or
integrated revision. The order is the project, its parent and root, then the
agents, tasks, and worktrees in snapshot order, then the decisions in agent
order; a requested session the snapshot does not list comes last.

A per-agent verdict is derived from the session state: `done` for idle and
completed, `working` for queued, planning, executing, and evaluating,
`blocked` for awaiting approval and needs input, `paused`, `failed` for failed
and cancelled, and `archived`. The exact state word is still printed, so a
cancelled agent stays distinguishable from a failed one. The project verdict is
the first matching word of `blocked`, `paused`, `working`, `failed`, `done`,
and `archived`; `archived` therefore only wins when every agent is archived, and
a project with no agent is `done`. No worker writes these words: they are
derived by this client from the records the worker returns.

`--json` carries the same projection under `data.project` — `id`,
`root_session_id`, `parent_project_id`, `verdict`, `counts` (`agents`, `tasks`,
`worktrees`, `decisions`), `agents`, `tasks`, `worktrees`, and `decisions` —
with the protocol records serialized as they are and the derived `verdict`
added to each agent. A decision carries `session_id`, `run_id`, `run_state`,
`kind`, and either `tool_call_id` and `tool_name` or `question`, all taken from
the session projection rather than from the project record. The top-level
`session`, `run`, `state`, `verdict`, `declaration`, and `pending_tool_call`
keys stay as they are without `--tree`.

`--tree` reports a project, so it refuses `--format verb` with a usage error
(exit `2`) before any connection is made, and it negotiates `read_project` in
addition to `read_agent_session`.

`liveness` never guesses. A session that is archived or in a terminal session
state is `dead` (exit `0`), an id the worker does not know is `gone` (exit
`5`), and a transport failure is `unknown` (exit `7`), never `gone`.

`result` reports each attached repository's branch, head, diffstat (files,
added, removed), and changed paths, plus the evidence links of `--task` when it
is given. A session with no repository and no evidence prints
`nothing to report` and sets `empty` in the `--json` envelope.

## Projects and child agents

Every `project` verb is addressed by the id of any project member session: a
project root's session id is also its project id, and the worker resolves a
child's id to the project it belongs to. The same command therefore works from
the root and from a child.

The reads send `Project/GetProjectSnapshotForSession`, which is the worker's
authoritative projection of the hierarchy and its tasks, and they negotiate
`read_project`; `project children` also sends
`Session/GetAgentSessionSnapshotMetadata` per direct child and negotiates
`read_agent_session`, and `project messages` also pages
`Project/ListProjectAgentMessages` under `read_project_agent_messages`.

The text output is byte-stable, one line per record. The task intent, the child
task summary, the child pending question, and the message bodies are bounded by
the same 4096-byte budget before they are escaped, so an over-budget value keeps
its first and last halves around an exact `[... N bytes omitted ...]` marker:

```
project <project-id> root <root-session-id> parent <parent-project-id|->
agent <session-id> <state> depth <n> parent <parent-session-id|->
<indent><session-id> <state> <task-summary|->
task <task-id> <status> owner <session-id> target <session-id> model <id> code <true|false> blocked-by <dep,dep|-> intent <text>
child <session-id> task <task-id|-> <status|-> session <state> run <run-id|-> <state|-> approval <tool-call-id|-> question <text|->
message <project-sequence> <kind> from <session-id> to <session-id> task <task-id|-> <body>
next <project-sequence>
```

`get` prints the header and one `agent` line per member in snapshot order.
`tree` prints the same header and walks from the root following
`parent_session_id`, indenting two spaces per depth below the root, with
siblings in snapshot order; a walk that would miss a member falls back to
snapshot order so every member is printed once. `tasks` reports
`blocked-by` as the dependencies that are still unfinished or absent from the
snapshot. `children` lists only the addressed session's direct children.
`messages` prints the returned page in server order and adds `next` only when
the worker set `next_after_project_sequence`.

Under `--json`, `project get` and `project tree` carry the `ProjectSnapshot`
itself, `project tasks` carries `{"project_id", "tasks"}`, `project children`
carries `{"project_id", "children": [{"agent", "task", "run"}], "count"}`
with `task` and `run` null when absent, `project messages` carries
`{"project_id", "session_id", "messages", "next_after_project_sequence"}`, and
`project child` carries `{"action", "task", "run"}`.

`project child` maps the sub-verb onto `Project/ControlProjectChild`: `pause` is
`Pause`, `resume` is `Continue`, `interrupt` is `Interrupt`, and `cancel` is
`Cancel`. The manager session id is never taken from a flag: the command reads
the project snapshot and sends the `requester_session_id` of the task it names,
because only the server-owned task record says which manager run owns the
child. A task id that no task in that project names is reported as `not_found`
(exit `5`) from the snapshot, without a control request; an unknown session is
also `not_found` (exit `5`), a denied capability is exit `3`, and a worker's
`invalid_state` refusal is exit `5`.

Message history is read-only here, and there is deliberately no send verb: an
agent-attributed message is reserved for the server-bound agent tool, so a
client cannot write one on an agent's behalf. To redirect a live child, a person
sends the direction to that child's run with `send --run`.

## Worker status and retention

`node` reports the worker node's id, name, online flag, capabilities, and the
health its status snapshot carries, one line per field: `node_id`, `name`,
`online`, `capabilities`, `cpu_count`, `cpu_usage_percent`,
`memory_usage_percent`, `memory_total_bytes`, `memory_available_bytes`,
`disk_total_bytes`, and `disk_available_bytes`. A sample the worker did not
carry is `unknown`. `--json` carries the `WorkerNodeStatus` object.

`retention get` prints the policy the worker stored as `window`,
`force-discard`, and `sub-agent`, with a disabled window as `off`. `retention
set` merges only the flags it was given into that stored policy, so `retention
set --window 14d` changes the window and leaves the other two fields alone; at
least one of `--window`, `--force-discard`, or `--sub-agent` is required. The
duration syntax is the server's: `<n><unit>` with `ms`, `s`, `m`, `h`, `d`, or
`w`, a bare number of seconds, or `off`/`never`. `--force-discard` takes
`1`/`true`/`yes` or `0`/`false`/`no`.

The command stores the policy durably and prints `stored for this worker; the
next sweep applies it`: it never deletes anything itself. The policy is one
worker-wide default, not a per-session setting, so a session that is already
archived is subject to the next sweep rather than deleted by the command, and a
session archived later is subject to a later one. A worker restarted on the
same state database without flags keeps the stored policy. `--json` reports the
stored policy plus `applies_from: "next_sweep"`.

`node` and `retention get` are reads. `retention set` needs
`configure_archive_retention`; a token without it exits `3`.

## Session and project lifecycle

Archiving, deletion, and worktree cleanup are separate operations, and every
lifecycle command reads `Project/GetProjectSnapshotForSession` before it mutates
anything, because an id alone does not say whether it names a project root whose
operation reaches other sessions.

### Archive

`session archive <session-id>` moves a session to the archived state and keeps
its transcript, checkpoints, files, and every checkout. It prints
`archived <session-id>` for the session and for each live member the worker also
archives, then `retained worktree <name> <path>` for every checkout in that set,
or `retained 0 worktrees` when there is none. `--json` reports
`archived_sessions`, `retained_worktrees`, and `cascade`.

The worker cascades an archive when the id names a project root that owns a live
member or a delegated task. `session archive` accepts that only with `--yes`,
which names the number of sessions the archive would reach; without it the
command exits `2` after the read and before the archive request. The worker
refuses to archive a root while a child task is unfinished or a child session is
active, and refuses to archive a session that is already archived, as
`invalid_state` (exit `5`). `session archive` needs `control_agent_session`.

`project archive <session-id>` is the same request with the scope stated by the
verb: the id must name such a root, otherwise it exits `2` and points at
`session archive`. It needs no `--yes`, because the command already names the
tree.

Archiving keeps the record and its worktrees, so it is not a deletion and frees
no checkout. This client exposes no restore verb; the protocol's
`Session/RestoreAgentSession` is the only path back.

`--request-id <uuid>` keys the archive request of `session archive` and
`project archive`, so repeating the command with the same id is answered from
the worker's durable cache instead of archiving twice, and the `--json`
envelope then reports `replayed: true` with the same `data`. The
`Project/GetProjectSnapshotForSession` read before it always keeps a fresh id,
because it is a lookup and not the mutation. Without the flag the archive uses
a fresh id, so it is a new operation.

### Deletion

`session delete <session-id>` (and `delete`, its original name) permanently
removes an archived session. There is no interactive prompt in a headless
client, so the id is the confirmation. A project root cascades to its whole
tree, and `--yes` is required whenever that would remove more than one session;
`--yes` is also required with `--force`. Without the confirmation the command
exits `2` after the read and before the delete request, so nothing is removed.
`--force` carries the protocol's meaning: it discards dirty or locked linked
worktrees that would otherwise refuse the deletion, and the command reports each
worktree it discarded as `discarded worktree <name> <path>` (`discarded 0
worktrees` when there was nothing to discard). `--json` reports
`deleted_sessions` and `discarded_worktrees`.

`project delete <session-id> [--force] --yes` is the same request with the scope
stated by the verb: the id must name such a root, otherwise it exits `2` and
points at `session delete`, and `--yes` is always required because deleting a
project is irreversible.

A session that is not archived and is not a project root is refused by the
worker as `invalid_state` (exit `5`) and nothing changes; the worker also
cancels the live work of a project tree before removing it. Both spellings need
the `delete_agent_session` capability, and a read-only-scoped token is refused
with exit `3`.

`--request-id <uuid>` keys the deletion of `session delete`, `project delete`,
and `delete`. Repeating the command with the same id is answered from the
worker's durable cache with the same `data` and `replayed: true`, even though
the session is gone and cannot be deleted again. The snapshot read before it
always keeps a fresh id, because it is a lookup and not the deletion. Without
the flag the deletion uses a fresh id, so it is a new operation.

### Worktree cleanup

`project worktree cleanup <session-id> --task <task-id> --keep|--remove|--discard`
acts on the checkout recorded for one delegated task of the project that
`<session-id>` belongs to. The command reads that record from the snapshot, so
the project id and the manager session come from the worker rather than the
command line. Exactly one disposition is required:

- `--keep` retains the checkout and records it as retained.
- `--remove` removes it. The worker refuses a checkout that still holds changes,
  so nothing is thrown away by a removal that did not ask for it.
- `--discard` removes it together with its uncommitted changes, so it also
  requires `--yes`. The command prints the worker's note about what was
  discarded as `note <text>`, because that loss has nowhere else to be seen.

The text is `retained worktree <name> <path>`, `removed worktree <name> <path>`,
or `worktree <name> <path> <status>` when the record ends in another state, and
`--json` carries `disposition` plus the full worktree record. A task without a
recorded worktree exits `2` after the read, and the worker refuses a task that is
not terminal as `invalid_state` (exit `5`). The command needs the
`cleanup_project_child_worktree` capability.

`--request-id <uuid>` keys the cleanup request, so repeating the command with
the same id is answered from the worker's durable cache with the same `data`
and `replayed: true`, which means the checkout was cleaned up once. The
snapshot read before it always keeps a fresh id. Without the flag the cleanup
uses a fresh id, so it is a new operation.

### Worktree inspection and integration

Listing, inspection, review, integration, and cleanup are separate operations
over the same worker-owned records, and each has its own verb. Every one of them
resolves the project id and the manager session from the worktree record in
`Project/GetProjectSnapshotForSession`, so the caller never supplies an id the
worker did not report; a task without a record exits `2` after that read and
before any further request.

`project worktrees <session-id>` lists what the snapshot records, one line per
record in snapshot order:

```text
project <project-id>
worktrees <count>
worktree <task-id> <status> <worktree-name> <relative-path>
```

`worktrees 0` states an empty project explicitly. `--json` reports `session_id`,
`project_id`, `root_session_id`, and `worktrees`, each entry being the protocol
record. The verb needs the `read_project` capability.

`project worktree get <session-id> --task <task-id>` reports the durable record
and never opens the checkout, so it answers from the snapshot alone:

```text
worktree <name> <relative-path>
task <task-id>
project <project-id>
owner <parent-session-id>
child <child-session-id>
parent-repository <repository-id>
child-repository <repository-id>
branch <branch-name>
parent-revision <base-revision>
child-revision <result-revision or ->
integrated-revision <integrated-revision or ->
status <status>
conflict <path>
cleanup <disposition or ->
note <error text>
```

There is one `conflict` line per recorded conflict path and none when the list
is empty; `note` appears only when the record carries an error. An absent value
is `-`, and the disposition words are the protocol's snake_case spellings:
`retain`, `remove_clean`, `discard_changes`. `--json` reports `session_id`,
`project_id`, `task_id`, and the record as `worktree`. The verb needs
`read_project`.

`project worktree diff <session-id> --task <task-id>` prints the worker's own
review, which is already bounded by the worker's byte limit, so the client never
re-bounds it and never runs Git itself:

```text
worktree <name> <relative-path>
branch <branch or ->
head <head or ->
parent-revision <base-revision>
status <worktree status word>
diffstat <files> <added> <removed>
changed <file-status> <path>
diff binary
diff truncated
diff none
diff <context|added|removed> <line content>
```

`diffstat` counts files as the number of changed paths and added/removed as the
hunk lines of that kind. `diff binary` marks a binary diff, `diff truncated`
marks one the worker's bound cut, and `diff none` states a diff that holds no
hunks and is not binary. One `diff` line follows per hunk line, with its content
escaped, so no line can span two physical lines. `--json` reports the protocol
`worktree`, `status`, and `diff` payloads together with `diffstat` and
`changed_paths` (`path`, `status`, `conflicted`). The verb needs `read_project`
and `read_project_child_review`.

`project worktree integrate <session-id> --task <task-id>
[--expected-parent-revision <rev>]` fast-forwards the parent with the task's
result. The expected parent revision is the flag when it is given and the
record's `base_revision` otherwise, because that is the revision the checkout
and the review were created from. The worker keeps the ancestor check and
refuses a rewritten or unrelated parent; the client sends exactly one
integration request and never retries. A `conflict` answer is therefore reported
as the failure it is: exit `1` with the protocol code `conflict`, the worker's
message naming the conflicting paths, and the checkout left untouched. On
success the text is:

```text
integrated worktree <name> <relative-path>
child-revision <result-revision or ->
integrated-revision <integrated-revision or ->
```

`--json` reports `session_id`, `project_id`, `task_id`,
`expected_parent_revision`, and the updated record as `worktree`. The verb needs
`read_project` and `integrate_project_child`.

`--request-id <uuid>` keys the integration request, so repeating the command
with the same id is answered from the worker's durable cache with the same `data`
and `replayed: true`, which means the parent was fast-forwarded once. The
snapshot read before it always keeps a fresh id. Without the flag the
integration uses a fresh id, so it is a new operation.

## Exit codes

| Exit code | Meaning |
| --- | --- |
| `0` | ok |
| `1` | internal error |
| `2` | usage error |
| `3` | blocked by a pending approval or a missing capability |
| `4` | timeout |
| `5` | not found or gone |
| `6` | protocol or auth mismatch |
| `7` | transport unreachable |

A reader that closes stdout, as in `loom-cli events | head`, is a clean stop
with exit `0` rather than an error.

## Examples

```sh
# Check the worker and the negotiated contract.
loom-cli doctor

# List workspaces, then the sessions of one.
loom-cli ls
loom-cli ls --workspace <workspace-id>

# Follow a session from a persisted cursor.
loom-cli events --session <session-id> --cursor-file .loom-cursor --follow

# Read the tail of a session's output.
loom-cli peek --session <session-id> --lines 40

# Start one run from a task file and print its ids.
loom-cli run --workspace <workspace-id> --name release-check \
  --repo https://github.com/acme/widgets.git --task-file task.md \
  --request-id 0199f0f4-0000-7000-8000-000000000000

# Retrigger CI without colliding with the completed session of the same name.
loom-cli run --workspace <workspace-id> --task-file task.md --new \
  --request-id 0199f0f4-0000-7000-8000-000000000001

# Supervise a run: wait for the approval, decide it, wait for the end.
loom-cli wait --session <session-id> --until blocked --timeout 30
loom-cli approve --run <run-id> --tool-call <tool-call-id>
loom-cli wait --session <session-id> --until terminal --timeout 600
loom-cli result --session <session-id>

# Inspect the worker and change its worker-wide retention policy.
loom-cli node
loom-cli retention get
loom-cli retention set --window 14d --force-discard true --sub-agent 10m

# Delete an archived session, then a whole archived project with confirmation.
loom-cli delete <session-id>
loom-cli delete <project-root-id> --yes

# Inspect the checkouts of a project and land one child's result, then tidy it.
loom-cli project worktrees <project-root-id>
loom-cli project worktree get <project-root-id> --task <task-id>
loom-cli project worktree diff <project-root-id> --task <task-id>
loom-cli project worktree integrate <project-root-id> --task <task-id>

# Keep a project's checkouts while archiving it, then tidy one child worktree.
loom-cli project archive <project-root-id>
loom-cli project worktree cleanup <project-root-id> --task <task-id> --keep
loom-cli project worktree cleanup <project-root-id> --task <task-id> --remove

# Retry an archive or a cleanup safely by keying it, so a repeat is a replay.
loom-cli session archive <session-id> \
  --request-id 0199f0f4-0000-7000-8000-000000000002
loom-cli project worktree cleanup <project-root-id> --task <task-id> \
  --remove --request-id 0199f0f4-0000-7000-8000-000000000003
```
