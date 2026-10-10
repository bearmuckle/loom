use super::*;

pub trait CredentialStore: Send + Sync {
    fn resolve(&self, reference: &CredentialRef) -> Result<String>;

    fn store(&self, _reference: &CredentialRef, _secret: String) -> Result<()> {
        Err(LoomError::new(
            ErrorCode::InvalidState,
            "credential store does not support updates",
            false,
        ))
    }
}

#[derive(Clone)]
pub struct FileCredentialStore {
    pub path: PathBuf,
    pub values: Arc<Mutex<BTreeMap<String, String>>>,
}

impl fmt::Debug for FileCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .field(
                "credential_count",
                &self.values.lock().map(|values| values.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl FileCredentialStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let values = Self::read_values(&path)?;
        Ok(Self {
            path,
            values: Arc::new(Mutex::new(values)),
        })
    }

    pub fn default_path() -> PathBuf {
        loom_core::config_dir().join("credentials.json")
    }

    pub fn insert(
        &self,
        reference: impl Into<CredentialRef>,
        secret: impl Into<String>,
    ) -> Result<()> {
        let mut values = self.values.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "credential store lock was poisoned",
                true,
            )
        })?;
        let reference = reference.into();
        values.insert(reference.as_str().to_owned(), secret.into());
        self.persist(&values)
    }

    pub fn remove(&self, reference: &CredentialRef) -> Result<bool> {
        let mut values = self.values.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "credential store lock was poisoned",
                true,
            )
        })?;
        let removed = values.remove(reference.as_str()).is_some();
        if removed {
            self.persist(&values)?;
        }
        Ok(removed)
    }

    pub fn persist(&self, values: &BTreeMap<String, String>) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            LoomError::invalid_request("credential store path must have a parent directory")
        })?;
        fs::create_dir_all(parent).map_err(|error| credential_store_error(&self.path, error))?;
        let temporary = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(values).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode credential store: {error}"),
                false,
            )
        })?;
        let mut file = fs::OpenOptions::new();
        file.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            file.mode(0o600);
        }
        file.open(&temporary)
            .and_then(|mut file| file.write_all(&bytes))
            .map_err(|error| credential_store_error(&temporary, error))?;
        restrict_file_permissions(&temporary)?;
        fs::rename(&temporary, &self.path)
            .map_err(|error| credential_store_error(&self.path, error))?;
        restrict_file_permissions(&self.path)?;
        Ok(())
    }

    pub fn read_values(path: &Path) -> Result<BTreeMap<String, String>> {
        if !path.is_file() {
            return Ok(BTreeMap::new());
        }
        let bytes = fs::read(path).map_err(|error| credential_store_error(path, error))?;
        serde_json::from_slice(&bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "credential store '{}' contains invalid JSON: {error}",
                    path.display()
                ),
                false,
            )
        })
    }
}

impl CredentialStore for FileCredentialStore {
    fn resolve(&self, reference: &CredentialRef) -> Result<String> {
        let mut values = self.values.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "credential store lock was poisoned",
                true,
            )
        })?;
        *values = Self::read_values(&self.path)?;
        values.get(reference.as_str()).cloned().ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderAuthentication,
                format!(
                    "credential reference '{}' was not found",
                    reference.as_str()
                ),
                false,
            )
        })
    }

    fn store(&self, reference: &CredentialRef, secret: String) -> Result<()> {
        self.insert(reference.clone(), secret)
    }
}

pub fn credential_store_error(path: &Path, error: impl fmt::Display) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!(
            "could not access credential store '{}': {error}",
            path.display()
        ),
        false,
    )
}

pub fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| credential_store_error(path, error))?;
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    pub values: Arc<Mutex<BTreeMap<String, String>>>,
}

impl fmt::Debug for InMemoryCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryCredentialStore")
            .field(
                "credential_count",
                &self.values.lock().map(|values| values.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl InMemoryCredentialStore {
    pub fn insert(&self, reference: impl Into<CredentialRef>, secret: impl Into<String>) {
        if let Ok(mut values) = self.values.lock() {
            let reference = reference.into();
            values.insert(reference.as_str().to_owned(), secret.into());
        }
    }

    pub fn remove(&self, reference: &CredentialRef) {
        if let Ok(mut values) = self.values.lock() {
            values.remove(reference.as_str());
        }
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn resolve(&self, reference: &CredentialRef) -> Result<String> {
        self.values
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "credential store lock was poisoned",
                    true,
                )
            })?
            .get(reference.as_str())
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    format!(
                        "credential reference '{}' was not found",
                        reference.as_str()
                    ),
                    false,
                )
            })
    }

    fn store(&self, reference: &CredentialRef, secret: String) -> Result<()> {
        self.values
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "credential store lock was poisoned",
                    true,
                )
            })?
            .insert(reference.as_str().to_owned(), secret);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variable that makes a child process of this test write the
    /// resolved default path to the file it names.
    const PROBE_DESTINATION: &str = "LOOM_DEFAULT_CREDENTIAL_PATH_PROBE";

    /// The configuration precedence is process-wide, so the default path is
    /// resolved in a child process with a controlled environment instead of by
    /// mutating this test process.
    const PROBE_TEST: &str = "credential_path_probe_writes_the_default_path";

    #[test]
    fn credential_path_probe_writes_the_default_path() {
        // In a normal test run this test has nothing to do; only the child
        // process spawned by `default_path_follows_the_shared_config_precedence`
        // has the probe destination set.
        let Some(destination) = std::env::var_os(PROBE_DESTINATION) else {
            return;
        };
        fs::write(
            destination,
            FileCredentialStore::default_path().display().to_string(),
        )
        .expect("could not write the probed credential path");
    }

    #[test]
    fn default_path_follows_the_shared_config_precedence() {
        let probe = |environment: &[(&str, &str)]| -> PathBuf {
            let destination = std::env::temp_dir().join(format!(
                "loom-credential-path-probe-{}",
                loom_core::RunId::new()
            ));
            let status = std::process::Command::new(
                std::env::current_exe().expect("the test binary path is known"),
            )
            .arg(PROBE_TEST)
            .env_clear()
            .env(PROBE_DESTINATION, &destination)
            .envs(environment.iter().copied())
            // `env_clear` drops the parent's LLVM profile settings, so a child
            // running under `cargo llvm-cov` writes its own profile data next to
            // its working directory; keep that out of the source tree.
            .current_dir(std::env::temp_dir())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("could not run the probe as a child process");
            assert!(status.success(), "probe exited with {status}");
            let reported = fs::read_to_string(&destination)
                .expect("the probe did not write the resolved path");
            fs::remove_file(&destination).unwrap();
            PathBuf::from(reported)
        };

        // `LOOM_CONFIG_DIR` is used verbatim, and the fallbacks match the
        // shared helper in `loom-core`.
        assert_eq!(
            probe(&[("LOOM_CONFIG_DIR", "/probe/loom-config")]),
            PathBuf::from("/probe/loom-config/credentials.json")
        );
        assert_eq!(
            probe(&[("XDG_CONFIG_HOME", "/probe/xdg")]),
            PathBuf::from("/probe/xdg/loom/credentials.json")
        );
        assert_eq!(
            probe(&[("HOME", "/probe/home")]),
            PathBuf::from("/probe/home/.config/loom/credentials.json")
        );
        assert_eq!(probe(&[]), PathBuf::from(".loom/credentials.json"));

        assert_eq!(
            FileCredentialStore::default_path(),
            loom_core::config_dir().join("credentials.json")
        );
        assert_eq!(
            FileCredentialStore::default_path()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("credentials.json")
        );
    }
}
