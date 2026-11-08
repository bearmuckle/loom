use super::*;

/// Repositories fetched per page when listing the account's repositories.
const GITHUB_REPOSITORY_PAGE_SIZE: usize = 100;
/// Safety cap so a huge account cannot make the picker page forever.
const GITHUB_REPOSITORY_MAX_PAGES: usize = 100;
/// Maximum number of matches returned to the picker.
const GITHUB_REPOSITORY_MAX_RESULTS: usize = 50;

pub(crate) fn search_github_repositories(
    token: &str,
    query: &str,
) -> Result<Vec<GitHubRepository>> {
    search_github_repositories_at(token, query, "https://api.github.com/user/repos")
}

/// Lists the repositories the authenticated account can access and keeps the
/// ones matching `query`.
///
/// GitHub's global `/search/repositories` endpoint searches public repositories
/// across all of GitHub, which is not what the source picker wants. The
/// `/user/repos` endpoint instead returns exactly the repositories the account
/// can access: owned, collaborator, and organization-member repositories,
/// including private ones. Those pages are filtered locally, so the picker only
/// ever offers repositories the account can clone.
pub(crate) fn search_github_repositories_at(
    token: &str,
    query: &str,
    endpoint: &str,
) -> Result<Vec<GitHubRepository>> {
    let query = query.trim();
    if query.chars().count() < GITHUB_REPOSITORY_QUERY_MIN_CHARS {
        return Err(LoomError::invalid_request(format!(
            "GitHub repository search requires at least {GITHUB_REPOSITORY_QUERY_MIN_CHARS} characters"
        )));
    }
    let needle = github_search_needle(query);
    run_github_async(async {
        let client = loom_providers::shared_http_client()?;
        let mut matches = Vec::new();
        for page in 1..=GITHUB_REPOSITORY_MAX_PAGES {
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
                        format!("could not list GitHub repositories: {error}"),
                        true,
                    )
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    format!(
                        "could not list GitHub repositories (HTTP {})",
                        status.as_u16()
                    ),
                    true,
                ));
            }
            let page_repositories: Vec<GitHubApiRepository> =
                response.json().await.map_err(|error| {
                    LoomError::new(
                        ErrorCode::ProviderInvalidResponse,
                        format!("GitHub returned an invalid repository list: {error}"),
                        false,
                    )
                })?;
            let page_len = page_repositories.len();
            matches.extend(page_repositories.into_iter().filter_map(|repository| {
                github_repository_rank(&repository, &needle).map(|rank| (rank, repository))
            }));
            if page_len < GITHUB_REPOSITORY_PAGE_SIZE {
                break;
            }
        }
        // Best match first: a name match outranks a description match, and
        // ties fall back to an alphabetical order for a stable picker.
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
    })
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
