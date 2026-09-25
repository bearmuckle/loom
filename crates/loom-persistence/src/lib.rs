use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use loom_core::{ErrorCode, LoomError, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

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

    pub fn import_sections_from(&self, source: &Self) -> Result<bool> {
        if !source.path.exists() {
            return Ok(false);
        }
        let source_connection = source.connection()?;
        let mut statement = source_connection
            .prepare("SELECT name, schema_version, payload FROM sections ORDER BY name")
            .map_err(|error| {
                persistence_error(
                    format!("could not read legacy persistence sections: {error}"),
                    true,
                )
            })?;
        let sections = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not read legacy persistence sections: {error}"),
                    true,
                )
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not decode legacy persistence sections: {error}"),
                    true,
                )
            })?;
        if sections.is_empty() {
            return Ok(false);
        }

        let mut destination = self.connection_for_write()?;
        let transaction = destination.transaction().map_err(|error| {
            persistence_error(format!("could not begin persistence import: {error}"), true)
        })?;
        let existing_sections: i64 = transaction
            .query_row("SELECT COUNT(*) FROM sections", [], |row| row.get(0))
            .map_err(|error| {
                persistence_error(
                    format!("could not inspect destination persistence: {error}"),
                    true,
                )
            })?;
        if existing_sections != 0 {
            return Ok(false);
        }
        for (name, schema_version, payload) in sections {
            transaction
                .execute(
                    "INSERT INTO sections (name, schema_version, payload) VALUES (?1, ?2, ?3)",
                    params![name, schema_version, payload],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not import persistence section '{name}': {error}"),
                        true,
                    )
                })?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence import: {error}"),
                true,
            )
        })?;
        Ok(true)
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

    #[test]
    fn legacy_sections_import_only_into_an_empty_destination() {
        let source_path =
            std::env::temp_dir().join(format!("loom-persistence-source-{}.db", Uuid::new_v4()));
        let destination_path = std::env::temp_dir().join(format!(
            "loom-persistence-destination-{}.db",
            Uuid::new_v4()
        ));
        let source = FilePersistence::open(&source_path).unwrap();
        let destination = FilePersistence::open(&destination_path).unwrap();
        source
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[("sessions", serde_json::json!({"count": 3}))],
            )
            .unwrap();

        assert!(destination.import_sections_from(&source).unwrap());
        assert!(!destination.import_sections_from(&source).unwrap());
        assert_eq!(
            destination
                .load_section::<serde_json::Value>("sessions", CURRENT_SCHEMA_VERSION)
                .unwrap(),
            Some(serde_json::json!({"count": 3}))
        );

        let existing_path =
            std::env::temp_dir().join(format!("loom-persistence-existing-{}.db", Uuid::new_v4()));
        let existing = FilePersistence::open(&existing_path).unwrap();
        existing
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[("sessions", serde_json::json!({"count": 7}))],
            )
            .unwrap();
        assert!(!existing.import_sections_from(&source).unwrap());
        assert_eq!(
            existing
                .load_section::<serde_json::Value>("sessions", CURRENT_SCHEMA_VERSION)
                .unwrap(),
            Some(serde_json::json!({"count": 7}))
        );

        for path in [source_path, destination_path, existing_path] {
            fs::remove_file(path).unwrap();
        }
    }
}
