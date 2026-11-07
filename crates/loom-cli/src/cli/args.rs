//! Argument parsing for `loom-cli`.
//!
//! Parsing is a two-phase split so that global flags are genuinely accepted
//! anywhere: the first pass turns the tokens into flags and positional words,
//! knowing only each flag's arity, and the second pass interprets the flags for
//! the verb the first positional word names. Both `--flag value` and
//! `--flag=value` forms reach the same place, and a flag the verb does not use
//! is a usage error rather than something silently ignored.

use std::{
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use loom_core::{AgentSessionId, RequestId, RunId, TaskId, ToolCallId, WorkspaceId};
use loom_model::ModelId;

/// The budget `--max-bytes` defaults to.
pub(crate) const DEFAULT_MAX_BYTES: usize = 4096;
/// The smallest budget `--max-bytes` accepts.
pub(crate) const MIN_MAX_BYTES: usize = 64;
/// How many events `peek` asks for by default.
pub(crate) const DEFAULT_LINES: u32 = 200;
/// The model `run` falls back to when the worker offers no credentialed
/// provider to derive one from.
pub(crate) const DEFAULT_MODEL: &str = "deterministic/demo";
/// The worker URL a command connects to when neither `--connect` nor `LOOM_URL`
/// names one.
pub(crate) const DEFAULT_CONNECT_URL: &str = "ws://127.0.0.1:8765/ws";
/// How many of the task's leading words a derived session name keeps.
const NAME_WORDS: usize = 6;
/// The longest derived session name, in characters.
const NAME_MAX_CHARS: usize = 48;
/// The name `run` uses when the task has no words at all.
const NAME_FALLBACK: &str = "loom-cli run";
/// How long `wait` polls before it reports `deadline_exceeded`.
pub(crate) const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 60;
/// How many project messages `project messages` asks for by default.
pub(crate) const DEFAULT_MESSAGE_LIMIT: u32 = 100;
/// The largest page `project messages --limit` accepts.
pub(crate) const MAX_MESSAGE_LIMIT: u32 = 512;
/// The confidence `jev triage --apply` requires by default before it approves.
pub(crate) const DEFAULT_JEV_THRESHOLD: f64 = 0.9;
/// The largest state `jev ask` sends, which is the endpoint's documented ceiling.
pub(crate) const MAX_JEV_STATE_BYTES: usize = 128 * 1024;
/// The most choice labels one Jev question may offer.
pub(crate) const MAX_JEV_OPTIONS: usize = 255;

pub(crate) const HELP: &str = r#"Usage: loom-cli <verb> [flags]

Verbs:
  doctor                            report the negotiated protocol,
                                    capabilities, participant and worker node
  node                              report the worker node's id, name, and
                                    health
  whoami                            report this connection's participant
  ls [--workspace <id>] [--archived]
                                    list workspaces, or one workspace's sessions
  get <session-id> [--metadata]     show one session, and its active run
  events --session <id> [--after <seq>] [--cursor-file <path>] [--follow]
         [--timeout <seconds>] [--format text|jsonl] [--max-bytes <n>]
                                    print a session's events, optionally
                                    following for new ones; --timeout bounds
                                    the follow and is exit 4 when it elapses
  peek --session <id> [--lines <n>] [--format text|jsonl] [--max-bytes <n>]
                                    print the newest events of a session
  run --workspace <id> (--task <text> | --task-file <path>) [--name <name>]
      [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]]
      [--as-directory <path> [--path <path>]]
      [--approval-policy default|auto] [--request-id <uuid>] [--new]
      [--project <session-id>]
      [--child-model <id>] [--child-model-for <kind>=<id>]...
                                    reuse the workspace's session named
                                    --name, or create it, attach the inputs
                                    to a session this run created, and start
                                    one agent run without waiting for it;
                                    without --model the first credentialed
                                    provider's default model is started;
                                    --project creates the session as a sub
                                    project of that project root;
                                    --child-model and --child-model-for set
                                    the models its delegated children use
  send --run <id> --text <text> [--request-id <uuid>]
                                    direct the active run and print its state
  answer --run <id> --text <text> [--request-id <uuid>]
                                    answer the question the run waits on
  approve --run <id> --tool-call <id> [--request-id <uuid>]
                                    approve the blocked tool call
  deny --run <id> --tool-call <id> [--reason <text>] [--request-id <uuid>]
                                    deny the blocked tool call
  stop --run <id> [--session <id> --archive] [--retry-on-conflict]
       [--request-id <uuid>]
                                    interrupt a run, and optionally archive
                                    its session afterwards
  wait --session <id> [--until blocked|terminal] [--timeout <seconds>]
                                    poll until the run blocks or reaches a
                                    terminal state; always time-bounded, and
                                    a terminal state is exit 0 whatever the
                                    run outcome
  status --session <id> [--format text|verb] [--tree]
                                    report the session and its latest run;
                                    --format verb prints one declaration line
                                    and --tree adds the project projection
  liveness --session <id>           report alive, dead, gone, or unknown
  result --session <id> [--task <id>]
                                    report branch, head, diffstat, changed
                                    paths, and task evidence
  retention get                     report the worker's archive-retention
                                    policy
  retention set [--window <duration>] [--force-discard <bool>]
                [--sub-agent <duration>]
                                    store a worker-wide archive-retention
                                    policy; the next sweep applies it
  providers list                    list configured providers and whether
                                    each has a credential; prints no secrets
  providers add --id <id> --kind openai-compatible --endpoint <url>
                [--model <id>]... [--default-model <id>]
                                    register an OpenAI-compatible provider such
                                    as OpenCode Go or OpenCode Zen; the default
                                    model is stored first
  providers remove --id <id>        remove an OpenAI-compatible provider and
                                    the API key stored for it
  providers discover --provider <id>
                                    replace one provider's models with the
                                    catalog its endpoint's /models returns
  models list [--provider <id>]     list the models of the configured
                                    providers and mark one default per
                                    provider; prints no secrets
  providers set-key --provider <id>
            (--from-env <NAME> | --key-file <path>)
                                    store an API key for openai, deepseek, or
                                    an openai-compatible provider
  jev status [--source opencode|typesafe] [--key-file <path>]
             [--endpoint <url>] [--model <id>]
                                    report whether a Jev key is configured,
                                    where it is read, and which endpoint and
                                    model a call would use; never prints it
  jev ask (--state <text> | --state-file <path>) --question <text>
          (--option <label>[=<when it applies>])... [--source ...]
          [--key-file <path>] [--endpoint <url>] [--model <id>]
                                    ask the decision model one choice
                                    question and print the choice, the
                                    confidence, and the probabilities
  jev triage --session <id> [--apply] [--threshold <0..1>]
             [--request-id <uuid>] [--source ...] [--key-file <path>]
             [--endpoint <url>] [--model <id>]
                                    suggest approve or ask-user for the
                                    approval a session waits on; --apply
                                    approves only a read inside the session
                                    root at or above the threshold
  session archive <session-id> [--yes] [--request-id <uuid>]
                                    archive one session, keeping its
                                    transcript, checkpoints, and every
                                    worktree; --yes is required when the id
                                    is a project root whose archive also
                                    reaches its descendants
  session delete <session-id> [--force] [--yes] [--request-id <uuid>]
                                    delete an archived session, or a project
                                    root with its descendants; --yes is
                                    required for a cascade or with --force
  project get <session-id>          report one project's members and parent
                                    project
  project models get <session-id>   report the models the project's delegated
                                    children run on
  project models set <session-id> [--child-model <id> | --no-child-model]
                     [--child-model-for <kind>=<id>]... [--unset-kind <kind>]...
                     [--clear] [--request-id <uuid>]
                                    change the project's child models; the
                                    id must be the project root
  project tree <session-id>         print the project hierarchy as an
                                    indented tree
  project tasks <session-id>        list the project's delegated tasks
  project children <session-id>     list the direct child agents with their
                                    task, run, and blocking state
  project messages <session-id> [--after <seq>] [--limit <n>]
                                    print a page of the project's agent
                                    messages; --limit is 1..512, default 100
  project send <session-id> (--text <text> | --text-file <path>)
               [--request-id <uuid>]
                                    direct the active run of a delegated
                                    child and print its state
  project child pause|resume|interrupt|cancel <task-id> --session <id>
                [--request-id <uuid>]
                                    control one delegated child task
  project archive <session-id> [--request-id <uuid>]
                                    archive a project root with its idle
                                    members; the id must name such a root
  project delete <session-id> [--force] [--yes] [--request-id <uuid>]
                                    delete a project root and its tree; the
                                    id must name such a root, and --yes is
                                    required
  project worktrees <session-id>    list the checkouts the project's snapshot
                                    records
  project worktree get <session-id> --task <task-id>
                                    report one delegated task's durable
                                    worktree record
  project worktree diff <session-id> --task <task-id>
                                    report the worker's bounded review of one
                                    checkout
  project worktree integrate <session-id> --task <task-id>
      [--expected-parent-revision <rev>] [--request-id <uuid>]
                                    fast-forward the parent with the task's
                                    result revision; a conflict is reported,
                                    never retried
  project worktree cleanup <session-id> --task <task-id>
      (--keep | --remove | --discard) [--yes] [--request-id <uuid>]
                                    keep or remove the checkout of one
                                    delegated task; --discard throws away
                                    uncommitted changes and needs --yes
  delete <session-id> [--force] [--yes] [--request-id <uuid>]
                                    delete an archived session, or a project
                                    root with its descendants; the same
                                    operation as 'session delete'

`run` never prints more than `session <id> <name>` and `run <id> <state>`: it
returns as soon as the worker accepts the run. --repo and --as-directory are
mutually exclusive, --revision applies to --repo only, and --path defaults to
the source's final path component without a trailing `.git`. --as-directory
attaches the folder in place and the run's tools work inside it, so the files
the session writes for that input land in the folder itself. Without --model,
`run` asks the worker for its providers and selects the first provider (in the
order the worker returns them) that has a credential and a known default model,
as `loom:model:v1:["<provider-id>","<model>"]`; when no provider has a
credential it warns on stderr and starts `deterministic/demo`. An explicit
--model always wins. A session is only attached to when this invocation
created it, so replaying a command with the same --request-id neither
duplicates an attachment nor fails on one that exists. --request-id is the
idempotency key of the run start alone: the same id
is answered from the worker's durable cache with the same run, while the other
mutations of the verb use fresh ids. On a session this invocation creates,
--approval-policy (default when absent) is pinned before the run starts; on a
reused session it is only changed when the flag is given, so a plain run does
not overwrite a policy a person set. --project <session-id> names a project
root of the workspace: the session is then reused only among that project's
sub projects, is created as a sub project of it (listed beneath it in
loom-ui), inherits its folders and repositories unless --repo or
--as-directory names the input instead, and `run` prints a third line,
`parent <project-id>`.

`status --tree` reports the project the session belongs to as well: one line
per agent, delegated task, and worktree, the conflict paths of a worktree, and
one `decision` line per agent waiting on an approval or a question. The
requested session's pending decision appears once, as a `decision` line,
instead of the plain `pending` line. The project verdict is derived by this
client from the agent states, in the order blocked, paused, working, failed,
done, and archived only when every agent is archived. `--tree` reports several
text lines, so it refuses `--format verb`.

`send`, `answer`, `approve`, and `deny` read the run revision, act against
it, and retry once after a conflict, because their intent is idempotent.
`project send` reads one `Project/GetProjectSnapshotForSession`, requires the
session to be the target of a task in it, reads the session metadata for the
child's active run, and then directs that run with the same rules as `send`.
`stop` refuses a conflict unless --retry-on-conflict is given, so a stale
revision cannot interrupt a run whose state the caller has not seen.

`--request-id` on `run`, `send`, `project send`, `answer`, `approve`, `deny`,
`stop`, `delete`, `session archive`, `session delete`, `project archive`,
`project delete`, `project child`, `project worktree integrate`, and `project
worktree cleanup` is the idempotency key of that verb's own mutation: a request
the worker has already answered under that id is answered from its durable
cache instead of acting a second time, so a repeated archive, deletion,
integration, or cleanup is applied once, and the --json envelope then reports
`replayed: true`. The key covers the mutation alone.
Reads are never keyed: `send`, `answer`, `approve`, `deny`, and `stop` read the
run revision first and keep a fresh id for that read, so a repeated command
whose run has since moved is refused as a conflict that names the key rather
than replayed, and the mutation is still never applied twice. `stop --archive`
archives the session under an id derived from the key, so a replay does not
archive twice. The lifecycle verbs key the archive, deletion, integration, or
cleanup request while their project snapshot pre-read keeps a fresh id, so a
repeated `session delete` is answered from the cache even though the session is
gone.
Reusing one key for a different mutation is refused the same way. A verb
without the flag, and any verb that does not take it such as `wait` or `ls`,
behaves exactly as before.

`run --new` always creates a session and never lists or matches one by name.
A session name is a label, not an identity: a repeated --new can produce a
second session with an existing name, so pass a unique --name when the name is
meant to be a handle.

`events` never reprints an event at or below its cursor. A snapshot that cannot
carry every event between the cursor and its window (the worker bounds it to
the newest side) prints the window, reports the missing range as `gap` on
stderr and in the --json envelope, and resumes at the head; those older events
are no longer retrievable through this verb. --json with --follow collects
every followed event in one envelope, so prefer --format jsonl for a long
follow.

`delete` is explicit and irreversible: there is no interactive prompt in a
headless client, so the session id is the confirmation. The worker refuses a
session that is not archived unless it is a project root, and archiving is
terminal, so nothing restores a deleted session. --yes is required when the
command would remove more than one session (a project root cascades) and
whenever --force is given: --force discards dirty or locked worktrees that
would otherwise refuse the deletion. `delete` and `session delete` are the same
operation; `session archive`, `session delete`, and `project worktree cleanup`
are separate: archiving keeps every checkout, and only an explicit cleanup
disposition removes one.

`project archive` and `project delete` are the project-scoped spellings of the
same requests. Their id must name a project root that owns another live member
or a delegated task, so a mistyped or plain session id is refused as a usage
error instead of quietly acting on one session. `session archive` accepts any
session, and asks for --yes when the id is a project root whose archive reaches
its descendants, so the blast radius of every command is one explicit decision.
`project worktree cleanup` keeps (--keep) or removes (--remove, --discard) the
checkout recorded for one delegated task, resolving its project and manager
from the worker's project snapshot. --remove refuses a checkout that holds
changes; only --discard, with --yes, throws them away.

Every `project` read is addressed by the id of any project member session; a
project root's session id is also its project id. All of them are reads and
negotiate no mutating capability, so a read-only-scoped token can run them.
Message history is read-only as well: there is no agent message verb, because
an agent-attributed message is reserved for the server-bound agent tool.
`project send` is a direction to the addressed child's active run with exactly
the rules of `send --run`, and it is not a durable project message: it reads
the child's session metadata for the run, then sends
`Collaboration/DirectAgent`. `project child` names the task id the project
snapshot reports and sends the server-owned task's requester as the manager, so
no flag can supply one. An unknown session is `not_found` (exit 5), and so is a
task id the project does not name. A child whose session carries no active run
is `invalid_state` (exit 5) and no direction is sent.

