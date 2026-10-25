//! Argument parsing for `loom-cli`.
//!
//! Parsing is a two-phase split so that global flags are genuinely accepted
//! anywhere: the first pass turns the tokens into flags and positional words,
//! knowing only each flag's arity, and the second pass interprets the flags for
//! the verb the first positional word names. Both `--flag value` and
//! `--flag=value` forms reach the same place, and a flag the verb does not use
//! is a usage error rather than something silently ignored.

use std::{path::PathBuf, str::FromStr};

use loom_core::{AgentSessionId, RequestId, RunId, TaskId, ToolCallId, WorkspaceId};
use loom_model::ModelId;

/// The budget `--max-bytes` defaults to.
pub(crate) const DEFAULT_MAX_BYTES: usize = 4096;
/// The smallest budget `--max-bytes` accepts.
pub(crate) const MIN_MAX_BYTES: usize = 64;
/// How many events `peek` asks for by default.
pub(crate) const DEFAULT_LINES: u32 = 200;
/// How many project messages `project messages` asks for by default.
const DEFAULT_MESSAGE_LIMIT: u32 = 100;
/// The largest project message page the worker accepts.
const MAX_MESSAGE_LIMIT: u32 = 512;
/// The model `run` starts when `--model` is absent.
pub(crate) const DEFAULT_MODEL: &str = "deterministic/demo";
/// How many of the task's leading words a derived session name keeps.
const NAME_WORDS: usize = 6;
/// The longest derived session name, in characters.
const NAME_MAX_CHARS: usize = 48;
/// The name `run` uses when the task has no words at all.
const NAME_FALLBACK: &str = "loom-cli run";
/// How long `wait` polls before it reports `deadline_exceeded`.
pub(crate) const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 60;

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
         [--format text|jsonl] [--max-bytes <n>]
                                    print a session's events, optionally
                                    following for new ones
  peek --session <id> [--lines <n>] [--format text|jsonl] [--max-bytes <n>]
                                    print the newest events of a session
  run --workspace <id> (--task <text> | --task-file <path>) [--name <name>]
      [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]]
      [--as-directory <path> [--path <path>]]
      [--approval-policy default|auto] [--request-id <uuid>] [--new]
                                    reuse the workspace's session named
                                    --name, or create it, attach the inputs
                                    to a session this run created, and start
                                    one agent run without waiting for it
  send --run <id> --text <text>     direct the active run and print its state
  answer --run <id> --text <text>   answer the question the run waits on
  approve --run <id> --tool-call <id>
                                    approve the blocked tool call
  deny --run <id> --tool-call <id> [--reason <text>]
                                    deny the blocked tool call
  stop --run <id> [--session <id> --archive] [--retry-on-conflict]
                                    interrupt a run, and optionally archive
                                    its session afterwards
  wait --session <id> [--until blocked|terminal] [--timeout <seconds>]
                                    poll until the run blocks or reaches a
                                    terminal state; always time-bounded
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
  session archive <session-id> [--yes]
                                    archive one session, keeping its
                                    transcript, checkpoints, and every
                                    worktree; --yes is required when the id
                                    is a project root whose archive also
                                    reaches its descendants
  session delete <session-id> [--force] [--yes]
                                    delete an archived session, or a project
                                    root with its descendants; --yes is
                                    required for a cascade or with --force
  project get <id>                  one project's ids, agent, task, and
                                    worktree counts
  project tree <id>                 one line per project agent, indented by
                                    its depth
  project tasks <id>                one line per delegated task of a project
  project children <id>             one line per child agent, with its
                                    pending approval or question
  project messages <id> [--after <seq>] [--limit <n>] [--max-bytes <n>]
                                    the project's durable parent-child log
  project send <id> (--text <text> | --text-file <path>)
                                    direct the child session's active run
  project child pause|resume|interrupt|cancel <id>
                                    control the delegated child task
  project archive <session-id>      archive a project root with its idle
                                    members; the id must name such a root
  project delete <session-id> [--force] [--yes]
                                    delete a project root and its tree; the
                                    id must name such a root, and --yes is
                                    required
  project worktree cleanup <session-id> --task <task-id>
      (--keep | --remove | --discard) [--yes]
                                    keep or remove the checkout of one
                                    delegated task; --discard throws away
                                    uncommitted changes and needs --yes
  delete <session-id> [--force] [--yes]
                                    delete an archived session, or a project
                                    root with its descendants; the same
                                    operation as 'session delete'

