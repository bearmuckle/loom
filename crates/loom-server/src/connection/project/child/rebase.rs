use super::*;
use std::io::Read;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long one bounded check may run before the rebase stops it.
///
/// A check the rebase starts is bounded because a tool call holds the project
/// admission lock: a build that hangs must not hold the project's other children
/// back indefinitely.
pub(crate) const PROJECT_CHILD_REBASE_CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the readers of a finished check's output are given to reach the end
/// of its pipes, so a descendant that still holds them cannot stall the rebase.
const PROJECT_CHILD_CHECK_OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// The wait between two polls of a running check.
const PROJECT_CHILD_CHECK_POLL: Duration = Duration::from_millis(20);

/// Most bytes of one check's output that the rebase reports.
const MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES: usize = 2048;

/// Most paths a refusal names, so a wide change cannot turn one error into
/// pages of text.
const MAX_NAMED_PATHS: usize = 20;

/// One bounded check a rebase runs in the child checkout.
#[derive(Clone)]
pub(crate) struct ProjectChildRebaseCheck {
    name: String,
    /// The command line, as it can be reproduced by hand.
    command: String,
    program: String,
    args: Vec<String>,
    timeout: Duration,
    /// Whether the check applies only to a Cargo workspace, which is why a
    /// checkout without a manifest reports it as skipped.
    requires_cargo_manifest: bool,
    /// Why the check is reported as skipped instead of run. `None` runs it.
    skip: Option<String>,
}

/// The outcome of one check the rebase ran or skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectChildRebaseCheckState {
    Passed,
    Failed,
    TimedOut,
    Skipped,
    /// The check's command could not be started at all.
    Unavailable,
}

impl ProjectChildRebaseCheckState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Skipped => "skipped",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether this outcome means the rebased result was not validated.
    fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut | Self::Unavailable)
    }
}

/// One check's outcome, reported with the revision the rebase produced.
pub(crate) struct ProjectChildRebaseCheckOutcome {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) outcome: ProjectChildRebaseCheckState,
    /// The reason a check was skipped or the bounded tail of its output.
    pub(crate) detail: Option<String>,
}

/// What one rebase of a finished child's own commits changed.
pub(crate) struct ProjectChildRebaseReport {
    pub(crate) previous_base_revision: String,
    pub(crate) previous_result_revision: String,
    pub(crate) base_revision: String,
    pub(crate) result_revision: String,
    pub(crate) commit_ids_changed: bool,
    pub(crate) commits_before: usize,
    pub(crate) commits_after: usize,
    pub(crate) checks: Vec<ProjectChildRebaseCheckOutcome>,
    /// The timeout each check was given, so a reader knows the bound that
    /// applies.
    pub(crate) check_timeout_secs: u64,
}

