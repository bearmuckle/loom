//! Persistence tests: writer.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn exclusive_writer_ownership_is_shared_by_clones_and_released_on_drop() {
    let _guard = EXCLUSIVE_WRITER_TEST_LOCK.lock().unwrap();
    let test_dir = std::env::temp_dir().join(format!(
        "loom-persistence-owner-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::create_dir(&test_dir).unwrap();
    let path = test_dir.join("state.db");
    let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
    let clone = writer.clone();
    assert_eq!(
        FilePersistence::open_exclusive_writer(&path)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    // Diagnostic handles do not claim backend ownership.
    assert!(FilePersistence::open(&path).is_ok());
    drop(writer);
    assert_eq!(
        FilePersistence::open_exclusive_writer(&path)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    // The shared lock remains owned until the final clone is dropped.
    drop(clone);
    let replacement = FilePersistence::open_exclusive_writer(&path).unwrap();
    drop(replacement);
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".loom-owner.lock");
    fs::remove_file(PathBuf::from(lock_path)).unwrap();
    fs::remove_dir(test_dir).unwrap();
}

#[test]
fn exclusive_writer_can_be_released_explicitly_through_a_clone() {
    let _guard = EXCLUSIVE_WRITER_TEST_LOCK.lock().unwrap();
    let test_dir = std::env::temp_dir().join(format!(
        "loom-persistence-owner-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::create_dir(&test_dir).unwrap();
    let path = test_dir.join("state.db");
    let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
    let clone = writer.clone();
    drop(writer);
    assert_eq!(
        FilePersistence::open_exclusive_writer(&path)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    clone.release_exclusive_writer().unwrap();
    let replacement = FilePersistence::open_exclusive_writer(&path).unwrap();
    drop(replacement);
    drop(clone);
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".loom-owner.lock");
    fs::remove_file(PathBuf::from(lock_path)).unwrap();
    fs::remove_dir(test_dir).unwrap();
}

#[test]
fn exclusive_writer_subprocess_probe() {
    let Ok(path) = std::env::var("LOOM_TEST_OWNER_PROBE_PATH") else {
        return;
    };
    let should_be_owned = std::env::var("LOOM_TEST_OWNER_EXPECTED")
        .unwrap()
        .parse::<bool>()
        .unwrap();
    match FilePersistence::open_exclusive_writer(path) {
        Ok(writer) => {
            assert!(!should_be_owned, "another process should own this database");
            drop(writer);
        }
        Err(error) => {
            assert!(
                should_be_owned,
                "unexpected exclusive writer error: {error}"
            );
            assert_eq!(error.code, ErrorCode::Conflict);
        }
    }
}

#[test]
fn exclusive_writer_lock_is_enforced_across_processes() {
    let _guard = EXCLUSIVE_WRITER_TEST_LOCK.lock().unwrap();
    let test_dir = std::env::temp_dir().join(format!(
        "loom-persistence-owner-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::create_dir(&test_dir).unwrap();
    let path = test_dir.join("state.db");
    let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
    run_exclusive_writer_probe(&path, true);
    drop(writer);
    run_exclusive_writer_probe(&path, false);
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".loom-owner.lock");
    fs::remove_file(PathBuf::from(lock_path)).unwrap();
    fs::remove_dir(test_dir).unwrap();
}
