//! In-process tests: util.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn non_sqlite_persistence_file_is_rejected_without_fallback() {
    let path =
        std::env::temp_dir().join(format!("loom-server-malformed-{}.db", WorkspaceId::new()));
    fs::write(&path, br#"{"schema_version":1,"state":{"broken":true}}"#).unwrap();
    let error = match InProcessBackend::new_persistent(&path) {
        Ok(_) => panic!("non-SQLite persistence unexpectedly loaded"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::Persistence);
    assert!(!path.with_extension("json.legacy").exists());
    fs::remove_file(path).unwrap();
}
