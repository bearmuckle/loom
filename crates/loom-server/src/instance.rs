//! Defaults for one standalone server instance.
//!
//! `loom-server` and `loom --serve` serve the same backend, so the two decisions
//! a first deployment should not have to make live here: where durable state
//! goes and where the bearer token comes from. Explicit `--persistence`,
//! `--token`, and `--token-file` always win over the defaults.

use std::{
    fs,
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use loom_core::{ErrorCode, LoomError};
use loom_protocol::ArchiveRetentionPolicy;

use crate::AuthTokenStore;

/// The durable state database inside an instance directory.
pub const STATE_DB_FILE: &str = "state.db";

/// The default bearer token file inside an instance directory.
pub const TOKEN_FILE: &str = "token";

/// The settings an operator passed for one standalone server, before defaults
/// are applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceOptions {
    pub bind: SocketAddr,
    /// Pins the instance directory for a host-aliased bind address.
    pub instance_name: Option<String>,
    /// Explicit `--persistence` path, which always wins over the default.
    pub persistence: Option<PathBuf>,
    /// Explicit `--token` value.
    pub token: Option<String>,
    /// Explicit `--token-file` path.
    pub token_file: Option<PathBuf>,
    /// `--reset-state`: wipe an incompatible durable database instead of
    /// failing.
    pub reset_state: bool,
    /// `--archive-retention`: when an archived session or project tree may be
    /// discarded automatically. Retention is disabled by default.
    pub archive_retention: ArchiveRetentionPolicy,
}

impl InstanceOptions {
    pub fn new(bind: SocketAddr) -> Self {
        Self {
            bind,
            instance_name: None,
            persistence: None,
            token: None,
            token_file: None,
            reset_state: false,
            archive_retention: ArchiveRetentionPolicy::disabled(),
        }
    }
}

/// Where one standalone server keeps its files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceLayout {
    directory: Option<PathBuf>,
    state_db: Option<PathBuf>,
    token_file: PathBuf,
}

impl InstanceLayout {
    /// Resolves the paths of `options` below `state_directory`, which is
    /// [`loom_core::state_dir`] for the shipped binaries.
    ///
    /// An explicit `--persistence` always wins. Without one, a bind address with
    /// a stable identity uses
    /// `<state_directory>/<instance-name or bind-key>/state.db`, so two servers
    /// on different ports or hosts never share state. Port 0 keeps the
    /// in-memory backend, because an OS-assigned port has no stable identity.
    pub fn resolve(options: &InstanceOptions, state_directory: &Path) -> Result<Self, LoomError> {
        let directory = loom_core::instance_dir(
            state_directory,
            options.bind,
            options.instance_name.as_deref(),
        )?;
        let state_db = match (&options.persistence, &directory) {
            (Some(path), _) => Some(path.clone()),
            (None, Some(directory)) => Some(directory.join(STATE_DB_FILE)),
            (None, None) => None,
        };
        let token_file = options
            .token_file
            .clone()
            .unwrap_or_else(|| match &directory {
                Some(directory) => directory.join(TOKEN_FILE),
                None => loom_core::token_path(state_directory),
            });
        Ok(Self {
            directory,
            state_db,
            token_file,
        })
    }

    /// The instance directory, when the bind address has a stable identity.
    pub fn directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }

    /// The durable state database, or `None` for the in-memory backend.
    pub fn state_db(&self) -> Option<&Path> {
        self.state_db.as_deref()
    }

    /// The file the default bearer token is read from or generated into.
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }

    /// Creates the owner-only directories this instance owns below the state
    /// directory: the instance directory when the default database uses it, and
    /// the parent of the default token file. Both hold transcripts, session
    /// roots, cached clones, and credential references, so they are created with
    /// mode `0700`.
    ///
    /// A path the operator named with `--token-file` is deliberately left alone.
    /// Its directory belongs to the deployment, so Loom neither creates nor
    /// tightens it; a missing file is reported as an unreadable token instead.
    pub fn prepare_directories(&self, options: &InstanceOptions) -> Result<(), LoomError> {
        let default_database_directory = match (&options.persistence, &self.directory) {
            (None, Some(directory)) => Some(directory.clone()),
            _ => None,
        };
        if let Some(directory) = default_database_directory.as_deref() {
            create_private_directory(directory)?;
        }
        if options.token.is_none() && options.token_file.is_none() {
            let parent = self.token_file.parent().ok_or_else(|| {
                LoomError::invalid_request(format!(
                    "token file '{}' must have a parent directory",
                    self.token_file.display()
                ))
            })?;
            create_private_directory(parent)?;
        }
        Ok(())
    }

    /// Makes the durable state database openable by this build. An incompatible
    /// database is wiped only when the operator asked for `--reset-state`, so a
    /// defaulted database that meets a newer schema never hard-fails without a
    /// documented recovery flag.
    pub fn prepare_state_database(&self, reset_requested: bool) -> Result<(), LoomError> {
        let Some(path) = self.state_db.as_deref() else {
            if reset_requested {
                return Err(LoomError::invalid_request(
                    "--reset-state needs a durable state database; pass --persistence or a bind \
                     address with a stable port",
                ));
            }
            return Ok(());
        };
        let status = loom_persistence::prepare_database(path, reset_requested)?;
        if status.is_compatible() {
            return Ok(());
        }
        Err(loom_persistence::incompatible_database_error(path, status))
    }
}

