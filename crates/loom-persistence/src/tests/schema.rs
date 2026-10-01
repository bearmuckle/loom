//! Persistence tests: schema.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn sqlite_connections_are_lazy_shared_by_clones_and_observe_other_handles() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let first = FilePersistence::open(&path).unwrap();
    let clone = first.clone();
    assert!(!first.exists());
    assert!(first.load_sessions().unwrap().is_none());
    assert!(!first.exists(), "read-only construction must stay lazy");
    assert!(Arc::ptr_eq(&first.connection, &clone.connection));
    drop((first, clone));
}

#[test]
fn non_sqlite_file_is_rejected_without_migration() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    fs::write(&path, b"{not json").unwrap();
    let store = FilePersistence::open(&path).unwrap();
    let error = store.load_feed_header().unwrap_err();
    assert_eq!(error.code, ErrorCode::Persistence);
    assert!(!path.with_extension("json.legacy").exists());
    fs::remove_file(path).unwrap();
}

#[test]
fn persistence_rejects_empty_paths() {
    assert_eq!(
        FilePersistence::open("").unwrap_err().code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn missing_database_reads_stay_lazy_and_return_empty_state() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let session_id = AgentSessionId::new();
    let run_id = RunId::new();
    assert!(!store.exists());
    assert!(store.load_sessions().unwrap().is_none());
    assert!(store.load_workspaces().unwrap().is_none());
    assert!(store.load_run_messages(run_id).unwrap().is_empty());
    assert!(
        store
            .load_run_message_page(run_id, None, 1)
            .unwrap()
            .is_empty()
    );
    assert!(store.load_filesystem_record(session_id).unwrap().is_none());
    assert!(store.load_feed_state().unwrap().is_none());
    assert!(store.load_feed_header().unwrap().is_none());
    assert!(
        store
            .load_feed_session_cursor(session_id)
            .unwrap()
            .is_none()
    );
    assert!(store.load_feed_events_since(None, None).unwrap().is_empty());
    assert!(!store.exists());
}

#[test]
fn database_with_user_tables_is_rejected_without_importing_or_modifying_it() {
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
        store.load_feed_header().unwrap_err().code,
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
    let typed_schema_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!typed_schema_exists);
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0);
    fs::remove_file(path).unwrap();
}

#[test]
fn previous_database_version_is_rejected_without_schema_changes() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE sentinel(value TEXT); PRAGMA user_version=40;")
        .unwrap();
    let original_journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    drop(connection);

    let store = FilePersistence::open(&path).unwrap();
    assert_eq!(
        store.load_feed_header().unwrap_err().code,
        ErrorCode::MalformedPayload
    );

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    let has_feed_meta: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='feed_session_meta')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 40);
    assert_eq!(journal_mode, original_journal_mode);
    assert!(!has_feed_meta);
    fs::remove_file(path).unwrap();
}

#[test]
fn future_database_version_is_rejected_without_schema_changes() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sentinel(value TEXT);
             INSERT INTO sentinel(value) VALUES ('keep');
             PRAGMA user_version=52;",
        )
        .unwrap();
    let original_journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    drop(connection);

    let store = FilePersistence::open(&path).unwrap();
    assert_eq!(
        store.load_feed_header().unwrap_err().code,
        ErrorCode::MalformedPayload
    );

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    let sentinel: String = connection
        .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
        .unwrap();
    let has_feed_meta: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='feed_session_meta')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 52);
    assert_eq!(journal_mode, original_journal_mode);
    assert_eq!(sentinel, "keep");
    assert!(!has_feed_meta);
    fs::remove_file(path).unwrap();
}

