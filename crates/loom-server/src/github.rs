use super::*;
use sha2::{Digest, Sha256};

/// Repositories fetched per page when listing the account's repositories.
const GITHUB_REPOSITORY_PAGE_SIZE: usize = 100;
/// Pages one listing walk may read, so a rare query on a huge account cannot
/// page for long. One listing page holds [`GITHUB_REPOSITORY_PAGE_SIZE`]
/// repositories, so a walk covers at most 1000 repositories.
const GITHUB_REPOSITORY_MAX_PAGES: usize = 10;
/// Maximum number of matches returned to the picker.
const GITHUB_REPOSITORY_MAX_RESULTS: usize = 50;
/// How long a credential's repository listing may be reused. Consecutive
/// keystrokes in the source picker arrive seconds apart and an account's
/// repository set changes rarely, so a minute removes almost every repeated
/// listing walk while a repository created or shared meanwhile still appears
/// within the minute.
const GITHUB_REPOSITORY_LISTING_TTL: Duration = Duration::from_secs(60);
/// How long one page request may take. A person waits on this request in the
/// picker, so it is far below the 30 s a background provider call allows.
const GITHUB_REPOSITORY_PAGE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one search may spend fetching pages across all batches. A search
/// that runs out of time returns the matches it already has.
const GITHUB_REPOSITORY_LISTING_DEADLINE: Duration = Duration::from_secs(5);
/// How many page requests one search keeps in flight.
const GITHUB_REPOSITORY_PAGE_CONCURRENCY: usize = 4;
/// How many credentials' listings the cache keeps. A worker serves a handful of
/// accounts, and dropping the least recently used listing keeps the memory of
/// the cache bounded however many credentials a worker sees.
const GITHUB_REPOSITORY_LISTING_ENTRIES: usize = 4;
/// The listing of the repositories the authenticated account can access.
const GITHUB_REPOSITORY_ENDPOINT: &str = "https://api.github.com/user/repos";

/// The bounds of one GitHub repository search.
///
/// The defaults are what the source picker runs with. Tests set their own so
/// they can assert request counts, cache reuse, and degradation without
/// sleeping for a production timeout.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GitHubRepositorySearchOptions {
    /// How long one page request may take.
    pub(crate) page_timeout: Duration,
    /// How long one search may spend fetching pages.
    pub(crate) listing_deadline: Duration,
    /// How long a fetched listing may be reused.
    pub(crate) listing_ttl: Duration,
    /// How many page requests are in flight at once.
    pub(crate) page_concurrency: usize,
    /// How many pages one listing may read.
    pub(crate) max_pages: usize,
}

impl Default for GitHubRepositorySearchOptions {
    fn default() -> Self {
        Self {
            page_timeout: GITHUB_REPOSITORY_PAGE_TIMEOUT,
            listing_deadline: GITHUB_REPOSITORY_LISTING_DEADLINE,
            listing_ttl: GITHUB_REPOSITORY_LISTING_TTL,
            page_concurrency: GITHUB_REPOSITORY_PAGE_CONCURRENCY,
            max_pages: GITHUB_REPOSITORY_MAX_PAGES,
        }
    }
}

/// A cached listing of the repositories one credential can access.
///
/// The entry holds the pages read so far, not the matches of one query: a later
/// keystroke filters the same pages and, when it needs more, continues the
/// listing from the first page it has not read.
#[derive(Clone)]
struct CachedGitHubRepositoryListing {
    /// When this listing last read a page; the entry's age is measured from
    /// here, so reading more pages also extends the listing's life.
    updated_at: Instant,
    /// The pages read so far, in listing order.
    repositories: Vec<GitHubApiRepository>,
    pages_read: usize,
    /// Whether the listing ended, so no further page can add a repository.
    exhausted: bool,
}

impl CachedGitHubRepositoryListing {
    fn new() -> Self {
        Self {
            updated_at: Instant::now(),
            repositories: Vec::new(),
            pages_read: 0,
            exhausted: false,
        }
    }

    /// The cached repositories that match an already-normalized `needle`.
    fn matched(&self, needle: &str) -> Vec<(u8, GitHubApiRepository)> {
        self.repositories
            .iter()
            .filter_map(|repository| {
                github_repository_rank(repository, needle).map(|rank| (rank, repository.clone()))
            })
            .collect()
    }

    /// Whether this listing can still gain repositories.
    fn can_read_more(&self, options: GitHubRepositorySearchOptions) -> bool {
        !self.exhausted && self.pages_read < options.max_pages
    }
}