`run` never prints more than `session <id> <name>` and `run <id> <state>`: it
returns as soon as the worker accepts the run. --repo and --as-directory are
mutually exclusive, --revision applies to --repo only, and --path defaults to
the source's final path component without a trailing `.git`. A session is only
attached to when this invocation created it, so replaying a command with the
same --request-id neither duplicates an attachment nor fails on one that
exists. --request-id is the idempotency key of the run start alone: the same id
is answered from the worker's durable cache with the same run, while the other
mutations of the verb use fresh ids. On a session this invocation creates,
--approval-policy (default when absent) is pinned before the run starts; on a
reused session it is only changed when the flag is given, so a plain run does
not overwrite a policy a person set.

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
`stop` refuses a conflict unless --retry-on-conflict is given, so a stale
revision cannot interrupt a run whose state the caller has not seen.

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

`project` takes any project member session id, and a project id too, because a
project id is its root session id. Every line it prints comes from one
`Project/GetProjectSnapshotForSession` read of the worker's own projection, so
no client-side tree or cache can disagree with it. `project send` directs the
child session's active run through `Collaboration/DirectAgent` with the same
revision check and single conflict retry as `send`, because
`Project/SendProjectAgentMessage` is only open to a server-bound agent run;
`project messages` is the read-only view of the durable parent-child log.
`--limit` accepts 1..=512 and defaults to 100.

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

Global flags, accepted anywhere as --flag value or --flag=value:
  --connect <url>             worker WebSocket URL (fallback LOOM_URL)
  --token-file <path>         file holding the bearer token
                              (fallback LOOM_TOKEN_FILE)
  --ca-file <pem>             also trust this PEM certificate
  --allow-insecure-remote     accept plaintext ws:// to a non-loopback host
  --max-frame-bytes <n>       transport frame size limit
  --json                      print exactly one JSON envelope on stdout
  --help, -h                  print this help

The token is read from a file only: --token is rejected, because a token on a
command line is visible to every process on the node.

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
    pub model: ModelId,
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
}

/// Which control action `project child` sends. The worker owns every lifecycle
/// rule, so the client maps the word to the action and reports the answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectChildAction {
    Pause,
    Resume,
    Interrupt,
    Cancel,
}

impl ProjectChildAction {
    /// The sub-verb word, as printed in help and reported in `--json`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Interrupt => "interrupt",
            Self::Cancel => "cancel",
        }
    }

    /// The word the text projection prints for the result.
    pub(crate) const fn past(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Resume => "resumed",
            Self::Interrupt => "interrupted",
            Self::Cancel => "cancelled",
        }
    }
}

/// The flags of `project messages`.
#[derive(Clone, Debug)]
pub(crate) struct ProjectMessagesArgs {
    pub session: AgentSessionId,
    pub after: Option<u64>,
    pub limit: u32,
    pub max_bytes: usize,
}

/// The sub-verbs of `project`.
#[derive(Clone, Debug)]
pub(crate) enum ProjectVerb {
    Get {
        session: AgentSessionId,
    },
    Tree {
        session: AgentSessionId,
    },
    Tasks {
        session: AgentSessionId,
    },
    Children {
        session: AgentSessionId,
    },
    Messages(ProjectMessagesArgs),
    Send {
        session: AgentSessionId,
        text: String,
    },
    Child {
        session: AgentSessionId,
        action: ProjectChildAction,
    },
}

