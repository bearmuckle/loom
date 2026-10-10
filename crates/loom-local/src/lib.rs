//! Native platform adapters: process arguments, workspace preparation, local
//! credential storage, and repository bootstrap.
//!
//! These are the pieces a browser target cannot use unchanged, so they are kept
//! out of the view and connection modules.

#[cfg(test)]
use std::{collections::BTreeMap, sync::Mutex};
use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use loom_core::{ErrorCode, LoomError, WorkspaceId};
use loom_model::ModelId;
use loom_providers::{CredentialRef, GITHUB_COPILOT_DEFAULT_MODEL};
use loom_vcs::GitService;
use sha2::{Digest, Sha256};

mod connection;

pub use connection::{LocalConnection, OwnedBackend, RemoteConnection, RemoteConnectionOptions};

/// Whether the native WebSocket transport can negotiate TLS (`wss://`).
///
/// Clients re-export this capability so they report what the transport can
/// really do instead of trusting a URL scheme.
pub use loom_server::WEBSOCKET_TLS_SUPPORTED;

const PEER_CREDENTIAL_SERVICE: &str = "com.bearmuckle.loom.worker-peer";

type SecretResult<T> = std::result::Result<T, String>;

trait PeerCredentialBackend: Send + Sync {
    fn get(&self, reference: &str) -> SecretResult<Option<String>>;
    fn set(&self, reference: &str, token: &str) -> SecretResult<()>;
    fn delete(&self, reference: &str) -> SecretResult<()>;
}

struct OsCredentialBackend;

impl PeerCredentialBackend for OsCredentialBackend {
    fn get(&self, reference: &str) -> SecretResult<Option<String>> {
        let entry = keyring::Entry::new(PEER_CREDENTIAL_SERVICE, reference)
            .map_err(|error| error.to_string())?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn set(&self, reference: &str, token: &str) -> SecretResult<()> {
        let entry = keyring::Entry::new(PEER_CREDENTIAL_SERVICE, reference)
            .map_err(|error| error.to_string())?;
        entry.set_password(token).map_err(|error| error.to_string())
    }

    fn delete(&self, reference: &str) -> SecretResult<()> {
        let entry = keyring::Entry::new(PEER_CREDENTIAL_SERVICE, reference)
            .map_err(|error| error.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub struct PeerCredentialStore {
    backend: Arc<dyn PeerCredentialBackend>,
}

impl Default for PeerCredentialStore {
    fn default() -> Self {
        Self {
            backend: Arc::new(OsCredentialBackend),
        }
    }
}

impl PeerCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn PeerCredentialBackend>) -> Self {
        Self { backend }
    }

    pub fn get(&self, workspace_id: WorkspaceId, url: &str) -> Result<Option<String>, LoomError> {
        self.backend
            .get(peer_credential_reference(workspace_id, url).as_str())
            .map_err(|error| peer_credential_error("read", error))
    }

    pub fn set(&self, workspace_id: WorkspaceId, url: &str, token: &str) -> Result<(), LoomError> {
        self.backend
            .set(peer_credential_reference(workspace_id, url).as_str(), token)
            .map_err(|error| peer_credential_error("save", error))
    }

    pub fn delete(&self, workspace_id: WorkspaceId, url: &str) -> Result<(), LoomError> {
        self.backend
            .delete(peer_credential_reference(workspace_id, url).as_str())
            .map_err(|error| peer_credential_error("remove", error))
    }
}

fn peer_credential_reference(workspace_id: WorkspaceId, url: &str) -> CredentialRef {
    let mut digest = Sha256::new();
    digest.update(workspace_id.to_string().as_bytes());
    digest.update([0]);
    digest.update(url.as_bytes());
    let key = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    CredentialRef::new(format!("worker-peer-{key}"))
}

fn peer_credential_error(operation: &str, error: String) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("could not {operation} worker-node credential in the OS credential store: {error}"),
        false,
    )
}

#[cfg(test)]
#[derive(Default)]
struct MemoryPeerCredentialBackend(Mutex<BTreeMap<String, String>>);