/// The accessible-repository listings this worker has fetched, keyed by a
/// digest of the credential and endpoint.
///
/// The key is a SHA-256 digest, so the cache holds no credential and nothing
/// read out of it can recover one. A different token hashes to a different key,
/// so one account's listing is never served to another credential. A listing
/// expires after [`GitHubRepositorySearchOptions::listing_ttl`] and the cache
/// keeps only the most recently read [`GITHUB_REPOSITORY_LISTING_ENTRIES`].
#[derive(Default)]
pub(crate) struct GitHubRepositoryListingCache {
    listings: Mutex<BTreeMap<String, CachedGitHubRepositoryListing>>,
}

impl GitHubRepositoryListingCache {
    /// The unexpired listing of `key`, if the cache has one. Expired entries are
    /// dropped as they are found.
    fn listing(&self, key: &str, ttl: Duration) -> Result<Option<CachedGitHubRepositoryListing>> {
        let mut listings = self.listings.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "GitHub repository listing cache lock was poisoned",
                true,
            )
        })?;
        listings.retain(|_, listing| listing.updated_at.elapsed() < ttl);
        Ok(listings.get(key).cloned())
    }

    /// Caches the pages read for `key`, dropping the least recently read
    /// listing when the cache is full.
    fn store(&self, key: String, listing: CachedGitHubRepositoryListing) -> Result<()> {
        let mut listings = self.listings.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "GitHub repository listing cache lock was poisoned",
                true,
            )
        })?;
        listings.insert(key, listing);
        while listings.len() > GITHUB_REPOSITORY_LISTING_ENTRIES {
            let oldest = listings
                .iter()
                .min_by_key(|(_, listing)| listing.updated_at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    listings.remove(&key);
                }
                None => break,
            }
        }
        Ok(())
    }
}

/// The cache key of one credential's listing of an endpoint.
///
/// The key is a digest rather than the token itself: the cache never holds a
/// credential and a digest cannot be turned back into one.
fn github_listing_cache_key(token: &str, endpoint: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"loom github repository listing\0");
    hasher.update(endpoint.as_bytes());
    hasher.update(b"\0");
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Lists the repositories the authenticated account can access and keeps the
/// ones matching `query`, without reusing an earlier listing.
///
/// The source picker calls [`InProcessBackend::search_github_repositories`]
/// instead, which caches the listing; this entry point exists so a test can
/// point the search at a fixture endpoint.
#[cfg(test)]
pub(crate) fn search_github_repositories_at(
    token: &str,
    query: &str,
    endpoint: &str,
) -> Result<Vec<GitHubRepository>> {
    search_github_repositories_with(
        token,
        query,
        endpoint,
        GitHubRepositorySearchOptions::default(),
        &GitHubRepositoryListingCache::default(),
    )
}

