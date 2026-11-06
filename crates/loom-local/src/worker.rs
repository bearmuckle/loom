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
    env, fs, io,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
};

use loom_core::{ErrorCode, LoomError, Result};
pub use loom_release::{
    ArchiveKind, GitHubReleases, RELEASE_REPOSITORY, ReleaseAsset, ReleaseInfo, ReleaseSource,
    asset_name, extract_server, host_platform, parse_sha256sums, platform_label,
    server_executable_name, sha256_hex, verify_checksum,
};

/// Environment variable naming an explicit `loom-server` executable.
pub const SERVER_BINARY_ENV: &str = "LOOM_SERVER_BINARY";
/// Environment variable overriding the download cache root.
pub const CACHE_DIR_ENV: &str = "LOOM_CACHE_DIR";

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
pub(crate) fn parse_tag(tag: &str) -> Option<(u64, u64, u64)> {
    loom_release::parse_release_tag(tag)
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
        loom_release::fetch_verified(self.source, release, platform)
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
