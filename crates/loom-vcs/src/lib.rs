use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use git2::{
    BranchType, Cred, DiffFormat, DiffOptions, Direction, FetchOptions, MergeOptions, Oid,
    ProxyOptions, PushOptions, RemoteCallbacks, Repository, RepositoryState, Signature, Status,
    StatusOptions, WorktreeAddOptions, WorktreeLockStatus, WorktreePruneOptions,
    build::{CheckoutBuilder, RepoBuilder},
};
use loom_core::{ErrorCode, LoomError, Result, Timestamp};
pub use loom_core::{
    GitBranch, GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus,
    GitFileStatusKind, GitRepositoryStatus,
};

#[derive(Clone, Debug)]
pub struct GitService {
    root: PathBuf,
}

/// Outcome of integrating a reviewed child revision into a parent checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeIntegrationOutcome {
    /// The parent checkout already contains the child revision.
    AlreadyPresent,
    /// The child revision was integrated by fast-forward.
    FastForward(Oid),
    /// The child revision was integrated with a two-parent merge commit.
    Merged(Oid),
    /// The parent and child conflict; no branch, index, or working tree change
    /// was made and the conflicting paths are reported.
    Conflicted(Vec<String>),
}

/// Outcome of replaying a checkout's own commits onto another revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RebaseIntegrationOutcome {
    /// The own commits were replayed and the checked-out branch now points at
    /// the replayed tip. The tip is the target itself when every replayed
    /// commit had no change left to apply.
    Rebased(Oid),
    /// The own commits include a merge, so replaying them would drop that
    /// merge's resolution; the caller must integrate with a merge instead.
    MergeRequired,
    /// A replayed change does not apply cleanly; the branch, index, and
    /// working tree are untouched and the conflicting paths are reported.
    Conflicted(Vec<String>),
}

/// Deadline applied by callers that have no tighter requirement for a bounded
/// remote refresh.
pub const DEFAULT_REMOTE_REFRESH_DEADLINE: Duration = Duration::from_secs(30);

/// Outcome of [`GitService::create_mirror`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MirrorCreation {
    /// This call cloned the mirror from the source it was given, so the
    /// mirror's heads are the source's heads as of that clone.
    Created,
    /// A mirror already existed at the requested path and was left untouched:
    /// this call performed no clone and therefore knows nothing new about the
    /// mirror's contents.
    Existing,
}

/// How far one local revision is ahead of and behind the default branch a
/// remote fetch resolved.
///
/// `default_branch_revision` is the tip that the fetch performed for this
/// report resolved, so the report is a statement about `fetched_at` and never
/// about a cached `refs/remotes/origin/*` ref. A caller that needs to decide
/// whether a checkout is current can therefore trust the answer without
/// fetching again itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRefreshReport {
    /// Remote that was fetched; currently always `origin`.
    pub remote: String,
    /// Default branch name the fetch resolved for that remote.
    pub default_branch: String,
    /// Tip of the default branch as resolved by that fetch.
    pub default_branch_revision: String,
    /// Local revision the drift was measured against.
    pub local_revision: String,
    /// Commits the local revision has that the default branch does not.
    pub ahead: usize,
    /// Commits the default branch has that the local revision does not.
    pub behind: usize,
    /// When the fetch behind this report finished.
    pub fetched_at: Timestamp,
}

/// One push request against the `origin` remote.
///
/// `remote_branch` names the remote ref to update when it differs from the
/// local branch name. `lease` turns the update into a force-with-lease rewrite
/// of the target ref. `dry_run` performs every read a push performs and reports
/// what it would change without sending a ref update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BranchPush<'a> {
    /// Local branch whose tip is pushed.
    pub branch: &'a str,
    /// Remote branch to update; the local branch name when omitted.
    pub remote_branch: Option<&'a str>,
    /// Revision the caller believes the remote ref is at. Supplying it asks for
    /// a force-with-lease rewrite instead of a fast-forward-only push.
    pub lease: Option<&'a str>,
    /// Report the update without performing it.
    pub dry_run: bool,
}

/// What one push or one remote ref deletion did to the `origin` remote.
///
/// Every field describes the update this call performed or would perform,
/// never what a cached `refs/remotes/origin/*` ref says afterwards:
/// `previous_revision` comes from the refs the remote itself advertised, so a
/// checkout that has been idle for hours cannot report a stale revision as the
/// one it replaced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushReport {
    /// Full ref name the call updated, deleted, or would update, such as
    /// `refs/heads/loom/topic` or `refs/tags/v1`.
    pub refname: String,
    /// Revision the remote ref held before this call, or `None` when the remote
    /// advertised no such ref, so the push created it.
    pub previous_revision: Option<String>,
    /// Revision the pushed ref points at once the push succeeded, or `None` for
    /// a delete, which leaves no revision behind.
    pub revision: Option<String>,
    /// Commits `revision` carries that `previous_revision` did not, counted over
    /// the range `previous_revision..revision`. `None` exactly when there is no
    /// previous revision (a ref that did not exist has no previous side of the
    /// range to count from), when the previous side cannot be counted, or for a
    /// delete.
    pub commits: Option<usize>,
    /// Whether the pushed tip equals or descends from the previous remote
    /// revision. `Some` only for a branch push that has a previous remote
    /// revision: a new ref, a tag push, and a delete have no fast-forward
    /// question to answer.
    pub fast_forward: Option<bool>,
    /// Whether this call deleted the ref.
    pub deleted: bool,
    /// Whether this call reported the change without performing it.
    pub dry_run: bool,
}

/// One ref the `origin` remote advertises.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRef {
    /// Full ref name, such as `refs/heads/main` or `refs/tags/v1`.
    pub refname: String,
    /// Revision the remote advertises for that ref.
    pub revision: String,
}

/// How far one local branch is ahead of and behind the head of the same name on
/// the `origin` remote, as resolved by a fetch performed for the report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBranchTracking {
    /// Branch name, which is both the local branch and the remote head name.
    pub branch: String,
    /// Tip of the local branch.
    pub local_revision: String,
    /// Tip of the remote head, as resolved by that fetch.
    pub remote_revision: String,
    /// Commits the local branch has that the remote head does not.
    pub ahead: usize,
    /// Commits the remote head has that the local branch does not.
    pub behind: usize,
}

/// The refs the `origin` remote advertises, plus tracking for one local branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRefsReport {
    /// Remote that was read; currently always `origin`.
    pub remote: String,
    /// Heads the remote advertises, sorted by ref name.
    pub heads: Vec<RemoteRef>,
    /// Tags the remote advertises, sorted by ref name, without peeled entries.
    pub tags: Vec<RemoteRef>,
    /// Ahead/behind of the requested local branch against the remote head of the
    /// same name, when both exist.
    pub tracking: Option<RemoteBranchTracking>,
}

impl GitService {
    pub fn clone_from(
        source: impl AsRef<str>,
        destination: impl AsRef<Path>,
        revision: Option<&str>,
    ) -> Result<Self> {
        Self::clone_from_authenticated(source, destination, revision, None)
    }

    pub fn clone_from_authenticated(
        source: impl AsRef<str>,
        destination: impl AsRef<Path>,
        revision: Option<&str>,
        token: Option<&str>,
    ) -> Result<Self> {
        let destination = destination.as_ref();
        if destination.exists() {
            return Err(LoomError::conflict(format!(
                "repository checkout destination '{}' already exists",
                destination.display()
            )));
        }
        let mut fetch_options = FetchOptions::new();
        if let Some(token) = token {
            let token = token.to_owned();
            let mut callbacks = RemoteCallbacks::new();
            callbacks.credentials(move |url, username_from_url, allowed_types| {
                credential_for(url, username_from_url, allowed_types, &token)
            });
            fetch_options.remote_callbacks(callbacks);
        }
        fetch_options.proxy_options(remote_proxy_options());
        let mut builder = RepoBuilder::new();
        builder.fetch_options(fetch_options);
        let repository = builder
            .clone(source.as_ref(), destination)
            .map_err(|error| git_error("could not clone repository", error))?;
        Self::checkout_cloned_revision(&repository, revision)?;
        Self::open(destination)
    }

    /// Clones a working checkout from a local repository (typically a node's
    /// cached mirror) without any network access, then points `origin` back at
    /// the canonical `origin_url` so pushes and pulls still target the host.
    pub fn clone_from_local(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        revision: Option<&str>,
        origin_url: &str,
    ) -> Result<Self> {
        let destination = destination.as_ref();
        if destination.exists() {
            return Err(LoomError::conflict(format!(
                "repository checkout destination '{}' already exists",
                destination.display()
            )));
        }
        let source = source.as_ref().to_str().ok_or_else(|| {
            LoomError::invalid_request("local repository mirror path is not valid UTF-8")
        })?;
        let repository = Repository::clone(source, destination)
            .map_err(|error| git_error("could not clone local repository cache", error))?;
        set_origin_url(&repository, origin_url)?;
        Self::checkout_cloned_revision(&repository, revision)?;
        Self::open(destination)
    }

