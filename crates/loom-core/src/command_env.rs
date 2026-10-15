//! Deterministic `PATH` for the processes Loom spawns for an agent.
//!
//! A session's shell can have `git` installed at `/usr/bin/git` while its
//! `PATH` omits `/usr/bin`. `git --version` then fails with "No such file or
//! directory", which is indistinguishable from git being absent, and an agent
//! told that git does not exist may refuse to commit. Every command Loom spawns
//! for an agent therefore runs with a `PATH` that covers the documented system
//! directories, without discarding the entries the caller did provide.

use std::ffi::{OsStr, OsString};

/// The directories the baseline `PATH` provides, in search order.
#[cfg(unix)]
pub const BASELINE_PATH_DIRECTORIES: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

/// The `PATH` a child of this process must run with, or `None` when the
/// inherited value can be passed through unchanged.
///
/// The inherited value is kept only when it already covers every baseline
/// directory. Otherwise the baseline directories come first, in their
/// documented order, followed by the inherited entries that are not themselves
/// baseline directories, so a caller that relies on its own entries still finds
/// them and a missing baseline directory can no longer hide an installed
/// program.
pub fn child_path() -> Option<OsString> {
    let inherited = std::env::var_os("PATH");
    let path = baseline_path(inherited.as_deref());
    let effective = path.as_deref().or(inherited.as_deref());
    log::debug!(
        "spawning with PATH {}",
        effective.unwrap_or(OsStr::new("")).to_string_lossy()
    );
    path
}

/// [`child_path`] for an explicit inherited value, without reading the
/// environment or logging, so callers and tests can reason about it directly.
///
/// Only unix has a documented baseline; other platforms always keep the
/// inherited value and return `None`.
#[cfg(unix)]
pub fn baseline_path(inherited: Option<&OsStr>) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let inherited = inherited.map(OsStrExt::as_bytes).unwrap_or_default();
    let mut covered = [false; BASELINE_PATH_DIRECTORIES.len()];
    let mut kept: Vec<&[u8]> = Vec::new();
    for entry in inherited.split(|byte| *byte == b':') {
        // An empty or blank entry names no directory; `PATH` commonly ends with
        // one or holds a whitespace placeholder.
        if entry.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match BASELINE_PATH_DIRECTORIES
            .iter()
            .position(|directory| directory.as_bytes() == entry)
        {
            Some(index) => covered[index] = true,
            None => kept.push(entry),
        }
    }
    if covered.iter().all(|covered| *covered) {
        return None;
    }
    let mut path = Vec::new();
    for directory in BASELINE_PATH_DIRECTORIES {
        if !path.is_empty() {
            path.push(b':');
        }
        path.extend_from_slice(directory.as_bytes());
    }
    for entry in kept {
        path.push(b':');
        path.extend_from_slice(entry);
    }
    Some(OsString::from_vec(path))
}

/// Non-unix platforms have no documented baseline, so the inherited `PATH` is
/// always passed through.
#[cfg(not(unix))]
pub fn baseline_path(_inherited: Option<&OsStr>) -> Option<OsString> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(inherited: &str) -> Option<String> {
        baseline_path(Some(OsStr::new(inherited))).map(|path| path.to_string_lossy().into_owned())
    }

    #[cfg(unix)]
    #[test]
    fn an_unset_or_empty_path_is_replaced_by_the_baseline() {
        assert_eq!(
            baseline_path(None),
            Some(OsString::from("/usr/local/bin:/usr/bin:/bin"))
        );
        assert_eq!(path(""), Some("/usr/local/bin:/usr/bin:/bin".to_owned()));
        assert_eq!(path("   "), Some("/usr/local/bin:/usr/bin:/bin".to_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_without_the_system_directories_keeps_its_own_entries_after_the_baseline() {
        // The reported failure: `PATH` has the tool's own directories but not
        // the system ones, so an installed `git` looks absent.
        assert_eq!(
            path("/root/.cargo/bin"),
            Some("/usr/local/bin:/usr/bin:/bin:/root/.cargo/bin".to_owned())
        );
        // Baseline entries the caller already provided are not duplicated.
        assert_eq!(
            path("/usr/bin:/opt/tools"),
            Some("/usr/local/bin:/usr/bin:/bin:/opt/tools".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_usable_path_is_left_alone() {
        assert_eq!(
            path("/usr/local/bin:/usr/bin:/bin"),
            None,
            "a PATH that already covers the baseline must not be rewritten"
        );
        assert_eq!(
            path("/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/snap/bin"),
            None
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn other_platforms_keep_the_inherited_path() {
        assert_eq!(path("/usr/bin"), None);
    }

    #[test]
    fn child_path_matches_the_environment_it_reads() {
        let expected = baseline_path(std::env::var_os("PATH").as_deref());
        assert_eq!(child_path(), expected);
    }
}
