use super::*;
use sha2::{Digest, Sha256};

/// File name of the on-disk index of cached repository clones.
const CACHE_INDEX_FILE: &str = "index.json";

/// Owns attached session repositories, their Git services, and the node-level
/// index of repositories this worker has already cloned.
#[derive(Default)]
pub(crate) struct RepositoryService {
    records: Mutex<BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>,
    vcs: Mutex<BTreeMap<(AgentSessionId, RepositoryId), GitService>>,
    cache: Mutex<RepositoryCache>,
}

/// A repository mirror cached on this worker node, keyed by its normalized
/// clone URL. Mirrors let a later session start from an existing clone instead
/// of downloading the repository again.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedRepositoryRecord {
    full_name: String,
    clone_url: String,
    branch: Option<String>,
    last_used_at: Timestamp,
    /// Mirror directory name relative to the cache base.
    mirror: String,
}

/// When a node mirror was last known to match its remote, recorded next to the
/// mirror directory.
///
/// The record lives beside the mirror instead of in a database row: it
/// describes one directory on this node's disk. A missing or unreadable record
/// is reported as unknown freshness, never assumed current.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CachedMirrorRefresh {
    /// Clone URL the mirror belongs to.
    pub(crate) clone_url: String,
    /// When the refresh behind this record finished.
    pub(crate) refreshed_at: Timestamp,
    /// Default branch that refresh resolved, when it resolved one.
    pub(crate) default_branch: Option<String>,
    /// Default-branch tip that refresh resolved, when it resolved one.
    pub(crate) revision: Option<String>,
}

impl CachedMirrorRefresh {
    /// How long before `now` this refresh finished, in milliseconds. A record
    /// dated after `now` has an age of zero rather than a negative age.
    pub(crate) fn age_millis(&self, now: Timestamp) -> u64 {
        now.as_unix_millis()
            .saturating_sub(self.refreshed_at.as_unix_millis())
    }

    /// The tip this record describes, for a report that names it.
    fn tip(&self) -> String {
        match (&self.default_branch, &self.revision) {
            (Some(branch), Some(revision)) => format!(" to {branch} at {revision}"),
            (Some(branch), None) => format!(" to {branch}"),
            (None, Some(revision)) => format!(" at {revision}"),
            (None, None) => String::new(),
        }
    }
}

/// How current a node mirror was when a session reused it.
///
/// Only [`Self::Refreshed`] claims that the mirror's heads describe the remote,
/// and only as of its record. When the bounded refresh fails the mirror is still
/// reused, but it holds a cached tip whose distance from upstream is unknown,
/// and [`Self::summary`] says so instead of presenting that tip as the current
/// upstream tip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MirrorFreshness {
    /// The bounded refresh of the mirror finished before the mirror was used.
    Refreshed(CachedMirrorRefresh),
    /// The bounded refresh failed or expired; `last_refresh` is the last
    /// successful refresh this node recorded, when it has one.
    Unknown {
        last_refresh: Option<CachedMirrorRefresh>,
        warning: String,
    },
}

impl MirrorFreshness {
    /// Whether this reuse could not establish how current the mirror is.
    pub(crate) fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// Reports the freshness of a reused mirror wherever it is stated. It names
    /// the mirror's age when a refresh established it and says the freshness is
    /// unknown when it did not.
    pub(crate) fn summary(&self, now: Timestamp) -> String {
        match self {
            Self::Refreshed(refresh) => format!(
                "reused, mirror refreshed {} ms ago{}",
                refresh.age_millis(now),
                refresh.tip()
            ),
            Self::Unknown {
                last_refresh,
                warning,
            } => match last_refresh {
                Some(last) => format!(
                    "reused, freshness unknown: the mirror refresh failed ({warning}); \
                     the last successful refresh was {} ms ago",
                    last.age_millis(now)
                ),
                None => format!(
                    "reused, freshness unknown: the mirror refresh failed ({warning}) \
                     and no successful refresh is recorded"
                ),
            },
        }
    }
}

#[derive(Default)]
struct RepositoryCache {
    base: PathBuf,
    records: BTreeMap<String, CachedRepositoryRecord>,
}

impl InProcessBackend {
    /// Opens (and caches) the Git service of one attached session repository.
    ///
    /// Restoring the session filesystem first is deliberate: it also loads the
    /// session's durable repository records, so the lookup below sees a
    /// lazily restored session too.
    pub(crate) fn session_git(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<GitService> {
        if let Some(service) = self
            .session_vcs()?
            .get(&(session_id, repository_id))
            .cloned()
        {
            return Ok(service);
        }
        let filesystem = self.restore_session_filesystem(session_id)?;
        let repository = self
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        let path = filesystem.directory_path(&repository.path)?;
        let service = GitService::open(path)?;
        self.session_vcs()?
            .insert((session_id, repository_id), service.clone());
        Ok(service)
    }
}

impl RepositoryService {
    pub(crate) fn new(clone_cache_base: PathBuf) -> Self {
        Self {
            records: Mutex::new(BTreeMap::new()),
            vcs: Mutex::new(BTreeMap::new()),
            cache: Mutex::new(RepositoryCache::load(clone_cache_base)),
        }
    }

