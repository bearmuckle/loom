//! Persistence tests: the per-project child model policy.

#[allow(unused_imports)]
use super::support::*;
use super::*;
use loom_core::ProjectChildModelPolicy;
use std::collections::BTreeMap;

/// Records `session_id` as a project root the way the session catalog does.
fn insert_project_root(path: &Path, session_id: AgentSessionId) {
    let connection = Connection::open(path).unwrap();
    let id = session_id.as_uuid().as_bytes().to_vec();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
             VALUES (?1, zeroblob(16), 'project', 'idle', 1, 1)",
            [&id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
             VALUES (?1, ?1, NULL, 1)",
            [&id],
        )
        .unwrap();
}

fn policy() -> ProjectChildModelPolicy {
    ProjectChildModelPolicy {
        default_model_id: Some("deepseek/deepseek-flash".to_owned()),
        by_kind: BTreeMap::from([
            ("vision".to_owned(), "openai/gpt-6-luna".to_owned()),
            ("cheap".to_owned(), "deepseek/deepseek-flash".to_owned()),
        ]),
    }
}

fn fresh_store() -> (std::path::PathBuf, FilePersistence) {
    let path = std::env::temp_dir().join(format!("loom-child-models-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();
    (path, store)
}

#[test]
fn a_child_model_policy_round_trips_and_is_replaced_whole() {
    let (path, store) = fresh_store();
    let root = AgentSessionId::new();
    insert_project_root(&path, root);
    let project_id = ProjectId::from_uuid(*root.as_uuid());

    assert!(
        store
            .load_project_child_model_policy(project_id)
            .unwrap()
            .is_empty()
    );
    store
        .replace_project_child_model_policy(project_id, &policy())
        .unwrap();
    assert_eq!(
        store.load_project_child_model_policy(project_id).unwrap(),
        policy()
    );

    let kinds_only = ProjectChildModelPolicy {
        default_model_id: None,
        by_kind: BTreeMap::from([("review".to_owned(), "a/b".to_owned())]),
    };
    store
        .replace_project_child_model_policy(project_id, &kinds_only)
        .unwrap();
    assert_eq!(
        store.load_project_child_model_policy(project_id).unwrap(),
        kinds_only
    );

    store
        .replace_project_child_model_policy(project_id, &ProjectChildModelPolicy::default())
        .unwrap();
    assert!(
        store
            .load_project_child_model_policy(project_id)
            .unwrap()
            .is_empty()
    );
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn a_child_model_policy_needs_an_existing_project_root_and_goes_with_it() {
    let (path, store) = fresh_store();
    let missing = ProjectId::new();
    let error = store
        .replace_project_child_model_policy(missing, &policy())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);

    let root = AgentSessionId::new();
    insert_project_root(&path, root);
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    store
        .replace_project_child_model_policy(project_id, &policy())
        .unwrap();
    let child = AgentSessionId::new();
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
                 VALUES (?1, zeroblob(16), 'child', 'idle', 1, 1)",
                [child.as_uuid().as_bytes().as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
                 VALUES (?1, ?2, ?1, 2)",
                params![
                    root.as_uuid().as_bytes().as_slice(),
                    child.as_uuid().as_bytes().as_slice()
                ],
            )
            .unwrap();
    }
    let error = store
        .replace_project_child_model_policy(ProjectId::from_uuid(*child.as_uuid()), &policy())
        .unwrap_err();
    assert_eq!(
        error.code,
        ErrorCode::NotFound,
        "a delegated member is no root"
    );
    {
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        connection
            .execute(
                "DELETE FROM sessions WHERE id=?1",
                [root.as_uuid().as_bytes().as_slice()],
            )
            .unwrap();
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM project_child_models", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "deleting the root removes its child models");
    }
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn the_child_model_table_refuses_malformed_rows() {
    let (path, store) = fresh_store();
    let root = AgentSessionId::new();
    insert_project_root(&path, root);
    let connection = Connection::open(&path).unwrap();
    for (kind, model) in [("Vision", "a/b"), ("has space", "a/b"), ("ok", "  ")] {
        let inserted = connection.execute(
            "INSERT INTO project_child_models(project_id, kind, model_id) VALUES (?1, ?2, ?3)",
            params![root.as_uuid().as_bytes().as_slice(), kind, model],
        );
        assert!(inserted.is_err(), "{kind:?} -> {model:?} must be refused");
    }
    drop(connection);
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn migrates_a_v16_database_by_adding_the_child_model_table() {
    let (path, store) = fresh_store();
    let root = AgentSessionId::new();
    insert_project_root(&path, root);
    drop(store);
    {
        // A v16 database is today's layout without the child model table.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE project_child_models;
                 PRAGMA user_version = 16;",
            )
            .unwrap();
    }

    let store = FilePersistence::open(&path).unwrap();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    assert!(
        store
            .load_project_child_model_policy(project_id)
            .unwrap()
            .is_empty(),
        "a migrated project has no child model policy"
    );
    store
        .replace_project_child_model_policy(project_id, &policy())
        .unwrap();
    assert_eq!(
        store.load_project_child_model_policy(project_id).unwrap(),
        policy()
    );
    drop(store);

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, DATABASE_SCHEMA_VERSION);
    assert_eq!(DATABASE_SCHEMA_VERSION, 18);
    let sessions: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sessions, 1, "a migration keeps existing sessions");
    drop(connection);
    fs::remove_file(path).unwrap();
}
