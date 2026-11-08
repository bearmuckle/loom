//! Bounded client cache of transcript pages.
//!
//! Reopening a session or scrolling back over it need not fetch a page the
//! client already has. Two kinds of page are safe to keep:
//!
//! - An older page, requested with `before_ordinal`, covers messages that are
//!   already durable and never change.
//! - The newest page of a run that has finished stops growing, so it is kept
//!   too, tagged with the run's `updated_at`. A run that is later continued or
//!   rewritten has a different `updated_at`, which turns the entry into a miss.
//!
//! The newest page of a run that is still working is never kept: it grows with
//! every message.
//!
//! The cache is bounded by page count and by the serialized size of what it
//! holds, and the least recently used page goes first.

use std::collections::VecDeque;

use loom_core::{AgentSessionId, RunId, Timestamp};
use loom_model::{AgentRunSnapshot, AgentRunState};
use loom_protocol::ActivityDetail;

use super::TranscriptPage;

/// The most pages kept.
pub(super) const MAX_CACHED_PAGES: usize = 64;
/// The most serialized message bytes kept across every page.
pub(super) const MAX_CACHED_BYTES: usize = 8 * 1024 * 1024;

/// Which page a lookup names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PageKey {
    pub(super) session_id: AgentSessionId,
    pub(super) run_id: RunId,
    /// `None` is the newest page.
    pub(super) before_ordinal: Option<u64>,
    pub(super) activity_detail: ActivityDetail,
}

/// The run's `updated_at` when it has finished, which is when its newest page
/// may be cached. `None` while the run is still working, or when `run` is not
/// the run the page belongs to.
pub(super) fn settled_revision(run: Option<&AgentRunSnapshot>, run_id: RunId) -> Option<Timestamp> {
    let run = run.filter(|run| run.id == run_id)?;
    matches!(
        run.state,
        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
    )
    .then_some(run.updated_at)
}

struct Entry {
    key: PageKey,
    /// The run revision the page was read at; only set for a newest page.
    revision: Option<Timestamp>,
    bytes: usize,
    page: TranscriptPage,
}

/// Least-recently-used page cache; the front is the most recent.
#[derive(Default)]
pub(super) struct TranscriptPageCache {
    entries: VecDeque<Entry>,
    bytes: usize,
    #[cfg(test)]
    hits: usize,
}

impl TranscriptPageCache {
    /// The cached page for `key`. A newest page is returned only for the
    /// `revision` it was stored with, and never when `revision` is `None`.
    pub(super) fn get(
        &mut self,
        key: &PageKey,
        revision: Option<Timestamp>,
    ) -> Option<TranscriptPage> {
        if key.before_ordinal.is_none() && revision.is_none() {
            return None;
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.key == *key && entry.revision == revision_for(key, revision))?;
        let entry = self.entries.remove(index)?;
        let page = entry.page.clone();
        self.entries.push_front(entry);
        #[cfg(test)]
        {
            self.hits += 1;
        }
        Some(page)
    }

    /// Keeps `page` for `key`, unless it is a newest page of a run that is
    /// still working or it alone exceeds the byte budget.
    pub(super) fn insert(
        &mut self,
        key: PageKey,
        revision: Option<Timestamp>,
        page: &TranscriptPage,
    ) {
        if key.before_ordinal.is_none() && revision.is_none() {
            return;
        }
        let Ok(encoded) = serde_json::to_vec(&page.0) else {
            return;
        };
        let bytes = encoded.len();
        if bytes > MAX_CACHED_BYTES {
            return;
        }
        let revision = revision_for(&key, revision);
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.key == key && entry.revision == revision)
            && let Some(old) = self.entries.remove(index)
        {
            self.bytes -= old.bytes;
        }
        // A newer revision of the same newest page replaces the old one.
        if key.before_ordinal.is_none() {
            self.retain(|entry| entry.key != key);
        }
        self.entries.push_front(Entry {
            key,
            revision,
            bytes,
            page: page.clone(),
        });
        self.bytes += bytes;
        while self.entries.len() > MAX_CACHED_PAGES || self.bytes > MAX_CACHED_BYTES {
            let Some(evicted) = self.entries.pop_back() else {
                break;
            };
            self.bytes -= evicted.bytes;
        }
    }

    /// Drops every page of `session_id`, for a session that was archived or
    /// deleted.
    pub(super) fn forget_session(&mut self, session_id: AgentSessionId) {
        self.retain(|entry| entry.key.session_id != session_id);
    }

    fn retain(&mut self, mut keep: impl FnMut(&Entry) -> bool) {
        self.entries.retain(|entry| keep(entry));
        self.bytes = self.entries.iter().map(|entry| entry.bytes).sum();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(super) fn hits(&self) -> usize {
        self.hits
    }
}

