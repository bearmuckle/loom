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
