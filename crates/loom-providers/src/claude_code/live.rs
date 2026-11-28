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
    pub delivered: usize,
    /// The user messages the process has seen, so a transcript it does not
    /// belong to never takes it.
    pub seen: u64,
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
            delivered: 0,
            seen: 0,
            pending: None,
            queued: Vec::new(),
            used: Instant::now(),
        }
    }

    /// Whether this process can serve a transcript of `messages`.
    pub fn fits(&self, launch: &str, messages: &[ModelMessage]) -> bool {
        self.launch == launch
            && self.delivered <= messages.len() + 1
            && self.seen == digest(&messages[..self.delivered.min(messages.len())])
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

/// A digest of the user messages in `messages`.
pub(super) fn digest(messages: &[ModelMessage]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for message in messages {
        if message.role == MessageRole::User {
            message.content.hash(&mut hasher);
        }
    }
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
    (!live.session.has_exited()).then_some(live)
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
