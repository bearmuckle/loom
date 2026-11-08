//! Bounded client cache of older transcript pages.
//!
//! A page requested with `before_ordinal` covers messages that are already
//! durable, so reopening a session or scrolling back over it need not fetch it
//! again. The newest page is never cached because it keeps growing.

use std::collections::VecDeque;

use loom_core::RunId;
use loom_protocol::ActivityDetail;

use super::TranscriptPage;

/// The most pages kept; the least recently used page is evicted past this.
pub(super) const MAX_CACHED_PAGES: usize = 32;

struct Entry {
    run_id: RunId,
    before_ordinal: u64,
    activity_detail: ActivityDetail,
    page: TranscriptPage,
}

/// Least-recently-used page cache; the front is the most recent.
#[derive(Default)]
pub(super) struct TranscriptPageCache {
    entries: VecDeque<Entry>,
}

impl TranscriptPageCache {
    pub(super) fn get(
        &mut self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        activity_detail: ActivityDetail,
    ) -> Option<TranscriptPage> {
        let before_ordinal = before_ordinal?;
        let index = self.entries.iter().position(|entry| {
            entry.run_id == run_id
                && entry.before_ordinal == before_ordinal
                && entry.activity_detail == activity_detail
        })?;
        let entry = self.entries.remove(index)?;
        let page = entry.page.clone();
        self.entries.push_front(entry);
        Some(page)
    }

    pub(super) fn insert(
        &mut self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        activity_detail: ActivityDetail,
        page: &TranscriptPage,
    ) {
        let Some(before_ordinal) = before_ordinal else {
            return;
        };
        self.entries.retain(|entry| {
            !(entry.run_id == run_id
                && entry.before_ordinal == before_ordinal
                && entry.activity_detail == activity_detail)
        });
        self.entries.push_front(Entry {
            run_id,
            before_ordinal,
            activity_detail,
            page: page.clone(),
        });
        self.entries.truncate(MAX_CACHED_PAGES);
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(n: u64) -> TranscriptPage {
        (Vec::new(), Some(n), false)
    }

    #[test]
    fn newest_page_is_never_cached() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(run, None, ActivityDetail::default(), &page(1));
        assert_eq!(cache.len(), 0);
        assert!(cache.get(run, None, ActivityDetail::default()).is_none());
    }

    #[test]
    fn older_page_hits_by_run_and_ordinal() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        cache.insert(run, Some(5), ActivityDetail::default(), &page(3));
        assert_eq!(
            cache.get(run, Some(5), ActivityDetail::default()),
            Some(page(3))
        );
        assert!(cache.get(run, Some(6), ActivityDetail::default()).is_none());
        assert!(
            cache
                .get(RunId::new(), Some(5), ActivityDetail::default())
                .is_none()
        );
    }

    #[test]
    fn evicts_least_recently_used() {
        let run = RunId::new();
        let mut cache = TranscriptPageCache::default();
        for n in 1..=MAX_CACHED_PAGES as u64 {
            cache.insert(run, Some(n), ActivityDetail::default(), &page(n));
        }
        // Touch the oldest so the next oldest is evicted instead.
        assert!(cache.get(run, Some(1), ActivityDetail::default()).is_some());
        cache.insert(run, Some(100), ActivityDetail::default(), &page(100));
        assert_eq!(cache.len(), MAX_CACHED_PAGES);
        assert!(cache.get(run, Some(1), ActivityDetail::default()).is_some());
        assert!(cache.get(run, Some(2), ActivityDetail::default()).is_none());
    }
}
