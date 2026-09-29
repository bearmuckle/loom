use super::*;

pub(crate) fn decode_content_blob(connection: &Connection, hash: &[u8]) -> Result<Vec<u8>> {
    let (raw_size, codec, payload): (i64, i64, Vec<u8>) = connection
        .query_row(
            "SELECT raw_size, codec, payload FROM content_blobs WHERE hash = ?1",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| {
            persistence_error(format!("could not read state content part: {error}"), true)
        })?;
    if raw_size <= 0 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted content part has an invalid size",
            false,
        ));
    }
    decode_content_payload(
        hash,
        raw_size,
        codec,
        payload,
        CONTENT_PART_BYTES,
        "content part",
    )
}

pub(crate) fn decode_content_payload(
    hash: &[u8],
    raw_size: i64,
    codec: i64,
    payload: Vec<u8>,
    max_size: usize,
    kind: &str,
) -> Result<Vec<u8>> {
    if raw_size < 0 || raw_size > max_size as i64 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {kind} has an invalid size"),
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
                        format!("persisted {kind} is malformed: {error}"),
                        false,
                    )
                })?;
            decoded
        }
        _ => {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted {kind} uses an unsupported codec"),
                false,
            ));
        }
    };
    if bytes.len() as i64 != raw_size || Sha256::digest(&bytes).as_slice() != hash {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {kind} failed its length or hash check"),
            false,
        ));
    }
    Ok(bytes)
}

pub(crate) fn load_content_range(
    connection: &Connection,
    hash: &[u8],
    offset: usize,
    requested_len: usize,
) -> Result<Vec<u8>> {
    let (raw_size, inline_codec, inline_payload): (i64, Option<i64>, Option<Vec<u8>>) = connection
        .query_row(
            "SELECT raw_size, inline_codec, inline_payload FROM content_objects WHERE hash = ?1",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read state content metadata: {error}"),
                true,
            )
        })?;
    if raw_size < 0 || raw_size > MAX_CONTENT_BYTES as i64 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted state content exceeds the maximum supported size",
            false,
        ));
    }
    let start = offset.min(raw_size as usize);
    let end = start.saturating_add(requested_len).min(raw_size as usize);
    if start == end {
        return Ok(Vec::new());
    };
    if let (Some(codec), Some(payload)) = (inline_codec, inline_payload) {
        let bytes = decode_content_payload(
            hash,
            raw_size,
            codec,
            payload,
            INLINE_CONTENT_BYTES,
            "inline content",
        )?;
        return Ok(bytes[start..end].to_vec());
    }
    let mut statement = connection
        .prepare(
            "SELECT byte_offset, byte_length, blob_hash FROM content_parts
             WHERE content_hash=?1 AND byte_offset < ?2
               AND byte_offset + byte_length > ?3
             ORDER BY byte_offset",
        )
        .map_err(|error| {
            persistence_error(format!("could not prepare content range: {error}"), true)
        })?;
    let parts = statement
        .query_map(params![hash, end as i64, start as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|error| {
            persistence_error(format!("could not read content range: {error}"), true)
        })?;
    let mut output = Vec::with_capacity(end - start);
    let mut cursor = start;
    for part in parts {
        let (part_offset, part_length, blob_hash) = part.map_err(|error| {
            persistence_error(format!("could not read content part: {error}"), true)
        })?;
        let part_offset = usize::try_from(part_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part has an invalid offset",
                false,
            )
        })?;
        let part_length = usize::try_from(part_length).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part has an invalid length",
                false,
            )
        })?;
        let part_end = part_offset.checked_add(part_length).ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part range overflowed",
                false,
            )
        })?;
        let slice_start = start.max(part_offset);
        let slice_end = end.min(part_end);
        if slice_start != cursor || slice_start >= slice_end {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content parts are not contiguous",
                false,
            ));
        }
        let part_bytes = decode_content_blob(connection, &blob_hash)?;
        if part_bytes.len() != part_length {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part length does not match its range",
                false,
            ));
        }
        output.extend_from_slice(&part_bytes[slice_start - part_offset..slice_end - part_offset]);
        cursor = slice_end;
    }
    if cursor != end {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted content range is incomplete",
            false,
        ));
    }
    Ok(output)
}

