use serde::{Deserialize, Serialize};

use crate::{LoomError, Result, RunId};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
    Command,
    Network,
    Destructive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    RequireApproval,
    Deny,
}

/// Per-kind approval decisions for tool actions.
///
/// `destructive` classifies audited, hard-to-reverse actions; it is not a
/// refusal. The shipped presets allow destructive actions, and a user who wants
/// a gate sets that dimension to [`PolicyDecision::RequireApproval`], which
/// prompts per instance instead of deciding for the user. The agent still
/// enforces [`PolicyDecision::Deny`], which a deployment can choose for a hard
/// refusal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalPolicy {
    pub read: PolicyDecision,
    pub write: PolicyDecision,
    pub command: PolicyDecision,
    pub network: PolicyDecision,
    pub destructive: PolicyDecision,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            read: PolicyDecision::Allow,
            write: PolicyDecision::RequireApproval,
            command: PolicyDecision::RequireApproval,
            network: PolicyDecision::RequireApproval,
            destructive: PolicyDecision::Allow,
        }
    }
}

impl ApprovalPolicy {
    /// Allows every action kind without prompting.
    ///
    /// The destructive classification describes an audited, hard-to-reverse
    /// action rather than a refusal, so it is allowed here like any other kind.
    /// Selecting this policy is the caller's explicit decision to let such
    /// actions run unapproved; a user who wants a per-instance prompt sets the
    /// destructive dimension to [`PolicyDecision::RequireApproval`] instead.
    pub const fn auto_approve() -> Self {
        Self {
            read: PolicyDecision::Allow,
            write: PolicyDecision::Allow,
            command: PolicyDecision::Allow,
            network: PolicyDecision::Allow,
            destructive: PolicyDecision::Allow,
        }
    }

    pub const fn decision(&self, action: ActionKind) -> PolicyDecision {
        match action {
            ActionKind::Read => self.read,
            ActionKind::Write => self.write,
            ActionKind::Command => self.command,
            ActionKind::Network => self.network,
            ActionKind::Destructive => self.destructive,
        }
    }
}

/// Operator opt-in that lets a command which provably only reads be classified
/// as [`ActionKind::Read`] instead of [`ActionKind::Command`].
///
/// `run_command` runs an arbitrary program, so the default policy gates it as a
/// command and the operator has to approve it. Some runs are dominated by
/// read-only inspection (`git log`, `git diff`, `rg`, `cat`) that repeats the
/// reads the file tools already perform unapproved, and an operator who knows
/// their checkouts may let exactly those run as reads.
///
/// The opt-in is off unless an operator turns it on, first by choosing the
/// configuration value (see [`ReadOnlyCommandPolicy::from_setting`]) and then
/// by handing the enabled policy to the tool executor. Disabled, the policy
/// classifies every command as a command, so nothing about the shipped default
/// policy widens on its own.
///
/// The allowlist is conservative in the only direction that is safe: a command
/// it does not recognise stays a command and still prompts. A missed entry costs
/// an approval; a wrongly allowed one would run unapproved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadOnlyCommandPolicy {
    enabled: bool,
}

/// The environment variable an operator sets to opt in to classifying
/// read-only commands as reads. Turning it on is a deliberate deployment
/// decision, never a default.
pub const READ_ONLY_COMMANDS_ENV: &str = "LOOM_READ_ONLY_COMMANDS";