impl ProjectVerb {
    /// The verb's name, as printed and as reported in a `--json` envelope.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Get { .. } => "project get",
            Self::Tree { .. } => "project tree",
            Self::Tasks { .. } => "project tasks",
            Self::Children { .. } => "project children",
            Self::Messages(_) => "project messages",
            Self::Send { .. } => "project send",
            Self::Child { action, .. } => match action {
                ProjectChildAction::Pause => "project child pause",
                ProjectChildAction::Resume => "project child resume",
                ProjectChildAction::Interrupt => "project child interrupt",
                ProjectChildAction::Cancel => "project child cancel",
            },
        }
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
    },
    Answer {
        run: RunId,
        text: String,
    },
    Approve {
        run: RunId,
        tool_call: ToolCallId,
    },
    Deny {
        run: RunId,
        tool_call: ToolCallId,
        reason: Option<String>,
    },
    Stop {
        run: RunId,
        archive: Option<AgentSessionId>,
        retry_on_conflict: bool,
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
    /// `session archive`: archive one session, and its descendants when the id
    /// is a project root that cascades. `yes` acknowledges that cascade.
    SessionArchive {
        session: AgentSessionId,
        yes: bool,
    },
    /// `session delete`: the explicit spelling of `delete`.
    SessionDelete {
        session: AgentSessionId,
        force: bool,
        yes: bool,
    },
    /// `project archive`: archive a project root with its idle members.
    ProjectArchive {
        session: AgentSessionId,
    },
    /// `project delete`: delete a project root with its tree.
    ProjectDelete {
        session: AgentSessionId,
        force: bool,
        yes: bool,
    },
    /// `project worktree cleanup`: keep or remove one delegated task's
    /// checkout.
    ProjectWorktreeCleanup {
        session: AgentSessionId,
        task: TaskId,
        disposition: WorktreeDisposition,
    },
    /// The live project verbs: `get`, `tree`, `tasks`, `children`, `messages`,
    /// `send`, and `child`. Boxed because the message flags make it the widest
    /// sub-verb of a value every command line carries once.
    Project(Box<ProjectVerb>),
    /// `delete`: the id is the confirmation. `force` discards dirty or locked
    /// worktrees; `yes` acknowledges a cascade or a force discard.
    Delete {
        session: AgentSessionId,
        force: bool,
        yes: bool,
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
    Help,
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
        Verb::SessionArchive { .. } => "session archive",
        Verb::SessionDelete { .. } => "session delete",
        Verb::ProjectArchive { .. } => "project archive",
        Verb::ProjectDelete { .. } => "project delete",
        Verb::ProjectWorktreeCleanup { .. } => "project worktree cleanup",
        Verb::Project(project) => project.name(),
        Verb::Delete { .. } => "delete",
    }
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
            "delete" => "delete",
            "session" => "session",
            "project" => "project",
            _ => "",
        })
        .unwrap_or("")
}