/// Where the bearer token came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenSource {
    /// `--token`, taken from the operator's command line.
    Flag,
    /// A file the operator named with `--token-file`.
    File(PathBuf),
    /// The default token file already held a token.
    ReusedFile(PathBuf),
    /// The default token file was absent or empty, so a new token was written.
    GeneratedFile(PathBuf),
}

/// The bearer token one server accepts, and where it came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedToken {
    token: String,
    source: TokenSource,
}

impl ResolvedToken {
    /// The token clients must present.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn source(&self) -> &TokenSource {
        &self.source
    }

    /// Logs where the token came from, naming the path and never the value.
    /// Generating one is a warning: a missing token file must not silently mint
    /// a new token and lock out the clients that already hold the old one.
    pub fn log_source(&self) {
        match &self.source {
            TokenSource::Flag => log::info!("Using the bearer token from --token"),
            TokenSource::File(path) => {
                log::info!("Using the bearer token from '{}'", path.display());
            }
            TokenSource::ReusedFile(path) => {
                log::info!("Reusing the bearer token in '{}'", path.display());
            }
            TokenSource::GeneratedFile(path) => log::warn!(
                "Generated a new bearer token in '{}'; clients that hold an older token must be \
                 given this one",
                path.display()
            ),
        }
    }

    /// The value to print on a terminal, which is only worth printing for a
    /// freshly generated token: an existing one is already known to the
    /// operator, and echoing it would copy a secret into scrollback.
    pub fn printable(&self) -> Option<&str> {
        match self.source {
            TokenSource::GeneratedFile(_) => Some(self.token.as_str()),
            _ => None,
        }
    }

    /// Prints a freshly generated token on stdout, and only when stdout is a
    /// terminal: an operator watching the start sees the token, while a service
    /// manager, `journald`, or `docker logs` only keeps the logged path.
    pub fn print_generated(&self) {
        if !io::stdout().is_terminal() {
            return;
        }
        if let Some(token) = self.printable() {
            println!("Bearer token: {token}");
        }
    }
}

/// Decides the bearer token this server accepts.
///
/// `--token` and `--token-file` are explicit overrides and stay mutually
/// exclusive. Without either, the token is read from `token_file` when it holds
/// one, and otherwise generated as `loom-<uuid>` and written atomically with
/// mode `0600`, mirroring the credential store write. Generation is
/// unconditional: there is no separate flag and no loopback gating.
pub fn resolve_token(
    options: &InstanceOptions,
    token_file: &Path,
) -> Result<ResolvedToken, LoomError> {
    match (&options.token, &options.token_file) {
        (Some(_), Some(_)) => Err(LoomError::invalid_request(
            "--token and --token-file are mutually exclusive; provide exactly one",
        )),
        (Some(token), None) => {
            let token = token.trim();
            if token.is_empty() {
                return Err(LoomError::invalid_request("--token must not be empty"));
            }
            Ok(ResolvedToken {
                token: token.to_owned(),
                source: TokenSource::Flag,
            })
        }
        (None, Some(path)) => Ok(ResolvedToken {
            token: read_explicit_token_file(path)?,
            source: TokenSource::File(path.clone()),
        }),
        (None, None) => match read_default_token_file(token_file)? {
            Some(token) => Ok(ResolvedToken {
                token,
                source: TokenSource::ReusedFile(token_file.to_path_buf()),
            }),
            None => {
                let token = AuthTokenStore::generate_token();
                write_token_file(token_file, &token)?;
                Ok(ResolvedToken {
                    token,
                    source: TokenSource::GeneratedFile(token_file.to_path_buf()),
                })
            }
        },
    }
}