    /// Creates a bare mirror of a local checkout at `mirror`, used as the node
    /// cache that later sessions can clone from. The mirror's `origin` points
    /// at `origin_url` so its identity matches the remote repository.
    ///
    /// A mirror that already exists is left exactly as it is, and the returned
    /// [`MirrorCreation::Existing`] says so: this call then knows nothing about
    /// how old the mirror's heads are, and a caller that records freshness must
    /// not treat the result as a fresh clone.
    pub fn create_mirror(
        source: impl AsRef<Path>,
        mirror: impl AsRef<Path>,
        origin_url: &str,
    ) -> Result<MirrorCreation> {
        let mirror = mirror.as_ref();
        if mirror.exists() {
            return Ok(MirrorCreation::Existing);
        }
        if let Some(parent) = mirror.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create repository mirror parent: {error}"),
                    false,
                )
            })?;
        }
        let source = source.as_ref().to_str().ok_or_else(|| {
            LoomError::invalid_request("repository checkout path is not valid UTF-8")
        })?;
        let mut builder = RepoBuilder::new();
        builder.bare(true);
        let repository = builder
            .clone(source, mirror)
            .map_err(|error| git_error("could not create repository mirror", error))?;
        set_origin_url(&repository, origin_url)?;
        Ok(MirrorCreation::Created)
    }

    /// Fetches `origin` into a bare mirror and moves the mirror's own heads to
    /// the tips that fetch resolved.
    ///
    /// A checkout cloned from a mirror takes its `refs/remotes/origin/*` from
    /// the mirror's `refs/heads/*`, so a mirror that is only fetched leaves
    /// those frozen at the clone it was created from and every later checkout
    /// starts from that tip. This call therefore runs the bounded fetch of
    /// [`Self::refresh_origin_default_branch`] (the same worker thread, deadline,
    /// credentials and report) and then advances the mirror's heads locally to
    /// the fetched remote-tracking tips, which costs no network access.
    ///
    /// The mirror must be bare: its heads are moved with a forced reference
    /// update, which is only safe when nothing is checked out.
    pub fn refresh_mirror_heads(
        &self,
        token: Option<&str>,
        deadline: Duration,
    ) -> Result<RemoteRefreshReport> {
        if !self.repository()?.is_bare() {
            return Err(LoomError::invalid_request(
                "refreshing mirror heads requires a bare repository",
            ));
        }
        let report = self.refresh_origin_default_branch(None, token, deadline)?;
        self.advance_mirror_heads()?;
        Ok(report)
    }

    /// Points every head a mirror holds a fetched `origin` tip for at that tip.
    ///
    /// Returns how many heads were advanced. Symbolic remote refs, such as
    /// `refs/remotes/origin/HEAD`, are skipped because they hold no tip of their
    /// own.
    fn advance_mirror_heads(&self) -> Result<usize> {
        let repository = self.repository()?;
        let mut fetched = Vec::new();
        let references = repository
            .references_glob("refs/remotes/origin/*")
            .map_err(|error| git_error("could not list the fetched remote refs", error))?;
        for reference in references {
            let reference = reference
                .map_err(|error| git_error("could not read a fetched remote ref", error))?;
            let name = reference.name().ok().and_then(strip_origin_prefix);
            if let (Some(name), Some(target)) = (name, reference.target()) {
                fetched.push((name.to_owned(), target));
            }
        }
        let advanced = fetched.len();
        for (name, target) in fetched {
            repository
                .reference(
                    &format!("refs/heads/{name}"),
                    target,
                    true,
                    "loom mirror refresh",
                )
                .map_err(|error| {
                    git_error(
                        &format!("could not advance the mirror branch '{name}'"),
                        error,
                    )
                })?;
        }
        Ok(advanced)
    }

    /// Whether `revision` resolves in this repository, either directly or as
    /// `refs/remotes/origin/<revision>`.
    ///
    /// This is the resolution [`Self::checkout_cloned_revision`] performs, so a
    /// caller can tell before cloning whether the source it is about to clone
    /// from can satisfy a requested revision.
    pub fn resolves_revision(&self, revision: &str) -> bool {
        let Ok(repository) = self.repository() else {
            return false;
        };
        repository.revparse_single(revision).is_ok()
            || repository
                .revparse_single(&format!("refs/remotes/origin/{revision}"))
                .is_ok()
    }

    fn checkout_cloned_revision(repository: &Repository, revision: Option<&str>) -> Result<()> {
        if let Some(revision) = revision {
            let object = repository
                .revparse_single(revision)
                .or_else(|_| repository.revparse_single(&format!("refs/remotes/origin/{revision}")))
                .map_err(|error| {
                    git_error(
                        &format!("could not resolve requested repository revision '{revision}'"),
                        error,
                    )
                })?;
            let commit = object.peel_to_commit().map_err(|error| {
                git_error("requested repository revision is not a commit", error)
            })?;
            repository
                .checkout_tree(commit.as_object(), None)
                .map_err(|error| git_error("could not check out requested revision", error))?;
            repository
                .set_head_detached(commit.id())
                .map_err(|error| git_error("could not set detached repository head", error))?;
        }
        Ok(())
    }

    pub fn init(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("Git root '{}' is not a directory", root.display()),
                false,
            ));
        }
        let root = fs::canonicalize(&root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve Git root '{}': {error}", root.display()),
                false,
            )
        })?;
        Repository::init(&root).map_err(|error| git_error("could not initialize Git", error))?;
        Self::open(root)
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        if !requested.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("Git root '{}' is not a directory", requested.display()),
                false,
            ));
        }
        let root = fs::canonicalize(&requested).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "could not resolve Git root '{}': {error}",
                    requested.display()
                ),
                false,
            )
        })?;
        Repository::open(&root)
            .map_err(|error| git_error("could not open Git repository", error))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository's effective commit identity (`user.name`/`user.email`),
    /// including any inherited global or system configuration.
    ///
    /// Returns `None` when either half is unset or blank, because Git cannot
    /// create a commit without both.
    pub fn configured_identity(&self) -> Result<Option<(String, String)>> {
        let repository = self.repository()?;
        let name = repository
            .config()
            .and_then(|config| config.get_string("user.name"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        let email = repository
            .config()
            .and_then(|config| config.get_string("user.email"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        Ok(match (name, email) {
            (Some(name), Some(email)) => Some((name, email)),
            _ => None,
        })
    }

    /// Sets the repository-local commit identity so commits created in this
    /// checkout are attributed to `name`/`email` instead of whatever the host
    /// environment happens to provide.
    pub fn set_identity(&self, name: &str, email: &str) -> Result<()> {
        if name.trim().is_empty() || email.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "commit identity name and email must not be empty",
            ));
        }
        let repository = self.repository()?;
        let mut config = repository
            .config()
            .map_err(|error| git_error("could not read Git configuration", error))?;
        config
            .set_str("user.name", name)
            .and_then(|()| config.set_str("user.email", email))
            .map_err(|error| git_error("could not set the repository commit identity", error))
    }

    pub fn status(&self) -> Result<GitRepositoryStatus> {
        let repository = self.repository()?;
        let branch = repository
            .head()
            .ok()
            .and_then(|head| head.shorthand().ok().map(str::to_owned));
        let head = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| oid.to_string());
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true)
            .renames_index_to_workdir(true);
        let statuses = repository
            .statuses(Some(&mut options))
            .map_err(|error| git_error("could not read Git status", error))?;
        let index_counts = count_diff_lines(&repository, true)?;
        let worktree_counts = count_diff_lines(&repository, false)?;
        let mut files = Vec::new();
        for entry in &statuses {
            let status = entry.status();
            let path = entry
                .path()
                .map_err(|error| git_error("Git status contained invalid UTF-8", error))?
                .to_owned();
            let original_path = entry
                .head_to_index()
                .or_else(|| entry.index_to_workdir())
                .and_then(|delta| delta.old_file().path())
                .filter(|old_path| *old_path != Path::new(&path))
                .map(|old_path| old_path.to_string_lossy().replace('\\', "/"));
            files.push(GitFileStatus {
                index_additions: index_counts.get(&path).map_or(0, |counts| counts.0),
                index_deletions: index_counts.get(&path).map_or(0, |counts| counts.1),
                worktree_additions: worktree_counts.get(&path).map_or(0, |counts| counts.0),
                worktree_deletions: worktree_counts.get(&path).map_or(0, |counts| counts.1),
                path,
                original_path,
                index: status_kind(status, true),
                worktree: status_kind(status, false),
                conflicted: status.is_conflicted(),
            });
        }
        let conflicts = files
            .iter()
            .filter(|file| file.conflicted)
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        Ok(GitRepositoryStatus {
            root: self.root.display().to_string(),
            branch,
            head,
            clean: files.is_empty(),
            files,
            conflicts,
            captured_at: Timestamp::now(),
        })
    }

    pub fn diff(&self, path: Option<&str>, staged: bool) -> Result<GitDiff> {
        self.render_diff(path, staged, None)
    }

    /// Renders a diff bounded to `max_bytes`.
    ///
    /// The bound keeps whole lines and ends on a line boundary. Where it cuts
    /// through a hunk, that hunk declares the lines it actually carries, so a
    /// bounded render stays a valid patch and `hunks` describes exactly what
    /// `patch` contains.
    pub fn diff_limited(
        &self,
        path: Option<&str>,
        staged: bool,
        max_bytes: usize,
    ) -> Result<GitDiff> {
        self.render_diff(path, staged, Some(max_bytes))
    }

    fn render_diff(
        &self,
        path: Option<&str>,
        staged: bool,
        max_bytes: Option<usize>,
    ) -> Result<GitDiff> {
        let normalized = path.map(|path| self.validate_path(path)).transpose()?;
        let repository = self.repository()?;
        let mut options = DiffOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true);
        if let Some(path) = normalized.as_deref() {
            options.pathspec(path);
        }
        let diff = if staged {
            let head = repository
                .head()
                .ok()
                .and_then(|head| head.peel_to_tree().ok());
            repository
                .diff_tree_to_index(head.as_ref(), None, Some(&mut options))
                .map_err(|error| git_error("could not create staged Git diff", error))?
        } else {
            repository
                .diff_index_to_workdir(None, Some(&mut options))
                .map_err(|error| git_error("could not create Git diff", error))?
        };
        render_git_diff(diff, normalized, staged, max_bytes)
    }

    /// Compares a base commit with the current index and working tree.
    ///
    /// The output is a bounded prefix of the rendered patch and hunks. A line
    /// that would exceed `max_bytes` is omitted whole and marks the result as
    /// truncated. `staged` is false because the result combines staged and
    /// unstaged changes relative to the requested base revision.
    pub fn diff_from_revision(&self, base_revision: &str, max_bytes: usize) -> Result<GitDiff> {
        let base_oid = Oid::from_str(base_revision).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid Git base revision '{base_revision}': {error}"
            ))
        })?;
        let repository = self.repository()?;
        let base = repository
            .find_commit(base_oid)
            .map_err(|error| git_error("could not resolve Git diff base commit", error))?;
        let tree = base
            .tree()
            .map_err(|error| git_error("could not read Git diff base tree", error))?;
        let mut options = DiffOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true);
        let diff = repository
            .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut options))
            .map_err(|error| git_error("could not create Git diff from base revision", error))?;
        render_git_diff(diff, None, false, Some(max_bytes))
    }

    pub fn branches(&self) -> Result<Vec<GitBranch>> {
        let repository = self.repository()?;
        let branches = repository
            .branches(Some(BranchType::Local))
            .map_err(|error| git_error("could not list Git branches", error))?;
        let mut result = Vec::new();
        for branch in branches {
            let (branch, _) =
                branch.map_err(|error| git_error("could not read Git branch", error))?;
            let name = branch
                .name()
                .map_err(|error| git_error("Git branch contained invalid UTF-8", error))?
                .unwrap_or_default()
                .to_owned();
            if name.is_empty() {
                continue;
            }
            let upstream = branch
                .upstream()
                .ok()
                .and_then(|upstream| upstream.name().ok().flatten().map(str::to_owned));
            result.push(GitBranch {
                name,
                current: branch.is_head(),
                upstream,
            });
        }
        Ok(result)
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self
            .repository()?
            .head()
            .ok()
            .filter(|head| head.is_branch())
            .and_then(|head| head.shorthand().ok().map(str::to_owned)))
    }

    /// Returns the configured fetch URL for a remote, if the remote exists.
    pub fn remote_url(&self, name: &str) -> Result<Option<String>> {
        let repository = self.repository()?;
        match repository.find_remote(name) {
            Ok(remote) => remote
                .url()
                .map(|url| Some(url.to_owned()))
                .map_err(|error| git_error("could not read Git remote URL", error)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(git_error("could not read Git remote", error)),
        }
    }

    /// Fetches `origin` with a bounded deadline and reports the remote's
    /// default-branch tip together with how far `local_revision` (the
    /// checkout's `HEAD` when omitted) is ahead of and behind it.
    ///
    /// The report describes the fetch performed by this call, never a cached
    /// `refs/remotes/origin/*` ref, so a checkout that has been idle for hours
    /// cannot conclude from this report that its base matches upstream. A
    /// failed or expired refresh returns an error instead of a cached answer;
    /// the caller decides how to degrade.
    ///
    /// The fetch runs on a worker thread that the caller waits at most
    /// `deadline` for, so a stalled remote delays the caller by at most that
    /// deadline and then reports [`ErrorCode::DeadlineExceeded`]. The worker is
    /// detached when the deadline expires; it holds no lock and its eventual
    /// result is discarded. A zero deadline never starts a worker, because its
    /// result could not be awaited and the fetch would only mutate the
    /// checkout after the caller had already been told the answer is unknown.
    ///
    /// `token` authenticates HTTPS remotes the same way as
    /// [`Self::push_branch_authenticated`] and is never written to the
    /// repository configuration.
    pub fn refresh_origin_default_branch(
        &self,
        local_revision: Option<&str>,
        token: Option<&str>,
        deadline: Duration,
    ) -> Result<RemoteRefreshReport> {
        if deadline.is_zero() {
            return Err(remote_refresh_deadline_error(deadline));
        }
        let root = self.root.clone();
        let local_revision = local_revision.map(str::to_owned);
        let token = token.map(str::to_owned);
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("loom-vcs-remote-refresh".to_owned())
            .spawn(move || {
                let report =
                    fetch_origin_default_branch(&root, local_revision.as_deref(), token.as_deref());
                let _ = sender.send(report);
            })
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not start the bounded remote refresh: {error}"),
                    false,
                )
            })?;
        await_remote_refresh(&receiver, deadline)
    }

    /// Pushes a local branch to the `origin` remote using the supplied access
    /// token and reports what the push changed.
    ///
    /// The branch must already exist and point at a commit. `push.remote_branch`
    /// names the remote ref to update when it differs from the local branch name.
    /// The update is fast-forward-only unless `push.lease` is supplied: the
    /// remote rejects a non-fast-forward push, and a lease turns the push into a
    /// force-with-lease rewrite of the target ref.
    ///
    /// Before pushing, the refs the remote advertises are read on a bounded
    /// worker (the deadline, credentials, and proxy options of
    /// [`Self::refresh_origin_default_branch`]), and the revision that read
    /// resolves for the target ref is what the report's previous revision
    /// states, so the report never describes a cached `refs/remotes/origin/*`
    /// ref. A read that does not finish within `deadline` fails the push instead
    /// of reporting an unknown update. With `push.dry_run` the same reads are
    /// performed and the same report is returned, and no ref update is sent. The
    /// token is used only for this push and the reads it performs and is never
    /// written to the repository configuration.
    pub fn push_branch_authenticated(
        &self,
        push: BranchPush<'_>,
        token: &str,
        deadline: Duration,
    ) -> Result<PushReport> {
        let remote_branch = push.remote_branch.unwrap_or(push.branch);
        let refname = format!("refs/heads/{remote_branch}");
        if !git2::Reference::is_valid_name(&refname) {
            return Err(LoomError::invalid_request(format!(
                "'{remote_branch}' is not a valid Git branch name"
            )));
        }
        let repository = self.repository()?;
        let revision = repository
            .find_branch(push.branch, BranchType::Local)
            .map_err(|error| {
                if error.code() == git2::ErrorCode::NotFound {
                    LoomError::not_found("local Git branch", push.branch)
                } else {
                    git_error("could not look up local Git branch", error)
                }
            })?
            .into_reference()
            .target()
            .ok_or_else(|| {
                LoomError::invalid_state(format!(
                    "local Git branch '{}' does not point to a commit",
                    push.branch
                ))
            })?;
        // The remote's own advertisement of the target ref decides both the lease
        // and the previous revision the report names, so nothing here trusts a
        // cached remote-tracking ref.
        let previous = self.remote_ref_revision(&refname, token, deadline)?;
        if let Some(lease) = push.lease
            && previous
                .as_deref()
                .is_none_or(|previous| !previous.eq_ignore_ascii_case(lease))
        {
            return Err(LoomError::conflict(match previous.as_deref() {
                Some(previous) => format!(
                    "the remote branch '{refname}' is at {previous}, not at the lease {lease}; the push was refused and the remote ref was left unchanged"
                ),
                None => format!(
                    "the remote branch '{refname}' does not exist, so the lease {lease} cannot match; the push was refused and the remote ref was left unchanged"
                ),
            }));
        }
        let (commits, fast_forward) = match previous.as_deref() {
            Some(previous) => {
                let commits = self.landed_commits(
                    &repository,
                    previous,
                    revision,
                    &refname,
                    token,
                    deadline,
                )?;
                let previous_oid = remote_revision_oid(previous, &refname)?;
                // The range above is materialised, so the previous revision is
                // readable here. A rewritten history is a legitimate lease push,
                // which is exactly the report that says the update is not a
                // fast-forward.
                let fast_forward = revision == previous_oid
                    || repository
                        .graph_descendant_of(revision, previous_oid)
                        .map_err(|error| {
                            git_error(
                                &format!(
                                    "could not verify that the pushed revision {revision} descends from the previous remote revision {previous} of '{refname}'"
                                ),
                                error,
                            )
                        })?;
                (Some(commits), Some(fast_forward))
            }
            None => (None, None),
        };
        if !push.dry_run {
            // Only a verified lease rewrites the ref, so only then is the refspec
            // forced; without one the remote rejects an update that is not a
            // fast-forward. The refspec source is the revision resolved above, so
            // the ref that is updated is the revision the report names.
            let forced = if push.lease.is_some() { "+" } else { "" };
            self.push_refspec(
                &repository,
                &format!("{forced}{revision}:{refname}"),
                "could not push Git branch",
                token,
            )?;
        }
        Ok(PushReport {
            refname,
            previous_revision: previous,
            revision: Some(revision.to_string()),
            commits,
            fast_forward,
            deleted: false,
            dry_run: push.dry_run,
        })
    }

    /// Pushes the local tag `tag` to `refs/tags/<tag>` on the `origin` remote.
    ///
    /// The tag must already exist locally. The pushed value is the tag's own
    /// target object, so an annotated tag is pushed as the tag object and not as
    /// the commit it peels to. The previous remote revision is read on the same
    /// bounded worker as a branch push, and the commit range is counted from the
    /// peeled previous tag to the peeled local tag when both peel. No lease
    /// applies to a tag, so `fast_forward` is `None`. With `dry_run` the same
    /// reads are performed and no ref update is sent.
    pub fn push_tag_authenticated(
        &self,
        tag: &str,
        dry_run: bool,
        token: &str,
        deadline: Duration,
    ) -> Result<PushReport> {
        let refname = format!("refs/tags/{tag}");
        if !git2::Reference::is_valid_name(&refname) {
            return Err(LoomError::invalid_request(format!(
                "'{tag}' is not a valid Git tag name"
            )));
        }
        let repository = self.repository()?;
        let revision = repository
            .find_reference(&refname)
            .map_err(|error| {
                if error.code() == git2::ErrorCode::NotFound {
                    LoomError::not_found("local Git tag", tag)
                } else {
                    git_error("could not look up local Git tag", error)
                }
            })?
            .target()
            .ok_or_else(|| {
                LoomError::invalid_state(format!(
                    "local Git tag '{tag}' does not point to an object"
                ))
            })?;
        let previous = self.remote_ref_revision(&refname, token, deadline)?;
        let commits = match previous.as_deref() {
            Some(previous) => {
                self.materialize_remote_revision(&repository, previous, &refname, token, deadline)?;
                peeled_commit_commits(&repository, previous, revision)?
            }
            None => None,
        };
        if !dry_run {
            self.push_refspec(
                &repository,
                &format!("{revision}:{refname}"),
                "could not push Git tag",
                token,
            )?;
        }
        Ok(PushReport {
            refname,
            previous_revision: previous,
            revision: Some(revision.to_string()),
            commits,
            fast_forward: None,
            deleted: false,
            dry_run,
        })
    }

    /// Deletes the full ref name `refname` from the `origin` remote.
    ///
    /// `refname` must be a valid full ref name. The ref the remote advertises is
    /// read first; a ref the remote does not advertise is reported as
    /// [`ErrorCode::NotFound`] naming the ref, because there is nothing to
    /// delete. The report names the previous revision and holds no new revision.
    /// With `dry_run` the read is performed and the deletion is not.
    pub fn delete_remote_ref_authenticated(
        &self,
        refname: &str,
        dry_run: bool,
        token: &str,
        deadline: Duration,
    ) -> Result<PushReport> {
        if !git2::Reference::is_valid_name(refname) {
            return Err(LoomError::invalid_request(format!(
                "'{refname}' is not a valid full Git ref name to delete"
            )));
        }
        let previous = self
            .remote_ref_revision(refname, token, deadline)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::NotFound,
                    format!(
                        "the 'origin' remote advertises no ref '{refname}', so there is nothing to delete"
                    ),
                    false,
                )
            })?;
        if !dry_run {
            let repository = self.repository()?;
            self.push_refspec(
                &repository,
                &format!(":{refname}"),
                "could not delete the remote Git ref",
                token,
            )?;
        }
        Ok(PushReport {
            refname: refname.to_owned(),
            previous_revision: Some(previous),
            revision: None,
            commits: None,
            fast_forward: None,
            deleted: true,
            dry_run,
        })
    }

    /// Sends one refspec to the `origin` remote with the push credentials.
    fn push_refspec(
        &self,
        repository: &Repository,
        refspec: &str,
        operation: &str,
        token: &str,
    ) -> Result<()> {
        let mut remote = find_origin_remote(repository)?;
        let token = token.to_owned();
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |url, username_from_url, allowed_types| {
            credential_for(url, username_from_url, allowed_types, &token)
        });
        callbacks.push_update_reference(|reference, status| match status {
            Some(status) => Err(git2::Error::from_str(&format!(
                "remote rejected updating {reference}: {status}"
            ))),
            None => Ok(()),
        });
        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);
        push_options.proxy_options(remote_proxy_options());
        remote
            .push(&[refspec], Some(&mut push_options))
            .map_err(|error| git_error(operation, error))?;
        Ok(())
    }

    /// Revision the `origin` remote advertises for `refname`, or `None` when the
    /// remote advertises no such ref.
    ///
    /// The read runs on a bounded worker with the credentials of a push and looks
    /// at the refs the remote itself advertised, so a cached
    /// `refs/remotes/origin/*` ref is never used as a stand-in for it.
    fn remote_ref_revision(
        &self,
        refname: &str,
        token: &str,
        deadline: Duration,
    ) -> Result<Option<String>> {
        let root = self.root.clone();
        let refname = refname.to_owned();
        let token = token.to_owned();
        let operation =
            format!("reading the revision the 'origin' remote advertises for '{refname}'");
        bounded_remote_operation(
            "loom-vcs-remote-ref-read",
            &operation,
            deadline,
            move || read_remote_ref(&root, &refname, &token),
        )
    }

    /// Makes a revision the remote advertises readable in this checkout.
    ///
    /// A previous revision the remote advertises but the checkout cannot read is
    /// the case a report matters most in: the remote moved on since this checkout
    /// was cloned, so the range is measured against what the remote really held,
    /// materialised with one bounded fetch into
    /// `refs/remotes/origin/<short name>`.
    fn materialize_remote_revision(
        &self,
        repository: &Repository,
        previous: &str,
        refname: &str,
        token: &str,
        deadline: Duration,
    ) -> Result<()> {
        let previous_oid = remote_revision_oid(previous, refname)?;
        let present = repository
            .odb()
            .map_err(|error| git_error("could not read the repository object database", error))?
            .exists(previous_oid);
        if present {
            return Ok(());
        }
        let root = self.root.clone();
        let refname = refname.to_owned();
        let fetched = previous.to_owned();
        let token = token.to_owned();
        let operation = format!(
            "fetching the revision '{previous}' the 'origin' remote advertises for '{refname}'"
        );
        bounded_remote_operation(
            "loom-vcs-remote-ref-fetch",
            &operation,
            deadline,
            move || fetch_remote_ref(&root, &refname, &fetched, &token),
        )
    }

    /// Commits the pushed branch revision carries that the previous remote
    /// revision did not, which is the range `previous..revision` a push landed.
    fn landed_commits(
        &self,
        repository: &Repository,
        previous: &str,
        revision: Oid,
        refname: &str,
        token: &str,
        deadline: Duration,
    ) -> Result<usize> {
        self.materialize_remote_revision(repository, previous, refname, token, deadline)?;
        let previous_oid = remote_revision_oid(previous, refname)?;
        let previous_commit = repository.find_commit(previous_oid).map_err(|error| {
            git_error(
                &format!(
                    "could not read the previous remote revision {previous} of '{refname}' after fetching it"
                ),
                error,
            )
        })?;
        count_landed_commits(repository, previous_commit.id(), revision)
    }

    /// Lists the refs the `origin` remote itself advertises and, when a local
    /// branch is named, how far it is ahead of and behind the remote head of the
    /// same name.
    ///
    /// The whole read runs on one bounded worker with the credentials of a push,
    /// so a stalled remote delays the caller by at most `deadline`. The heads and
    /// tags come from the remote's advertisement, never from cached
    /// `refs/remotes/origin/*` refs. When `branch` is given and both a local
    /// branch and a remote head of that name exist, that one head is fetched into
    /// `refs/remotes/origin/<branch>` and the tracking report is measured against
    /// it; `ahead` counts the commits the local branch has that the remote head
    /// does not.
    pub fn remote_refs_authenticated(
        &self,
        branch: Option<&str>,
        token: &str,
        deadline: Duration,
    ) -> Result<RemoteRefsReport> {
        let root = self.root.clone();
        let branch = branch.map(str::to_owned);
        let token = token.to_owned();
        bounded_remote_operation(
            "loom-vcs-remote-refs-read",
            "listing the refs the 'origin' remote advertises",
            deadline,
            move || read_remote_refs(&root, branch.as_deref(), &token),
        )
    }

    /// Fast-forwards the currently checked-out local branch to `target_commit`.
    ///
    /// The parent checkout must be clean, attached to a local branch, and at
    /// `expected_head`. The target must descend from that expected revision.
    /// Checkout uses Git's safe strategy; if checkout or the ref update fails,
    /// the resulting checkout state is preserved for inspection and recovery.
    pub fn advance_clean_head(&self, expected_head: Oid, target_commit: Oid) -> Result<Oid> {
        let repository = self.repository()?;
        if repository.state() != git2::RepositoryState::Clean {
            return Err(LoomError::invalid_state(
                "cannot fast-forward while another Git operation is in progress",
            ));
        }

        let head = repository
            .head()
            .map_err(|error| git_error("could not read parent Git HEAD", error))?;
        if !head.is_branch() {
            return Err(LoomError::invalid_state(
                "cannot fast-forward a detached or non-local Git HEAD",
            ));
        }
        let branch_refname = head
            .name()
            .map_err(|error| git_error("could not read the parent Git branch name", error))?;
        let head_target = head.target().ok_or_else(|| {
            LoomError::invalid_state("the checked-out Git branch does not point to a commit")
        })?;
        if head_target != expected_head {
            return Err(LoomError::conflict(format!(
                "parent Git HEAD changed: expected {expected_head}, found {head_target}"
            )));
        }

        let blocking = blocking_parent_checkout_paths(&repository)?;
        if !blocking.is_empty() {
            return Err(LoomError::conflict(format!(
                "cannot fast-forward a parent Git checkout with staged, modified, untracked, or unreadable files: {}",
                blocking.join(", ")
            )));
        }

        let target = repository
            .find_commit(target_commit)
            .map_err(|error| git_error("could not resolve fast-forward target commit", error))?;
        if target_commit != expected_head
            && !repository
                .graph_descendant_of(target_commit, expected_head)
                .map_err(|error| git_error("could not verify fast-forward ancestry", error))?
        {
            return Err(LoomError::conflict(format!(
                "target commit {target_commit} is not a descendant of expected parent HEAD {expected_head}"
            )));
        }
        if target_commit == expected_head {
            return Ok(expected_head);
        }

        let mut transaction = repository
            .transaction()
            .map_err(|error| git_error("could not start parent branch update", error))?;
        transaction
            .lock_ref(branch_refname)
            .map_err(|error| git_error("could not lock parent branch for fast-forward", error))?;
        let locked_target = repository
            .find_reference(branch_refname)
            .map_err(|error| git_error("could not re-read locked parent branch", error))?
            .target()
            .ok_or_else(|| LoomError::invalid_state("locked parent branch has no target commit"))?;
        if locked_target != expected_head {
            return Err(LoomError::conflict(format!(
                "parent Git branch changed before fast-forward: expected {expected_head}, found {locked_target}"
            )));
        }

        let mut checkout = CheckoutBuilder::new();
        checkout.safe().update_index(true);
        if let Err(error) = repository.checkout_tree(target.as_object(), Some(&mut checkout)) {
            return Err(git_error(
                &format!(
                    "safe checkout of fast-forward target {target_commit} failed; parent branch remains at {expected_head} and checkout state was preserved"
                ),
                error,
            ));
        }

        let checked_out_branch_target = repository
            .find_reference(branch_refname)
            .map_err(|error| {
                git_error(
                    "could not verify parent branch after checkout; checkout state was preserved",
                    error,
                )
            })?
            .target()
            .ok_or_else(|| {
                LoomError::invalid_state(
                    "parent branch lost its target after checkout; checkout state was preserved",
                )
            })?;
        if checked_out_branch_target != expected_head {
            return Err(LoomError::conflict(format!(
                "parent branch changed during fast-forward checkout: expected {expected_head}, found {checked_out_branch_target}; checkout state was preserved"
            )));
        }

        transaction
            .set_target(
                branch_refname,
                target_commit,
                None,
                "loom: fast-forward parent worktree",
            )
            .map_err(|error| {
                git_error(
                    &format!(
                        "could not update parent branch to {target_commit}; checkout state was preserved for recovery"
                    ),
                    error,
                )
            })?;
        transaction.commit().map_err(|error| {
            git_error(
                &format!(
                    "could not commit parent branch update to {target_commit}; checkout state was preserved for recovery"
                ),
                error,
            )
        })?;

        Ok(target_commit)
    }

    /// String-based wrapper for callers that persist Git revisions as text.
    pub fn advance_clean_head_revisions(
        &self,
        expected_head: &str,
        target_commit: &str,
    ) -> Result<String> {
        let expected_head_oid = Oid::from_str(expected_head).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid expected parent Git revision '{expected_head}': {error}"
            ))
        })?;
        let target_commit_oid = Oid::from_str(target_commit).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid target Git revision '{target_commit}': {error}"
            ))
        })?;
        self.advance_clean_head(expected_head_oid, target_commit_oid)
            .map(|revision| revision.to_string())
    }

    /// Reports the Git operation state of this checkout.
    ///
    /// A non-clean state means an operation such as a merge or rebase is
    /// already in progress and must be recovered explicitly before Loom may
    /// touch the checkout.
    pub fn operation_state(&self) -> Result<RepositoryState> {
        Ok(self.repository()?.state())
    }

    /// Returns true when the checkout has an in-progress Git operation.
    pub fn operation_in_progress(&self) -> Result<bool> {
        Ok(self.operation_state()? != RepositoryState::Clean)
    }

    /// Returns true when `ancestor` is `descendant` or an ancestor of it.
    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        self.repository()?
            .graph_descendant_of(descendant, ancestor)
            .map_err(|error| git_error("could not verify Git ancestry", error))
    }

    /// String-based wrapper for [`Self::is_ancestor`].
    pub fn is_ancestor_revision(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let ancestor = Oid::from_str(ancestor).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid Git ancestor revision '{ancestor}': {error}"
            ))
        })?;
        let descendant = Oid::from_str(descendant).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid Git descendant revision '{descendant}': {error}"
            ))
        })?;
        self.is_ancestor(ancestor, descendant)
    }

    /// Integrates a reviewed child commit into the clean checked-out branch at
    /// `expected_parent`.
    ///
    /// A fast-forward is used when the child descends from the parent. When the
    /// parent has advanced independently, the two histories are merged in
    /// memory: a clean result is committed as a two-parent merge commit and
    /// checked out, while a conflict leaves the branch, index, and working tree
    /// completely untouched and returns the conflicting paths.
    pub fn integrate_merge(
        &self,
        expected_parent: Oid,
        child_commit: Oid,
        message: &str,
    ) -> Result<MergeIntegrationOutcome> {
        let repository = self.repository()?;
        if repository.state() != RepositoryState::Clean {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "cannot integrate while another Git operation is in progress",
                true,
            ));
        }

        let head = repository
            .head()
            .map_err(|error| git_error("could not read parent Git HEAD", error))?;
        if !head.is_branch() {
            return Err(LoomError::invalid_state(
                "cannot integrate into a detached or non-local Git HEAD",
            ));
        }
        let branch_refname = head
            .name()
            .map_err(|error| git_error("could not read the parent Git branch name", error))?
            .to_owned();
        let head_target = head.target().ok_or_else(|| {
            LoomError::invalid_state("the checked-out Git branch does not point to a commit")
        })?;
        if head_target != expected_parent {
            return Err(LoomError::conflict(format!(
                "parent Git HEAD changed: expected {expected_parent}, found {head_target}"
            )));
        }

        let blocking = blocking_parent_checkout_paths(&repository)?;
        if !blocking.is_empty() {
            return Err(LoomError::conflict(format!(
                "cannot integrate into a parent Git checkout with staged, modified, untracked, or unreadable files: {}",
                blocking.join(", ")
            )));
        }

        if child_commit == expected_parent {
            return Ok(MergeIntegrationOutcome::AlreadyPresent);
        }
        let child = repository
            .find_commit(child_commit)
            .map_err(|error| git_error("could not resolve child Git commit", error))?;
        if repository
            .graph_descendant_of(expected_parent, child_commit)
            .map_err(|error| git_error("could not verify child Git ancestry", error))?
        {
            return Ok(MergeIntegrationOutcome::AlreadyPresent);
        }
        if repository
            .graph_descendant_of(child_commit, expected_parent)
            .map_err(|error| git_error("could not verify child Git ancestry", error))?
        {
            let revision = self.advance_clean_head(expected_parent, child_commit)?;
            return Ok(MergeIntegrationOutcome::FastForward(revision));
        }

        let parent = repository
            .find_commit(expected_parent)
            .map_err(|error| git_error("could not resolve parent Git commit", error))?;
        let mut index = repository
            .merge_commits(&parent, &child, Some(&MergeOptions::new()))
            .map_err(|error| git_error("could not compute project child merge", error))?;
        if index.has_conflicts() {
            return Ok(MergeIntegrationOutcome::Conflicted(index_conflict_paths(
                &index,
            )?));
        }

        let tree_oid = index
            .write_tree_to(&repository)
            .map_err(|error| git_error("could not write project child merge tree", error))?;
        let tree = repository
            .find_tree(tree_oid)
            .map_err(|error| git_error("could not read project child merge tree", error))?;
        let signature = repository
            .signature()
            .or_else(|_| Signature::now("loom", "loom@localhost"))
            .map_err(|error| git_error("could not resolve a Git signature for the merge", error))?;
        let merge_commit = repository
            .commit(
                None,
                &signature,
                &signature,
                message,
                &tree,
                &[&parent, &child],
            )
            .map_err(|error| git_error("could not create the project child merge commit", error))?;
        let merge_commit = repository
            .find_commit(merge_commit)
            .map_err(|error| git_error("could not read the project child merge commit", error))?;

        let mut transaction = repository
            .transaction()
            .map_err(|error| git_error("could not start parent branch update", error))?;
        transaction
            .lock_ref(&branch_refname)
            .map_err(|error| git_error("could not lock parent branch for integration", error))?;
        let locked_target = repository
            .find_reference(&branch_refname)
            .map_err(|error| git_error("could not re-read locked parent branch", error))?
            .target()
            .ok_or_else(|| LoomError::invalid_state("locked parent branch has no target commit"))?;
        if locked_target != expected_parent {
            return Err(LoomError::conflict(format!(
                "parent Git branch changed before integration: expected {expected_parent}, found {locked_target}"
            )));
        }

        let mut checkout = CheckoutBuilder::new();
        checkout.safe().update_index(true);
        if let Err(error) = repository.checkout_tree(merge_commit.as_object(), Some(&mut checkout))
        {
            return Err(git_error(
                &format!(
                    "safe checkout of merge result {} failed; parent branch remains at {expected_parent} and checkout state was preserved",
                    merge_commit.id()
                ),
                error,
            ));
        }

        transaction
            .set_target(
                &branch_refname,
                merge_commit.id(),
                None,
                "loom: merge reviewed project child",
            )
            .map_err(|error| {
                git_error(
                    &format!(
                        "could not update parent branch to {}; checkout state was preserved for recovery",
                        merge_commit.id()
                    ),
                    error,
                )
            })?;
        transaction.commit().map_err(|error| {
            git_error(
                &format!(
                    "could not commit parent branch update to {}; checkout state was preserved for recovery",
                    merge_commit.id()
                ),
                error,
            )
        })?;

        Ok(MergeIntegrationOutcome::Merged(merge_commit.id()))
    }

    /// String-based wrapper for [`Self::integrate_merge`].
    pub fn integrate_merge_revisions(
        &self,
        expected_parent: &str,
        child_commit: &str,
        message: &str,
    ) -> Result<MergeIntegrationOutcome> {
        let expected_parent_oid = Oid::from_str(expected_parent).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid expected parent Git revision '{expected_parent}': {error}"
            ))
        })?;
        let child_commit_oid = Oid::from_str(child_commit).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid child Git revision '{child_commit}': {error}"
            ))
        })?;
        self.integrate_merge(expected_parent_oid, child_commit_oid, message)
    }

    /// Message of a committed revision, including its subject and body.
    ///
    /// A commit with no message, or one that is not valid UTF-8, is reported as
    /// an empty message so a caller that reuses the message can fall back to its
    /// own wording instead of failing.
    pub fn commit_message(&self, revision: &str) -> Result<String> {
        let repository = self.repository()?;
        let revision_oid = Oid::from_str(revision).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid Git revision '{revision}' whose message was requested: {error}"
            ))
        })?;
        let commit = repository.find_commit(revision_oid).map_err(|error| {
            git_error(
                "could not resolve the commit whose message was requested",
                error,
            )
        })?;
        Ok(commit.message().unwrap_or_default().to_owned())
    }

    /// Replays the commits this checkout has and `onto` does not, then moves the
    /// checked-out branch to the replayed tip.
    ///
    /// The replayed commits keep their own messages and authors; only the
    /// committer is this checkout's Git identity. Their revisions change, so the
    /// checkout ends on its own branch at a revision that descends from `onto`.
    ///
    /// Nothing is written until every commit has been replayed, so a replay that
    /// does not apply cleanly reports the conflicting paths and leaves the
    /// branch, index, and working tree exactly as they were, with no rebase in
    /// progress. A checkout whose own commits contain a merge cannot be replayed
    /// without dropping that merge, so the caller is told to merge instead.
    pub fn rebase_onto_revision(&self, onto: &str) -> Result<RebaseIntegrationOutcome> {
        let repository = self.repository()?;
        if repository.state() != RepositoryState::Clean {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "cannot replay committed changes while another Git operation is in progress",
                true,
            ));
        }
        let head = repository
            .head()
            .map_err(|error| git_error("could not read Git HEAD", error))?;
        if !head.is_branch() {
            return Err(LoomError::invalid_state(
                "cannot replay the committed changes of a detached or non-local Git HEAD",
            ));
        }
        let branch_refname = head
            .name()
            .map_err(|error| git_error("could not read the Git branch name", error))?
            .to_owned();
        let head_target = head.target().ok_or_else(|| {
            LoomError::invalid_state("the checked-out Git branch does not point to a commit")
        })?;
        let onto_revision = Oid::from_str(onto).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid Git replay target revision '{onto}': {error}"
            ))
        })?;
        let onto_commit = repository
            .find_commit(onto_revision)
            .map_err(|error| git_error("could not resolve the replay target commit", error))?;

        let merge_base = match repository.merge_base(head_target, onto_revision) {
            Ok(merge_base) => merge_base,
            // Unrelated histories share nothing to replay, so the caller has to
            // integrate them with a merge.
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                return Ok(RebaseIntegrationOutcome::MergeRequired);
            }
            Err(error) => {
                return Err(git_error(
                    "could not find the merge base of the checkout and its replay target",
                    error,
                ));
            }
        };
        if merge_base == head_target {
            // The target already contains the checked-out revision.
            return Ok(RebaseIntegrationOutcome::Rebased(head_target));
        }

        let mut commits = Vec::new();
        let mut walk = repository
            .revwalk()
            .map_err(|error| git_error("could not walk the commits to replay", error))?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .map_err(|error| git_error("could not order the commits to replay", error))?;
        walk.push(head_target)
            .map_err(|error| git_error("could not walk the checked-out revision", error))?;
        walk.hide(merge_base).map_err(|error| {
            git_error("could not exclude the already-integrated history", error)
        })?;
        for id in walk {
            let id =
                id.map_err(|error| git_error("could not walk the commits to replay", error))?;
            let commit = repository
                .find_commit(id)
                .map_err(|error| git_error("could not read a commit to replay", error))?;
            if commit.parent_count() > 1 {
                return Ok(RebaseIntegrationOutcome::MergeRequired);
            }
            commits.push(commit);
        }

        let committer = repository
            .signature()
            .or_else(|_| Signature::now("loom", "loom@localhost"))
            .map_err(|error| {
                git_error("could not resolve a Git signature for the replay", error)
            })?;

        let mut replayed = onto_commit;
        for commit in &commits {
            let our_tree = replayed
                .tree()
                .map_err(|error| git_error("could not read the replay target tree", error))?;
            let their_tree = commit
                .tree()
                .map_err(|error| git_error("could not read a replayed commit tree", error))?;
            // The change a commit introduces is its own tree against its parent
            // tree, which is what applying it to the replayed tip has to use.
            let mut index = match commit.parent(0) {
                Ok(parent) => parent.tree().map_err(|error| {
                    git_error("could not read a replayed commit's parent tree", error)
                }),
                // A commit without a parent introduces its whole tree.
                Err(_) => empty_tree(&repository),
            }
            .and_then(|ancestor| {
                repository
                    .merge_trees(
                        &ancestor,
                        &our_tree,
                        &their_tree,
                        Some(&MergeOptions::new()),
                    )
                    .map_err(|error| git_error("could not replay a committed change", error))
            })?;
            if index.has_conflicts() {
                return Ok(RebaseIntegrationOutcome::Conflicted(index_conflict_paths(
                    &index,
                )?));
            }
            let tree_oid = index
                .write_tree_to(&repository)
                .map_err(|error| git_error("could not write a replayed tree", error))?;
            if tree_oid == our_tree.id() {
                // The change is already in the target; the replay drops it the
                // way `git rebase` drops a commit that becomes empty.
                continue;
            }
            let tree = repository
                .find_tree(tree_oid)
                .map_err(|error| git_error("could not read a replayed tree", error))?;
            let author = commit.author();
            let message = commit.message().unwrap_or_default();
            let replayed_oid = repository
                .commit(None, &author, &committer, message, &tree, &[&replayed])
                .map_err(|error| git_error("could not create a replayed commit", error))?;
            replayed = repository
                .find_commit(replayed_oid)
                .map_err(|error| git_error("could not read a replayed commit", error))?;
        }

        let tip = replayed.id();
        if tip == head_target {
            // Every replayed change was already in the target.
            return Ok(RebaseIntegrationOutcome::Rebased(tip));
        }

        let mut transaction = repository
            .transaction()
            .map_err(|error| git_error("could not start the replay branch update", error))?;
        transaction
            .lock_ref(&branch_refname)
            .map_err(|error| git_error("could not lock the branch for the replay", error))?;
        let locked_target = repository
            .find_reference(&branch_refname)
            .map_err(|error| git_error("could not re-read the locked branch", error))?
            .target()
            .ok_or_else(|| LoomError::invalid_state("the locked branch has no target commit"))?;
        if locked_target != head_target {
            return Err(LoomError::conflict(format!(
                "Git branch changed before the replay: expected {head_target}, found {locked_target}"
            )));
        }

        let mut checkout = CheckoutBuilder::new();
        checkout.safe().update_index(true);
        repository
            .checkout_tree(replayed.as_object(), Some(&mut checkout))
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "safe checkout of the replayed revision {tip} failed; the branch remains at {head_target} and checkout state was preserved: {error}"
                    ),
                    true,
                )
            })?;
        transaction
            .set_target(
                &branch_refname,
                tip,
                None,
                "loom: replay committed changes onto their integration target",
            )
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "could not update the branch to the replayed revision {tip}; checkout state was preserved for recovery: {error}"
                    ),
                    true,
                )
            })?;
        transaction.commit().map_err(|error| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!(
                    "could not commit the branch update to the replayed revision {tip}; checkout state was preserved for recovery: {error}"
                ),
                true,
            )
        })?;

        Ok(RebaseIntegrationOutcome::Rebased(tip))
    }

    /// Creates a linked worktree at `path`, based on `base_commit`, and checks
    /// out a newly created local branch in it.
    ///
    /// Worktree names and local branch names must be unused. The target path's
    /// parent directory must already exist, and the target must not overlap
    /// this repository's working tree.
    pub fn create_linked_worktree(
        &self,
        worktree_name: &str,
        branch_name: &str,
        path: impl AsRef<Path>,
        base_commit: Oid,
    ) -> Result<Self> {
        validate_worktree_name(worktree_name)?;
        let path = self.validate_new_worktree_path(path.as_ref())?;
        let repository = self.repository()?;

        let worktrees = repository
            .worktrees()
            .map_err(|error| git_error("could not list Git worktrees", error))?;
        let worktree_name_exists = worktrees.iter().try_fold(false, |found, name| {
            let name =
                name.map_err(|error| git_error("could not read Git worktree name", error))?;
            Ok::<_, LoomError>(found || name == Some(worktree_name))
        })?;
        if worktree_name_exists {
            return Err(LoomError::conflict(format!(
                "Git worktree '{worktree_name}' already exists"
            )));
        }
        if repository
            .find_reference(&format!("refs/heads/{branch_name}"))
            .is_ok()
        {
            return Err(LoomError::conflict(format!(
                "local Git branch '{branch_name}' already exists"
            )));
        }

        let commit = repository
            .find_commit(base_commit)
            .map_err(|error| git_error("could not resolve Git worktree base commit", error))?;

        let branch = match repository.branch(branch_name, &commit, false) {
            Ok(branch) => branch,
            Err(error) => return Err(git_error("could not create Git worktree branch", error)),
        };
        let branch_reference = branch.into_reference();
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(&branch_reference));
        let worktree = match repository.worktree(worktree_name, &path, Some(&options)) {
            Ok(worktree) => worktree,
            Err(error) => {
                let remove_unregistered_path = error.code() != git2::ErrorCode::Exists;
                let original = git_error("could not create linked Git worktree", error);
                drop(branch_reference);
                let cleanup = cleanup_new_linked_worktree(
                    &repository,
                    worktree_name,
                    branch_name,
                    &path,
                    remove_unregistered_path,
                );
                return Err(with_worktree_setup_cleanup(original, cleanup));
            }
        };
        drop(worktree);
        drop(branch_reference);

        match Self::open(&path) {
            Ok(worktree) => Ok(worktree),
            Err(error) => {
                let cleanup = cleanup_new_linked_worktree(
                    &repository,
                    worktree_name,
                    branch_name,
                    &path,
                    true,
                );
                Err(with_worktree_setup_cleanup(error, cleanup))
            }
        }
    }

    /// Creates a linked worktree from a persisted commit OID string.
    pub fn create_linked_worktree_at_revision(
        &self,
        worktree_name: &str,
        branch_name: &str,
        path: impl AsRef<Path>,
        base_revision: &str,
    ) -> Result<Self> {
        let base_commit = Oid::from_str(base_revision).map_err(|error| {
            LoomError::invalid_request(format!(
                "invalid Git worktree base revision '{base_revision}': {error}"
            ))
        })?;
        self.create_linked_worktree(worktree_name, branch_name, path, base_commit)
    }

    /// Opens a linked worktree only when its registered name and path match.
    pub fn open_linked_worktree(
        &self,
        worktree_name: &str,
        path: impl AsRef<Path>,
    ) -> Result<Self> {
        validate_worktree_name(worktree_name)?;
        let repository = self.repository()?;
        let worktree = repository.find_worktree(worktree_name).map_err(|error| {
            if error.code() == git2::ErrorCode::NotFound {
                LoomError::not_found("Git worktree", worktree_name)
            } else {
                git_error("could not find registered Git worktree", error)
            }
        })?;
        let registered_path = worktree.path();
        for candidate in [path.as_ref(), registered_path] {
            match fs::symlink_metadata(candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!(
                            "refusing to open linked Git worktree '{worktree_name}' through symlink path '{}'",
                            candidate.display()
                        ),
                        false,
                    ));
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!(
                            "could not inspect linked Git worktree path '{}': {error}",
                            candidate.display()
                        ),
                        false,
                    ));
                }
            }
        }
        worktree
            .validate()
            .map_err(|error| git_error("registered Git worktree is invalid", error))?;

        let requested_path = fs::canonicalize(path.as_ref()).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve requested Git worktree path: {error}"),
                false,
            )
        })?;
        let registered_path = fs::canonicalize(registered_path).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve registered Git worktree path: {error}"),
                false,
            )
        })?;
        if requested_path != registered_path {
            return Err(LoomError::conflict(format!(
                "path '{}' does not match the registered path for Git worktree '{worktree_name}'",
                path.as_ref().display()
            )));
        }
        Self::open(registered_path)
    }

    /// Reports whether a named linked worktree is locked.
    ///
    /// A worktree that is no longer registered is reported as unlocked so a
    /// caller can treat a missing registration as an already removed checkout.
    pub fn linked_worktree_is_locked(&self, worktree_name: &str) -> Result<bool> {
        validate_worktree_name(worktree_name)?;
        let repository = self.repository()?;
        let worktree = match repository.find_worktree(worktree_name) {
            Ok(worktree) => worktree,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(false),
            Err(error) => {
                return Err(git_error("could not find registered Git worktree", error));
            }
        };
        match worktree.is_locked() {
            Ok(WorktreeLockStatus::Locked(_)) => Ok(true),
            Ok(WorktreeLockStatus::Unlocked) => Ok(false),
            Err(error) => Err(git_error(
                "could not inspect Git worktree lock state",
                error,
            )),
        }
    }

    /// Removes a named linked worktree and prunes its Git metadata.
    ///
    /// A worktree with tracked, untracked, ignored, or staged changes is kept
    /// unless `force` is true. Its local branch is deliberately retained.
    pub fn remove_linked_worktree(&self, worktree_name: &str, force: bool) -> Result<()> {
        validate_worktree_name(worktree_name)?;
        let repository = self.repository()?;
        let worktree = repository.find_worktree(worktree_name).map_err(|error| {
            if error.code() == git2::ErrorCode::NotFound {
                LoomError::not_found("Git worktree", worktree_name)
            } else {
                git_error("could not open linked Git worktree", error)
            }
        })?;
        let path = worktree.path().to_path_buf();

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!(
                        "refusing to remove linked Git worktree '{worktree_name}' because its path is a symlink"
                    ),
                    false,
                ));
            }
            Ok(_) => {
                worktree.validate().map_err(|error| {
                    git_error(
                        "could not validate linked Git worktree before removal",
                        error,
                    )
                })?;
                let checkout = Repository::open(&path)
                    .map_err(|error| git_error("could not inspect linked Git worktree", error))?;
                let mut options = StatusOptions::new();
                options
                    .include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .include_ignored(true)
                    .recurse_ignored_dirs(true);
                let statuses = checkout.statuses(Some(&mut options)).map_err(|error| {
                    git_error("could not inspect linked Git worktree changes", error)
                })?;
                if !statuses.is_empty() && !force {
                    return Err(LoomError::conflict(format!(
                        "Git worktree '{worktree_name}' has changes; pass force only when its contents may be discarded"
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not inspect linked Git worktree path: {error}"),
                    false,
                ));
            }
        }

        if matches!(worktree.is_locked(), Ok(WorktreeLockStatus::Locked(_))) && !force {
            return Err(LoomError::conflict(format!(
                "Git worktree '{worktree_name}' is locked; pass force only when its lock may be removed"
            )));
        }

        let mut options = WorktreePruneOptions::new();
        options.valid(true).locked(force).working_tree(true);
        worktree
            .prune(Some(&mut options))
            .map_err(|error| git_error("could not remove linked Git worktree", error))
    }

    pub fn conflicts(&self) -> Result<Vec<String>> {
        Ok(self.status()?.conflicts)
    }

    fn validate_path(&self, path: &str) -> Result<String> {
        let candidate = Path::new(path);
        if path.trim().is_empty()
            || candidate.is_absolute()
            || candidate
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("Git path '{path}' must stay inside the repository root"),
                false,
            ));
        }
        Ok(candidate.to_string_lossy().replace('\\', "/"))
    }

    fn validate_new_worktree_path(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(LoomError::invalid_request(
                "Git worktree destination must be an absolute path without parent traversal",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            LoomError::invalid_request("Git worktree destination must have a parent directory")
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            LoomError::invalid_request("Git worktree destination must name a directory")
        })?;
        let parent = fs::canonicalize(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve Git worktree destination parent: {error}"),
                false,
            )
        })?;
        let path = parent.join(file_name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(LoomError::conflict(format!(
                    "Git worktree destination '{}' already exists",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not inspect Git worktree destination: {error}"),
                    false,
                ));
            }
        }
        if path.starts_with(&self.root) || self.root.starts_with(&path) {
            return Err(LoomError::invalid_request(
                "Git worktree destination must not overlap the source working tree",
            ));
        }
        Ok(path)
    }

    fn repository(&self) -> Result<Repository> {
        Repository::open(&self.root)
            .map_err(|error| git_error("could not open Git repository", error))
    }
}