`--request-id` keys the mutation of `project send` and `project child` and not
their pre-reads: a repeated id is answered from the worker's durable cache
instead of directing or controlling twice, and the `--json` envelope then
reports `replayed: true`. The project snapshot read, the session metadata read,
and the run revision read always keep fresh ids.

`jev` is the supervision helper of this client alone: it calls a decision model
directly, not through the worker, and no core crate knows it exists.
`jev status` and `jev ask` are local, so they need neither a worker nor a
`--token-file`; `jev triage` reads one session's pending approval from the
worker and always prints a suggestion. The key is read from `--key-file`, else
`~/.config/loom/typesafe.key` or `~/.config/loom/opencode.key` by source, and is
never printed, stored in a parsed command, or included in an error. `--source`
selects the endpoint and model (`typesafe` is `jev-latest` at
https://api.typesafe.ai/v1/systemone, `opencode` is `jev-1.13-free` at
https://opencode.ai/inference/systemone/v1/systemone); without it the first
source whose default key file exists wins, typesafe first. `--endpoint` and
`--model` override them. Every call sends `User-Agent: loom-cli/<version>`.

`jev triage --session <id>` suggests a verdict, and `--apply` is the only way it
acts: it approves the pending tool call only when the suggested verdict is
`approve`, its confidence is at or above `--threshold` (0.9 by default), the
call is one of the read tools (`read_file`, `list_files`, `search_text`,
`glob`), and every path argument resolves inside the session root the worker
reports. Anything else is reported as a skip and nothing is approved. A
suggestion is always advisory: it is this client's reading of one model answer
and never a substitute for `approve`.

Global flags, accepted anywhere as --flag value or --flag=value:
  --connect <url>             worker WebSocket URL (fallback LOOM_URL, then
                              ws://127.0.0.1:8765/ws)
  --token-file <path>         file holding the bearer token (fallback
                              LOOM_TOKEN_FILE, then <state dir>/<host>_<port>
                              /token derived from the connected URL); a
                              leading ~/ is expanded with HOME
  --ca-file <pem>             also trust this PEM certificate
  --allow-insecure-remote     accept plaintext ws:// to a non-loopback host
  --max-frame-bytes <n>       transport frame size limit
  --json                      print exactly one JSON envelope on stdout
  --help, -h                  print this help, or one verb's page when the
                              line names a verb, as in 'loom-cli run --help'

The token is read from a file only: --token is rejected, because a token on a
command line is visible to every process on the node. A provider API key is
read from the environment or a file only: --key is rejected for the same
reason.

`providers add` registers an OpenAI-compatible gateway the worker did not seed,
which is how a headless node reaches OpenCode Go or OpenCode Zen. The models
keep the order given and `--default-model` is moved to the front, because that
first model is the one `run` starts when it derives a model from the worker.
Add the provider first, then store its key with `providers set-key`; a provider
without a key is not restored after a worker restart. Go and Zen may share one
key, which is stored once per provider id. `providers discover` replaces the
models with the endpoint's own `/models` catalog, and `providers remove` drops
the provider and its stored key. A built-in provider (openai, deepseek,
ollama, github-copilot, deterministic) can be neither redefined nor removed.

Exit codes: 0 ok, 1 internal, 2 usage, 3 blocked, 4 timeout, 5 not found or
gone, 6 protocol or auth mismatch, 7 transport unreachable.
"#;

/// How much detail a verb asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Format {
    Text,
    Jsonl,
}

/// Settings every verb shares.
#[derive(Clone, Debug, Default)]
pub(crate) struct Globals {
    pub connect: Option<String>,
    pub token_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub allow_insecure_remote: bool,
    pub max_frame_bytes: Option<usize>,
    pub json: bool,
}

/// The flags of `events`.
#[derive(Clone, Debug)]
pub(crate) struct EventsArgs {
    pub session: AgentSessionId,
    pub after: Option<u64>,
    pub cursor_file: Option<PathBuf>,
    pub follow: bool,
    /// `--timeout`: the bound of a `--follow`, in seconds. `None` follows until
    /// the run completes, the journal stalls the caller, or the process is
    /// interrupted.
    pub timeout_secs: Option<u64>,
    pub format: Format,
    pub max_bytes: usize,
}

/// The flags of `peek`.
#[derive(Clone, Debug)]
pub(crate) struct PeekArgs {
    pub session: AgentSessionId,
    pub lines: u32,
    pub format: Format,
    pub max_bytes: usize,
}

/// The preset `--approval-policy` selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalPreset {
    /// The shipped policy: reads and destructive actions run, writes, commands,
    /// and network access ask first.
    Default,
    /// Every action kind runs without a prompt.
    Auto,
}

/// One input `run` attaches to a session it creates: a repository to clone or a
/// local directory to mount, and the relative path it lands at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Attach {
    pub source: String,
    pub path: String,
    /// The revision to clone for a repository; always absent for a directory.
    pub revision: Option<String>,
}

/// What `wait` waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitUntil {
    /// A pending approval or question, which is exit `3` at once.
    Blocked,
    /// A completed, failed, or cancelled run.
    Terminal,
}

/// How `status` renders its answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatusFormat {
    Text,
    /// One declaration line: `working: …`, `blocked: …`, and so on.
    Verb,
}

/// The flags of `run`.
#[derive(Clone, Debug)]
pub(crate) struct RunArgs {
    pub workspace: WorkspaceId,
    /// The task text, either `--task` or the contents of `--task-file`.
    pub task: String,
    /// The session to reuse or create, derived from the task when absent.
    pub name: String,
    /// `--model`, or `None` when the flag was absent and the model is derived
    /// from the worker's providers at run time.
    pub model: Option<ModelId>,
    /// `--repo`, attached only when this invocation creates the session.
    pub repository: Option<Attach>,
    /// `--as-directory`, attached only when this invocation creates the session.
    pub directory: Option<Attach>,
    /// `--approval-policy`, or `None` when the flag was absent. A created
    /// session gets the default preset when it is absent; a reused session is
    /// only changed when the flag was given.
    pub approval: Option<ApprovalPreset>,
    /// The run start's idempotency key, or `None` when the caller left it open.
    pub request_id: Option<RequestId>,
    /// `--new`: create a session unconditionally instead of reusing one by
    /// name.
    pub new: bool,
    /// `--project`: the project root the session is a sub project of. The
    /// session is reused only among that project's sub projects and created
    /// with `Project/CreateSubProject`, so a client lists it beneath the
    /// project instead of as a project of its own.
    pub project: Option<AgentSessionId>,
    /// `--child-model` and `--child-model-for`: the child model entries stored
    /// on the session's project before the run starts.
    pub child_models: ChildModelChanges,
}

/// The child model entries one command changes. Entries a flag does not name
/// keep their stored value, so a command never drops a kind it did not mention.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChildModelChanges {
    /// `--clear`: start from an empty policy.
    pub clear: bool,
    /// `--child-model` (`Some(Some(id))`) or `--no-child-model` (`Some(None)`).
    pub default_model: Option<Option<String>>,
    /// `--child-model-for <kind>=<id>`, with normalized kinds.
    pub set_kinds: Vec<(String, String)>,
    /// `--unset-kind <kind>`, normalized.
    pub unset_kinds: Vec<String>,
}

impl ChildModelChanges {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The policy these changes turn `stored` into.
    pub(crate) fn apply(
        &self,
        stored: loom_core::ProjectChildModelPolicy,
    ) -> loom_core::ProjectChildModelPolicy {
        let mut policy = if self.clear {
            loom_core::ProjectChildModelPolicy::default()
        } else {
            stored
        };
        if let Some(default_model) = &self.default_model {
            policy.default_model_id = default_model.clone();
        }
        for kind in &self.unset_kinds {
            policy.by_kind.remove(kind);
        }
        for (kind, model) in &self.set_kinds {
            policy.by_kind.insert(kind.clone(), model.clone());
        }
        policy
    }
}

/// Where `providers set-key` reads the API key. The secret itself is not stored
/// here: the verb reads it when it runs, so a debug print of the parsed command
/// cannot contain the key.
#[derive(Clone, Debug)]
pub(crate) enum ProviderKeySource {
    Env(String),
    File(PathBuf),
}

/// The flags of `providers set-key`.
#[derive(Clone, Debug)]
pub(crate) struct ProviderSetKeyArgs {
    pub provider: String,
    pub source: ProviderKeySource,
}

/// The flags of `providers add`.
///
/// The provider is an OpenAI-compatible gateway the operator supplies: its id,
/// its chat-completions endpoint, and the models it serves. The models keep the
/// order given, and `--default-model` names the one stored first, which is the
/// model `run` starts when it derives a model from the worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderAddArgs {
    pub provider: String,
    pub endpoint: String,
    pub models: Vec<String>,
    pub default_model: Option<String>,
}

impl ProviderAddArgs {
    /// The models in the order the worker stores them: the default model
    /// first, then the rest as they were given.
    pub(crate) fn ordered_models(&self) -> Vec<String> {
        let Some(default) = &self.default_model else {
            return self.models.clone();
        };
        let mut models = Vec::with_capacity(self.models.len());
        models.push(default.clone());
        models.extend(
            self.models
                .iter()
                .filter(|model| *model != default)
                .cloned(),
        );
        models
    }
}

/// The flags of `retention set`.
///
/// Each field is `None` when its flag was absent, so that value is kept from
/// the policy the worker already stores. A window flag carries `Some(None)` for
/// `off`, which disables that window.
#[derive(Clone, Debug, Default)]
pub(crate) struct RetentionSetArgs {
    pub window_ms: Option<Option<u64>>,
    pub force_discard_worktrees: Option<bool>,
    pub sub_agent_ms: Option<Option<u64>>,
}

/// Where the `jev` verbs find their key and which endpoint and model they call.
///
/// The secret is never stored here: the verb reads the file when it runs, so a
/// debug print of a parsed command cannot contain a key. `None` fields were not
/// named on the command line and are resolved by the verb, which is what lets
/// `jev status` report the path it would read without any flag at all.
#[derive(Clone, Debug, Default)]
pub(crate) struct JevFlags {
    pub source: Option<String>,
    pub key_file: Option<PathBuf>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
}

/// The flags of `jev ask`: one choice question over a bounded state.
#[derive(Clone, Debug)]
pub(crate) struct JevAskArgs {
    pub state: String,
    pub question: String,
    /// The choice labels, each with the condition under which it applies.
    pub options: Vec<(String, String)>,
    pub flags: JevFlags,
}

/// The flags of `jev triage`: the approval one session waits on.
#[derive(Clone, Debug)]
pub(crate) struct JevTriageArgs {
    pub session: AgentSessionId,
    pub flags: JevFlags,
    /// `--apply`: approve the call when the verdict clears every guard.
    pub apply: bool,
    pub threshold: f64,
    /// The approval's idempotency key, or `None` when the caller left it open.
    pub request_id: Option<RequestId>,
}

/// The action a `project child` sub-verb sends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectChildAction {
    Pause,
    Resume,
    Interrupt,
    Cancel,
}

