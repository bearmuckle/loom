use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use git2::{
    BranchType, Cred, DiffFormat, DiffOptions, FetchOptions, RemoteCallbacks, Repository, Status,
    StatusOptions, build::RepoBuilder,
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
        Self::open(destination)
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
        let mut patch = Vec::new();
        let mut hunks: Vec<GitDiffHunk> = Vec::new();
        diff.print(DiffFormat::Patch, |_delta, hunk, line| {
            if let Some(hunk) = hunk {
                let new_hunk = hunks.last().is_none_or(|last| {
                    last.old_start != hunk.old_start() || last.new_start != hunk.new_start()
                });
                if new_hunk {
                    hunks.push(GitDiffHunk {
                        old_start: hunk.old_start(),
                        old_lines: hunk.old_lines(),
                        new_start: hunk.new_start(),
                        new_lines: hunk.new_lines(),
                        lines: Vec::new(),
                    });
                }
            }
            if matches!(line.origin(), ' ' | '+' | '-') {
                patch.push(line.origin() as u8);
                if let Some(hunk) = hunks.last_mut() {
                    let kind = match line.origin() {
                        '+' => GitDiffLineKind::Added,
                        '-' => GitDiffLineKind::Removed,
                        _ => GitDiffLineKind::Context,
                    };
                    hunk.lines.push(GitDiffLine {
                        kind,
                        old_line: line.old_lineno(),
                        new_line: line.new_lineno(),
                        content: String::from_utf8_lossy(line.content())
                            .trim_end_matches('\n')
                            .trim_end_matches('\r')
                            .to_owned(),
                    });
                }
            }
            patch.extend_from_slice(line.content());
            true
        })
        .map_err(|error| git_error("could not render Git diff", error))?;
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
            path: normalized,
            staged,
            binary,
            patch,
            hunks,
            truncated: false,
        })
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

    fn repository(&self) -> Result<Repository> {
        Repository::open(&self.root)
            .map_err(|error| git_error("could not open Git repository", error))
    }
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

fn git_error(operation: &str, error: git2::Error) -> LoomError {
    LoomError::new(ErrorCode::Vcs, format!("{operation}: {error}"), false)
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
}
