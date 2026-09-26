use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use loom_core::{ErrorCode, LoomError, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

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

    pub fn load_section<T: DeserializeOwned>(
        &self,
        section: &str,
        expected_version: u32,
    ) -> Result<Option<T>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT schema_version, payload FROM sections WHERE name = ?1",
                [section],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read section '{section}': {error}"), true)
            })?;
        let Some((schema_version, payload)) = row else {
            return Ok(None);
        };
        if schema_version != expected_version {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "unsupported persistence schema version {schema_version} (expected {expected_version})"
                ),
                false,
            ));
        }
        serde_json::from_slice(&payload).map(Some).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persistence section '{section}' contains malformed JSON: {error}"),
                false,
            )
        })
    }

    pub fn save_sections(&self, schema_version: u32, sections: &[(&str, Value)]) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin persistence transaction: {error}"),
                true,
            )
        })?;
        for (name, value) in sections {
            let payload = serde_json::to_vec(value).map_err(|error| {
                persistence_error(
                    format!("could not serialize persistence section '{name}': {error}"),
                    false,
                )
            })?;
            transaction.execute(
                "INSERT INTO sections (name, schema_version, payload) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET schema_version = excluded.schema_version, payload = excluded.payload",
                params![name, schema_version, payload],
            ).map_err(|error| persistence_error(format!("could not write persistence section '{name}': {error}"), true))?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence transaction: {error}"),
                true,
            )
        })
    }

    fn connection(&self) -> Result<Connection> {
        Connection::open(&self.path).map_err(|error| {
            persistence_error(
                format!("could not open persistence database: {error}"),
                true,
            )
        })
    }

    fn connection_for_write(&self) -> Result<Connection> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                persistence_error(
                    format!(
                        "could not create persistence directory '{}': {error}",
                        parent.display()
                    ),
                    true,
                )
            })?;
        }
        let connection = self.connection()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS sections (
                 name TEXT PRIMARY KEY NOT NULL,
                 schema_version INTEGER NOT NULL,
                 payload BLOB NOT NULL
             );",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not initialize persistence database: {error}"),
                    true,
                )
            })?;
        Ok(connection)
    }
}

fn persistence_error(message: String, retryable: bool) -> LoomError {
    LoomError::new(ErrorCode::Persistence, message, retryable)
}

pub type DurableStore = FilePersistence;

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
    use uuid::Uuid;

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Fixture {
        value: String,
    }

    #[test]
    fn file_store_round_trips_section_data_atomically() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[("state", serde_json::json!({"value": "durable"}))],
            )
            .unwrap();
        assert_eq!(
            store
                .load_section::<Fixture>("state", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap(),
            Fixture {
                value: "durable".to_owned()
            }
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn non_sqlite_file_is_rejected_without_migration() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        fs::write(&path, b"{not json").unwrap();
        let store = FilePersistence::open(&path).unwrap();
        let error = store
            .save_sections(CURRENT_SCHEMA_VERSION, &[("state", serde_json::json!({}))])
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Persistence);
        assert!(!path.with_extension("json.legacy").exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_store_reports_missing_schema_and_malformed_sections() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        assert!(
            store
                .load_section::<Fixture>("missing", 1)
                .unwrap()
                .is_none()
        );
        assert!(!store.exists());
        store
            .save_sections(1, &[("state", serde_json::json!({"value": "old"}))])
            .unwrap();
        assert_eq!(
            store.load_section::<Fixture>("state", 2).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE sections SET payload = ?1 WHERE name = 'state'",
                [b"invalid json".as_slice()],
            )
            .unwrap();
        assert_eq!(
            store.load_section::<Fixture>("state", 1).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn memory_store_round_trips_and_rejects_malformed_values() {
        let store = MemoryPersistence::default();
        assert!(store.load::<Fixture>().unwrap().is_none());
        store
            .save(&Fixture {
                value: "memory".to_owned(),
            })
            .unwrap();
        assert_eq!(
            store.load::<Fixture>().unwrap(),
            Some(Fixture {
                value: "memory".to_owned(),
            })
        );
        store.set_raw(serde_json::json!({"wrong": true})).unwrap();
        assert_eq!(
            store.load::<Fixture>().unwrap_err().code,
            ErrorCode::MalformedPayload
        );
    }

    #[test]
    fn persistence_rejects_empty_paths() {
        assert_eq!(
            FilePersistence::open("").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn sections_are_updated_without_rewriting_other_sections() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[
                    ("sessions", serde_json::json!({"count": 1})),
                    ("journal", serde_json::json!({"events": 3})),
                ],
            )
            .unwrap();
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[("sessions", serde_json::json!({"count": 2}))],
            )
            .unwrap();
        assert_eq!(
            store
                .load_section::<serde_json::Value>("journal", CURRENT_SCHEMA_VERSION)
                .unwrap(),
            Some(serde_json::json!({"events": 3}))
        );
        assert_eq!(
            store
                .load_section::<serde_json::Value>("sessions", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap()["count"],
            2
        );
        fs::remove_file(path).unwrap();
    }
}
