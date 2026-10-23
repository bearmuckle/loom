//! Plain-language classification, copy, and backoff for worker connectivity
//! failures.
//!
//! An operation error is an answer from the worker about what the client asked
//! for, and the banner names the operation and the code for it. A connectivity
//! failure is not an answer at all: the client could not reach its worker, so
//! the internal code and the raw transport text say nothing a user can act on.
//! Those failures are classified here and described in the same plain language
//! the worker-connection details already use, while the raw code and message
//! stay as banner details and in the log.

use super::*;

/// The plain-language sentence every surface uses for a lost connection, so
/// the banner, the log, and the disconnected screen read the same way.
pub(crate) const CONNECTION_LOST_COPY: &str =
    "Loom lost its connection to the worker and is reconnecting automatically.";

/// Why the client could not reach its worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectivityFailure {
    /// The transport is closed: a dropped socket, a reset, or a single-use
    /// connection that already failed.
    ConnectionLost,
    /// The worker did not answer in time.
    TimedOut,
    /// The worker refused or could not accept the connection.
    Unreachable,
}

/// Phrases a cancelled request carries when it is the transport that ended it.
///
/// `request_cancelled` also reports a cancellation the user or a provider
/// asked for, so the message has to name the connection before the failure is
/// treated as a lost worker.
const TRANSPORT_MESSAGE_HINTS: &[&str] = &[
    "connection",
    "websocket",
    "socket",
    "closed",
    "reset",
    "stream",
    "worker",
];

/// Phrases an unavailable worker carries when the socket could not be opened.
const UNREACHABLE_MESSAGE_HINTS: &[&str] = &[
    "refused",
    "unreachable",
    "no route",
    "connection",
    "websocket",
];

/// Classifies a failure as a connectivity failure, or `None` for an error the
/// worker reported about the operation itself.
///
/// Only transport-shaped failures qualify, and everything else keeps the
/// `{operation} · {code}` banner, so a genuine operation error is never
/// rewritten as a connection problem.
pub(crate) fn classify_connectivity_failure(error: &LoomError) -> Option<ConnectivityFailure> {
    let message = error.message.to_ascii_lowercase();
    let mentions = |hints: &[&str]| hints.iter().any(|hint| message.contains(hint));
    match error.code {
        ErrorCode::DeadlineExceeded => Some(ConnectivityFailure::TimedOut),
        ErrorCode::RequestCancelled if mentions(TRANSPORT_MESSAGE_HINTS) => {
            Some(ConnectivityFailure::ConnectionLost)
        }
        ErrorCode::ProviderUnavailable if mentions(UNREACHABLE_MESSAGE_HINTS) => {
            Some(ConnectivityFailure::Unreachable)
        }
        _ => None,
    }
}

impl ConnectivityFailure {
    /// The plain-language headline for the status banner.
    pub(crate) fn headline(self) -> &'static str {
        match self {
            Self::ConnectionLost => "Worker connection lost",
            Self::TimedOut => "Worker is not responding",
            Self::Unreachable => "Worker is unreachable",
        }
    }

    /// The plain-language body: what happened and what the client is doing
    /// about it.
    ///
    /// Timeouts and refused connections reuse the worker-connection vocabulary
    /// the settings pane already shows, so the same failure reads the same way
    /// wherever it appears.
    pub(crate) fn detail(self, error: &LoomError) -> String {
        match self {
            Self::ConnectionLost => format!(
                "{CONNECTION_LOST_COPY} Nothing needs to be done unless the worker has stopped."
            ),
            Self::TimedOut | Self::Unreachable => format!(
                "{} Loom is reconnecting automatically.",
                worker_connection_failure_detail(WorkerConnectionStage::Transport, error, None)
            ),
        }
    }
}

/// The raw code and message, kept for the banner's details affordance and the
/// log line so a connectivity failure stays debuggable.
pub(crate) fn error_details(error: &LoomError) -> String {
    format!("{}: {}", error.code, error.message)
}

/// The first automatic reconnect attempt waits this long. A worker that
/// restarted is picked up quickly, and a worker that is down is not hammered.
pub(crate) const RECONNECT_BASE_MILLIS: u64 = 500;

/// The longest automatic reconnect wait.
pub(crate) const RECONNECT_MAX_MILLIS: u64 = 15_000;

/// Automatic reconnect attempts before the client stops and asks the user to
/// retry. Eight attempts reach the 15 s cap and cover about a minute.
pub(crate) const RECONNECT_MAX_ATTEMPTS: u32 = 8;

