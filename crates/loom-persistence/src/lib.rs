use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use loom_core::{ErrorCode, LoomError, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const CURRENT_SCHEMA_VERSION: u32 = 3;
const DATABASE_SCHEMA_VERSION: u32 = 3;
const EXTERNAL_STRING_THRESHOLD: usize = 4096;
const MAX_CONTENT_BYTES: usize = 512 * 1024 * 1024;

const DATABASE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS section_meta (
    name TEXT PRIMARY KEY NOT NULL,
    schema_version INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS content_blobs (
    hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32),
    raw_size INTEGER NOT NULL CHECK(raw_size >= 0),
    codec INTEGER NOT NULL CHECK(codec IN (0, 1)),
    payload BLOB NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS state_nodes (
    section TEXT NOT NULL REFERENCES section_meta(name) ON DELETE CASCADE,
    path TEXT NOT NULL,
    node_kind INTEGER NOT NULL CHECK(node_kind BETWEEN 0 AND 3),
    scalar BLOB,
    content_hash BLOB REFERENCES content_blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY(section, path),
    CHECK((node_kind = 2 AND scalar IS NOT NULL AND content_hash IS NULL)
       OR (node_kind = 3 AND scalar IS NULL AND content_hash IS NOT NULL)
       OR (node_kind IN (0, 1) AND scalar IS NULL AND content_hash IS NULL))
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS state_nodes_by_section_kind
    ON state_nodes(section, node_kind);
";

#[derive(Default)]
struct RestoreNode {
    kind: Option<i64>,
    scalar: Option<Vec<u8>>,
    content_hash: Option<Vec<u8>>,
    children: BTreeMap<String, RestoreNode>,
}

fn child_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        format!("/{segment}")
    } else {
        format!("{parent}/{segment}")
    }
}

fn object_segment(key: &str) -> String {
    format!("k{}", key.replace('~', "~0").replace('/', "~1"))
}

fn array_segment(index: usize) -> String {
    format!("a{index}")
}

fn insert_restore_node(
    root: &mut RestoreNode,
    path: &str,
    kind: i64,
    scalar: Option<Vec<u8>>,
    content_hash: Option<Vec<u8>>,
) -> Result<()> {
    let mut current = root;
    if !path.is_empty() {
        for segment in path.trim_start_matches('/').split('/') {
            current = current.children.entry(segment.to_owned()).or_default();
        }
    }
    if current.kind.replace(kind).is_some() || !current.children.is_empty() {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persistence contains duplicate or inconsistent state paths",
            false,
        ));
    }
    current.scalar = scalar;
    current.content_hash = content_hash;
    Ok(())
}

fn unescape_object_segment(segment: &str) -> Option<String> {
    let segment = segment.strip_prefix('k')?;
    let mut decoded = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(character) = chars.next() {
        if character == '~' {
            match chars.next()? {
                '0' => decoded.push('~'),
                '1' => decoded.push('/'),
                _ => return None,
            }
        } else {
            decoded.push(character);
        }
    }
    Some(decoded)
}

fn decode_content(connection: &Connection, hash: &[u8]) -> Result<String> {
    let (raw_size, codec, payload): (i64, i64, Vec<u8>) = connection
        .query_row(
            "SELECT raw_size, codec, payload FROM content_blobs WHERE hash = ?1",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| {
            persistence_error(format!("could not read state content: {error}"), true)
        })?;
    if raw_size < 0 || raw_size > MAX_CONTENT_BYTES as i64 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted state content exceeds the maximum supported size",
            false,
        ));
    }
    let bytes = match codec {
        0 => payload,
        1 => {
            let mut decoded = Vec::with_capacity(raw_size as usize);
            ZlibDecoder::new(payload.as_slice())
                .take((raw_size as u64).saturating_add(1))
                .read_to_end(&mut decoded)
                .map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted state content is malformed: {error}"),
                        false,
                    )
                })?;
            decoded
        }
        _ => {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted state content uses an unsupported codec",
                false,
            ));
        }
    };
    if bytes.len() as i64 != raw_size || Sha256::digest(&bytes).as_slice() != hash {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted state content failed its length or hash check",
            false,
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted state text is not UTF-8: {error}"),
            false,
        )
    })
}