impl ProjectChildAction {
    /// The word the sub-verb is spelled with, which is also printed and
    /// reported in the `--json` payload.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Interrupt => "interrupt",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Verb {
    Doctor,
    Node,
    WhoAmI,
    Ls {
        workspace: Option<WorkspaceId>,
        archived: bool,
    },
    Get {
        session: AgentSessionId,
        metadata: bool,
    },
    Events(EventsArgs),
    Peek(PeekArgs),
    /// Boxed because the box keeps every variant small: a `Verb` is carried by
    /// one `Invocation` for the life of a command line.
    Run(Box<RunArgs>),
    Send {
        run: RunId,
        text: String,
        /// The collaboration mutation's idempotency key, or `None` when the
        /// caller left it open.
        request_id: Option<RequestId>,
    },
    Answer {
        run: RunId,
        text: String,
        /// The collaboration mutation's idempotency key, or `None` when the
        /// caller left it open.
        request_id: Option<RequestId>,
    },
    Approve {
        run: RunId,
        tool_call: ToolCallId,
        /// The control mutation's idempotency key, or `None` when the caller
        /// left it open.
        request_id: Option<RequestId>,
    },
    Deny {
        run: RunId,
        tool_call: ToolCallId,
        reason: Option<String>,
        /// The control mutation's idempotency key, or `None` when the caller
        /// left it open.
        request_id: Option<RequestId>,
    },
    Stop {
        run: RunId,
        archive: Option<AgentSessionId>,
        retry_on_conflict: bool,
        /// The interrupt's idempotency key, or `None` when the caller left it
        /// open. An archive is keyed by an id derived from it.
        request_id: Option<RequestId>,
    },
    Wait {
        session: AgentSessionId,
        until: WaitUntil,
        timeout_secs: u64,
    },
    Status {
        session: AgentSessionId,
        format: StatusFormat,
        /// `--tree`: also print the project the session belongs to.
        tree: bool,
    },
    Liveness {
        session: AgentSessionId,
    },
    Result {
        session: AgentSessionId,
        task: Option<TaskId>,
    },
    RetentionGet,
    RetentionSet(RetentionSetArgs),
    ProvidersList,
    ProvidersSetKey(ProviderSetKeyArgs),
    /// `providers add`: register an OpenAI-compatible provider.
    ProvidersAdd(ProviderAddArgs),
    /// `providers remove`: drop an OpenAI-compatible provider and its key.
    ProvidersRemove {
        provider: String,
    },
    /// `providers discover`: replace one provider's models with the endpoint's
    /// own `/models` catalog.
    ProvidersDiscover {
        provider: String,
    },
    /// `jev status`: the key, endpoint, and model a Jev call would use.
    JevStatus(JevFlags),
    /// `jev ask`: one raw choice question to the decision model.
    JevAsk(Box<JevAskArgs>),
    /// `jev triage`: a suggested verdict for the approval a session waits on.
    JevTriage(Box<JevTriageArgs>),
    /// `models list`: the models of the configured providers, optionally of one
    /// exact provider.
    ModelsList {
        provider: Option<String>,
    },
    /// `session archive`: archive one session, and its descendants when the id
    /// is a project root that cascades. `yes` acknowledges that cascade.
    SessionArchive {
        session: AgentSessionId,
        yes: bool,
        /// The archive request's idempotency key, or `None` when the caller
        /// left it open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
    /// `session delete`: the explicit spelling of `delete`.
    SessionDelete {
        session: AgentSessionId,
        force: bool,
        yes: bool,
        /// The deletion's idempotency key, or `None` when the caller left it
        /// open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
    /// `project archive`: archive a project root with its idle members.
    ProjectArchive {
        session: AgentSessionId,
        /// The archive request's idempotency key, or `None` when the caller
        /// left it open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
    /// `project delete`: delete a project root with its tree.
    ProjectDelete {
        session: AgentSessionId,
        force: bool,
        yes: bool,
        /// The deletion's idempotency key, or `None` when the caller left it
        /// open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
    /// `project worktrees`: every checkout the project's snapshot records.
    ProjectWorktrees {
        session: AgentSessionId,
    },
    /// `project worktree get`: one delegated task's durable worktree record.
    ProjectWorktreeGet {
        session: AgentSessionId,
        task: TaskId,
    },
    /// `project worktree diff`: the worker's bounded review of one checkout.
    ProjectWorktreeDiff {
        session: AgentSessionId,
        task: TaskId,
    },
    /// `project worktree integrate`: fast-forward the parent with the task's
    /// result, expecting `expected_parent_revision` (or the record's base
    /// revision) as the parent tip.
    ProjectWorktreeIntegrate {
        session: AgentSessionId,
        task: TaskId,
        expected_parent_revision: Option<String>,
        /// The integration request's idempotency key, or `None` when the
        /// caller left it open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
    /// `project worktree cleanup`: keep or remove one delegated task's
    /// checkout.
    ProjectWorktreeCleanup {
        session: AgentSessionId,
        task: TaskId,
        disposition: WorktreeDisposition,
        /// The cleanup request's idempotency key, or `None` when the caller
        /// left it open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
    /// `delete`: the id is the confirmation. `force` discards dirty or locked
    /// worktrees; `yes` acknowledges a cascade or a force discard.
    Delete {
        session: AgentSessionId,
        force: bool,
        yes: bool,
        /// The deletion's idempotency key, or `None` when the caller left it
        /// open.
        request_id: Option<RequestId>,
    },
    /// `project models get`: the project's child model policy.
    ProjectModelsGet {
        session: AgentSessionId,
    },
    /// `project models set`: change the project's child model policy.
    ProjectModelsSet {
        session: AgentSessionId,
        changes: ChildModelChanges,
        /// The change's idempotency key, or `None` when the caller left it
        /// open.
        request_id: Option<RequestId>,
    },
    /// `project get`: the hierarchy and parent project of one member.
    ProjectGet {
        session: AgentSessionId,
    },
    /// `project tree`: the same hierarchy as an indented pre-order tree.
    ProjectTree {
        session: AgentSessionId,
    },
    /// `project tasks`: the project's delegated tasks.
    ProjectTasks {
        session: AgentSessionId,
    },
    /// `project children`: the direct children of the addressed session.
    ProjectChildren {
        session: AgentSessionId,
    },
    /// `project messages`: one page of the project's agent message history.
    ProjectMessages {
        session: AgentSessionId,
        after: Option<u64>,
        limit: u32,
    },
    /// `project send`: a direction to a delegated child's active run.
    ProjectSend {
        session: AgentSessionId,
        text: String,
        /// The direction's idempotency key, or `None` when the caller left it
        /// open. The pre-reads before it are never keyed.
        request_id: Option<RequestId>,
    },
    /// `project child`: one control action on a delegated task.
    ProjectChild {
        session: AgentSessionId,
        task: TaskId,
        action: ProjectChildAction,
        /// The control request's idempotency key, or `None` when the caller
        /// left it open. The project pre-read is never keyed.
        request_id: Option<RequestId>,
    },
}

/// The disposition `project worktree cleanup` asks the worker for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorktreeDisposition {
    /// Keep the checkout and record it as retained.
    Keep,
    /// Remove the checkout, which the worker refuses while it holds changes.
    Remove,
    /// Remove the checkout together with the uncommitted changes it holds.
    Discard,
}

impl WorktreeDisposition {
    /// The flag spelling, which text and `--json` output report.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Remove => "remove",
            Self::Discard => "discard",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Invocation {
    pub globals: Globals,
    pub verb: Verb,
}

pub(crate) enum Parsed {
    /// Print this text: the global manual, or one verb's page when the command
    /// line named a verb.
    Help(&'static str),
    Run(Invocation),
}

/// The verb's name, as printed and as reported in a `--json` envelope.
pub(crate) fn verb_name(verb: &Verb) -> &'static str {
    match verb {
        Verb::Doctor => "doctor",
        Verb::Node => "node",
        Verb::WhoAmI => "whoami",
        Verb::Ls { .. } => "ls",
        Verb::Get { .. } => "get",
        Verb::Events(_) => "events",
        Verb::Peek(_) => "peek",
        Verb::Run(_) => "run",
        Verb::Send { .. } => "send",
        Verb::Answer { .. } => "answer",
        Verb::Approve { .. } => "approve",
        Verb::Deny { .. } => "deny",
        Verb::Stop { .. } => "stop",
        Verb::Wait { .. } => "wait",
        Verb::Status { .. } => "status",
        Verb::Liveness { .. } => "liveness",
        Verb::Result { .. } => "result",
        Verb::RetentionGet => "retention get",
        Verb::RetentionSet(_) => "retention set",
        Verb::ProvidersList => "providers list",
        Verb::ProvidersSetKey(_) => "providers set-key",
        Verb::ProvidersAdd(_) => "providers add",
        Verb::ProvidersRemove { .. } => "providers remove",
        Verb::ProvidersDiscover { .. } => "providers discover",
        Verb::JevStatus(_) => "jev status",
        Verb::JevAsk(_) => "jev ask",
        Verb::JevTriage(_) => "jev triage",
        Verb::ModelsList { .. } => "models list",
        Verb::SessionArchive { .. } => "session archive",
        Verb::SessionDelete { .. } => "session delete",
        Verb::ProjectArchive { .. } => "project archive",
        Verb::ProjectDelete { .. } => "project delete",
        Verb::ProjectWorktrees { .. } => "project worktrees",
        Verb::ProjectWorktreeGet { .. } => "project worktree get",
        Verb::ProjectWorktreeDiff { .. } => "project worktree diff",
        Verb::ProjectWorktreeIntegrate { .. } => "project worktree integrate",
        Verb::ProjectWorktreeCleanup { .. } => "project worktree cleanup",
        Verb::Delete { .. } => "delete",
        Verb::ProjectGet { .. } => "project get",
        Verb::ProjectModelsGet { .. } => "project models get",
        Verb::ProjectModelsSet { .. } => "project models set",
        Verb::ProjectTree { .. } => "project tree",
        Verb::ProjectTasks { .. } => "project tasks",
        Verb::ProjectChildren { .. } => "project children",
        Verb::ProjectMessages { .. } => "project messages",
        Verb::ProjectSend { .. } => "project send",
        Verb::ProjectChild { action, .. } => match action {
            ProjectChildAction::Pause => "project child pause",
            ProjectChildAction::Resume => "project child resume",
            ProjectChildAction::Interrupt => "project child interrupt",
            ProjectChildAction::Cancel => "project child cancel",
        },
    }
}

/// Whether a verb answers without a worker connection.
///
/// `jev status` and `jev ask` read the local key file and call the decision
/// model, so they run on a machine that has no worker and no token. `jev
/// triage` reads one session from the worker and is therefore not local.
pub(crate) fn is_local(verb: &Verb) -> bool {
    matches!(verb, Verb::JevStatus(_) | Verb::JevAsk(_))
}

/// Whether `--json` appears anywhere in the arguments, so a usage error can be
/// reported in the mode the caller asked for even when parsing failed.
pub(crate) fn wants_json(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| argument == "--json")
}

/// The verb a rejected command line named, so its failure envelope can still
/// report which verb failed. Empty when no token names a known verb.
pub(crate) fn named_verb(arguments: &[String]) -> &'static str {
    arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map(|name| match name.as_str() {
            "doctor" => "doctor",
            "node" => "node",
            "whoami" => "whoami",
            "ls" => "ls",
            "get" => "get",
            "events" => "events",
            "peek" => "peek",
            "run" => "run",
            "send" => "send",
            "answer" => "answer",
            "approve" => "approve",
            "deny" => "deny",
            "stop" => "stop",
            "wait" => "wait",
            "status" => "status",
            "liveness" => "liveness",
            "result" => "result",
            "retention" => "retention",
            "providers" => "providers",
            "jev" => "jev",
            "models" => "models",
            "delete" => "delete",
            "session" => "session",
            "project" => "project",
            _ => "",
        })
        .unwrap_or("")
}

/// The per-verb help pages, keyed by the words that name the verb.
///
/// A page prints only its own usage, its flags, and a few copy-pasteable
/// examples, so a caller who knows the verb does not have to read the whole
/// manual. A bare `--help`, and a verb the table does not know, keep the global
/// manual in [`HELP`]. The table is static text: it is the contract a reader
/// copies from, not a second parser.
const VERB_HELP: &[(&str, &str)] = &[
    (
        "doctor",
        r#"Usage: loom-cli doctor

Report the negotiated protocol, the granted capability intersection, the
participant this connection acts as, and the worker node.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli doctor
  loom-cli doctor --json
"#,
    ),
    (
        "node",
        r#"Usage: loom-cli node

Report the worker node's id, name, online flag, capabilities, and health.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli node
  loom-cli node --json
"#,
    ),
    (
        "whoami",
        r#"Usage: loom-cli whoami

Report the participant this connection acts as: the person the token belongs
to, or {"kind":"operator"} for an operator token.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli whoami
  loom-cli whoami --json
"#,
    ),
    (
        "ls",
        r#"Usage: loom-cli ls [--workspace <id>] [--archived]

List every workspace, or the sessions of one workspace.

Flags:
  --workspace <id>            list that workspace's sessions instead
  --archived                  include archived sessions
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli ls
  loom-cli ls --workspace <workspace-id> --archived
  loom-cli ls --json
"#,
    ),
    (
        "get",
        r#"Usage: loom-cli get <session-id> [--metadata]

Show one session, and with --metadata its active run.

Flags:
  --metadata                  also read the session's active run
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli get <session-id>
  loom-cli get <session-id> --metadata --json
"#,
    ),
    (
        "events",
        r#"Usage: loom-cli events --session <id> [--after <seq>] [--cursor-file <path>]
                        [--follow] [--timeout <seconds>]
                        [--format text|jsonl] [--max-bytes <n>]

Print a session's events, optionally following for new ones.

Flags:
  --session <id>              the session whose journal to read (required)
  --after <seq>               resume after that event sequence
  --cursor-file <path>        resume from, and persist, the cursor file
  --follow                    keep reading until the run completes
  --timeout <seconds>         bound the follow; exit 4 when it elapses
  --format text|jsonl         one text line or one JSON object per event
  --max-bytes <n>             byte budget of one rendered payload
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli events --session <session-id>
  loom-cli events --session <session-id> --cursor-file .loom-cursor --follow
  loom-cli events --session <session-id> --follow --timeout 60 --format jsonl
"#,
    ),
    (
        "peek",
        r#"Usage: loom-cli peek --session <id> [--lines <n>] [--format text|jsonl]
                     [--max-bytes <n>]

Print the newest events of a session, without following it.

Flags:
  --session <id>              the session to read (required)
  --lines <n>                 how many events to ask for (200 by default)
  --format text|jsonl         one text line or one JSON object per event
  --max-bytes <n>             byte budget of one rendered payload
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli peek --session <session-id>
  loom-cli peek --session <session-id> --lines 40 --json
"#,
    ),
    (
        "run",
        r#"Usage: loom-cli run --workspace <id> (--task <text> | --task-file <path>)
                      [--name <name>] [--model <id>]
                      [--repo <source> [--revision <rev>] [--path <path>]]
                      [--as-directory <path> [--path <path>]]
                      [--approval-policy default|auto] [--request-id <uuid>] [--new]
                      [--project <session-id>]

                      [--child-model <id>] [--child-model-for <kind>=<id>]...

Reuse the workspace's session named --name, or create it, attach the inputs to
a session this run created, and start one agent run without waiting for it.
Without --model, the first provider that has a credential and a known default
model is started; when there is none, 'deterministic/demo' is started and a
warning is printed on stderr.

--as-directory attaches a local folder in place as the session's input and makes
it the agent's working directory, so the files the run writes for that input
land in the folder instead of in the worker's private session storage; --path
names the session path the same folder is also reachable at.

With --project, the session belongs to that project instead of being a project
of its own. The id must be a live project root in --workspace (the id
`status --tree` prints on its `project` line); an agent of a project, an
archived session, or a session of another workspace is refused. --name is then
reused only among that project's sub projects, --new always creates a new sub
project, and a sub project this run creates inherits the project's folders and
repositories, the way loom-ui's "New sub project" does, unless --repo or
--as-directory names its input instead. A source that cannot be inherited is a
warning on stderr, not a failure. The output gains a third line,
`parent <project-id>`, and --json reports `project.parent_id`.

--model is the model of this run. --child-model and --child-model-for choose
the models the run's delegated children use, stored on the session's project
before the run starts: a child of a mapped kind runs on that kind's model, any
other child on --child-model, and without either it reuses the run's model.
They change only the entries they name; 'project models set' edits the rest.

Flags:
  --workspace <id>            the workspace the session belongs to (required)
  --task <text>               the task text
  --task-file <path>          read the task text from a file instead
  --name <name>               reuse or create the session with that name
  --model <id>                the model to start
  --repo <source>             clone a repository into a new session
  --as-directory <path>       the local folder the run works in (new session)
  --revision <rev>            the revision to clone (with --repo)
  --path <path>               where an attached input lands
  --approval-policy <preset>  pin default or auto before the run starts
  --request-id <uuid>         the idempotency key of the run start
  --new                       always create a session
  --project <session-id>      create the session as a sub project of this
                              project root, and reuse only its sub projects
  --child-model <id>          the default model of delegated children
  --child-model-for <k>=<id>  the model of children of kind k; repeatable
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli run --workspace <workspace-id> --task 'fix the failing test'
  loom-cli run --workspace <workspace-id> --project <project-session-id> \
    --name 'cli: run --project' --task-file task.md --new --json
  loom-cli run --workspace <workspace-id> --task 'triage the screenshots' \
    --model deepseek-flash --child-model deepseek-flash \
    --child-model-for vision=<vision-model-id>
  loom-cli run --workspace <workspace-id> --task-file task.md \
    --repo https://github.com/acme/widgets.git --approval-policy auto
  loom-cli run --workspace <workspace-id> --task 'summarise the diff' \
    --model 'loom:model:v1:["deepseek","deepseek-flash"]' --json
"#,
    ),
    (
        "send",
        r#"Usage: loom-cli send --run <id> --text <text> [--request-id <uuid>]

Direct the active run and print its state.

Flags:
  --run <id>                  the run to direct (required)
  --text <text>               the direction to send (required)
  --request-id <uuid>         the idempotency key of the direction
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli send --run <run-id> --text 'prefer the smaller diff'
  loom-cli send --run <run-id> --text 'stop at the first failure' --json
"#,
    ),
    (
        "answer",
        r#"Usage: loom-cli answer --run <id> --text <text> [--request-id <uuid>]

Answer the question the run is waiting on.

Flags:
  --run <id>                  the run to answer (required)
  --text <text>               the answer (required)
  --request-id <uuid>         the idempotency key of the answer
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli answer --run <run-id> --text 'use the existing migration'
  loom-cli answer --run <run-id> --text 'yes, delete the fixture' --json
"#,
    ),
    (
        "approve",
        r#"Usage: loom-cli approve --run <id> --tool-call <id> [--request-id <uuid>]

Approve the tool call that blocked the run.

Flags:
  --run <id>                  the blocked run (required)
  --tool-call <id>            the pending tool call (required)
  --request-id <uuid>         the idempotency key of the approval
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli approve --run <run-id> --tool-call <tool-call-id>
  loom-cli approve --run <run-id> --tool-call <tool-call-id> --json
"#,
    ),
    (
        "deny",
        r#"Usage: loom-cli deny --run <id> --tool-call <id> [--reason <text>]
                    [--request-id <uuid>]

Deny the tool call that blocked the run.

Flags:
  --run <id>                  the blocked run (required)
  --tool-call <id>            the pending tool call (required)
  --reason <text>             why it was denied
  --request-id <uuid>         the idempotency key of the denial
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli deny --run <run-id> --tool-call <tool-call-id>
  loom-cli deny --run <run-id> --tool-call <tool-call-id> --reason 'watch the tests' --json
"#,
    ),
    (
        "stop",
        r#"Usage: loom-cli stop --run <id> [--session <id> --archive]
                    [--retry-on-conflict] [--request-id <uuid>]

Interrupt a run, and optionally archive its session afterwards.

Flags:
  --run <id>                  the run to interrupt (required)
  --archive                   archive the session after the interrupt
  --session <id>              the session --archive archives (required with it)
  --retry-on-conflict         re-read the revision and retry once
  --request-id <uuid>         the idempotency key of the interrupt
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli stop --run <run-id>
  loom-cli stop --run <run-id> --session <session-id> --archive --json
"#,
    ),
    (
        "wait",
        r#"Usage: loom-cli wait --session <id> [--until blocked|terminal] [--timeout <seconds>]

Poll until the run blocks or reaches a terminal state; always time-bounded.

Flags:
  --session <id>              the session to poll (required)
  --until blocked|terminal    what to wait for (blocked by default)
  --timeout <seconds>         the poll deadline (60 by default)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli wait --session <session-id> --until blocked --timeout 30
  loom-cli wait --session <session-id> --until terminal --timeout 600 --json
"#,
    ),
    (
        "status",
        r#"Usage: loom-cli status --session <id> [--format text|verb] [--tree]

Report the session and its latest run.

Flags:
  --session <id>              the session to report (required)
  --format text|verb          one declaration line with verb
  --tree                      also print the project the session belongs to
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli status --session <session-id>
  loom-cli status --session <session-id> --tree
  loom-cli status --session <session-id> --format verb --json
"#,
    ),
    (
        "liveness",
        r#"Usage: loom-cli liveness --session <id>

Report alive, dead, gone, or unknown for one session, never guessing.

Flags:
  --session <id>              the session to classify (required)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli liveness --session <session-id>
  loom-cli liveness --session <session-id> --json
"#,
    ),
    (
        "result",
        r#"Usage: loom-cli result --session <id> [--task <id>]

Report branch, head, diffstat, changed paths, and task evidence.

Flags:
  --session <id>              the session whose result to report (required)
  --task <id>                 also report that task's evidence links
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli result --session <session-id>
  loom-cli result --session <session-id> --task <task-id> --json
"#,
    ),
    (
        "retention",
        r#"Usage: loom-cli retention get|set

Report or change the worker-wide archive-retention policy.

Sub-verbs:
  get                         report the stored policy
  set                         merge fields into the stored policy

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli retention get
  loom-cli retention set --window 14d --json
"#,
    ),
    (
        "retention get",
        r#"Usage: loom-cli retention get

Report the worker-wide archive-retention policy, with a disabled window as off.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli retention get
  loom-cli retention get --json
"#,
    ),
    (
        "retention set",
        r#"Usage: loom-cli retention set [--window <duration>] [--force-discard <bool>]
                              [--sub-agent <duration>]

Merge the given fields into the stored worker-wide archive-retention policy.
The next sweep applies it; the command never deletes anything itself.

Flags:
  --window <duration>         how long an archived session is kept
  --force-discard <bool>      discard dirty worktrees on the next sweep
  --sub-agent <duration>      the window of a delegated worktree
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli retention set --window 14d
  loom-cli retention set --window off --force-discard true --json
"#,
    ),
    (
        "providers",
        r#"Usage: loom-cli providers list|add|remove|discover|set-key

List the configured providers, register an OpenAI-compatible one, or store one
provider's API key.

Sub-verbs:
  list                        list the providers and their credentials
  add                         register an OpenAI-compatible provider
  remove                      remove an OpenAI-compatible provider and its key
  discover                    replace one provider's models with its /models
  set-key                     store a key read from a variable or a file

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli providers list
  loom-cli providers add --id opencode-go --kind openai-compatible \
    --endpoint https://opencode.ai/inference/go/openai/v1/chat/completions \
    --model qwen3.8-flash
  loom-cli providers set-key --provider openai --from-env OPENAI_API_KEY
  loom-cli providers remove --id opencode-go
"#,
    ),
    (
        "providers list",
        r#"Usage: loom-cli providers list

List the configured providers and whether each has a credential. No secret,
credential reference, or key is ever printed.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli providers list
  loom-cli providers list --json
"#,
    ),
    (
        "providers set-key",
        r#"Usage: loom-cli providers set-key --provider <id>
                              (--from-env <NAME> | --key-file <path>)

Store an API key for openai, deepseek, or an openai-compatible provider. The
key is read from a variable or a file, never from the command line.

Flags:
  --provider <id>             the provider to configure (required)
  --from-env <NAME>           read the key from that variable
  --key-file <path>           read the key from a file
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli providers set-key --provider openai --from-env OPENAI_API_KEY
  loom-cli providers set-key --provider deepseek --key-file ~/.config/loom/deepseek.key --json
  loom-cli providers set-key --provider opencode-go --key-file ~/.config/loom/opencode.key
"#,
    ),
    (
        "providers add",
        r#"Usage: loom-cli providers add --id <id> --kind openai-compatible
                              --endpoint <url> [--model <id>]...
                              [--default-model <id>]

Register an OpenAI-compatible provider that the worker did not seed, then store
its key with 'providers set-key --provider <id>'.

Flags:
  --id <id>                   the provider id, such as opencode-go (required)
  --kind <kind>               openai-compatible (required)
  --endpoint <url>            the chat-completions URL (required)
  --model <id>                a model the endpoint serves; repeatable, and at
                              least one is required
  --default-model <id>        the model 'run' starts when it derives one; it
                              must be one of the --model ids and is stored
                              first
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

A provider without a stored key is not restored after a worker restart, so add
it and store its key in the same session.

Examples:
  loom-cli providers add --id opencode-go --kind openai-compatible \
    --endpoint https://opencode.ai/inference/go/openai/v1/chat/completions \
    --model qwen3.8-flash --model glm-5.3-flash --default-model qwen3.8-flash
  loom-cli providers set-key --provider opencode-go \
    --key-file ~/.config/loom/opencode.key
  loom-cli providers discover --provider opencode-go
"#,
    ),
    (
        "providers remove",
        r#"Usage: loom-cli providers remove --id <id>

Remove an OpenAI-compatible provider and the API key stored for it. A built-in
provider (openai, deepseek, ollama, github-copilot, deterministic) is refused.

Flags:
  --id <id>                   the provider to remove (required)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli providers remove --id opencode-go
  loom-cli providers remove --id opencode-go --json
"#,
    ),
    (
        "providers discover",
        r#"Usage: loom-cli providers discover --provider <id>

Replace one provider's model catalog with the catalog its endpoint's /models
returns. The worker stores the answer, so a later restart serves it without
contacting the provider. Store the API key first when the endpoint requires
one.

Flags:
  --provider <id>             the provider to refresh (required)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli providers discover --provider opencode-go
  loom-cli providers discover --provider opencode-go --json
"#,
    ),
    (
        "models",
        r#"Usage: loom-cli models list [--provider <id>]

List the models of the configured providers.

Sub-verbs:
  list                        list the models and one default per provider

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli models list
  loom-cli models list --provider deepseek --json
"#,
    ),
    (
        "models list",
        r#"Usage: loom-cli models list [--provider <id>]

List the models of the configured providers and mark one default per provider.
No secret is printed.

Flags:
  --provider <id>             keep only that exact provider
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli models list
  loom-cli models list --provider deepseek --json
"#,
    ),
    (
        "session",
        r#"Usage: loom-cli session archive|delete <session-id>

Archive or delete one session.

Sub-verbs:
  archive                     keep the transcript and every checkout
  delete                      delete an archived session for good

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli session archive <session-id>
  loom-cli session delete <session-id> --yes --json
"#,
    ),
    (
        "session archive",
        r#"Usage: loom-cli session archive <session-id> [--yes] [--request-id <uuid>]

Archive one session, keeping its transcript, checkpoints, and every checkout.
--yes is required when the id is a project root whose archive also reaches its
descendants.

Flags:
  --yes                       acknowledge a project-root cascade
  --request-id <uuid>         the idempotency key of the archive
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli session archive <session-id>
  loom-cli session archive <project-root-id> --yes --json
"#,
    ),
    (
        "session delete",
        r#"Usage: loom-cli session delete <session-id> [--force] [--yes] [--request-id <uuid>]

Delete an archived session, or a project root with its descendants. --yes is
required for a cascade or with --force, and nothing restores a deleted session.

Flags:
  --force                     discard dirty or locked worktrees
  --yes                       acknowledge a cascade or a force discard
  --request-id <uuid>         the idempotency key of the deletion
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli session delete <session-id>
  loom-cli session delete <project-root-id> --yes --json
"#,
    ),
    (
        "delete",
        r#"Usage: loom-cli delete <session-id> [--force] [--yes] [--request-id <uuid>]

Delete an archived session, or a project root with its descendants; the same
operation as 'session delete', and irreversible.

Flags:
  --force                     discard dirty or locked worktrees
  --yes                       acknowledge a cascade or a force discard
  --request-id <uuid>         the idempotency key of the deletion
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli delete <session-id>
  loom-cli delete <project-root-id> --yes --json
"#,
    ),
    (
        "project",
        r#"Usage: loom-cli project <sub-verb> [flags]

Read a project's hierarchy, tasks, children, messages, worktrees, and child
model policy; direct or control one delegated child; archive or delete a
project.

Sub-verbs:
  get <session-id>            members and the project it was branched from
  tree <session-id>           the hierarchy as a tree
  tasks <session-id>          delegated tasks with status
  children <session-id>       direct child agents
  messages <session-id>       delegated-agent message history
  send <session-id>           direct a delegated child's active run
  child pause|resume|interrupt|cancel <task-id>
                              control one delegated child task
  archive <session-id>        archive a project root and its idle members
  delete <session-id>         delete a project root and its tree
  worktrees <session-id>      the checkouts the snapshot records
  worktree get|diff|integrate|cleanup <session-id> --task <task-id>
                              inspect or land one delegated checkout
  models get|set <session-id> the models delegated children run on

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project get <session-id>
  loom-cli project children <session-id> --json
"#,
    ),
    (
        "project get",
        r#"Usage: loom-cli project get <session-id>

Report one project's members and the project it was branched from.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project get <session-id>
  loom-cli project get <session-id> --json
"#,
    ),
    (
        "project models",
        r#"Usage: loom-cli project models get|set <session-id>

Report or change the models the project's delegated children run on.

Sub-verbs:
  get                         the stored default and per-kind models
  set                         change only the entries a flag names

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project models get <session-id>
  loom-cli project models set <project-root-id> --child-model deepseek-flash
"#,
    ),
    (
        "project models get",
        r#"Usage: loom-cli project models get <session-id>

Report the models the project's delegated children run on: the default child
model and one model per kind of work. The policy belongs to the project root,
so a member session is resolved to its root first; the printed project line
names that root. A child the agent delegates with a mapped `kind` runs on that
kind's model, any other child on the default, and without a default it reuses
its parent's model. A model the agent names explicitly overrides the project.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project models get <project-root-id>
  loom-cli project models get <session-id> --json
"#,
    ),
    (
        "project models set",
        r#"Usage: loom-cli project models set <project-root-id>
                      [--child-model <id> | --no-child-model]
                      [--child-model-for <kind>=<id>]... [--unset-kind <kind>]...
                      [--clear] [--request-id <uuid>]

Change the models the project's delegated children run on, and print the
stored result. Only the entries a flag names change; --clear starts from an
empty policy. A kind is 1 to 32 characters of a-z, 0-9, '_' or '-' such as
cheap, coding, review, or vision, and at most 16 kinds are stored. The worker
refuses a model it has not configured; 'loom-cli models list' shows them.

Flags:
  --child-model <id>          the default model of delegated children
  --no-child-model            remove the default; children reuse their parent's
  --child-model-for <k>=<id>  the model of children of kind k; repeatable
  --unset-kind <kind>         remove one kind's model; repeatable
  --clear                     drop every entry before applying the others
  --request-id <uuid>         the idempotency key of the change
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project models set <project-root-id> --child-model deepseek-flash
  loom-cli project models set <project-root-id> --child-model-for cheap=deepseek-flash \
    --child-model-for vision=<vision-model-id> --json
  loom-cli project models set <project-root-id> --clear
"#,
    ),
    (
        "project tree",
        r#"Usage: loom-cli project tree <session-id>

Print the project hierarchy as an indented tree, rooted at the project root.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project tree <session-id>
  loom-cli project tree <project-root-id> --json
"#,
    ),
    (
        "project tasks",
        r#"Usage: loom-cli project tasks <session-id>

List the project's delegated tasks with status, owner, target, and intent.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project tasks <session-id>
  loom-cli project tasks <session-id> --json
"#,
    ),
    (
        "project children",
        r#"Usage: loom-cli project children <session-id>

List the direct child agents with their task, run, and blocking state.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project children <session-id>
  loom-cli project children <session-id> --json
"#,
    ),
    (
        "project messages",
        r#"Usage: loom-cli project messages <session-id> [--after <seq>] [--limit <n>]

Print one page of the project's agent message history. --limit is 1..512 and
100 by default.

Flags:
  --after <seq>               resume after that project message sequence
  --limit <n>                 the page size (1 to 512, 100 by default)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project messages <session-id>
  loom-cli project messages <session-id> --after 40 --limit 20 --json
"#,
    ),
    (
        "project send",
        r#"Usage: loom-cli project send <session-id> (--text <text> | --text-file <path>)
                              [--request-id <uuid>]

Direct the active run of a delegated child and print its state.

Flags:
  --text <text>               the direction to send
  --text-file <path>          read the direction from a file instead
  --request-id <uuid>         the idempotency key of the direction
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project send <child-session-id> --text 'prefer the smaller diff'
  loom-cli project send <child-session-id> --text-file direction.md --json
"#,
    ),
    (
        "project child",
        r#"Usage: loom-cli project child pause|resume|interrupt|cancel <task-id> --session <id>
                              [--request-id <uuid>]

Control one delegated child task. The manager session comes from the project
snapshot, never from a flag.

Flags:
  --session <id>              the project member session to read (required)
  --request-id <uuid>         the idempotency key of the control request
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project child pause <task-id> --session <session-id>
  loom-cli project child cancel <task-id> --session <session-id> --json
"#,
    ),
    (
        "project child pause",
        r#"Usage: loom-cli project child pause <task-id> --session <id> [--request-id <uuid>]

Pause one delegated child task; its run stops at the next safe point.

Flags:
  --session <id>              the project member session to read (required)
  --request-id <uuid>         the idempotency key of the control request
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project child pause <task-id> --session <session-id>
  loom-cli project child pause <task-id> --session <session-id> --json
"#,
    ),
    (
        "project child resume",
        r#"Usage: loom-cli project child resume <task-id> --session <id> [--request-id <uuid>]

Resume one paused delegated child task.

Flags:
  --session <id>              the project member session to read (required)
  --request-id <uuid>         the idempotency key of the control request
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project child resume <task-id> --session <session-id>
  loom-cli project child resume <task-id> --session <session-id> --json
"#,
    ),
    (
        "project child interrupt",
        r#"Usage: loom-cli project child interrupt <task-id> --session <id> [--request-id <uuid>]

Interrupt the run of one delegated child task, keeping the task and its
checkout.

Flags:
  --session <id>              the project member session to read (required)
  --request-id <uuid>         the idempotency key of the control request
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project child interrupt <task-id> --session <session-id>
  loom-cli project child interrupt <task-id> --session <session-id> --json
"#,
    ),
    (
        "project child cancel",
        r#"Usage: loom-cli project child cancel <task-id> --session <id> [--request-id <uuid>]

Cancel one delegated child task and its run.

Flags:
  --session <id>              the project member session to read (required)
  --request-id <uuid>         the idempotency key of the control request
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project child cancel <task-id> --session <session-id>
  loom-cli project child cancel <task-id> --session <session-id> --json
"#,
    ),
    (
        "project archive",
        r#"Usage: loom-cli project archive <session-id> [--request-id <uuid>]

Archive a project root with its idle members. The id must name such a root.

Flags:
  --request-id <uuid>         the idempotency key of the archive
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project archive <project-root-id>
  loom-cli project archive <project-root-id> --json
"#,
    ),
    (
        "project delete",
        r#"Usage: loom-cli project delete <session-id> [--force] [--yes] [--request-id <uuid>]

Delete a project root and its tree. The id must name such a root, and --yes is
required because deleting a project is irreversible.

Flags:
  --force                     discard dirty or locked worktrees
  --yes                       confirm the deletion
  --request-id <uuid>         the idempotency key of the deletion
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project delete <project-root-id> --yes
  loom-cli project delete <project-root-id> --force --yes --json
"#,
    ),
    (
        "project worktrees",
        r#"Usage: loom-cli project worktrees <session-id>

List the checkouts the project's snapshot records, in snapshot order.

Flags:
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project worktrees <project-root-id>
  loom-cli project worktrees <project-root-id> --json
"#,
    ),
    (
        "project worktree",
        r#"Usage: loom-cli project worktree get|diff|integrate|cleanup <session-id> --task <task-id>

Inspect, land, or clean up the checkout one delegated task records.

Sub-verbs:
  get                         the durable worktree record
  diff                        the worker's bounded review of the checkout
  integrate                   fast-forward the parent with the result revision
  cleanup                     keep, remove, or discard the checkout

Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project worktree diff <session-id> --task <task-id>
  loom-cli project worktree cleanup <session-id> --task <task-id> --keep
"#,
    ),
    (
        "project worktree get",
        r#"Usage: loom-cli project worktree get <session-id> --task <task-id>

Report one delegated task's durable worktree record without opening it.

Flags:
  --task <task-id>            the delegated task (required)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project worktree get <project-root-id> --task <task-id>
  loom-cli project worktree get <project-root-id> --task <task-id> --json
"#,
    ),
    (
        "project worktree diff",
        r#"Usage: loom-cli project worktree diff <session-id> --task <task-id>

Report the worker's bounded review of one checkout. The client never runs Git
and never re-bounds a diff it did not compute.

Flags:
  --task <task-id>            the delegated task (required)
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project worktree diff <project-root-id> --task <task-id>
  loom-cli project worktree diff <project-root-id> --task <task-id> --json
"#,
    ),
    (
        "project worktree integrate",
        r#"Usage: loom-cli project worktree integrate <session-id> --task <task-id>
                                    [--expected-parent-revision <rev>]
                                    [--request-id <uuid>]

Fast-forward the parent with the task's result revision. A conflict is
reported, never retried.

Flags:
  --task <task-id>            the delegated task (required)
  --expected-parent-revision <rev>
                              the parent tip the integration expects
  --request-id <uuid>         the idempotency key of the integration
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project worktree integrate <project-root-id> --task <task-id>
  loom-cli project worktree integrate <project-root-id> --task <task-id> --json
"#,
    ),
    (
        "project worktree cleanup",
        r#"Usage: loom-cli project worktree cleanup <session-id> --task <task-id>
                                  (--keep | --remove | --discard) [--yes]
                                  [--request-id <uuid>]

Keep or remove the checkout recorded for one delegated task. --discard throws
away uncommitted changes and needs --yes.

Flags:
  --task <task-id>            the delegated task (required)
  --keep                      retain the checkout
  --remove                    remove it; refused while it holds changes
  --discard                   remove it with those changes (needs --yes)
  --yes                       confirm a discard
  --request-id <uuid>         the idempotency key of the cleanup
  --json                      print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli project worktree cleanup <project-root-id> --task <task-id> --keep
  loom-cli project worktree cleanup <project-root-id> --task <task-id> --discard --yes --json
"#,
    ),
    (
        "jev",
        r#"Usage: loom-cli jev status|ask|triage [flags]

Call the Jev decision model for a supervision judgement this client makes
itself: whether a Jev key is configured, one raw choice question, or a
suggested verdict for the approval a session waits on. No core crate is
involved and the worker is never asked about Jev.

Sub-verbs:
  status                      the key, endpoint, and model a call would use
  ask                         one choice question, with its probabilities
  triage --session <id>       a verdict for the pending approval of a session

Flags:
  --source <opencode|typesafe>  the endpoint and model, and the default key
  --key-file <path>             the key file (default ~/.config/loom/<source>.key)
  --endpoint <url>              override the endpoint
  --model <id>                  override the model
  --json                        print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli jev --help
  loom-cli jev status --json
"#,
    ),
    (
        "jev status",
        r#"Usage: loom-cli jev status [--source <opencode|typesafe>] [--key-file <path>]
                            [--endpoint <url>] [--model <id>] [--json]

Report whether a Jev key is configured, the path it is read from, and the
endpoint and model a call would use. The key itself is never printed, and this
verb needs neither a worker nor a token.

Flags:
  --source <opencode|typesafe>  the endpoint and model, and the default key
  --key-file <path>             the key file (default ~/.config/loom/<source>.key)
  --endpoint <url>              override the endpoint
  --model <id>                  override the model
  --json                        print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli jev status
  loom-cli jev status --source opencode --json
"#,
    ),
    (
        "jev ask",
        r#"Usage: loom-cli jev ask (--state <text> | --state-file <path>)
                 --question <text>
                 (--option <label>[=<when it applies>])...
                 [--source <opencode|typesafe>] [--key-file <path>]
                 [--endpoint <url>] [--model <id>] [--json]

Ask the decision model one choice question over a bounded state, and print the
chosen label, its confidence, and the full probability distribution. At least
two --option labels are required; the text after `=` says when that option
applies, which is how the service is asked to interpret each label.

Flags:
  --state <text>                the situation, as compactly as possible
  --state-file <path>           the same state, read from a file
  --question <text>             the instruction for the choice
  --option <label>[=<when>]     one label; repeat for each option
  --source <opencode|typesafe>  the endpoint and model, and the default key
  --key-file <path>             the key file (default ~/.config/loom/<source>.key)
  --endpoint <url>              override the endpoint
  --model <id>                  override the model
  --json                        print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli jev ask --state 'The build passes.' --question 'Is the change safe?' \
    --option approve='safe to merge' --option ask_user='a person must look'
  loom-cli jev ask --state-file /tmp/state.txt --question 'Which crate?' \
    --option cli --option server --json
"#,
    ),
    (
        "jev triage",
        r#"Usage: loom-cli jev triage --session <session-id> [--apply]
                           [--threshold <0..1>] [--request-id <uuid>]
                           [--source <opencode|typesafe>] [--key-file <path>]
                           [--endpoint <url>] [--model <id>] [--json]

Read the approval a session waits on, ask the decision model for a verdict, and
print it with its confidence. It never approves anything by itself: --apply
approves the pending call only when the verdict is approve, its confidence is
at or above --threshold (0.9 by default), the call is a read tool, and every
path argument resolves inside the session root.

Flags:
  --session <session-id>        the session whose approval is triaged
  --apply                       approve the call when every guard passes
  --threshold <0..1>            the confidence --apply requires (default 0.9)
  --request-id <uuid>           the idempotency key of the approval
  --source <opencode|typesafe>  the endpoint and model, and the default key
  --key-file <path>             the key file (default ~/.config/loom/<source>.key)
  --endpoint <url>              override the endpoint
  --model <id>                  override the model
  --json                        print one JSON envelope instead of text
Global flags are accepted anywhere; see 'loom-cli --help'.

Examples:
  loom-cli jev triage --session <session-id>
  loom-cli jev triage --session <session-id> --apply --threshold 0.95 --json
"#,
    ),
];

/// The help text a command line asking for help prints: the named verb's page
/// when the table knows it, the global manual otherwise.
pub(crate) fn help_text(arguments: &[String]) -> &'static str {
    let words = verb_words(arguments);
    for length in (1..=words.len()).rev() {
        let key = words[..length].join(" ");
        if let Some((_, text)) = VERB_HELP.iter().find(|(name, _)| *name == key) {
            return text;
        }
    }
    HELP
}

/// The first three positional words of a command line, so help can name the verb
/// (`project worktree integrate`) even when the rest of the line would be
/// rejected. A flag's value is skipped for the flags whose arity says it takes
/// one, so `--session <id>` never becomes a verb word.
fn verb_words(arguments: &[String]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut skip_value = false;
    for argument in arguments {
        if skip_value {
            skip_value = false;
            continue;
        }
        if argument.starts_with('-') {
            // `--flag=value` already carries its value.
            skip_value = !argument.contains('=') && arity(argument).unwrap_or(false);
            continue;
        }
        words.push(argument.as_str());
        if words.len() == 3 {
            break;
        }
    }
    words
}

/// Every flag the parser knows, with whether it takes a value.
fn arity(name: &str) -> Option<bool> {
    Some(match name {
        "--connect" | "--token-file" | "--token" | "--ca-file" | "--max-frame-bytes" => true,
        "--allow-insecure-remote" | "--json" | "--help" | "-h" => false,
        "--workspace" | "--session" | "--after" | "--cursor-file" | "--format" | "--max-bytes"
        | "--lines" | "--limit" => true,
        "--archived" | "--metadata" | "--follow" => false,
        "--task" | "--task-file" | "--name" | "--model" | "--repo" | "--revision" | "--path"
        | "--as-directory" | "--approval-policy" | "--request-id" | "--text-file" => true,
        "--run" | "--text" | "--tool-call" | "--reason" | "--until" | "--timeout" => true,
        "--window" | "--force-discard" | "--sub-agent" => true,
        "--provider" | "--from-env" | "--key-file" | "--key" | "--api-key" => true,
        "--id" | "--kind" | "--endpoint" | "--default-model" => true,
        "--expected-parent-revision" | "--project" => true,
        "--new" | "--archive" | "--retry-on-conflict" | "--force" | "--yes" | "--tree" => false,
        "--keep" | "--remove" | "--discard" => false,
        "--child-model" | "--child-model-for" | "--unset-kind" => true,
        "--no-child-model" | "--clear" => false,
        "--source" | "--endpoint" | "--state" | "--state-file" | "--question" | "--option"
        | "--threshold" => true,
        "--apply" => false,
        _ => return None,
    })
}

struct Flag {
    name: String,
    value: Option<String>,
}

/// Collects the flags of one verb and reports any the verb does not use.
struct FlagSet {
    verb: String,
    flags: Vec<Flag>,
}

impl FlagSet {
    fn value(&mut self, name: &str) -> Option<String> {
        let index = self.flags.iter().position(|flag| flag.name == name)?;
        self.flags.remove(index).value
    }

    /// Every value of a repeatable flag, in command-line order.
    fn values(&mut self, name: &str) -> Vec<String> {
        let mut values = Vec::new();
        while let Some(index) = self.flags.iter().position(|flag| flag.name == name) {
            values.extend(self.flags.remove(index).value);
        }
        values
    }

    fn required(&mut self, name: &str) -> Result<String, String> {
        self.value(name)
            .ok_or_else(|| format!("{} requires {name} <value>", self.verb))
    }

    fn flag(&mut self, name: &str) -> bool {
        let Some(index) = self.flags.iter().position(|flag| flag.name == name) else {
            return false;
        };
        self.flags.remove(index);
        true
    }

    fn finish(self) -> Result<(), String> {
        match self.flags.first() {
            Some(flag) => Err(format!(
                "flag '{}' does not apply to '{}'",
                flag.name, self.verb
            )),
            None => Ok(()),
        }
    }
}

pub(crate) fn parse(arguments: &[String]) -> Result<Parsed, String> {
    // Help wins over everything else on the line, including an argument that
    // would otherwise be rejected, so asking for help always explains the
    // surface instead of failing on one token. A line that names a verb gets
    // that verb's own page; one that names none, or names an unknown verb,
    // gets the global manual.
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        return Ok(Parsed::Help(help_text(arguments)));
    }
    let (flags, positionals) = split(arguments)?;
    let mut globals = Globals::default();
    let mut verb_flags = Vec::new();
    for flag in flags {
        match flag.name.as_str() {
            "--connect" => globals.connect = Some(flag.value.unwrap_or_default()),
            "--token-file" => {
                globals.token_file = Some(PathBuf::from(flag.value.unwrap_or_default()));
            }
            "--ca-file" => globals.ca_file = Some(PathBuf::from(flag.value.unwrap_or_default())),
            "--allow-insecure-remote" => globals.allow_insecure_remote = true,
            "--json" => globals.json = true,
            "--max-frame-bytes" => {
                let value = flag.value.unwrap_or_default();
                let bytes = value.parse::<usize>().map_err(|_| {
                    format!("--max-frame-bytes must be a positive byte count, got '{value}'")
                })?;
                if bytes == 0 {
                    return Err("--max-frame-bytes must be greater than zero".to_owned());
                }
                globals.max_frame_bytes = Some(bytes);
            }
            _ => verb_flags.push(flag),
        }
    }

    let Some(name) = positionals.first() else {
        return Err("missing verb; try --help".to_owned());
    };
    let rest = &positionals[1..];
    let mut flags = FlagSet {
        verb: name.clone(),
        flags: verb_flags,
    };
    let verb = match name.as_str() {
        "doctor" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Doctor
        }
        "node" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Node
        }
        "whoami" => {
            no_positionals(&flags.verb, rest)?;
            Verb::WhoAmI
        }
        "ls" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Ls {
                workspace: optional_id(flags.value("--workspace"))?,
                archived: flags.flag("--archived"),
            }
        }
        "get" => {
            let Some(session) = rest.first() else {
                return Err("get requires a <session-id>".to_owned());
            };
            if rest.len() > 1 {
                return Err(format!("get takes one session id, got '{}'", rest[1]));
            }
            Verb::Get {
                session: parse_id("session id", session)?,
                metadata: flags.flag("--metadata"),
            }
        }
        "events" => {
            no_positionals(&flags.verb, rest)?;
            let session = flags.required("--session")?;
            let after =
                match flags.value("--after") {
                    Some(value) => Some(value.parse::<u64>().map_err(|_| {
                        format!("--after must be an event sequence, got '{value}'")
                    })?),
                    None => None,
                };
            // `--timeout` bounds a follow only. A plain read already ends at the
            // journal head, so the flag there would promise a bound it does not
            // need and is refused instead of ignored.
            let follow = flags.flag("--follow");
            let timeout_secs = match flags.value("--timeout") {
                None => None,
                Some(value) => {
                    let seconds = value
                        .parse::<u64>()
                        .map_err(|_| format!("--timeout must be a second count, got '{value}'"))?;
                    if seconds == 0 {
                        return Err("--timeout must be greater than zero".to_owned());
                    }
                    if !follow {
                        return Err(
                            "--timeout bounds events --follow; a plain read already ends at the \
                             journal head"
                                .to_owned(),
                        );
                    }
                    Some(seconds)
                }
            };
            Verb::Events(EventsArgs {
                session: parse_id("session id", &session)?,
                after,
                cursor_file: flags.value("--cursor-file").map(PathBuf::from),
                follow,
                timeout_secs,
                format: parse_format(flags.value("--format"))?,
                max_bytes: parse_max_bytes(flags.value("--max-bytes"))?,
            })
        }
        "peek" => {
            no_positionals(&flags.verb, rest)?;
            let session = flags.required("--session")?;
            let lines = match flags.value("--lines") {
                Some(value) => {
                    let lines = value
                        .parse::<u32>()
                        .map_err(|_| format!("--lines must be a count, got '{value}'"))?;
                    if lines == 0 {
                        return Err("--lines must be greater than zero".to_owned());
                    }
                    lines
                }
                None => DEFAULT_LINES,
            };
            Verb::Peek(PeekArgs {
                session: parse_id("session id", &session)?,
                lines,
                format: parse_format(flags.value("--format"))?,
                max_bytes: parse_max_bytes(flags.value("--max-bytes"))?,
            })
        }
        "run" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Run(Box::new(parse_run(&mut flags)?))
        }
        "send" => {
            no_positionals(&flags.verb, rest)?;
            let request_id = parse_request_id(&mut flags)?;
            Verb::Send {
                run: parse_id("run id", &flags.required("--run")?)?,
                text: flags.required("--text")?,
                request_id,
            }
        }
        "answer" => {
            no_positionals(&flags.verb, rest)?;
            let request_id = parse_request_id(&mut flags)?;
            Verb::Answer {
                run: parse_id("run id", &flags.required("--run")?)?,
                text: flags.required("--text")?,
                request_id,
            }
        }
        "approve" => {
            no_positionals(&flags.verb, rest)?;
            let request_id = parse_request_id(&mut flags)?;
            Verb::Approve {
                run: parse_id("run id", &flags.required("--run")?)?,
                tool_call: parse_id("tool call id", &flags.required("--tool-call")?)?,
                request_id,
            }
        }
        "deny" => {
            no_positionals(&flags.verb, rest)?;
            let request_id = parse_request_id(&mut flags)?;
            Verb::Deny {
                run: parse_id("run id", &flags.required("--run")?)?,
                tool_call: parse_id("tool call id", &flags.required("--tool-call")?)?,
                reason: flags.value("--reason"),
                request_id,
            }
        }
        "stop" => {
            no_positionals(&flags.verb, rest)?;
            let request_id = parse_request_id(&mut flags)?;
            let archive = flags.value("--session");
            let archive = match (flags.flag("--archive"), archive) {
                (true, Some(session)) => Some(parse_id("session id", &session)?),
                (true, None) => {
                    return Err("stop --archive requires --session <session-id>".to_owned());
                }
                (false, Some(_)) => {
                    return Err("--session only applies to stop with --archive".to_owned());
                }
                (false, None) => None,
            };
            Verb::Stop {
                run: parse_id("run id", &flags.required("--run")?)?,
                archive,
                retry_on_conflict: flags.flag("--retry-on-conflict"),
                request_id,
            }
        }
        "wait" => {
            no_positionals(&flags.verb, rest)?;
            let session = flags.required("--session")?;
            let until = match flags.value("--until").as_deref() {
                None | Some("blocked") => WaitUntil::Blocked,
                Some("terminal") => WaitUntil::Terminal,
                Some(other) => {
                    return Err(format!(
                        "--until must be blocked or terminal, got '{other}'"
                    ));
                }
            };
            let timeout_secs = match flags.value("--timeout") {
                None => DEFAULT_WAIT_TIMEOUT_SECS,
                Some(value) => {
                    let seconds = value
                        .parse::<u64>()
                        .map_err(|_| format!("--timeout must be a second count, got '{value}'"))?;
                    if seconds == 0 {
                        return Err("--timeout must be greater than zero".to_owned());
                    }
                    seconds
                }
            };
            Verb::Wait {
                session: parse_id("session id", &session)?,
                until,
                timeout_secs,
            }
        }
        "status" => {
            no_positionals(&flags.verb, rest)?;
            let session = flags.required("--session")?;
            let format = match flags.value("--format").as_deref() {
                None | Some("text") => StatusFormat::Text,
                Some("verb") => StatusFormat::Verb,
                Some(other) => {
                    return Err(format!("--format must be text or verb, got '{other}'"));
                }
            };
            let tree = flags.flag("--tree");
            if tree && format == StatusFormat::Verb {
                return Err(
                    "--tree reports a project, so use --format text to print its lines".to_owned(),
                );
            }
            Verb::Status {
                session: parse_id("session id", &session)?,
                format,
                tree,
            }
        }
        "liveness" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Liveness {
                session: parse_id("session id", &flags.required("--session")?)?,
            }
        }
        "result" => {
            no_positionals(&flags.verb, rest)?;
            let session = flags.required("--session")?;
            let task = match flags.value("--task") {
                Some(value) => Some(parse_id("task id", &value)?),
                None => None,
            };
            Verb::Result {
                session: parse_id("session id", &session)?,
                task,
            }
        }
        "retention" => parse_retention(&mut flags, rest)?,
        "providers" => parse_providers(&mut flags, rest)?,
        "models" => parse_models(&mut flags, rest)?,
        "jev" => parse_jev(&mut flags, rest)?,
        "session" => parse_session(&mut flags, rest)?,
        "project" => parse_project(&mut flags, rest)?,
        "delete" => {
            let Some(session) = rest.first() else {
                return Err("delete requires a <session-id>".to_owned());
            };
            if rest.len() > 1 {
                return Err(format!("delete takes one session id, got '{}'", rest[1]));
            }
            let force = flags.flag("--force");
            let yes = flags.flag("--yes");
            if force && !yes {
                return Err(
                    "delete --force discards dirty or locked worktrees; pass --yes to confirm"
                        .to_owned(),
                );
            }
            Verb::Delete {
                session: parse_id("session id", session)?,
                force,
                yes,
                request_id: parse_request_id(&mut flags)?,
            }
        }
        other => return Err(format!("unknown verb '{other}'; try --help")),
    };
    flags.finish()?;
    Ok(Parsed::Run(Invocation { globals, verb }))
}