/// Exponential backoff: `base` doubled once per attempt, capped at `cap`.
pub(crate) const fn backoff_millis(attempt: u32, base_millis: u64, cap_millis: u64) -> u64 {
    let doublings = if attempt > 16 { 16 } else { attempt };
    let millis = base_millis.saturating_mul(1 << doublings);
    if millis < cap_millis {
        millis
    } else {
        cap_millis
    }
}

/// The delay an attempt counter describes, rounded up to whole seconds for
/// the indicator copy.
fn delay_seconds(millis: u64) -> u64 {
    millis.div_ceil(1_000)
}

/// A bounded, exponentially backing-off schedule for one reconnect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetrySchedule {
    attempts: u32,
}

impl RetrySchedule {
    /// The schedule for one visible reconnect.
    pub(crate) const fn reconnect() -> Self {
        Self { attempts: 0 }
    }

    /// How many automatic attempts have been consumed.
    pub(crate) const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// How many automatic attempts the schedule allows.
    pub(crate) const fn max_attempts(&self) -> u32 {
        RECONNECT_MAX_ATTEMPTS
    }

    /// Whether every automatic attempt has been consumed.
    pub(crate) const fn exhausted(&self) -> bool {
        self.attempts >= RECONNECT_MAX_ATTEMPTS
    }

    /// The delay the next attempt waits, without consuming the attempt.
    ///
    /// The polls read this so a poll that is failing against an unreachable
    /// worker backs off with the reconnect instead of keeping its own cadence.
    pub(crate) const fn next_delay_millis(&self) -> u64 {
        backoff_millis(self.attempts, RECONNECT_BASE_MILLIS, RECONNECT_MAX_MILLIS)
    }

    /// Consumes one attempt and returns the delay before it, or `None` once
    /// the schedule is spent.
    pub(crate) fn take_attempt(&mut self) -> Option<u64> {
        if self.exhausted() {
            return None;
        }
        let delay = self.next_delay_millis();
        self.attempts += 1;
        Some(delay)
    }

    /// Starts the schedule over for a retry the user asked for.
    pub(crate) fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// The client's single reconnecting state.
///
/// One state rather than a stream of errors: while the worker is unreachable
/// every connectivity failure updates this, so the user reads one coherent
/// message with an attempt indicator instead of successive coded errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Reconnecting {
    failure: ConnectivityFailure,
    schedule: RetrySchedule,
    text: String,
    details: String,
}

impl Reconnecting {
    pub(crate) fn new(failure: ConnectivityFailure, error: &LoomError) -> Self {
        Self {
            failure,
            schedule: RetrySchedule::reconnect(),
            text: failure.detail(error),
            details: error_details(error),
        }
    }

    pub(crate) fn failure(&self) -> ConnectivityFailure {
        self.failure
    }

    pub(crate) fn schedule(&self) -> RetrySchedule {
        self.schedule
    }

    /// The plain-language body the banner shows.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The raw code and message the banner keeps behind its details.
    pub(crate) fn details(&self) -> &str {
        &self.details
    }

    /// Folds a later failure into the same state.
    ///
    /// The connection is what is failing, so a repeat keeps the state (and its
    /// attempt counter); only the copy and the raw details follow the latest
    /// failure.
    pub(crate) fn update(&mut self, failure: ConnectivityFailure, error: &LoomError) {
        self.failure = failure;
        self.text = failure.detail(error);
        self.details = error_details(error);
    }

    /// The one progress line shown under the message.
    ///
    /// The attempt count and the wait stay on a single line so the reconnect
    /// card and the status banner can show them without repeating the copy
    /// above them.
    pub(crate) fn progress(&self) -> String {
        if self.schedule.exhausted() {
            return format!(
                "Automatic reconnection stopped after {} attempts.",
                self.schedule.attempts()
            );
        }
        format!(
            "Attempt {} of {} · next attempt in about {}s",
            self.schedule.attempts() + 1,
            self.schedule.max_attempts(),
            delay_seconds(self.schedule.next_delay_millis())
        )
    }

    /// The retry control's label, which asks for a retry the schedule cannot
    /// start on its own once it is spent.
    pub(crate) fn retry_label(&self) -> &'static str {
        if self.schedule.exhausted() {
            "Try again"
        } else {
            "Retry now"
        }
    }

    /// Consumes one automatic attempt, or `None` once the schedule is spent.
    pub(crate) fn take_attempt(&mut self) -> Option<u64> {
        self.schedule.take_attempt()
    }

    /// Restarts the schedule for a retry the user asked for.
    pub(crate) fn reset_schedule(&mut self) {
        self.schedule.reset();
    }
}
