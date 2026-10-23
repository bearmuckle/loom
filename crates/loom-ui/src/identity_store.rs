//! The GitHub participant a client last signed in as on each worker.
//!
//! A GitHub sign-in binds a person to one connection, not to the worker token
//! that reopened it. Remembering the participant id locally lets a reload
//! resume the same person on an operator connection, so the roster shows them
//! online instead of leaving them behind as an offline grant. The id is not a
//! secret: the worker only resumes it for an operator token or the person's own
//! credential (see [`crate::credential_store`]), and only when the person is
//! still in the worker's directory.
//!
//! Entries are keyed by [`worker_key`], never by a worker's reported node id:
//! that id names one worker process and the server generates a new one on every
//! start, so a store keyed by it could not be read again after a restart.

use loom_core::ParticipantIdentity;

/// The durable key of the worker a client reached at `endpoint`.
///
/// [`loom_protocol::WorkerNodeStatus::node_id`] names one worker *process*, so
/// it cannot key anything that has to outlive a restart. A worker is instead
/// remembered by what a restart keeps: the address a remote worker is reached at
/// (`Some`), or the state database the one managed local worker owns (`None`).
/// The address has to stay the same across restarts for the worker to keep its
/// identity, which the stored access token already assumes.
pub(crate) fn worker_key(endpoint: Option<&str>) -> String {
    endpoint.map_or_else(local_worker_key, |url| {
        // A trailing slash or surrounding whitespace is the same endpoint.
        format!("remote:{}", url.trim().trim_end_matches('/'))
    })
}

#[cfg(not(target_family = "wasm"))]
fn local_worker_key() -> String {
    // Every managed worker a client starts keeps its state in this database, so
    // the file names that worker across any number of process restarts.
    format!("local:{}", loom_local::backend_persistence_path().display())
}