impl InProcessConnection {
    /// Replays a finished code child's own commits onto a newer base.
    ///
    /// The child's branch is replayed inside its own checkout, so each of its
    /// commits keeps its author and message and no merge commit is created. The
    /// operation is refused, with the checkout and the parent left untouched,
    /// when the child has uncommitted work, when its commits are already
    /// contained in the target, when the target does not descend from the base
    /// the child's commits were created from, when a commit does not replay
    /// cleanly, and when the replay would touch a path the reviewed change never
    /// touched.
    ///
    /// The replayed result is then validated by the bounded checks this module
    /// documents. A check that fails stops the rebase and restores the child's
    /// branch, so a result that no longer formats is never reported as rebased.
    /// On success the record names the new base and result revisions and the
    /// rebase it performed.
    pub(crate) fn rebase_project_child(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        onto: &str,
    ) -> Result<(ProjectWorktreeRecord, ProjectChildRebaseReport)> {
        if !self.project_agent_permission_enabled_for_session(
            manager_session_id,
            Capability::ControlProjectChild,
            ProjectAgentPermission::ChildControl,
        )? {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "project child control grant is no longer valid",
                false,
            ));
        }
        let onto = onto.trim();
        if onto.is_empty() {
            return Err(LoomError::invalid_request(
                "rebase requires onto: a commit id in the parent repository, or `main`",
            ));
        }
        let admission = self.backend.admissions.project(project_id)?;
        let _admission = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project worktree lock was poisoned",
                true,
            )
        })?;
        let (task, mut worktree) =
            self.load_project_child_worktree(project_id, manager_session_id, task_id)?;
        if !task.code_change {
            return Err(LoomError::not_found("project child code task", task_id));
        }
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project worktrees require durable storage",
                false,
            )
        })?;
        self.ensure_project_child_rebase_finished(persistence, &task)?;
        if !matches!(
            worktree.status,
            ProjectWorktreeStatus::Ready | ProjectWorktreeStatus::Stale
        ) {
            return Err(LoomError::invalid_state(format!(
                "a project child checkout in {:?} state cannot be rebased",
                worktree.status
            )));
        }

        let child_git = self.open_project_child_worktree(&worktree)?;
        let status = child_git.status()?;
        if status.branch.as_deref() != Some(worktree.branch_name.as_str()) {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "project child checkout is no longer on its assigned branch",
                true,
            ));
        }
        if !status.clean {
            return Err(LoomError::conflict(format!(
                "project child checkout has uncommitted changes; commit or discard them before rebasing: {}",
                project_child_rebase_paths(status.files.iter().map(|file| &file.path))
            )));
        }
        let result_revision = status.head.ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                "project child checkout has no commit at HEAD",
                true,
            )
        })?;
        let previous_base_revision = worktree.base_revision.clone();
        if result_revision == previous_base_revision {
            return Err(LoomError::invalid_state(
                "project child has no committed change to rebase",
            ));
        }

        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let target = project_child_rebase_target(&parent_git, onto)?;
        if parent_git.is_ancestor_revision(&result_revision, &target)? {
            return Err(LoomError::invalid_state(format!(
                "the child's commits are already contained in {target}, so there is nothing to rebase"
            )));
        }
        if !parent_git.is_ancestor_revision(&previous_base_revision, &target)? {
            return Err(LoomError::invalid_state(format!(
                "{target} does not descend from the child's base {previous_base_revision}; replaying the child's commits would carry history it was not created from, so nothing was rebased"
            )));
        }

        // The reviewed set is the set of paths the child's committed change
        // touches. A replay applies the same commits, so it must not reach a
        // path outside that set.
        let reviewed_paths = project_child_rebase_stat_paths(&child_git, &previous_base_revision)?;

        let replay = match child_git.rebase_onto_revision(&target)? {
            loom_vcs::RebaseIntegrationOutcome::Rebased(revision) => revision.to_string(),
            loom_vcs::RebaseIntegrationOutcome::MergeRequired => {
                return Err(LoomError::invalid_state(
                    "the child's own commits contain a merge, so they cannot be replayed without dropping it; nothing was rebased",
                ));
            }
            loom_vcs::RebaseIntegrationOutcome::Conflicted(paths) => {
                return Err(LoomError::conflict(format!(
                    "the child's commits do not replay onto {target} cleanly; the child checkout and the parent were left unchanged, and the conflicting paths are: {}",
                    project_child_rebase_paths(paths.iter())
                )));
            }
        };

        let replay_paths = project_child_rebase_stat_paths(&child_git, &target)?;
        let gained = paths_outside_set(&replay_paths, &reviewed_paths);
        if !gained.is_empty() {
            self.restore_project_child_rebase(&child_git, &result_revision);
            return Err(LoomError::invalid_state(format!(
                "the rebase would change paths the reviewed commit never touched: {}; the child checkout was restored to {result_revision}",
                project_child_rebase_paths(gained.iter())
            )));
        }

        let checks = self.run_project_child_rebase_checks(&child_git);
        let failed = checks
            .iter()
            .filter(|check| check.outcome.is_failure())
            .map(|check| format!("{} ({})", check.name, check.outcome.as_str()))
            .collect::<Vec<_>>();
        if !failed.is_empty() {
            self.restore_project_child_rebase(&child_git, &result_revision);
            return Err(LoomError::conflict(format!(
                "the replayed commits did not pass their checks: {}; the child checkout was restored to {result_revision}, so nothing was rebased",
                failed.join(", ")
            )));
        }

        let commits_before = child_git
            .patch_commits_since(&previous_base_revision, &result_revision)?
            .len();
        let commits_after = child_git.patch_commits_since(&target, &replay)?.len();
        // A replay applies the same changes, so a path the reviewed change
        // touched can only disappear when the new base already carries it.
        let lost = paths_outside_set(&reviewed_paths, &replay_paths);

        worktree.base_revision = target.clone();
        worktree.result_revision = Some(replay.clone());
        worktree.rebase = Some(loom_core::ProjectWorktreeRebase {
            previous_base_revision: previous_base_revision.clone(),
            previous_result_revision: result_revision.clone(),
            base_revision: target.clone(),
            result_revision: replay.clone(),
            rebased_at: Timestamp::now(),
        });
        if worktree.status == ProjectWorktreeStatus::Stale {
            worktree.status = ProjectWorktreeStatus::Ready;
        }
        worktree.conflict_paths.clear();
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(&worktree)?;
        log::info!(
            "[loom-server] rebased project child task {} from {previous_base_revision} onto {target}: {result_revision} became {replay}",
            task.task_id
        );
        if !lost.is_empty() {
            log::warn!(
                "rebase of project child task {} dropped changes to paths the reviewed commit touched, because the new base already carries them: {}",
                task.task_id,
                project_child_rebase_paths(lost.iter())
            );
        }

        Ok((
            worktree,
            ProjectChildRebaseReport {
                previous_base_revision,
                previous_result_revision: result_revision.clone(),
                base_revision: target,
                result_revision: replay.clone(),
                commit_ids_changed: replay != result_revision,
                commits_before,
                commits_after,
                checks,
                check_timeout_secs: PROJECT_CHILD_REBASE_CHECK_TIMEOUT.as_secs(),
            },
        ))
    }

    /// Refuses a rebase of a child that has not finished.
    ///
    /// A live run may still commit to its checkout, so rebasing it would move a
    /// revision it is still writing; the manager waits for it to finish, or
    /// cancels it, and rebases what it committed.
    fn ensure_project_child_rebase_finished(
        &self,
        persistence: &dyn Persistence,
        task: &loom_core::DelegatedTaskRecord,
    ) -> Result<()> {
        if let Some(summary) =
            persistence.load_latest_run_summary_for_session(task.target_session_id)?
            && !is_terminal_agent_run_state(summary.snapshot.state)
        {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "the child is still running; wait for it to finish before rebasing its commits",
                false,
            ));
        }
        if !matches!(
            task.status,
            loom_core::DelegatedTaskStatus::Completed
                | loom_core::DelegatedTaskStatus::Failed
                | loom_core::DelegatedTaskStatus::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "the child has not finished, so it has no result to rebase",
                false,
            ));
        }
        Ok(())
    }

    /// Puts the child's checkout back on the revision it held before a replay,
    /// so a rebase that was refused never leaves the branch moved.
    ///
    /// A restore that fails is logged with the recovery-required state: the
    /// branch then holds the replayed revision, which the manager can see.
    fn restore_project_child_rebase(&self, child_git: &GitService, revision: &str) {
        if let Err(error) = child_git.restore_branch_to_revision(revision) {
            log::error!(
                "could not restore a project child checkout to {revision} after a refused rebase; it holds the replayed revision: {}",
                error.message
            );
        }
    }

    /// The bounded checks a rebase runs in the child checkout.
    ///
    /// The set is fixed and documented instead of the child's own last commands,
    /// which Loom does not record: formatting is checked because it is fast and
    /// deterministic, while a compile and a test run are reported as skipped
    /// because neither can be bounded to the crates the child touched from a
    /// tool call. A test may replace the set so it does not invoke a real
    /// toolchain.
    fn project_child_rebase_checks(&self, root: &Path) -> Vec<ProjectChildRebaseCheck> {
        #[cfg(test)]
        if let Some((program, args, timeout)) = self
            .backend
            .project_child_rebase_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return vec![ProjectChildRebaseCheck {
                name: "override".to_owned(),
                command: std::iter::once(program.clone())
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" "),
                program,
                args,
                timeout,
                requires_cargo_manifest: false,
                skip: None,
            }];
        }
        let documented = vec![
            ProjectChildRebaseCheck {
                name: "fmt".to_owned(),
                command: "cargo fmt --all -- --check".to_owned(),
                program: "cargo".to_owned(),
                args: vec![
                    "fmt".to_owned(),
                    "--all".to_owned(),
                    "--".to_owned(),
                    "--check".to_owned(),
                ],
                timeout: PROJECT_CHILD_REBASE_CHECK_TIMEOUT,
                requires_cargo_manifest: true,
                skip: None,
            },
            ProjectChildRebaseCheck {
                name: "clippy".to_owned(),
                command: "cargo clippy --workspace --all-targets -- -D warnings".to_owned(),
                program: "cargo".to_owned(),
                args: Vec::new(),
                timeout: PROJECT_CHILD_REBASE_CHECK_TIMEOUT,
                requires_cargo_manifest: true,
                skip: Some(
                    "not run: a workspace clippy run is not bounded to the crates this child touched, so the rebase never starts one; run it in the child checkout before integrating"
                        .to_owned(),
                ),
            },
            ProjectChildRebaseCheck {
                name: "test".to_owned(),
                command: "cargo test --workspace --locked".to_owned(),
                program: "cargo".to_owned(),
                args: Vec::new(),
                timeout: PROJECT_CHILD_REBASE_CHECK_TIMEOUT,
                requires_cargo_manifest: true,
                skip: Some(
                    "not run: a test suite is never started from a tool call; run it in the child checkout before integrating"
                        .to_owned(),
                ),
            },
        ];
        documented
            .into_iter()
            .map(|mut check| {
                if check.skip.is_none()
                    && check.requires_cargo_manifest
                    && !root.join("Cargo.toml").is_file()
                {
                    check.skip = Some(
                        "not run: the child checkout has no Cargo.toml, so it is not a Cargo workspace"
                            .to_owned(),
                    );
                }
                check
            })
            .collect()
    }

    /// Runs every check the rebase reports, in order, each within its bound.
    fn run_project_child_rebase_checks(
        &self,
        child_git: &GitService,
    ) -> Vec<ProjectChildRebaseCheckOutcome> {
        let root = child_git.root();
        self.project_child_rebase_checks(root)
            .iter()
            .map(|check| run_project_child_check(check, root))
            .collect()
    }
}

