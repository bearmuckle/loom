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
        Ok(None)
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
}