#[cfg(test)]
impl PeerCredentialBackend for MemoryPeerCredentialBackend {
    fn get(&self, reference: &str) -> SecretResult<Option<String>> {
        Ok(self.0.lock().unwrap().get(reference).cloned())
    }

    fn set(&self, reference: &str, token: &str) -> SecretResult<()> {
        self.0
            .lock()
            .unwrap()
            .insert(reference.to_owned(), token.to_owned());
        Ok(())
    }

    fn delete(&self, reference: &str) -> SecretResult<()> {
        self.0.lock().unwrap().remove(reference);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct UiOptions {
    pub project: Option<PathBuf>,
    pub task: String,
    pub demo: bool,
    pub model: ModelId,
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub remote: Option<String>,
    pub token: Option<String>,
    pub reset_state: bool,
    /// PEM file whose certificates are trusted in addition to the OS roots for
    /// `wss://` workers. `--ca` overrides the `LOOM_TLS_CA` environment
    /// variable.
    pub ca: Option<PathBuf>,
    /// Explicit opt-in that permits plaintext `ws://` to a non-loopback worker.
    pub allow_insecure_remote: bool,
}

/// The version reported by `--version`: the release version stamped at build
/// time, plus the revision when one was stamped.
pub fn build_version_label() -> String {
    version_label(
        option_env!("LOOM_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
        option_env!("LOOM_GIT_REVISION").unwrap_or("unknown"),
    )
}

/// Joins a version and revision the way the client's About pane does, omitting
/// an absent revision.
fn version_label(version: &str, revision: &str) -> String {
    let revision = revision.trim();
    if revision.is_empty() || revision == "unknown" {
        version.to_owned()
    } else {
        format!("{version} · {revision}")
    }
}

/// The usage text printed by `--help`.
fn usage_text() -> String {
    "Usage: loom-ui [--project PATH] [--task DESCRIPTION] [--model ID] [--endpoint URL] \
     [--remote URL] [--ca PATH] [--allow-insecure-remote] [--reset-state] [--demo]\n\
     \n\
     Options:\n\
     \x20 --project PATH        open this directory as the workspace\n\
     \x20 --task DESCRIPTION    initial task description\n\
     \x20 --model ID            model id to use\n\
     \x20 --endpoint URL        OpenAI-compatible endpoint\n\
     \x20 --remote URL          connect to a Loom worker instead of a local backend\n\
     \x20 --ca PATH             PEM file of additional trusted CAs for wss:// workers\n\
     \x20 --allow-insecure-remote\n\
     \x20                       allow plaintext ws:// to a non-loopback worker\n\
     \x20 --reset-state         wipe an incompatible local state database\n\
     \x20 --demo                use the deterministic demo provider\n\
     \x20 -h, --help            print this help and exit\n\
     \x20 -V, --version         print the version and exit\n\
     \n\
     LOOM_TLS_CA names the same PEM file as --ca when the flag is not given."
        .to_owned()
}

impl UiOptions {
    /// Parses the command line. `Ok(None)` means `--help` or `--version` already
    /// printed what was asked for and the process should exit successfully.
    pub fn parse<I>(args: I) -> Result<Option<Self>, LoomError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut project = None;
        let mut task = "make a small repository change and validate it".to_owned();
        let mut demo = false;
        let mut model = env::var("LOOM_MODEL")
            .map(ModelId::new)
            .unwrap_or_else(|_| ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL));
        let mut endpoint = env::var("LOOM_OPENAI_ENDPOINT").ok();
        let api_key = env::var("LOOM_API_KEY").ok();
        let mut remote = env::var("LOOM_REMOTE_URL").ok();
        let token = env::var("LOOM_TOKEN").ok();
        let mut reset_state = false;
        let mut ca = env::var_os("LOOM_TLS_CA").map(PathBuf::from);
        let mut allow_insecure_remote = false;
        let mut args = args.into_iter().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--project" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--project requires a path"))?;
                    project = Some(PathBuf::from(value));
                    demo = false;
                }
                "--task" => {
                    task = args.next().ok_or_else(|| {
                        LoomError::invalid_request("--task requires a description")
                    })?;
                    if task.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--task requires a non-empty description",
                        ));
                    }
                }
                "--model" => {
                    model = ModelId::new(args.next().ok_or_else(|| {
                        LoomError::invalid_request("--model requires a model id")
                    })?);
                    if model.as_str().trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--model requires a non-empty model id",
                        ));
                    }
                }
                "--endpoint" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--endpoint requires a URL"))?;
                    if value.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--endpoint requires a non-empty URL",
                        ));
                    }
                    endpoint = Some(value);
                }
                "--remote" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--remote requires a URL"))?;
                    if value.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--remote requires a non-empty URL",
                        ));
                    }
                    remote = Some(value);
                    demo = false;
                }
                "--demo" => {
                    project = None;
                    demo = true;
                    model = ModelId::new("deterministic/demo");
                }
                "--ca" => {
                    let value = args.next().ok_or_else(|| {
                        LoomError::invalid_request("--ca requires a PEM file path")
                    })?;
                    if value.trim().is_empty() {
                        return Err(LoomError::invalid_request(
                            "--ca requires a non-empty PEM file path",
                        ));
                    }
                    ca = Some(PathBuf::from(value));
                }
                "--allow-insecure-remote" => {
                    allow_insecure_remote = true;
                }
                "--reset-state" => {
                    reset_state = true;
                }
                "--help" | "-h" => {
                    println!("{}", usage_text());
                    return Ok(None);
                }
                "--version" | "-V" => {
                    println!("loom-ui {}", build_version_label());
                    return Ok(None);
                }
                unknown => {
                    return Err(LoomError::invalid_request(format!(
                        "unknown argument '{unknown}'; use --help for usage"
                    )));
                }
            }
        }
        Ok(Some(Self {
            project,
            task,
            demo,
            model,
            endpoint,
            api_key,
            remote,
            token,
            reset_state,
            ca,
            allow_insecure_remote,
        }))
    }
}

