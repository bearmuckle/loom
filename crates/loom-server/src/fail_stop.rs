//! Reopening a backend that latched its persistence fail-stop.
//!
//! A durable state save can fail for a reason that goes away on its own: the
//! deployment filesystem filling up is the case that produced this module. The
//! fail-stop such a failure latches is deliberate and never clears
//! ([`crate::InProcessBackend`] rejects every durable request while it is set),
//! because a request-scoped save can fail after the handler changed in-memory
//! state, and answering a later request from a state the database does not hold
//! would replay a partially applied mutation. Reopening from the last committed
//! snapshot is the safe recovery, so the standalone server does what a service
//! manager can act on: it records the fail-stop, waits, and exits non-zero so
//! `Restart=on-failure` reopens it.
//!
//! The wait grows with the number of fail-stops recorded in the recent past, so a
//! failure that is still present — the disk is still full — spaces the restarts
//! out instead of leaving them to trip the service manager's start limit, which
//! would stop the unit instead of retrying it.
//!
//! This lives in the standalone server rather than in the shared serve bootstrap
//! because exiting is only right when something reopens the process. The native
//! shell's `--serve` mode is started by a person, so it keeps refusing writes and
//! reports the fail-stop through `/health` instead.

use std::{
    convert::Infallible,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use loom_core::{ErrorCode, LoomError, Timestamp};

use crate::InProcessBackend;

/// How long a recorded fail-stop keeps counting towards the reopen delay.
const FAIL_STOP_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Most fail-stops the record keeps, so the file stays small and a long outage
/// cannot grow the delay past its cap.
const FAIL_STOP_RECORD_LIMIT: usize = 8;
/// How often the supervisor looks for a latched fail-stop.
const FAIL_STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How the wait before a fail-stopped process exits to be reopened grows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReopenBackoff {
    /// The wait after the first recorded fail-stop.
    pub base: Duration,
    /// The longest wait, so a failure that is still present keeps retrying at a
    /// steady rate instead of waiting longer and longer.
    pub max: Duration,
}

impl Default for ReopenBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(30),
            max: Duration::from_secs(5 * 60),
        }
    }
}

impl ReopenBackoff {
    /// The wait after `recent_fail_stops` recorded fail-stops: `base` doubled
    /// once per earlier fail-stop and capped at `max`.
    pub fn delay(self, recent_fail_stops: usize) -> Duration {
        let mut delay = self.base;
        for _ in 1..recent_fail_stops {
            if delay >= self.max {
                break;
            }
            delay = delay.saturating_mul(2);
        }
        delay.min(self.max)
    }
}

/// The file the standalone server records fail-stops in: a sibling of the state
/// database whose saves failed, so the record belongs to the instance that owns
/// that database.
pub fn fail_stop_record_path(state_db: &Path) -> PathBuf {
    let mut path = state_db.as_os_str().to_os_string();
    path.push(".fail-stop");
    PathBuf::from(path)
}

/// Records a fail-stop at `now_millis` in `path` and returns how long to wait
/// before exiting to be reopened.
///
/// Recording is best effort. A full filesystem is both the failure this exists
/// for and a reason the record cannot be written, so an unreadable or unwritable
/// record falls back to the shortest wait rather than to no reopen at all: the
/// restarts are then spaced by that wait instead of by the grown one.
fn record_fail_stop(path: &Path, now_millis: u64, backoff: ReopenBackoff) -> Duration {
    let mut recent = recent_fail_stops(path, now_millis);
    recent.push(now_millis);
    let expired = recent.len().saturating_sub(FAIL_STOP_RECORD_LIMIT);
    recent.drain(..expired);
    if let Err(error) = write_fail_stops(path, &recent) {
        log::warn!(
            "[loom-server] could not record the persistence fail-stop in {}: {error}",
            path.display()
        );
    }
    backoff.delay(recent.len())
}

/// The fail-stops in `path` that still count at `now_millis`. A record in the
/// future does not count: the clock moved back, and an entry that never expires
/// would hold the delay at its cap forever.
fn recent_fail_stops(path: &Path, now_millis: u64) -> Vec<u64> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let oldest = now_millis.saturating_sub(FAIL_STOP_WINDOW.as_millis() as u64);
    contents
        .lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .filter(|recorded| (oldest..=now_millis).contains(recorded))
        .collect()
}

fn write_fail_stops(path: &Path, fail_stops: &[u64]) -> io::Result<()> {
    let contents = fail_stops
        .iter()
        .map(|fail_stop| format!("{fail_stop}\n"))
        .collect::<String>();
    fs::write(path, contents)
}