/// Every flag the parser knows, with whether it takes a value.
fn arity(name: &str) -> Option<bool> {
    Some(match name {
        "--connect" | "--token-file" | "--token" | "--ca-file" | "--max-frame-bytes" => true,
        "--allow-insecure-remote" | "--json" | "--help" | "-h" => false,
        "--workspace" | "--session" | "--after" | "--cursor-file" | "--format" | "--max-bytes"
        | "--lines" => true,
        "--archived" | "--metadata" | "--follow" => false,
        "--task" | "--task-file" | "--name" | "--model" | "--repo" | "--revision" | "--path"
        | "--as-directory" | "--approval-policy" | "--request-id" => true,
        "--run" | "--text" | "--text-file" | "--tool-call" | "--reason" | "--until"
        | "--timeout" => true,
        "--limit" => true,
        "--window" | "--force-discard" | "--sub-agent" => true,
        "--new" | "--archive" | "--retry-on-conflict" | "--force" | "--yes" | "--tree" => false,
        "--keep" | "--remove" | "--discard" => false,
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
    // surface instead of failing on one token.
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        return Ok(Parsed::Help);
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
            Verb::Events(EventsArgs {
                session: parse_id("session id", &session)?,
                after,
                cursor_file: flags.value("--cursor-file").map(PathBuf::from),
                follow: flags.flag("--follow"),
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
            Verb::Send {
                run: parse_id("run id", &flags.required("--run")?)?,
                text: flags.required("--text")?,
            }
        }
        "answer" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Answer {
                run: parse_id("run id", &flags.required("--run")?)?,
                text: flags.required("--text")?,
            }
        }
        "approve" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Approve {
                run: parse_id("run id", &flags.required("--run")?)?,
                tool_call: parse_id("tool call id", &flags.required("--tool-call")?)?,
            }
        }
        "deny" => {
            no_positionals(&flags.verb, rest)?;
            Verb::Deny {
                run: parse_id("run id", &flags.required("--run")?)?,
                tool_call: parse_id("tool call id", &flags.required("--tool-call")?)?,
                reason: flags.value("--reason"),
            }
        }
        "stop" => {
            no_positionals(&flags.verb, rest)?;
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

/// The `session` sub-verbs: `archive` keeps a session and its checkouts,
/// `delete` removes it permanently.
fn parse_session(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err("session requires 'archive' or 'delete'".to_owned());
    };
    flags.verb = format!("session {sub_verb}");
    match sub_verb.as_str() {
        "archive" => {
            let session = one_session_id(flags, rest)?;
            Ok(Verb::SessionArchive {
                session,
                yes: flags.flag("--yes"),
            })
        }
        "delete" => {
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
            })
        }
        other => Err(format!(
            "unknown session sub-verb '{other}'; use archive or delete"
        )),
    }
}

/// The `project` sub-verbs: the live verbs, which print the worker's project
/// projection (`get`, `tree`, `tasks`, `children`, `messages`, `send`, and
/// `child`), plus the project-scoped `archive` and `delete`, which require the
/// id to name a project root, and `worktree cleanup`, which acts on one
/// delegated task's checkout. Every one of them names one member session, and
/// a project id is accepted too because a project id is its root session id.
fn parse_project(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.first() else {
        return Err(
            "project requires a sub-verb: get, tree, tasks, children, messages, send, child, \
             archive, delete, or worktree"
                .to_owned(),
        );
    };
    flags.verb = format!("project {sub_verb}");
    match sub_verb.as_str() {
        "archive" => Ok(Verb::ProjectArchive {
            session: one_session_id(flags, rest)?,
        }),
        "delete" => {
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
            })
        }
        "worktree" => parse_project_worktree(flags, rest),
        "get" | "tree" | "tasks" | "children" | "messages" | "send" | "child" => {
            parse_project_live(flags, rest, sub_verb)
        }
        other => Err(format!(
            "unknown project sub-verb '{other}'; use get, tree, tasks, children, messages, send, \
             child, archive, delete, or worktree"
        )),
    }
}