/// The sorted, de-duplicated paths a change touches from `base_revision`.
fn project_child_rebase_stat_paths(
    child_git: &GitService,
    base_revision: &str,
) -> Result<Vec<String>> {
    let mut paths = child_git
        .review_stat_from_revision(base_revision)?
        .iter()
        .map(|stat| stat.path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The revision a rebase target names.
///
/// `main` names the parent checkout's current head, which is the revision an
/// integration would land the child on. Any other target has to be a commit id
/// the parent repository resolves.
fn project_child_rebase_target(parent_git: &GitService, onto: &str) -> Result<String> {
    if onto == "main" {
        return parent_git.status()?.head.ok_or_else(|| {
            LoomError::invalid_state("the parent repository has no commit at HEAD to rebase onto")
        });
    }
    parent_git.resolve_revision(onto).map_err(|error| {
        LoomError::invalid_request(format!(
            "onto '{onto}' is not a revision of the parent repository: {}",
            error.message
        ))
    })
}

/// A bounded, sorted, de-duplicated list of changed paths.
fn project_child_rebase_paths<'a>(paths: impl Iterator<Item = &'a String>) -> String {
    let mut paths = paths.cloned().collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return "none".to_owned();
    }
    let remaining = paths.len().saturating_sub(MAX_NAMED_PATHS);
    paths.truncate(MAX_NAMED_PATHS);
    let named = paths.join(", ");
    if remaining > 0 {
        format!("{named} and {remaining} more")
    } else {
        named
    }
}

