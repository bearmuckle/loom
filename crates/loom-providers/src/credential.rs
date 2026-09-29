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
        std::env::var_os("LOOM_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join("loom"))
            })
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".config").join("loom"))
            })
            .unwrap_or_else(|| PathBuf::from(".loom"))
            .join("credentials.json")
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