fn render_git_diff(
    diff: git2::Diff<'_>,
    path: Option<String>,
    staged: bool,
    max_bytes: Option<usize>,
) -> Result<GitDiff> {
    let mut patch = Vec::new();
    let mut hunks: Vec<GitDiffHunk> = Vec::new();
    let mut truncated = false;
    // Where the open hunk's header starts in the patch, so a hunk whose body
    // did not fit at all can be removed whole, and the end of the last hunk
    // that arrived complete, so a cut between hunks keeps only whole output.
    let mut hunk_patch_start = 0usize;
    let mut whole_patch_len = 0usize;
    let mut whole_hunks = 0usize;
    let mut old_used = 0u32;
    let mut new_used = 0u32;
    let print_result = diff.print(DiffFormat::Patch, |_delta, hunk, line| {
        let new_hunk = hunk.and_then(|hunk| {
            let is_new = hunks.last().is_none_or(|last| {
                last.old_start != hunk.old_start() || last.new_start != hunk.new_start()
            });
            is_new.then(|| {
                old_used = 0;
                new_used = 0;
                GitDiffHunk {
                    old_start: hunk.old_start(),
                    old_lines: hunk.old_lines(),
                    new_start: hunk.new_start(),
                    new_lines: hunk.new_lines(),
                    lines: Vec::new(),
                }
            })
        });
        let origin = line.origin();
        let contents = line.content();
        let has_prefix = matches!(origin, ' ' | '+' | '-');
        let output_line_len = contents.len() + usize::from(has_prefix);
        if max_bytes.is_some_and(|limit| output_line_len > limit.saturating_sub(patch.len())) {
            truncated = true;
            if has_prefix && new_hunk.is_none() {
                // The cut landed in the open hunk's body: keep the lines that
                // fit under counts that describe them.
                match hunks.last_mut() {
                    Some(hunk) if hunk.lines.is_empty() => {
                        patch.truncate(hunk_patch_start);
                        hunks.pop();
                    }
                    Some(hunk) => {
                        hunk.old_lines = old_used;
                        hunk.new_lines = new_used;
                    }
                    None => {}
                }
            } else {
                // The cut landed between hunks or inside a file header, so drop
                // the partial tail and keep the last whole hunk.
                patch.truncate(whole_patch_len);
                hunks.truncate(whole_hunks);
            }
            return false;
        }
        if let Some(hunk) = new_hunk {
            hunk_patch_start = patch.len();
            hunks.push(hunk);
        }
        if has_prefix {
            patch.push(origin as u8);
            if let Some(hunk) = hunks.last_mut() {
                let kind = match origin {
                    '+' => GitDiffLineKind::Added,
                    '-' => GitDiffLineKind::Removed,
                    _ => GitDiffLineKind::Context,
                };
                hunk.lines.push(GitDiffLine {
                    kind,
                    old_line: line.old_lineno(),
                    new_line: line.new_lineno(),
                    content: String::from_utf8_lossy(contents)
                        .trim_end_matches('\n')
                        .trim_end_matches('\r')
                        .to_owned(),
                });
                match origin {
                    '+' => new_used += 1,
                    '-' => old_used += 1,
                    _ => {
                        old_used += 1;
                        new_used += 1;
                    }
                }
            }
        }
        patch.extend_from_slice(contents);
        if has_prefix
            && hunks
                .last()
                .is_some_and(|hunk| old_used == hunk.old_lines && new_used == hunk.new_lines)
        {
            whole_patch_len = patch.len();
            whole_hunks = hunks.len();
        }
        true
    });
    if let Err(error) = print_result
        && (!truncated || error.code() != git2::ErrorCode::User)
    {
        return Err(git_error("could not render Git diff", error));
    }
    let binary = diff
        .deltas()
        .any(|delta| delta.old_file().is_binary() || delta.new_file().is_binary());
    let patch = String::from_utf8(patch).map_err(|error| {
        LoomError::new(
            ErrorCode::Vcs,
            format!("Git diff contained invalid UTF-8: {error}"),
            false,
        )
    })?;
    Ok(GitDiff {
        path,
        staged,
        patch,
        binary,
        hunks,
        truncated,
    })
}