#[test]
fn fresh_database_uses_typed_schema_without_generic_section_tables() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    let generic_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='table' AND name IN ('section_meta', 'state_nodes')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(generic_tables, 0);
    assert_eq!(version, DATABASE_SCHEMA_VERSION);
    drop(connection);
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn baseline_schema_consolidates_project_grants_into_single_json_columns() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, DATABASE_SCHEMA_VERSION);

    let run_columns: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_info('run_runtime_config')
             WHERE name LIKE 'project_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(run_columns, vec!["project_grants".to_owned()]);

    let task_columns: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_info('delegated_tasks')
             WHERE name LIKE 'permission_%' OR name = 'permissions' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(task_columns, vec!["permissions".to_owned()]);

    drop(connection);
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn baseline_schema_folds_transcript_and_tool_state_into_parent_rows() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    let child_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN (
                'run_message_fragments', 'run_message_tool_calls', 'run_tool_attempts'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(child_tables, 0);

    let message_columns: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_info('run_messages')
             WHERE name IN ('tool_calls', 'fragments') ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        message_columns,
        vec!["fragments".to_owned(), "tool_calls".to_owned()]
    );

    let call_columns: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_info('run_tool_calls')
             WHERE name = 'attempts'",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(call_columns, vec!["attempts".to_owned()]);

    drop(connection);
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn opens_and_migrates_an_older_schema_in_place() {
    let path = std::env::temp_dir().join(format!("loom-persistence-migrate-{}.db", Uuid::new_v4()));
    let run_id = RunId::new();
    {
        let connection = Connection::open(&path).unwrap();
        // Minimal v2 layout: `run_messages` without the v3 reasoning column,
        // plus the run tables the v4 direction queue migration touches.
        connection
            .execute_batch(
                "CREATE TABLE run_messages (
                    run_id BLOB NOT NULL,
                    session_id BLOB NOT NULL,
                    ordinal INTEGER NOT NULL,
                    timeline_ordinal INTEGER NOT NULL DEFAULT 0,
                    role TEXT NOT NULL,
                    content_hash BLOB,
                    name TEXT,
                    tool_call_id BLOB,
                    tool_calls TEXT NOT NULL DEFAULT '[]',
                    fragments TEXT NOT NULL DEFAULT '[]',
                    PRIMARY KEY(run_id, ordinal)
                ) WITHOUT ROWID, STRICT;
                CREATE TABLE run_summaries (
                    run_id BLOB NOT NULL,
                    session_id BLOB NOT NULL,
                    PRIMARY KEY(run_id, session_id)
                ) WITHOUT ROWID, STRICT;
                CREATE TABLE run_execution_state (
                    run_id BLOB PRIMARY KEY NOT NULL
                ) WITHOUT ROWID, STRICT;
                PRAGMA user_version = 2;",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO run_messages(
                    run_id, session_id, ordinal, timeline_ordinal, role, tool_calls, fragments)
                 VALUES (?1, ?2, 0, 0, 'assistant', '[]', '[]')",
                params![run_id.as_uuid().as_bytes().as_slice(), [0u8; 16].as_slice()],
            )
            .unwrap();
    }

    let store = FilePersistence::open(&path).unwrap();
    let messages = store.load_run_messages(run_id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].reasoning_content, None);
    drop(store);

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, DATABASE_SCHEMA_VERSION);
    let has_column: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('run_messages')
             WHERE name='reasoning_content')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(has_column, "migration adds the reasoning_content column");
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn schema_status_detects_incompatible_state_and_resets_only_on_request() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    assert_eq!(
        FilePersistence::schema_status(&path).unwrap(),
        SchemaStatus::Absent
    );

    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();
    assert_eq!(
        FilePersistence::schema_status(&path).unwrap(),
        SchemaStatus::Current
    );
    drop(store);

    // Simulate a database written by an older Loom revision.
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", 40u32)
            .unwrap();
    }
    assert_eq!(
        FilePersistence::schema_status(&path).unwrap(),
        SchemaStatus::OtherVersion(40)
    );

    // Inspecting and refusing to wipe must leave the file untouched.
    assert_eq!(
        prepare_database(&path, false).unwrap(),
        SchemaStatus::OtherVersion(40)
    );
    assert!(path.is_file());

    // An explicit wipe removes the database and its sidecars.
    assert_eq!(prepare_database(&path, true).unwrap(), SchemaStatus::Absent);
    assert!(!path.is_file());
    fs::remove_file(&path).ok();
}

#[test]
fn schema_records_baseline_version_and_rejects_future_versions() {
    let dir = std::env::temp_dir().join(format!("loom-schema-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");

    {
        let connection = Connection::open(&path).unwrap();
        initialize_schema(&connection).unwrap();
    }
    assert_eq!(
        FilePersistence::schema_status(&path).unwrap(),
        SchemaStatus::Current
    );

    {
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", 999u32)
            .unwrap();
    }
    assert!(matches!(
        FilePersistence::schema_status(&path).unwrap(),
        SchemaStatus::OtherVersion(999)
    ));
    assert_eq!(prepare_database(&path, true).unwrap(), SchemaStatus::Absent);
    assert_eq!(
        FilePersistence::schema_status(&path).unwrap(),
        SchemaStatus::Absent
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn in_memory_backend_shares_the_sqlite_code_path() {
    let store = FilePersistence::in_memory();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();
    let loaded = store.load_sessions().unwrap();
    assert!(loaded.is_none_or(|state| state.sessions.is_empty()));
}