/// Splits the arguments into flags and positional words, rejecting an unknown
/// flag, a missing value, and the command-line token.
fn split(arguments: &[String]) -> Result<(Vec<Flag>, Vec<String>), String> {
    let mut flags = Vec::new();
    let mut positionals = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        if !argument.starts_with('-') {
            positionals.push(argument.clone());
            continue;
        }
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) if name.starts_with('-') => {
                (name.to_owned(), Some(value.to_owned()))
            }
            _ => (argument.clone(), None),
        };
        let Some(takes_value) = arity(&name) else {
            return Err(format!("unknown flag '{name}'"));
        };
        if name == "--token" {
            return Err(
                "the token must come from a file: --token is not accepted, use \
                 --token-file <path> or LOOM_TOKEN_FILE"
                    .to_owned(),
            );
        }
        if name == "--key" || name == "--api-key" {
            return Err(
                "a provider API key must not be passed on the command line; use \
                 --from-env <NAME> or --key-file <path>"
                    .to_owned(),
            );
        }
        if takes_value {
            let value = match inline {
                Some(value) if !value.is_empty() => value,
                Some(_) => return Err(format!("{name} requires a value")),
                None => {
                    let Some(value) = arguments.get(index) else {
                        return Err(format!("{name} requires a value"));
                    };
                    index += 1;
                    value.clone()
                }
            };
            flags.push(Flag {
                name,
                value: Some(value),
            });
        } else {
            if inline.is_some() {
                return Err(format!("{name} does not take a value"));
            }
            flags.push(Flag { name, value: None });
        }
    }
    Ok((flags, positionals))
}

