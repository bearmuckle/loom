use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use super::*;

pub(crate) const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(crate) const LEGACY_IDEMPOTENCY_RETENTION: usize = 1024;
pub(crate) const REQUEST_ID_FUTURE_SKEW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct IdempotencyRecord {
    pub(crate) created_at: Timestamp,
    pub(crate) expires_at: Option<Timestamp>,
    pub(crate) request: ClientRequest,
    pub(crate) response: ResponseEnvelope,
}

impl IdempotencyRecord {
    pub(crate) fn new(
        request_id: RequestId,
        request: ClientRequest,
        response: ResponseEnvelope,
    ) -> Self {
        Self {
            created_at: Timestamp::now(),
            expires_at: request_id.issued_at_unix_millis().map(|issued_at| {
                Timestamp::from_unix_millis(
                    issued_at.saturating_add(IDEMPOTENCY_RETENTION.as_millis() as u64),
                )
            }),
            request,
            response,
        }
    }
}

fn validate_retry_horizon(request_id: RequestId, now_ms: u64) -> Result<()> {
    let Some(issued_at_ms) = request_id.issued_at_unix_millis() else {
        // UUIDv4 IDs were used by earlier protocol clients. Keep their bounded
        // count-based cache behavior while new clients use timestamped UUIDv7.
        return Ok(());
    };
    let future_skew_ms = REQUEST_ID_FUTURE_SKEW.as_millis() as u64;
    if issued_at_ms > now_ms.saturating_add(future_skew_ms) {
        return Err(LoomError::invalid_request(
            "request id issue time is too far in the future",
        ));
    }
    if now_ms.saturating_sub(issued_at_ms) > IDEMPOTENCY_RETENTION.as_millis() as u64 {
        return Err(LoomError::new(
            ErrorCode::DeadlineExceeded,
            "retry horizon expired; submit the operation as a new request",
            false,
        ));
    }
    Ok(())
}

pub(crate) fn trim_idempotency_cache(cache: &mut BTreeMap<RequestId, IdempotencyRecord>) {
    let now = current_unix_millis();
    cache.retain(|request_id, record| {
        record
            .expires_at
            .is_none_or(|expires_at| expires_at.as_unix_millis() > now)
            || request_id.issued_at_unix_millis().is_none()
    });

    let mut legacy = cache
        .iter()
        .filter(|(request_id, _)| request_id.issued_at_unix_millis().is_none())
        .map(|(request_id, record)| (*request_id, record.created_at))
        .collect::<Vec<_>>();
    if legacy.len() > LEGACY_IDEMPOTENCY_RETENTION {
        let expired_count = legacy.len() - LEGACY_IDEMPOTENCY_RETENTION;
        legacy.sort_by_key(|(_, created_at)| *created_at);
        for (request_id, _) in legacy.into_iter().take(expired_count) {
            cache.remove(&request_id);
        }
    }
}

/// Owns the idempotency cache, the per-request serialization slots, and the
/// gate that serializes durable mutations.
#[derive(Default)]
pub(crate) struct IdempotencyStore {
    records: Mutex<BTreeMap<RequestId, IdempotencyRecord>>,
    in_flight: Mutex<BTreeMap<RequestId, Arc<Mutex<()>>>>,
    durable_gate: Mutex<()>,
}

impl IdempotencyStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn records(&self) -> Result<MutexGuard<'_, BTreeMap<RequestId, IdempotencyRecord>>> {
        self.records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "idempotency cache lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn replace_records(
        &self,
        records: BTreeMap<RequestId, IdempotencyRecord>,
    ) -> Result<()> {
        let mut records = records;
        trim_idempotency_cache(&mut records);
        *self.records()? = records;
        Ok(())
    }

    /// Serializes retries of one request id without serializing unrelated
    /// mutations, so a long-running request cannot block a control request.
    pub(crate) fn request_slot(&self, request_id: RequestId) -> Result<Arc<Mutex<()>>> {
        let mut in_flight = self.in_flight.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "request serialization lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(in_flight.entry(request_id).or_default()))
    }

    pub(crate) fn release_request_slot(&self, request_id: RequestId) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if in_flight
            .get(&request_id)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            in_flight.remove(&request_id);
        }
    }

    pub(crate) fn durable_gate(&self) -> Result<MutexGuard<'_, ()>> {
        self.durable_gate.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "durable request serialization lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn validate_retry_horizon(&self, request_id: RequestId) -> Result<()> {
        validate_retry_horizon(request_id, current_unix_millis())
    }

    pub(crate) fn cached_response(
        &self,
        request_id: RequestId,
        request: &ClientRequest,
    ) -> Result<Option<ResponseEnvelope>> {
        let cache = self.records()?;
        let Some(record) = cache.get(&request_id) else {
            return Ok(None);
        };
        if &record.request != request {
            return Err(LoomError::conflict(format!(
                "request id {request_id} was already used for a different mutation"
            )));
        }
        Ok(Some(record.response.clone()))
    }

    pub(crate) fn publish(&self, request_id: RequestId, record: IdempotencyRecord) -> Result<()> {
        let mut cache = self.records()?;
        cache.insert(request_id, record);
        trim_idempotency_cache(&mut cache);
        Ok(())
    }

    pub(crate) fn durable_records(
        &self,
        candidate: Option<&(RequestId, IdempotencyRecord)>,
    ) -> Result<BTreeMap<RequestId, DurableIdempotencyRecord>> {
        let mut idempotency = self
            .records()?
            .iter()
            .map(|(id, record)| Ok((*id, durable_record_from(record)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        if let Some((request_id, record)) = candidate {
            idempotency.insert(*request_id, durable_record_from(record)?);
        }
        Ok(idempotency)
    }
}

fn durable_record_from(record: &IdempotencyRecord) -> Result<DurableIdempotencyRecord> {
    Ok(DurableIdempotencyRecord {
        created_at: record.created_at,
        expires_at: record.expires_at,
        request: json_value(&record.request)?,
        response: json_value(&record.response)?,
    })
}