fn count_diff_lines(repository: &Repository, staged: bool) -> Result<BTreeMap<String, (u32, u32)>> {
    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let diff = if staged {
        let head = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_tree().ok());
        repository.diff_tree_to_index(head.as_ref(), None, Some(&mut options))
    } else {
        repository.diff_index_to_workdir(None, Some(&mut options))
    }
    .map_err(|error| git_error("could not count Git diff lines", error))?;
    let mut counts: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) else {
            return true;
        };
        let entry = counts
            .entry(path.to_string_lossy().replace('\\', "/"))
            .or_default();
        match line.origin() {
            '+' => entry.0 = entry.0.saturating_add(1),
            '-' => entry.1 = entry.1.saturating_add(1),
            _ => {}
        }
        true
    })
    .map_err(|error| git_error("could not count Git diff lines", error))?;
    Ok(counts)
}

/// Collects the paths that block a fast-forward or merge into a parent
/// checkout: staged, modified, untracked, unreadable, or conflicted files.
///
/// Ignored paths are deliberately excluded. A path that neither the parent nor
/// the child revision tracks cannot conflict with a fast-forward or merge, so
/// ignored build output such as `target/` must not block integration. libgit2's
/// safe checkout still refuses to overwrite a file that the target revision
/// tracks, so a genuine collision remains protected. Naming the blocking paths
/// also makes the error actionable instead of listing five file states.
fn blocking_parent_checkout_paths(repository: &Repository) -> Result<Vec<String>> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_unreadable(true)
        .include_unreadable_as_untracked(true);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(|error| git_error("could not inspect parent Git checkout", error))?;
    let mut paths = Vec::new();
    for entry in statuses.iter() {
        let path = entry
            .path()
            .map_err(|error| git_error("Git status contained invalid UTF-8", error))?;
        paths.push(path.to_owned());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Conflicting paths of a merge or replay index, in a stable order.
fn index_conflict_paths(index: &git2::Index) -> Result<Vec<String>> {
    let conflicts = index
        .conflicts()
        .map_err(|error| git_error("could not inspect Git conflict paths", error))?;
    let mut paths = Vec::new();
    for conflict in conflicts {
        let conflict =
            conflict.map_err(|error| git_error("could not read a Git conflict path", error))?;
        if let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) {
            paths.push(String::from_utf8_lossy(&entry.path).replace('\\', "/"));
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The empty tree, which is the parent tree of a commit without a parent.
fn empty_tree<'repo>(repository: &'repo Repository) -> Result<git2::Tree<'repo>> {
    let oid = repository
        .treebuilder(None)
        .and_then(|builder| builder.write())
        .map_err(|error| git_error("could not write the empty Git tree", error))?;
    repository
        .find_tree(oid)
        .map_err(|error| git_error("could not read the empty Git tree", error))
}

fn status_kind(status: Status, index: bool) -> GitFileStatusKind {
    if status.is_conflicted() {
        return GitFileStatusKind::Conflicted;
    }
    if status.is_ignored() {
        return GitFileStatusKind::Ignored;
    }
    if index {
        if status.is_index_renamed() {
            GitFileStatusKind::Renamed
        } else if status.is_index_new() {
            GitFileStatusKind::Added
        } else if status.is_index_modified() {
            GitFileStatusKind::Modified
        } else if status.is_index_deleted() {
            GitFileStatusKind::Deleted
        } else {
            GitFileStatusKind::Unknown
        }
    } else if status.is_wt_renamed() {
        GitFileStatusKind::Renamed
    } else if status.is_wt_new() {
        GitFileStatusKind::Untracked
    } else if status.is_wt_modified() {
        GitFileStatusKind::Modified
    } else if status.is_wt_deleted() {
        GitFileStatusKind::Deleted
    } else {
        GitFileStatusKind::Unknown
    }
}

fn validate_worktree_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(LoomError::invalid_request(
            "Git worktree name must be a non-empty single path component",
        ));
    }
    Ok(())
}

fn remove_new_worktree_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "refusing to remove a partially created Git worktree path that is not a directory",
                false,
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(LoomError::new(
                ErrorCode::Vcs,
                format!("could not inspect failed Git worktree path: {error}"),
                false,
            ));
        }
    }
    .map_err(|error| {
        LoomError::new(
            ErrorCode::Vcs,
            format!("could not remove partially created Git worktree directory: {error}"),
            false,
        )
    })
}

fn cleanup_new_linked_worktree(
    repository: &Repository,
    worktree_name: &str,
    branch_name: &str,
    path: &Path,
    remove_unregistered_path: bool,
) -> Result<()> {
    let mut cleanup_errors = Vec::new();
    let mut worktree_removed = false;
    match repository.find_worktree(worktree_name) {
        Ok(worktree) if worktree.path() == path && worktree_is_on_branch(path, branch_name) => {
            let mut options = WorktreePruneOptions::new();
            options.valid(true).locked(true).working_tree(true);
            match worktree.prune(Some(&mut options)) {
                Ok(()) => worktree_removed = true,
                Err(error) => cleanup_errors.push(format!(
                    "could not prune partially created worktree: {error}"
                )),
            }
        }
        // A same-named worktree may have been created concurrently after the
        // preflight check. Leave it and its path alone; this call owns only its
        // newly created branch.
        Ok(_) => worktree_removed = true,
        Err(error) if error.code() == git2::ErrorCode::NotFound && remove_unregistered_path => {
            match remove_new_worktree_directory(path) {
                Ok(()) => worktree_removed = true,
                Err(error) => cleanup_errors.push(error.message),
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => worktree_removed = true,
        Err(error) => cleanup_errors.push(format!(
            "could not inspect worktree during setup cleanup: {error}"
        )),
    }

    if worktree_removed {
        match repository.find_branch(branch_name, BranchType::Local) {
            Ok(mut branch) => {
                if let Err(error) = branch.delete() {
                    cleanup_errors.push(format!("could not delete new local branch: {error}"));
                }
            }
            Err(error) if error.code() == git2::ErrorCode::NotFound => {}
            Err(error) => {
                cleanup_errors.push(format!("could not look up new local branch: {error}"))
            }
        }
    }

    if cleanup_errors.is_empty() {
        Ok(())
    } else {
        Err(LoomError::new(
            ErrorCode::Vcs,
            cleanup_errors.join("; "),
            false,
        ))
    }
}

fn worktree_is_on_branch(path: &Path, branch_name: &str) -> bool {
    Repository::open(path).ok().is_some_and(|repository| {
        repository
            .head()
            .ok()
            .and_then(|head| head.shorthand().ok().map(|name| name == branch_name))
            .unwrap_or(false)
    })
}

fn with_worktree_setup_cleanup(original: LoomError, cleanup: Result<()>) -> LoomError {
    match cleanup {
        Ok(()) => original,
        Err(cleanup) => LoomError::new(
            original.code,
            format!(
                "{}; setup cleanup failed: {}",
                original.message, cleanup.message
            ),
            false,
        ),
    }
}

/// Connect timeout, in milliseconds, that [`configure_remote_timeouts`] applies
/// to libgit2's socket transports.
///
/// It is chosen to fail well before the operating system's own TCP connect
/// timeout (about 130 seconds on Linux), because a stalled remote otherwise
/// holds the backend thread that issued the Git operation, and the project
/// admission slot that thread serves, for that whole time.
pub const REMOTE_CONNECT_TIMEOUT_MS: i32 = 30_000;

/// Idle timeout, in milliseconds, that [`configure_remote_timeouts`] applies to
/// reads and writes on libgit2's socket transports.
///
/// It bounds a transfer that has stopped making progress, not the total duration
/// of a transfer: a large clone on a slow but working link is not cut off.
pub const REMOTE_TRANSFER_TIMEOUT_MS: i32 = 120_000;

/// Applies libgit2's process-wide connect and transfer timeouts to the remote
/// operations that start after this call.
///
/// `git2` 0.21 exposes no per-operation bound: libgit2 carries one in
/// `git_remote_connect_options.connect_timeout`, but that structure is not part
/// of the bindings, so the only reachable knobs are the `git_libgit2_opts`
/// options `GIT_OPT_SET_SERVER_CONNECT_TIMEOUT` (read when a socket stream is
/// created) and `GIT_OPT_SET_SERVER_TIMEOUT` (read per stalled read or write).
/// Both write C globals. This is a process-wide stop-gap, not a per-operation
/// deadline:
///
/// - The values belong to the whole process and to every libgit2 caller in it.
/// - They apply to connections opened afterwards; one already in flight keeps
///   the values it was created with.
/// - They cover libgit2's own socket transports (HTTP and HTTPS, whose TLS
///   stream wraps the plain socket). Transports that delegate to an external
///   program, such as SSH, use no socket stream and are unaffected.
/// - They bound a stalled remote, they do not retry: nothing here repeats an
///   operation, so a push that reached the remote is never sent twice.
///
/// # Safety
///
/// git2 marks the underlying setters unsafe because they write C globals
/// without synchronisation, and documents that they are only safe before any
/// thread is spawned. Each binary therefore calls this from `main`, before its
/// async runtime is built. Calling it later is a silent no-op at best: a
/// concurrent connection may have copied the previous value, and the write
/// itself races with those copies.
pub unsafe fn configure_remote_timeouts() {
    // SAFETY: the caller's obligation is the ordering documented above, and the
    // timeouts cannot be rejected: libgit2 only refuses a negative value.
    unsafe {
        git2::opts::set_server_connect_timeout_in_milliseconds(REMOTE_CONNECT_TIMEOUT_MS)
            .expect("libgit2 rejects only negative connect timeouts");
        git2::opts::set_server_timeout_in_milliseconds(REMOTE_TRANSFER_TIMEOUT_MS)
            .expect("libgit2 rejects only negative transfer timeouts");
    }
}

/// The proxy options every Loom remote operation passes explicitly.
///
/// libgit2 defaults an operation's proxy options to `GIT_PROXY_NONE`, which for
/// the builtin HTTP transport means no proxy at all: neither `http.<url>.proxy`
/// from Git configuration nor the `http_proxy`/`https_proxy`/`no_proxy`
/// environment variables that the `git` command line honours are consulted, so
/// no clone, fetch, or push succeeds on a network that is only reachable
/// through a proxy. Auto-detection restores that behaviour.
///
/// Unlike the timeout knobs above this is per-operation: the options travel with
/// the single fetch or push that carries them and need no process-global state.
fn remote_proxy_options() -> ProxyOptions<'static> {
    let mut proxy = ProxyOptions::new();
    proxy.auto();
    proxy
}

/// Resolves a credential request from a remote against the configured token.
///
/// SSH-key requests are answered with an SSH agent credential, user/password
/// requests with the token as the password, and every other request fails
/// explicitly so the remote reports a useful error instead of replaying the
/// callback. Failures name the remote but never include the token.
fn credential_for(
    url: &str,
    username_from_url: Option<&str>,
    allowed_types: git2::CredentialType,
    token: &str,
) -> std::result::Result<Cred, git2::Error> {
    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
        return Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"));
    }
    if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
        return Cred::userpass_plaintext(username_from_url.unwrap_or("x-access-token"), token);
    }
    Err(git2::Error::from_str(&format!(
        "remote '{url}' requested unsupported credential type {allowed_types:?}; Loom supplies \
         tokens over HTTPS and keys through the SSH agent"
    )))
}

