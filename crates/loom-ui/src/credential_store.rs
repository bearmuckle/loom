//! The GitHub sign-in credential a client keeps for each worker.
//!
//! A GitHub sign-in binds a person to one connection, but the worker can mint
//! that person a revocable credential of their own and hand it to the client
//! once, on the poll that reports the sign-in. Presenting it on a later
//! connection authenticates the same person, so a reload or reconnect does not
//! ask for a device flow again.
//!
//! The value is a bearer secret, so it is stored apart from the remembered
//! identity in [`crate::identity_store`]: natively in its own owner-only file,
//! and only ever replayed over a transport that is secure for secrets.
//!
//! Entries are keyed by [`crate::identity_store::worker_key`], never by a
//! worker's reported node id: that id names one worker process and the server
//! generates a new one on every start, so a store keyed by it could not be read
//! again after a restart.

use crate::connection::remote_url_is_secure_for_secrets;

/// The stored credential a connection at `url` may present, if any.
///
/// An operator token always decides a connection, so a person credential is
/// only ever a replacement for a missing one. It is also a bearer secret, so it
/// is offered only over a transport that is secure for secrets: anything else
/// would put it on the wire in the clear.
pub(crate) fn replayable(
    operator_token: &str,
    stored: Option<String>,
    url: &str,
) -> Option<String> {
    if !operator_token.trim().is_empty() || !remote_url_is_secure_for_secrets(url) {
        return None;
    }
    stored.filter(|credential| !credential.trim().is_empty())
}