fn parse_id<T: FromStr>(what: &str, value: &str) -> Result<T, String> {
    value
        .parse::<T>()
        .map_err(|_| format!("'{value}' is not a valid {what}"))
}

/// The idempotency key `--request-id` names, or `None` when the flag was
/// absent, in which case the mutation uses a fresh id and is a new operation.
fn parse_request_id(flags: &mut FlagSet) -> Result<Option<RequestId>, String> {
    match flags.value("--request-id") {
        Some(value) => Ok(Some(value.parse::<RequestId>().map_err(|_| {
            format!("'{value}' is not a valid request id; --request-id takes a UUID")
        })?)),
        None => Ok(None),
    }
}

/// The `retention` sub-verbs: `get` reads the policy, `set` merges the given
/// fields into it. No flag on `set` means there is nothing to store.
fn parse_retention(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err("retention requires 'get' or 'set'".to_owned());
    };
    if rest.len() > 1 {
        return Err(format!("retention takes one sub-verb, got '{}'", rest[1]));
    }
    flags.verb = format!("retention {sub_verb}");
    match sub_verb.as_str() {
        "get" => Ok(Verb::RetentionGet),
        "set" => {
            let window_ms = match flags.value("--window") {
                Some(value) => Some(
                    loom_core::parse_retention_window(&value, "--window")
                        .map_err(|error| error.message)?,
                ),
                None => None,
            };
            let force_discard_worktrees = match flags.value("--force-discard") {
                Some(value) => Some(
                    loom_core::parse_force_discard(&value, "--force-discard")
                        .map_err(|error| error.message)?,
                ),
                None => None,
            };
            let sub_agent_ms = match flags.value("--sub-agent") {
                Some(value) => Some(
                    loom_core::parse_retention_window(&value, "--sub-agent")
                        .map_err(|error| error.message)?,
                ),
                None => None,
            };
            if window_ms.is_none() && force_discard_worktrees.is_none() && sub_agent_ms.is_none() {
                return Err(
                    "retention set requires --window, --force-discard, or --sub-agent".to_owned(),
                );
            }
            Ok(Verb::RetentionSet(RetentionSetArgs {
                window_ms,
                force_discard_worktrees,
                sub_agent_ms,
            }))
        }
        other => Err(format!(
            "unknown retention sub-verb '{other}'; use get or set"
        )),
    }
}