impl ReadOnlyCommandPolicy {
    /// The shipped behaviour: every command is a command action.
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }

    /// Classification of provably read-only commands is turned on.
    pub const fn enabled() -> Self {
        Self { enabled: true }
    }

    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Resolves the operator setting.
    ///
    /// An absent or blank value keeps the policy disabled, `1`/`true`/`yes`/`on`
    /// enables it, `0`/`false`/`no`/`off` disables it explicitly, and any other
    /// value is an error instead of being read as one of the two: a typo must not
    /// silently leave the opt-in on or off.
    pub fn from_setting(value: Option<&str>) -> Result<Self> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::disabled());
        };
        match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Self::enabled()),
            "0" | "false" | "no" | "off" => Ok(Self::disabled()),
            _ => Err(LoomError::invalid_request(format!(
                "{READ_ONLY_COMMANDS_ENV} must be one of 1, true, yes, on, 0, false, no, off, but was '{value}'"
            ))),
        }
    }

    /// The action kind one `run_command` call is evaluated under.
    ///
    /// `program` is the executable and `args` its arguments, as the tool
    /// receives them. A disabled policy answers [`ActionKind::Command`] without
    /// inspecting the call at all.
    pub fn classify(self, program: &str, args: &[String]) -> ActionKind {
        if self.enabled && command_only_reads(program, args) {
            ActionKind::Read
        } else {
            ActionKind::Command
        }
    }
}

/// One program the read-only allowlist accepts, with the arguments it may carry.
struct ReadOnlyProgram {
    program: &'static str,
    /// Long options accepted without the leading `--`. An option's value is its
    /// own argument, which is then checked as a positional argument.
    long_options: &'static [&'static str],
    /// Short-option characters accepted, possibly combined in one argument.
    short_options: &'static [char],
    /// Subcommands accepted as the first positional argument; empty when the
    /// program takes none and every positional argument is a path or a pattern.
    subcommands: &'static [&'static str],
    /// Whether a bare number is accepted as an option (`git log -3`,
    /// `head -20`), which those programs read as their count option.
    numeric_shorthand: bool,
}

impl ReadOnlyProgram {
    fn accepts_option(&self, argument: &str) -> bool {
        if let Some(long) = argument.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            return self.long_options.contains(&name);
        }
        let short = argument.trim_start_matches('-');
        if short.is_empty() {
            return false;
        }
        if short.chars().all(|character| character.is_ascii_digit()) {
            return self.numeric_shorthand;
        }
        // A short option may carry its value attached (`-n5`), so trailing
        // digits are stripped before the characters are checked.
        let flags = short.trim_end_matches(|character: char| character.is_ascii_digit());
        !flags.is_empty()
            && flags
                .chars()
                .all(|character| self.short_options.contains(&character))
    }
}

/// Characters a shell reads as structure: chaining, redirection, substitution,
/// globbing, quoting, or a comment.
///
/// `run_command` does not run a shell, so these are literal bytes to the
/// program. They are still refused here, because a classified read must stay a
/// read even if a future caller passes the same string through a shell, and
/// because each of them makes a command's effect depend on parsing rules the
/// allowlist would have to model.
const SHELL_STRUCTURE: &[char] = &[
    '|', '&', ';', '<', '>', '`', '$', '(', ')', '{', '}', '*', '?', '[', ']', '\\', '"', '\'',
    '#', '!', '\n', '\r',
];

/// Whether one argument names something outside the session tree or carries
/// shell structure, which keeps a classified read inside the tree the file tools
/// are scoped to.
fn argument_is_scoped(argument: &str) -> bool {
    if argument
        .chars()
        .any(|character| SHELL_STRUCTURE.contains(&character))
    {
        return false;
    }
    // An absolute path (POSIX, or a Windows drive or UNC path) leaves the tree
    // the command runs in, and `..` may do the same, so both stay unapproved.
    if argument.starts_with('/') || argument.starts_with('\\') {
        return false;
    }
    if argument.len() > 2 && argument.as_bytes()[1] == b':' {
        return false;
    }
    !argument.split(['/', '\\']).any(|segment| segment == "..")
}

/// Whether `program` with `args` provably only reads.
fn command_only_reads(program: &str, args: &[String]) -> bool {
    let Some(spec) = READ_ONLY_PROGRAMS
        .iter()
        .find(|spec| spec.program == program)
    else {
        return false;
    };
    let mut options_ended = false;
    let mut subcommand_seen = spec.subcommands.is_empty();
    for argument in args {
        if !argument_is_scoped(argument) {
            return false;
        }
        if options_ended {
            continue;
        }
        if argument == "--" {
            options_ended = true;
            continue;
        }
        if argument.starts_with('-') {
            if !spec.accepts_option(argument) {
                return false;
            }
            continue;
        }
        if !subcommand_seen {
            if !spec.subcommands.contains(&argument.as_str()) {
                return false;
            }
            subcommand_seen = true;
        }
    }
    // A program that dispatches on a subcommand is only classified when the
    // call actually selects one of the reading subcommands; `git` with no
    // subcommand prints usage and is not a read.
    subcommand_seen
}