/// Ensures the local state database can be opened by this build. An
/// incompatible database is wiped only when the operator passed
/// `--reset-state` or explicitly confirmed the interactive prompt; otherwise
/// the error is returned so the GUI reports it instead of failing silently.
pub fn prepare_backend_state(options: &UiOptions) -> Result<(), LoomError> {
    if options.remote.is_some() || options.demo {
        return Ok(());
    }
    let path = backend_persistence_path();
    let status = loom_persistence::FilePersistence::schema_status(&path)?;
    if status.is_compatible() {
        return Ok(());
    }
    let wipe = options.reset_state || confirm_state_wipe(&path, status)?;
    if !wipe {
        return Err(loom_persistence::incompatible_database_error(&path, status));
    }
    loom_persistence::FilePersistence::reset_database(&path)
}

fn confirm_state_wipe(
    path: &Path,
    status: loom_persistence::SchemaStatus,
) -> Result<bool, LoomError> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    eprintln!(
        "Loom state database '{}' uses {} and cannot be opened by this build.",
        path.display(),
        status.description()
    );
    eprint!("Wipe it and start with an empty database? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| LoomError::new(ErrorCode::Internal, format!("{error}"), true))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| LoomError::new(ErrorCode::Internal, format!("{error}"), true))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Resolves the default project root from a process working directory. Returns
/// an empty path when the working directory is inside Loom's own state
/// directory, because adopting an internal session root as a user project is
/// not useful. A non-empty result is canonicalized.
fn default_project_root(
    current_dir: io::Result<PathBuf>,
    state_directory: &Path,
) -> Result<PathBuf, LoomError> {
    let current = current_dir.map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not determine current workspace: {error}"),
            false,
        )
    })?;
    let root = fs::canonicalize(current).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not open current workspace: {error}"),
            false,
        )
    })?;
    if root.starts_with(state_directory) {
        return Ok(PathBuf::new());
    }
    Ok(root)
}

/// Loom's own state directory, canonicalized when it already exists so it can
/// be compared against canonical working directories.
fn canonical_state_directory() -> PathBuf {
    let directory = loom_core::state_dir();
    if let Ok(canonical) = fs::canonicalize(&directory) {
        return canonical;
    }
    // The state directory does not exist yet, so canonicalize the state root
    // instead and name the `loom` subdirectory the shared helper appends.
    let root = loom_core::state_root();
    let root = fs::canonicalize(&root).unwrap_or(root);
    root.join("loom")
}

