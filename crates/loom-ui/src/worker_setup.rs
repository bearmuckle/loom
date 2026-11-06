//! The first-run worker setup a native client shows when it has no worker.
//!
//! The client no longer starts a backend itself. With no `--remote`, it either
//! reconnects to the worker this setup chose last time or shows this screen:
//!
//! - "Install a worker on this machine" installs the latest `loom-server`
//!   release as a user service and connects to it;
//! - a URL, an optional token, and Connect reach a worker elsewhere, including
//!   an `ssh://` URL the client provisions.
//!
//! The choice is kept in `config_dir()/initial-worker.json`; a remote worker's
//! token is kept in `config_dir()/initial-worker-token` (mode `0600`), and a
//! local worker's token stays in the worker's own state directory.

use std::{fs, io::Write as _, path::Path};

use gpui_kit::{Entity, component::input::InputState};
use loom_local::UiOptions;

/// The setup screen's state.
pub(crate) struct WorkerSetup {
    /// The startup options the connected view is built from.
    pub(crate) options: UiOptions,
    pub(crate) url_input: Option<Entity<InputState>>,
    pub(crate) token_input: Option<Entity<InputState>>,
    pub(crate) busy: bool,
    pub(crate) progress: Option<String>,
    pub(crate) error: Option<String>,
}

impl WorkerSetup {
    pub(crate) fn new(options: UiOptions, startup_error: Option<String>) -> Self {
        Self {
            options,
            url_input: None,
            token_input: None,
            busy: false,
            progress: None,
            error: startup_error,
        }
    }
}

/// The worker this client connects to when `--remote` is not given.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct InitialWorker {
    pub url: String,
    /// Installed on this machine, so its token comes from the worker's own
    /// file instead of the client's credential file.
    pub local: bool,
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn load_initial_worker() -> Option<InitialWorker> {
    let path = worker_file();
    match read_worker(&path) {
        Ok(worker) => worker,
        Err(error) => {
            log::warn!(
                "[loom-ui] could not read the initial worker from '{}': {error}",
                path.display()
            );
            None
        }
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn save_initial_worker(worker: &InitialWorker) {
    let path = worker_file();
    if let Err(error) = write_worker(&path, worker) {
        log::warn!(
            "[loom-ui] could not save the initial worker to '{}': {error}",
            path.display()
        );
    }
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn save_initial_worker_token(token: &str) {
    let path = token_file();
    if let Err(error) = write_secret(&path, token) {
        log::warn!(
            "[loom-ui] could not save the initial worker token to '{}': {error}",
            path.display()
        );
    }
}

/// The token to present to the initial worker.
#[cfg(all(not(target_family = "wasm"), not(test)))]
pub(crate) fn initial_worker_token(worker: &InitialWorker) -> String {
    if worker.local {
        return loom_local::local::installed_token().unwrap_or_default();
    }
    read_secret(&token_file()).unwrap_or_default()
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
fn worker_file() -> std::path::PathBuf {
    loom_core::config_dir().join("initial-worker.json")
}

#[cfg(all(not(target_family = "wasm"), not(test)))]
fn token_file() -> std::path::PathBuf {
    loom_core::config_dir().join("initial-worker-token")
}

#[cfg(not(target_family = "wasm"))]
fn read_worker(path: &Path) -> Result<Option<InitialWorker>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let worker = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    Ok(Some(worker))
}

#[cfg(not(target_family = "wasm"))]
fn write_worker(path: &Path, worker: &InitialWorker) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(worker).map_err(|error| error.to_string())?;
    write_replaced(path, &bytes)
}

#[cfg(not(target_family = "wasm"))]
fn read_secret(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

#[cfg(not(target_family = "wasm"))]
fn write_secret(path: &Path, token: &str) -> Result<(), String> {
    write_replaced(path, token.as_bytes())
}

/// Writes a file owner-only through a temporary so a reader never sees a
/// partial file.
#[cfg(not(target_family = "wasm"))]
fn write_replaced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "the initial worker path must have a parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("tmp");
    write_private(&temporary, bytes)?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    restrict_to_owner(path)
}

#[cfg(all(not(target_family = "wasm"), unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())
}

#[cfg(all(not(target_family = "wasm"), not(unix)))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())
}

#[cfg(all(not(target_family = "wasm"), unix))]
fn restrict_to_owner(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| error.to_string())
}

#[cfg(all(not(target_family = "wasm"), not(unix)))]
fn restrict_to_owner(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Tests never read or write the developer's real config directory.
#[cfg(test)]
mod test_store {
    use super::*;
    use std::sync::Mutex;

    static STORE: Mutex<Option<(InitialWorker, Option<String>)>> = Mutex::new(None);

    pub(super) fn load() -> Option<InitialWorker> {
        STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(worker, _)| worker.clone())
    }

    pub(super) fn save(worker: InitialWorker) {
        let mut store = STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let token = store.as_ref().and_then(|(_, token)| token.clone());
        *store = Some((worker, token));
    }

    pub(super) fn save_token(token: &str) {
        let mut store = STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let worker = store.as_ref().map(|(worker, _)| worker.clone());
        *store = worker.map(|worker| (worker, Some(token.to_owned())));
    }

    pub(super) fn token(worker: &InitialWorker) -> String {
        if worker.local {
            return String::new();
        }
        STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|(_, token)| token.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
pub(crate) fn load_initial_worker() -> Option<InitialWorker> {
    test_store::load()
}

#[cfg(test)]
pub(crate) fn save_initial_worker(worker: &InitialWorker) {
    test_store::save(worker.clone());
}

#[cfg(test)]
pub(crate) fn save_initial_worker_token(token: &str) {
    test_store::save_token(token);
}

#[cfg(test)]
pub(crate) fn initial_worker_token(worker: &InitialWorker) -> String {
    test_store::token(worker)
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("loom-initial-worker-{}", uuid::Uuid::new_v4()))
            .join("worker.json")
    }

    #[test]
    fn the_initial_worker_round_trips() {
        let worker = InitialWorker {
            url: "ws://127.0.0.1:8765/ws".to_owned(),
            local: true,
        };
        let path = temporary_path();
        write_worker(&path, &worker).unwrap();
        assert_eq!(read_worker(&path).unwrap(), Some(worker));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_missing_file_means_no_initial_worker() {
        assert_eq!(
            read_worker(&temporary_path().join("missing")).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_path();
        let path = root.join("token");
        write_secret(&path, "secret-token").unwrap();
        assert_eq!(read_secret(&path).as_deref(), Some("secret-token"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }
}
