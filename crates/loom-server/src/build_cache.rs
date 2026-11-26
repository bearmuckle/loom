//! Build-output retention.
//!
//! Every session builds into its own Cargo target directory, so the cache grows
//! with every session that ran a build. Output is reproducible, so what is kept
//! is bounded instead of trusted to the person to reclaim: an idle cache is
//! dropped after a while, and the rest is evicted least recently written first
//! until the whole cache fits under a byte cap.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use loom_core::{ErrorCode, LoomError, Result};

/// Bounds the whole build cache in bytes.
pub const BUILD_CACHE_MAX_BYTES_ENV: &str = "LOOM_BUILD_CACHE_MAX_BYTES";
/// Bounds how long a session's build output is kept after its last write, in
/// days.
pub const BUILD_CACHE_MAX_IDLE_ENV: &str = "LOOM_BUILD_CACHE_MAX_IDLE_DAYS";

/// The default cap: 20 GiB.
pub const DEFAULT_BUILD_CACHE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;
/// The default idle window: 14 days.
pub const DEFAULT_BUILD_CACHE_MAX_IDLE_DAYS: u64 = 14;

/// How much build output is kept and for how long.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildCacheRetention {
    /// The most any number of caches may occupy together.
    pub max_bytes: u64,
    /// How long a cache may sit unwritten before it is dropped.
    pub max_idle: Duration,
}

impl Default for BuildCacheRetention {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_BUILD_CACHE_MAX_BYTES,
            max_idle: Duration::from_secs(DEFAULT_BUILD_CACHE_MAX_IDLE_DAYS * 24 * 60 * 60),
        }
    }
}

/// One session's build cache, as measured.
#[derive(Clone, Debug)]
pub struct BuildCacheEntry {
    pub path: PathBuf,
    pub bytes: u64,
    /// The newest write anywhere in the cache, which is when it was last used.
    pub last_write: SystemTime,
}

/// Resolves the policy from the operator's environment.
pub fn resolve_build_cache_retention(
    max_bytes: Option<&str>,
    max_idle_days: Option<&str>,
) -> Result<BuildCacheRetention> {
    let defaults = BuildCacheRetention::default();
    let max_bytes = match max_bytes.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => parse_byte_size(value, BUILD_CACHE_MAX_BYTES_ENV)?,
        None => defaults.max_bytes,
    };
    let max_idle = match max_idle_days
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            let days = value.parse::<u64>().map_err(|_| {
                LoomError::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "{BUILD_CACHE_MAX_IDLE_ENV} must be a whole number of days, got '{value}'"
                    ),
                    false,
                )
            })?;
            Duration::from_secs(days.saturating_mul(24 * 60 * 60))
        }
        None => defaults.max_idle,
    };
    Ok(BuildCacheRetention {
        max_bytes,
        max_idle,
    })
}

/// Parses a byte count, accepting a `K`, `M`, `G` or `T` suffix.
pub fn parse_byte_size(value: &str, source: &str) -> Result<u64> {
    let trimmed = value.trim();
    let (digits, scale) = match trimmed.chars().last() {
        Some(last) if last.is_ascii_alphabetic() => {
            let scale = match last.to_ascii_uppercase() {
                'K' => 1024_u64,
                'M' => 1024_u64 * 1024,
                'G' => 1024_u64 * 1024 * 1024,
                'T' => 1024_u64 * 1024 * 1024 * 1024,
                _ => {
                    return Err(LoomError::new(
                        ErrorCode::InvalidRequest,
                        format!("{source} has an unknown size unit '{last}'; use K, M, G or T"),
                        false,
                    ));
                }
            };
            (&trimmed[..trimmed.len() - 1], scale)
        }
        _ => (trimmed, 1),
    };
    let value = digits.trim().parse::<u64>().map_err(|_| {
        LoomError::new(
            ErrorCode::InvalidRequest,
            format!("{source} must be a byte count, got '{trimmed}'"),
            false,
        )
    })?;
    Ok(value.saturating_mul(scale))
}

/// The caches to remove under `retention` at `now`.
///
/// A cache that sat unwritten for longer than the idle window is expired. The
/// survivors are then evicted least recently written first until what is left
/// fits under the byte cap, so one large session cannot push out a small active
/// one.
pub fn expired_build_caches(
    mut entries: Vec<BuildCacheEntry>,
    retention: BuildCacheRetention,
    now: SystemTime,
) -> Vec<PathBuf> {
    let mut expired = Vec::new();
    entries.retain(|entry| {
        let idle = now.duration_since(entry.last_write).unwrap_or_default();
        if idle > retention.max_idle {
            expired.push(entry.path.clone());
            false
        } else {
            true
        }
    });
    let mut total = entries.iter().map(|entry| entry.bytes).sum::<u64>();
    if total <= retention.max_bytes {
        return expired;
    }
    entries.sort_by_key(|entry| entry.last_write);
    for entry in entries {
        if total <= retention.max_bytes {
            break;
        }
        total = total.saturating_sub(entry.bytes);
        expired.push(entry.path);
    }
    expired
}