    pub(crate) fn records(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session repository manager lock was poisoned",
                true,
            )
        })
    }

    pub(crate) fn vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.vcs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session Git service manager lock was poisoned",
                true,
            )
        })
    }

    fn cache(&self) -> Result<MutexGuard<'_, RepositoryCache>> {
        self.cache.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "repository clone cache lock was poisoned",
                true,
            )
        })
    }

    /// Repositories already cloned on this node, sorted by full name. Mirrors
    /// that no longer exist are pruned from the index.
    pub(crate) fn cached_repositories(&self) -> Result<Vec<ClonedRepository>> {
        let mut cache = self.cache()?;
        let base = cache.base.clone();
        let mut pruned = false;
        cache.records.retain(|_, record| {
            let available = base.join(&record.mirror).is_dir();
            if !available {
                pruned = true;
                discard_mirror_refresh(&base, &record.clone_url);
            }
            available
        });
        if pruned {
            cache.save()?;
        }
        let mut repositories = cache
            .records
            .values()
            .map(|record| ClonedRepository {
                full_name: record.full_name.clone(),
                clone_url: record.clone_url.clone(),
                branch: record.branch.clone(),
                last_used_at: record.last_used_at,
            })
            .collect::<Vec<_>>();
        repositories.sort_by(|left, right| left.full_name.cmp(&right.full_name));
        Ok(repositories)
    }

    /// Path of the mirror for `clone_url`, whether or not it exists yet.
    pub(crate) fn mirror_path(&self, clone_url: &str) -> Result<PathBuf> {
        let cache = self.cache()?;
        Ok(cache.base.join(mirror_file_name(clone_url)))
    }

    /// Existing mirror for `clone_url`, if this node has already cloned it.
    pub(crate) fn cached_mirror(&self, clone_url: &str) -> Result<Option<PathBuf>> {
        let key = normalized_clone_url(clone_url);
        let mut cache = self.cache()?;
        let base = cache.base.clone();
        let Some(record) = cache.records.get(&key) else {
            return Ok(None);
        };
        let path = base.join(&record.mirror);
        if path.is_dir() {
            return Ok(Some(path));
        }
        cache.records.remove(&key);
        cache.save()?;
        discard_mirror_refresh(&base, clone_url);
        Ok(None)
    }

    /// When the mirror of `clone_url` was last known to match its remote, when
    /// this node has a record for it.
    ///
    /// The record is read from beside the mirror on every call instead of being
    /// cached in memory, so a mirror another process of this node refreshed is
    /// reported with the time it actually was refreshed at.
    pub(crate) fn mirror_refresh(&self, clone_url: &str) -> Result<Option<CachedMirrorRefresh>> {
        let cache = self.cache()?;
        Ok(read_mirror_refresh(&cache.base, clone_url))
    }

    /// Records when the mirror of `refresh.clone_url` was last known to match
    /// its remote.
    ///
    /// The record is written through a temporary file and renamed into place, so
    /// a reader never sees a partial record: the record decides whether a mirror
    /// is reused at all, and a torn one would be reported as unknown freshness.
    pub(crate) fn record_mirror_refresh(&self, refresh: &CachedMirrorRefresh) -> Result<()> {
        let cache = self.cache()?;
        fs::create_dir_all(&cache.base).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not create the repository cache directory: {error}"),
                false,
            )
        })?;
        let path = cache
            .base
            .join(mirror_refresh_file_name(&refresh.clone_url));
        let bytes = serde_json::to_vec_pretty(refresh).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not encode the mirror refresh record: {error}"),
                false,
            )
        })?;
        let temporary = path.with_extension(format!("json.tmp-{}", uuid::Uuid::new_v4()));
        fs::write(&temporary, bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not write the mirror refresh record: {error}"),
                false,
            )
        })?;
        fs::rename(&temporary, &path).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not install the mirror refresh record: {error}"),
                false,
            )
        })
    }

    /// Record that this node has cloned `repository`, so later sessions can
    /// reuse its mirror.
    pub(crate) fn register_cloned_repository(&self, repository: &ClonedRepository) -> Result<()> {
        let key = normalized_clone_url(&repository.clone_url);
        let mut cache = self.cache()?;
        cache.records.insert(
            key,
            CachedRepositoryRecord {
                full_name: repository.full_name.clone(),
                clone_url: repository.clone_url.clone(),
                branch: repository.branch.clone(),
                last_used_at: repository.last_used_at,
                mirror: mirror_file_name(&repository.clone_url),
            },
        );
        cache.save()
    }
}