pub fn prepare_workspace(options: &UiOptions) -> Result<(PathBuf, bool), LoomError> {
    if let Some(project) = &options.project {
        let root = fs::canonicalize(project).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not open project '{}': {error}", project.display()),
                false,
            )
        })?;
        if !root.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("project '{}' is not a directory", root.display()),
                false,
            ));
        }
        return Ok((root, false));
    }

    if !options.demo {
        let root = default_project_root(env::current_dir(), &canonical_state_directory())?;
        return Ok((root, false));
    }

    let root = env::temp_dir().join("loom-m5-ui-demo");
    fs::create_dir_all(&root).map_err(|error| {
        LoomError::new(
            ErrorCode::ToolExecution,
            format!("could not create UI workspace: {error}"),
            false,
        )
    })?;
    let readme = root.join("README.md");
    if !readme.exists() {
        fs::write(
            readme,
            "Workspace used by the Loom M5 agent workspace demo.\n",
        )
        .map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not seed UI workspace: {error}"),
                false,
            )
        })?;
    }
    if !root.join(".git").is_dir() {
        GitService::init(&root)?;
    }
    Ok((root, options.demo))
}

pub fn backend_persistence_path() -> PathBuf {
    loom_core::state_dir().join("state.db")
}

/// Public device-code payload for a GitHub sign-in.
pub use loom_providers::GitHubCopilotAuthenticator;
pub use loom_providers::GitHubDeviceCode;

/// Starts a GitHub Copilot device authorization flow.
pub fn github_copilot_login_begin() -> Result<GitHubDeviceCode, LoomError> {
    loom_providers::GitHubCopilotAuthenticator::default().begin()
}