pub(crate) fn decode_content(connection: &Connection, hash: &[u8]) -> Result<String> {
    let raw_size: i64 = connection
        .query_row(
            "SELECT raw_size FROM content_objects WHERE hash = ?1",
            [hash],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read state content metadata: {error}"),
                true,
            )
        })?;
    let bytes = load_content_range(connection, hash, 0, raw_size.max(0) as usize)?;
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
pub(crate) fn store_content(transaction: &Transaction<'_>, content: &[u8]) -> Result<Vec<u8>> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(LoomError::invalid_request(
            "persisted content exceeds the maximum supported size",
        ));
    }
    let hash = Sha256::digest(content).to_vec();
    let already_stored = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM content_objects WHERE hash=?1)",
            [hash.as_slice()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| {
            persistence_error(format!("could not check state content: {error}"), true)
        })?;
    if already_stored {
        return Ok(hash);
    }
    let inline = (content.len() <= INLINE_CONTENT_BYTES)
        .then(|| encode_inline_content(content))
        .transpose()?;
    transaction
        .execute(
            "INSERT INTO content_objects(hash, raw_size, inline_codec, inline_payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                hash,
                content.len() as i64,
                inline.as_ref().map(|(codec, _)| *codec),
                inline.as_ref().map(|(_, payload)| payload.as_slice()),
            ],
        )
        .map_err(|error| {
            persistence_error(format!("could not store content metadata: {error}"), true)
        })?;
    if inline.is_some() {
        return Ok(hash);
    }
    for (ordinal, part) in content.chunks(CONTENT_PART_BYTES).enumerate() {
        let byte_offset = ordinal * CONTENT_PART_BYTES;
        let part_hash = store_content_blob(transaction, part)?;
        transaction
            .execute(
                "INSERT INTO content_parts(
                    content_hash, ordinal, byte_offset, byte_length, blob_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    hash,
                    ordinal as i64,
                    byte_offset as i64,
                    part.len() as i64,
                    part_hash
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not store content part reference: {error}"),
                    true,
                )
            })?;
    }
    Ok(hash)
}

pub(crate) fn encode_inline_content(content: &[u8]) -> Result<(i64, Vec<u8>)> {
    if content.len() >= 512 {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(content).map_err(|error| {
            persistence_error(
                format!("could not compress inline state content: {error}"),
                false,
            )
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(
                format!("could not compress inline state content: {error}"),
                false,
            )
        })?;
        if compressed.len().saturating_mul(100) <= content.len().saturating_mul(90) {
            return Ok((1, compressed));
        }
    }
    Ok((0, content.to_vec()))
}

pub(crate) fn store_content_blob(transaction: &Transaction<'_>, content: &[u8]) -> Result<Vec<u8>> {
    debug_assert!(!content.is_empty() && content.len() <= CONTENT_PART_BYTES);
    let hash = Sha256::digest(content).to_vec();
    let (codec, payload) = if content.len() >= EXTERNAL_STRING_THRESHOLD {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(content).map_err(|error| {
            persistence_error(format!("could not compress state content: {error}"), false)
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(format!("could not compress state content: {error}"), false)
        })?;
        if compressed.len().saturating_mul(100) <= content.len().saturating_mul(90) {
            (1_i64, compressed)
        } else {
            (0_i64, content.to_vec())
        }
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
            persistence_error(format!("could not store state content part: {error}"), true)
        })?;
    Ok(hash)
}

pub(crate) fn collect_unused_content(
    transaction: &Transaction<'_>,
    candidate_limit: usize,
) -> Result<()> {
    let candidate_limit = i64::try_from(candidate_limit).map_err(|_| {
        LoomError::invalid_request("content garbage-collection limit is out of range")
    })?;
    transaction
        .execute(
            "DELETE FROM content_objects
             WHERE hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='object' ORDER BY hash LIMIT ?1
             ) AND NOT EXISTS (
                SELECT 1 FROM checkpoint_files
                WHERE checkpoint_files.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_messages
                WHERE run_messages.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_context_checkpoints
                WHERE run_context_checkpoints.summary_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_activities
                WHERE run_activities.data_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_tool_calls
                WHERE run_tool_calls.arguments_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM runtime_configurations
                WHERE runtime_configurations.system_instructions_hash=content_objects.hash
                   OR runtime_configurations.repository_instructions_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM filesystem_edits
                WHERE filesystem_edits.before_hash=content_objects.hash
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused content metadata: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_gc_candidates
             WHERE kind='object' AND hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='object' ORDER BY hash LIMIT ?1
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune processed content candidates: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_blobs
             WHERE hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='blob' ORDER BY hash LIMIT ?1
             ) AND NOT EXISTS (
                 SELECT 1 FROM content_parts WHERE content_parts.blob_hash=content_blobs.hash
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused content parts: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_gc_candidates
             WHERE kind='blob' AND hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='blob' ORDER BY hash LIMIT ?1
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune processed blob candidates: {error}"),
                true,
            )
        })?;
    Ok(())
}