/// The paths of `candidate` that `known` does not carry, in the order of
/// `candidate`.
fn paths_outside_set(candidate: &[String], known: &[String]) -> Vec<String> {
    candidate
        .iter()
        .filter(|path| !known.contains(*path))
        .cloned()
        .collect()
}

/// Runs one check in the child checkout within its timeout.
///
/// The check runs in its own process group so a timeout stops the whole tree,
/// its output is bounded, and the outcome names exactly what ran.
fn run_project_child_check(
    check: &ProjectChildRebaseCheck,
    root: &Path,
) -> ProjectChildRebaseCheckOutcome {
    if let Some(reason) = &check.skip {
        return ProjectChildRebaseCheckOutcome {
            name: check.name.clone(),
            command: check.command.clone(),
            outcome: ProjectChildRebaseCheckState::Skipped,
            detail: Some(reason.clone()),
        };
    }
    let mut command = std::process::Command::new(&check.program);
    command
        .args(&check.args)
        .current_dir(root)
        // A check must never block waiting for terminal input.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The whole process tree is stopped as one group when the timeout expires.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ProjectChildRebaseCheckOutcome {
                name: check.name.clone(),
                command: check.command.clone(),
                outcome: ProjectChildRebaseCheckState::Unavailable,
                detail: Some(format!("could not start '{}': {error}", check.program)),
            };
        }
    };
    let stdout = spawn_project_child_check_reader(child.stdout.take());
    let stderr = spawn_project_child_check_reader(child.stderr.take());
    let deadline = Instant::now() + check.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                terminate_project_child_check(&mut child);
                break None;
            }
            Ok(None) => std::thread::sleep(PROJECT_CHILD_CHECK_POLL),
            Err(error) => {
                terminate_project_child_check(&mut child);
                return ProjectChildRebaseCheckOutcome {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    outcome: ProjectChildRebaseCheckState::Unavailable,
                    detail: Some(format!("could not wait for '{}': {error}", check.program)),
                };
            }
        }
    };
    let output = project_child_check_detail(
        &collect_project_child_check_output(stdout),
        &collect_project_child_check_output(stderr),
    );
    let outcome = if timed_out {
        ProjectChildRebaseCheckState::TimedOut
    } else if status.is_some_and(|status| status.success()) {
        ProjectChildRebaseCheckState::Passed
    } else {
        ProjectChildRebaseCheckState::Failed
    };
    let detail = match (outcome, status) {
        (ProjectChildRebaseCheckState::Passed, _) => None,
        (_, Some(status)) if !timed_out => Some(format!("{status}: {output}")),
        _ => Some(format!("stopped after {:?}: {output}", check.timeout)),
    };
    ProjectChildRebaseCheckOutcome {
        name: check.name.clone(),
        command: check.command.clone(),
        outcome,
        detail,
    }
}