impl RepositoryCache {
    fn load(base: PathBuf) -> Self {
        let path = base.join(CACHE_INDEX_FILE);
        let records = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(records) => records,
                Err(error) => {
                    log::warn!(
                        "[loom-server] ignoring malformed repository cache index {}: {error}",
                        path.display()
                    );
                    BTreeMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                log::warn!(
                    "[loom-server] could not read repository cache index {}: {error}",
                    path.display()
                );
                BTreeMap::new()
            }
        };
        Self { base, records }
    }

    fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.base).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not create the repository cache directory: {error}"),
                false,
            )
        })?;
        let bytes = serde_json::to_vec_pretty(&self.records).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not encode the repository cache index: {error}"),
                false,
            )
        })?;
        let path = self.base.join(CACHE_INDEX_FILE);
        let temporary = self
            .base
            .join(format!("{CACHE_INDEX_FILE}.tmp-{}", uuid::Uuid::new_v4()));
        fs::write(&temporary, bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not write the repository cache index: {error}"),
                false,
            )
        })?;
        fs::rename(&temporary, &path).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not install the repository cache index: {error}"),
                false,
            )
        })
    }
}

/// Stable key for a clone URL: scheme, host, and owner/name are
/// case-insensitive, and a trailing `.git` or slash never changes identity.
pub(crate) fn normalized_clone_url(clone_url: &str) -> String {
    clone_url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase()
}

fn mirror_file_name(clone_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized_clone_url(clone_url).as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{}.git", &hex[..32])
}

/// File name of the refresh record recorded next to a mirror directory, derived
/// from the same hash as the mirror itself.
fn mirror_refresh_file_name(clone_url: &str) -> String {
    format!("{}.refresh.json", mirror_file_name(clone_url))
}

