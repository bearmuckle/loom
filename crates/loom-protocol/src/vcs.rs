use loom_core::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStatusKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
    Conflicted,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitFileStatus {
    pub path: String,
    pub original_path: Option<String>,
    pub index: GitFileStatusKind,
    pub worktree: GitFileStatusKind,
    pub conflicted: bool,
    #[serde(default)]
    pub index_additions: u32,
    #[serde(default)]
    pub index_deletions: u32,
    #[serde(default)]
    pub worktree_additions: u32,
    #[serde(default)]
    pub worktree_deletions: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitRepositoryStatus {
    pub root: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub files: Vec<GitFileStatus>,
    pub conflicts: Vec<String>,
    pub clean: bool,
    pub captured_at: Timestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitDiff {
    pub path: Option<String>,
    pub staged: bool,
    pub patch: String,
    pub binary: bool,
    #[serde(default)]
    pub hunks: Vec<GitDiffHunk>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitDiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<GitDiffLine>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitDiffLine {
    pub kind: GitDiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub content: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitBranch {
    pub name: String,
    pub current: bool,
    pub upstream: Option<String>,
}