/// The `providers` sub-verbs. `list` takes no flags. `set-key` takes the
/// provider id and exactly one of `--from-env` or `--key-file`. `add`,
/// `remove`, and `discover` manage an operator-supplied OpenAI-compatible
/// provider and its model catalog.
fn parse_providers(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err(
            "providers requires 'list', 'add', 'remove', 'discover', or 'set-key'".to_owned(),
        );
    };
    if rest.len() > 1 {
        return Err(format!("providers takes one sub-verb, got '{}'", rest[1]));
    }
    flags.verb = format!("providers {sub_verb}");
    match sub_verb.as_str() {
        "list" => Ok(Verb::ProvidersList),
        "set-key" => {
            let provider = provider_id(&flags.required("--provider")?)?;
            let from_env = flags.value("--from-env");
            let key_file = flags.value("--key-file");
            let source = match (from_env, key_file) {
                (Some(name), None) => ProviderKeySource::Env(env_var_name(&name)?),
                (None, Some(path)) => {
                    if path.trim().is_empty() {
                        return Err("--key-file requires a path".to_owned());
                    }
                    ProviderKeySource::File(PathBuf::from(path))
                }
                (Some(_), Some(_)) => {
                    return Err(
                        "providers set-key accepts --from-env or --key-file, not both".to_owned(),
                    );
                }
                (None, None) => {
                    return Err(
                        "providers set-key requires --from-env <NAME> or --key-file <path>"
                            .to_owned(),
                    );
                }
            };
            Ok(Verb::ProvidersSetKey(ProviderSetKeyArgs {
                provider,
                source,
            }))
        }
        "add" => parse_providers_add(flags),
        "remove" => {
            let provider = provider_id(&flags.required("--id")?)?;
            Ok(Verb::ProvidersRemove { provider })
        }
        "discover" => {
            let provider = provider_id(&flags.required("--provider")?)?;
            Ok(Verb::ProvidersDiscover { provider })
        }
        other => Err(format!(
            "unknown providers sub-verb '{other}'; use list, add, remove, discover, or set-key"
        )),
    }
}