/// The programs and options a classified read may use.
///
/// Every entry was chosen against two questions: can this option write a file,
/// run another program, or change configuration, and can the program write at
/// all with the options listed? An option that is not listed is refused, so a
/// newly added tool option prompts until it is deliberately allowlisted here.
const READ_ONLY_PROGRAMS: &[ReadOnlyProgram] = &[
    ReadOnlyProgram {
        program: "git",
        numeric_shorthand: true,
        // `git log`, `status`, `diff`, `show`, `grep`, and the plumbing reads
        // below only write the index stat cache. The options that would write a
        // file (`--output`), run an external program (`--ext-diff`, `--textconv`,
        // a pager through `--paginate`), or take configuration (`-c`, `-O`) are
        // deliberately absent, which is also why `-c`, `-O`, and `--exec-path`
        // are not accepted as global options.
        long_options: &[
            "abbrev",
            "abbrev-commit",
            "abbrev-ref",
            "after",
            "all",
            "all-match",
            "author",
            "basic-regexp",
            "before",
            "binary",
            "branch",
            "branches",
            "cached",
            "color",
            "color-moved",
            "color-words",
            "column",
            "committer",
            "count",
            "date",
            "decorate",
            "decorate-refs",
            "dense",
            "diff-algorithm",
            "diff-filter",
            "exit-code",
            "extended-regexp",
            "files-with-matches",
            "find-copies",
            "find-renames",
            "first-parent",
            "fixed-strings",
            "follow",
            "format",
            "full-index",
            "full-name",
            "graph",
            "grep",
            "heading",
            "histogram",
            "ignore-all-space",
            "ignore-blank-lines",
            "ignore-case",
            "ignore-space-at-eol",
            "ignore-space-change",
            "ignored",
            "invert-grep",
            "line-number",
            "line-prefix",
            "long",
            "max-count",
            "merges",
            "minimal",
            "name-only",
            "name-status",
            "no-color",
            "no-ext-diff",
            "no-merges",
            "no-pager",
            "no-patch",
            "no-renames",
            "no-textconv",
            "not",
            "numstat",
            "oneline",
            "patch",
            "patch-with-raw",
            "patch-with-stat",
            "patience",
            "porcelain",
            "pretty",
            "quiet",
            "raw",
            "regexp-ignore-case",
            "relative",
            "relative-date",
            "remotes",
            "reverse",
            "short",
            "shortstat",
            "show-signature",
            "since",
            "skip",
            "source",
            "stat",
            "stat-count",
            "stat-width",
            "submodule",
            "summary",
            "tags",
            "topo-order",
            "unified",
            "unified-context",
            "untracked-files",
            "verbose",
            "with-filename",
            "word-diff",
            "word-diff-regex",
            "z",
        ],
        short_options: &[
            'b', 'C', 'E', 'F', 'G', 'i', 'L', 'M', 'm', 'n', 'p', 'S', 's', 'U', 'u', 'w', 'z',
        ],
        subcommands: &[
            "blame",
            "describe",
            "diff",
            "grep",
            "log",
            "ls-files",
            "rev-parse",
            "shortlog",
            "show",
            "status",
            "whatchanged",
        ],
    },
    ReadOnlyProgram {
        program: "ls",
        numeric_shorthand: false,
        long_options: &[
            "all",
            "almost-all",
            "block-size",
            "classify",
            "color",
            "directory",
            "dereference",
            "file-type",
            "format",
            "full-time",
            "group-directories-first",
            "hide",
            "human-readable",
            "ignore",
            "inode",
            "literal",
            "long",
            "no-group",
            "numeric-uid-gid",
            "quote-name",
            "recursive",
            "reverse",
            "si",
            "size",
            "sort",
            "time",
            "time-style",
        ],
        short_options: &[
            '1', 'A', 'a', 'B', 'C', 'd', 'F', 'G', 'g', 'h', 'i', 'k', 'l', 'm', 'n', 'o', 'p',
            'Q', 'q', 'R', 'r', 'S', 's', 't', 'U', 'u', 'v', 'X', 'x',
        ],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "cat",
        numeric_shorthand: false,
        long_options: &[
            "number",
            "number-nonblank",
            "show-all",
            "show-ends",
            "show-nonprinting",
            "show-tabs",
            "squeeze-blank",
        ],
        short_options: &['A', 'b', 'E', 'e', 'n', 's', 'T', 't', 'u', 'v'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "head",
        numeric_shorthand: true,
        long_options: &[
            "bytes",
            "lines",
            "quiet",
            "silent",
            "verbose",
            "zero-terminated",
        ],
        short_options: &['c', 'n', 'q', 'v', 'z'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        // `--follow`/`-f` is absent: it never ends on its own, so it is not a
        // read the tool can bound without the caller's timeout.
        program: "tail",
        numeric_shorthand: true,
        long_options: &[
            "bytes",
            "lines",
            "quiet",
            "silent",
            "sleep-interval",
            "verbose",
            "zero-terminated",
        ],
        short_options: &['c', 'n', 'q', 's', 'v', 'z'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "wc",
        numeric_shorthand: false,
        long_options: &["bytes", "chars", "lines", "max-line-length", "words"],
        short_options: &['c', 'L', 'l', 'm', 'w'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "rg",
        numeric_shorthand: false,
        // `--pre`, `--pre-glob`, `--hostname-bin`, and `--search-zip` run or
        // decompress through another program, so none of them is accepted.
        long_options: &[
            "after-context",
            "before-context",
            "binary",
            "case-sensitive",
            "column",
            "context",
            "count",
            "count-matches",
            "crlf",
            "encoding",
            "files",
            "files-with-matches",
            "files-without-match",
            "fixed-strings",
            "glob",
            "glob-case-insensitive",
            "heading",
            "hidden",
            "iglob",
            "ignore-case",
            "json",
            "line-number",
            "max-count",
            "max-depth",
            "max-filesize",
            "multiline",
            "multiline-dotall",
            "no-config",
            "no-filename",
            "no-heading",
            "no-ignore",
            "no-ignore-global",
            "no-ignore-vcs",
            "no-line-number",
            "no-messages",
            "no-require-git",
            "no-unicode",
            "null",
            "null-data",
            "only-matching",
            "passthru",
            "pretty",
            "quiet",
            "regexp",
            "replace",
            "smart-case",
            "sort",
            "sortr",
            "stats",
            "text",
            "threads",
            "trim",
            "type",
            "type-not",
            "type-list",
            "unicode",
            "unrestricted",
            "vimgrep",
            "with-filename",
            "word-regexp",
        ],
        short_options: &[
            '0', 'A', 'a', 'B', 'b', 'C', 'c', 'E', 'e', 'F', 'f', 'g', 'H', 'h', 'I', 'i', 'j',
            'L', 'l', 'M', 'm', 'N', 'n', 'o', 'p', 'q', 'r', 'S', 's', 'T', 't', 'U', 'u', 'v',
            'w', 'x',
        ],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "grep",
        numeric_shorthand: false,
        long_options: &[
            "after-context",
            "basic-regexp",
            "before-context",
            "binary-files",
            "byte-offset",
            "color",
            "colour",
            "context",
            "count",
            "dereference-recursive",
            "directories",
            "exclude",
            "exclude-dir",
            "exclude-from",
            "extended-regexp",
            "file",
            "files-with-matches",
            "files-without-match",
            "fixed-strings",
            "ignore-case",
            "include",
            "invert-match",
            "line-buffered",
            "line-number",
            "line-regexp",
            "max-count",
            "no-filename",
            "no-ignore-case",
            "no-messages",
            "null",
            "null-data",
            "only-matching",
            "perl-regexp",
            "quiet",
            "recursive",
            "regexp",
            "silent",
            "text",
            "with-filename",
            "word-regexp",
        ],
        short_options: &[
            'A', 'a', 'B', 'b', 'C', 'c', 'E', 'e', 'F', 'f', 'G', 'H', 'h', 'I', 'i', 'L', 'l',
            'm', 'n', 'o', 'q', 'r', 's', 'v', 'w', 'x', 'z',
        ],
        subcommands: &[],
    },
    ReadOnlyProgram {
        // `-C`/`--compile` writes a compiled magic file, so it is absent.
        program: "file",
        numeric_shorthand: false,
        long_options: &[
            "brief",
            "dereference",
            "exclude",
            "extension",
            "mime",
            "mime-encoding",
            "mime-type",
            "no-dereference",
            "print0",
            "separator",
        ],
        short_options: &['0', 'b', 'f', 'h', 'i', 'k', 'L', 'N', 's', 'z'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "stat",
        numeric_shorthand: false,
        long_options: &["dereference", "file-system", "format", "printf", "terse"],
        short_options: &['c', 'f', 'L', 't'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "pwd",
        numeric_shorthand: false,
        long_options: &["logical", "physical"],
        short_options: &['L', 'P'],
        subcommands: &[],
    },
    ReadOnlyProgram {
        // `-o`/`--output` writes a file, so it is absent.
        program: "tree",
        numeric_shorthand: false,
        long_options: &[
            "all",
            "dirsfirst",
            "du",
            "filelimit",
            "gitignore",
            "human-readable",
            "ignore-case",
            "level",
            "matchdirs",
            "noreport",
            "prune",
            "size",
        ],
        short_options: &[
            'a', 'C', 'D', 'd', 'F', 'f', 'h', 'I', 'i', 'J', 'l', 'L', 'N', 'n', 'P', 'p', 'Q',
            'q', 'R', 'r', 's', 't', 'U', 'u', 'v', 'X', 'x',
        ],
        subcommands: &[],
    },
    ReadOnlyProgram {
        program: "du",
        numeric_shorthand: false,
        long_options: &[
            "all",
            "apparent-size",
            "block-size",
            "bytes",
            "count-links",
            "dereference",
            "disk-usage",
            "exclude",
            "human-readable",
            "inodes",
            "max-depth",
            "null",
            "one-file-system",
            "separate-dirs",
            "summarize",
            "threshold",
            "time",
            "total",
        ],
        short_options: &[
            '0', 'a', 'b', 'c', 'd', 'h', 'k', 'L', 'm', 'P', 's', 'S', 'x',
        ],
        subcommands: &[],
    },
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyEvaluation {
    pub action: ActionKind,
    pub decision: PolicyDecision,
    pub reason: String,
}

impl PolicyEvaluation {
    pub fn evaluate(policy: &ApprovalPolicy, action: ActionKind, detail: &str) -> Self {
        let decision = policy.decision(action);
        let reason = match decision {
            PolicyDecision::Allow => format!("{detail} is allowed by the workspace policy"),
            PolicyDecision::RequireApproval => {
                format!("{detail} requires explicit user approval")
            }
            PolicyDecision::Deny => format!("{detail} is denied by the workspace policy"),
        };
        Self {
            action,
            decision,
            reason,
        }
    }

    /// States the remedy in a denial whose policy was pinned to a run.
    ///
    /// A run resolves its approval policy once, when it is created, and
    /// evaluates every step against that copy, so a later session policy change
    /// only reaches the next run. A caller that reads only "denied by the
    /// workspace policy", which is already true for a policy that changed, has
    /// no way to learn that or what would change the decision, so the denial
    /// names the run and the remedy. Only [`PolicyDecision::Deny`] gains the
    /// note: an allow, or a prompt the user can still answer, has no remedy to
    /// state.
    pub fn pinned_to_run(mut self, run_id: RunId) -> Self {
        if self.decision == PolicyDecision::Deny {
            self.reason.push_str(&format!(
                ". Run {run_id} fixed its approval policy when the run started, so this decision holds \
                 for the rest of the run; start a new run, or change the session approval policy to \
                 apply it from the next run, to allow this action"
            ));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classifies one call the way the tool executor does, from a program and
    /// its arguments.
    fn classify(policy: ReadOnlyCommandPolicy, program: &str, args: &[&str]) -> ActionKind {
        policy.classify(
            program,
            &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        )
    }

    /// Every positive case is a command a run actually issues to inspect a
    /// workspace: the git reads, plus the file and search reads the file tools
    /// already perform unapproved.
    #[test]
    fn opted_in_policy_classifies_reading_commands_as_reads() {
        let policy = ReadOnlyCommandPolicy::enabled();
        let cases: &[(&str, &[&str])] = &[
            ("git", &["log", "--oneline", "-n", "5"]),
            ("git", &["log", "--format=%h %s", "-n5"]),
            ("git", &["status", "--short", "--branch"]),
            ("git", &["diff", "--stat", "HEAD~1"]),
            ("git", &["diff", "--cached", "--name-only"]),
            ("git", &["show", "--no-patch", "HEAD"]),
            ("git", &["rev-parse", "--abbrev-ref", "HEAD"]),
            ("git", &["grep", "-n", "pattern", "crates"]),
            ("git", &["--no-pager", "log", "-3"]),
            ("ls", &["-la", "crates"]),
            ("ls", &["--long", "--human-readable"]),
            ("cat", &["Cargo.toml"]),
            ("head", &["-n", "20", "README.md"]),
            ("tail", &["-n", "20", "README.md"]),
            ("wc", &["-l", "Cargo.toml"]),
            ("rg", &["-n", "--type", "rust", "pattern", "crates"]),
            ("grep", &["-rn", "pattern", "crates"]),
            ("pwd", &[]),
            ("stat", &["-c", "%s", "Cargo.toml"]),
            ("file", &["-b", "Cargo.toml"]),
            ("du", &["-sh", "crates"]),
        ];
        for (program, args) in cases {
            assert_eq!(
                classify(policy, program, args),
                ActionKind::Read,
                "{program} {args:?} should read"
            );
        }
    }

    /// The security regression: nothing that can write, run another program, or
    /// leave the tree may pass as a read, however it is spelled.
    #[test]
    fn opted_in_policy_keeps_every_writing_or_escaping_command_a_command() {
        let policy = ReadOnlyCommandPolicy::enabled();
        let cases: &[(&str, &[&str])] = &[
            // A program that can write, or a shell that would run anything one
            // argument names.
            ("rm", &["-rf", "crates"]),
            ("sh", &["-c", "ls"]),
            ("bash", &["-c", "git log"]),
            ("env", &["ls"]),
            ("sudo", &["ls"]),
            ("cargo", &["test"]),
            ("git", &["commit", "-m", "x"]),
            ("git", &["push"]),
            ("git", &["checkout", "--", "."]),
            ("git", &[]),
            // Options that write a file or run an external program.
            ("git", &["diff", "--output=patch.diff"]),
            ("git", &["diff", "--output", "patch.diff"]),
            ("git", &["diff", "--ext-diff"]),
            ("git", &["diff", "--textconv"]),
            ("git", &["log", "--paginate"]),
            ("git", &["-c", "core.pager=cat", "log"]),
            ("git", &["-O", "orderfile", "log"]),
            ("git", &["log", "-O", "orderfile"]),
            ("git", &["grep", "-O", "pattern"]),
            ("rg", &["--pre", "cat", "pattern"]),
            ("rg", &["--pre-glob", "*.rs", "pattern"]),
            ("rg", &["--hostname-bin", "hostname", "pattern"]),
            ("rg", &["-z", "pattern"]),
            ("file", &["-C", "-m", "magic"]),
            ("tail", &["-f", "log"]),
            ("tree", &["-o", "listing.txt"]),
            // Shell structure: chaining, redirection, substitution, globbing.
            ("ls", &["| tee", "out.txt"]),
            ("cat", &["a;rm b"]),
            ("cat", &["$(id)"]),
            ("cat", &["`id`"]),
            ("cat", &["a&&rm b"]),
            ("ls", &[">out.txt"]),
            ("ls", &["*.rs"]),
            ("rg", &["a|b", "crates"]),
            // A glob metacharacter stays a command everywhere, including in an
            // option value: the argument would change meaning if it ever reached
            // a shell, so the allowlist does not model glob parsing.
            ("rg", &["--glob", "*.rs", "pattern", "crates"]),
            ("rg", &["--glob=*.rs", "pattern", "crates"]),
            ("cat", &["a\nb"]),
            // Paths that leave the session tree.
            ("cat", &["/etc/passwd"]),
            ("cat", &["../outside.txt"]),
            ("cat", &["crates/../../outside.txt"]),
            ("ls", &["\\\\server\\share"]),
        ];
        for (program, args) in cases {
            assert_eq!(
                classify(policy, program, args),
                ActionKind::Command,
                "{program} {args:?} must stay a command"
            );
        }
    }

    /// The opt-in is the only thing that changes the classification, and the
    /// shipped default keeps every command a command.
    #[test]
    fn the_shipped_default_policy_keeps_even_a_reading_command_a_command() {
        let policy = ReadOnlyCommandPolicy::default();
        assert_eq!(policy, ReadOnlyCommandPolicy::disabled());
        assert!(!policy.is_enabled());
        assert_eq!(
            classify(policy, "git", &["status", "--short"]),
            ActionKind::Command
        );
        assert_eq!(classify(policy, "pwd", &[]), ActionKind::Command);
        assert_eq!(
            ApprovalPolicy::default().decision(classify(policy, "git", &["status"])),
            PolicyDecision::RequireApproval
        );
    }

    /// The opt-in changes only the read dimension: a classified read runs under
    /// the read decision, and every command the allowlist refuses keeps the
    /// command decision.
    #[test]
    fn the_opt_in_only_moves_allowlisted_commands_to_the_read_decision() {
        let policy = ReadOnlyCommandPolicy::enabled();
        let approvals = ApprovalPolicy::default();
        assert_eq!(
            approvals.decision(classify(policy, "git", &["log", "--oneline"])),
            PolicyDecision::Allow
        );
        assert_eq!(
            approvals.decision(classify(policy, "git", &["push"])),
            PolicyDecision::RequireApproval
        );
        // A read dimension that still gates reads keeps gating a classified
        // command: the opt-in moves the kind, it never grants the action.
        let gated = ApprovalPolicy {
            read: PolicyDecision::Deny,
            ..Default::default()
        };
        assert_eq!(
            gated.decision(classify(policy, "ls", &["-l"])),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn the_setting_accepts_the_documented_values_and_refuses_the_rest() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert_eq!(
                ReadOnlyCommandPolicy::from_setting(Some(value)).unwrap(),
                ReadOnlyCommandPolicy::enabled(),
                "{value} enables"
            );
        }
        for value in ["0", "false", "no", "off"] {
            assert_eq!(
                ReadOnlyCommandPolicy::from_setting(Some(value)).unwrap(),
                ReadOnlyCommandPolicy::disabled(),
                "{value} disables"
            );
        }
        for value in [None, Some(""), Some("  ")] {
            assert_eq!(
                ReadOnlyCommandPolicy::from_setting(value).unwrap(),
                ReadOnlyCommandPolicy::disabled(),
                "{value:?} keeps the default"
            );
        }
        let error = ReadOnlyCommandPolicy::from_setting(Some("maybe")).unwrap_err();
        assert!(error.message.contains(READ_ONLY_COMMANDS_ENV));
        assert!(error.message.contains("maybe"));
    }

    #[test]
    fn default_policy_allows_reads_and_destructive_actions() {
        let policy = ApprovalPolicy::default();
        assert_eq!(policy.decision(ActionKind::Read), PolicyDecision::Allow);
        assert_eq!(
            policy.decision(ActionKind::Write),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Command),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Network),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Destructive),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluations_explain_the_policy_decision() {
        let policy = ApprovalPolicy::default();
        let evaluation = PolicyEvaluation::evaluate(&policy, ActionKind::Command, "cargo test");
        assert_eq!(evaluation.decision, PolicyDecision::RequireApproval);
        assert!(evaluation.reason.contains("approval"));
    }

    #[test]
    fn auto_approve_allows_every_action_kind() {
        let policy = ApprovalPolicy::auto_approve();
        assert_eq!(policy.decision(ActionKind::Read), PolicyDecision::Allow);
        assert_eq!(policy.decision(ActionKind::Write), PolicyDecision::Allow);
        assert_eq!(policy.decision(ActionKind::Command), PolicyDecision::Allow);
        assert_eq!(policy.decision(ActionKind::Network), PolicyDecision::Allow);
        assert_eq!(
            policy.decision(ActionKind::Destructive),
            PolicyDecision::Allow
        );
    }

    /// A deployment that wants destructive actions refused outright can still
    /// say so; the agent keeps enforcing that decision.
    #[test]
    fn a_policy_that_denies_destructive_actions_evaluates_to_deny() {
        let policy = ApprovalPolicy {
            destructive: PolicyDecision::Deny,
            ..Default::default()
        };
        let evaluation =
            PolicyEvaluation::evaluate(&policy, ActionKind::Destructive, "github_push_branch");
        assert_eq!(evaluation.decision, PolicyDecision::Deny);
        assert!(evaluation.reason.contains("denied"));
    }

    /// A denial states the remedy, not only the decision: the run fixed its own
    /// policy and only a new run (or a session policy change taken up by the
    /// next run) changes it.
    #[test]
    fn a_run_pinned_denial_states_the_pin_and_the_remedy() {
        let policy = ApprovalPolicy {
            destructive: PolicyDecision::Deny,
            ..Default::default()
        };
        let run_id = RunId::new();
        let evaluation =
            PolicyEvaluation::evaluate(&policy, ActionKind::Destructive, "github_push_branch")
                .pinned_to_run(run_id);

        assert_eq!(evaluation.decision, PolicyDecision::Deny);
        assert!(evaluation.reason.contains("denied by the workspace policy"));
        assert!(evaluation.reason.contains(&run_id.to_string()));
        assert!(
            evaluation
                .reason
                .contains("fixed its approval policy when the run started")
        );
        assert!(evaluation.reason.contains("start a new run"));
        assert!(evaluation.reason.contains("next run"));
    }

    /// An allow or an approval prompt must not gain run-pinning text: neither
    /// decision is pinned, because the user can still answer the prompt and an
    /// allow needs no remedy.
    #[test]
    fn only_a_denial_gains_the_run_pinning_note() {
        let policy = ApprovalPolicy::default();
        let run_id = RunId::new();
        let reason_for = |action: ActionKind| {
            PolicyEvaluation::evaluate(&policy, action, "a tool call")
                .pinned_to_run(run_id)
                .reason
        };

        assert_eq!(
            reason_for(ActionKind::Read),
            "a tool call is allowed by the workspace policy"
        );
        assert_eq!(
            reason_for(ActionKind::Command),
            "a tool call requires explicit user approval"
        );
    }

    /// A user who wants a gate on the destructive dimension asks for a prompt
    /// rather than a refusal.
    #[test]
    fn a_policy_can_require_approval_for_destructive_actions() {
        let policy = ApprovalPolicy {
            destructive: PolicyDecision::RequireApproval,
            ..Default::default()
        };
        let evaluation =
            PolicyEvaluation::evaluate(&policy, ActionKind::Destructive, "github_push_branch");
        assert_eq!(evaluation.decision, PolicyDecision::RequireApproval);
        assert!(evaluation.reason.contains("approval"));
    }
}