/// Resolves once the backend latched its persistence fail-stop and the wait
/// recorded for it has elapsed, carrying the error the caller reports before it
/// exits to be reopened.
///
/// `record` is where the fail-stop is recorded, normally
/// [`fail_stop_record_path`] of the state database; `None` means this instance
/// has no durable state database, so its saves cannot fail and the returned
/// future never resolves.
pub async fn reopen_after_fail_stop(
    backend: Arc<InProcessBackend>,
    record: Option<PathBuf>,
    backoff: ReopenBackoff,
) -> LoomError {
    let Some(record) = record else {
        // Nothing durable can fail here, so the latch cannot be set.
        match std::future::pending::<Infallible>().await {}
    };
    let mut poll = tokio::time::interval(FAIL_STOP_POLL_INTERVAL);
    loop {
        poll.tick().await;
        if !is_fail_stopped(&backend) {
            continue;
        }
        let delay = record_fail_stop(&record, Timestamp::now().as_unix_millis(), backoff);
        log::error!(
            "[loom-server] persistence fail-stop is latched; exiting in {}s so the backend can be reopened from its last committed state",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
        // Re-read the latch: a future in-place recovery clears it, and this
        // supervisor then has nothing to do.
        if !is_fail_stopped(&backend) {
            continue;
        }
        return LoomError::new(
            ErrorCode::Persistence,
            format!(
                "exiting after a {}s persistence fail-stop so the backend can be reopened from its last committed state",
                delay.as_secs()
            ),
            true,
        );
    }
}

fn is_fail_stopped(backend: &InProcessBackend) -> bool {
    backend.persistence_failed.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    use loom_core::{ErrorCode, Timestamp};

    use super::{
        FAIL_STOP_RECORD_LIMIT, FAIL_STOP_WINDOW, ReopenBackoff, fail_stop_record_path,
        recent_fail_stops, record_fail_stop, reopen_after_fail_stop,
    };
    use crate::InProcessBackend;

    fn backoff(base_millis: u64, max_millis: u64) -> ReopenBackoff {
        ReopenBackoff {
            base: Duration::from_millis(base_millis),
            max: Duration::from_millis(max_millis),
        }
    }

    fn record_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "loom-fail-stop-{name}-{}.db.fail-stop",
            loom_core::WorkspaceId::new()
        ))
    }

    #[test]
    fn reopen_delay_doubles_from_the_base_and_is_capped() {
        let backoff = ReopenBackoff::default();
        assert_eq!(backoff.delay(0), Duration::from_secs(30));
        assert_eq!(backoff.delay(1), Duration::from_secs(30));
        assert_eq!(backoff.delay(2), Duration::from_secs(60));
        assert_eq!(backoff.delay(3), Duration::from_secs(120));
        assert_eq!(backoff.delay(4), Duration::from_secs(240));
        assert_eq!(backoff.delay(5), Duration::from_secs(300));
        assert_eq!(backoff.delay(64), Duration::from_secs(300));
    }

    #[test]
    fn fail_stops_are_recorded_next_to_the_state_database() {
        assert_eq!(
            fail_stop_record_path(std::path::Path::new(
                "/var/lib/loom/loom/loom-server/state.db"
            )),
            PathBuf::from("/var/lib/loom/loom/loom-server/state.db.fail-stop")
        );
    }

    #[test]
    fn recorded_fail_stops_grow_the_delay_and_the_record_is_bounded() {
        let path = record_path("growth");
        let backoff = backoff(1_000, 60_000);
        let start = 1_000_000;

        assert_eq!(record_fail_stop(&path, start, backoff), backoff.base);
        assert_eq!(
            record_fail_stop(&path, start + 1, backoff),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            record_fail_stop(&path, start + 2, backoff),
            Duration::from_millis(4_000)
        );

        for index in 0..FAIL_STOP_RECORD_LIMIT as u64 * 2 {
            record_fail_stop(&path, start + 10 + index, backoff);
        }
        assert_eq!(
            recent_fail_stops(&path, start + 100).len(),
            FAIL_STOP_RECORD_LIMIT
        );
        assert_eq!(
            record_fail_stop(&path, start + 200, backoff),
            backoff.max,
            "a bounded record keeps the delay at its cap"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fail_stops_outside_the_window_and_in_the_future_do_not_count() {
        let path = record_path("window");
        let backoff = backoff(1_000, 60_000);
        let now = FAIL_STOP_WINDOW.as_millis() as u64 * 2;
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\nnot a timestamp\n",
                now - 1,
                now - FAIL_STOP_WINDOW.as_millis() as u64 - 1,
                now + 60_000
            ),
        )
        .unwrap();

        // Only the entry inside the window counts, so the new fail-stop is the
        // second one and doubles the base.
        assert_eq!(
            record_fail_stop(&path, now, backoff),
            Duration::from_millis(2_000)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn an_unwritable_record_still_reopens_after_the_base_delay() {
        let directory = record_path("unwritable");
        fs::create_dir_all(&directory).unwrap();
        let backoff = backoff(1_000, 60_000);

        // The path is a directory, so neither reading nor writing it succeeds.
        // A full filesystem looks the same way and must not disable the reopen.
        assert_eq!(
            record_fail_stop(&directory, Timestamp::now().as_unix_millis(), backoff),
            backoff.base
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_latched_backend_exits_to_be_reopened() {
        let backend = InProcessBackend::new();
        backend.persistence_failed.store(true, Ordering::SeqCst);
        let path = record_path("latched");
        let started = Instant::now();

        let error = reopen_after_fail_stop(backend, Some(path.clone()), backoff(10, 20)).await;

        assert_eq!(error.code, ErrorCode::Persistence);
        assert!(
            started.elapsed() >= Duration::from_millis(10),
            "the reopen waits at least the base delay"
        );
        assert_eq!(
            recent_fail_stops(&path, Timestamp::now().as_unix_millis()).len(),
            1
        );
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_serving_backend_is_never_reopened() {
        let backend = InProcessBackend::new();
        let path = record_path("serving");

        let reopened = tokio::time::timeout(
            Duration::from_millis(50),
            reopen_after_fail_stop(backend, Some(path.clone()), backoff(1, 2)),
        )
        .await;

        assert!(reopened.is_err(), "a serving backend must not be reopened");
        assert!(!path.exists(), "a serving backend records no fail-stop");
    }

    #[tokio::test]
    async fn an_in_memory_instance_is_never_reopened() {
        let backend = InProcessBackend::new();
        backend.persistence_failed.store(true, Ordering::SeqCst);

        let reopened = tokio::time::timeout(
            Duration::from_millis(50),
            reopen_after_fail_stop(backend, None, backoff(1, 2)),
        )
        .await;

        assert!(
            reopened.is_err(),
            "an in-memory backend has no database to reopen"
        );
    }
}