/// `providers add`: an OpenAI-compatible provider the operator supplies.
///
/// Only `--kind openai-compatible` is accepted, because every other provider
/// kind is seeded by the worker. At least one `--model` is required, and
/// `--default-model` must name one of them, so a provider never registers with
/// a model `run` cannot start.
fn parse_providers_add(flags: &mut FlagSet) -> Result<Verb, String> {
    let kind = flags.required("--kind")?;
    if !kind
        .trim()
        .replace('_', "-")
        .eq_ignore_ascii_case("openai-compatible")
    {
        return Err(format!(
            "providers add supports --kind openai-compatible, got '{}'",
            kind.trim()
        ));
    }
    let provider = provider_id(&flags.required("--id")?)?;
    let endpoint = flags.required("--endpoint")?;
    let endpoint = endpoint.trim().to_owned();
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        || endpoint
            .split_once("://")
            .is_none_or(|(_, rest)| rest.trim().is_empty())
    {
        return Err(format!(
            "--endpoint must be an http:// or https:// URL, got '{endpoint}'"
        ));
    }
    let mut models: Vec<String> = Vec::new();
    for model in flags.values("--model") {
        let model = model.trim().to_owned();
        if model.is_empty() || models.contains(&model) {
            continue;
        }
        models.push(model);
    }
    if models.is_empty() {
        return Err(
            "providers add requires at least one --model <id>; an OpenAI-compatible provider \
             needs the models it serves"
                .to_owned(),
        );
    }
    let default_model = flags
        .value("--default-model")
        .map(|model| model.trim().to_owned());
    if let Some(default) = &default_model
        && !models.contains(default)
    {
        return Err(format!(
            "--default-model '{default}' is not one of the --model ids; the default is stored \
             first, so it must be one of them"
        ));
    }
    Ok(Verb::ProvidersAdd(ProviderAddArgs {
        provider,
        endpoint,
        models,
        default_model,
    }))
}

/// The `models` sub-verbs. `list` takes the optional exact provider id.
fn parse_models(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err("models requires 'list'".to_owned());
    };
    if rest.len() > 1 {
        return Err(format!("models takes one sub-verb, got '{}'", rest[1]));
    }
    flags.verb = format!("models {sub_verb}");
    match sub_verb.as_str() {
        "list" => {
            let provider = flags
                .value("--provider")
                .map(|value| provider_id(&value))
                .transpose()?;
            Ok(Verb::ModelsList { provider })
        }
        other => Err(format!("unknown models sub-verb '{other}'; use list")),
    }
}

/// The `jev` sub-verbs: the supervision helper that calls the decision model
/// directly instead of through the worker.
///
/// `status` and `ask` are local, so they never name a session and never need a
/// token; `triage` reads one session's approval from the worker and is the only
/// sub-verb that negotiates a capability.
fn parse_jev(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err("jev requires 'status', 'ask', or 'triage'".to_owned());
    };
    if rest.len() > 1 {
        return Err(format!("jev takes one sub-verb, got '{}'", rest[1]));
    }
    flags.verb = format!("jev {sub_verb}");
    match sub_verb.as_str() {
        "status" => Ok(Verb::JevStatus(jev_flags(flags)?)),
        "ask" => {
            let state = match (flags.value("--state"), flags.value("--state-file")) {
                (Some(text), None) => text,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .map_err(|error| format!("could not read the state file '{path}': {error}"))?,
                (Some(_), Some(_)) => {
                    return Err("jev ask accepts --state or --state-file, not both".to_owned());
                }
                (None, None) => {
                    return Err("jev ask requires --state <text> or --state-file <path>".to_owned());
                }
            };
            if state.trim().is_empty() {
                return Err("the state to ask about must not be empty".to_owned());
            }
            if state.len() > MAX_JEV_STATE_BYTES {
                return Err(format!(
                    "the state is {} bytes, over the {MAX_JEV_STATE_BYTES} the endpoint accepts",
                    state.len()
                ));
            }
            let question = flags.required("--question")?;
            if question.trim().is_empty() {
                return Err("--question must not be empty".to_owned());
            }
            let options = parse_jev_options(flags.values("--option"))?;
            Ok(Verb::JevAsk(Box::new(JevAskArgs {
                state,
                question,
                options,
                flags: jev_flags(flags)?,
            })))
        }
        "triage" => {
            let session = parse_id("session id", &flags.required("--session")?)?;
            let threshold = match flags.value("--threshold") {
                None => DEFAULT_JEV_THRESHOLD,
                Some(value) => {
                    let threshold = value.parse::<f64>().map_err(|_| {
                        format!("--threshold must be a number between 0 and 1, got '{value}'")
                    })?;
                    // A NaN fails the range test, so it is refused as well.
                    if !(0.0..=1.0).contains(&threshold) {
                        return Err(format!(
                            "--threshold must be between 0 and 1, got '{value}'"
                        ));
                    }
                    threshold
                }
            };
            Ok(Verb::JevTriage(Box::new(JevTriageArgs {
                session,
                flags: jev_flags(flags)?,
                apply: flags.flag("--apply"),
                threshold,
                request_id: parse_request_id(flags)?,
            })))
        }
        other => Err(format!(
            "unknown jev sub-verb '{other}'; use status, ask, or triage"
        )),
    }
}

/// The flags every `jev` sub-verb shares. The key is not read here: the path is,
/// so a parsed command never carries a secret.
fn jev_flags(flags: &mut FlagSet) -> Result<JevFlags, String> {
    let source = flags.value("--source");
    if let Some(source) = &source
        && !matches!(source.as_str(), "typesafe" | "opencode")
    {
        return Err(format!(
            "--source must be typesafe or opencode, got '{source}'"
        ));
    }
    let key_file = flags.value("--key-file").map(PathBuf::from);
    if let Some(path) = &key_file
        && path.as_os_str().is_empty()
    {
        return Err("--key-file requires a path".to_owned());
    }
    let endpoint = flags.value("--endpoint");
    if let Some(endpoint) = &endpoint
        && !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
    {
        return Err(format!(
            "--endpoint must be an http or https URL, got '{endpoint}'"
        ));
    }
    let model = flags.value("--model");
    if let Some(model) = &model
        && model.trim().is_empty()
    {
        return Err("--model must not be empty".to_owned());
    }
    Ok(JevFlags {
        source,
        key_file,
        endpoint,
        model,
    })
}

/// The `--option` labels of `jev ask`, each with the condition it applies under.
fn parse_jev_options(values: Vec<String>) -> Result<Vec<(String, String)>, String> {
    if values.len() < 2 {
        return Err(
            "jev ask requires at least two --option <label>[=<when it applies>]".to_owned(),
        );
    }
    if values.len() > MAX_JEV_OPTIONS {
        return Err(format!(
            "jev ask accepts at most {MAX_JEV_OPTIONS} options, got {}",
            values.len()
        ));
    }
    let mut options: Vec<(String, String)> = Vec::new();
    for value in values {
        let (label, when) = match value.split_once('=') {
            Some((label, when)) => (label.trim().to_owned(), when.trim().to_owned()),
            None => (value.trim().to_owned(), String::new()),
        };
        if label.is_empty() {
            return Err("a --option label must not be empty".to_owned());
        }
        if options.iter().any(|(existing, _)| *existing == label) {
            return Err(format!("--option '{label}' was given more than once"));
        }
        options.push((label, when));
    }
    Ok(options)
}

/// A provider id is the worker's id, such as `openai`. Blank is a usage error;
/// the worker decides whether the id names an API-key provider.
fn provider_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("--provider must not be empty".to_owned());
    }
    Ok(value.to_owned())
}

/// `--from-env` names a variable. The name is checked here so a typo fails
/// before the process environment is read, and the value is never parsed as
/// part of the command line.
fn env_var_name(value: &str) -> Result<String, String> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err("--from-env requires a variable name".to_owned());
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || chars.any(|character| !(character.is_ascii_alphanumeric() || character == '_'))
    {
        return Err(format!(
            "'{value}' is not an environment variable name; --from-env takes a name such as \
             OPENAI_API_KEY"
        ));
    }
    Ok(value.to_owned())
}

/// The `session` sub-verbs: `archive` keeps a session and its checkouts,
/// `delete` removes it permanently.
fn parse_session(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err("session requires 'archive' or 'delete'".to_owned());
    };
    flags.verb = format!("session {sub_verb}");
    match sub_verb.as_str() {
        "archive" => {
            let request_id = parse_request_id(flags)?;
            let session = one_session_id(flags, rest)?;
            Ok(Verb::SessionArchive {
                session,
                yes: flags.flag("--yes"),
                request_id,
            })
        }
        "delete" => {
            let request_id = parse_request_id(flags)?;
            let session = one_session_id(flags, rest)?;
            let force = flags.flag("--force");
            let yes = flags.flag("--yes");
            if force && !yes {
                return Err(
                    "session delete --force discards dirty or locked worktrees; pass --yes to \
                     confirm"
                        .to_owned(),
                );
            }
            Ok(Verb::SessionDelete {
                session,
                force,
                yes,
                request_id,
            })
        }
        other => Err(format!(
            "unknown session sub-verb '{other}'; use archive or delete"
        )),
    }
}

/// The `project` sub-verbs: the reads addressed by any project member session,
/// the direction to a delegated child's active run, the child control
/// addressed by a task id the snapshot names, the project-scoped `archive` and
/// `delete`, and the `worktrees`/`worktree` verbs that read and act on the
/// checkouts the snapshot records.
fn parse_project(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err(
            "project requires 'get', 'tree', 'tasks', 'children', 'messages', 'send', \
             'child', 'archive', 'delete', 'worktrees', 'worktree', or 'models'"
                .to_owned(),
        );
    };
    flags.verb = format!("project {sub_verb}");
    match sub_verb.as_str() {
        "get" | "tree" | "tasks" | "children" => {
            let session = one_session_id(flags, rest)?;
            match sub_verb.as_str() {
                "get" => Ok(Verb::ProjectGet { session }),
                "tree" => Ok(Verb::ProjectTree { session }),
                "tasks" => Ok(Verb::ProjectTasks { session }),
                _ => Ok(Verb::ProjectChildren { session }),
            }
        }
        "messages" => {
            let session = one_session_id(flags, rest)?;
            let after = match flags.value("--after") {
                Some(value) => Some(value.parse::<u64>().map_err(|_| {
                    format!("--after must be a project message sequence, got '{value}'")
                })?),
                None => None,
            };
            let limit = match flags.value("--limit") {
                None => DEFAULT_MESSAGE_LIMIT,
                Some(value) => {
                    let limit = value
                        .parse::<u32>()
                        .map_err(|_| format!("--limit must be a message count, got '{value}'"))?;
                    if !(1..=MAX_MESSAGE_LIMIT).contains(&limit) {
                        return Err(format!(
                            "--limit must be between 1 and {MAX_MESSAGE_LIMIT}, got {limit}"
                        ));
                    }
                    limit
                }
            };
            Ok(Verb::ProjectMessages {
                session,
                after,
                limit,
            })
        }
        "send" => {
            let request_id = parse_request_id(flags)?;
            let session = one_session_id(flags, rest)?;
            let text = match (flags.value("--text"), flags.value("--text-file")) {
                (Some(_), Some(_)) => {
                    return Err("project send accepts --text or --text-file, not both".to_owned());
                }
                (Some(text), None) => text,
                (None, Some(path)) => read_input_file("text", &path)?,
                (None, None) => {
                    return Err(
                        "project send requires --text <text> or --text-file <path>".to_owned()
                    );
                }
            };
            Ok(Verb::ProjectSend {
                session,
                text,
                request_id,
            })
        }
        "child" => {
            let Some(action_word) = rest.get(1) else {
                return Err("project child requires pause, resume, interrupt, or cancel".to_owned());
            };
            let action = match action_word.as_str() {
                "pause" => ProjectChildAction::Pause,
                "resume" => ProjectChildAction::Resume,
                "interrupt" => ProjectChildAction::Interrupt,
                "cancel" => ProjectChildAction::Cancel,
                other => {
                    return Err(format!(
                        "unknown project child action '{other}'; use pause, resume, interrupt, or \
                         cancel"
                    ));
                }
            };
            flags.verb = format!("project child {}", action.word());
            let Some(task) = rest.get(2) else {
                return Err(format!("{} requires a <task-id>", flags.verb));
            };
            if rest.len() > 3 {
                return Err(format!(
                    "{} takes one task id, got '{}'",
                    flags.verb, rest[3]
                ));
            }
            Ok(Verb::ProjectChild {
                session: parse_id("session id", &flags.required("--session")?)?,
                task: parse_id("task id", task)?,
                action,
                request_id: parse_request_id(flags)?,
            })
        }
        "archive" => Ok(Verb::ProjectArchive {
            session: one_session_id(flags, rest)?,
            request_id: parse_request_id(flags)?,
        }),
        "delete" => {
            let request_id = parse_request_id(flags)?;
            let session = one_session_id(flags, rest)?;
            let force = flags.flag("--force");
            let yes = flags.flag("--yes");
            if force && !yes {
                return Err(
                    "project delete --force discards dirty or locked worktrees; pass --yes to \
                     confirm"
                        .to_owned(),
                );
            }
            Ok(Verb::ProjectDelete {
                session,
                force,
                yes,
                request_id,
            })
        }
        "worktrees" => Ok(Verb::ProjectWorktrees {
            session: one_session_id(flags, rest)?,
        }),
        "worktree" => parse_project_worktree(flags, rest),
        "models" => parse_project_models(flags, rest),
        other => Err(format!(
            "unknown project sub-verb '{other}'; use get, tree, tasks, children, messages, send, \
             child, archive, delete, worktrees, worktree, or models"
        )),
    }
}

/// The `project models` sub-verbs: `get` and `set`.
fn parse_project_models(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.get(1) else {
        return Err("project models requires 'get' or 'set'".to_owned());
    };
    flags.verb = format!("project models {sub_verb}");
    match sub_verb.as_str() {
        "get" => Ok(Verb::ProjectModelsGet {
            session: one_session_id(flags, rest)?,
        }),
        "set" => {
            let request_id = parse_request_id(flags)?;
            let session = one_session_id(flags, rest)?;
            let changes = parse_child_model_changes(flags, true)?;
            if changes.is_empty() {
                return Err(
                    "project models set requires --child-model, --no-child-model, \
                     --child-model-for, --unset-kind, or --clear"
                        .to_owned(),
                );
            }
            Ok(Verb::ProjectModelsSet {
                session,
                changes,
                request_id,
            })
        }
        other => Err(format!(
            "unknown project models sub-verb '{other}'; use get or set"
        )),
    }
}