/// Polls a device authorization until it yields an access token.
pub fn github_copilot_login_poll(device: &GitHubDeviceCode) -> Result<String, LoomError> {
    loom_providers::GitHubCopilotAuthenticator::default().poll(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FailingCredentialBackend;

    impl PeerCredentialBackend for FailingCredentialBackend {
        fn get(&self, _: &str) -> SecretResult<Option<String>> {
            Err("read failed".to_owned())
        }

        fn set(&self, _: &str, _: &str) -> SecretResult<()> {
            Err("save failed".to_owned())
        }

        fn delete(&self, _: &str) -> SecretResult<()> {
            Err("remove failed".to_owned())
        }
    }

    #[test]
    fn ui_options_allow_explicit_workspace_and_task() {
        let options = UiOptions::parse([
            "loom-ui".to_owned(),
            "--project".to_owned(),
            "/tmp/project".to_owned(),
            "--task".to_owned(),
            "fix the agent flow".to_owned(),
            "--model".to_owned(),
            "gpt-4o-mini".to_owned(),
            "--endpoint".to_owned(),
            "http://127.0.0.1:8000/v1/chat/completions".to_owned(),
            "--remote".to_owned(),
            "ws://127.0.0.1:8080/ws".to_owned(),
            "--ca".to_owned(),
            "/tmp/loom-ca.pem".to_owned(),
            "--allow-insecure-remote".to_owned(),
            "--reset-state".to_owned(),
        ])
        .unwrap()
        .expect("arguments describe a runnable client");
        assert_eq!(options.project, Some(PathBuf::from("/tmp/project")));
        assert_eq!(options.task, "fix the agent flow");
        assert_eq!(options.model.as_str(), "gpt-4o-mini");
        assert_eq!(
            options.endpoint.as_deref(),
            Some("http://127.0.0.1:8000/v1/chat/completions")
        );
        assert_eq!(options.remote.as_deref(), Some("ws://127.0.0.1:8080/ws"));
        assert_eq!(options.ca.as_deref(), Some(Path::new("/tmp/loom-ca.pem")));
        assert!(options.allow_insecure_remote);
        assert!(options.reset_state);
        assert!(!options.demo);
    }

    #[test]
    fn ui_options_reject_missing_empty_and_unknown_arguments() {
        for args in [
            vec!["--project"],
            vec!["--task"],
            vec!["--task", "   "],
            vec!["--model"],
            vec!["--model", "  "],
            vec!["--endpoint"],
            vec!["--endpoint", "  "],
            vec!["--remote"],
            vec!["--remote", "  "],
            vec!["--ca"],
            vec!["--ca", "  "],
            vec!["--unknown"],
            vec!["-x"],
        ] {
            let result = UiOptions::parse(
                std::iter::once("loom-ui".to_owned())
                    .chain(args.iter().copied().map(str::to_owned)),
            );
            assert!(result.is_err(), "accepted arguments: {args:?}");
            assert_eq!(result.unwrap_err().code, ErrorCode::InvalidRequest);
        }
        let demo = UiOptions::parse(["loom-ui".to_owned(), "--demo".to_owned()])
            .unwrap()
            .expect("demo arguments describe a runnable client");
        assert!(demo.demo);
        assert_eq!(demo.model.as_str(), "deterministic/demo");
        assert_eq!(demo.ca, None);
        assert!(!demo.allow_insecure_remote);
    }

    #[test]
    fn ui_options_defaults_and_environment_supply_the_tls_ca() {
        let defaults = UiOptions::parse(["loom-ui".to_owned()])
            .unwrap()
            .expect("no arguments describe a runnable client");
        assert_eq!(defaults.ca, None);
        assert!(!defaults.allow_insecure_remote);

        let options = UiOptions::parse([
            "loom-ui".to_owned(),
            "--ca".to_owned(),
            "/tmp/flag-ca.pem".to_owned(),
        ])
        .unwrap()
        .expect("a CA path describes a runnable client");
        assert_eq!(options.ca.as_deref(), Some(Path::new("/tmp/flag-ca.pem")));
    }

    #[test]
    fn ui_options_help_and_version_print_and_stop_argument_parsing() {
        for args in [
            vec!["--help"],
            vec!["-h", "--unknown"],
            vec!["--version"],
            vec!["-V", "--project"],
        ] {
            let result = UiOptions::parse(
                std::iter::once("loom-ui".to_owned())
                    .chain(args.iter().copied().map(str::to_owned)),
            );
            assert!(
                result.unwrap().is_none(),
                "expected printed output for {args:?}"
            );
        }
        assert!(!build_version_label().is_empty());
        assert!(usage_text().contains("--remote URL"));
        assert!(usage_text().contains("--ca PATH"));
        assert!(usage_text().contains("--allow-insecure-remote"));
        assert_eq!(version_label("v0.8.1", "a1b2c3d"), "v0.8.1 · a1b2c3d");
        assert_eq!(version_label("0.1.0", "unknown"), "0.1.0");
        assert_eq!(version_label("0.1.0", "   "), "0.1.0");
    }

    #[test]
    fn workspace_preparation_validates_and_initializes_a_repository() {
        let root = env::temp_dir().join(format!("loom-ui-platform-{}", WorkspaceId::new()));
        fs::create_dir_all(&root).unwrap();
        let mut options = UiOptions::parse(["loom-ui".to_owned()])
            .unwrap()
            .expect("no arguments describe a runnable client");
        options.project = Some(root.clone());
        let (resolved, demo) = prepare_workspace(&options).unwrap();
        assert_eq!(resolved, fs::canonicalize(&root).unwrap());
        assert!(!demo);

        options.project = Some(root.join("missing"));
        assert_eq!(
            prepare_workspace(&options).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        let file = root.join("not-a-directory");
        fs::write(&file, "file").unwrap();
        options.project = Some(file);
        assert_eq!(
            prepare_workspace(&options).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_project_root_skips_loom_state_directories() {
        let base = env::temp_dir().join(format!("loom-ui-default-root-{}", WorkspaceId::new()));
        let state = base.join("state").join("loom");
        let internal = state
            .join("state.session-roots")
            .join("workspace")
            .join("session")
            .join("fs");
        let external = base.join("project");
        fs::create_dir_all(&internal).unwrap();
        fs::create_dir_all(&external).unwrap();
        let state = fs::canonicalize(&state).unwrap();
        let external = fs::canonicalize(&external).unwrap();

        assert_eq!(
            default_project_root(Ok(internal), &state).unwrap(),
            PathBuf::new()
        );
        assert_eq!(
            default_project_root(Ok(external.clone()), &state).unwrap(),
            external
        );
        assert_eq!(
            default_project_root(Ok(base.join("missing")), &state)
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceAccessDenied
        );
        assert_eq!(
            default_project_root(
                Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
                &state
            )
            .unwrap_err()
            .code,
            ErrorCode::WorkspaceAccessDenied
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn canonical_state_directory_is_the_loom_state_directory() {
        assert_eq!(
            canonical_state_directory()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("loom")
        );
    }

    #[test]
    fn workspace_preparation_defaults_to_the_process_directory_unless_it_is_internal() {
        let options = UiOptions::parse(["loom-ui".to_owned()])
            .unwrap()
            .expect("no arguments describe a runnable client");
        let (root, demo) = prepare_workspace(&options).unwrap();
        assert!(!demo);
        let current = fs::canonicalize(env::current_dir().unwrap()).unwrap();
        if current.starts_with(canonical_state_directory()) {
            assert!(root.as_os_str().is_empty());
        } else {
            assert_eq!(root, current);
        }
    }

    #[test]
    fn persistence_is_shared_across_workspaces() {
        assert_eq!(
            backend_persistence_path(),
            loom_core::state_dir().join("state.db")
        );
    }

    #[test]
    fn remote_connection_rejects_unusable_ca_files_before_connecting() {
        let options = RemoteConnectionOptions {
            ca_certificate: Some(PathBuf::from("/nonexistent/loom-ca.pem")),
            allow_insecure_remote: false,
        };
        let error = RemoteConnection::connect("wss://worker.example/ws", "token", false, &options)
            .err()
            .expect("a missing CA file must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(
            error.message.contains("could not read the TLS CA file"),
            "{error}"
        );
    }

    #[test]
    fn remote_connection_defers_the_plaintext_guard_to_the_transport() {
        let options = RemoteConnectionOptions::default();
        let error =
            RemoteConnection::connect("ws://worker.example:8765/ws", "token", false, &options)
                .err()
                .expect("a plaintext remote endpoint must be refused");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("--allow-insecure-remote"), "{error}");
        const { assert!(super::WEBSOCKET_TLS_SUPPORTED) };
    }

    #[test]
    fn peer_credentials_are_scoped_by_workspace_and_url_and_deleted_with_peer() {
        let backend = Arc::new(MemoryPeerCredentialBackend::default());
        let store = PeerCredentialStore::with_backend(backend);
        let workspace_id = WorkspaceId::new();
        let other_workspace_id = WorkspaceId::new();
        let first_url = "wss://worker.example/ws";
        let reference = peer_credential_reference(workspace_id, first_url);
        assert!(!reference.as_str().contains(first_url));

        store.set(workspace_id, first_url, "peer-secret").unwrap();

        assert_eq!(
            store.get(workspace_id, first_url).unwrap().as_deref(),
            Some("peer-secret")
        );
        assert_eq!(store.get(other_workspace_id, first_url).unwrap(), None);
        assert_eq!(
            store.get(workspace_id, "wss://other.example/ws").unwrap(),
            None
        );
        store.delete(workspace_id, first_url).unwrap();
        assert_eq!(store.get(workspace_id, first_url).unwrap(), None);
    }

    #[test]
    fn peer_credential_errors_are_wrapped_with_the_operation_name() {
        let store = PeerCredentialStore::with_backend(Arc::new(FailingCredentialBackend));
        let workspace_id = WorkspaceId::new();
        assert!(
            store
                .get(workspace_id, "wss://worker.example/ws")
                .unwrap_err()
                .message
                .contains("read worker-node credential")
        );
        assert!(
            store
                .set(workspace_id, "wss://worker.example/ws", "secret")
                .unwrap_err()
                .message
                .contains("save worker-node credential")
        );
        assert!(
            store
                .delete(workspace_id, "wss://worker.example/ws")
                .unwrap_err()
                .message
                .contains("remove worker-node credential")
        );
    }
}