/// An older page does not change, so it carries no revision.
fn revision_for(key: &PageKey, revision: Option<Timestamp>) -> Option<Timestamp> {
    if key.before_ordinal.is_none() {
        revision
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use loom_model::{MessageRole, ModelMessage};

    use super::*;

    /// The session every test key shares unless it sets its own.
    static SESSION: std::sync::LazyLock<AgentSessionId> =
        std::sync::LazyLock::new(AgentSessionId::new);

    fn key(run_id: RunId, before_ordinal: Option<u64>) -> PageKey {
        PageKey {
            session_id: *SESSION,
            run_id,
            before_ordinal,
            activity_detail: ActivityDetail::default(),
        }
    }

    fn page(n: u64) -> TranscriptPage {
        (Vec::new(), Some(n), false)
    }

    fn big_page(n: u64, text_len: usize) -> TranscriptPage {
        (
            vec![(
                n,
                n,
                ModelMessage::new(MessageRole::User, "x".repeat(text_len)),
            )],
            Some(n),
            true,
        )
    }

    fn at(millis: u64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    #[test]
    fn the_newest_page_needs_a_settled_run() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(key(run, None), None, &page(1));
        assert_eq!(cache.len(), 0, "a working run's newest page is not kept");
        assert!(cache.get(&key(run, None), None).is_none());

        cache.insert(key(run, None), Some(at(10)), &page(1));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&key(run, None), Some(at(10))), Some(page(1)));
        assert!(
            cache.get(&key(run, None), None).is_none(),
            "a run that is working again must refetch"
        );
    }

    #[test]
    fn a_changed_revision_is_a_miss_and_replaces_the_entry() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(key(run, None), Some(at(10)), &page(1));
        assert!(cache.get(&key(run, None), Some(at(11))).is_none());
        cache.insert(key(run, None), Some(at(11)), &page(2));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&key(run, None), Some(at(10))).is_none());
        assert_eq!(cache.get(&key(run, None), Some(at(11))), Some(page(2)));
    }

    #[test]
    fn an_older_page_hits_by_run_and_ordinal_whatever_the_revision() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(key(run, Some(5)), None, &page(3));
        assert_eq!(cache.get(&key(run, Some(5)), None), Some(page(3)));
        assert_eq!(cache.get(&key(run, Some(5)), Some(at(99))), Some(page(3)));
        assert!(cache.get(&key(run, Some(6)), None).is_none());
        assert!(cache.get(&key(RunId::new(), Some(5)), None).is_none());
        let mut other_detail = key(run, Some(5));
        other_detail.activity_detail = ActivityDetail::Replies;
        assert!(cache.get(&other_detail, None).is_none());
    }

    #[test]
    fn evicts_the_least_recently_used_page_past_the_count_bound() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        for n in 1..=MAX_CACHED_PAGES as u64 {
            cache.insert(key(run, Some(n)), None, &page(n));
        }
        // Touch the oldest so the next oldest is evicted instead.
        assert!(cache.get(&key(run, Some(1)), None).is_some());
        cache.insert(key(run, Some(1000)), None, &page(1000));
        assert_eq!(cache.len(), MAX_CACHED_PAGES);
        assert!(cache.get(&key(run, Some(1)), None).is_some());
        assert!(cache.get(&key(run, Some(2)), None).is_none());
    }

    #[test]
    fn evicts_to_stay_inside_the_byte_budget() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        let chunk = MAX_CACHED_BYTES / 4;
        for n in 1..=6u64 {
            cache.insert(key(run, Some(n)), None, &big_page(n, chunk));
            assert!(cache.bytes() <= MAX_CACHED_BYTES);
        }
        assert!(cache.len() < 6, "the oldest pages were evicted");
        assert!(cache.get(&key(run, Some(6)), None).is_some());
        assert!(cache.get(&key(run, Some(1)), None).is_none());
    }

    #[test]
    fn a_page_larger_than_the_whole_budget_is_not_kept() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(key(run, Some(1)), None, &big_page(1, MAX_CACHED_BYTES + 1));
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn forgetting_a_session_drops_only_its_pages_and_their_bytes() {
        let run = RunId::new();
        let mut kept = key(run, Some(1));
        kept.session_id = AgentSessionId::new();
        let mut gone = key(run, Some(2));
        gone.session_id = AgentSessionId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(kept, None, &big_page(1, 100));
        cache.insert(gone, None, &big_page(2, 100));
        let before = cache.bytes();
        cache.forget_session(gone.session_id);
        assert_eq!(cache.len(), 1);
        assert!(cache.bytes() < before && cache.bytes() > 0);
        assert!(cache.get(&kept, None).is_some());
        assert!(cache.get(&gone, None).is_none());
    }
}
