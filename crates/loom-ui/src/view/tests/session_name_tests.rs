use super::{SessionCreationSource, session_name_for_path, session_name_for_source};
use loom_protocol::GitHubRepository;
use std::path::Path;

#[test]
fn local_source_uses_its_folder_name() {
    let source = SessionCreationSource::LocalDirectory("/home/user/work/my-project/".to_owned());
    assert_eq!(session_name_for_source(&source), "my-project");
    assert_eq!(
        session_name_for_path(Path::new("/home/user/work/my-project")),
        Some("my-project".to_owned())
    );
}

#[test]
fn github_source_uses_repository_name_without_owner() {
    let source = SessionCreationSource::GitHub(GitHubRepository {
        full_name: "bearmuckle/loom".to_owned(),
        description: None,
        clone_url: "https://github.com/bearmuckle/loom.git".to_owned(),
        private: false,
        default_branch: "main".to_owned(),
    });
    assert_eq!(session_name_for_source(&source), "loom");
}

#[test]
fn sources_without_a_usable_name_receive_a_safe_fallback() {
    assert_eq!(
        session_name_for_source(&SessionCreationSource::LocalDirectory("/".to_owned())),
        "New session"
    );
    assert_eq!(session_name_for_path(Path::new("/")), None);
    assert_eq!(
        session_name_for_source(&SessionCreationSource::GitHub(GitHubRepository {
            full_name: "owner/ ".to_owned(),
            description: None,
            clone_url: "https://github.com/owner/repo.git".to_owned(),
            private: false,
            default_branch: "main".to_owned(),
        })),
        "New session"
    );
}