/// The live `project` sub-verbs: one member session id and, for `messages` and
/// `send`, their own flags.
fn parse_project_live(
    flags: &mut FlagSet,
    rest: &[String],
    sub_verb: &str,
) -> Result<Verb, String> {
    if sub_verb == "child" {
        let Some(action) = rest.get(1) else {
            return Err("project child requires pause, resume, interrupt, or cancel".to_owned());
        };
        let action = match action.as_str() {
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
        flags.verb = format!("project child {}", action.name());
        let session = one_session_id(flags, rest)?;
        return Ok(Verb::Project(Box::new(ProjectVerb::Child {
            session,
            action,
        })));
    }
    let session = one_session_id(flags, rest)?;
    let verb = match sub_verb {
        "messages" => ProjectVerb::Messages(ProjectMessagesArgs {
            session,
            after: parse_after(flags.value("--after"))?,
            limit: parse_limit(flags.value("--limit"))?,
            max_bytes: parse_max_bytes(flags.value("--max-bytes"))?,
        }),
        "send" => ProjectVerb::Send {
            session,
            text: parse_send_text(flags)?,
        },
        "get" => ProjectVerb::Get { session },
        "tree" => ProjectVerb::Tree { session },
        "tasks" => ProjectVerb::Tasks { session },
        _ => ProjectVerb::Children { session },
    };
    Ok(Verb::Project(Box::new(verb)))
}

/// The `project worktree` sub-verbs, of which only `cleanup` is exposed.
fn parse_project_worktree(flags: &mut FlagSet, rest: &[String]) -> Result<Verb, String> {
    let Some(sub_verb) = rest.get(1) else {
        return Err("project worktree requires 'cleanup'".to_owned());
    };
    if sub_verb != "cleanup" {
        return Err(format!(
            "unknown project worktree sub-verb '{sub_verb}'; use cleanup"
        ));
    }
    flags.verb = "project worktree cleanup".to_owned();
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

fn parse_after(value: Option<String>) -> Result<Option<u64>, String> {
    match value {
        Some(value) => Ok(Some(value.parse::<u64>().map_err(|_| {
            format!("--after must be a project sequence, got '{value}'")
        })?)),
        None => Ok(None),
    }
}

/// The page size `project messages` asks for: the worker accepts 1..=512, so a
/// larger or zero value is a usage error rather than a clamped page.
fn parse_limit(value: Option<String>) -> Result<u32, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_MESSAGE_LIMIT);
    };
    let limit = value
        .parse::<u32>()
        .map_err(|_| format!("--limit must be a message count, got '{value}'"))?;
    if !(1..=MAX_MESSAGE_LIMIT).contains(&limit) {
        return Err(format!(
            "--limit must be between 1 and {MAX_MESSAGE_LIMIT}, got {limit}"
        ));
    }
    Ok(limit)
}

/// The text `project send` delivers, from `--text` or from `--text-file`.
fn parse_send_text(flags: &mut FlagSet) -> Result<String, String> {
    match (flags.value("--text"), flags.value("--text-file")) {
        (Some(_), Some(_)) => {
            Err("project send accepts --text or --text-file, not both".to_owned())
        }
        (Some(text), None) => Ok(text),
        (None, Some(path)) => read_input_file("text file", &path),
        (None, None) => Err("project send requires --text <text> or --text-file <path>".to_owned()),
    }
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
        (None, Some(path)) => read_input_file("task file", &path)?,
        (None, None) => return Err("run requires --task <text> or --task-file <path>".to_owned()),
    };
    let name = match flags.value("--name") {
        Some(name) => name,
        None => derived_name(&task),
    };
    if name.trim().is_empty() {
        return Err("--name must not be empty".to_owned());
    }
    let model = ModelId::new(
        flags
            .value("--model")
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
    );

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
    let request_id = match flags.value("--request-id") {
        Some(value) => Some(value.parse::<RequestId>().map_err(|_| {
            format!("'{value}' is not a valid request id; --request-id takes a UUID")
        })?),
        None => None,
    };
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
    })
}

/// Reads an input file as UTF-8, dropping one trailing line ending.
///
/// Exactly one newline is removed, so a file that ends with `\n` and one that
/// ends with `\r\n` produce the same text, and any further trailing whitespace
/// the author wrote is kept. `what` names the flag in the error: `run
/// --task-file` and `project send --text-file` share this one rule.
fn read_input_file(what: &str, path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read the {what} '{path}': {error}"))?;
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

/// The worker URL, with the flag winning over the environment.
///
/// `parse` deliberately leaves the environment alone so parsing is a pure
/// function of its arguments; the binary applies this once, beside the token
/// file fallback, and stores the result in the invocation before the verb runs,
/// so the URL a cursor file is scoped to is the one that was connected to. An
/// empty or whitespace-only environment value is treated as unset, so it fails
/// the "a worker URL is required" check instead of producing an unusable link.
pub(crate) fn resolve_connect(flag: Option<String>, environment: Option<String>) -> Option<String> {
    flag.or_else(|| environment.filter(|url| !url.trim().is_empty()))
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
