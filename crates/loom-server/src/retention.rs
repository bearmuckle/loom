//! Archive-retention input and the periodic sweep that acts on it.
//!
//! The window and the force-discard switch come from
//! `--archive-retention`/`LOOM_ARCHIVE_RETENTION` and
//! `--archive-retention-force-discard`/`LOOM_ARCHIVE_RETENTION_FORCE_DISCARD`,
//! and both binaries resolve that input here, so the standalone server and
//! `loom --serve` accept exactly the same syntax and the same fallback order.
//!
//! Explicit input at a start is a startup default rather than the policy
//! itself: it overrides the policy stored in durable state and is stored in
//! turn. Either way the policy is changeable at runtime through
//! `SetArchiveRetentionPolicy`.

use std::{sync::Arc, time::Duration};

use loom_core::{AgentSessionId, ErrorCode, LoomError, Result};
use loom_protocol::{ArchiveRetentionPolicy, DEFAULT_SUB_AGENT_RETENTION_MS};

/// The duration and force-discard grammars live in `loom-core` so the server
/// flags and the CLI share one parser; this module re-exports them for the
/// server's public surface.
pub use loom_core::{parse_force_discard, parse_retention_window};

use crate::InProcessBackend;

/// Environment variable holding the archived sub-agent retention window.
pub const ARCHIVE_RETENTION_SUB_AGENT_ENV: &str = "LOOM_ARCHIVE_RETENTION_SUB_AGENT";
/// The flag that carries the archived sub-agent retention window.
pub const ARCHIVE_RETENTION_SUB_AGENT_FLAG: &str = "--archive-retention-sub-agent";

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

/// Resolves the archive-retention policy from the explicit flags and their
/// environment fallback.
///
/// An explicit flag always wins over the variable it falls back to. `None`
/// means this start gave no retention input at all, so the policy stored in
/// durable state applies; a returned policy is the startup default, and a
/// missing, `off`, `never`, or zero window in it keeps retention disabled.
/// Either way an absent value never auto-deletes anything.
pub fn resolve_archive_retention(
    flag: Option<&str>,
    force_flag: Option<&str>,
    env: Option<&str>,
    force_env: Option<&str>,
) -> Result<Option<ArchiveRetentionPolicy>> {
    resolve_archive_retention_with_sub_agent(flag, force_flag, None, env, force_env, None)
}

/// [`resolve_archive_retention`] with the archived sub-agent window input. An
/// unset window keeps the 10 minute default; `off`, `never`, or zero disables it.
pub fn resolve_archive_retention_with_sub_agent(
    flag: Option<&str>,
    force_flag: Option<&str>,
    sub_agent_flag: Option<&str>,
    env: Option<&str>,
    force_env: Option<&str>,
    sub_agent_env: Option<&str>,
) -> Result<Option<ArchiveRetentionPolicy>> {
    if flag.is_none()
        && force_flag.is_none()
        && sub_agent_flag.is_none()
        && env.is_none()
        && force_env.is_none()
        && sub_agent_env.is_none()
    {
        return Ok(None);
    }
    let sub_agent_retention_ms = match sub_agent_flag {
        Some(value) => parse_retention_window(value, ARCHIVE_RETENTION_SUB_AGENT_FLAG)?,
        None => match sub_agent_env {
            Some(value) => parse_retention_window(value, ARCHIVE_RETENTION_SUB_AGENT_ENV)?,
            None => Some(DEFAULT_SUB_AGENT_RETENTION_MS),
        },
    };
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
    Ok(Some(ArchiveRetentionPolicy {
        retention_ms: retention_ms.filter(|retention_ms| *retention_ms > 0),
        force_discard_worktrees,
        sub_agent_retention_ms: sub_agent_retention_ms.filter(|value| *value > 0),
    }))
}

/// The input that decided the archive-retention policy a start applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveRetentionSource {
    /// A flag or environment variable given at this start. It overrides the
    /// persisted policy and is persisted in turn.
    Explicit,
    /// The policy stored in durable state by an earlier start or by a client.
    Persisted,
    /// Neither: retention is disabled.
    Default,
}

impl ArchiveRetentionSource {
    /// How the source is named in the startup log line.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Explicit => "an explicit flag or environment variable",
            Self::Persisted => "durable state",
            Self::Default => "the built-in default",
        }
    }
}

/// One-line description of a policy for the logs.
pub(crate) fn describe_archive_retention(policy: ArchiveRetentionPolicy) -> String {
    let sub_agent = match policy.sub_agent_retention() {
        Some(window) => format!("{} ms", window.as_millis()),
        None => "disabled".to_owned(),
    };
    match policy.retention() {
        Some(window) => format!(
            "{} ms window (force discard worktrees: {}; sub-agent window: {sub_agent})",
            window.as_millis(),
            policy.force_discard_worktrees
        ),
        None => format!(
            "disabled (force discard worktrees: {}; sub-agent window: {sub_agent})",
            policy.force_discard_worktrees
        ),
    }
}

/// Applies the startup precedence for the archive-retention policy and logs
/// which source won.
///
/// An explicit `--archive-retention`/`LOOM_ARCHIVE_RETENTION` (with the
/// force-discard flag or variable) wins over the persisted policy and is
/// persisted in turn, so the next start keeps it. Without explicit input the
/// policy loaded when the durable state was opened applies. Without either,
/// retention is disabled.
pub fn apply_startup_archive_retention(
    backend: &InProcessBackend,
    explicit: Option<ArchiveRetentionPolicy>,
) -> Result<ArchiveRetentionSource> {
    let source = match explicit {
        Some(policy) => {
            backend.set_archive_retention(policy)?;
            ArchiveRetentionSource::Explicit
        }
        None if backend.archive_retention_is_stored() => ArchiveRetentionSource::Persisted,
        None => ArchiveRetentionSource::Default,
    };
    log::info!(
        "[loom-server] archive retention policy from {}: {}",
        source.description(),
        describe_archive_retention(backend.archive_retention())
    );
    Ok(source)
}

