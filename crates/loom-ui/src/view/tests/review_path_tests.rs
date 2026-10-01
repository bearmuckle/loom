use super::belongs_to_repository;
use loom_core::{RepositoryId, Timestamp};
use loom_protocol::SessionRepository;

#[test]
fn mounted_repository_activity_does_not_appear_as_other_workspace_files() {
    let repositories = ["repositories/first-id", "sources/local/second-id"]
        .into_iter()
        .map(|path| SessionRepository {
            id: RepositoryId::new(),
            source: "/code/project".to_owned(),
            path: path.to_owned(),
            revision: None,
            attached_at: Timestamp::from_unix_millis(0),
        })
        .collect::<Vec<_>>();
    assert!(belongs_to_repository(
        "repositories/first-id/src/lib.rs",
        &repositories
    ));
    assert!(belongs_to_repository(
        "sources/local/second-id/README.md",
        &repositories
    ));
    assert!(!belongs_to_repository(
        "repositories/first-id-extra/file",
        &repositories
    ));
    assert!(!belongs_to_repository("notes/todo.md", &repositories));
}