#[cfg(target_family = "wasm")]
fn local_worker_key() -> String {
    // The browser client reaches only remote workers; the fallback keeps the key
    // function total for a caller that has no endpoint.
    "local".to_owned()
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
pub(crate) fn load(worker_key: &str) -> Option<ParticipantIdentity> {
    let _guard = lock_store();
    let path = native_path();
    match read_identities(&path) {
        Ok(identities) => identities.get(worker_key).cloned(),
        Err(error) => {
            log::warn!(
                "[loom-ui] could not read remembered worker identities from '{}': {}",
                path.display(),
                error
            );
            None
        }
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn save(worker_key: &str, identity: &ParticipantIdentity) {
    let _guard = lock_store();
    let path = native_path();
    let mut identities = read_identities(&path).unwrap_or_default();
    if identities.get(worker_key) == Some(identity) {
        return;
    }
    identities.insert(worker_key.to_owned(), identity.clone());
    if let Err(error) = write_identities(&path, &identities) {
        log::warn!(
            "[loom-ui] could not remember the GitHub identity for worker '{}': {}",
            worker_key,
            error
        );
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn clear(worker_key: &str) {
    let _guard = lock_store();
    let path = native_path();
    let mut identities = read_identities(&path).unwrap_or_default();
    if identities.remove(worker_key).is_none() {
        return;
    }
    if let Err(error) = write_identities(&path, &identities) {
        log::warn!(
            "[loom-ui] could not forget the GitHub identity for worker '{}': {}",
            worker_key,
            error
        );
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
fn native_path() -> std::path::PathBuf {
    loom_core::config_dir().join("worker-identities.json")
}

#[cfg(not(target_family = "wasm"))]
type Identities = std::collections::BTreeMap<String, ParticipantIdentity>;

#[cfg(not(target_family = "wasm"))]
fn read_identities(path: &std::path::Path) -> std::result::Result<Identities, String> {
    if !path.is_file() {
        return Ok(Identities::new());
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

#[cfg(not(target_family = "wasm"))]
fn write_identities(
    path: &std::path::Path,
    identities: &Identities,
) -> std::result::Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "identity store path must have a parent directory".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(identities).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

#[cfg(target_family = "wasm")]
fn storage_key(worker_key: &str) -> String {
    format!("loom.worker_identity.{worker_key}")
}

#[cfg(target_family = "wasm")]
pub(crate) fn load(worker_key: &str) -> Option<ParticipantIdentity> {
    let storage = crate::browser::browser_storage().ok()?;
    let stored = storage.get_item(&storage_key(worker_key)).ok().flatten()?;
    serde_json::from_str(&stored).ok()
}

#[cfg(target_family = "wasm")]
pub(crate) fn save(worker_key: &str, identity: &ParticipantIdentity) {
    let Ok(storage) = crate::browser::browser_storage() else {
        return;
    };
    match serde_json::to_string(identity) {
        Ok(serialized) => {
            if let Err(error) = storage.set_item(&storage_key(worker_key), &serialized) {
                log::warn!(
                    "[loom-ui] could not remember the GitHub identity for worker '{}': {:?}",
                    worker_key,
                    error
                );
            }
        }
        Err(error) => log::warn!(
            "[loom-ui] could not encode the GitHub identity for worker '{}': {}",
            worker_key,
            error
        ),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) fn clear(worker_key: &str) {
    let Ok(storage) = crate::browser::browser_storage() else {
        return;
    };
    if let Err(error) = storage.remove_item(&storage_key(worker_key)) {
        log::warn!(
            "[loom-ui] could not forget the GitHub identity for worker '{}': {:?}",
            worker_key,
            error
        );
    }
}

/// Tests never touch the developer's real config directory. The file helpers
/// above are exercised with temporary paths instead.
#[cfg(test)]
pub(crate) fn load(_worker_key: &str) -> Option<ParticipantIdentity> {
    None
}

#[cfg(test)]
pub(crate) fn save(_worker_key: &str, _identity: &ParticipantIdentity) {}

#[cfg(test)]
pub(crate) fn clear(_worker_key: &str) {}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use loom_core::ParticipantId;

    fn person(name: &str) -> ParticipantIdentity {
        ParticipantIdentity {
            id: ParticipantId::new(),
            display_name: name.to_owned(),
        }
    }

    #[test]
    fn identities_round_trip_per_worker_and_can_be_forgotten() {
        let path = std::env::temp_dir().join(format!(
            "loom-worker-identities-{}.json",
            ParticipantId::new()
        ));
        let first = person("octocat");
        let second = person("hubot");
        let mut identities = Identities::new();
        identities.insert("worker-a".to_owned(), first.clone());
        identities.insert("worker-b".to_owned(), second.clone());
        write_identities(&path, &identities).unwrap();

        let loaded = read_identities(&path).unwrap();
        assert_eq!(loaded.get("worker-a"), Some(&first));
        assert_eq!(loaded.get("worker-b"), Some(&second));

        let mut identities = loaded;
        identities.remove("worker-a");
        write_identities(&path, &identities).unwrap();
        let loaded = read_identities(&path).unwrap();
        assert!(!loaded.contains_key("worker-a"));
        assert_eq!(loaded.get("worker-b"), Some(&second));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_worker_key_outlives_a_restart_and_never_aliases_another_worker() {
        // A remote worker is named by its address, so the entry a restart reads
        // is the one the previous process wrote even though its node id changed.
        let address = "ws://127.0.0.1:8765/";
        assert_eq!(
            worker_key(Some(address)),
            worker_key(Some(" ws://127.0.0.1:8765 "))
        );
        assert_eq!(
            worker_key(Some(address)),
            "remote:ws://127.0.0.1:8765".to_owned()
        );
        assert_ne!(
            worker_key(Some(address)),
            worker_key(Some("ws://127.0.0.1:8766"))
        );

        // The managed local worker is named by the state database it owns, which
        // is never one of the remote workers' keys.
        let local = worker_key(None);
        assert!(local.starts_with("local:"), "{local}");
        assert_ne!(local, worker_key(Some(address)));
    }
}
