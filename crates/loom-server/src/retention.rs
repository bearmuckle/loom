//! Archive-retention input and the periodic sweep that acts on it.
//!
//! Retention is operator input rather than a client request: the window comes
//! from `--archive-retention` or `LOOM_ARCHIVE_RETENTION` and the force-discard
//! switch from `--archive-retention-force-discard` or
//! `LOOM_ARCHIVE_RETENTION_FORCE_DISCARD`. Both binaries resolve that input
//! here, so the standalone server and `loom --serve` accept exactly the same
//! syntax and the same fallback order.

use std::{sync::Arc, time::Duration};

use loom_core::{AgentSessionId, LoomError, Result};
use loom_protocol::ArchiveRetentionPolicy;

use crate::InProcessBackend;

/// Environment variable holding the retention window.
pub const ARCHIVE_RETENTION_ENV: &str = "LOOM_ARCHIVE_RETENTION";
/// Environment variable holding the force-discard switch.
pub const ARCHIVE_RETENTION_FORCE_DISCARD_ENV: &str = "LOOM_ARCHIVE_RETENTION_FORCE_DISCARD";
/// The flag that carries the retention window, named in parse errors.
pub const ARCHIVE_RETENTION_FLAG: &str = "--archive-retention";
/// The flag that carries the force-discard switch, named in parse errors.
pub const ARCHIVE_RETENTION_FORCE_DISCARD_FLAG: &str = "--archive-retention-force-discard";
/// How often a running server sweeps expired archived sessions.
pub const ARCHIVE_RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// What one retention sweep did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArchiveSweepReport {
    /// Sessions the sweep deleted, including a project's descendants.
    pub deleted_sessions: Vec<AgentSessionId>,
    /// Candidates the sweep left alone this round.
    pub skipped: Vec<ArchiveSweepSkip>,
}

/// One candidate the sweep left alone, with the reason it did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSweepSkip {
    /// The candidate session; for a project tree this is its root.
    pub session_id: AgentSessionId,
    /// Why the candidate was left alone.
    pub reason: String,
}

impl ArchiveSweepReport {
    /// Whether the sweep found nothing to delete and nothing to report.
    pub fn is_empty(&self) -> bool {
        self.deleted_sessions.is_empty() && self.skipped.is_empty()
    }
}

/// Parses a retention window.
///
/// The accepted syntax is `<n><unit>` with unit `ms`, `s`, `m`, `h`, `d`, or
/// `w`; a bare integer means seconds; `off` and `never` mean retention stays
/// disabled. `source` names the flag or environment variable for the error, so
/// an operator is told which input to fix.
pub fn parse_retention_window(value: &str, source: &str) -> Result<Option<u64>> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("never")
    {
        return Ok(None);
    }
    let normalized = value.to_ascii_lowercase();
    let (number, multiplier_ms) = if let Some(number) = normalized.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = normalized.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = normalized.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = normalized.strip_suffix('h') {
        (number, 3_600_000)
    } else if let Some(number) = normalized.strip_suffix('d') {
        (number, 86_400_000)
    } else if let Some(number) = normalized.strip_suffix('w') {
        (number, 604_800_000)
    } else {
        (normalized.as_str(), 1_000)
    };
    let number = number.parse::<u64>().map_err(|_| {
        LoomError::invalid_request(format!(
            "{source} must be a duration such as 30s, 5m, 12h, 14d, or 2w, a plain number of \
             seconds, or off; got '{value}'"
        ))
    })?;
    Ok(Some(number.saturating_mul(multiplier_ms)))
}

/// Parses the force-discard switch: `1`/`true`/`yes` and `0`/`false`/`no`,
/// case-insensitively. `source` names the flag or environment variable for the
/// error.
pub fn parse_force_discard(value: &str, source: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(LoomError::invalid_request(format!(
            "{source} must be 1/true/yes or 0/false/no; got '{value}'"
        ))),
    }
}

/// Resolves the archive-retention policy from the explicit flags and their
/// environment fallback.
///
/// An explicit flag always wins over the variable it falls back to. A missing,
/// `off`, `never`, or zero window keeps retention disabled, so an absent value
/// never auto-deletes anything.
pub fn resolve_archive_retention(
    flag: Option<&str>,
    force_flag: Option<&str>,
    env: Option<&str>,
    force_env: Option<&str>,
) -> Result<ArchiveRetentionPolicy> {
    let retention_ms = match flag {
        Some(value) => parse_retention_window(value, ARCHIVE_RETENTION_FLAG)?,
        None => match env {
            Some(value) => parse_retention_window(value, ARCHIVE_RETENTION_ENV)?,
            None => None,
        },
    };
    let force_discard_worktrees = match force_flag {
        Some(value) => parse_force_discard(value, ARCHIVE_RETENTION_FORCE_DISCARD_FLAG)?,
        None => match force_env {
            Some(value) => parse_force_discard(value, ARCHIVE_RETENTION_FORCE_DISCARD_ENV)?,
            None => false,
        },
    };
    Ok(ArchiveRetentionPolicy {
        retention_ms: retention_ms.filter(|retention_ms| *retention_ms > 0),
        force_discard_worktrees,
    })
}

/// Sweeps once before a server starts accepting clients, logging the active
/// policy and any failure instead of aborting the start.
pub fn sweep_archive_retention_at_startup(backend: &InProcessBackend) {
    let policy = backend.archive_retention();
    match policy.retention() {
        Some(window) => log::info!(
            "[loom-server] archive retention deletes archived projects after {} ms (force discard worktrees: {})",
            window.as_millis(),
            policy.force_discard_worktrees
        ),
        None => log::info!("[loom-server] archive retention is disabled"),
    }
    match backend.sweep_archive_retention() {
        Ok(report) => log::info!(
            "[loom-server] archive retention startup sweep deleted {} session(s) and skipped {} candidate(s)",
            report.deleted_sessions.len(),
            report.skipped.len()
        ),
        Err(error) => log::warn!(
            "[loom-server] archive retention startup sweep failed: {}",
            error.message
        ),
    }
}

/// Sweeps immediately, then every `interval` until the caller drops the future.
///
/// A failed sweep is logged and the next interval still runs, so a locked or
/// unreachable database cannot stop retention or abort the server.
pub async fn run_archive_retention_sweeps(backend: Arc<InProcessBackend>, interval: Duration) {
    run_periodic_sweeps(interval, move || backend.sweep_archive_retention()).await;
}

/// [`run_archive_retention_sweeps`] with the sweep injected, so a test can watch
/// the periodic driver without a server.
pub(crate) async fn run_periodic_sweeps(
    interval: Duration,
    mut sweep: impl FnMut() -> Result<ArchiveSweepReport>,
) {
    // `tokio::time::interval` panics on a zero period, and the sweep must never
    // take the server down, so a caller-supplied zero becomes the smallest
    // useful period instead.
    let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // The first tick completes immediately, so a sweep runs as soon as the
        // driver is polled.
        ticker.tick().await;
        match sweep() {
            Ok(report) if report.is_empty() => {}
            Ok(report) => log::info!(
                "[loom-server] archive retention deleted {} session(s) and skipped {} candidate(s)",
                report.deleted_sessions.len(),
                report.skipped.len()
            ),
            Err(error) => log::warn!(
                "[loom-server] archive retention sweep failed: {}",
                error.message
            ),
        }
    }
}
