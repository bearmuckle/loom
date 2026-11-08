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

- `--connect <url>` (or `LOOM_URL`) names the worker WebSocket endpoint. When
  neither names one, `ws://127.0.0.1:8765/ws` is used, so a local worker needs no
  flags.
- `--token-file <path>` (or `LOOM_TOKEN_FILE`) is the only credential path.
  There is deliberately no `--token` flag, so a token never appears on a
  command line, and no diagnostic or output line carries it. When neither names
  a file, the path is derived from the connected URL as
  `<state dir>/<host>_<port>/token`, which is where a standalone worker stores
  the token of the instance bound to that address
  (`<state dir>` honours `LOOM_STATE_DIR`, then `XDG_STATE_HOME`, then
  `$HOME/.local/state`; `ws` defaults to port 80 and `wss` to 443, and the host
  and port map the way the worker's bind key does). A leading `~/` in a named
  path is expanded with `HOME`.
- `--ca-file <pem>` trusts an additional TLS certificate.
- `--allow-insecure-remote` permits plaintext `ws://` to a non-loopback host.
- `--max-frame-bytes <n>` sets the client's frame limit; the default is 4 MiB.
- `--json` prints one JSON envelope for a call instead of text.
- `--help` (or `-h`) prints the global manual, or one verb's page when the line
  names a verb: `loom-cli run --help`, `loom-cli providers list --help`, and
  `loom-cli project worktree cleanup --help` each print only that verb's usage,
  its flags, and a few copy-pasteable examples.

A token file that cannot be read is a usage error (exit `2`) that names the path
that was tried, and, when the path was derived, the URL it was derived for:

```text
error invalid_request could not read the token file \
  '/state/loom/127.0.0.1_8765/token' (default for ws://127.0.0.1:8765/ws): …
```

## Verbs

| Verb | Purpose |
| --- | --- |
| `doctor` | Negotiated protocol, capability intersection, participant, and worker node. |
| `node` | The worker node's id, name, capabilities, and health. |
| `whoami` | The participant bound to the token; `{"kind":"operator"}` under `--json` for an operator token. |
| `ls [--workspace <id>] [--archived]` | Workspaces, or the sessions of one workspace. |
| `get <session-id> [--metadata]` | One session snapshot, or that snapshot with its active run metadata. |
| `events --session <id> [--after <seq>] [--cursor-file <path>] [--follow] [--timeout <seconds>] [--format text\|jsonl] [--max-bytes <n>]` | Session events, followed until the run completes or the caller interrupts; `--timeout` bounds the follow and exits `4` when it elapses. |
| `peek --session <id> [--lines <n>] [--format text\|jsonl] [--max-bytes <n>]` | The tail of the session's current output with a time on every line; 200 lines by default. |
| `run --workspace <id> (--task <text> \| --task-file <path>) [--name <name>] [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]] [--as-directory <path>] [--approval-policy default\|auto] [--request-id <uuid>] [--new] [--project <session-id>] [--child-model <id>] [--child-model-for <kind>=<id>]...` | Reuse or create the session named `--name`, attach the inputs, and start one run. `--new` always creates a session. `--project` makes it a sub project of that project root. Without `--model` the first credentialed provider's default model is started. `--child-model` and `--child-model-for` store the models delegated children use before the run starts. |
| `send --run <id> --text <text>` | Follow-up direction to the active run. |
| `answer --run <id> --text <text>` | Answer the question the run is waiting on. |
| `approve --run <id> --tool-call <id>` | Approve the tool call that blocked the run. |
| `deny --run <id> --tool-call <id> [--reason <text>]` | Deny the blocked tool call. |
| `stop --run <id> [--session <id> --archive] [--retry-on-conflict]` | Interrupt the run, optionally archiving its session afterwards. |
| `wait --session <id> [--until blocked\|terminal] [--timeout <seconds>]` | Poll until the run blocks or reaches a terminal state; 60 seconds by default. |
| `status --session <id> [--format text\|verb] [--tree]` | The session, its latest run, and the tool call that run is executing; `--format verb` prints one declaration line, `--tree` adds the project projection. |
| `liveness --session <id>` | `alive`, `dead`, `gone`, or `unknown`. |
| `result --session <id> [--task <id>]` | Branch, head, diffstat, changed paths, and task evidence. |
| `retention get` | The worker-wide archive-retention policy, with a disabled window as `off`. |
| `retention set [--window <duration>] [--force-discard <bool>] [--sub-agent <duration>]` | Merge the given fields into the stored worker-wide archive-retention policy. |
| `providers list` | Configured providers, and whether each has a credential. No secrets. |
| `models list [--provider <id>]` | The models of the configured providers, one default per provider. No secrets. |
| `providers set-key --provider <id> (--from-env <NAME> \| --key-file <path>)` | Store an API key for `openai`, `deepseek`, or an OpenAI-compatible provider. |
| `delete <session-id> [--force] [--yes] [--request-id <uuid>]` | Permanently delete an archived session, or a project root with its descendants; the same operation as `session delete`. |
| `session archive <session-id> [--yes] [--request-id <uuid>]` | Archive one session, and the live descendants of a project root that cascades; every checkout is kept. |
| `session delete <session-id> [--force] [--yes] [--request-id <uuid>]` | Permanently delete an archived session, or a project root with its descendants. |
| `project get <session-id>` | One project's members, their depth and parent, and the project it was branched from. |
| `project tree <session-id>` | The same hierarchy as an indented pre-order tree. |
| `project models get <session-id>` | The models the project's delegated children run on: the default child model and one model per kind of work. A member session resolves to its project root. |
| `project models set <project-root-id> [--child-model <id> \| --no-child-model] [--child-model-for <kind>=<id>]... [--unset-kind <kind>]... [--clear] [--request-id <uuid>]` | Change only the named child model entries of a project, and print the stored policy. |
| `project tasks <session-id>` | The project's delegated tasks with status, owner, target, model, dependency blocks, and intent. |
| `project children <session-id>` | The direct child agents with their task, run, pending approval, and pending question. |
| `project messages <session-id> [--after <seq>] [--limit <n>]` | One page of the project's agent message history; `--limit` is 1 to 512 and 100 by default. |
| `project send <session-id> (--text <text> \| --text-file <path>) [--request-id <uuid>]` | A direction to a delegated child's active run, with the same rules as `send`. |
| `project child pause\|resume\|interrupt\|cancel <task-id> --session <id> [--request-id <uuid>]` | Control one delegated child task. |
| `project archive <session-id> [--request-id <uuid>]` | Archive a project root with its idle members; the id must name such a root. |
| `project delete <session-id> [--force] [--yes] [--request-id <uuid>]` | Delete a project root and its tree; the id must name such a root. |
| `project worktrees <session-id>` | The checkouts the project's snapshot records, in snapshot order. |
| `project worktree get <session-id> --task <task-id>` | The durable worktree record of one delegated task; the checkout is not opened. |
| `project worktree diff <session-id> --task <task-id>` | The worker's bounded review of one checkout: status, diffstat, changed paths, and hunks. |
| `project worktree integrate <session-id> --task <task-id> [--expected-parent-revision <rev>] [--request-id <uuid>]` | Fast-forward the parent with the task's result revision; a conflict is reported, never retried. |
| `project worktree cleanup <session-id> --task <task-id> (--keep \| --remove \| --discard) [--yes] [--request-id <uuid>]` | Keep or remove the checkout recorded for one delegated task. |

`node`, `whoami`, `ls`, `get`, `events`, `peek`, `retention get`, `providers
list`, `models list`, and every `project` read are read-only, issue no mutation,
and work with a read-only-scoped token. `run` starts a run; `send`,
`project send`, `answer`, `approve`, `deny`, and `stop` control one; `wait`,
`status`, `liveness`, and `result` read one. `retention set`, `providers
set-key`, `session archive`, `session delete` (and its `delete` alias), `project
archive`, `project delete`, `project models set`, `project worktree integrate`,
and `project worktree cleanup` are operator-level mutations. No verb waits for a run except `wait`,
which is always bounded by `--timeout`.

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

`peek` prints `<seq> <time> <kind> <body>`, so a tail answers when as well as
what. `<time>` is RFC 3339 in UTC with milliseconds. The event envelope carries
no timestamp, so an event that records one for itself — an activity, a run
start, a run completion — is named by it, and every other event is named by the
moment this read saw it. `events` keeps `<seq> <kind> <body>`, so a follow stays
comparable byte for byte with an earlier run of the same command.

`events` and `peek` bound the tool payload of each rendered event to 4096 bytes
by default; `--max-bytes` changes that cap.

### `jsonl`

`--format jsonl` writes one JSON object per line that carries `seq` and
`stream_epoch`, so a caller can resume. Each object also carries the rendered
`kind`/`text` and the full `event` payload. A `peek` object also carries `at`,
the same time its text line prints.

### `--json`

`--json` prints one envelope per call:

```json
{"ok":true,"verb":"whoami","request_id":"…","replayed":false,"data":{…}}
```

`replayed` is `true` when the worker answered this call's mutating request
from its idempotency cache instead of acting again, which happens only when the
caller repeats a `--request-id`; a repeated archive, deletion, integration,
cleanup, child control, or child direction is recognised that way. A failure
uses the same envelope with `ok: false`
and an `error` object carrying `code`, `message`, `retryable`, and `exit_code`.
`whoami` and `doctor` carry `participant`: the identity object of the person the
token belongs to, or `{"kind":"operator"}` for an operator token, which is the
same connection the `participant operator` text line reports. `events` adds
`gap { after, before, missing }` to `data` when a snapshot could
not carry every event after the cursor, and `null` otherwise.

`--json` with `--follow` keeps every followed event in that one envelope, so
prefer `--format jsonl` for a long follow.

### Project and worktree event projections

A client that follows a project learns its tree's lifecycle from ordinary
session events, and each one is rendered as its own kind instead of the generic
`event` fallback. Every body names the project and the record it is about, so a
line identifies itself without any out-of-band state:

| Kind | Body after the kind |
| --- | --- |
| `project_child_created` | `project <project-id> child <session-id> parent <session-id-or-dash> depth <depth> state <state>` |
| `project_child_state_changed` | `project <project-id> child <session-id> state <state>` |
| `project_task_updated` | `project <project-id> task <task-id> <status> child <session-id> <child-name>` |
| `project_message` | `project <project-id> message <message-id> <kind> from <session-id> to <session-id> seq <project-sequence> <body>` |
| `project_integration_started` | worktree body |
| `project_integration_completed` | worktree body |
| `project_integration_conflict` | worktree body |
| `project_worktree_cleanup` | worktree body |
| `project_worktree_updated` | worktree body |

The five worktree kinds share one body, which always carries the checkout's
identity and both revisions:

```
project <project-id> worktree <name> <status> task <task-id> <relative-path> base <base-revision> result <result-revision-or-dash> integrated <integrated-revision-or-dash>
```

The kind names the lifecycle step the record's status carries:

| Status | Kind |
| --- | --- |
| `integrating` | `project_integration_started` |
| `integrated` | `project_integration_completed` |
| `conflict` | `project_integration_conflict` |
| `cleanup_pending`, `retained`, `removed` | `project_worktree_cleanup` |
| `creating`, `ready`, `stale`, `recovery_required` | `project_worktree_updated` |

The parts that only some records have come last: `conflicts <comma-joined
paths>` when the record holds conflict paths, `disposition
<disposition-or-dash>` on the cleanup kinds only (spelled `retain`,
`remove_clean`, or `discard_changes`), and `note <error>` when the record
carries one — a forced discard records there what it threw away, so that loss
stays in the stream instead of disappearing with the checkout. `-` stands for
an absent value, and states, statuses, and dispositions use their protocol
snake_case spelling.

`project_child_created`, `project_task_updated`, and the worktree kinds are
journaled on the parent's stream, and `project_message` on the recipient's (and,
for a branch route, the root's). A child state change is journaled as
`project_child_state_changed` beside the task update, so a supervisor that
follows only the parent never has to infer a child's state from its task
status.

These are ordinary session events, so nothing about reading them is special:
the `--after` and `--cursor-file` cursor, a changed `stream_epoch` invalidating
the stored cursor, the `gap` report of a bounded snapshot, and snapshot
resynchronisation all apply unchanged. The `project_message` body is bounded by
the same `--max-bytes` cap as tool output and is always the last field.
`--format jsonl` carries the rendered `kind` and `text` plus the full typed
payload in `event`, so no caller has to parse the text.

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

`--follow --timeout <seconds>` bounds the follow. The cursor file is written
after every rendered batch, so when the bound elapses the last delivered
sequence is already stored: `events` stops and the next run with the same
cursor file continues exactly there. Text and `--format jsonl` stream every
event as it arrives and report the elapsed bound as the `deadline_exceeded`
failure with exit `4`. `--json` buffers the events into one envelope, so a
bounded follow that reaches its deadline reports `ok: true` with
`data.timed_out: true`, the events it delivered before the deadline, and exit
`4`: no delivered event is dropped from the result the cursor points past. The
deadline is retryable for that reason, even though nothing is retried by the
command itself. Without `--timeout` a follow is unbounded, because it still
stops at the run's `run_completed` event, and `--timeout` without `--follow` is
a usage error (exit `2`).

The polling cadence, paging, snapshot replacement, and epoch rules are stated in
[the protocol document](protocol.md#headless-polling-client).

## Starting a run

`run` reuses the workspace session whose name matches `--name` (or a name
derived from the task) and otherwise creates it. It attaches `--repo` or
`--as-directory` only when it created the session, so a replay neither
duplicates an attachment nor fails on one that already exists.

`--as-directory <path>` attaches the folder in place as the session's input and
makes it the agent's working directory: relative tool paths and the working
directory of commands the run starts resolve inside it, so the files the
session writes for that input land in the folder itself rather than in the
worker's private session storage. The folder is also reachable at `--path`
(its final path component by default) inside the session filesystem. A session
that already has the folder attached keeps it as it is, because the input is
only attached when `run` created the session.

Without `--model`, `run` reads the provider catalog with the same
`Provider/ListProviders` request `providers list` and `models list` send, and
starts the first provider, in the order the worker returned it, that has a
credential and a known default model. The selection is written as
`loom:model:v1:["<provider-id>","<model>"]`, so the worker cannot resolve the
model to another provider, and the model is the default `models list` marks for
that provider, so the two verbs never disagree. When no provider has a
credential, or when the worker refuses the catalog read as a missing capability,
`run` prints one warning on stderr and starts `deterministic/demo`; a transport
failure is still a failure, because the worker was never reached. An explicit
`--model` always wins and never reads the catalog.

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

`--child-model <id>` and `--child-model-for <kind>=<id>` (repeatable) store the
models the run's delegated children use on the session's project before the run
starts, so the run's first delegation already follows them. They change only
the entries they name, exactly like `project models set`, and the `--json`
envelope reports the stored policy under `data.child_models`. A model the worker
has not configured stops the command before the run starts.

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

### `run --project`

`--project <session-id>` puts the session in an existing project instead of
making it a project of its own. The id is a project root, the id
`status --tree` prints on its `project` line. The verb lists the workspace and
reads `GetProjectSnapshotForSession` first: an id that is not a live session
of `--workspace` (unknown, archived, or in another workspace) is `not_found`
(exit `5`), and an agent of a project is a usage error that names the root to
pass instead. These checks run with `--new` too, so `--new --project` does list
the workspace.

The session is created with `Project/CreateSubProject`, the request loom-ui's
**New sub project** sends, so loom-ui lists it beneath the project and
`status --session <id> --tree` prints `parent <project-id>`. Without `--new`,
`--name` is reused only among the project's sub projects; a project root of the
same name elsewhere is never picked. A sub project this command created
inherits the project's folders, then the repositories no inherited folder
covers, unless `--repo` or `--as-directory` names its input instead; a source
that cannot be inherited is a warning on stderr. Text output gains a third
line, `parent <project-id>`, and `--json` reports
`data.project = {"id": <session-id>, "parent_id": <project-id>}` (`null`
without `--project`). With `--new --request-id <base>` the creation and every
inherited attachment use ids derived from `<base>`, so the command replays.

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
`awaiting_approval` or `needs_input`, and it does not wait for a person to
decide. The text output names the run, the attempt, and the pending tool call,
and for a pending approval it prints the exact command that unblocks the run:

```
run <run-id> awaiting_approval
blocked <run-id> <attempt-id> <tool-call-id|->
approve loom-cli approve --run <run-id> --tool-call <tool-call-id>
```

The `--json` envelope carries the same values as `data.run_id`,
`data.attempt_id`, `data.tool_call_id`, and `data.approve_command` (null
without a pending approval), so a supervisor never has to reconstruct the run
id from another request. The timeout covers only a run that is still working,
and a run parked on a question or approval is likewise reported at once by
`--until terminal`, because it cannot reach a terminal state without a decision.
`--until terminal` exits `0` for every terminal state, whether the run
`completed`, `failed`, or was `cancelled`; `data.state` and `terminal: true`
distinguish them, so a caller reads the outcome from the envelope instead of
the exit code.

`status --format verb` prints the declaration grammar the rest of the ecosystem
uses: `working: …`, `blocked: …`, `paused: …`, `done: …`, or `failed: …`, each
followed by the run state and the run id. The word is derived by this client
from the run state; no worker writes it. A session with no run is
`done: idle -`.

### The running tool call

`status` names the tool call the run is executing right now, under the `run`
line:

```
tool <tool-name> <tool-call-id> <elapsed> <arguments>
```

`<elapsed>` is how long the call has been running, as `123ms`, `4.5s`, or
`2m 5s`. `<arguments>` is the call's argument JSON, escaped the way
`loom-text/1` escapes a body and bounded to 512 bytes with the usual
`[... N bytes omitted ...]` marker. The wire envelope carries no timestamp, so
the duration compares the start the worker recorded with this client's clock;
a skewed clock on either side shows up as skew in the duration.

Credentials never reach the line: the value under a key that names one (`token`,
`secret`, `password`, `passwd`, `credential`, `authorization`, `cookie`, `key`,
case-insensitively) and any string that starts with a credential prefix (`sk-`,
`ghp_`, `github_pat_`, `xoxb-`, `bearer `, and the other published prefixes) is
printed as `[redacted]`. Redaction runs before bounding, so a secret is not
printed even when it sits past the budget.

The line appears only while a call is executing. A call that finished, a call
parked on an approval or a question, and a run whose newest activity is a model
turn print no `tool` line. The `--json` envelope carries the same values as
`data.running_tool` (`name`, `tool_call_id`, `elapsed_ms`, `elapsed`,
`arguments`), or `null`.

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
worktree <worktree-name> <relative-path> <status> <task-id> <base-revision> <result-revision-or-dash> <integrated-revision-or-dash>
conflict <worktree-name> <path>
decision approval <session-id> <run-id> <tool-call-id> <tool-name>
decision question <session-id> <run-id> <question-text>
```

`parent` appears only for a sub project, `conflict` appears after the worktree
it belongs to, and `-` stands for an absent task summary, result revision, or
integrated revision. The `worktree` line is the one
[`project worktrees`](#worktree-inspection-and-integration) line, so a record
reads the same in both verbs and its task id joins it to a `task` line without
`--json`. The order is the project, its parent and root, then the
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
`project send` is not a read: it also sends
`Session/GetAgentSessionSnapshotMetadata`, `Collaboration/GetRunRevision`, and
`Collaboration/DirectAgent`, and it is described below.

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
`{"project_id", "session_id", "messages", "next_after_project_sequence"}`,
`project send` carries `{"action", "run", "revision"}` like `send`, and
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

`--request-id <uuid>` on `project child` keys the control request alone, so a
repeat is answered from the worker's durable cache with the same `data` and
`replayed: true` instead of controlling the child twice. The snapshot pre-read
always keeps a fresh id, and there is no retry, so one key never names a second
request.

Message history is read-only here, and there is deliberately no verb that sends
an agent-attributed message: the worker refuses
`Project/SendProjectAgentMessage` for every client connection, because that
channel is reserved for the server-bound agent tool. A person directs a live
child instead, with `project send`.

### Child models

`--model` chooses the model of the run you start. The agents that run delegates
to start their own runs, and a project stores which models those children use:
one optional default child model and one model per kind of work. A kind is a
short label of `a-z`, `0-9`, `_`, or `-` (at most 32 characters, 16 kinds per
project) such as `cheap`, `coding`, `review`, `vision`, or `long-reasoning`.
Kinds are free-form because what counts as a kind of work differs per project
and per provider line-up; the delegation tools list the kinds a project
defines, with their models, so an agent does not have to guess them.

A delegating agent passes an optional `kind` and an optional `model_id`. The
child's model is, in order: the `model_id` the agent named, the model the
project maps the `kind` to, the project's default child model, and last the
delegating agent's own model. `current`, like an omitted `model_id`, leaves the
choice to the project, so a policy is not bypassed by an agent that habitually
passes the alias; an agent that wants its own model despite the policy names
it. An unknown kind falls through to the default. The delegation result reports
`model_id`, `model_source` (`explicit`, `kind`, `project_default`, or
`parent`), and the normalized `kind`.

`project models get <session-id>` resolves the id to the project root first,
then sends `Project/GetProjectChildModels` with that root, so any member
session addresses the same policy; a session the worker does not know fails
instead of reporting the policy of a made-up project. The verb negotiates
`read_project`. `project models set
<project-root-id>` reads the stored policy, applies only the entries its flags
name (`--clear` starts from an empty policy), and sends
`Project/SetProjectChildModels`, negotiating `control_agent_session`; the id
must name the project root, and the change needs the owner role. The worker
trims the model ids, lower-cases the kinds, and refuses a model it has not
configured (`loom-cli models list` shows them) before it changes anything.
`--request-id` keys the replacement. The text output is the project line then
the policy, which `project get` and `status --tree` also print after their own
lines:

```
project <project-id>
child-model default <model-id|->
child-model kind <kind> <model-id>
```

Under `--json` both verbs carry `{"project_id", "child_models":
{"default_model_id", "by_kind"}}`; `project get` adds `child_models` to the
snapshot and `status --tree` adds it to `data.project`, `null` when the worker
does not report it. The policy is stored on the project root and removed with
it; a change applies to the next delegation, also of a run that is already
going.

### `project send`

`project send <session-id> (--text <text> | --text-file <path>)` directs a
delegated child's active run. It is deliberately not
`Project/SendProjectAgentMessage` and it stores no durable project message: it
reads `Project/GetProjectSnapshotForSession` and requires the session to be the
target of a delegated task in it, otherwise it is a usage error naming
`project children <root>`. It then reads the child's
`Session/GetAgentSessionSnapshotMetadata` for the active run and reaches that
run exactly as `send --run` does: `Collaboration/GetRunRevision` followed by
`Collaboration/DirectAgent`, one retry after a `Conflict`, and the same
`sent <run-id> <state>` text and `{"action", "run", "revision"}` payload.

`--text-file` reads UTF-8 and drops one trailing line ending, like
`run --task-file`, so a file written with a trailing newline carries the same
text as the `--text` value. A child whose session reports no active run is
`invalid_state` (exit `5`) with that metadata request id, and no direction is
sent. The verb negotiates `read_project`, `read_agent_session`, `read_agent_run`,
and `control_agent_run`.

`--request-id <uuid>` keys the `Collaboration/DirectAgent` request, so a repeat
is answered from the worker's durable cache with the same `data` and
`replayed: true`; the snapshot, metadata, and revision reads always keep fresh
ids.

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

## Providers

`providers list` prints one line per provider the worker has registered, with
labeled fields:

```text
id=openai kind=open_ai credential=false api-key-configurable=true
id=deepseek kind=deepseek credential=false api-key-configurable=true
id=openai-compatible kind=open_ai_compatible credential=true api-key-configurable=true
```

`credential` is whether a credential reference is stored.
`api-key-configurable` is whether `providers set-key` can attach a key. The line
never includes the secret or the credential reference. `--json` is
`{"providers":[{"id","kind","display_name","has_credential","api_key_configurable"}]}`.
A caller that needs a field without parsing this line should use `--json`.

### `models list`

`models list` reads the same `Provider/ListProviders` response and reports every
model the configured providers expose, so it needs `list_providers` as well and
works with a read-only-scoped token. The text line is:

```text
provider=<provider-id> model=<model-id> default=<true|false>
```

```text
provider=deepseek model=deepseek-flash default=true
provider=deepseek model=deepseek-test default=false
provider=openai model=gpt-6-luna default=true
```

`--provider <id>` keeps only that exact provider id; an id the worker does not
have is an empty list with exit `0`, not an error. `--json` is
`{"provider":<filter-or-null>,"models":[{"provider","id","display_name","default","context_window","max_output_tokens"}]}`,
where an undeclared window or output limit is `null`. Only the model catalog is
reported: no secret, credential reference, or health record is part of the
answer.

Exactly one model per provider is marked `default`. The provider kind's declared
default wins when that model is present (`deepseek-flash` for `deepseek`,
`gpt-6-luna` for `openai`, `openai-compatible`, and `github-copilot`); otherwise
the first model in sorted order is marked. The models are sorted by provider id
and then by model id, and a provider that declares no default
(`deterministic`, `ollama`) marks its first model, so two models are never
marked.

`providers set-key --provider <id>` stores a key for an API-key provider
(`openai`, `deepseek`, or an OpenAI-compatible provider) by sending
`Provider/ConfigureApiKeyProvider`, the same request Settings → Providers sends.
The key comes from exactly one of:

```sh
loom-cli providers set-key --provider openai --from-env OPENAI_API_KEY
loom-cli providers set-key --provider openai --key-file ~/.config/loom/openai.key
```

`--from-env` names a variable; it does not take the key. `--key-file` is read as
UTF-8 and one trailing newline is dropped, then surrounding whitespace is
trimmed. `--key` is rejected. A missing variable, an unreadable file, and a
blank key exit `2` and send no request. The command prints
`provider <id> credential set` and nothing else. The worker stores the secret in
its instance credential store, next to the state database, and the provider
record keeps only an opaque reference. `github-copilot` is not an API-key
provider; `set-key` refuses it. A provider id the worker does not have is
`not_found` (exit `5`).

`providers list` and `models list` need `list_providers`. `providers set-key`
needs `configure_providers`; a token without it exits `3`.

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
worktree <worktree-name> <relative-path> <status> <task-id> <base-revision> <result-revision-or-dash> <integrated-revision-or-dash>
```

The `worktree` line is the same one `status --tree` prints, so a supervisor can
read a record the same way in either verb; the name and path lead, so the
`worktree <name> <relative-path>` header of `project worktree get` and
`project worktree diff` is a prefix of it. `worktrees 0` states an empty project
explicitly. `--json` reports `session_id`, `project_id`, `root_session_id`, and
`worktrees`, each entry being the protocol record. The verb needs the
`read_project` capability.

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

# See which providers have a credential, then store an OpenAI key from the environment.
loom-cli providers list
loom-cli providers set-key --provider openai --from-env OPENAI_API_KEY

# List workspaces, then the sessions of one.
loom-cli ls
loom-cli ls --workspace <workspace-id>

# Follow a session from a persisted cursor, bounded to five minutes.
loom-cli events --session <session-id> --cursor-file .loom-cursor --follow \
  --timeout 300

# Read the tail of a session's output.
loom-cli peek --session <session-id> --lines 40

# Start one run from a task file and print its ids.
loom-cli run --workspace <workspace-id> --name release-check \
  --repo https://github.com/acme/widgets.git --task-file task.md \
  --request-id 0199f0f4-0000-7000-8000-000000000000

# Start a run on the worker's first credentialed provider and its default model.
loom-cli run --workspace <workspace-id> --task 'tidy the fixtures'

# Choose the model explicitly instead, and ask for the JSON envelope.
loom-cli run --workspace <workspace-id> --task 'tidy the fixtures' \
  --model 'loom:model:v1:["deepseek","deepseek-flash"]' --json

# Run the manager on one model, its children on a cheap default, and screenshots
# on a vision model; then inspect and change the project's child models.
loom-cli run --workspace <workspace-id> --task 'triage the bug reports' \
  --model deepseek-flash --child-model deepseek-flash \
  --child-model-for vision=<vision-model-id>
loom-cli project models get <project-root-id>
loom-cli project models set <project-root-id> --child-model-for review=<model-id> --json

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

# Direct a delegated child's active run, and key the repeat of the direction.
loom-cli project send <child-session-id> --text "prefer the smaller diff" \
  --request-id 0199f0f4-0000-7000-8000-000000000004
```
