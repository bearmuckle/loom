use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use loom_core::{ErrorCode, LoomError, Result, Timestamp};
pub use loom_protocol::{
    GitBranch, GitDiff, GitFileStatus, GitFileStatusKind, GitRepositoryStatus,
};

#[derive(Clone, Debug)]
pub struct GitService {
    root: PathBuf,
}

impl GitService {
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
        let output = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .output()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Vcs,
                    format!("could not initialize Git: {error}"),
                    true,
                )
            })?;
        if !output.status.success() {
            return Err(LoomError::new(
                ErrorCode::Vcs,
                format!(
                    "could not initialize Git: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                false,
            ));
        }
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
        let service = Self { root };
        service.run(&["rev-parse", "--show-toplevel"])?;
        Ok(service)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn status(&self) -> Result<GitRepositoryStatus> {
        let output = self.run_bytes(&["status", "--porcelain=v1", "-z", "--branch"])?;
        let records = output
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty());
        let mut branch = None;
        let mut files = Vec::new();
        for record in records {
            let record = String::from_utf8(record.to_vec()).map_err(|error| {
                LoomError::new(
                    ErrorCode::Vcs,
                    format!("Git status contained invalid UTF-8: {error}"),
                    false,
                )
            })?;
            if let Some(header) = record.strip_prefix("## ") {
                branch = Some(
                    header
                        .split("...")
                        .next()
                        .unwrap_or(header)
                        .trim_end_matches(" (no branch)")
                        .to_owned(),
                );
                continue;
            }
            if record.len() < 3 {
                continue;
            }
            let bytes = record.as_bytes();
            let index = status_kind(bytes[0]);
            let worktree = status_kind(bytes[1]);
            let raw_path = record[3..].to_owned();
            let original_path = raw_path
                .split_once(" -> ")
                .map(|(original, _)| original.to_owned());
            let path = raw_path
                .split_once(" -> ")
                .map_or(raw_path.clone(), |(_, current)| current.to_owned());
            let conflicted = is_conflict(bytes[0], bytes[1]);
            files.push(GitFileStatus {
                path,
                original_path,
                index,
                worktree,
                conflicted,
            });
        }
        let head = self
            .run(&["rev-parse", "HEAD"])
            .ok()
            .map(|head| head.trim().to_owned())
            .filter(|head| !head.is_empty());
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
        let mut arguments = vec!["diff"];
        if staged {
            arguments.push("--cached");
        }
        let normalized = path.map(|path| self.validate_path(path)).transpose()?;
        if let Some(path) = normalized.as_deref() {
            arguments.extend(["--", path]);
        }
        let patch = self.run(&arguments)?;
        Ok(GitDiff {
            path: normalized,
            staged,
            binary: patch.contains("Binary files"),
            patch,
        })
    }

    pub fn branches(&self) -> Result<Vec<GitBranch>> {
        let current = self.run(&["branch", "--show-current"])?.trim().to_owned();
        let output = self.run(&[
            "for-each-ref",
            "--format=%(refname:short)\t%(upstream:short)",
            "refs/heads",
        ])?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let (name, upstream) = line.split_once('\t').unwrap_or((line, ""));
                (!name.is_empty()).then(|| GitBranch {
                    name: name.to_owned(),
                    current: name == current,
                    upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
                })
            })
            .collect())
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        let branch = self.run(&["branch", "--show-current"])?;
        Ok((!branch.trim().is_empty()).then(|| branch.trim().to_owned()))
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

    fn run(&self, arguments: &[&str]) -> Result<String> {
        let bytes = self.run_bytes(arguments)?;
        String::from_utf8(bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::Vcs,
                format!("Git command returned invalid UTF-8: {error}"),
                false,
            )
        })
    }

    fn run_bytes(&self, arguments: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(&self.root)
            .output()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Vcs,
                    format!("could not execute Git: {error}"),
                    true,
                )
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(LoomError::new(
                ErrorCode::Vcs,
                if detail.is_empty() {
                    format!("Git command failed with status {}", output.status)
                } else {
                    format!("Git command failed: {detail}")
                },
                false,
            ));
        }
        Ok(output.stdout)
    }
}

fn status_kind(value: u8) -> GitFileStatusKind {
    match value {
        b'A' => GitFileStatusKind::Added,
        b'M' => GitFileStatusKind::Modified,
        b'D' => GitFileStatusKind::Deleted,
        b'R' => GitFileStatusKind::Renamed,
        b'C' => GitFileStatusKind::Copied,
        b'?' => GitFileStatusKind::Untracked,
        b'!' => GitFileStatusKind::Ignored,
        b'U' => GitFileStatusKind::Conflicted,
        b' ' => GitFileStatusKind::Unknown,
        _ => GitFileStatusKind::Unknown,
    }
}

fn is_conflict(index: u8, worktree: u8) -> bool {
    index == b'U' || worktree == b'U' || matches!((index, worktree), (b'A', b'A') | (b'D', b'D'))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::ProjectId;

    use super::*;

    fn repository() -> (GitService, PathBuf) {
        let root = std::env::temp_dir().join(format!("loom-git-{}", ProjectId::new()));
        fs::create_dir_all(&root).unwrap();
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
                .args(arguments)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn status_and_diff_are_structured() {
        let (git, root) = repository();
        fs::write(root.join("README.md"), "after\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        let status = git.status().unwrap();
        assert!(!status.clean);
        assert!(status.files.iter().any(|file| file.path == "README.md"));
        let diff = git.diff(Some("README.md"), false).unwrap();
        assert!(diff.patch.contains("-before"));
        assert!(diff.patch.contains("+after"));
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
