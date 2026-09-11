use loom_core::Result;

use crate::{ContextFileKind, ContextFileReference, Workspace, WorkspaceEntryKind};

/// Upper bound for a single context file. Larger files are skipped instead of
/// being pulled into an agent prompt.
pub const MAX_CONTEXT_FILE_BYTES: u64 = 2 * 1024 * 1024;

const INSTRUCTION_FILE_NAMES: [&str; 5] = [
    "AGENTS.md",
    "CLAUDE.md",
    "LOOM.md",
    "CONTRIBUTING.md",
    ".github/copilot-instructions.md",
];

impl Workspace {
    /// Repository instruction and context files that an agent run may load.
    pub fn context_files(&self) -> Result<Vec<ContextFileReference>> {
        let snapshot = self.snapshot()?;
        let mut files = Vec::new();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| entry.kind == WorkspaceEntryKind::File)
        {
            let normalized = entry.path.trim_start_matches("./");
            let name = normalized.rsplit('/').next().unwrap_or(normalized);
            let classified = if INSTRUCTION_FILE_NAMES
                .iter()
                .any(|candidate| normalized == *candidate || name == *candidate)
            {
                Some((
                    ContextFileKind::RepositoryInstructions,
                    "repository instructions",
                ))
            } else if normalized == "README.md"
                || normalized.starts_with("docs/")
                || normalized.starts_with(".github/")
            {
                Some((ContextFileKind::ContextReference, "repository context"))
            } else {
                None
            };
            let Some((kind, reason)) = classified else {
                continue;
            };
            let bytes = self.read_file_bytes(&entry.path)?;
            if bytes.bytes.len() as u64 > MAX_CONTEXT_FILE_BYTES {
                continue;
            }
            let file = self.read_file(&entry.path)?;
            files.push(ContextFileReference {
                path: entry.path.clone(),
                kind,
                content: file.content,
                reason: reason.to_owned(),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    /// Concatenated repository instructions used as agent run context.
    pub fn instruction_text(&self) -> Result<String> {
        Ok(self
            .context_files()?
            .into_iter()
            .filter(|file| file.kind == ContextFileKind::RepositoryInstructions)
            .map(|file| format!("## {}\n{}", file.path, file.content))
            .collect::<Vec<_>>()
            .join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use loom_core::ProjectId;

    use super::*;

    #[test]
    fn repository_instructions_are_loaded_from_the_workspace() {
        let root = std::env::temp_dir().join(format!("loom-instructions-{}", ProjectId::new()));
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("AGENTS.md"), "Follow the house style.\n").unwrap();
        fs::write(root.join("README.md"), "Loom\n").unwrap();
        fs::write(root.join("docs/architecture.md"), "Layers\n").unwrap();
        fs::write(root.join("src.rs"), "fn main() {}\n").unwrap();
        let workspace = Workspace::open(ProjectId::new(), &root).unwrap();

        let files = workspace.context_files().unwrap();
        assert!(
            files.iter().any(|file| file.path == "AGENTS.md"
                && file.kind == ContextFileKind::RepositoryInstructions)
        );
        assert!(
            files
                .iter()
                .any(|file| file.path == "README.md"
                    && file.kind == ContextFileKind::ContextReference)
        );
        assert!(!files.iter().any(|file| file.path == "src.rs"));

        let instructions = workspace.instruction_text().unwrap();
        assert!(instructions.contains("## AGENTS.md"));
        assert!(instructions.contains("Follow the house style."));
        assert!(!instructions.contains("Loom\n"));
        fs::remove_dir_all(root).unwrap();
    }
}