/// Answers a credential request when no token is available: SSH keys come from
/// the agent, and any other request fails explicitly instead of replaying the
/// callback with an empty credential.
fn credential_without_token(
    url: &str,
    username_from_url: Option<&str>,
    allowed_types: git2::CredentialType,
) -> std::result::Result<Cred, git2::Error> {
    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
        return Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"));
    }
    Err(git2::Error::from_str(&format!(
        "remote '{url}' requested credentials but Loom holds no token for it"
    )))
}

fn git_error(operation: &str, error: git2::Error) -> LoomError {
    LoomError::new(ErrorCode::Vcs, format!("{operation}: {error}"), false)
}

/// The failure a bounded remote refresh reports once `deadline` has expired.
fn remote_refresh_deadline_error(deadline: Duration) -> LoomError {
    LoomError::new(
        ErrorCode::DeadlineExceeded,
        format!(
            "refreshing the 'origin' remote did not finish within {} ms; the drift from its default branch is unknown and no cached remote ref was used instead",
            deadline.as_millis()
        ),
        false,
    )
}

/// Waits up to `deadline` for a bounded remote refresh worker to report.
fn await_remote_refresh(
    receiver: &mpsc::Receiver<Result<RemoteRefreshReport>>,
    deadline: Duration,
) -> Result<RemoteRefreshReport> {
    match receiver.recv_timeout(deadline) {
        Ok(report) => report,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(remote_refresh_deadline_error(deadline)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(LoomError::new(
            ErrorCode::Internal,
            "the bounded remote refresh stopped without reporting a result",
            false,
        )),
    }
}

/// Looks up the `origin` remote, reporting a missing remote as such instead of
/// as a generic Git failure.
fn find_origin_remote(repository: &Repository) -> Result<git2::Remote<'_>> {
    repository.find_remote("origin").map_err(|error| {
        if error.code() == git2::ErrorCode::NotFound {
            LoomError::not_found("Git remote", "origin")
        } else {
            git_error("could not look up the 'origin' Git remote", error)
        }
    })
}

/// Runs one remote operation on a worker thread the caller waits at most
/// `deadline` for, so a stalled remote delays the caller by that deadline and
/// then reports [`ErrorCode::DeadlineExceeded`]. The worker is detached when the
/// deadline expires; it holds no lock and its eventual result is discarded.
///
/// `operation` names the read in every failure, so an expired read is never
/// mistaken for an answer.
fn bounded_remote_operation<T>(
    thread_name: &str,
    operation: &str,
    deadline: Duration,
    run: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            let _ = sender.send(run());
        })
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not start the bounded remote operation ({operation}): {error}"),
                false,
            )
        })?;
    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(LoomError::new(
            ErrorCode::DeadlineExceeded,
            format!(
                "{operation} did not finish within {} ms; no cached remote ref was used instead",
                deadline.as_millis()
            ),
            false,
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(LoomError::new(
            ErrorCode::Internal,
            format!(
                "the bounded remote operation ({operation}) stopped without reporting a result"
            ),
            false,
        )),
    }
}

/// Parses a revision the remote advertised, naming the ref in every failure.
fn remote_revision_oid(previous: &str, refname: &str) -> Result<Oid> {
    Oid::from_str(previous).map_err(|error| {
        LoomError::new(
            ErrorCode::Vcs,
            format!(
                "the 'origin' remote advertised an unreadable revision '{previous}' for '{refname}': {error}"
            ),
            false,
        )
    })
}

/// Counts the commits in the range `previous..revision` a push landed.
///
/// Counted with a revision walk rather than a merge-base comparison: replacing
/// history with an unrelated one is a legitimate force push, and the range is
/// then every commit the pushed revision carries.
fn count_landed_commits(repository: &Repository, previous: Oid, revision: Oid) -> Result<usize> {
    let mut walk = repository
        .revwalk()
        .map_err(|error| git_error("could not walk the Git history", error))?;
    walk.push(revision).map_err(|error| {
        git_error(
            &format!("could not walk the pushed revision {revision}"),
            error,
        )
    })?;
    walk.hide(previous).map_err(|error| {
        git_error(
            &format!("could not walk up to the previous remote revision {previous}"),
            error,
        )
    })?;
    Ok(walk.count())
}

/// Commits a tag push lands, counted from the peeled previous remote revision to
/// the peeled local one. `None` when either side does not peel to a commit, such
/// as a tag of a blob: there is then no commit range to count.
fn peeled_commit_commits(
    repository: &Repository,
    previous: &str,
    revision: Oid,
) -> Result<Option<usize>> {
    let peeled = |revision: &str| {
        repository
            .revparse_single(revision)
            .and_then(|object| object.peel_to_commit())
            .ok()
            .map(|commit| commit.id())
    };
    let (Some(previous_commit), Some(revision_commit)) =
        (peeled(previous), peeled(&revision.to_string()))
    else {
        return Ok(None);
    };
    count_landed_commits(repository, previous_commit, revision_commit).map(Some)
}

/// The `refs/remotes/origin/*` ref a fetched ref name is materialised as. A tag
/// keeps its `tags/` segment so it cannot shadow a branch of the same name.
fn remote_tracking_refname(refname: &str) -> String {
    let short = refname
        .strip_prefix("refs/heads/")
        .map(str::to_owned)
        .or_else(|| {
            refname
                .strip_prefix("refs/tags/")
                .map(|tag| format!("tags/{tag}"))
        })
        .unwrap_or_else(|| refname.to_owned());
    format!("refs/remotes/origin/{short}")
}

/// Revision `origin` advertises for the full ref name `refname`, read from the
/// refs the remote sends when it is contacted for a fetch. `None` means the
/// remote advertises no such ref, so a push would create it.
///
/// Runs on a bounded worker thread, so it never opens a second repository handle
/// on the caller's behalf.
fn read_remote_ref(root: &Path, refname: &str, token: &str) -> Result<Option<String>> {
    let repository = Repository::open(root)
        .map_err(|error| git_error("could not open Git repository", error))?;
    let mut remote = find_origin_remote(&repository)?;
    let token = token.to_owned();
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username_from_url, allowed_types| {
        credential_for(url, username_from_url, allowed_types, &token)
    });
    // The connection guard holds the mutable borrow of the remote, so it is
    // dropped before the refs are read; libgit2 keeps the refs the remote
    // advertised available after the connection closes.
    let connection = remote
        .connect_auth(
            Direction::Fetch,
            Some(callbacks),
            Some(remote_proxy_options()),
        )
        .map_err(|error| git_error("could not contact the 'origin' remote", error))?;
    drop(connection);
    let heads = remote.list().map_err(|error| {
        git_error(
            "could not read the refs the 'origin' remote advertises",
            error,
        )
    })?;
    Ok(heads
        .iter()
        .find(|head| head.name() == refname)
        .map(|head| head.oid().to_string()))
}

/// Fetches the full ref name `refname` from `origin` into
/// `refs/remotes/origin/<short name>`, so a revision the remote advertises but
/// this checkout does not hold yet becomes readable.
///
/// Runs on a bounded worker thread, so it never opens a second repository handle
/// on the caller's behalf.
fn fetch_remote_ref(root: &Path, refname: &str, revision: &str, token: &str) -> Result<()> {
    let repository = Repository::open(root)
        .map_err(|error| git_error("could not open Git repository", error))?;
    let mut remote = find_origin_remote(&repository)?;
    let token = token.to_owned();
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username_from_url, allowed_types| {
        credential_for(url, username_from_url, allowed_types, &token)
    });
    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);
    fetch_options.proxy_options(remote_proxy_options());
    let refspec = format!("+{refname}:{}", remote_tracking_refname(refname));
    remote
        .fetch(&[refspec.as_str()], Some(&mut fetch_options), None)
        .map_err(|error| {
            git_error(
                &format!("could not fetch the remote revision {revision} of '{refname}'"),
                error,
            )
        })
}

/// Reads the refs the `origin` remote advertises and, when `branch` is given and
/// both sides exist, the ahead/behind of that local branch against the remote
/// head of the same name.
///
/// Runs on a bounded worker thread, so it never opens a second repository handle
/// on the caller's behalf.
fn read_remote_refs(root: &Path, branch: Option<&str>, token: &str) -> Result<RemoteRefsReport> {
    let repository = Repository::open(root)
        .map_err(|error| git_error("could not open Git repository", error))?;
    let mut remote = find_origin_remote(&repository)?;
    let connection_token = token.to_owned();
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username_from_url, allowed_types| {
        credential_for(url, username_from_url, allowed_types, &connection_token)
    });
    // The connection guard holds the mutable borrow of the remote, so it is
    // dropped before the refs are read; libgit2 keeps the refs the remote
    // advertised available after the connection closes.
    let connection = remote
        .connect_auth(
            Direction::Fetch,
            Some(callbacks),
            Some(remote_proxy_options()),
        )
        .map_err(|error| git_error("could not contact the 'origin' remote", error))?;
    drop(connection);
    let advertised = remote.list().map_err(|error| {
        git_error(
            "could not read the refs the 'origin' remote advertises",
            error,
        )
    })?;
    let mut heads = Vec::new();
    let mut tags = Vec::new();
    for reference in advertised {
        let refname = reference.name();
        // A peeled tag entry names the commit the tag points at; it is not a ref
        // the remote holds, so it is not reported as one.
        if refname.ends_with("^{}") {
            continue;
        }
        let advertised_ref = RemoteRef {
            refname: refname.to_owned(),
            revision: reference.oid().to_string(),
        };
        if refname.starts_with("refs/heads/") {
            heads.push(advertised_ref);
        } else if refname.starts_with("refs/tags/") {
            tags.push(advertised_ref);
        }
    }
    heads.sort_by(|left, right| left.refname.cmp(&right.refname));
    tags.sort_by(|left, right| left.refname.cmp(&right.refname));
    let tracking = match branch {
        Some(branch) => remote_branch_tracking(&repository, &mut remote, branch, &heads, token)?,
        None => None,
    };
    Ok(RemoteRefsReport {
        remote: "origin".to_owned(),
        heads,
        tags,
        tracking,
    })
}

/// Ahead/behind of the local branch `branch` against the remote head of the same
/// name, materialised with one fetch into `refs/remotes/origin/<branch>`.
///
/// `None` when the remote advertises no such head or the checkout has no local
/// branch of that name: there is then nothing to compare.
fn remote_branch_tracking(
    repository: &Repository,
    remote: &mut git2::Remote<'_>,
    branch: &str,
    heads: &[RemoteRef],
    token: &str,
) -> Result<Option<RemoteBranchTracking>> {
    let refname = format!("refs/heads/{branch}");
    if !heads.iter().any(|head| head.refname == refname) {
        return Ok(None);
    }
    let local_branch = match repository.find_branch(branch, BranchType::Local) {
        Ok(local_branch) => local_branch,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(git_error("could not look up local Git branch", error)),
    };
    let local_commit = local_branch
        .into_reference()
        .peel_to_commit()
        .map_err(|error| {
            git_error(
                &format!("could not read the local Git branch '{branch}' as a commit"),
                error,
            )
        })?;
    let token_owned = token.to_owned();
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username_from_url, allowed_types| {
        credential_for(url, username_from_url, allowed_types, &token_owned)
    });
    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);
    fetch_options.proxy_options(remote_proxy_options());
    let refspec = format!("+{refname}:{}", remote_tracking_refname(&refname));
    remote
        .fetch(&[refspec.as_str()], Some(&mut fetch_options), None)
        .map_err(|error| {
            git_error(
                &format!("could not fetch the remote head '{refname}' for branch tracking"),
                error,
            )
        })?;
    let tracking_refname = remote_tracking_refname(&refname);
    let remote_commit = repository
        .find_reference(&tracking_refname)
        .and_then(|reference| reference.peel_to_commit())
        .map_err(|error| {
            git_error(
                &format!(
                    "the fetch left no remote head at '{tracking_refname}' to compare '{branch}' against"
                ),
                error,
            )
        })?;
    let (ahead, behind) = repository
        .graph_ahead_behind(local_commit.id(), remote_commit.id())
        .map_err(|error| {
            git_error(
                &format!("could not compare local branch '{branch}' with its remote head"),
                error,
            )
        })?;
    Ok(Some(RemoteBranchTracking {
        branch: branch.to_owned(),
        local_revision: local_commit.id().to_string(),
        remote_revision: remote_commit.id().to_string(),
        ahead,
        behind,
    }))
}

/// Fetches every head of `origin` into `refs/remotes/origin/*` and resolves the
/// default-branch tip from that fetch.
///
/// Runs on the refresh worker thread, so it never opens a second repository
/// handle on the caller's behalf.
fn fetch_origin_default_branch(
    root: &Path,
    local_revision: Option<&str>,
    token: Option<&str>,
) -> Result<RemoteRefreshReport> {
    let repository = Repository::open(root)
        .map_err(|error| git_error("could not open Git repository", error))?;
    let mut remote = find_origin_remote(&repository)?;
    let token = token.map(str::to_owned);
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(
        move |url, username_from_url, allowed_types| match token.as_deref() {
            Some(token) => credential_for(url, username_from_url, allowed_types, token),
            None => credential_without_token(url, username_from_url, allowed_types),
        },
    );
    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);
    remote
        .fetch(
            &["+refs/heads/*:refs/remotes/origin/*"],
            Some(&mut fetch_options),
            None,
        )
        .map_err(|error| git_error("could not fetch the 'origin' remote", error))?;

    let default_branch = resolve_remote_default_branch(&repository, &remote)?;
    let default_branch_refname = format!("refs/remotes/origin/{default_branch}");
    let default_branch_commit = repository
        .find_reference(&default_branch_refname)
        .and_then(|reference| reference.peel_to_commit())
        .map_err(|error| {
            git_error(
                &format!(
                    "the fetch left no default branch at '{default_branch_refname}' to compare against"
                ),
                error,
            )
        })?;
    let local_commit = match local_revision {
        Some(revision) => repository
            .revparse_single(revision)
            .and_then(|object| object.peel_to_commit())
            .map_err(|error| {
                git_error(
                    &format!("could not resolve local revision '{revision}'"),
                    error,
                )
            })?,
        None => repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| {
                git_error(
                    "could not resolve the checkout HEAD for the remote refresh",
                    error,
                )
            })?,
    };
    let (ahead, behind) = repository
        .graph_ahead_behind(local_commit.id(), default_branch_commit.id())
        .map_err(|error| {
            git_error(
                "could not compare the local revision with the fetched default branch",
                error,
            )
        })?;
    Ok(RemoteRefreshReport {
        remote: "origin".to_owned(),
        default_branch,
        default_branch_revision: default_branch_commit.id().to_string(),
        local_revision: local_commit.id().to_string(),
        ahead,
        behind,
        fetched_at: Timestamp::now(),
    })
}

/// Resolves the default branch name of a fetched remote.
///
/// The name is taken from the live connection when the remote advertises its
/// default branch, and otherwise from the cached `refs/remotes/origin/HEAD`
/// (or the conventional `main`/`master`). Only the name may come from a cached
/// ref; the revision compared against is always read from the remote-tracking
/// ref that this fetch just updated.
fn resolve_remote_default_branch(repository: &Repository, remote: &git2::Remote) -> Result<String> {
    let advertised = remote
        .default_branch()
        .ok()
        .and_then(|name| name.as_str().ok().map(str::to_owned));
    if let Some(name) = advertised.as_deref() {
        // The advertised name is normally a full ref name such as
        // `refs/heads/main`, but a short name is accepted as-is because the
        // caller verifies that the fetched ref exists before using it.
        let name = strip_heads_prefix(name).unwrap_or(name);
        if !name.is_empty() {
            return Ok(name.to_owned());
        }
    }
    if let Some(name) = repository
        .find_reference("refs/remotes/origin/HEAD")
        .ok()
        .and_then(|reference| {
            reference
                .symbolic_target()
                .ok()
                .flatten()
                .map(str::to_owned)
        })
        .as_deref()
        .and_then(strip_origin_prefix)
    {
        return Ok(name.to_owned());
    }
    for candidate in ["main", "master"] {
        if repository
            .find_reference(&format!("refs/remotes/origin/{candidate}"))
            .is_ok()
        {
            return Ok(candidate.to_owned());
        }
    }
    Err(LoomError::new(
        ErrorCode::Vcs,
        "the 'origin' remote did not report a default branch and no origin/main or origin/master ref exists after the fetch",
        false,
    ))
}

fn strip_heads_prefix(refname: &str) -> Option<&str> {
    refname.strip_prefix("refs/heads/")
}

fn strip_origin_prefix(refname: &str) -> Option<&str> {
    refname.strip_prefix("refs/remotes/origin/")
}