impl RestoreNode {
    fn into_value(self, connection: &Connection, path: &str) -> Result<Value> {
        match self.kind.ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persistence state path '{path}' has no value"),
                false,
            )
        })? {
            0 => {
                let mut object = serde_json::Map::new();
                for (segment, child) in self.children {
                    let key = unescape_object_segment(&segment).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "persistence contains an invalid object key",
                            false,
                        )
                    })?;
                    object.insert(
                        key,
                        child.into_value(connection, &child_path(path, &segment))?,
                    );
                }
                Ok(Value::Object(object))
            }
            1 => {
                let mut indexed = Vec::with_capacity(self.children.len());
                for (segment, child) in self.children {
                    let index = segment
                        .strip_prefix('a')
                        .and_then(|value| value.parse::<usize>().ok())
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                "persistence contains an invalid array index",
                                false,
                            )
                        })?;
                    indexed.push((
                        index,
                        child.into_value(connection, &child_path(path, &segment))?,
                    ));
                }
                indexed.sort_by_key(|(index, _)| *index);
                if indexed
                    .iter()
                    .enumerate()
                    .any(|(expected, (actual, _))| expected != *actual)
                {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persistence contains a sparse array",
                        false,
                    ));
                }
                Ok(Value::Array(
                    indexed.into_iter().map(|(_, value)| value).collect(),
                ))
            }
            2 => self
                .scalar
                .as_deref()
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persistence scalar payload is missing",
                        false,
                    )
                })
                .and_then(|payload| {
                    serde_json::from_slice(payload).map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("persistence contains malformed JSON data: {error}"),
                            false,
                        )
                    })
                }),
            3 => {
                let hash = self.content_hash.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persistence content reference is missing",
                        false,
                    )
                })?;
                Ok(Value::String(decode_content(connection, &hash)?))
            }
            _ => Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persistence contains an unknown node kind",
                false,
            )),
        }
    }
}

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
                "SELECT schema_version FROM section_meta WHERE name = ?1",
                [section],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read section '{section}': {error}"), true)
            })?;
        let Some(schema_version) = row else {
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
        let mut root = RestoreNode::default();
        {
            let mut statement = connection
                .prepare(
                    "SELECT path, node_kind, scalar, content_hash
                     FROM state_nodes WHERE section = ?1 ORDER BY path",
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prepare section '{section}': {error}"),
                        true,
                    )
                })?;
            let rows = statement
                .query_map([section], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read section '{section}': {error}"), true)
                })?;
            for row in rows {
                let (path, kind, scalar, content_hash) = row.map_err(|error| {
                    persistence_error(format!("could not read section '{section}': {error}"), true)
                })?;
                insert_restore_node(&mut root, &path, kind, scalar, content_hash)?;
            }
        }
        root.into_value(&connection, "")
            .and_then(|value| {
                serde_json::from_value(value).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persistence section '{section}' has invalid data: {error}"),
                        false,
                    )
                })
            })
            .map(Some)
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
            save_section_nodes(&transaction, name, schema_version, value)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence transaction: {error}"),
                true,
            )
        })
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path).map_err(|error| {
            persistence_error(
                format!("could not open persistence database: {error}"),
                true,
            )
        })?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| {
                persistence_error(format!("could not configure persistence: {error}"), true)
            })?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
            .map_err(|error| {
                persistence_error(format!("could not configure persistence: {error}"), true)
            })?;
        initialize_schema(&connection)?;
        Ok(connection)
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
        self.connection()
    }
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    let database_version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| {
            persistence_error(
                format!("could not inspect persistence schema: {error}"),
                true,
            )
        })?;
    if database_version == DATABASE_SCHEMA_VERSION {
        return Ok(());
    }
    if database_version != 0 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("unsupported persistence database version {database_version}"),
            false,
        ));
    }

    // Inspect before changing persistent SQLite settings. In particular, opening
    // a database from the old section format must not even switch its journal
    // mode; this release deliberately starts with an empty database only.
    let has_user_tables: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
            )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not inspect persistence database: {error}"),
                true,
            )
        })?;
    if has_user_tables {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "this database uses an unsupported persistence format; this version starts with a new database and does not import or modify existing state",
            false,
        ));
    }

    connection
        .execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|error| {
            persistence_error(
                format!("could not initialize persistence database: {error}"),
                true,
            )
        })?;

    let transaction = connection.unchecked_transaction().map_err(|error| {
        persistence_error(
            format!("could not initialize persistence schema: {error}"),
            true,
        )
    })?;
    transaction
        .execute_batch(DATABASE_SCHEMA)
        .map_err(|error| {
            persistence_error(
                format!("could not create persistence schema: {error}"),
                true,
            )
        })?;
    transaction
        .pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
        .map_err(|error| {
            persistence_error(
                format!("could not record persistence schema version: {error}"),
                true,
            )
        })?;
    transaction.commit().map_err(|error| {
        persistence_error(
            format!("could not commit persistence schema: {error}"),
            true,
        )
    })?;
    Ok(())
}

