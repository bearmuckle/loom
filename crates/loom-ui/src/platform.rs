//! Native platform adapters: process arguments, workspace preparation, local
//! credential storage, and repository bootstrap.
//!
//! These are the pieces a browser target cannot use unchanged, so they are kept
//! out of the view and connection modules.

#[cfg(test)]
use std::{collections::BTreeMap, sync::Mutex};
use std::{env, fs, path::PathBuf, sync::Arc};

use loom_core::{ErrorCode, LoomError, WorkspaceId};
use loom_model::ModelId;
use loom_providers::{CredentialRef, GITHUB_COPILOT_DEFAULT_MODEL};
use loom_vcs::GitService;
use sha2::{Digest, Sha256};

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

pub(crate) struct PeerCredentialStore {
    backend: Arc<dyn PeerCredentialBackend>,
}

impl PeerCredentialStore {
    pub(crate) fn new() -> Self {
        Self {
            backend: Arc::new(OsCredentialBackend),
        }
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn PeerCredentialBackend>) -> Self {
        Self { backend }
    }

    pub(crate) fn get(
        &self,
        workspace_id: WorkspaceId,
        url: &str,
    ) -> Result<Option<String>, LoomError> {
        self.backend
            .get(peer_credential_reference(workspace_id, url).as_str())
            .map_err(|error| peer_credential_error("read", error))
    }

    pub(crate) fn set(
        &self,
        workspace_id: WorkspaceId,
        url: &str,
        token: &str,
    ) -> Result<(), LoomError> {
        self.backend
            .set(peer_credential_reference(workspace_id, url).as_str(), token)
            .map_err(|error| peer_credential_error("save", error))
    }

    pub(crate) fn delete(&self, workspace_id: WorkspaceId, url: &str) -> Result<(), LoomError> {
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
pub(crate) struct UiOptions {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) task: String,
    pub(crate) demo: bool,
    pub(crate) model: ModelId,
    pub(crate) endpoint: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) remote: Option<String>,
    pub(crate) token: Option<String>,
}

impl UiOptions {
    pub(crate) fn parse<I>(args: I) -> Result<Self, LoomError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut workspace = None;
        let mut task = "make a small repository change and validate it".to_owned();
        let mut demo = false;
        let mut model = env::var("LOOM_MODEL")
            .map(ModelId::new)
            .unwrap_or_else(|_| ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL));
        let mut endpoint = env::var("LOOM_OPENAI_ENDPOINT").ok();
        let api_key = env::var("LOOM_API_KEY").ok();
        let mut remote = env::var("LOOM_REMOTE_URL").ok();
        let token = env::var("LOOM_TOKEN").ok();
        let mut args = args.into_iter().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--workspace" => {
                    let value = args
                        .next()
                        .ok_or_else(|| LoomError::invalid_request("--workspace requires a path"))?;
                    workspace = Some(PathBuf::from(value));
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
                    workspace = None;
                    demo = true;
                    model = ModelId::new("deterministic/demo");
                }
                "--help" | "-h" => {
                    return Err(LoomError::invalid_request(
                        "usage: loom-ui [--workspace PATH] [--task DESCRIPTION] [--model ID] [--endpoint URL] [--remote URL] [--demo]",
                    ));
                }
                unknown => {
                    return Err(LoomError::invalid_request(format!(
                        "unknown argument '{unknown}'; use --help for usage"
                    )));
                }
            }
        }
        Ok(Self {
            workspace,
            task,
            demo,
            model,
            endpoint,
            api_key,
            remote,
            token,
        })
    }
}

pub(crate) fn prepare_workspace(options: &UiOptions) -> Result<(PathBuf, bool), LoomError> {
    if let Some(workspace) = &options.workspace {
        let root = fs::canonicalize(workspace).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "could not open workspace '{}': {error}",
                    workspace.display()
                ),
                false,
            )
        })?;
        if !root.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("workspace '{}' is not a directory", root.display()),
                false,
            ));
        }
        return Ok((root, false));
    }

    if !options.demo {
        let root = fs::canonicalize(env::current_dir().map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not determine current workspace: {error}"),
                false,
            )
        })?)
        .map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not open current workspace: {error}"),
                false,
            )
        })?;
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

pub(crate) fn backend_persistence_path() -> PathBuf {
    state_root().join("loom").join("state.db")
}

fn state_root() -> PathBuf {
    env::var_os("LOOM_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .unwrap_or_else(|| env::temp_dir().join("loom-state"))
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
            "--workspace".to_owned(),
            "/tmp/project".to_owned(),
            "--task".to_owned(),
            "fix the agent flow".to_owned(),
            "--model".to_owned(),
            "gpt-4o-mini".to_owned(),
            "--endpoint".to_owned(),
            "http://127.0.0.1:8000/v1/chat/completions".to_owned(),
            "--remote".to_owned(),
            "ws://127.0.0.1:8080/ws".to_owned(),
        ])
        .unwrap();
        assert_eq!(options.workspace, Some(PathBuf::from("/tmp/project")));
        assert_eq!(options.task, "fix the agent flow");
        assert_eq!(options.model.as_str(), "gpt-4o-mini");
        assert_eq!(
            options.endpoint.as_deref(),
            Some("http://127.0.0.1:8000/v1/chat/completions")
        );
        assert_eq!(options.remote.as_deref(), Some("ws://127.0.0.1:8080/ws"));
        assert!(!options.demo);
    }

    #[test]
    fn ui_options_reject_missing_empty_and_unknown_arguments() {
        for args in [
            vec!["--workspace"],
            vec!["--task"],
            vec!["--task", "   "],
            vec!["--model"],
            vec!["--model", "  "],
            vec!["--endpoint"],
            vec!["--endpoint", "  "],
            vec!["--remote"],
            vec!["--remote", "  "],
            vec!["--unknown"],
            vec!["--help"],
        ] {
            let result = UiOptions::parse(
                std::iter::once("loom-ui".to_owned())
                    .chain(args.iter().copied().map(str::to_owned)),
            );
            assert!(result.is_err(), "accepted arguments: {args:?}");
            assert_eq!(result.unwrap_err().code, ErrorCode::InvalidRequest);
        }
        let demo = UiOptions::parse(["loom-ui".to_owned(), "--demo".to_owned()]).unwrap();
        assert!(demo.demo);
        assert_eq!(demo.model.as_str(), "deterministic/demo");
    }

    #[test]
    fn workspace_preparation_validates_and_initializes_a_repository() {
        let root = env::temp_dir().join(format!("loom-ui-platform-{}", WorkspaceId::new()));
        fs::create_dir_all(&root).unwrap();
        let mut options = UiOptions::parse(["loom-ui".to_owned()]).unwrap();
        options.workspace = Some(root.clone());
        let (resolved, demo) = prepare_workspace(&options).unwrap();
        assert_eq!(resolved, fs::canonicalize(&root).unwrap());
        assert!(!demo);

        options.workspace = Some(root.join("missing"));
        assert_eq!(
            prepare_workspace(&options).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        let file = root.join("not-a-directory");
        fs::write(&file, "file").unwrap();
        options.workspace = Some(file);
        assert_eq!(
            prepare_workspace(&options).unwrap_err().code,
            ErrorCode::WorkspaceAccessDenied
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistence_is_shared_across_workspaces() {
        assert_eq!(
            backend_persistence_path(),
            state_root().join("loom").join("state.db")
        );
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
