use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use git2::{
    BranchType, Cred, DiffFormat, DiffOptions, FetchOptions, MergeOptions, Oid, PushOptions,
    RemoteCallbacks, Repository, RepositoryState, Signature, Status, StatusOptions,
    WorktreeAddOptions, WorktreeLockStatus, WorktreePruneOptions,
    build::{CheckoutBuilder, RepoBuilder},
};
use loom_core::{ErrorCode, LoomError, Result, Timestamp};
pub use loom_protocol::{
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
        let repository = if let Some(token) = token {
            let token = token.to_owned();
            let mut callbacks = RemoteCallbacks::new();
            callbacks.credentials(move |_url, username_from_url, _allowed_types| {
                Cred::userpass_plaintext(username_from_url.unwrap_or("x-access-token"), &token)
            });
            let mut fetch_options = FetchOptions::new();
            fetch_options.remote_callbacks(callbacks);
            let mut builder = RepoBuilder::new();
            builder.fetch_options(fetch_options);
            builder
                .clone(source.as_ref(), destination)
                .map_err(|error| git_error("could not clone repository", error))?
        } else {
            Repository::clone(source.as_ref(), destination)
                .map_err(|error| git_error("could not clone repository", error))?
        };
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
    pub fn create_mirror(
        source: impl AsRef<Path>,
        mirror: impl AsRef<Path>,
        origin_url: &str,
    ) -> Result<()> {
        let mirror = mirror.as_ref();
        if mirror.exists() {
            return Ok(());
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
        set_origin_url(&repository, origin_url)
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
        render_git_diff(diff, normalized, staged, None)
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

    /// Pushes a local branch to the `origin` remote using the supplied access
    /// token.
    ///
    /// The branch must already exist and point at a commit. The push is a
    /// fast-forward-only update; the remote rejects non-fast-forward updates.
    /// The token is used only for this push and is never written to the
    /// repository configuration.
    pub fn push_branch_authenticated(&self, branch: &str, token: &str) -> Result<()> {
        let refname = format!("refs/heads/{branch}");
        if !git2::Reference::is_valid_name(&refname) {
            return Err(LoomError::invalid_request(format!(
                "'{branch}' is not a valid Git branch name"
            )));
        }
        let repository = self.repository()?;
        let branch_reference = repository
            .find_branch(branch, BranchType::Local)
            .map_err(|error| {
                if error.code() == git2::ErrorCode::NotFound {
                    LoomError::not_found("local Git branch", branch)
                } else {
                    git_error("could not look up local Git branch", error)
                }
            })?
            .into_reference();
        if branch_reference.target().is_none() {
            return Err(LoomError::invalid_state(format!(
                "local Git branch '{branch}' does not point to a commit"
            )));
        }
        let mut remote = repository.find_remote("origin").map_err(|error| {
            if error.code() == git2::ErrorCode::NotFound {
                LoomError::not_found("Git remote", "origin")
            } else {
                git_error("could not look up the 'origin' Git remote", error)
            }
        })?;
        let token = token.to_owned();
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |_url, username_from_url, _allowed_types| {
            Cred::userpass_plaintext(username_from_url.unwrap_or("x-access-token"), &token)
        });
        callbacks.push_update_reference(|reference, status| match status {
            Some(status) => Err(git2::Error::from_str(&format!(
                "remote rejected updating {reference}: {status}"
            ))),
            None => Ok(()),
        });
        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);
        let refspec = format!("{refname}:{refname}");
        remote
            .push(&[refspec.as_str()], Some(&mut push_options))
            .map_err(|error| git_error("could not push Git branch", error))
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

        let mut status_options = StatusOptions::new();
        status_options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(true)
            .recurse_ignored_dirs(true)
            .include_unreadable(true)
            .include_unreadable_as_untracked(true);
        let statuses = repository
            .statuses(Some(&mut status_options))
            .map_err(|error| git_error("could not inspect parent Git checkout", error))?;
        if !statuses.is_empty() {
            return Err(LoomError::conflict(
                "cannot fast-forward a parent Git checkout with staged, modified, untracked, ignored, or unreadable files",
            ));
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

        let mut status_options = StatusOptions::new();
        status_options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(true)
            .recurse_ignored_dirs(true)
            .include_unreadable(true)
            .include_unreadable_as_untracked(true);
        let statuses = repository
            .statuses(Some(&mut status_options))
            .map_err(|error| git_error("could not inspect parent Git checkout", error))?;
        if !statuses.is_empty() {
            return Err(LoomError::conflict(
                "cannot integrate into a parent Git checkout with staged, modified, untracked, ignored, or unreadable files",
            ));
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
            let conflicts = index.conflicts().map_err(|error| {
                git_error("could not inspect project child merge conflicts", error)
            })?;
            let mut paths = Vec::new();
            for conflict in conflicts {
                let conflict = conflict.map_err(|error| {
                    git_error("could not read project child merge conflict", error)
                })?;
                if let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) {
                    paths.push(String::from_utf8_lossy(&entry.path).replace('\\', "/"));
                }
            }
            paths.sort();
            paths.dedup();
            return Ok(MergeIntegrationOutcome::Conflicted(paths));
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
    let print_result = diff.print(DiffFormat::Patch, |_delta, hunk, line| {
        let new_hunk = hunk.and_then(|hunk| {
            let is_new = hunks.last().is_none_or(|last| {
                last.old_start != hunk.old_start() || last.new_start != hunk.new_start()
            });
            is_new.then(|| GitDiffHunk {
                old_start: hunk.old_start(),
                old_lines: hunk.old_lines(),
                new_start: hunk.new_start(),
                new_lines: hunk.new_lines(),
                lines: Vec::new(),
            })
        });
        let origin = line.origin();
        let contents = line.content();
        let has_prefix = matches!(origin, ' ' | '+' | '-');
        let output_line_len = contents.len() + usize::from(has_prefix);
        if max_bytes.is_some_and(|limit| output_line_len > limit.saturating_sub(patch.len())) {
            truncated = true;
            return false;
        }
        if let Some(hunk) = new_hunk {
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
            }
        }
        patch.extend_from_slice(contents);
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

fn git_error(operation: &str, error: git2::Error) -> LoomError {
    LoomError::new(ErrorCode::Vcs, format!("{operation}: {error}"), false)
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

    #[test]
    fn pushing_a_branch_updates_the_origin_remote() {
        let (git, root) = repository();
        let bare =
            std::env::temp_dir().join(format!("loom-git-bare-{}", loom_core::RepositoryId::new()));
        Repository::init_bare(&bare).unwrap();
        run(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        let branch = git.current_branch().unwrap().unwrap();
        git.push_branch_authenticated(&branch, "test-token")
            .unwrap();
        let pushed = Repository::open_bare(&bare).unwrap();
        assert!(
            pushed
                .find_reference(&format!("refs/heads/{branch}"))
                .is_ok()
        );
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
            git.push_branch_authenticated("bad name", "test-token")
                .is_err()
        );
        assert!(
            git.push_branch_authenticated("does-not-exist", "test-token")
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bare).unwrap();
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

    fn write_commit(root: &Path, file: &str, content: &str, message: &str) {
        fs::write(root.join(file), content).unwrap();
        run(root, &["add", "--", file]);
        run(root, &["commit", "-qm", message]);
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