/// Lists the repositories the authenticated account can access and keeps the
/// ones matching `query`, reusing `cache` and the given `options`.
///
/// GitHub's global `/search/repositories` endpoint searches public repositories
/// across all of GitHub, which is not what the source picker wants. The
/// `/user/repos` endpoint instead returns exactly the repositories the account
/// can access: owned, collaborator, and organization-member repositories,
/// including private ones. Those pages are filtered locally, so the picker only
/// ever offers repositories the account can clone.
///
/// Paging stops as soon as enough matches are found, when the listing ends,
/// when the page budget is read, or when the search runs out of time. A page
/// that fails after matches exist leaves those matches returned, so a slow or
/// briefly failing page degrades the result instead of failing the search.
pub(crate) fn search_github_repositories_with(
    token: &str,
    query: &str,
    endpoint: &str,
    options: GitHubRepositorySearchOptions,
    cache: &GitHubRepositoryListingCache,
) -> Result<Vec<GitHubRepository>> {
    let query = query.trim();
    if query.chars().count() < GITHUB_REPOSITORY_QUERY_MIN_CHARS {
        return Err(LoomError::invalid_request(format!(
            "GitHub repository search requires at least {GITHUB_REPOSITORY_QUERY_MIN_CHARS} characters"
        )));
    }
    let needle = github_search_needle(query);
    let key = github_listing_cache_key(token, endpoint);
    let mut matches = run_github_async(async {
        let mut listing = cache
            .listing(&key, options.listing_ttl)?
            .unwrap_or_else(CachedGitHubRepositoryListing::new);
        let pages_before = listing.pages_read;
        let mut matches = listing.matched(&needle);
        if matches.len() < GITHUB_REPOSITORY_MAX_RESULTS && listing.can_read_more(options) {
            match read_github_repository_pages(
                endpoint,
                token,
                &needle,
                options,
                &mut listing,
                &mut matches,
            )
            .await
            {
                Ok(()) => {}
                Err(error) => {
                    // A failure with nothing to show is the search's failure; a
                    // failure after some matches keeps them, because a picker
                    // that shows a few results beats one that shows an error.
                    if matches.is_empty() && listing.pages_read == 0 {
                        return Err(error);
                    }
                    log::debug!(
                        "[loom-server] GitHub repository search kept {} matches after reading {} pages: {error}",
                        matches.len(),
                        listing.pages_read
                    );
                }
            }
        }
        if listing.pages_read > pages_before {
            cache.store(key, listing)?;
        }
        Ok(matches)
    })?;
    // Best match first: a name match outranks a description match, and
    // ties fall back to an alphabetical order for a stable picker.
    //
    // The ranking is exact for the pages this search read. The listing is
    // ordered by update recency, so a search that stopped early can miss a
    // better-ranked repository whose last update is older than the pages it
    // read; it never returns a worse-ranked one in place of a better one it
    // saw.
    matches.sort_by(|(left_rank, left), (right_rank, right)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| {
                left.full_name
                    .to_lowercase()
                    .cmp(&right.full_name.to_lowercase())
            })
            .then_with(|| left.full_name.cmp(&right.full_name))
    });
    let mut seen = std::collections::HashSet::new();
    matches.retain(|(_, repository)| seen.insert(repository.full_name.clone()));
    matches.truncate(GITHUB_REPOSITORY_MAX_RESULTS);
    Ok(matches
        .into_iter()
        .map(|(_, repository)| GitHubRepository {
            full_name: repository.full_name,
            description: repository.description,
            clone_url: repository.clone_url,
            private: repository.private,
            default_branch: repository.default_branch,
        })
        .collect())
}

impl InProcessBackend {
    /// Searches the GitHub repositories the configured account can access,
    /// reusing this backend's listing cache so the keystrokes of one picker
    /// session filter a cached listing instead of listing the account again.
    pub(crate) fn search_github_repositories(&self, query: &str) -> Result<Vec<GitHubRepository>> {
        let token = self.providers.github_account_token()?;
        search_github_repositories_with(
            &token,
            query,
            GITHUB_REPOSITORY_ENDPOINT,
            GitHubRepositorySearchOptions::default(),
            self.repository_service.github_repository_listings(),
        )
    }
}

/// Reads further pages of `listing` into `matches` until the search has enough
/// matches, the listing ends, or the page budget, deadline, or a failing page
/// stops it.
///
/// The first page is requested alone because it decides most searches; a search
/// that needs more then reads the following pages concurrently, bounded by
/// [`GitHubRepositorySearchOptions::page_concurrency`]. A page that fails stops
/// the walk after the pages before it have been added, so the caller keeps the
/// prefix that was read.
async fn read_github_repository_pages(
    endpoint: &str,
    token: &str,
    needle: &str,
    options: GitHubRepositorySearchOptions,
    listing: &mut CachedGitHubRepositoryListing,
    matches: &mut Vec<(u8, GitHubApiRepository)>,
) -> Result<()> {
    let client = loom_providers::shared_http_client()?;
    let started = Instant::now();
    while listing.can_read_more(options)
        && matches.len() < GITHUB_REPOSITORY_MAX_RESULTS
        && started.elapsed() < options.listing_deadline
    {
        let remaining = options.max_pages - listing.pages_read;
        let first_page = listing.pages_read + 1;
        let batch = if listing.pages_read == 0 {
            1
        } else {
            options.page_concurrency.max(1)
        }
        .min(remaining);
        let responses =
            futures_util::future::join_all((first_page..first_page + batch).map(|page| {
                fetch_github_repository_page(&client, endpoint, token, page, options.page_timeout)
            }))
            .await;
        for (offset, response) in responses.into_iter().enumerate() {
            let page = first_page + offset;
            let repositories = response?;
            let short_page = repositories.len() < GITHUB_REPOSITORY_PAGE_SIZE;
            listing.pages_read = page;
            listing.updated_at = Instant::now();
            listing.repositories.extend(repositories.iter().cloned());
            matches.extend(repositories.into_iter().filter_map(|repository| {
                github_repository_rank(&repository, needle).map(|rank| (rank, repository))
            }));
            if short_page {
                listing.exhausted = true;
                return Ok(());
            }
            if matches.len() >= GITHUB_REPOSITORY_MAX_RESULTS {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Fetches one page of the authenticated account's repositories.
async fn fetch_github_repository_page(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    page: usize,
    timeout: Duration,
) -> Result<Vec<GitHubApiRepository>> {
    let mut url = url::Url::parse(endpoint).map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not build the GitHub repository URL: {error}"),
            false,
        )
    })?;
    url.query_pairs_mut()
        .append_pair("per_page", &GITHUB_REPOSITORY_PAGE_SIZE.to_string())
        .append_pair("sort", "updated")
        .append_pair("direction", "desc")
        .append_pair("affiliation", "owner,collaborator,organization_member")
        .append_pair("page", &page.to_string());
    let response = client
        .get(url)
        .timeout(timeout)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "Loom")
        .send()
        .await
        .map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderAuthentication,
                format!("could not list GitHub repositories: {error}"),
                true,
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(github_listing_status_error(
            status.as_u16(),
            response.headers(),
        ));
    }
    response.json().await.map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("GitHub returned an invalid repository list: {error}"),
            false,
        )
    })
}

