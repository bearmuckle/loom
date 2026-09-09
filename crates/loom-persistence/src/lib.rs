use std::{
    fs,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use loom_core::{ErrorCode, LoomError, Result};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct FilePersistence {
    path: PathBuf,
}

impl FilePersistence {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open(path)
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(LoomError::invalid_request(
                "persistence path must not be empty",
            ));
        }

        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "could not read persistence file '{}': {error}",
                        self.path.display()
                    ),
                    true,
                ));
            }
        };
        if bytes.is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persistence file '{}' is empty", self.path.display()),
                false,
            ));
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "persistence file '{}' contains malformed JSON: {error}",
                    self.path.display()
                ),
                false,
            )
        })
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not serialize durable state: {error}"),
                false,
            )
        })?;
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "could not create persistence directory '{}': {error}",
                        parent.display()
                    ),
                    true,
                )
            })?;
        }
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
        if let Err(error) = fs::write(&temporary, bytes) {
            let _ = fs::remove_file(&temporary);
            return Err(LoomError::new(
                ErrorCode::Persistence,
                format!("could not write temporary persistence file: {error}"),
                true,
            ));
        }
        if let Err(error) = File::open(&temporary).and_then(|file| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(LoomError::new(
                ErrorCode::Persistence,
                format!("could not flush temporary persistence file: {error}"),
                true,
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&temporary)
                .map_err(|error| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        format!("could not inspect temporary persistence file: {error}"),
                        true,
                    )
                })?
                .permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&temporary, permissions).map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!("could not protect temporary persistence file: {error}"),
                    true,
                )
            })?;
        }
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not atomically replace persistence file '{}': {error}",
                    self.path.display()
                ),
                true,
            ));
        }
        Ok(())
    }

    pub fn load_versioned<T: DeserializeOwned>(&self, expected_version: u32) -> Result<Option<T>> {
        let Some(value) = self.load::<VersionedState<T>>()? else {
            return Ok(None);
        };
        if value.schema_version != expected_version {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "unsupported persistence schema version {} (expected {expected_version})",
                    value.schema_version
                ),
                false,
            ));
        }
        Ok(Some(value.state))
    }

    pub fn save_versioned<T: Serialize>(&self, schema_version: u32, state: &T) -> Result<()> {
        self.save(&VersionedStateRef {
            schema_version,
            state,
        })
    }
}

pub type DurableStore = FilePersistence;

#[derive(Clone, Debug, serde::Deserialize)]
struct VersionedState<T> {
    schema_version: u32,
    state: T,
}

#[derive(serde::Serialize)]
struct VersionedStateRef<'a, T> {
    schema_version: u32,
    state: &'a T,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryPersistence {
    value: Arc<Mutex<Option<Value>>>,
}

impl MemoryPersistence {
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let value = self
            .value
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "memory persistence lock was poisoned",
                    true,
                )
            })?
            .clone();
        value
            .map(|value| {
                serde_json::from_value(value).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("memory persistence contains malformed data: {error}"),
                        false,
                    )
                })
            })
            .transpose()
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        let value = serde_json::to_value(value).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not serialize memory state: {error}"),
                false,
            )
        })?;
        *self.value.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "memory persistence lock was poisoned",
                true,
            )
        })? = Some(value);
        Ok(())
    }

    pub fn load_versioned<T: DeserializeOwned>(&self, expected_version: u32) -> Result<Option<T>> {
        let Some(value) = self.load::<VersionedState<T>>()? else {
            return Ok(None);
        };
        if value.schema_version != expected_version {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "unsupported persistence schema version {} (expected {expected_version})",
                    value.schema_version
                ),
                false,
            ));
        }
        Ok(Some(value.state))
    }

    pub fn save_versioned<T: Serialize>(&self, schema_version: u32, state: &T) -> Result<()> {
        self.save(&VersionedStateRef {
            schema_version,
            state,
        })
    }

    pub fn set_raw(&self, value: Value) -> Result<()> {
        *self.value.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "memory persistence lock was poisoned",
                true,
            )
        })? = Some(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Fixture {
        value: String,
    }

    #[test]
    fn file_store_round_trips_versioned_data_atomically() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.json", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_versioned(
                CURRENT_SCHEMA_VERSION,
                &Fixture {
                    value: "durable".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            store
                .load_versioned::<Fixture>(CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap(),
            Fixture {
                value: "durable".to_owned()
            }
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_data_is_a_structured_error() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.json", Uuid::new_v4()));
        fs::write(&path, b"{not json").unwrap();
        let error = FilePersistence::open(&path)
            .unwrap()
            .load::<Fixture>()
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::MalformedPayload);
        fs::remove_file(path).unwrap();
    }
}
