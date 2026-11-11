use crate::Timestamp;
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

/// How one path differs between a base revision and a checkout's working
/// tree, as a review summary reports it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRevisionChangeKind {
    New,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

/// One path of a reviewed change, without its patch body.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitRevisionFileStat {
    pub path: String,
    /// The path the file had before a rename or copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    pub change: GitRevisionChangeKind,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default)]
    pub binary: bool,
}

/// The rendered patch of one path of a reviewed change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitRevisionFileDiff {
    pub path: String,
    /// The patch text, or a one-line notice for a binary file. A patch this
    /// render cut short ends with a line saying so.
    pub patch: String,
    #[serde(default)]
    pub binary: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
}

/// A bounded patch of a reviewed change.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitRevisionDiff {
    /// The paths whose patch is included, in the order the render visited them.
    pub files: Vec<GitRevisionFileDiff>,
    /// Paths whose patch was cut short by the byte budget.
    pub truncated_paths: Vec<String>,
    /// Paths the render did not include at all because the budget was spent.
    pub omitted_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitBranch {
    pub name: String,
    pub current: bool,
    pub upstream: Option<String>,
}