/// Reads the refresh record recorded next to a mirror, if it has one.
///
/// A missing, unreadable, or malformed record is `None`, which callers report as
/// unknown freshness: a record that cannot be read is never taken as evidence
/// that the mirror is current.
fn read_mirror_refresh(base: &Path, clone_url: &str) -> Option<CachedMirrorRefresh> {
    let path = base.join(mirror_refresh_file_name(clone_url));
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!(
                "[loom-server] could not read the mirror refresh record {}: {error}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(refresh) => Some(refresh),
        Err(error) => {
            log::warn!(
                "[loom-server] ignoring malformed mirror refresh record {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Removes the refresh record of a mirror that no longer exists. A leftover
/// record would describe a mirror directory that is gone.
fn discard_mirror_refresh(base: &Path, clone_url: &str) {
    let path = base.join(mirror_refresh_file_name(clone_url));
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            log::warn!(
                "[loom-server] could not remove the mirror refresh record {}: {error}",
                path.display()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_url_keys_normalize_case_and_suffixes() {
        let key = normalized_clone_url("https://github.com/Owner/Name.git");
        assert_eq!(key, "https://github.com/owner/name");
        assert_eq!(normalized_clone_url("https://github.com/owner/name/"), key);
        assert_eq!(normalized_clone_url("https://github.com/owner/name"), key);
    }

    #[test]
    fn mirror_file_names_are_stable_and_distinct() {
        let first = mirror_file_name("https://github.com/owner/name.git");
        let second = mirror_file_name("https://github.com/owner/name");
        assert_eq!(first, second);
        assert_ne!(
            mirror_file_name("https://github.com/owner/other.git"),
            first
        );
        assert!(first.ends_with(".git"));
    }

    #[test]
    fn cache_round_trips_records_and_prunes_missing_mirrors() {
        let base = std::env::temp_dir().join(format!("loom-cache-{}", uuid::Uuid::new_v4()));
        let service = RepositoryService::new(base.clone());
        let mirror = service
            .mirror_path("https://github.com/owner/name.git")
            .unwrap();
        fs::create_dir_all(&mirror).unwrap();
        let repository = ClonedRepository {
            full_name: "owner/name".to_owned(),
            clone_url: "https://github.com/owner/name.git".to_owned(),
            branch: Some("main".to_owned()),
            last_used_at: Timestamp::from_unix_millis(7),
        };
        service.register_cloned_repository(&repository).unwrap();
        assert_eq!(
            service.cached_repositories().unwrap(),
            vec![repository.clone()]
        );
        assert!(
            service
                .cached_mirror(&repository.clone_url)
                .unwrap()
                .is_some()
        );

        // Reloading from disk keeps the entry, then pruning removes it with the mirror.
        let reloaded = RepositoryService::new(base.clone());
        assert_eq!(reloaded.cached_repositories().unwrap().len(), 1);
        fs::remove_dir_all(&mirror).unwrap();
        assert!(reloaded.cached_repositories().unwrap().is_empty());
        assert!(
            reloaded
                .cached_mirror(&repository.clone_url)
                .unwrap()
                .is_none()
        );
        let _ = fs::remove_dir_all(&base);
    }

    fn refresh(clone_url: &str, refreshed_at: Timestamp) -> CachedMirrorRefresh {
        CachedMirrorRefresh {
            clone_url: clone_url.to_owned(),
            refreshed_at,
            default_branch: Some("main".to_owned()),
            revision: Some("a1b2c3".to_owned()),
        }
    }

    #[test]
    fn mirror_refresh_records_are_written_beside_the_mirror() {
        let base = std::env::temp_dir().join(format!("loom-cache-{}", uuid::Uuid::new_v4()));
        let service = RepositoryService::new(base.clone());
        let clone_url = "https://github.com/owner/name.git";
        let mirror = service.mirror_path(clone_url).unwrap();
        fs::create_dir_all(&mirror).unwrap();
        service
            .register_cloned_repository(&ClonedRepository {
                full_name: "owner/name".to_owned(),
                clone_url: clone_url.to_owned(),
                branch: Some("main".to_owned()),
                last_used_at: Timestamp::from_unix_millis(1),
            })
            .unwrap();

        assert_eq!(service.mirror_refresh(clone_url).unwrap(), None);
        let record = refresh(clone_url, Timestamp::from_unix_millis(1_000));
        service.record_mirror_refresh(&record).unwrap();

        // The record is a sibling of the mirror directory, and reloading the
        // service reads the age it actually recorded.
        let record_path = base.join(format!(
            "{}.refresh.json",
            mirror.file_name().unwrap().to_string_lossy()
        ));
        assert!(record_path.is_file());
        let reloaded = RepositoryService::new(base.clone());
        let stored = reloaded.mirror_refresh(clone_url).unwrap().unwrap();
        assert_eq!(stored, record);
        assert_eq!(stored.age_millis(Timestamp::from_unix_millis(3_500)), 2_500);
        // A record dated after the reading clock has no negative age.
        assert_eq!(stored.age_millis(Timestamp::from_unix_millis(10)), 0);

        // A record that cannot be read is no record at all, so its mirror is
        // reported with unknown freshness instead of as current.
        fs::write(&record_path, "not json").unwrap();
        assert_eq!(reloaded.mirror_refresh(clone_url).unwrap(), None);

        // Removing the mirror removes its record with it.
        service.record_mirror_refresh(&record).unwrap();
        fs::remove_dir_all(&mirror).unwrap();
        assert!(reloaded.cached_repositories().unwrap().is_empty());
        assert_eq!(reloaded.mirror_refresh(clone_url).unwrap(), None);
        assert!(!record_path.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn mirror_freshness_states_the_recorded_age_or_the_unknown_case() {
        let now = Timestamp::from_unix_millis(60_000);
        let refreshed = MirrorFreshness::Refreshed(refresh(
            "https://github.com/owner/name.git",
            Timestamp::from_unix_millis(59_000),
        ));
        assert!(!refreshed.is_unknown());
        assert_eq!(
            refreshed.summary(now),
            "reused, mirror refreshed 1000 ms ago to main at a1b2c3"
        );

        let unknown = MirrorFreshness::Unknown {
            last_refresh: Some(refresh(
                "https://github.com/owner/name.git",
                Timestamp::from_unix_millis(59_000),
            )),
            warning: "remote unreachable".to_owned(),
        };
        assert!(unknown.is_unknown());
        assert_eq!(
            unknown.summary(now),
            "reused, freshness unknown: the mirror refresh failed (remote unreachable); \
             the last successful refresh was 1000 ms ago"
        );

        let unrecorded = MirrorFreshness::Unknown {
            last_refresh: None,
            warning: "remote unreachable".to_owned(),
        };
        assert_eq!(
            unrecorded.summary(now),
            "reused, freshness unknown: the mirror refresh failed (remote unreachable) \
             and no successful refresh is recorded"
        );

        // A record that names no default branch still describes itself.
        let mut partial = refresh("https://github.com/owner/name.git", now);
        partial.default_branch = None;
        assert_eq!(
            MirrorFreshness::Refreshed(partial.clone()).summary(now),
            "reused, mirror refreshed 0 ms ago at a1b2c3"
        );
        partial.revision = None;
        assert_eq!(
            MirrorFreshness::Refreshed(partial.clone()).summary(now),
            "reused, mirror refreshed 0 ms ago"
        );
        partial.default_branch = Some("main".to_owned());
        assert_eq!(
            MirrorFreshness::Refreshed(partial).summary(now),
            "reused, mirror refreshed 0 ms ago to main"
        );
    }
}
