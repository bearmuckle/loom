//! The GitHub participant a client last signed in as on each worker.
//!
//! A GitHub sign-in binds a person to one connection, not to the worker token
//! that reopened it. Remembering the participant id locally lets a reload
//! resume the same person on an operator connection, so the roster shows them
//! online instead of leaving them behind as an offline grant. The id is not a
//! secret: the worker only resumes it for an operator token and only when the
//! person is still in the worker's directory.

use loom_core::ParticipantIdentity;

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
pub(crate) fn load(node_id: &str) -> Option<ParticipantIdentity> {
    let _guard = lock_store();
    let path = native_path();
    match read_identities(&path) {
        Ok(identities) => identities.get(node_id).cloned(),
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
pub(crate) fn save(node_id: &str, identity: &ParticipantIdentity) {
    let _guard = lock_store();
    let path = native_path();
    let mut identities = read_identities(&path).unwrap_or_default();
    if identities.get(node_id) == Some(identity) {
        return;
    }
    identities.insert(node_id.to_owned(), identity.clone());
    if let Err(error) = write_identities(&path, &identities) {
        log::warn!(
            "[loom-ui] could not remember the GitHub identity for worker '{}': {}",
            node_id,
            error
        );
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn clear(node_id: &str) {
    let _guard = lock_store();
    let path = native_path();
    let mut identities = read_identities(&path).unwrap_or_default();
    if identities.remove(node_id).is_none() {
        return;
    }
    if let Err(error) = write_identities(&path, &identities) {
        log::warn!(
            "[loom-ui] could not forget the GitHub identity for worker '{}': {}",
            node_id,
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
fn storage_key(node_id: &str) -> String {
    format!("loom.worker_identity.{node_id}")
}

#[cfg(target_family = "wasm")]
pub(crate) fn load(node_id: &str) -> Option<ParticipantIdentity> {
    let storage = crate::browser::browser_storage().ok()?;
    let stored = storage.get_item(&storage_key(node_id)).ok().flatten()?;
    serde_json::from_str(&stored).ok()
}

#[cfg(target_family = "wasm")]
pub(crate) fn save(node_id: &str, identity: &ParticipantIdentity) {
    let Ok(storage) = crate::browser::browser_storage() else {
        return;
    };
    match serde_json::to_string(identity) {
        Ok(serialized) => {
            if let Err(error) = storage.set_item(&storage_key(node_id), &serialized) {
                log::warn!(
                    "[loom-ui] could not remember the GitHub identity for worker '{}': {:?}",
                    node_id,
                    error
                );
            }
        }
        Err(error) => log::warn!(
            "[loom-ui] could not encode the GitHub identity for worker '{}': {}",
            node_id,
            error
        ),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) fn clear(node_id: &str) {
    let Ok(storage) = crate::browser::browser_storage() else {
        return;
    };
    if let Err(error) = storage.remove_item(&storage_key(node_id)) {
        log::warn!(
            "[loom-ui] could not forget the GitHub identity for worker '{}': {:?}",
            node_id,
            error
        );
    }
}

/// Tests never touch the developer's real config directory. The file helpers
/// above are exercised with temporary paths instead.
#[cfg(test)]
pub(crate) fn load(_node_id: &str) -> Option<ParticipantIdentity> {
    None
}

#[cfg(test)]
pub(crate) fn save(_node_id: &str, _identity: &ParticipantIdentity) {}

#[cfg(test)]
pub(crate) fn clear(_node_id: &str) {}

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
}