/// Points the `origin` remote at `origin_url`, creating it if the repository
/// was cloned from a local mirror and has no `origin` yet.
fn set_origin_url(repository: &Repository, origin_url: &str) -> Result<()> {
    let result = if repository.find_remote("origin").is_ok() {
        repository.remote_set_url("origin", origin_url)
    } else {
        repository.remote("origin", origin_url).map(|_| ())
    };
    result.map_err(|error| git_error("could not set repository origin", error))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use loom_core::AgentSessionId;

    use super::*;

    fn repository() -> (GitService, PathBuf) {
        let root = std::env::temp_dir().join(format!("loom-git-{}", AgentSessionId::new()));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-q"]);
        run(&root, &["config", "user.email", "loom@example.test"]);
        run(&root, &["config", "user.name", "Loom Test"]);
        fs::write(root.join("README.md"), "before\n").unwrap();
        run(&root, &["add", "--", "README.md"]);
        run(&root, &["commit", "-qm", "initial"]);
        (GitService::open(&root).unwrap(), root)
    }

    fn run(root: &Path, arguments: &[&str]) {
        assert!(
            Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .args(arguments)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    fn descendant_commit(git: &GitService, root: &Path) -> (Oid, String, String) {
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/ff-{}", loom_core::RepositoryId::new());
        run(root, &["checkout", "-qb", &child_branch]);
        fs::write(root.join("README.md"), "after\n").unwrap();
        run(root, &["add", "--", "README.md"]);
        run(root, &["commit", "-qm", "child change"]);
        let target = git.repository().unwrap().head().unwrap().target().unwrap();
        run(root, &["checkout", "-q", &parent_branch]);
        (target, child_branch, parent_branch)
    }

    #[test]
    fn cloning_creates_an_independent_working_tree() {
        let (source, source_root) = repository();
        let destination =
            std::env::temp_dir().join(format!("loom-git-clone-{}", loom_core::RepositoryId::new()));
        let cloned =
            GitService::clone_from(source.root().to_string_lossy(), &destination, None).unwrap();
        fs::write(cloned.root().join("README.md"), "changed in clone\n").unwrap();
        assert_eq!(
            fs::read_to_string(source.root().join("README.md")).unwrap(),
            "before\n"
        );
        assert!(!cloned.status().unwrap().clean);
        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    /// The token path builds its own fetch options, with the credential callback
    /// and the shared proxy options attached. A local source never asks for
    /// credentials, so this cannot prove the callback answers a real remote, but
    /// it does prove the options still clone over the local transport, and that
    /// the token is not required to reach the checkout.
    #[test]
    fn clone_with_a_supplied_token_clones_a_local_source() {
        let (source, source_root) = repository();
        let destination =
            std::env::temp_dir().join(format!("loom-git-token-{}", loom_core::RepositoryId::new()));
        let cloned = GitService::clone_from_authenticated(
            source.root().to_string_lossy(),
            &destination,
            Some("HEAD"),
            Some("test-token"),
        )
        .unwrap();
        // The requested revision was resolved and checked out by the clone.
        assert_eq!(
            cloned.status().unwrap().head.as_deref(),
            source.status().unwrap().head.as_deref()
        );
        assert!(cloned.status().unwrap().clean);
        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    /// A plain fast-forward push of `branch` to `refs/heads/<branch>`.
    fn branch_push(branch: &str) -> BranchPush<'_> {
        BranchPush {
            branch,
            remote_branch: None,
            lease: None,
            dry_run: false,
        }
    }

    #[test]
    fn pushing_a_branch_updates_the_origin_remote() {
        let (git, root) = repository();
        let bare =
            std::env::temp_dir().join(format!("loom-git-bare-{}", loom_core::RepositoryId::new()));
        Repository::init_bare(&bare).unwrap();
        run(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        let branch = git.current_branch().unwrap().unwrap();
        let tip = git
            .repository()
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        let report = git
            .push_branch_authenticated(
                branch_push(&branch),
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        let pushed = Repository::open_bare(&bare).unwrap();
        let remote_tip = pushed
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        assert_eq!(remote_tip, tip);
        // The report states the update that reached the remote, and the ref it
        // replaced did not exist before it.
        assert_eq!(report.refname, format!("refs/heads/{branch}"));
        assert_eq!(report.revision.as_deref(), Some(tip.as_str()));
        assert_eq!(report.previous_revision, None);
        assert_eq!(report.commits, None);
        assert_eq!(report.fast_forward, None);
        assert!(!report.deleted);
        assert!(!report.dry_run);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn pushing_rejects_invalid_or_missing_branches() {
        let (git, root) = repository();
        let bare =
            std::env::temp_dir().join(format!("loom-git-bare-{}", loom_core::RepositoryId::new()));
        Repository::init_bare(&bare).unwrap();
        run(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        assert!(
            git.push_branch_authenticated(
                branch_push("bad name"),
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE
            )
            .is_err()
        );
        assert!(
            git.push_branch_authenticated(
                branch_push("does-not-exist"),
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    /// Amends the checkout's tip, so the branch no longer descends from the
    /// revision the origin already has: the shape a linearised branch has.
    fn amend_branch_tip(git: &GitService, root: &Path) -> Oid {
        fs::write(root.join("README.md"), "rewritten\n").unwrap();
        run(root, &["add", "--", "README.md"]);
        run(root, &["commit", "-q", "--amend", "-m", "rewritten"]);
        git.repository().unwrap().head().unwrap().target().unwrap()
    }

    /// The revision a bare origin holds for `branch`.
    fn remote_tip(bare: &Path, branch: &str) -> Oid {
        Repository::open_bare(bare)
            .unwrap()
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap()
            .target()
            .unwrap()
    }

    #[test]
    fn force_pushing_with_a_matching_lease_rewrites_the_remote_ref() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let previous = remote_tip(&bare, &branch).to_string();
        let rewritten = amend_branch_tip(&git, &root);

        // Without a lease the update stays fast-forward-only.
        let refused = git
            .push_branch_authenticated(
                branch_push(&branch),
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap_err();
        assert!(
            refused.message.contains("cannot push non-fastforwardable"),
            "{}",
            refused.message
        );
        assert_eq!(remote_tip(&bare, &branch).to_string(), previous);

        let report = git
            .push_branch_authenticated(
                BranchPush {
                    lease: Some(&previous),
                    ..branch_push(&branch)
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert_eq!(report.refname, format!("refs/heads/{branch}"));
        assert_eq!(report.previous_revision.as_deref(), Some(previous.as_str()));
        assert_eq!(
            report.revision.as_deref(),
            Some(rewritten.to_string().as_str())
        );
        assert_eq!(report.commits, Some(1));
        assert_eq!(report.fast_forward, Some(false));
        assert_eq!(remote_tip(&bare, &branch), rewritten);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn force_pushing_with_a_mismatched_lease_is_refused() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let previous = remote_tip(&bare, &branch).to_string();
        let rewritten = amend_branch_tip(&git, &root).to_string();

        // The lease names the rewritten tip, while the remote still holds the
        // revision it replaced.
        let refused = git
            .push_branch_authenticated(
                BranchPush {
                    lease: Some(&rewritten),
                    ..branch_push(&branch)
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Conflict);
        assert!(
            refused
                .message
                .contains(&format!("is at {previous}, not at the lease {rewritten}")),
            "{}",
            refused.message
        );
        assert_eq!(remote_tip(&bare, &branch).to_string(), previous);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn force_pushing_a_branch_the_remote_does_not_have_is_refused() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let tip = git
            .repository()
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        run(&root, &["checkout", "-qb", "loom/new"]);

        let refused = git
            .push_branch_authenticated(
                BranchPush {
                    lease: Some(&tip),
                    ..branch_push("loom/new")
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Conflict);
        assert!(
            refused.message.contains("does not exist, so the lease"),
            "{}",
            refused.message
        );
        assert!(
            Repository::open_bare(&bare)
                .unwrap()
                .find_reference("refs/heads/loom/new")
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn force_pushing_counts_against_a_previous_revision_the_checkout_lacks() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        // The remote moves on in another clone, so the revision the lease names is
        // not in this checkout and has to be fetched to measure the range.
        advance_origin(&bare, &branch, 1);
        let moved = remote_tip(&bare, &branch);
        let rewritten = amend_branch_tip(&git, &root);
        assert!(
            !git.repository().unwrap().odb().unwrap().exists(moved),
            "the checkout unexpectedly holds {moved}"
        );

        let report = git
            .push_branch_authenticated(
                BranchPush {
                    lease: Some(&moved.to_string()),
                    ..branch_push(&branch)
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert_eq!(
            report.previous_revision.as_deref(),
            Some(moved.to_string().as_str())
        );
        assert_eq!(
            report.revision.as_deref(),
            Some(rewritten.to_string().as_str())
        );
        assert_eq!(report.commits, Some(1));
        assert_eq!(remote_tip(&bare, &branch), rewritten);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    /// The revision a repository (bare or not) holds for the full ref name
    /// `refname`.
    fn remote_ref(repository: &Path, refname: &str) -> String {
        Repository::open(repository)
            .unwrap()
            .find_reference(refname)
            .unwrap()
            .target()
            .unwrap()
            .to_string()
    }

    #[test]
    fn pushing_to_a_remote_branch_name_updates_that_ref() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let previous = remote_tip(&bare, &branch);
        write_commit(&root, "remote-name.txt", "remote name\n", "remote name");
        let first = head_revision(&git);

        let report = git
            .push_branch_authenticated(
                BranchPush {
                    branch: &branch,
                    remote_branch: Some("loom/remote"),
                    lease: None,
                    dry_run: false,
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        // The report names the target ref, and the local branch's own remote ref
        // was not touched by this push.
        assert_eq!(report.refname, "refs/heads/loom/remote");
        assert_eq!(report.previous_revision, None);
        assert_eq!(report.revision.as_deref(), Some(first.as_str()));
        assert_eq!(report.commits, None);
        assert_eq!(report.fast_forward, None);
        assert_eq!(remote_tip(&bare, "loom/remote").to_string(), first);
        assert_eq!(remote_tip(&bare, &branch), previous);

        // A second push to the same target ref is a fast-forward.
        write_commit(&root, "remote-name.txt", "second\n", "second remote name");
        let second = head_revision(&git);
        let report = git
            .push_branch_authenticated(
                BranchPush {
                    branch: &branch,
                    remote_branch: Some("loom/remote"),
                    lease: None,
                    dry_run: false,
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert_eq!(report.previous_revision.as_deref(), Some(first.as_str()));
        assert_eq!(report.revision.as_deref(), Some(second.as_str()));
        assert_eq!(report.commits, Some(1));
        assert_eq!(report.fast_forward, Some(true));
        assert_eq!(remote_tip(&bare, "loom/remote").to_string(), second);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn a_dry_run_push_reports_the_update_without_touching_the_remote() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let previous = remote_tip(&bare, &branch).to_string();
        write_commit(&root, "dry-run.txt", "dry run\n", "dry run change");
        let revision = head_revision(&git);

        let report = git
            .push_branch_authenticated(
                BranchPush {
                    branch: &branch,
                    remote_branch: None,
                    lease: None,
                    dry_run: true,
                },
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert!(report.dry_run);
        assert!(!report.deleted);
        assert_eq!(report.previous_revision.as_deref(), Some(previous.as_str()));
        assert_eq!(report.revision.as_deref(), Some(revision.as_str()));
        assert_eq!(report.commits, Some(1));
        assert_eq!(report.fast_forward, Some(true));
        // The same report a push would produce, with the remote left as it was.
        assert_eq!(remote_tip(&bare, &branch).to_string(), previous);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn deleting_a_remote_ref_removes_it_and_reports_the_previous_revision() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let previous = remote_tip(&bare, &branch).to_string();
        let refname = format!("refs/heads/{branch}");

        let dry_run = git
            .delete_remote_ref_authenticated(
                &refname,
                true,
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert!(dry_run.deleted);
        assert!(dry_run.dry_run);
        assert_eq!(dry_run.revision, None);
        assert_eq!(remote_tip(&bare, &branch).to_string(), previous);

        let report = git
            .delete_remote_ref_authenticated(
                &refname,
                false,
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert_eq!(report.refname, refname);
        assert!(report.deleted);
        assert!(!report.dry_run);
        assert_eq!(report.previous_revision.as_deref(), Some(previous.as_str()));
        assert_eq!(report.revision, None);
        assert_eq!(report.commits, None);
        assert_eq!(report.fast_forward, None);
        assert!(
            Repository::open_bare(&bare)
                .unwrap()
                .find_reference(&refname)
                .is_err()
        );

        // The ref is gone, so there is nothing left to delete.
        let absent = git
            .delete_remote_ref_authenticated(
                &refname,
                false,
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap_err();
        assert_eq!(absent.code, ErrorCode::NotFound);
        assert!(absent.message.contains(&refname), "{}", absent.message);

        let invalid = git
            .delete_remote_ref_authenticated(
                "not a ref name",
                false,
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap_err();
        assert_eq!(invalid.code, ErrorCode::InvalidRequest);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn pushing_a_tag_creates_the_remote_tag() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        run(&root, &["tag", "-a", "v1", "-m", "release v1"]);
        let tag_object = remote_ref(&root, "refs/tags/v1");

        let report = git
            .push_tag_authenticated("v1", false, "test-token", DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert_eq!(report.refname, "refs/tags/v1");
        assert_eq!(report.previous_revision, None);
        // The tag object itself is pushed, not the commit it peels to.
        assert_eq!(report.revision.as_deref(), Some(tag_object.as_str()));
        assert_eq!(report.commits, None);
        assert_eq!(report.fast_forward, None);
        assert!(!report.deleted);
        assert_eq!(remote_ref(&bare, "refs/tags/v1"), tag_object);

        // Recreating the tag locally and asking for a dry run measures the peeled
        // commit range and leaves the remote tag where it was.
        write_commit(&root, "tagged.txt", "tagged\n", "tagged change");
        run(&root, &["tag", "-f", "-a", "v1", "-m", "release v1 moved"]);
        let moved = remote_ref(&root, "refs/tags/v1");
        let dry_run = git
            .push_tag_authenticated("v1", true, "test-token", DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert!(dry_run.dry_run);
        assert_eq!(
            dry_run.previous_revision.as_deref(),
            Some(tag_object.as_str())
        );
        assert_eq!(dry_run.revision.as_deref(), Some(moved.as_str()));
        assert_eq!(dry_run.commits, Some(1));
        assert_eq!(remote_ref(&bare, "refs/tags/v1"), tag_object);

        assert!(
            git.push_tag_authenticated(
                "absent",
                false,
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE
            )
            .is_err()
        );
        assert!(
            git.push_tag_authenticated(
                "bad tag",
                false,
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE
            )
            .is_err()
        );

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn remote_refs_lists_heads_and_tags_with_branch_tracking() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        run(&root, &["tag", "-a", "v1", "-m", "release v1"]);
        git.push_tag_authenticated("v1", false, "test-token", DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        advance_origin(&bare, &branch, 2);

        let report = git
            .remote_refs_authenticated(Some(&branch), "test-token", DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert_eq!(report.remote, "origin");
        assert_eq!(
            report.heads,
            vec![RemoteRef {
                refname: format!("refs/heads/{branch}"),
                revision: remote_tip(&bare, &branch).to_string(),
            }]
        );
        // Only the tag itself is listed; its peeled `^{}` entry is not a ref.
        assert_eq!(report.tags.len(), 1);
        assert_eq!(report.tags[0].refname, "refs/tags/v1");
        let tracking = report.tracking.expect("tracking for the local branch");
        assert_eq!(tracking.branch, branch);
        assert_eq!(tracking.local_revision, head_revision(&git));
        assert_eq!(
            tracking.remote_revision,
            remote_tip(&bare, &branch).to_string()
        );
        assert_eq!((tracking.ahead, tracking.behind), (0, 2));

        // Without a branch name nothing is tracked, and a branch name the remote
        // does not advertise has no remote head to track against.
        assert!(
            git.remote_refs_authenticated(None, "test-token", DEFAULT_REMOTE_REFRESH_DEADLINE)
                .unwrap()
                .tracking
                .is_none()
        );
        assert!(
            git.remote_refs_authenticated(
                Some("loom/absent"),
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE
            )
            .unwrap()
            .tracking
            .is_none()
        );

        // A remote head with no local branch of that name has nothing to compare.
        git.push_branch_authenticated(
            BranchPush {
                branch: &branch,
                remote_branch: Some("loom/only-remote"),
                lease: None,
                dry_run: false,
            },
            "test-token",
            DEFAULT_REMOTE_REFRESH_DEADLINE,
        )
        .unwrap();
        let report = git
            .remote_refs_authenticated(
                Some("loom/only-remote"),
                "test-token",
                DEFAULT_REMOTE_REFRESH_DEADLINE,
            )
            .unwrap();
        assert!(report.tracking.is_none());
        assert!(
            report
                .heads
                .iter()
                .any(|head| head.refname == "refs/heads/loom/only-remote")
        );

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn remote_refresh_reports_drift_from_the_fetched_default_branch() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let base = git
            .repository()
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        let bare = origin_remote(&git, &root, &branch);

        let report = git
            .refresh_origin_default_branch(Some(&base), None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert_eq!(report.remote, "origin");
        assert_eq!(report.default_branch, branch);
        assert_eq!(report.default_branch_revision, base);
        assert_eq!(report.local_revision, base);
        assert_eq!((report.ahead, report.behind), (0, 0));

        advance_origin(&bare, &branch, 3);
        // The cached remote ref still points at the tip the checkout started
        // from until a fetch updates it, which is the trap this API closes.
        assert_eq!(
            git.repository()
                .unwrap()
                .find_reference(&format!("refs/remotes/origin/{branch}"))
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            base
        );

        let report = git
            .refresh_origin_default_branch(Some(&base), None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert_eq!((report.ahead, report.behind), (0, 3));
        assert_ne!(report.default_branch_revision, base);

        write_commit(&root, "local.txt", "local change\n", "local change");
        let report = git
            .refresh_origin_default_branch(None, None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert_eq!((report.ahead, report.behind), (1, 3));
        assert_eq!(
            report.local_revision,
            git.repository()
                .unwrap()
                .head()
                .unwrap()
                .target()
                .unwrap()
                .to_string()
        );

        fs::remove_dir_all(bare).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_refresh_reports_a_deadline_and_never_a_cached_ref() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        git.refresh_origin_default_branch(None, None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();

        // A worker that only reports after the deadline is surfaced as a
        // deadline instead of as an answer.
        let (_sender, receiver) = mpsc::channel();
        assert_eq!(
            await_remote_refresh(&receiver, Duration::from_millis(5))
                .unwrap_err()
                .code,
            ErrorCode::DeadlineExceeded
        );
        // A worker that stops without reporting is an internal failure rather
        // than an empty report.
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        assert_eq!(
            await_remote_refresh(&receiver, Duration::from_millis(5))
                .unwrap_err()
                .code,
            ErrorCode::Internal
        );

        // The public refresh degrades on a deadline instead of waiting for the
        // remote indefinitely.
        assert_eq!(
            git.refresh_origin_default_branch(None, None, Duration::ZERO)
                .unwrap_err()
                .code,
            ErrorCode::DeadlineExceeded
        );

        fs::remove_dir_all(bare).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_refresh_fails_instead_of_reporting_a_cached_remote_ref() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        git.refresh_origin_default_branch(None, None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();

        // With an unusable remote the refresh fails even though
        // origin/<branch> is cached locally and looks current.
        run(
            &root,
            &["remote", "set-url", "origin", "/nonexistent/loom-origin"],
        );
        assert_eq!(
            git.refresh_origin_default_branch(None, None, DEFAULT_REMOTE_REFRESH_DEADLINE)
                .unwrap_err()
                .code,
            ErrorCode::Vcs
        );

        run(&root, &["remote", "remove", "origin"]);
        assert_eq!(
            git.refresh_origin_default_branch(None, None, DEFAULT_REMOTE_REFRESH_DEADLINE)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );

        fs::remove_dir_all(bare).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    /// Revision the repository's `HEAD` resolves to.
    fn head_revision(git: &GitService) -> String {
        git.repository()
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string()
    }

    fn mirror_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "loom-git-mirror-{}",
            loom_core::RepositoryId::new()
        ))
    }

    #[test]
    fn create_mirror_reports_whether_it_cloned_the_mirror() {
        let (_git, root) = repository();
        let bare = std::env::temp_dir().join(format!(
            "loom-git-mirror-origin-{}",
            loom_core::RepositoryId::new()
        ));
        Repository::init_bare(&bare).unwrap();
        let mirror = mirror_root();

        assert_eq!(
            GitService::create_mirror(&root, &mirror, bare.to_str().unwrap()).unwrap(),
            MirrorCreation::Created
        );
        // A mirror that already exists is not cloned again, so the caller learns
        // that this call knows nothing new about the mirror's contents.
        assert_eq!(
            GitService::create_mirror(&root, &mirror, bare.to_str().unwrap()).unwrap(),
            MirrorCreation::Existing
        );

        fs::remove_dir_all(&mirror).unwrap();
        fs::remove_dir_all(&bare).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refreshing_a_mirror_moves_its_heads_to_the_fetched_tips() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let mirror = mirror_root();
        assert_eq!(
            GitService::create_mirror(&root, &mirror, bare.to_str().unwrap()).unwrap(),
            MirrorCreation::Created
        );
        let frozen = head_revision(&GitService::open(&mirror).unwrap());

        advance_origin(&bare, &branch, 2);
        let mirror_git = GitService::open(&mirror).unwrap();
        // A clone reads the mirror's heads, so before the refresh the mirror still
        // hands out the tip it was created from.
        assert_eq!(head_revision(&mirror_git), frozen);
        mirror_git
            .repository()
            .unwrap()
            .reference_symbolic(
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{branch}"),
                true,
                "loom test: record the remote default branch",
            )
            .unwrap();

        let report = mirror_git
            .refresh_mirror_heads(None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert_eq!(report.default_branch, branch);
        assert_ne!(report.default_branch_revision, frozen);
        // The fetched tip is now both the mirror's own head and what a checkout
        // cloned from the mirror starts at.
        assert_eq!(head_revision(&mirror_git), report.default_branch_revision);
        assert!(
            mirror_git
                .repository()
                .unwrap()
                .find_reference("refs/heads/HEAD")
                .is_err(),
            "a symbolic remote ref holds no tip to advance"
        );

        let destination = std::env::temp_dir().join(format!(
            "loom-git-mirror-clone-{}",
            loom_core::RepositoryId::new()
        ));
        let cloned = GitService::clone_from_local(
            &mirror,
            &destination,
            None,
            "https://example.test/owner/name.git",
        )
        .unwrap();
        assert_eq!(head_revision(&cloned), report.default_branch_revision);

        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir_all(&mirror).unwrap();
        fs::remove_dir_all(&bare).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refreshing_mirror_heads_rejects_a_working_checkout() {
        let (git, root) = repository();
        assert_eq!(
            git.refresh_mirror_heads(None, DEFAULT_REMOTE_REFRESH_DEADLINE)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolving_a_revision_accepts_a_local_tip_or_a_remote_tip() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap().unwrap();
        let bare = origin_remote(&git, &root, &branch);
        let revision = head_revision(&git);
        assert!(git.resolves_revision(&branch));
        assert!(git.resolves_revision(&revision));
        assert!(!git.resolves_revision("loom-unknown-revision"));

        // A remote tip resolves once a fetch has written it, which is the ref a
        // requested revision falls back to.
        let remote_tip = format!("refs/remotes/origin/{branch}");
        git.refresh_origin_default_branch(None, None, DEFAULT_REMOTE_REFRESH_DEADLINE)
            .unwrap();
        assert!(git.resolves_revision(&remote_tip));

        fs::remove_dir_all(bare).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credential_requests_are_answered_by_their_type() {
        // HTTPS remotes ask for user/password, so the token is the password.
        let userpass = credential_for(
            "https://example.test/owner/repo.git",
            Some("x-access-token"),
            git2::CredentialType::USER_PASS_PLAINTEXT | git2::CredentialType::DEFAULT,
            "test-token",
        )
        .unwrap();
        assert_eq!(
            i64::from(userpass.credtype()),
            i64::from(git2::CredentialType::USER_PASS_PLAINTEXT.bits())
        );

        // SSH remotes ask for a key, which the agent supplies instead of the
        // token. `credtype()` is `i32` on Windows and `u32` elsewhere.
        let ssh = credential_for(
            "ssh://git@example.test/owner/repo.git",
            Some("git"),
            git2::CredentialType::SSH_KEY | git2::CredentialType::SSH_CUSTOM,
            "test-token",
        )
        .unwrap();
        assert_eq!(
            i64::from(ssh.credtype()),
            i64::from(git2::CredentialType::SSH_KEY.bits())
        );

        // Any other request fails with the remote in the message instead of
        // replaying the callback, and never repeats the token.
        let error = match credential_for(
            "ssh://example.test/owner/repo.git",
            None,
            git2::CredentialType::USERNAME,
            "test-token",
        ) {
            Ok(_) => panic!("expected an unsupported credential type failure"),
            Err(error) => error,
        };
        assert!(error.message().contains("unsupported credential type"));
        assert!(
            error
                .message()
                .contains("ssh://example.test/owner/repo.git")
        );
        assert!(!error.message().contains("test-token"));
    }

    /// The startup hook writes libgit2's two process-global socket timeouts.
    ///
    /// This asserts that the values a binary applies are the documented ones and
    /// that applying them again changes nothing. The test reads them back through
    /// git2's getters, which are the only observable effect: there is no way to
    /// inspect a live connection's timeout without a network, and the ordering
    /// the safety contract requires (before any thread is spawned) cannot be
    /// reproduced inside a test binary that already runs tests in parallel. The
    /// write is therefore the same benign, identical-value write the binaries
    /// perform at startup.
    #[test]
    fn remote_timeout_configuration_applies_and_repeats_its_documented_values() {
        // SAFETY: no test in this crate opens a network connection, and every
        // write below stores the same values, so no concurrent socket stream can
        // observe an inconsistent pair.
        unsafe {
            configure_remote_timeouts();
            assert_eq!(
                git2::opts::get_server_connect_timeout_in_milliseconds().unwrap(),
                REMOTE_CONNECT_TIMEOUT_MS
            );
            assert_eq!(
                git2::opts::get_server_timeout_in_milliseconds().unwrap(),
                REMOTE_TRANSFER_TIMEOUT_MS
            );
        }

        // Applying it a second time is idempotent.
        // SAFETY: as above; the second call stores the same two values.
        unsafe {
            configure_remote_timeouts();
            assert_eq!(
                git2::opts::get_server_connect_timeout_in_milliseconds().unwrap(),
                REMOTE_CONNECT_TIMEOUT_MS
            );
            assert_eq!(
                git2::opts::get_server_timeout_in_milliseconds().unwrap(),
                REMOTE_TRANSFER_TIMEOUT_MS
            );
        }

        // Both bounds are non-zero: libgit2 treats 0 as "no timeout", which is
        // exactly the behaviour this hook replaces. These two relations are
        // properties of the constants, so they are checked when the test is
        // compiled.
        const { assert!(REMOTE_CONNECT_TIMEOUT_MS > 0) };
        const { assert!(REMOTE_TRANSFER_TIMEOUT_MS > 0) };
        const { assert!(REMOTE_CONNECT_TIMEOUT_MS < REMOTE_TRANSFER_TIMEOUT_MS) };
    }

    #[test]
    fn status_and_diff_are_structured() {
        let (git, root) = repository();
        fs::write(root.join("README.md"), "after\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        let status = git.status().unwrap();
        assert!(!status.clean);
        assert!(status.files.iter().any(|file| file.path == "README.md"));
        let changed = status
            .files
            .iter()
            .find(|file| file.path == "README.md")
            .unwrap();
        assert_eq!(
            (changed.worktree_additions, changed.worktree_deletions),
            (1, 1)
        );
        let diff = git.diff(Some("README.md"), false).unwrap();
        assert!(diff.patch.contains("-before"));
        assert!(diff.patch.contains("+after"));
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].lines[0].old_line, Some(1));
        assert_eq!(diff.hunks[0].lines[0].new_line, None);
        assert_eq!(diff.hunks[0].lines[1].kind, GitDiffLineKind::Added);
        let untracked = git.diff(Some("new.txt"), false).unwrap();
        assert_eq!(untracked.hunks[0].lines[0].content, "new");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_bounded_diff_keeps_whole_lines_under_counts_that_match_them() {
        let (git, root) = repository();
        let before = (1..=40)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("README.md"), &before).unwrap();
        run(&root, &["add", "--", "README.md"]);
        run(&root, &["commit", "-qm", "many lines"]);
        let after = before
            .replace("line 1\n", "first\n")
            .replace("line 40\n", "last\n");
        fs::write(root.join("README.md"), &after).unwrap();

        let full = git.diff(Some("README.md"), false).unwrap();
        assert_eq!(full.hunks.len(), 2, "patch: {}", full.patch);
        // A budget that lets the last hunk's header and part of its body in.
        let budget = full.patch.rfind("@@ ").unwrap() + 40;
        let limited = git.diff_limited(Some("README.md"), false, budget).unwrap();
        assert!(limited.truncated);
        assert!(full.patch.starts_with(&limited.patch));
        assert_eq!(limited.hunks.len(), 2);
        assert_eq!(
            limited.hunks[0].lines.len(),
            full.hunks[0].lines.len(),
            "a hunk that arrived whole is kept whole"
        );
        assert!(limited.hunks[1].lines.len() < full.hunks[1].lines.len());

        let hunk = limited.hunks.last().unwrap();
        let mut counted = (0u32, 0u32);
        let mut body = String::new();
        for line in &hunk.lines {
            let marker = match line.kind {
                GitDiffLineKind::Added => {
                    counted.1 += 1;
                    '+'
                }
                GitDiffLineKind::Removed => {
                    counted.0 += 1;
                    '-'
                }
                GitDiffLineKind::Context => {
                    counted.0 += 1;
                    counted.1 += 1;
                    ' '
                }
            };
            body.push(marker);
            body.push_str(&line.content);
            body.push('\n');
        }
        // The cut hunk declares exactly the lines its patch carries, and those
        // lines end the patch, so the bounded render is a patch a client can
        // display as sent.
        assert_eq!((hunk.old_lines, hunk.new_lines), counted);
        assert!(limited.patch.ends_with(&body));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revision_diff_includes_commits_and_unstaged_files_with_a_limit() {
        let (git, root) = repository();
        let base = git.repository().unwrap().head().unwrap().target().unwrap();
        fs::write(root.join("committed.txt"), "committed content\n").unwrap();
        run(&root, &["add", "--", "committed.txt"]);
        run(&root, &["commit", "-qm", "commit after base"]);
        fs::write(root.join("unstaged.txt"), "unstaged content\n").unwrap();

        let diff = git.diff_from_revision(&base.to_string(), 4096).unwrap();
        assert!(diff.patch.contains("committed.txt"));
        assert!(diff.patch.contains("+committed content"));
        assert!(diff.patch.contains("unstaged.txt"));
        assert!(diff.patch.contains("+unstaged content"));
        assert!(!diff.truncated);

        let limited = git.diff_from_revision(&base.to_string(), 24).unwrap();
        assert!(limited.truncated);
        assert!(limited.patch.len() <= 24);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn separate_changes_produce_navigable_hunks() {
        let (git, root) = repository();
        let before = (1..=30)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("README.md"), &before).unwrap();
        run(&root, &["add", "--", "README.md"]);
        run(&root, &["commit", "-qm", "long file"]);
        let after = before
            .replace("line 1\n", "first\n")
            .replace("line 30\n", "last\n");
        fs::write(root.join("README.md"), after).unwrap();

        let diff = git.diff(Some("README.md"), false).unwrap();
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.hunks[0].new_start, 1);
        assert!(diff.hunks[1].new_start > 20);
        assert_eq!(diff.hunks[1].lines.last().unwrap().new_line, Some(30));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binary_changes_have_an_explicit_state() {
        let (git, root) = repository();
        fs::write(root.join("image.bin"), b"\0\x01\x02").unwrap();
        let diff = git.diff(Some("image.bin"), false).unwrap();
        assert!(diff.binary);
        assert!(diff.hunks.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_out_of_scope_diff_paths() {
        let (git, root) = repository();
        assert_eq!(
            git.diff(Some("../secret"), false).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_awareness_is_available() {
        let (git, root) = repository();
        let branch = git.current_branch().unwrap();
        assert!(branch.is_some());
        assert!(git.branches().unwrap().iter().any(|branch| branch.current));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_checks_out_and_advances_branch() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (target, _child_branch, parent_branch) = descendant_commit(&git, &root);

        assert_eq!(git.advance_clean_head(expected, target).unwrap(), target);
        let repository = git.repository().unwrap();
        assert_eq!(repository.head().unwrap().target(), Some(target));
        assert_eq!(
            git.current_branch().unwrap().as_deref(),
            Some(parent_branch.as_str())
        );
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "after\n"
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_accepts_persisted_revision_strings() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (target, _, _) = descendant_commit(&git, &root);

        assert_eq!(
            git.advance_clean_head_revisions(&expected.to_string(), &target.to_string())
                .unwrap(),
            target.to_string()
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(target)
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_rejects_stale_expected_head() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (target, child_branch, _) = descendant_commit(&git, &root);
        run(&root, &["checkout", "-q", &child_branch]);

        assert_eq!(
            git.advance_clean_head(expected, target).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(target)
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_rejects_non_descendant_target() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let unrelated_branch = format!("codex/unrelated-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-q", "--orphan", &unrelated_branch]);
        fs::write(root.join("README.md"), "unrelated\n").unwrap();
        run(&root, &["add", "--", "README.md"]);
        run(&root, &["commit", "-qm", "unrelated change"]);
        let unrelated = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);

        assert_eq!(
            git.advance_clean_head(expected, unrelated)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(expected)
        );
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "before\n"
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_rejects_detached_head() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        git.repository()
            .unwrap()
            .set_head_detached(expected)
            .unwrap();

        assert_eq!(
            git.advance_clean_head(expected, expected).unwrap_err().code,
            ErrorCode::InvalidState
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_rejects_dirty_parent_checkout() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (target, _, _) = descendant_commit(&git, &root);
        fs::write(root.join("README.md"), "local change\n").unwrap();

        assert_eq!(
            git.advance_clean_head(expected, target).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(expected)
        );
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "local change\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_head_fast_forward_ignores_ignored_build_output() {
        let (git, root) = repository();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        run(&root, &["add", "--", ".gitignore"]);
        run(&root, &["commit", "-qm", "ignore build output"]);
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (target, _, _) = descendant_commit(&git, &root);
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/artifact"), "build output\n").unwrap();

        // Ignored build output is in neither revision, so it must not block a
        // fast-forward and must be left untouched.
        assert_eq!(git.advance_clean_head(expected, target).unwrap(), target);
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(target)
        );
        assert_eq!(
            fs::read_to_string(root.join("target/artifact")).unwrap(),
            "build output\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn write_commit(root: &Path, file: &str, content: &str, message: &str) {
        fs::write(root.join(file), content).unwrap();
        run(root, &["add", "--", file]);
        run(root, &["commit", "-qm", message]);
    }

    /// Points the checkout's `origin` at a fresh bare repository that already
    /// holds `branch` at the checkout's current revision, and records the
    /// remote's default branch the way a clone would.
    fn origin_remote(git: &GitService, root: &Path, branch: &str) -> PathBuf {
        let bare = std::env::temp_dir().join(format!(
            "loom-git-origin-{}",
            loom_core::RepositoryId::new()
        ));
        let repository = Repository::init_bare(&bare).unwrap();
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .unwrap();
        drop(repository);
        run(root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git.push_branch_authenticated(
            BranchPush {
                branch,
                remote_branch: None,
                lease: None,
                dry_run: false,
            },
            "test-token",
            DEFAULT_REMOTE_REFRESH_DEADLINE,
        )
        .unwrap();
        git.repository()
            .unwrap()
            .reference_symbolic(
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{branch}"),
                true,
                "loom test: record the remote default branch",
            )
            .unwrap();
        bare
    }

    /// Adds `count` commits to `branch` on the origin through a second clone,
    /// so a checkout that does not fetch falls behind by exactly that many
    /// commits without any network access.
    fn advance_origin(bare: &Path, branch: &str, count: usize) {
        let work = std::env::temp_dir().join(format!(
            "loom-git-origin-work-{}",
            loom_core::RepositoryId::new()
        ));
        GitService::clone_from(bare.to_str().unwrap(), &work, None).unwrap();
        run(&work, &["config", "user.email", "loom@example.test"]);
        run(&work, &["config", "user.name", "Loom Test"]);
        for index in 0..count {
            write_commit(
                &work,
                &format!("upstream-{index}.txt"),
                "upstream change\n",
                &format!("upstream change {index}"),
            );
        }
        run(&work, &["push", "-q", "origin", branch]);
        fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn merge_integration_fast_forwards_a_descendant_child() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (target, _, _) = descendant_commit(&git, &root);

        assert_eq!(
            git.integrate_merge(expected, target, "integrate child")
                .unwrap(),
            MergeIntegrationOutcome::FastForward(target)
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(target)
        );
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "after\n"
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_integration_merges_an_advanced_parent_without_conflict() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/child-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "child.txt", "child\n", "child change");
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "README.md", "parent advanced\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();

        let MergeIntegrationOutcome::Merged(merge) = git
            .integrate_merge(parent_advanced, child, "integrate child")
            .unwrap()
        else {
            panic!("expected a clean merge commit");
        };
        let repository = git.repository().unwrap();
        assert_eq!(repository.head().unwrap().target(), Some(merge));
        let commit = repository.find_commit(merge).unwrap();
        assert_eq!(commit.parent_count(), 2);
        assert_eq!(commit.parent_id(0).unwrap(), parent_advanced);
        assert_eq!(commit.parent_id(1).unwrap(), child);
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "parent advanced\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("child.txt")).unwrap(),
            "child\n"
        );
        assert!(git.status().unwrap().clean);
        assert_eq!(
            git.integrate_merge(merge, child, "integrate child")
                .unwrap(),
            MergeIntegrationOutcome::AlreadyPresent
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_integration_reports_conflicts_without_touching_the_checkout() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/child-{}", loom_core::RepositoryId::new());
        let base = git.repository().unwrap().head().unwrap().target().unwrap();
        assert!(
            git.is_ancestor_revision(&base.to_string(), &base.to_string())
                .unwrap()
        );
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "README.md", "child version\n", "child change");
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "README.md", "parent version\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();

        assert_eq!(
            git.integrate_merge(parent_advanced, child, "integrate child")
                .unwrap(),
            MergeIntegrationOutcome::Conflicted(vec!["README.md".to_owned()])
        );
        let repository = git.repository().unwrap();
        assert_eq!(repository.head().unwrap().target(), Some(parent_advanced));
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "parent version\n"
        );
        let status = git.status().unwrap();
        assert!(status.clean);
        assert!(status.conflicts.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_replays_a_child_branch_onto_the_target_and_keeps_its_message() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/replay-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(
            &root,
            "child.txt",
            "child\n",
            "feat: child change\n\nwhy the child changed",
        );
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "parent.txt", "parent\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &child_branch]);

        let RebaseIntegrationOutcome::Rebased(tip) = git
            .rebase_onto_revision(&parent_advanced.to_string())
            .unwrap()
        else {
            panic!("expected the child branch to be replayed");
        };
        assert_ne!(tip, child, "the replayed revision is a new commit");
        let repository = git.repository().unwrap();
        assert_eq!(repository.head().unwrap().target(), Some(tip));
        assert_eq!(
            git.current_branch().unwrap().as_deref(),
            Some(child_branch.as_str())
        );
        let commit = repository.find_commit(tip).unwrap();
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0).unwrap(), parent_advanced);
        assert_eq!(commit.author().name().unwrap(), "Loom Test");
        assert_eq!(
            git.commit_message(&tip.to_string()).unwrap(),
            "feat: child change\n\nwhy the child changed\n"
        );
        assert!(
            git.is_ancestor_revision(&parent_advanced.to_string(), &tip.to_string())
                .unwrap()
        );
        assert_eq!(
            fs::read_to_string(root.join("child.txt")).unwrap(),
            "child\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("parent.txt")).unwrap(),
            "parent\n"
        );
        let status = git.status().unwrap();
        assert!(status.clean);
        assert!(status.conflicts.is_empty());
        assert!(!git.operation_in_progress().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_reports_conflicts_without_touching_the_checkout() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/replay-conflict-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "README.md", "child version\n", "child change");
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "README.md", "parent version\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &child_branch]);

        assert_eq!(
            git.rebase_onto_revision(&parent_advanced.to_string())
                .unwrap(),
            RebaseIntegrationOutcome::Conflicted(vec!["README.md".to_owned()])
        );
        let repository = git.repository().unwrap();
        assert_eq!(repository.head().unwrap().target(), Some(child));
        assert_eq!(repository.state(), RepositoryState::Clean);
        assert!(!root.join(".git").join("rebase-merge").exists());
        assert!(!root.join(".git").join("REBASE_HEAD").exists());
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "child version\n"
        );
        let status = git.status().unwrap();
        assert!(status.clean);
        assert!(status.conflicts.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_leaves_a_branch_that_merges_its_own_history_to_the_caller() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/replay-merge-{}", loom_core::RepositoryId::new());
        let side_branch = format!("codex/replay-side-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "child.txt", "child\n", "child change");
        run(&root, &["checkout", "-qb", &side_branch]);
        write_commit(&root, "side.txt", "side\n", "side change");
        run(&root, &["checkout", "-q", &child_branch]);
        run(
            &root,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge side work",
                &side_branch,
            ],
        );
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "parent.txt", "parent\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &child_branch]);

        assert_eq!(
            git.rebase_onto_revision(&parent_advanced.to_string())
                .unwrap(),
            RebaseIntegrationOutcome::MergeRequired
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(child)
        );
        let status = git.status().unwrap();
        assert!(status.clean);
        assert!(status.conflicts.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_drops_a_change_the_target_already_has() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/replay-empty-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "README.md", "child version\n", "child change");
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        // The parent reaches the same content through its own commit.
        write_commit(&root, "README.md", "child version\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &child_branch]);

        let RebaseIntegrationOutcome::Rebased(tip) = git
            .rebase_onto_revision(&parent_advanced.to_string())
            .unwrap()
        else {
            panic!("expected the already-applied change to be dropped");
        };
        assert_ne!(tip, child);
        assert_eq!(tip, parent_advanced);
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(parent_advanced)
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_leaves_a_branch_the_target_already_contains_alone() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let base = git.repository().unwrap().head().unwrap().target().unwrap();
        let child_branch = format!("codex/replay-contained-{}", loom_core::RepositoryId::new());
        run(
            &root,
            &["checkout", "-qb", &child_branch, &base.to_string()],
        );
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "parent.txt", "parent\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &child_branch]);

        assert_eq!(
            git.rebase_onto_revision(&parent_advanced.to_string())
                .unwrap(),
            RebaseIntegrationOutcome::Rebased(base)
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(base)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_refuses_an_unrelated_history() {
        let (git, root) = repository();
        let base = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", "--orphan", "unrelated"]);
        write_commit(&root, "orphan.txt", "orphan\n", "orphan change");

        assert_eq!(
            git.rebase_onto_revision(&base.to_string()).unwrap(),
            RebaseIntegrationOutcome::MergeRequired
        );
        assert_ne!(
            git.repository().unwrap().head().unwrap().target(),
            Some(base)
        );
        assert!(git.status().unwrap().clean);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_integration_locks_the_parent_branch_before_checkout() {
        let (git, root) = repository();
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/child-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "child.txt", "child\n", "child change");
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "README.md", "parent advanced\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();

        // Another writer holds the parent branch lock, mirroring the race the
        // locked-branch re-check guards against: the merge result must not
        // reach the working tree because the branch cannot move.
        let holder = git.repository().unwrap();
        let mut held = holder.transaction().unwrap();
        held.lock_ref(&format!("refs/heads/{parent_branch}"))
            .unwrap();

        assert_eq!(
            git.integrate_merge(parent_advanced, child, "integrate child")
                .unwrap_err()
                .code,
            ErrorCode::Vcs
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(parent_advanced)
        );
        assert!(!root.join("child.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "parent advanced\n"
        );
        assert!(git.status().unwrap().clean);

        drop(held);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_integration_rejects_a_dirty_parent_checkout() {
        let (git, root) = repository();
        let expected = git.repository().unwrap().head().unwrap().target().unwrap();
        let (child, _, _) = descendant_commit(&git, &root);
        fs::write(root.join("README.md"), "local change\n").unwrap();

        assert_eq!(
            git.integrate_merge(expected, child, "integrate child")
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(expected)
        );
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "local change\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merge_integration_ignores_ignored_build_output() {
        let (git, root) = repository();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        run(&root, &["add", "--", ".gitignore"]);
        run(&root, &["commit", "-qm", "ignore build output"]);
        let parent_branch = git.current_branch().unwrap().unwrap();
        let child_branch = format!("codex/child-{}", loom_core::RepositoryId::new());
        run(&root, &["checkout", "-qb", &child_branch]);
        write_commit(&root, "child.txt", "child\n", "child change");
        let child = git.repository().unwrap().head().unwrap().target().unwrap();
        run(&root, &["checkout", "-q", &parent_branch]);
        write_commit(&root, "README.md", "parent advanced\n", "parent change");
        let parent_advanced = git.repository().unwrap().head().unwrap().target().unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/artifact"), "build output\n").unwrap();

        // Ignored build output cannot conflict with the merge and must not
        // block it; the file is left in place for the next build.
        let MergeIntegrationOutcome::Merged(merge) = git
            .integrate_merge(parent_advanced, child, "integrate child")
            .unwrap()
        else {
            panic!("expected a clean merge commit");
        };
        assert_eq!(
            git.repository().unwrap().head().unwrap().target(),
            Some(merge)
        );
        assert_eq!(
            fs::read_to_string(root.join("target/artifact")).unwrap(),
            "build output\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_worktree_uses_the_requested_commit_and_new_branch() {
        let (git, root) = repository();
        let base_commit = git.repository().unwrap().head().unwrap().target().unwrap();
        let id = loom_core::RepositoryId::new();
        let worktree_name = format!("child-{id}");
        let branch_name = format!("codex/child-{id}");
        let path = std::env::temp_dir().join(format!("loom-linked-{id}"));
        let parent_branch = git.current_branch().unwrap();
        let expected_commit = base_commit.to_string();

        let child = git
            .create_linked_worktree(&worktree_name, &branch_name, &path, base_commit)
            .unwrap();

        assert_eq!(
            child.current_branch().unwrap().as_deref(),
            Some(branch_name.as_str())
        );
        assert_eq!(
            child.status().unwrap().head.as_deref(),
            Some(expected_commit.as_str())
        );
        assert!(child.status().unwrap().clean);
        assert_eq!(git.current_branch().unwrap(), parent_branch);
        assert!(
            git.branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name == branch_name)
        );

        let colliding_path = std::env::temp_dir().join(format!("loom-linked-{id}-duplicate"));
        let duplicate_branch = format!("codex/duplicate-{id}");
        assert_eq!(
            git.create_linked_worktree(
                &worktree_name,
                &duplicate_branch,
                &colliding_path,
                base_commit
            )
            .unwrap_err()
            .code,
            ErrorCode::Conflict
        );
        assert!(path.exists());
        assert!(!colliding_path.exists());
        assert!(
            git.repository()
                .unwrap()
                .find_reference(&format!("refs/heads/{duplicate_branch}"))
                .is_err()
        );

        git.remove_linked_worktree(&worktree_name, false).unwrap();
        assert!(!path.exists());
        assert!(
            git.branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name == branch_name)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_worktree_revision_wrapper_and_registered_open_are_validated() {
        let (git, root) = repository();
        let base_commit = git.repository().unwrap().head().unwrap().target().unwrap();
        let id = loom_core::RepositoryId::new();
        let worktree_name = format!("child-{id}");
        let branch_name = format!("codex/child-{id}");
        let path = std::env::temp_dir().join(format!("loom-linked-{id}"));

        assert_eq!(
            git.create_linked_worktree_at_revision(
                &format!("invalid-{id}"),
                &format!("codex/invalid-{id}"),
                std::env::temp_dir().join(format!("loom-invalid-{id}")),
                "not-an-oid"
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
        git.create_linked_worktree_at_revision(
            &worktree_name,
            &branch_name,
            &path,
            &base_commit.to_string(),
        )
        .unwrap();

        let opened = git.open_linked_worktree(&worktree_name, &path).unwrap();
        assert_eq!(opened.root(), path.canonicalize().unwrap());
        assert_eq!(
            opened.current_branch().unwrap().as_deref(),
            Some(branch_name.as_str())
        );
        assert_eq!(
            git.open_linked_worktree(&worktree_name, &root)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        git.remove_linked_worktree(&worktree_name, false).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_worktree_creation_rejects_branch_and_path_collisions() {
        let (git, root) = repository();
        let base_commit = git.repository().unwrap().head().unwrap().target().unwrap();
        let id = loom_core::RepositoryId::new();
        let current_branch = git.current_branch().unwrap().unwrap();
        let branch_collision_path = std::env::temp_dir().join(format!("loom-linked-{id}-branch"));
        assert_eq!(
            git.create_linked_worktree(
                &format!("child-{id}-branch"),
                &current_branch,
                &branch_collision_path,
                base_commit
            )
            .unwrap_err()
            .code,
            ErrorCode::Conflict
        );
        assert!(!branch_collision_path.exists());

        let path_collision = std::env::temp_dir().join(format!("loom-linked-{id}-existing"));
        fs::create_dir(&path_collision).unwrap();
        let branch_name = format!("codex/child-{id}-existing");
        assert_eq!(
            git.create_linked_worktree(
                &format!("child-{id}-existing"),
                &branch_name,
                &path_collision,
                base_commit
            )
            .unwrap_err()
            .code,
            ErrorCode::Conflict
        );
        assert!(
            git.repository()
                .unwrap()
                .find_reference(&format!("refs/heads/{branch_name}"))
                .is_err()
        );
        fs::remove_dir_all(path_collision).unwrap();

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_worktree_removal_preserves_changes_without_force() {
        let (git, root) = repository();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        run(&root, &["add", "--", ".gitignore"]);
        run(&root, &["commit", "-qm", "ignore generated files"]);
        let base_commit = git.repository().unwrap().head().unwrap().target().unwrap();
        let id = loom_core::RepositoryId::new();
        let worktree_name = format!("child-{id}");
        let branch_name = format!("codex/child-{id}");
        let path = std::env::temp_dir().join(format!("loom-linked-{id}"));
        git.create_linked_worktree(&worktree_name, &branch_name, &path, base_commit)
            .unwrap();
        fs::write(path.join("ignored.txt"), "keep unless forced\n").unwrap();

        assert_eq!(
            git.remove_linked_worktree(&worktree_name, false)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            fs::read_to_string(path.join("ignored.txt")).unwrap(),
            "keep unless forced\n"
        );

        git.remove_linked_worktree(&worktree_name, true).unwrap();
        assert!(!path.exists());
        assert!(
            git.branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name == branch_name)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_worktree_lock_state_gates_removal() {
        let (git, root) = repository();
        let base_commit = git.repository().unwrap().head().unwrap().target().unwrap();
        let id = loom_core::RepositoryId::new();
        let worktree_name = format!("child-{id}");
        let branch_name = format!("codex/child-{id}");
        let path = std::env::temp_dir().join(format!("loom-linked-{id}"));
        git.create_linked_worktree(&worktree_name, &branch_name, &path, base_commit)
            .unwrap();
        assert!(!git.linked_worktree_is_locked(&worktree_name).unwrap());
        assert!(
            !git.linked_worktree_is_locked(&format!("missing-{id}"))
                .unwrap()
        );

        git.repository()
            .unwrap()
            .find_worktree(&worktree_name)
            .unwrap()
            .lock(Some("loom test"))
            .unwrap();
        assert!(git.linked_worktree_is_locked(&worktree_name).unwrap());
        assert_eq!(
            git.remove_linked_worktree(&worktree_name, false)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert!(path.exists());

        git.repository()
            .unwrap()
            .find_worktree(&worktree_name)
            .unwrap()
            .unlock()
            .unwrap();
        assert!(!git.linked_worktree_is_locked(&worktree_name).unwrap());
        git.remove_linked_worktree(&worktree_name, false).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_branch_does_not_leave_a_worktree_path() {
        let (git, root) = repository();
        let base_commit = git.repository().unwrap().head().unwrap().target().unwrap();
        let id = loom_core::RepositoryId::new();
        let path = std::env::temp_dir().join(format!("loom-linked-{id}"));
        assert_eq!(
            git.create_linked_worktree(
                &format!("child-{id}"),
                "invalid..branch",
                &path,
                base_commit
            )
            .unwrap_err()
            .code,
            ErrorCode::Vcs
        );
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_open_init_and_clone_failures_are_structured() {
        let root = std::env::temp_dir().join(format!("loom-git-errors-{}", AgentSessionId::new()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("not-a-directory");
        fs::write(&file, "file").unwrap();
        assert_eq!(
            GitService::init(&file).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        assert_eq!(
            GitService::open(&file).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        assert_eq!(GitService::open(&root).unwrap_err().code, ErrorCode::Vcs);

        let destination = root.join("already-exists");
        fs::create_dir(&destination).unwrap();
        assert_eq!(
            GitService::clone_from("missing", &destination, None)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        fs::remove_dir(&destination).unwrap();
        assert_eq!(
            GitService::clone_from("/path/that/does/not/exist", &destination, None)
                .unwrap_err()
                .code,
            ErrorCode::Vcs
        );
        let (source, source_root) = repository();
        assert_eq!(
            GitService::clone_from(
                source.root().to_string_lossy(),
                &destination,
                Some("missing-ref")
            )
            .unwrap_err()
            .code,
            ErrorCode::Vcs
        );
        fs::remove_dir_all(&destination).unwrap();
        fs::remove_dir_all(source_root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_supports_an_empty_repository_and_staged_diffs() {
        let root = std::env::temp_dir().join(format!("loom-git-init-{}", AgentSessionId::new()));
        fs::create_dir_all(&root).unwrap();
        let git = GitService::init(&root).unwrap();
        assert_eq!(git.current_branch().unwrap(), None);
        assert!(git.branches().unwrap().is_empty());
        fs::write(root.join("new.txt"), "added\n").unwrap();
        run(&root, &["add", "--", "new.txt"]);
        let diff = git.diff(Some("new.txt"), true).unwrap();
        assert!(diff.staged);
        assert!(diff.patch.contains("+added"));
        assert_eq!(
            git.validate_path("nested\\file.txt").unwrap(),
            "nested/file.txt"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn set_identity_configures_the_repository_commit_author() {
        let root =
            std::env::temp_dir().join(format!("loom-git-identity-{}", AgentSessionId::new()));
        fs::create_dir_all(&root).unwrap();
        let git = GitService::init(&root).unwrap();

        git.set_identity("Loom Author", "author@example.test")
            .unwrap();
        assert_eq!(
            git.configured_identity().unwrap(),
            Some(("Loom Author".to_owned(), "author@example.test".to_owned()))
        );

        // The identity is written to the local config, so it survives reopen.
        let reopened = GitService::open(&root).unwrap();
        assert_eq!(
            reopened.configured_identity().unwrap(),
            Some(("Loom Author".to_owned(), "author@example.test".to_owned()))
        );

        // A blank half is not a usable identity.
        run(&root, &["config", "user.name", "   "]);
        assert_eq!(git.configured_identity().unwrap(), None);

        assert_eq!(
            git.set_identity("", "author@example.test")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            git.set_identity("author", "  ").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_kinds_cover_index_and_worktree_states() {
        assert_eq!(
            status_kind(Status::CONFLICTED, false),
            GitFileStatusKind::Conflicted
        );
        assert_eq!(
            status_kind(Status::IGNORED, false),
            GitFileStatusKind::Ignored
        );
        assert_eq!(
            status_kind(Status::INDEX_RENAMED, true),
            GitFileStatusKind::Renamed
        );
        assert_eq!(
            status_kind(Status::INDEX_NEW, true),
            GitFileStatusKind::Added
        );
        assert_eq!(
            status_kind(Status::INDEX_MODIFIED, true),
            GitFileStatusKind::Modified
        );
        assert_eq!(
            status_kind(Status::INDEX_DELETED, true),
            GitFileStatusKind::Deleted
        );
        assert_eq!(
            status_kind(Status::WT_RENAMED, false),
            GitFileStatusKind::Renamed
        );
        assert_eq!(
            status_kind(Status::WT_NEW, false),
            GitFileStatusKind::Untracked
        );
        assert_eq!(
            status_kind(Status::WT_MODIFIED, false),
            GitFileStatusKind::Modified
        );
        assert_eq!(
            status_kind(Status::WT_DELETED, false),
            GitFileStatusKind::Deleted
        );
        assert_eq!(
            status_kind(Status::CURRENT, true),
            GitFileStatusKind::Unknown
        );
        assert_eq!(
            status_kind(Status::CURRENT, false),
            GitFileStatusKind::Unknown
        );
    }

    #[test]
    fn clone_can_check_out_a_requested_revision() {
        let (source, source_root) = repository();
        let destination = std::env::temp_dir().join(format!(
            "loom-git-revision-{}",
            loom_core::RepositoryId::new()
        ));
        let cloned =
            GitService::clone_from(source.root().to_string_lossy(), &destination, Some("HEAD"))
                .unwrap();
        assert_eq!(cloned.current_branch().unwrap(), None);
        assert!(cloned.status().unwrap().clean);
        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn validates_empty_absolute_and_parent_paths() {
        let (git, root) = repository();
        for path in ["", "   ", "/etc/passwd", "../secret"] {
            assert_eq!(
                git.diff(Some(path), false).unwrap_err().code,
                ErrorCode::WorkspaceAccessDenied,
                "path should be rejected: {path:?}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