/// Maps a failed listing response to an error.
///
/// A rate limit is reported as its own condition with the wait GitHub asked
/// for, because it is the one failure a person can wait out. Any other failure,
/// including a 403 without rate-limit headers, keeps the authentication error
/// the picker already surfaces.
fn github_listing_status_error(status: u16, headers: &reqwest::header::HeaderMap) -> LoomError {
    if let Some(retry_after) = github_rate_limit_wait(status, headers) {
        let message = match retry_after {
            Some(wait) => format!(
                "GitHub repository search is rate limited; retry after {} s (HTTP {status})",
                wait.as_secs().max(1)
            ),
            None => format!(
                "GitHub repository search is rate limited; retry after the rate limit resets (HTTP {status})"
            ),
        };
        return LoomError::new(ErrorCode::ProviderRateLimited, message, true)
            .with_retry_after(retry_after);
    }
    LoomError::new(
        ErrorCode::ProviderAuthentication,
        format!("could not list GitHub repositories (HTTP {status})"),
        true,
    )
}

/// Whether a failed listing response is a rate limit, and how long GitHub said
/// to wait when it said so.
///
/// `None` means the response is not a rate limit: a 403 without a rate-limit
/// header is a permission failure. `Some(None)` is a rate limit whose wait the
/// response did not state.
fn github_rate_limit_wait(
    status: u16,
    headers: &reqwest::header::HeaderMap,
) -> Option<Option<Duration>> {
    if status != 403 && status != 429 {
        return None;
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .map(str::to_owned)
    };
    let exhausted = header("x-ratelimit-remaining").as_deref() == Some("0");
    let retry_after = header("retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    let reset = header("x-ratelimit-reset")
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(github_wait_until_reset);
    if status == 429 || exhausted || retry_after.is_some() || reset.is_some() {
        Some(retry_after.or(reset))
    } else {
        None
    }
}

/// How long until GitHub's rate limit resets, given the Unix second it resets
/// at. A reset time that has already passed states no wait.
fn github_wait_until_reset(reset_unix_seconds: u64) -> Option<Duration> {
    let now_unix_seconds = Timestamp::now().as_unix_millis() / 1_000;
    reset_unix_seconds
        .checked_sub(now_unix_seconds)
        .filter(|wait| *wait > 0)
        .map(Duration::from_secs)
}

/// The commit identity Loom attributes its commits to, derived from the
/// authenticated GitHub account's verified `users.noreply.github.com` address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitHubAccountIdentity {
    pub(crate) name: String,
    pub(crate) email: String,
}

/// Fetches the identity of the authenticated GitHub account.
pub(crate) fn github_account_identity(token: &str) -> Result<GitHubAccountIdentity> {
    github_account_identity_at(token, "https://api.github.com/user")
}

/// Chooses the commit identity for a newly attached checkout.
///
/// The source repository's own identity wins because a local import already
/// knows how the contributor signs commits. A hosted GitHub checkout has no
/// source identity to copy, so it uses the authenticated account instead.
/// Anything else keeps the host-provided identity.
pub(crate) fn resolve_attached_identity(
    source_identity: Option<(String, String)>,
    source_is_github: bool,
    account_identity: Option<GitHubAccountIdentity>,
) -> Option<(String, String)> {
    if source_identity.is_some() {
        return source_identity;
    }
    if source_is_github {
        return account_identity.map(|identity| (identity.name, identity.email));
    }
    None
}

