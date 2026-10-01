use super::resolve_local_source_path;
use std::path::Path;

#[test]
fn explicit_absolute_entry_wins_over_the_current_directory() {
    assert_eq!(
        resolve_local_source_path("/typed/project", Some(Path::new("/current/project"))),
        Some("/typed/project".to_owned())
    );
}

#[test]
fn empty_entry_falls_back_to_the_current_directory() {
    assert_eq!(
        resolve_local_source_path("   ", Some(Path::new("/current/project"))),
        Some("/current/project".to_owned())
    );
}

#[test]
fn relative_entries_are_rejected() {
    assert_eq!(
        resolve_local_source_path("relative/project", Some(Path::new("/current/project"))),
        None
    );
}

#[test]
fn empty_entry_without_a_current_directory_is_rejected() {
    assert_eq!(resolve_local_source_path("", None), None);
}