/// Everything one standalone server needs before it opens the backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instance {
    layout: InstanceLayout,
    token: ResolvedToken,
}

impl Instance {
    /// Resolves the layout, creates the directories it writes into, prepares the
    /// durable database, and resolves the bearer token.
    pub fn prepare(options: &InstanceOptions, state_directory: &Path) -> Result<Self, LoomError> {
        let layout = InstanceLayout::resolve(options, state_directory)?;
        layout.prepare_directories(options)?;
        layout.prepare_state_database(options.reset_state)?;
        let token = resolve_token(options, layout.token_file())?;
        Ok(Self { layout, token })
    }

    pub fn layout(&self) -> &InstanceLayout {
        &self.layout
    }

    pub fn token(&self) -> &ResolvedToken {
        &self.token
    }

    /// The durable state database to open, or `None` for the in-memory backend.
    pub fn state_db(&self) -> Option<&Path> {
        self.layout.state_db()
    }
}

/// Reads a token file the operator named with `--token-file`. A missing or empty
/// file is an error, because the operator asked for that exact file.
fn read_explicit_token_file(path: &Path) -> Result<String, LoomError> {
    let contents = fs::read_to_string(path).map_err(|error| {
        LoomError::invalid_request(format!(
            "could not read token file '{}': {error}",
            path.display()
        ))
    })?;
    let token = contents.trim();
    if token.is_empty() {
        return Err(LoomError::invalid_request(format!(
            "token file '{}' is empty",
            path.display()
        )));
    }
    Ok(token.to_owned())
}

/// Reads the default token file when it holds a token. An absent or empty file
/// means a first start, which generates a token instead of failing.
fn read_default_token_file(path: &Path) -> Result<Option<String>, LoomError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(token_error(format!(
                "could not read the bearer token file '{}': {error}",
                path.display()
            )));
        }
    };
    let token = contents.trim();
    Ok((!token.is_empty()).then(|| token.to_owned()))
}

/// Writes a generated token the way the credential store writes its file: a
/// temporary file next to the target, an explicit owner-only mode, then a rename
/// so a reader never sees a partial token.
fn write_token_file(path: &Path, token: &str) -> Result<(), LoomError> {
    let parent = path.parent().ok_or_else(|| {
        LoomError::invalid_request(format!(
            "token file '{}' must have a parent directory",
            path.display()
        ))
    })?;
    create_private_directory(parent)?;
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut file = fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        file.mode(0o600);
    }
    file.open(&temporary)
        .and_then(|mut file| file.write_all(format!("{token}\n").as_bytes()))
        .map_err(|error| write_token_error(&temporary, error))?;
    restrict_token_file(&temporary)?;
    fs::rename(&temporary, path).map_err(|error| write_token_error(path, error))?;
    restrict_token_file(path)
}

/// Restricts a token file to its owner, which is the mode the credential store
/// uses for the same class of secret.
fn restrict_token_file(path: &Path) -> Result<(), LoomError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| write_token_error(path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), LoomError> {
    loom_core::create_private_dir(path).map_err(|error| {
        LoomError::new(
            ErrorCode::Persistence,
            format!(
                "could not create the state directory '{}': {error}",
                path.display()
            ),
            false,
        )
    })
}

fn write_token_error(path: &Path, error: impl std::fmt::Display) -> LoomError {
    token_error(format!(
        "could not write the bearer token file '{}': {error}",
        path.display()
    ))
}