/// Fetches the authenticated account identity from an explicit endpoint,
/// extracted so tests can point it at a local fixture server.
pub(crate) fn github_account_identity_at(
    token: &str,
    endpoint: &str,
) -> Result<GitHubAccountIdentity> {
    let account = fetch_github_account_at(token, endpoint)?;
    Ok(GitHubAccountIdentity {
        name: account
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| account.login.clone()),
        email: format!("{}+{}@users.noreply.github.com", account.id, account.login),
    })
}

/// The immutable account facts a GitHub sign-in needs.
///
/// The numeric id is what identifies a person across a rename and a
/// re-authentication; the login is only the display name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitHubAccountRef {
    pub(crate) id: u64,
    pub(crate) login: String,
}

/// Reads the account a GitHub token belongs to from an explicit endpoint.
///
/// The sign-in path calls this instead of [`github_account_identity_at`] so
/// the commit identity keeps its own extraction rules; the HTTP call and its
/// error handling are shared.
pub(crate) fn github_account_ref_at(token: &str, endpoint: &str) -> Result<GitHubAccountRef> {
    let account = fetch_github_account_at(token, endpoint)?;
    Ok(GitHubAccountRef {
        id: account.id,
        login: account.login,
    })
}

fn fetch_github_account_at(token: &str, endpoint: &str) -> Result<GitHubApiUser> {
    run_github_async(async {
        let client = loom_providers::shared_http_client()?;
        let response = client
            .get(endpoint)
            .timeout(Duration::from_secs(30))
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {token}"))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "Loom")
            .send()
            .await
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    format!("could not read the GitHub account: {error}"),
                    true,
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(LoomError::new(
                ErrorCode::ProviderAuthentication,
                format!(
                    "could not read the GitHub account (HTTP {})",
                    status.as_u16()
                ),
                true,
            ));
        }
        response.json().await.map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("GitHub returned an invalid account: {error}"),
                false,
            )
        })
    })
}

/// Normalizes a picker query for local matching.
///
/// A pasted GitHub URL (or an `owner/name` reference) is reduced to the
/// `owner/name` portion so it matches a repository `full_name`, including when
/// the URL points at a subdirectory or ends in `.git`.
pub(crate) fn github_search_needle(query: &str) -> String {
    let query = query.trim();
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "github.com/",
        "git@github.com:",
    ] {
        if let Some(reference) = query.strip_prefix(prefix) {
            let mut parts = reference.split(['/', '?', '#']);
            let owner = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default().trim_end_matches(".git");
            return match (owner.is_empty(), name.is_empty()) {
                (true, _) => query.to_lowercase(),
                (false, true) => format!("{owner}/").to_lowercase(),
                (false, false) => format!("{owner}/{name}").to_lowercase(),
            };
        }
    }
    query.trim_end_matches(".git").to_lowercase()
}

/// Returns how well `repository` matches an already-normalized `needle`, where
/// a lower rank is a better match. `None` means the repository does not match.
///
/// A name match always outranks a description match so typing a repository name
/// surfaces that repository before others that merely mention the term.
pub(crate) fn github_repository_rank(repository: &GitHubApiRepository, needle: &str) -> Option<u8> {
    let full_name = repository.full_name.to_lowercase();
    let name = full_name.rsplit('/').next().unwrap_or(full_name.as_str());
    if full_name == needle {
        Some(0)
    } else if name == needle {
        Some(1)
    } else if name.starts_with(needle) {
        Some(2)
    } else if name.contains(needle) {
        Some(3)
    } else if full_name.contains(needle) {
        Some(4)
    } else if repository
        .description
        .as_deref()
        .is_some_and(|description| description.to_lowercase().contains(needle))
    {
        Some(5)
    } else {
        None
    }
}

fn run_github_async<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    loom_providers::run_async(future)
}