/// The size of the tree at `path` and its newest write.
///
pub fn measure_build_cache(path: &Path) -> (u64, SystemTime) {
    let fallback = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut bytes = 0_u64;
    let mut newest = fallback;
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                bytes = bytes.saturating_add(metadata.len());
            }
            if let Ok(modified) = metadata.modified()
                && modified > newest
            {
                newest = modified;
            }
        }
    }
    (bytes, newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, bytes: u64, age_days: u64, now: SystemTime) -> BuildCacheEntry {
        BuildCacheEntry {
            path: PathBuf::from(name),
            bytes,
            last_write: now - Duration::from_secs(age_days * 24 * 60 * 60),
        }
    }

    #[test]
    fn byte_sizes_take_a_unit_suffix() {
        assert_eq!(parse_byte_size("2048", "test").unwrap(), 2048);
        assert_eq!(parse_byte_size("2K", "test").unwrap(), 2048);
        assert_eq!(
            parse_byte_size(" 3g ", "test").unwrap(),
            3 * 1024 * 1024 * 1024
        );
        assert_eq!(
            parse_byte_size("1T", "test").unwrap(),
            1024 * 1024 * 1024 * 1024
        );
        let error = parse_byte_size("2P", "test").unwrap_err();
        assert!(error.message.contains("unknown size unit 'P'"), "{error:?}");
        let error = parse_byte_size("12.5", "test").unwrap_err();
        assert!(error.message.contains("must be a byte count"), "{error:?}");
    }

    #[test]
    fn the_policy_falls_back_to_defaults_and_rejects_nonsense() {
        assert_eq!(
            resolve_build_cache_retention(None, None).unwrap(),
            BuildCacheRetention::default()
        );
        assert_eq!(
            resolve_build_cache_retention(Some("1G"), Some("2")).unwrap(),
            BuildCacheRetention {
                max_bytes: 1024 * 1024 * 1024,
                max_idle: Duration::from_secs(2 * 24 * 60 * 60),
            }
        );
        // An empty value is the same as no value.
        assert_eq!(
            resolve_build_cache_retention(Some("  "), Some("")).unwrap(),
            BuildCacheRetention::default()
        );
        let error = resolve_build_cache_retention(None, Some("soon")).unwrap_err();
        assert!(error.message.contains("whole number of days"), "{error:?}");
    }

    #[test]
    fn idle_caches_expire_and_the_rest_are_evicted_oldest_first() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(30 * 24 * 60 * 60);
        let retention = BuildCacheRetention {
            max_bytes: 100,
            max_idle: Duration::from_secs(14 * 24 * 60 * 60),
        };
        let entries = vec![
            entry("idle", 10, 20, now),
            entry("old", 60, 5, now),
            entry("new", 60, 1, now),
        ];
        let expired = expired_build_caches(entries, retention, now);
        // The idle one is expired outright, then the older of the two that
        // remain is evicted so the rest fits under the cap.
        assert_eq!(expired, vec![PathBuf::from("idle"), PathBuf::from("old")]);
    }

    #[test]
    fn a_cache_under_both_bounds_is_kept() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(30 * 24 * 60 * 60);
        let retention = BuildCacheRetention {
            max_bytes: 1_000,
            max_idle: Duration::from_secs(14 * 24 * 60 * 60),
        };
        let entries = vec![entry("a", 100, 3, now), entry("b", 100, 20, now)];
        assert_eq!(
            expired_build_caches(entries, retention, now),
            vec![PathBuf::from("b")]
        );
    }

    #[test]
    fn measuring_reports_the_tree_size_and_its_newest_write() {
        let root = std::env::temp_dir().join(format!("loom-build-cache-{}", std::process::id()));
        let nested = root.join("debug/deps");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("object.rlib"), vec![0_u8; 4096]).unwrap();
        let (bytes, newest) = measure_build_cache(&root);
        assert!(bytes >= 4096, "{bytes}");
        assert!(newest > SystemTime::UNIX_EPOCH);
        let _ = std::fs::remove_dir_all(&root);
    }
}