/// Sweeps once before a server starts accepting clients, logging the active
/// policy and any failure instead of aborting the start.
pub fn sweep_archive_retention_at_startup(backend: &InProcessBackend) {
    let policy = backend.archive_retention();
    log::info!(
        "[loom-server] archive retention at startup: {}",
        describe_archive_retention(policy)
    );
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
/// unreachable database cannot stop retention or abort the server. Each round
/// reads the policy again, so a policy changed through the protocol applies from
/// the next round: a change does not start a sweep of its own.
///
/// The sweep is synchronous SQLite work, so each round runs on a blocking
/// thread: run inline it would stall the runtime worker that also polls the
/// shutdown signal and the remote listener.
pub async fn run_archive_retention_sweeps(backend: Arc<InProcessBackend>, interval: Duration) {
    run_periodic_sweeps(interval, move || backend.sweep_archive_retention()).await;
}

/// Owns the periodic archive-retention sweep on a dedicated thread.
///
/// A native client that embeds its backend in-process has no async runtime of
/// its own, so this owns one: it applies the stored policy, then sweeps
/// immediately and on every `interval` from a background thread, so the client
/// deletes expired archived sessions exactly like the standalone server. The
/// first sweep runs on that thread rather than on the caller, so a client does
/// not block its startup on a large database. Dropping the driver signals the
/// loop and joins the thread, so a shutdown stops sweeping before the backend
/// releases its database.
///
/// Starting is best-effort: a driver whose thread cannot start logs the reason
/// and leaves automatic deletion manual, matching how a failed sweep never
/// aborts a server.
pub struct ArchiveRetentionSweepDriver {
    stop: tokio::sync::watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ArchiveRetentionSweepDriver {
    /// Applies the retained durable policy and starts sweeping `backend`.
    pub fn start(backend: Arc<InProcessBackend>, interval: Duration) -> Self {
        if let Err(error) = apply_startup_archive_retention(&backend, None) {
            log::warn!(
                "[loom-server] archive retention sweep driver could not apply the stored policy: {}",
                error.message
            );
        }
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let sweep_backend = Arc::clone(&backend);
        let thread = std::thread::Builder::new()
            .name("loom-archive-retention".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        log::warn!(
                            "[loom-server] archive retention sweep driver could not start a runtime: {error}"
                        );
                        return;
                    }
                };
                // This runtime is private to the thread, so the sweep may run
                // inline on it.
                runtime.block_on(run_periodic_sweeps_until(interval, stop_rx, move || {
                    std::future::ready(sweep_backend.sweep_archive_retention())
                }));
            })
            .map_err(|error| {
                log::warn!(
                    "[loom-server] could not start the archive retention sweep thread: {error}"
                );
            })
            .ok();
        log::info!(
            "[loom-server] archive retention sweep driver started; interval {} s",
            interval.as_secs()
        );
        Self { stop, thread }
    }
}

impl Drop for ArchiveRetentionSweepDriver {
    fn drop(&mut self) {
        // A watch send both wakes a loop that is sleeping on a long interval and
        // marks the stop, so dropping the driver joins promptly.
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// [`run_archive_retention_sweeps`] with the sweep injected, so a test can watch
/// the periodic driver without a server.
pub(crate) async fn run_periodic_sweeps(
    interval: Duration,
    sweep: impl Fn() -> Result<ArchiveSweepReport> + Send + Sync + 'static,
) {
    // No stop channel: the caller drops the future to stop the driver. The
    // sender is kept alive for the loop's lifetime so the receiver never sees
    // the channel close.
    let (_stop, stop_rx) = tokio::sync::watch::channel(false);
    let sweep = Arc::new(sweep);
    run_periodic_sweeps_until(interval, stop_rx, move || {
        let sweep = Arc::clone(&sweep);
        async move {
            tokio::task::spawn_blocking(move || sweep())
                .await
                .unwrap_or_else(|error| {
                    Err(LoomError::new(
                        ErrorCode::Internal,
                        format!("archive retention sweep worker failed: {error}"),
                        true,
                    ))
                })
        }
    })
    .await;
}

/// [`run_periodic_sweeps`] with a stop channel, so a background driver can end
/// its loop and join its thread instead of being dropped.
async fn run_periodic_sweeps_until<F>(
    interval: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut sweep: impl FnMut() -> F,
) where
    F: std::future::Future<Output = Result<ArchiveSweepReport>>,
{
    // `tokio::time::interval` panics on a zero period, and the sweep must never
    // take the server down, so a caller-supplied zero becomes the smallest
    // useful period instead.
    let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // The first tick completes immediately, so a sweep runs as soon as the
        // driver is polled. Ticking and stopping are one select, so a stop
        // requested while the loop sleeps on a long interval ends it promptly.
        tokio::select! {
            _ = stop.changed() => {
                log::debug!("[loom-server] archive retention sweep driver stopping");
                return;
            }
            _ = ticker.tick() => {}
        }
        log::debug!("[loom-server] archive retention sweep round started");
        match sweep().await {
            Ok(report) if report.is_empty() => log::debug!(
                "[loom-server] archive retention sweep round had no candidate to act on"
            ),
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
