//! Argument parsing for `loom-ctl`.
//!
//! Parsing is a two-phase split so that global flags are genuinely accepted
//! anywhere: the first pass turns the tokens into flags and positional words,
//! knowing only each flag's arity, and the second pass interprets the flags for
//! the verb the first positional word names. Both `--flag value` and
//! `--flag=value` forms reach the same place, and a flag the verb does not use
//! is a usage error rather than something silently ignored.

use std::{path::PathBuf, str::FromStr};

use loom_core::{AgentSessionId, WorkspaceId};

/// The budget `--max-bytes` defaults to.
pub(crate) const DEFAULT_MAX_BYTES: usize = 4096;
/// The smallest budget `--max-bytes` accepts.
pub(crate) const MIN_MAX_BYTES: usize = 64;
/// How many events `peek` asks for by default.
pub(crate) const DEFAULT_LINES: u32 = 200;

pub(crate) const HELP: &str = r#"Usage: loom-ctl <verb> [flags]

Verbs:
  doctor                            report the negotiated protocol,
                                    capabilities, participant and worker node
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

#[derive(Clone, Debug)]
pub(crate) enum Verb {
    Doctor,
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
        Verb::WhoAmI => "whoami",
        Verb::Ls { .. } => "ls",
        Verb::Get { .. } => "get",
        Verb::Events(_) => "events",
        Verb::Peek(_) => "peek",
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
            "whoami" => "whoami",
            "ls" => "ls",
            "get" => "get",
            "events" => "events",
            "peek" => "peek",
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