/// Reads, saves, and clears can arrive from the UI thread and a reconnect
/// thread at once; one lock keeps the read-modify-write whole.
#[cfg(all(not(target_family = "wasm"), not(test)))]
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(not(target_family = "wasm"), not(test)))]
fn lock_store() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn load(worker_key: &str) -> Option<String> {
    let _guard = lock_store();
    let path = native_path();
    match read_credentials(&path) {
        Ok(credentials) => credentials.get(worker_key).cloned(),
        Err(error) => {
            log::warn!(
                "[loom-ui] could not read kept worker sign-ins from '{}': {}",
                path.display(),
                error
            );
            None
        }
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn save(worker_key: &str, credential: &str) {
    let _guard = lock_store();
    let path = native_path();
    let mut credentials = read_credentials(&path).unwrap_or_default();
    if !insert(&mut credentials, worker_key, credential) {
        return;
    }
    if let Err(error) = write_credentials(&path, &credentials) {
        log::warn!(
            "[loom-ui] could not keep the GitHub sign-in for worker '{}': {}",
            worker_key,
            error
        );
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn clear(worker_key: &str) {
    let _guard = lock_store();
    let path = native_path();
    let mut credentials = read_credentials(&path).unwrap_or_default();
    if !remove(&mut credentials, worker_key) {
        return;
    }
    if let Err(error) = write_credentials(&path, &credentials) {
        log::warn!(
            "[loom-ui] could not forget the GitHub sign-in for worker '{}': {}",
            worker_key,
            error
        );
    }
}

/// The file the kept sign-ins live in.
///
/// It is deliberately not the identity store's file: these values are secrets,
/// so they get their own file whose permissions no other client state shares.
#[cfg(not(target_family = "wasm"))]
fn native_path() -> std::path::PathBuf {
    loom_core::config_dir().join("worker-credentials.json")
}

#[cfg(not(target_family = "wasm"))]
type Credentials = std::collections::BTreeMap<String, String>;

/// Inserts `credential`, reporting whether the store changed.
#[cfg(not(target_family = "wasm"))]
fn insert(credentials: &mut Credentials, worker_key: &str, credential: &str) -> bool {
    if credentials
        .get(worker_key)
        .is_some_and(|stored| stored == credential)
    {
        return false;
    }
    credentials.insert(worker_key.to_owned(), credential.to_owned());
    true
}

/// Removes the entry for `worker_key`, reporting whether the store changed.
#[cfg(not(target_family = "wasm"))]
fn remove(credentials: &mut Credentials, worker_key: &str) -> bool {
    credentials.remove(worker_key).is_some()
}

#[cfg(not(target_family = "wasm"))]
fn read_credentials(path: &std::path::Path) -> std::result::Result<Credentials, String> {
    if !path.is_file() {
        return Ok(Credentials::new());
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

#[cfg(not(target_family = "wasm"))]
fn write_credentials(
    path: &std::path::Path,
    credentials: &Credentials,
) -> std::result::Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "credential store path must have a parent directory".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(credentials).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    write_private(&temporary, &bytes)?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    // The temporary file was created owner-only and the rename keeps its mode,
    // but setting it again also covers a file an older version left readable.
    restrict_to_owner(path)
}

#[cfg(not(target_family = "wasm"))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::result::Result<(), String> {
    use std::io::Write as _;
    let mut file = open_private(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())
}

#[cfg(all(not(target_family = "wasm"), unix))]
fn open_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(all(not(target_family = "wasm"), not(unix)))]
fn open_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(all(not(target_family = "wasm"), unix))]
fn restrict_to_owner(path: &std::path::Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())
}

#[cfg(all(not(target_family = "wasm"), not(unix)))]
fn restrict_to_owner(_path: &std::path::Path) -> std::result::Result<(), String> {
    Ok(())
}

#[cfg(target_family = "wasm")]
fn storage_key(worker_key: &str) -> String {
    format!("loom.worker_credential.{worker_key}")
}

#[cfg(target_family = "wasm")]
pub(crate) fn load(worker_key: &str) -> Option<String> {
    let storage = crate::browser::browser_storage().ok()?;
    let stored = storage.get_item(&storage_key(worker_key)).ok().flatten()?;
    (!stored.trim().is_empty()).then_some(stored)
}

#[cfg(target_family = "wasm")]
pub(crate) fn save(worker_key: &str, credential: &str) {
    let Ok(storage) = crate::browser::browser_storage() else {
        return;
    };
    match storage.get_item(&storage_key(worker_key)) {
        Ok(Some(stored)) if stored == credential => return,
        _ => {}
    }
    if let Err(error) = storage.set_item(&storage_key(worker_key), credential) {
        log::warn!(
            "[loom-ui] could not keep the GitHub sign-in for worker '{}': {:?}",
            worker_key,
            error
        );
    }
}

#[cfg(target_family = "wasm")]
pub(crate) fn clear(worker_key: &str) {
    let Ok(storage) = crate::browser::browser_storage() else {
        return;
    };
    // Nothing stored means nothing to forget, which matches the native store's
    // no-op for an absent entry.
    if matches!(storage.get_item(&storage_key(worker_key)), Ok(None)) {
        return;
    }
    if let Err(error) = storage.remove_item(&storage_key(worker_key)) {
        log::warn!(
            "[loom-ui] could not forget the GitHub sign-in for worker '{}': {:?}",
            worker_key,
            error
        );
    }
}

/// Tests never touch the developer's real config directory. The stub keeps the
/// entries in memory instead, so a test can see what a sign-in kept without a
/// file, while the file helpers above are exercised with temporary paths.
#[cfg(test)]
static TEST_STORE: std::sync::Mutex<Credentials> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
fn test_store() -> std::sync::MutexGuard<'static, Credentials> {
    TEST_STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn load(worker_key: &str) -> Option<String> {
    test_store().get(worker_key).cloned()
}

#[cfg(test)]
pub(crate) fn save(worker_key: &str, credential: &str) {
    insert(&mut test_store(), worker_key, credential);
}

#[cfg(test)]
pub(crate) fn clear(worker_key: &str) {
    remove(&mut test_store(), worker_key);
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use loom_core::ParticipantId;

    fn temporary_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "loom-worker-credentials-{}.json",
            ParticipantId::new()
        ))
    }

    #[test]
    fn credentials_round_trip_per_worker_and_can_be_forgotten() {
        let path = temporary_path();
        let mut credentials = Credentials::new();
        credentials.insert("worker-a".to_owned(), "secret-a".to_owned());
        credentials.insert("worker-b".to_owned(), "secret-b".to_owned());
        write_credentials(&path, &credentials).unwrap();

        let loaded = read_credentials(&path).unwrap();
        assert_eq!(loaded.get("worker-a").map(String::as_str), Some("secret-a"));
        assert_eq!(loaded.get("worker-b").map(String::as_str), Some("secret-b"));

        let mut credentials = loaded;
        assert!(remove(&mut credentials, "worker-a"));
        write_credentials(&path, &credentials).unwrap();
        let loaded = read_credentials(&path).unwrap();
        assert!(!loaded.contains_key("worker-a"));
        assert_eq!(loaded.get("worker-b").map(String::as_str), Some("secret-b"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn saving_and_clearing_are_idempotent() {
        let mut credentials = Credentials::new();
        assert!(insert(&mut credentials, "worker-a", "secret-a"));
        assert!(
            !insert(&mut credentials, "worker-a", "secret-a"),
            "saving the same credential again changes nothing"
        );
        assert!(
            insert(&mut credentials, "worker-a", "secret-b"),
            "a new credential replaces the old one"
        );
        assert_eq!(
            credentials.get("worker-a").map(String::as_str),
            Some("secret-b")
        );
        assert!(remove(&mut credentials, "worker-a"));
        assert!(
            !remove(&mut credentials, "worker-a"),
            "clearing an absent entry changes nothing"
        );
        assert!(credentials.is_empty());
    }

    #[test]
    fn the_credential_file_is_not_the_identity_file() {
        assert_eq!(
            native_path().file_name().map(|name| name.to_owned()),
            Some(std::ffi::OsString::from("worker-credentials.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_credential_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_path();
        let mut credentials = Credentials::new();
        credentials.insert("worker-a".to_owned(), "secret-a".to_owned());
        write_credentials(&path, &credentials).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the credential file is a bearer secret");

        // A rewrite replaces the file with the temporary one and keeps the mode.
        assert!(remove(&mut credentials, "worker-a"));
        write_credentials(&path, &credentials).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_credential_is_replayed_only_over_a_transport_that_is_secure_for_secrets() {
        let stored = Some("person-credential".to_owned());
        assert_eq!(
            replayable("", stored.clone(), "ws://127.0.0.1:8765/ws"),
            stored
        );
        assert_eq!(
            replayable("", stored.clone(), "ws://worker.example/ws"),
            None,
            "a plaintext connection would put the credential on the wire"
        );
        assert_eq!(
            replayable("operator-token", stored.clone(), "ws://127.0.0.1:8765/ws"),
            None,
            "an operator token always decides the connection"
        );
        assert_eq!(
            replayable("", None, "ws://127.0.0.1:8765/ws"),
            None,
            "nothing is replayed when no sign-in was kept"
        );
        assert_eq!(
            replayable("", Some(String::new()), "ws://127.0.0.1:8765/ws"),
            None
        );
    }
}
