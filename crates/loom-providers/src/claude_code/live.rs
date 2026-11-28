//! Claude Code processes kept running between turns.
//!
//! A run's provider is dropped when the run ends or parks for an approval, so
//! a process that should outlive it waits here, keyed by the session it works
//! for. The next run's provider takes it back. A process left alone too long is
//! ended; the provider then starts a new one with `--resume`, as after a worker
//! restart.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use loom_harness::ClaudeCodeSession;
use loom_model::{MessageRole, ModelMessage};

/// How long an idle process is kept.
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
/// How long a process that waits for an approval is kept.
const PENDING_LIMIT: Duration = Duration::from_secs(4 * 60 * 60);
/// The most processes kept at once; the least recently used goes first.
const MAX_KEPT: usize = 16;

/// A tool call the CLI is still waiting to have answered.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Pending {
    pub request_id: String,
    pub tool: String,
    pub input: serde_json::Value,
}

/// One running CLI and what it has seen.
pub(super) struct Live {
    pub session: ClaudeCodeSession,
    /// What the process was started with; a change needs a new process.
    pub launch: String,
    pub claude_session: Option<String>,
    /// Where the process stands in the transcript, so a transcript it does not
    /// belong to never takes it.
    pub anchor: Anchor,
    pub pending: Option<Pending>,
    /// User messages that arrived while a call waited and still need sending
    /// once the CLI finishes its turn.
    pub queued: Vec<String>,
    used: Instant,
}

impl Live {
    pub fn new(session: ClaudeCodeSession, launch: String) -> Self {
        Self {
            session,
            launch,
            claude_session: None,
            anchor: Anchor::default(),
            pending: None,
            queued: Vec::new(),
            used: Instant::now(),
        }
    }

    /// How many messages of `messages` this process has read, when it can
    /// serve that transcript.
    pub fn fits(&self, launch: &str, messages: &[ModelMessage]) -> Option<usize> {
        if self.launch != launch {
            return None;
        }
        self.anchor.locate(messages)
    }

    fn expired(&self, now: Instant) -> bool {
        let limit = if self.pending.is_some() {
            PENDING_LIMIT
        } else {
            IDLE_LIMIT
        };
        now.duration_since(self.used) > limit
    }
}

/// How many user and tool messages mark a process's place in a transcript.
const ANCHOR_LEN: usize = 3;

/// A process's place in a transcript.
///
/// The runtime may replace older history with a summary that changes from
/// turn to turn, so positions and the transcript's start do not stay put. The
/// most recent user and tool messages are kept verbatim, so the place is the
/// last few of those the process read, and how far past the last one it got.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Anchor {
    marks: Vec<u64>,
    offset: usize,
}

impl Anchor {
    /// The place of a process that has read the first `delivered` messages.
    pub fn of(messages: &[ModelMessage], delivered: usize) -> Self {
        let read = &messages[..delivered.min(messages.len())];
        let marked = marked(read);
        let tail = &marked[marked.len().saturating_sub(ANCHOR_LEN)..];
        match tail.last() {
            Some(&last) => Self {
                marks: tail.iter().map(|&index| mark(&messages[index])).collect(),
                offset: delivered - last,
            },
            None => Self {
                marks: Vec::new(),
                offset: delivered,
            },
        }
    }

    /// How many messages of `messages` a process at this place has read, when
    /// the transcript still holds its place.
    pub fn locate(&self, messages: &[ModelMessage]) -> Option<usize> {
        let delivered = if self.marks.is_empty() {
            (marked(messages).is_empty() || self.offset == 0).then_some(self.offset)
        } else {
            // The latest match, since the process is near the end of the
            // transcript. The newest mark must be there; older ones may have
            // gone into the summary at the transcript's start, so the
            // comparison stops there or at the first message.
            let marked = marked(messages);
            (0..marked.len()).rev().find_map(|end| {
                self.marks
                    .iter()
                    .rev()
                    .zip(marked[..=end].iter().rev())
                    .enumerate()
                    .all(|(step, (&want, &index))| {
                        mark(&messages[index]) == want || (step > 0 && index == marked[0])
                    })
                    .then(|| marked[end] + self.offset)
            })
        }?;
        (delivered <= messages.len() + 1).then_some(delivered)
    }
}