/// Extracts `owner/name` from a GitHub clone URL. Returns `None` for URLs that
/// are not recognizable GitHub repository URLs.
pub(crate) fn github_repository_full_name(clone_url: &str) -> Option<String> {
    let url = url::Url::parse(clone_url).ok()?;
    if url.host_str() != Some("github.com") {
        return None;
    }
    let mut segments = url.path_segments()?;
    let owner = segments.next()?.to_owned();
    let name = segments.next()?.trim_end_matches(".git").to_owned();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn listing_cache_keys_are_digests_that_differ_per_credential() {
        let endpoint = "https://api.github.com/user/repos";
        let first = github_listing_cache_key("gho_first_secret", endpoint);
        let second = github_listing_cache_key("gho_second_secret", endpoint);

        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        assert!(!first.contains("gho_first_secret"));
        assert_ne!(first, second);
        assert_ne!(
            first,
            github_listing_cache_key("gho_first_secret", "http://127.0.0.1:1/user/repos")
        );
        // The same credential and endpoint always map to the same key.
        assert_eq!(
            first,
            github_listing_cache_key("gho_first_secret", endpoint)
        );
    }

    #[test]
    fn listing_cache_expires_and_keeps_only_a_bounded_number_of_credentials() {
        let cache = GitHubRepositoryListingCache::default();
        let ttl = Duration::from_secs(60);
        for index in 0..GITHUB_REPOSITORY_LISTING_ENTRIES + 1 {
            let key = github_listing_cache_key(&format!("token-{index}"), "endpoint");
            cache
                .store(
                    key.clone(),
                    CachedGitHubRepositoryListing {
                        updated_at: Instant::now() + Duration::from_millis(index as u64),
                        ..CachedGitHubRepositoryListing::new()
                    },
                )
                .unwrap();
            assert!(cache.listing(&key, ttl).unwrap().is_some());
        }
        assert_eq!(
            cache.listings.lock().unwrap().len(),
            GITHUB_REPOSITORY_LISTING_ENTRIES
        );

        // An expired listing is dropped instead of being served again.
        let key = github_listing_cache_key("token-0", "endpoint");
        assert!(cache.listing(&key, Duration::ZERO).unwrap().is_none());
        assert!(cache.listings.lock().unwrap().is_empty());
    }

    #[test]
    fn rate_limit_waits_come_from_the_retry_headers() {
        // A 403 without a rate-limit header is a permission failure.
        assert_eq!(github_rate_limit_wait(403, &HeaderMap::new()), None);
        assert_eq!(github_rate_limit_wait(401, &HeaderMap::new()), None);
        assert_eq!(
            github_listing_status_error(403, &HeaderMap::new()).code,
            ErrorCode::ProviderAuthentication
        );

        // `Retry-After` wins over the reset time.
        let wait = github_rate_limit_wait(
            403,
            &headers(&[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "0"),
                ("retry-after", "12"),
            ]),
        );
        assert_eq!(wait, Some(Some(Duration::from_secs(12))));

        // An exhausted window without a stated wait still is a rate limit, and
        // a reset in the past states no wait.
        assert_eq!(
            github_rate_limit_wait(403, &headers(&[("x-ratelimit-remaining", "0")])),
            Some(None)
        );
        assert_eq!(
            github_rate_limit_wait(429, &headers(&[("x-ratelimit-reset", "1")])),
            Some(None)
        );

        // Without `Retry-After` the reset time states the wait.
        let now = Timestamp::now().as_unix_millis() / 1_000;
        assert_eq!(
            github_rate_limit_wait(
                403,
                &headers(&[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", &(now + 30).to_string()),
                ]),
            ),
            Some(Some(Duration::from_secs(30)))
        );

        // A 429 is a rate limit even without any rate-limit header.
        assert_eq!(github_rate_limit_wait(429, &HeaderMap::new()), Some(None));
    }

    #[test]
    fn rate_limit_errors_name_the_condition_and_the_wait() {
        let error = github_listing_status_error(
            429,
            &headers(&[("x-ratelimit-remaining", "0"), ("retry-after", "9")]),
        );
        assert_eq!(error.code, ErrorCode::ProviderRateLimited);
        assert!(error.retryable);
        assert_eq!(error.retry_after, Some(Duration::from_secs(9)));
        assert!(error.message.contains("rate limited"));
        assert!(error.message.contains("retry after 9 s"));

        let unstated =
            github_listing_status_error(403, &headers(&[("x-ratelimit-remaining", "0")]));
        assert_eq!(unstated.code, ErrorCode::ProviderRateLimited);
        assert_eq!(unstated.retry_after, None);
        assert!(unstated.message.contains("rate limit resets"));
    }

    #[test]
    fn reset_waits_are_the_time_left_and_never_negative() {
        let now = Timestamp::now().as_unix_millis() / 1_000;
        assert_eq!(
            github_wait_until_reset(now + 30).map(|wait| wait.as_secs()),
            Some(30)
        );
        assert_eq!(github_wait_until_reset(now.saturating_sub(1)), None);
        assert_eq!(github_wait_until_reset(0), None);
    }
}
