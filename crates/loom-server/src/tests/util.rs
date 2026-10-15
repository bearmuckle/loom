//! In-process tests: util.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn a_short_timeline_order_fails_the_run_checkpoint_instead_of_panicking() {
    // The message checkpoint cursor can advance past the end of a timeline
    // order that a recovered run left shorter than its transcript. The durable
    // projection must report the mismatch rather than index out of bounds.
    let messages = vec![
        ModelMessage::new(loom_model::MessageRole::User, "first"),
        ModelMessage::new(loom_model::MessageRole::Assistant, "second"),
    ];
    let short_ordinals = vec![7_u64];

    let error = durable_run_messages_from_runtime_tail(&messages, &short_ordinals, 1)
        .expect_err("a shorter timeline order must not be sliced blindly");
    assert_eq!(error.code, ErrorCode::MalformedPayload);

    let ordinals = vec![7_u64, 8];
    let projected = durable_run_messages_from_runtime_tail(&messages, &ordinals, 1)
        .expect("a matching tail still projects");
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].timeline_ordinal, 8);
    assert_eq!(projected[0].content, "second");
}

#[test]
fn non_sqlite_persistence_file_is_rejected_without_fallback() {
    let path =
        std::env::temp_dir().join(format!("loom-server-malformed-{}.db", WorkspaceId::new()));
    fs::write(&path, br#"{"schema_version":1,"state":{"broken":true}}"#).unwrap();
    let error = match InProcessBackend::new_persistent(&path) {
        Ok(_) => panic!("non-SQLite persistence unexpectedly loaded"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::Persistence);
    assert!(!path.with_extension("json.legacy").exists());
    fs::remove_file(path).unwrap();
}