/// The indices of the messages that mark a place: what the person and the
/// tools said, which a later transcript repeats word for word.
fn marked(messages: &[ModelMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message.role, MessageRole::User | MessageRole::Tool))
        .map(|(index, _)| index)
        .collect()
}

fn mark(message: &ModelMessage) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{:?}", message.role).hash(&mut hasher);
    message.content.hash(&mut hasher);
    format!("{:?}", message.tool_call_id).hash(&mut hasher);
    hasher.finish()
}

fn pool() -> &'static Mutex<HashMap<PathBuf, Live>> {
    static POOL: OnceLock<Mutex<HashMap<PathBuf, Live>>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}

fn locked() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Live>> {
    pool().lock().unwrap_or_else(|error| error.into_inner())
}

/// Takes the process kept for `key`, if one is still running.
pub(super) fn take(key: &PathBuf) -> Option<Live> {
    let mut live = locked().remove(key)?;
    if live.session.has_exited() {
        log::info!("[claude-code] the kept process had exited; starting a new one");
        return None;
    }
    Some(live)
}

/// Keeps `live` for `key`, ending processes that waited too long.
pub(super) fn put(key: PathBuf, mut live: Live) {
    if live.session.has_exited() {
        return;
    }
    live.used = Instant::now();
    let now = Instant::now();
    // Dropping a process ends it, so the ended ones are collected and dropped
    // after the lock is released.
    let mut ended = Vec::new();
    {
        let mut pool = locked();
        let expired = pool
            .iter()
            .filter(|(_, kept)| kept.expired(now))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        ended.extend(expired.iter().filter_map(|key| pool.remove(key)));
        if let Some(replaced) = pool.insert(key, live) {
            ended.push(replaced);
        }
        while pool.len() > MAX_KEPT {
            let Some(oldest) = pool
                .iter()
                .min_by_key(|(_, kept)| kept.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            ended.extend(pool.remove(&oldest));
        }
    }
    drop(ended);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ModelMessage {
        ModelMessage::new(MessageRole::User, text)
    }

    fn reply(text: &str) -> ModelMessage {
        ModelMessage::new(MessageRole::Assistant, text)
    }

    #[test]
    fn a_place_is_found_again_after_new_messages() {
        let read = vec![user("a"), reply("1"), user("b")];
        let anchor = Anchor::of(&read, read.len() + 1);
        let later = vec![user("a"), reply("1"), user("b"), reply("2"), user("c")];
        assert_eq!(anchor.locate(&later), Some(4));
    }

    #[test]
    fn a_place_survives_a_summary_of_older_history() {
        let read = vec![user("a"), reply("1"), user("b"), reply("2"), user("c")];
        let anchor = Anchor::of(&read, read.len() + 1);
        let compacted = vec![user("summary of a and b"), user("c"), reply("3"), user("d")];
        assert_eq!(anchor.locate(&compacted), Some(3));
    }

    #[test]
    fn a_different_transcript_has_no_place() {
        let read = vec![user("a"), reply("1"), user("b")];
        let anchor = Anchor::of(&read, read.len() + 1);
        assert_eq!(anchor.locate(&[user("x"), reply("1"), user("y")]), None);
        assert_eq!(anchor.locate(&[user("x"), user("a"), user("b")]), Some(4));
    }

    #[test]
    fn a_place_before_any_message_is_the_start() {
        let anchor = Anchor::of(&[], 0);
        assert_eq!(anchor.locate(&[user("a")]), Some(0));
        let past = Anchor::of(&[reply("1")], 2);
        assert_eq!(past.locate(&[user("a")]), None);
    }
}
