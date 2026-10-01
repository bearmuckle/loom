//! Persistence tests: content.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn small_content_is_inline_compressed_and_read_by_range() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let text = "repeated transcript text with useful detail\n".repeat(48);
    let connection = Connection::open(&path).unwrap();
    initialize_schema(&connection).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    let hash = store_content(&transaction, text.as_bytes()).unwrap();
    transaction.commit().unwrap();

    let (codec, payload_length, parts, blobs): (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT objects.inline_codec, length(objects.inline_payload),
                    (SELECT COUNT(*) FROM content_parts WHERE content_hash=objects.hash),
                    (SELECT COUNT(*) FROM content_blobs)
             FROM content_objects objects WHERE objects.hash=?1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(codec, 1);
    assert!(payload_length < text.len() as i64);
    assert_eq!((parts, blobs), (0, 0));
    assert_eq!(decode_content(&connection, &hash).unwrap(), text);
    assert_eq!(
        load_content_range(&connection, &hash, 13, 41).unwrap(),
        text.as_bytes()[13..54]
    );

    connection
        .execute(
            "UPDATE content_objects SET inline_payload=x'0102' WHERE hash=?1",
            [&hash],
        )
        .unwrap();
    assert_eq!(
        decode_content(&connection, &hash).unwrap_err().code,
        ErrorCode::MalformedPayload
    );
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn corrupt_content_objects_are_rejected_during_restore() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let text = "content whose integrity must survive process restarts ".repeat(200);
    let connection = Connection::open(&path).unwrap();
    initialize_schema(&connection).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    store_content(&transaction, text.as_bytes()).unwrap();
    transaction.commit().unwrap();
    let (object_hash, blob_hash): (Vec<u8>, Vec<u8>) = connection
        .query_row(
            "SELECT objects.hash, parts.blob_hash
             FROM content_objects AS objects
             JOIN content_parts AS parts ON parts.content_hash=objects.hash
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    connection
        .execute(
            "UPDATE content_blobs SET codec=1, payload=x'0102' WHERE hash=?1",
            [&blob_hash],
        )
        .unwrap();
    let malformed_compressed = decode_content(&connection, &object_hash).unwrap_err();
    assert_eq!(malformed_compressed.code, ErrorCode::MalformedPayload);

    connection
        .execute(
            "UPDATE content_blobs SET codec=0, payload=x'00' WHERE hash=?1",
            [&blob_hash],
        )
        .unwrap();
    let damaged_content = decode_content(&connection, &object_hash).unwrap_err();
    assert_eq!(damaged_content.code, ErrorCode::MalformedPayload);

    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn reconnect_event_decoder_accepts_compressed_rows_and_rejects_corruption() {
    let session_id = AgentSessionId::new();
    let event = ServerEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(7),
        session_id,
        event: loom_model::ServerEvent::AgentSessionCreated {
            snapshot: AgentSessionSnapshot {
                id: session_id,
                workspace_id: WorkspaceId::new(),
                name: "compressed event".to_owned(),
                state: AgentSessionState::Idle,
                created_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(1),
            },
        },
    };
    let raw = serde_json::to_vec(&event).unwrap();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&raw).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < raw.len());
    assert_eq!(
        decode_feed_event(
            7,
            session_id.as_uuid().as_bytes().to_vec(),
            1,
            compressed.clone(),
        )
        .unwrap(),
        event
    );
    assert_eq!(
        decode_feed_event(
            7,
            session_id.as_uuid().as_bytes().to_vec(),
            99,
            compressed.clone(),
        )
        .unwrap_err()
        .code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        decode_feed_event(
            7,
            session_id.as_uuid().as_bytes().to_vec(),
            1,
            b"not a zlib stream".to_vec(),
        )
        .unwrap_err()
        .code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        decode_feed_event(8, session_id.as_uuid().as_bytes().to_vec(), 1, compressed,)
            .unwrap_err()
            .code,
        ErrorCode::MalformedPayload
    );
}