/// Reads one check output pipe on its own thread and sends what it read.
///
/// The read is what keeps a chatty check from blocking on a full pipe. It ends
/// at the pipe's end of file, which a stopped check reaches once it is killed.
fn spawn_project_child_check_reader<R>(pipe: Option<R>) -> Option<mpsc::Receiver<String>>
where
    R: Read + Send + 'static,
{
    let mut pipe = pipe?;
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("loom-project-child-check-output".to_owned())
        .spawn(move || {
            let mut collected = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let room = MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES
                            .saturating_sub(collected.len())
                            .min(read);
                        collected.extend_from_slice(&buffer[..room]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = sender.send(String::from_utf8_lossy(&collected).into_owned());
        })
        .ok()?;
    Some(receiver)
}

/// Collects what a reader thread read, waiting at most the output grace.
fn collect_project_child_check_output(receiver: Option<mpsc::Receiver<String>>) -> String {
    receiver
        .and_then(|receiver| receiver.recv_timeout(PROJECT_CHILD_CHECK_OUTPUT_GRACE).ok())
        .unwrap_or_default()
}

/// The bounded tail of a check's output, for the report.
fn project_child_check_detail(stdout: &str, stderr: &str) -> String {
    let mut detail = String::new();
    for part in [stderr.trim(), stdout.trim()] {
        if part.is_empty() {
            continue;
        }
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str(part);
    }
    if detail.is_empty() {
        return "no output".to_owned();
    }
    if detail.len() <= MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES {
        return detail;
    }
    let mut start = detail.len() - MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES;
    while start < detail.len() && !detail.is_char_boundary(start) {
        start += 1;
    }
    format!("... [loom: output truncated] {}", &detail[start..])
}

/// Stops a running check, including the process group it leads.
fn terminate_project_child_check(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The check was started in its own process group, so the negative pid
        // names the whole tree it spawned.
        if let Ok(group) = i32::try_from(child.id()) {
            // SAFETY: the signal goes to a process group this call created;
            // `kill` is async-signal-safe and has no memory-safety contract.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_NAMED_PATHS, MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES, ProjectChildRebaseCheckState,
        paths_outside_set, project_child_check_detail, project_child_rebase_paths,
    };

    #[test]
    fn changed_path_sets_are_named_in_order_without_duplicates_and_stay_bounded() {
        let paths = [
            "b.txt".to_owned(),
            "a.txt".to_owned(),
            "a.txt".to_owned(),
            "c.txt".to_owned(),
        ];
        assert_eq!(
            project_child_rebase_paths(paths.iter()),
            "a.txt, b.txt, c.txt"
        );
        assert_eq!(
            project_child_rebase_paths(Vec::<String>::new().iter()),
            "none"
        );

        let many = (0..MAX_NAMED_PATHS + 3)
            .map(|index| format!("path-{index:02}.txt"))
            .collect::<Vec<_>>();
        let named = project_child_rebase_paths(many.iter());
        assert!(named.ends_with("and 3 more"), "{named}");
        assert_eq!(named.matches(',').count(), MAX_NAMED_PATHS - 1);
    }

    #[test]
    fn a_path_set_is_compared_without_order_or_duplicates() {
        let known = vec!["a.txt".to_owned(), "b.txt".to_owned()];
        assert_eq!(
            paths_outside_set(&["c.txt".to_owned(), "a.txt".to_owned()], &known),
            vec!["c.txt".to_owned()],
        );
        assert!(paths_outside_set(&known, &known).is_empty());
        assert_eq!(
            paths_outside_set(&known, &[]),
            vec!["a.txt".to_owned(), "b.txt".to_owned()],
        );
    }

    #[test]
    fn check_output_is_reported_as_a_bounded_tail() {
        assert_eq!(project_child_check_detail("", ""), "no output");
        assert_eq!(project_child_check_detail("out\n", "err\n"), "err\nout");
        let long = "x".repeat(MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES + 100);
        let detail = project_child_check_detail(&long, "");
        assert!(
            detail.starts_with("... [loom: output truncated] "),
            "{detail}"
        );
        assert!(
            detail.len() <= MAX_PROJECT_CHILD_CHECK_OUTPUT_BYTES + 40,
            "the detail stays bounded: {}",
            detail.len()
        );
    }

    #[test]
    fn check_states_report_their_name_and_whether_the_result_was_validated() {
        assert_eq!(ProjectChildRebaseCheckState::Passed.as_str(), "passed");
        assert_eq!(ProjectChildRebaseCheckState::Failed.as_str(), "failed");
        assert_eq!(ProjectChildRebaseCheckState::TimedOut.as_str(), "timed_out");
        assert_eq!(ProjectChildRebaseCheckState::Skipped.as_str(), "skipped");
        assert_eq!(
            ProjectChildRebaseCheckState::Unavailable.as_str(),
            "unavailable"
        );
        assert!(!ProjectChildRebaseCheckState::Passed.is_failure());
        assert!(!ProjectChildRebaseCheckState::Skipped.is_failure());
        assert!(ProjectChildRebaseCheckState::Failed.is_failure());
        assert!(ProjectChildRebaseCheckState::TimedOut.is_failure());
        assert!(ProjectChildRebaseCheckState::Unavailable.is_failure());
    }
}
