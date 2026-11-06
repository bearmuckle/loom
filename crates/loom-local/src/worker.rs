//! Finding the `loom-server` executable a worker runs.
//!
//! [`ServerResolver`] looks for the executable, in this order:
//!
//! 1. an explicit path (`--server-binary` or `LOOM_SERVER_BINARY`);
//! 2. `loom-server` next to the running executable, as release archives,
//!    the AppImage and the `.deb` install it;
//! 3. `loom-server` on `PATH`;
//! 4. a cached download whose tag equals this client's release tag;
//! 5. a download of that release (or the latest one for a development build),
//!    verified against the release's `SHA256SUMS` before it is extracted;
//! 6. without network, the newest cached download.
//!
//! The [`crate::local`] installer and the SSH provisioner both build on the
//! release lookup and verified download here; neither embeds a backend.

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};

use loom_core::{ErrorCode, LoomError, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The repository whose releases carry `loom-server` archives.
pub const RELEASE_REPOSITORY: &str = "bearmuckle/loom";
/// Environment variable naming an explicit `loom-server` executable.
pub const SERVER_BINARY_ENV: &str = "LOOM_SERVER_BINARY";
/// Environment variable overriding the download cache root.
pub const CACHE_DIR_ENV: &str = "LOOM_CACHE_DIR";
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
    fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
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
                .user_agent(format!("loom-ui/{}", crate::build_version_label()))
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

/// The per-user cache root: `LOOM_CACHE_DIR` verbatim, else the platform cache
/// directory plus `loom` (`$XDG_CACHE_HOME`, `~/.cache`, `~/Library/Caches`,
/// or `%LOCALAPPDATA%`), else a directory in the system temp folder.
pub fn cache_root() -> PathBuf {
    if let Some(path) = env::var_os(CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let platform = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME").map(|home| PathBuf::from(home).join("Library").join("Caches"))
    } else {
        env::var_os("XDG_CACHE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    platform.unwrap_or_else(env::temp_dir).join("loom")
}

/// Parses a release tag such as `v0.11.0` into a sortable triple.
fn parse_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v')?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.split(['-', '+']).next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The release tag this client was built as, when it was built from a tag.
/// A development build (`0.1.0`, a branch build) has none and follows the
/// latest release.
pub fn client_release_tag() -> Option<String> {
    let version = option_env!("LOOM_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    parse_tag(version).map(|_| version.to_owned())
}

/// How the `loom-server` executable is found.
pub struct ServerResolver<'a> {
    /// `--server-binary` or `LOOM_SERVER_BINARY`.
    pub explicit: Option<PathBuf>,
    /// Directories searched before the cache: the running executable's
    /// directory, then each `PATH` entry.
    pub search_dirs: Vec<PathBuf>,
    /// The download cache root; versions live in `<cache_root>/server/<tag>`.
    pub cache_root: PathBuf,
    /// The release tag preferred over the latest release.
    pub preferred_tag: Option<String>,
    /// The host platform label, or `None` when no release fits the host.
    pub platform: Option<&'static str>,
    pub source: &'a dyn ReleaseSource,
}

impl<'a> ServerResolver<'a> {
    /// The resolver the client uses: the real environment and GitHub releases.
    pub fn for_host(explicit: Option<PathBuf>, source: &'a dyn ReleaseSource) -> Self {
        let mut search_dirs = Vec::new();
        if let Some(directory) = env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
        {
            search_dirs.push(directory);
        }
        if let Some(path) = env::var_os("PATH") {
            search_dirs.extend(env::split_paths(&path));
        }
        Self {
            explicit,
            search_dirs,
            cache_root: cache_root(),
            preferred_tag: client_release_tag(),
            platform: host_platform(),
            source,
        }
    }

    fn versions_dir(&self) -> PathBuf {
        self.cache_root.join("server")
    }

    fn cached(&self, tag: &str) -> Option<PathBuf> {
        let path = self.versions_dir().join(tag).join(server_executable_name());
        path.is_file().then_some(path)
    }

    /// The newest cached version, by tag.
    fn newest_cached(&self) -> Option<PathBuf> {
        let entries = fs::read_dir(self.versions_dir()).ok()?;
        entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter_map(|tag| Some((parse_tag(&tag)?, tag)))
            .filter(|(_, tag)| self.cached(tag).is_some())
            .max()
            .and_then(|(_, tag)| self.cached(&tag))
    }

    /// Resolves the executable, downloading it only when nothing local fits.
    pub fn resolve(&self) -> Result<PathBuf> {
        if let Some(path) = &self.explicit {
            if path.is_file() {
                return Ok(path.clone());
            }
            return Err(LoomError::invalid_request(format!(
                "the loom-server executable '{}' named by --server-binary or {SERVER_BINARY_ENV} \
                 does not exist",
                path.display()
            )));
        }
        let name = server_executable_name();
        if let Some(path) = self
            .search_dirs
            .iter()
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
        {
            log::info!("using local worker binary '{}'", path.display());
            return Ok(path);
        }
        if let Some(path) = self
            .preferred_tag
            .as_deref()
            .and_then(|tag| self.cached(tag))
        {
            log::info!("using cached local worker binary '{}'", path.display());
            return Ok(path);
        }
        match self.download() {
            Ok(path) => Ok(path),
            Err(error) => match self.newest_cached() {
                Some(path) => {
                    log::warn!(
                        "could not fetch a loom-server release ({error}); using cached '{}'",
                        path.display()
                    );
                    Ok(path)
                }
                None => Err(LoomError::new(
                    error.code,
                    format!(
                        "no loom-server executable was found next to this client, on PATH, or \
                         in the cache '{}', and downloading one failed: {}. Install loom-server \
                         or pass --server-binary.",
                        self.versions_dir().display(),
                        error.message
                    ),
                    error.retryable,
                )),
            },
        }
    }

    /// Finds the release to install: the preferred tag when it has a release,
    /// otherwise the latest one.
    pub fn release(&self) -> Result<ReleaseInfo> {
        if let Some(tag) = self.preferred_tag.as_deref() {
            if let Some(release) = self.source.release(Some(tag))? {
                return Ok(release);
            }
            log::warn!("release {tag} was not found; using the latest loom-server release");
        }
        self.source.release(None)?.ok_or_else(|| {
            LoomError::new(
                ErrorCode::NotFound,
                format!("{RELEASE_REPOSITORY} has no published release"),
                false,
            )
        })
    }

    fn download(&self) -> Result<PathBuf> {
        let platform = self.platform.ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                format!(
                    "no loom-server release is built for {}-{}",
                    env::consts::OS,
                    env::consts::ARCH
                ),
                false,
            )
        })?;
        let release = self.release()?;
        let tag = release.tag_name.as_str();
        if let Some(path) = self.cached(tag) {
            log::info!("using cached local worker binary '{}'", path.display());
            return Ok(path);
        }
        let (archive, kind) = self.fetch_verified(&release, platform)?;
        self.install(tag, &archive, kind)
    }

    /// Downloads the `loom-server` archive of `release` for `platform` and
    /// checks it against the release's `SHA256SUMS`.
    pub fn fetch_verified(
        &self,
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
        let sums = self.source.download(&sums_asset.browser_download_url)?;
        let sums = parse_sha256sums(&String::from_utf8_lossy(&sums));
        let archive = self.source.download(&asset.browser_download_url)?;
        verify_checksum(&sums, &name, &archive)?;
        Ok((archive, kind))
    }

    /// Extracts into a private staging directory and renames it into place, so
    /// a cache entry is either complete or absent.
    fn install(&self, tag: &str, archive: &[u8], kind: ArchiveKind) -> Result<PathBuf> {
        let versions = self.versions_dir();
        fs::create_dir_all(&versions).map_err(|error| io_error("create", &versions, error))?;
        let staging = versions.join(format!(".{tag}.{}", uuid::Uuid::new_v4()));
        fs::create_dir(&staging).map_err(|error| io_error("create", &staging, error))?;
        let result = extract_server(archive, kind, &staging.join(server_executable_name()))
            .and_then(|()| {
                let target = versions.join(tag);
                match fs::rename(&staging, &target) {
                    Ok(()) => Ok(()),
                    // Another client installed the same version first.
                    Err(_) if self.cached(tag).is_some() => Ok(()),
                    Err(error) => Err(io_error("install", &target, error)),
                }
            });
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        let path = self
            .cached(tag)
            .ok_or_else(|| internal(format!("installed loom-server {tag} is missing")))?;
        log::info!("installed loom-server {tag} at '{}'", path.display());
        Ok(path)
    }
}

pub(crate) fn free_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| internal(format!("could not reserve a loopback port: {error}")))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| internal(format!("could not reserve a loopback port: {error}")))
}

#[cfg(test)]
mod tests;
