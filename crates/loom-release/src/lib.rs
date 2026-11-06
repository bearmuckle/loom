//! The published `loom-server` release archives: where they are, what they
//! contain, and how to obtain one that matches its checksum.
//!
//! Both ends of Loom use this: a client installs a worker (locally or over
//! SSH), and a worker updates itself. Keeping the asset naming, the checksum
//! manifest, and the extraction here means the two paths verify a download the
//! same way.
//!
//! Nothing here writes a durable install; callers decide where a verified
//! archive goes. [`GitHubReleases`] is the network-backed [`ReleaseSource`];
//! tests substitute a fixture so no test touches the network.

use std::{
    collections::BTreeMap,
    env, fs, io,
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use loom_core::{ErrorCode, LoomError, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The repository whose releases carry `loom-server` archives.
pub const RELEASE_REPOSITORY: &str = "bearmuckle/loom";
/// The bound on one release API call or archive download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// The executable name of the server on this platform.
pub fn server_executable_name() -> &'static str {
    if cfg!(windows) {
        "loom-server.exe"
    } else {
        "loom-server"
    }
}

/// The release platform label of the host, as used in archive names, or `None`
/// for a host no release is built for.
pub fn host_platform() -> Option<&'static str> {
    platform_label(env::consts::OS, env::consts::ARCH)
}

pub fn platform_label(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "x86_64" | "aarch64") => Some("macos-universal"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        _ => None,
    }
}

/// The archive format of a release asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// The `loom-server` archive name for `tag` on `platform`.
pub fn asset_name(tag: &str, platform: &str) -> (String, ArchiveKind) {
    if platform.starts_with("windows") {
        (
            format!("loom-server-{tag}-{platform}.zip"),
            ArchiveKind::Zip,
        )
    } else {
        (
            format!("loom-server-{tag}-{platform}.tar.gz"),
            ArchiveKind::TarGz,
        )
    }
}

/// One asset attached to a release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// The parts of a GitHub release the resolver reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReleaseInfo {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

impl ReleaseInfo {
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// Where releases come from. The GitHub implementation is used in production;
/// tests substitute a local fixture so no network is touched.
pub trait ReleaseSource {
    /// The release tagged `tag`, or the latest release when `tag` is `None`.
    /// `Ok(None)` means the tag has no release.
    fn release(&self, tag: Option<&str>) -> Result<Option<ReleaseInfo>>;
    /// The bytes behind an asset's download URL.
    fn download(&self, url: &str) -> Result<Vec<u8>>;
}

/// [`ReleaseSource`] backed by the GitHub releases API.
pub struct GitHubReleases {
    repository: String,
}

impl Default for GitHubReleases {
    fn default() -> Self {
        Self {
            repository: RELEASE_REPOSITORY.to_owned(),
        }
    }
}

impl GitHubReleases {
    fn get(&self, url: &str, accept: &str) -> Result<(u16, Vec<u8>)> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| internal(format!("could not start the download runtime: {error}")))?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(DOWNLOAD_TIMEOUT)
                .connect_timeout(Duration::from_secs(10))
                .user_agent(format!("loom/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| network_error(url, error))?;
            let response = client
                .get(url)
                .header("Accept", accept)
                .send()
                .await
                .map_err(|error| network_error(url, error))?;
            let status = response.status().as_u16();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| network_error(url, error))?;
            Ok((status, bytes.to_vec()))
        })
    }
}

impl ReleaseSource for GitHubReleases {
    fn release(&self, tag: Option<&str>) -> Result<Option<ReleaseInfo>> {
        let url = match tag {
            Some(tag) => format!(
                "https://api.github.com/repos/{}/releases/tags/{tag}",
                self.repository
            ),
            None => format!(
                "https://api.github.com/repos/{}/releases/latest",
                self.repository
            ),
        };
        let (status, body) = self.get(&url, "application/vnd.github+json")?;
        if status == 404 {
            return Ok(None);
        }
        if status >= 400 {
            return Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("GitHub release lookup '{url}' failed with HTTP {status}"),
                true,
            ));
        }
        serde_json::from_slice(&body).map(Some).map_err(|error| {
            LoomError::malformed_payload(format!("GitHub release '{url}' is not valid: {error}"))
        })
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let (status, body) = self.get(url, "application/octet-stream")?;
        if status >= 400 {
            return Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("download of '{url}' failed with HTTP {status}"),
                true,
            ));
        }
        Ok(body)
    }
}

fn network_error(url: &str, error: reqwest::Error) -> LoomError {
    LoomError::new(
        ErrorCode::ProviderUnavailable,
        format!("could not reach '{url}': {error}"),
        true,
    )
}

fn internal(message: String) -> LoomError {
    LoomError::new(ErrorCode::Internal, message, false)
}

fn io_error(context: &str, path: &Path, error: io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("could not {context} '{}': {error}", path.display()),
        false,
    )
}