fn save_section_nodes(
    transaction: &Transaction<'_>,
    name: &str,
    schema_version: u32,
    value: &Value,
) -> Result<()> {
    if name.is_empty() {
        return Err(LoomError::invalid_request(
            "persistence section name must not be empty",
        ));
    }
    transaction
        .execute(
            "INSERT INTO section_meta(name, schema_version) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET schema_version=excluded.schema_version",
            params![name, schema_version],
        )
        .map_err(|error| {
            persistence_error(format!("could not write section '{name}': {error}"), true)
        })?;
    transaction
        .execute_batch("CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_nodes (path TEXT PRIMARY KEY) WITHOUT ROWID;")
        .map_err(|error| persistence_error(format!("could not stage section '{name}': {error}"), true))?;
    transaction
        .execute("DELETE FROM _loom_wanted_nodes", [])
        .map_err(|error| {
            persistence_error(format!("could not stage section '{name}': {error}"), true)
        })?;

    let mut nodes = Vec::new();
    flatten_value(value, "", &mut nodes)?;
    for (path, kind, scalar, content) in nodes {
        let content_hash = match content {
            Some(content) => Some(store_content(transaction, &content)?),
            None => None,
        };
        transaction
            .execute("INSERT INTO _loom_wanted_nodes(path) VALUES (?1)", [&path])
            .map_err(|error| {
                persistence_error(format!("could not stage section '{name}': {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO state_nodes(section, path, node_kind, scalar, content_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(section, path) DO UPDATE SET
                    node_kind=excluded.node_kind, scalar=excluded.scalar, content_hash=excluded.content_hash
                 WHERE state_nodes.node_kind IS NOT excluded.node_kind
                    OR state_nodes.scalar IS NOT excluded.scalar
                    OR state_nodes.content_hash IS NOT excluded.content_hash",
                params![name, path, kind, scalar, content_hash],
            )
            .map_err(|error| persistence_error(format!("could not write section '{name}': {error}"), true))?;
    }
    transaction
        .execute(
            "DELETE FROM state_nodes
             WHERE section=?1 AND NOT EXISTS (
                 SELECT 1 FROM _loom_wanted_nodes wanted WHERE wanted.path=state_nodes.path
             )",
            [name],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune section '{name}': {error}"), true)
        })?;
    transaction
        .execute(
            "DELETE FROM content_blobs WHERE NOT EXISTS (
                 SELECT 1 FROM state_nodes WHERE state_nodes.content_hash=content_blobs.hash
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused state content: {error}"),
                true,
            )
        })?;
    Ok(())
}

type EncodedNode = (String, i64, Option<Vec<u8>>, Option<Vec<u8>>);

fn flatten_value(value: &Value, path: &str, nodes: &mut Vec<EncodedNode>) -> Result<()> {
    match value {
        Value::Object(object) => {
            nodes.push((path.to_owned(), 0, None, None));
            for (key, value) in object {
                flatten_value(value, &child_path(path, &object_segment(key)), nodes)?;
            }
        }
        Value::Array(array) => {
            nodes.push((path.to_owned(), 1, None, None));
            for (index, value) in array.iter().enumerate() {
                flatten_value(value, &child_path(path, &array_segment(index)), nodes)?;
            }
        }
        Value::String(value) if value.len() >= EXTERNAL_STRING_THRESHOLD => {
            if value.len() > MAX_CONTENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "state text exceeds the maximum supported size",
                    false,
                ));
            }
            nodes.push((path.to_owned(), 3, None, Some(value.as_bytes().to_vec())));
        }
        _ => {
            let scalar = serde_json::to_vec(value).map_err(|error| {
                persistence_error(
                    format!("could not encode persistence value: {error}"),
                    false,
                )
            })?;
            nodes.push((path.to_owned(), 2, Some(scalar), None));
        }
    }
    Ok(())
}