/// The `project worktree` sub-verbs: `get`, `diff`, `integrate`, and `cleanup`.
fn parse_project_worktree(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.get(1) else {
        return Err(
            "project worktree requires 'get', 'diff', 'integrate', or 'cleanup'".to_owned(),
        );
    };
    flags.verb = format!("project worktree {sub_verb}");
    match sub_verb.as_str() {
        "get" => {
            let session = one_session_id(flags, rest)?;
            Ok(Verb::ProjectWorktreeGet {
                session,
                task: parse_id("task id", &flags.required("--task")?)?,
            })
        }
        "diff" => {
            let session = one_session_id(flags, rest)?;
            Ok(Verb::ProjectWorktreeDiff {
                session,
                task: parse_id("task id", &flags.required("--task")?)?,
            })
        }
        "integrate" => {
            let request_id = parse_request_id(flags)?;
            let session = one_session_id(flags, rest)?;
            let task = flags.required("--task")?;
            Ok(Verb::ProjectWorktreeIntegrate {
                session,
                task: parse_id("task id", &task)?,
                expected_parent_revision: flags.value("--expected-parent-revision"),
                request_id,
            })
        }
        "cleanup" => parse_worktree_cleanup(flags, rest),
        other => Err(format!(
            "unknown project worktree sub-verb '{other}'; use get, diff, integrate, or cleanup"
        )),
    }
}

/// `project worktree cleanup`: the explicit disposition of one checkout.
fn parse_worktree_cleanup(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    flags.verb = "project worktree cleanup".to_owned();
    let request_id = parse_request_id(flags)?;
    let session = one_session_id(flags, rest)?;
    let task = flags.required("--task")?;
    let keep = flags.flag("--keep");
    let remove = flags.flag("--remove");
    let discard = flags.flag("--discard");
    let disposition = match (keep, remove, discard) {
        (true, false, false) => WorktreeDisposition::Keep,
        (false, true, false) => WorktreeDisposition::Remove,
        (false, false, true) => WorktreeDisposition::Discard,
        (false, false, false) => {
            return Err(
                "project worktree cleanup requires --keep, --remove, or --discard".to_owned(),
            );
        }
        _ => {
            return Err(
                "project worktree cleanup accepts one of --keep, --remove, or --discard".to_owned(),
            );
        }
    };
    if disposition == WorktreeDisposition::Discard && !flags.flag("--yes") {
        return Err(
            "project worktree cleanup --discard throws away uncommitted changes; pass --yes to \
             confirm"
                .to_owned(),
        );
    }
    Ok(Verb::ProjectWorktreeCleanup {
        session,
        task: parse_id("task id", &task)?,
        disposition,
        request_id,
    })
}

/// The single positional id a sub-verb takes, after the words its own name
/// already spells.
fn one_session_id(flags: &FlagSet, rest: &[String]) -> Result<AgentSessionId, String> {
    let words = flags.verb.split(' ').count();
    let Some(session) = rest.get(words - 1) else {
        return Err(format!("{} requires a <session-id>", flags.verb));
    };
    if rest.len() > words {
        return Err(format!(
            "{} takes one session id, got '{}'",
            flags.verb, rest[words]
        ));
    }
    parse_id("session id", session)
}

/// The flags of `run`, which is the one verb whose flags interact: the task and
/// the name default feed each other, and the two attach flags share `--path`.
fn parse_run(flags: &mut FlagSet) -> Result<RunArgs, String> {
    let workspace = flags.required("--workspace")?;
    let task = match (flags.value("--task"), flags.value("--task-file")) {
        (Some(_), Some(_)) => {
            return Err("run accepts --task or --task-file, not both".to_owned());
        }
        (Some(task), None) => task,
        (None, Some(path)) => read_input_file("task", &path)?,
        (None, None) => return Err("run requires --task <text> or --task-file <path>".to_owned()),
    };
    let name = match flags.value("--name") {
        Some(name) => name,
        None => derived_name(&task),
    };
    if name.trim().is_empty() {
        return Err("--name must not be empty".to_owned());
    }
    let model = flags.value("--model").map(ModelId::new);

    let repository = flags.value("--repo");
    let directory = flags.value("--as-directory");
    let path = flags.value("--path");
    let revision = flags.value("--revision");
    if repository.is_some() && directory.is_some() {
        return Err("run accepts --repo or --as-directory, not both".to_owned());
    }
    if revision.is_some() && repository.is_none() {
        return Err("--revision only applies to --repo".to_owned());
    }
    if path.is_some() && repository.is_none() && directory.is_none() {
        return Err("--path requires --repo or --as-directory".to_owned());
    }
    let attach = |source: String, revision: Option<String>| -> Result<Attach, String> {
        let path = match &path {
            Some(path) => path.clone(),
            None => default_attach_path(&source).ok_or_else(|| {
                format!(
                    "'{source}' has no final path component; pass --path <path> to name the \
                     session path it lands at"
                )
            })?,
        };
        Ok(Attach {
            source,
            path,
            revision,
        })
    };
    let repository = repository
        .map(|source| attach(source, revision))
        .transpose()?;
    let directory = directory.map(|source| attach(source, None)).transpose()?;

    let approval = match flags.value("--approval-policy").as_deref() {
        None => None,
        Some("default") => Some(ApprovalPreset::Default),
        Some("auto") => Some(ApprovalPreset::Auto),
        Some(other) => {
            return Err(format!(
                "--approval-policy must be default or auto, got '{other}'"
            ));
        }
    };
    let request_id = parse_request_id(flags)?;
    let project = flags
        .value("--project")
        .map(|project| parse_id("project session id", &project))
        .transpose()?;
    let child_models = parse_child_model_changes(flags, false)?;
    Ok(RunArgs {
        workspace: parse_id("workspace id", &workspace)?,
        task,
        name,
        model,
        repository,
        directory,
        approval,
        request_id,
        new: flags.flag("--new"),
        project,
        child_models,
    })
}

/// The child model flags of `run` and `project models set`. Only `project
/// models set` removes entries, so `removals` gates `--no-child-model`,
/// `--unset-kind`, and `--clear`.
fn parse_child_model_changes(
    flags: &mut FlagSet,
    removals: bool,
) -> Result<ChildModelChanges, String> {
    let model = |raw: &str, flag: &str| {
        loom_core::normalize_child_model_id(raw)
            .map_err(|error| format!("{flag}: {}", error.message))
    };
    let kind = |raw: &str, flag: &str| {
        loom_core::normalize_child_model_kind(raw)
            .map_err(|error| format!("{flag}: {}", error.message))
    };
    let mut changes = ChildModelChanges::default();
    let defaults = flags.values("--child-model");
    if defaults.len() > 1 {
        return Err("--child-model may be given once".to_owned());
    }
    if let Some(default_model) = defaults.first() {
        changes.default_model = Some(Some(model(default_model, "--child-model")?));
    }
    for entry in flags.values("--child-model-for") {
        let Some((raw_kind, raw_model)) = entry.split_once('=') else {
            return Err(format!(
                "--child-model-for takes <kind>=<model-id>, got '{entry}'"
            ));
        };
        let entry_kind = kind(raw_kind, "--child-model-for")?;
        if changes
            .set_kinds
            .iter()
            .any(|(existing, _)| *existing == entry_kind)
        {
            return Err(format!(
                "--child-model-for names kind '{entry_kind}' more than once"
            ));
        }
        let entry_model = model(raw_model, "--child-model-for")?;
        changes.set_kinds.push((entry_kind, entry_model));
    }
    if removals {
        if flags.flag("--no-child-model") {
            if changes.default_model.is_some() {
                return Err("--child-model and --no-child-model exclude each other".to_owned());
            }
            changes.default_model = Some(None);
        }
        for raw in flags.values("--unset-kind") {
            let unset = kind(&raw, "--unset-kind")?;
            if changes.set_kinds.iter().any(|(set, _)| *set == unset) {
                return Err(format!(
                    "kind '{unset}' is both set and unset; pass one of them"
                ));
            }
            changes.unset_kinds.push(unset);
        }
        changes.clear = flags.flag("--clear");
    }
    Ok(changes)
}

/// Reads the task file or the direction text file as UTF-8, dropping one
/// trailing line ending.
///
/// Exactly one newline is removed, so a file that ends with `\n` and one that
/// ends with `\r\n` produce the same text, and any further trailing whitespace
/// the author wrote is kept as part of it. `what` names the flag the file came
/// from, so the error names the flag the caller passed.
fn read_input_file(what: &str, path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read the {what} file '{path}': {error}"))?;
    Ok(match text.strip_suffix('\n') {
        Some(trimmed) => trimmed.strip_suffix('\r').unwrap_or(trimmed).to_owned(),
        None => text,
    })
}

/// The session name a task derives when `--name` is absent: the task's first six
/// whitespace-separated words, at most 48 characters, without trailing
/// whitespace, and `loom-cli run` when the task has no words.
pub(crate) fn derived_name(task: &str) -> String {
    let mut name = String::new();
    for word in task.split_whitespace().take(NAME_WORDS) {
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    let bounded = name.chars().take(NAME_MAX_CHARS).collect::<String>();
    let bounded = bounded.trim_end();
    if bounded.is_empty() {
        NAME_FALLBACK.to_owned()
    } else {
        bounded.to_owned()
    }
}

/// The session path an attach source lands at when `--path` is absent: the
/// source's final path component, without a trailing `.git`.
pub(crate) fn default_attach_path(source: &str) -> Option<String> {
    let trimmed = source.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or_default();
    let last = last.strip_suffix(".git").unwrap_or(last);
    if last.is_empty() || last == "." || last == ".." {
        return None;
    }
    Some(last.to_owned())
}

fn optional_id<T: FromStr>(value: Option<String>) -> Result<Option<T>, String> {
    match value {
        Some(value) => Ok(Some(parse_id("workspace id", &value)?)),
        None => Ok(None),
    }
}

fn no_positionals(verb: &str, rest: &[String]) -> Result<(), String> {
    match rest.first() {
        Some(extra) => Err(format!(
            "'{verb}' takes no positional argument, got '{extra}'"
        )),
        None => Ok(()),
    }
}

/// The worker URL, with the flag winning over the environment and
/// [`DEFAULT_CONNECT_URL`] filling in when neither names one.
///
/// `parse` deliberately leaves the environment alone so parsing is a pure
/// function of its arguments; the binary applies this once, beside the token
/// file fallback, and stores the result in the invocation before the verb runs,
/// so the URL a cursor file is scoped to is the one that was connected to. An
/// empty or whitespace-only environment value is treated as unset, so it never
/// produces an unusable link.
pub(crate) fn resolve_connect(flag: Option<String>, environment: Option<String>) -> String {
    flag.or_else(|| environment.filter(|url| !url.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_CONNECT_URL.to_owned())
}

/// Where a command reads its bearer token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TokenFile {
    pub path: PathBuf,
    /// The worker URL the path was derived from, or `None` when the caller named
    /// the file. A read error names it, so a missing default says which URL it
    /// was derived for.
    pub derived_for: Option<String>,
}

/// The token file, in the precedence the contract documents: `--token-file`,
/// then `LOOM_TOKEN_FILE`, then the path derived from the connected URL inside
/// `state_directory`.
///
/// The derived path is `<state directory>/<host>_<port>/token`, which is exactly
/// where a standalone worker stores the token of the instance bound to that
/// address: [`loom_core::instance_dir`] names the instance directory by
/// [`loom_core::bind_key`], and [`loom_core::token_path`] is `token` inside it.
/// The host and port come from the URL's authority, with `ws` defaulting to port
/// 80 and `wss` to 443, and the key applies the same transform `bind_key` does
/// to a socket address, so `ws://127.0.0.1:8765/ws` reads
/// `<state directory>/127.0.0.1_8765/token`.
///
/// A blank environment value is treated as unset, like `LOOM_URL`, so an empty
/// variable falls through to the derived path instead of naming no file.
pub(crate) fn resolve_token_file(
    flag: Option<PathBuf>,
    environment: Option<PathBuf>,
    home: Option<&Path>,
    state_directory: &Path,
    connect: &str,
) -> Result<TokenFile, String> {
    if let Some(path) = flag {
        return Ok(TokenFile {
            path: expand_home(path, home),
            derived_for: None,
        });
    }
    if let Some(path) = environment.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(TokenFile {
            path: expand_home(path, home),
            derived_for: None,
        });
    }
    let (host, port) = ws_authority(connect).ok_or_else(|| {
        format!(
            "could not derive a token file from the worker URL '{connect}': it has no host and \
             port, so pass --token-file <path> or set LOOM_TOKEN_FILE"
        )
    })?;
    let instance = state_directory.join(authority_key(&host, port));
    Ok(TokenFile {
        path: loom_core::token_path(&instance),
        derived_for: Some(connect.to_owned()),
    })
}

/// The host and port of a `ws://` or `wss://` URL, using the scheme's default
/// port when the URL has none.
///
/// The CLI has no URL crate, so this reads the one part it needs: everything
/// between `scheme://` and the first `/`, `?`, or `#`, minus any user
/// information. `None` means the URL names no authority this client can address,
/// and the caller reports that instead of guessing a token path.
fn ws_authority(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme.trim().to_ascii_lowercase().as_str() {
        "ws" => 80,
        "wss" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    // An IPv6 literal is bracketed, so its port follows the closing bracket.
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        let port = match tail {
            "" => default_port,
            tail => tail.strip_prefix(':')?.parse().ok()?,
        };
        return Some((format!("[{host}]"), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host.to_owned(), port.parse().ok()?)),
        Some(_) => None,
        None => Some((authority.to_owned(), default_port)),
    }
}

/// The instance-directory key of a host and port: the same transform
/// [`loom_core::bind_key`] applies to a socket address, so a hostname maps the
/// same way a literal address does.
fn authority_key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
        .chars()
        .filter(|value| !matches!(value, '[' | ']'))
        .map(|value| if value == ':' { '_' } else { value })
        .collect()
}

/// Expands a leading `~/` (or a bare `~`) with `home`. Any other path, including
/// `~user` and a path with no `~` at all, is returned unchanged, and a path whose
/// home is unknown is returned unchanged as well so the error names what the
/// caller wrote.
fn expand_home(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(first)) if first == "~") {
        return path;
    }
    let Some(home) = home else {
        return path;
    };
    components.fold(home.to_path_buf(), |expanded, component| {
        expanded.join(component)
    })
}

fn parse_format(value: Option<String>) -> Result<Format, String> {
    match value.as_deref() {
        None | Some("text") => Ok(Format::Text),
        Some("jsonl") => Ok(Format::Jsonl),
        Some(other) => Err(format!("--format must be text or jsonl, got '{other}'")),
    }
}

fn parse_max_bytes(value: Option<String>) -> Result<usize, String> {
    match value {
        None => Ok(DEFAULT_MAX_BYTES),
        Some(value) => {
            let bytes = value
                .parse::<usize>()
                .map_err(|_| format!("--max-bytes must be a byte count, got '{value}'"))?;
            if bytes < MIN_MAX_BYTES {
                return Err(format!(
                    "--max-bytes must be at least {MIN_MAX_BYTES} bytes, got {bytes}"
                ));
            }
            Ok(bytes)
        }
    }
}