/// Parses a `sha256sum` manifest into file name to lowercase hex digest.
pub fn parse_sha256sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (digest, name) = line.trim().split_once(char::is_whitespace)?;
            // `sha256sum` marks binary-mode entries with a leading `*`.
            let name = name.trim_start().trim_start_matches('*');
            let digest = digest.to_ascii_lowercase();
            (digest.len() == 64 && digest.chars().all(|value| value.is_ascii_hexdigit()))
                .then(|| (name.to_owned(), digest))
        })
        .collect()
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Refuses `archive` unless the manifest names `name` with the archive's digest.
pub fn verify_checksum(sums: &BTreeMap<String, String>, name: &str, archive: &[u8]) -> Result<()> {
    let expected = sums.get(name).ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("SHA256SUMS has no entry for '{name}'; refusing to install it"),
            false,
        )
    })?;
    let actual = sha256_hex(archive);
    if &actual != expected {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!(
                "'{name}' does not match SHA256SUMS (expected {expected}, got {actual}); \
                 refusing to install it"
            ),
            false,
        ));
    }
    Ok(())
}

/// Downloads the archive of `release` for `platform` and checks it against the
/// release's `SHA256SUMS`.
pub fn fetch_verified(
    source: &dyn ReleaseSource,
    release: &ReleaseInfo,
    platform: &str,
) -> Result<(Vec<u8>, ArchiveKind)> {
    let tag = release.tag_name.as_str();
    let (name, kind) = asset_name(tag, platform);
    let asset = release.asset(&name).ok_or_else(|| {
        LoomError::new(
            ErrorCode::NotFound,
            format!("release {tag} has no asset '{name}'"),
            false,
        )
    })?;
    let sums_asset = release.asset("SHA256SUMS").ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("release {tag} has no SHA256SUMS; refusing an unverified download"),
            false,
        )
    })?;
    log::info!("downloading {name} from release {tag}");
    let sums = source.download(&sums_asset.browser_download_url)?;
    let sums = parse_sha256sums(&String::from_utf8_lossy(&sums));
    let archive = source.download(&asset.browser_download_url)?;
    verify_checksum(&sums, &name, &archive)?;
    Ok((archive, kind))
}

/// Extracts the `loom-server` executable from a release archive into
/// `destination`. Only a regular file whose base name is the executable name is
/// taken, so archive paths can never write outside `destination`.
pub fn extract_server(archive: &[u8], kind: ArchiveKind, destination: &Path) -> Result<()> {
    let wanted = server_executable_name();
    let malformed = |error: String| LoomError::malformed_payload(error);
    let mut output =
        fs::File::create(destination).map_err(|error| io_error("create", destination, error))?;
    let found = match kind {
        ArchiveKind::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
            let entries = tar
                .entries()
                .map_err(|error| malformed(format!("server archive is not a tarball: {error}")))?;
            let mut found = false;
            for entry in entries {
                let mut entry = entry
                    .map_err(|error| malformed(format!("server archive is damaged: {error}")))?;
                let is_server = entry.header().entry_type().is_file()
                    && entry
                        .path()
                        .ok()
                        .and_then(|path| path.file_name().map(|name| name == wanted))
                        .unwrap_or(false);
                if is_server {
                    io::copy(&mut entry, &mut output)
                        .map_err(|error| io_error("write", destination, error))?;
                    found = true;
                    break;
                }
            }
            found
        }
        ArchiveKind::Zip => {
            let mut zip = zip::ZipArchive::new(io::Cursor::new(archive))
                .map_err(|error| malformed(format!("server archive is not a zip: {error}")))?;
            let mut found = false;
            for index in 0..zip.len() {
                let mut file = zip
                    .by_index(index)
                    .map_err(|error| malformed(format!("server archive is damaged: {error}")))?;
                let is_server = file.is_file()
                    && file
                        .enclosed_name()
                        .and_then(|path| path.file_name().map(|name| name == wanted))
                        .unwrap_or(false);
                if is_server {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).map_err(|error| {
                        malformed(format!("server archive is damaged: {error}"))
                    })?;
                    output
                        .write_all(&bytes)
                        .map_err(|error| io_error("write", destination, error))?;
                    found = true;
                    break;
                }
            }
            found
        }
    };
    if !found {
        return Err(malformed(format!(
            "server archive does not contain '{wanted}'"
        )));
    }
    output
        .sync_all()
        .map_err(|error| io_error("write", destination, error))?;
    make_executable(destination)
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|error| io_error("mark executable", path, error))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Parses a stable release tag such as `v0.11.0` into a sortable triple.
///
/// A tag with a pre-release or build suffix (`v0.12.0-rc.1`) is not stable and
/// parses to `None`, so it can never be selected as an automatic update.
pub fn parse_release_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v')?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `latest` is a stable release newer than `running`.
///
/// Both must be release tags; a development build (`0.1.0`) never updates
/// itself, and a pre-release tag never becomes an update target.
pub fn is_newer_stable(running: &str, latest: &str) -> bool {
    match (parse_release_tag(running), parse_release_tag(latest)) {
        (Some(running), Some(latest)) => latest > running,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