fn token_error(message: String) -> LoomError {
    LoomError::new(ErrorCode::Persistence, message, false)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use loom_core::RunId;

    use super::{Instance, InstanceLayout, InstanceOptions, TokenSource, resolve_token};

    fn temporary_directory(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("loom-server-instance-{label}-{}", RunId::new()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn options(bind: &str) -> InstanceOptions {
        InstanceOptions::new(bind.parse().unwrap())
    }

    fn mode(path: &Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            0
        }
    }

    #[test]
    fn layout_defaults_to_one_directory_per_bind_address() {
        let state = PathBuf::from("/state");
        let first = InstanceLayout::resolve(&options("127.0.0.1:8765"), &state).unwrap();
        assert_eq!(
            first.directory(),
            Some(state.join("127.0.0.1_8765").as_path())
        );
        assert_eq!(
            first.state_db(),
            Some(state.join("127.0.0.1_8765").join("state.db").as_path())
        );
        assert_eq!(
            first.token_file(),
            state.join("127.0.0.1_8765").join("token")
        );

        // A different port or host is a different instance.
        let other_port = InstanceLayout::resolve(&options("127.0.0.1:8766"), &state).unwrap();
        assert_ne!(other_port.directory(), first.directory());
        assert_ne!(other_port.state_db(), first.state_db());
        let other_host = InstanceLayout::resolve(&options("0.0.0.0:8765"), &state).unwrap();
        assert_ne!(other_host.directory(), first.directory());

        // --instance-name pins a stable directory for a host-aliased bind.
        let mut pinned = options("0.0.0.0:8765");
        pinned.instance_name = Some("worker".to_owned());
        let pinned = InstanceLayout::resolve(&pinned, &state).unwrap();
        assert_eq!(pinned.directory(), Some(state.join("worker").as_path()));
        assert_eq!(
            pinned.state_db(),
            Some(state.join("worker").join("state.db").as_path())
        );
    }

    #[test]
    fn explicit_persistence_and_token_paths_win_over_the_defaults() {
        let state = PathBuf::from("/state");
        let mut explicit = options("127.0.0.1:8765");
        explicit.persistence = Some(PathBuf::from("/var/lib/loom/loom.db"));
        explicit.token_file = Some(PathBuf::from("/etc/loom/token"));
        let layout = InstanceLayout::resolve(&explicit, &state).unwrap();
        assert_eq!(layout.state_db(), Some(Path::new("/var/lib/loom/loom.db")));
        assert_eq!(layout.token_file(), Path::new("/etc/loom/token"));
        // The instance directory still describes this bind address; an explicit
        // path only overrides what it names.
        assert_eq!(
            layout.directory(),
            Some(state.join("127.0.0.1_8765").as_path())
        );
    }

    #[test]
    fn ephemeral_ports_keep_the_in_memory_backend_and_a_fallback_token_path() {
        let state = PathBuf::from("/state");
        let layout = InstanceLayout::resolve(&options("127.0.0.1:0"), &state).unwrap();
        assert_eq!(layout.directory(), None);
        assert_eq!(layout.state_db(), None);
        assert_eq!(layout.token_file(), state.join("token"));
        assert!(layout.prepare_state_database(false).is_ok());
        // Resetting state needs something to reset.
        let error = layout.prepare_state_database(true).unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
        assert!(error.message.contains("--reset-state"), "{error}");
    }

    #[test]
    fn invalid_instance_names_are_rejected() {
        let mut with_name = options("127.0.0.1:8765");
        with_name.instance_name = Some("../escape".to_owned());
        let error = InstanceLayout::resolve(&with_name, Path::new("/state")).unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
    }

    #[test]
    fn a_defaulted_instance_generates_once_then_reuses_its_token() {
        let state = temporary_directory("generate");
        let options = options("127.0.0.1:8765");
        let first = Instance::prepare(&options, &state).unwrap();
        let token_file = state.join("127.0.0.1_8765").join("token");
        assert_eq!(first.layout().token_file(), token_file.as_path());
        assert_eq!(
            first.state_db(),
            Some(state.join("127.0.0.1_8765").join("state.db").as_path())
        );
        assert!(matches!(
            first.token().source(),
            TokenSource::GeneratedFile(path) if path == &token_file
        ));
        assert!(first.token().token().starts_with("loom-"));
        assert_eq!(first.token().printable(), Some(first.token().token()));
        assert_eq!(
            fs::read_to_string(&token_file).unwrap().trim(),
            first.token().token()
        );
        assert_eq!(mode(&token_file), 0o600, "token file mode");
        assert_eq!(
            mode(&state.join("127.0.0.1_8765")),
            0o700,
            "instance directory mode"
        );

        // A second start on the same bind reuses the token instead of minting a
        // new one, and keeps the same database path.
        let second = Instance::prepare(&options, &state).unwrap();
        assert!(matches!(
            second.token().source(),
            TokenSource::ReusedFile(_)
        ));
        assert_eq!(second.token().token(), first.token().token());
        assert_eq!(second.token().printable(), None);
        assert_eq!(second.state_db(), first.state_db());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn an_explicit_token_never_touches_the_state_directory() {
        let state = temporary_directory("explicit-token");
        let mut options = options("127.0.0.1:8765");
        options.token = Some("  flag-token  ".to_owned());
        let instance = Instance::prepare(&options, &state).unwrap();
        assert_eq!(instance.token().token(), "flag-token");
        assert_eq!(instance.token().source(), &TokenSource::Flag);
        assert_eq!(instance.token().printable(), None);
        // The default database directory is created, but nothing writes a token
        // file when the operator passed --token.
        assert_ne!(instance.state_db(), None);
        assert!(!state.join("127.0.0.1_8765").join("token").exists());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn an_operator_named_token_file_is_never_created_for() {
        let state = temporary_directory("named-token");
        let token_file = state.join("deployment").join("token");
        let mut options = options("127.0.0.1:8765");
        options.token_file = Some(token_file.clone());
        let layout = InstanceLayout::resolve(&options, &state).unwrap();

        // Only Loom's own instance directory is created. The directory of a
        // file the operator named belongs to the deployment, so a missing token
        // is reported as unreadable instead of being created here, which would
        // also have to work when the deployment directory is not writable.
        layout.prepare_directories(&options).unwrap();
        assert_eq!(
            layout.directory(),
            Some(state.join("127.0.0.1_8765").as_path())
        );
        assert!(
            !token_file.parent().unwrap().exists(),
            "an operator-named token directory must not be created"
        );
        let error = resolve_token(&options, layout.token_file()).unwrap_err();
        assert!(
            error.message.contains("could not read token file"),
            "{error}"
        );
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn token_resolution_prefers_the_explicit_flags_and_rejects_both() {
        let path = temporary_directory("flags").join("token");
        let mut both = options("127.0.0.1:8765");
        both.token = Some("flag-token".to_owned());
        both.token_file = Some(path.clone());
        let error = resolve_token(&both, &path).unwrap_err();
        assert!(error.message.contains("mutually exclusive"), "{error}");

        let mut empty = options("127.0.0.1:8765");
        empty.token = Some("   ".to_owned());
        let error = resolve_token(&empty, &path).unwrap_err();
        assert!(error.message.contains("must not be empty"), "{error}");

        fs::write(&path, "  file-secret\n").unwrap();
        let mut from_file = options("127.0.0.1:8765");
        from_file.token_file = Some(path.clone());
        let resolved = resolve_token(&from_file, &path).unwrap();
        assert_eq!(resolved.token(), "file-secret");
        assert_eq!(resolved.source(), &TokenSource::File(path.clone()));

        // An explicit token file must exist and hold something.
        fs::write(&path, "\n  \n").unwrap();
        let error = resolve_token(&from_file, &path).unwrap_err();
        assert!(error.message.contains("is empty"), "{error}");
        fs::remove_file(&path).unwrap();
        let error = resolve_token(&from_file, &path).unwrap_err();
        assert!(
            error.message.contains("could not read token file"),
            "{error}"
        );
    }

    #[test]
    fn an_empty_default_token_file_is_regenerated() {
        let state = temporary_directory("empty-token");
        let token_file = state.join("token");
        fs::write(&token_file, "\n").unwrap();
        let options = options("127.0.0.1:0");
        let resolved = resolve_token(&options, &token_file).unwrap();
        assert!(matches!(
            resolved.source(),
            TokenSource::GeneratedFile(path) if path == &token_file
        ));
        assert_eq!(mode(&token_file), 0o600, "token file mode");
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn an_incompatible_state_database_needs_reset_state() {
        let state = temporary_directory("reset");
        let options = options("127.0.0.1:8765");
        let layout = InstanceLayout::resolve(&options, &state).unwrap();
        layout.prepare_directories(&options).unwrap();
        let state_db = layout.state_db().unwrap().to_path_buf();
        fs::write(
            &state_db,
            br#"{"schema_version":1,"state":{"broken":true}}"#,
        )
        .unwrap();

        let error = InstanceLayout::resolve(&options, &state)
            .unwrap()
            .prepare_state_database(false)
            .unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::MalformedPayload);
        assert!(error.message.contains("--reset-state"), "{error}");
        assert!(state_db.exists(), "a refused database is left alone");

        InstanceLayout::resolve(&options, &state)
            .unwrap()
            .prepare_state_database(true)
            .unwrap();
        assert!(!state_db.exists(), "reset wipes the database");
        assert!(
            state
                .join("127.0.0.1_8765")
                .join("state.db.loom-owner.lock")
                .exists(),
            "the owner lock is created next to the database"
        );
        fs::remove_dir_all(&state).unwrap();
    }
}