fn store_content(transaction: &Transaction<'_>, content: &[u8]) -> Result<Vec<u8>> {
    let hash = Sha256::digest(content).to_vec();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(content).map_err(|error| {
        persistence_error(format!("could not compress state content: {error}"), false)
    })?;
    let compressed = encoder.finish().map_err(|error| {
        persistence_error(format!("could not compress state content: {error}"), false)
    })?;
    let (codec, payload) = if compressed.len() < content.len() {
        (1_i64, compressed)
    } else {
        (0_i64, content.to_vec())
    };
    transaction
        .execute(
            "INSERT INTO content_blobs(hash, raw_size, codec, payload) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO NOTHING",
            params![hash, content.len() as i64, codec, payload],
        )
        .map_err(|error| {
            persistence_error(format!("could not store state content: {error}"), true)
        })?;
    Ok(hash)
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
                "UPDATE state_nodes SET scalar = ?1 WHERE section = 'state' AND path = '/kvalue'",
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

    #[test]
    fn section_format_is_rejected_without_importing_or_modifying_it() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let repeated = "large checkpoint content ".repeat(1000);
        let legacy_value = serde_json::json!({
            "sessions": [
                {"id": "first", "checkpoint": repeated},
                {"id": "second", "checkpoint": repeated}
            ],
            "next_sequence": 17
        });
        let connection = Connection::open(&path).unwrap();
        let original_journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sections (
                    name TEXT PRIMARY KEY NOT NULL,
                    schema_version INTEGER NOT NULL,
                    payload BLOB NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sections(name, schema_version, payload) VALUES ('legacy', 2, ?1)",
                [serde_json::to_vec(&legacy_value).unwrap()],
            )
            .unwrap();
        drop(connection);

        let store = FilePersistence::open(&path).unwrap();
        assert_eq!(
            store
                .load_section::<Value>("legacy", CURRENT_SCHEMA_VERSION)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );
        let connection = Connection::open(&path).unwrap();
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, original_journal_mode);
        let raw_payload: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM sections WHERE name='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_payload, serde_json::to_vec(&legacy_value).unwrap());
        let v3_schema_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='state_nodes')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!v3_schema_exists);
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unchanged_tree_nodes_are_not_updated() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let value = serde_json::json!({"session": {"name": "alpha", "sequence": 1}});
        store
            .save_sections(CURRENT_SCHEMA_VERSION, &[("sessions", value.clone())])
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE node_updates(count INTEGER NOT NULL);
                 INSERT INTO node_updates VALUES (0);
                 CREATE TRIGGER count_node_updates AFTER UPDATE ON state_nodes
                 BEGIN UPDATE node_updates SET count = count + 1; END;",
            )
            .unwrap();
        drop(connection);

        store
            .save_sections(CURRENT_SCHEMA_VERSION, &[("sessions", value)])
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        let updates: u32 = connection
            .query_row("SELECT count FROM node_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(updates, 0);
        drop(connection);

        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[(
                    "sessions",
                    serde_json::json!({"session": {"name": "beta", "sequence": 1}}),
                )],
            )
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        let updates: u32 = connection
            .query_row("SELECT count FROM node_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(updates, 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn large_text_is_deduplicated_and_compressed() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let text = "checkpoint content that compresses well ".repeat(2_000);
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[
                    ("one", serde_json::json!({"content": text})),
                    ("two", serde_json::json!({"content": text})),
                ],
            )
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        let (blob_count, codec, payload_size, raw_size): (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), MAX(codec), MAX(length(payload)), MAX(raw_size) FROM content_blobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(blob_count, 1);
        assert_eq!(codec, 1);
        assert!(payload_size < raw_size);
        assert_eq!(
            store
                .load_section::<Value>("two", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap()["content"],
            text
        );
        drop(connection);
        fs::remove_file(path).unwrap();
    }
}
