//! Environment-derived locations for Loom's own on-disk state.
//!
//! The native client, the CLI, and the standalone server keep state and
//! configuration in the same per-user directories. `loom-server` cannot depend
//! on `loom-local` (the client depends on the server), so the precedence is
//! defined once here instead of by each entry point.
//!
//! This module is compiled for native targets only: a browser client has no
//! private filesystem to keep state in, and the environment variables these
//! helpers read do not exist there.

use std::{
    env, fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::LoomError;

/// Loom's own directory inside the per-user state root.
const OWN_STATE_DIRECTORY: &str = "loom";

/// Root of the per-user state tree: `LOOM_STATE_DIR`, then `XDG_STATE_HOME`,
/// then `$HOME/.local/state`, then a directory in the system temp folder.
pub fn state_root() -> PathBuf {
    env::var_os("LOOM_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .unwrap_or_else(|| env::temp_dir().join("loom-state"))
}

/// Loom's own state directory, `state_root()/loom`. `LOOM_STATE_DIR` names the
/// state *root*, so Loom still keeps its files in the `loom` subdirectory.
pub fn state_dir() -> PathBuf {
    state_root().join(OWN_STATE_DIRECTORY)
}

/// Root of the per-user configuration tree: `LOOM_CONFIG_DIR` used verbatim,
/// then `XDG_CONFIG_HOME/loom`, then `$HOME/.config/loom`, then `.loom`.
pub fn config_dir() -> PathBuf {
    env::var_os("LOOM_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("loom")))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("loom"))
        })
        .unwrap_or_else(|| PathBuf::from(".loom"))
}

/// The bearer token file a standalone server uses when its bind address has no
/// stable identity to name an instance directory after: `state_directory/token`.
pub fn token_path(state_directory: &Path) -> PathBuf {
    state_directory.join("token")
}

/// A human-readable, filesystem-safe key for a bind address, so servers on
/// different hosts or ports get different state directories: `127.0.0.1_8765`
/// and `__1_8765` for `[::1]:8765`.
pub fn bind_key(bind: SocketAddr) -> String {
    bind.to_string()
        .chars()
        .filter(|value| !matches!(value, '[' | ']'))
        .map(|value| if value == ':' { '_' } else { value })
        .collect()
}

/// The state directory of one standalone server: `state_directory` plus the
/// operator's `--instance-name` or, without one, the [`bind_key`] of its bind
/// address.
///
/// Returns `Ok(None)` for an ephemeral port (0), because an OS-assigned port has
/// no stable identity to name a directory after.
pub fn instance_dir(
    state_directory: &Path,
    bind: SocketAddr,
    instance_name: Option<&str>,
) -> Result<Option<PathBuf>, LoomError> {
    if bind.port() == 0 {
        return Ok(None);
    }
    let key = match instance_name {
        Some(name) => instance_name_key(name)?,
        None => bind_key(bind),
    };
    Ok(Some(state_directory.join(key)))
}

/// Validates `--instance-name`: a single directory name, never empty, `.`, `..`
/// or anything containing a path separator, so it cannot name a directory
/// outside the state directory.
fn instance_name_key(name: &str) -> Result<String, LoomError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LoomError::invalid_request(
            "--instance-name must not be empty",
        ));
    }
    if name == "." || name == ".." || name.chars().any(|value| matches!(value, '/' | '\\' | '\0')) {
        return Err(LoomError::invalid_request(format!(
            "--instance-name '{name}' must be a directory name without path separators"
        )));
    }
    Ok(name.to_owned())
}

/// Creates `path` and its parents and restricts it to the owner (mode `0700`),
/// because an instance directory holds transcripts, session roots, cached
/// clones, and credential references.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_directory_is_the_loom_subdirectory_of_the_state_root() {
        assert_eq!(state_dir(), state_root().join("loom"));
    }

    #[test]
    fn bind_keys_distinguish_hosts_and_ports() {
        assert_eq!(
            bind_key("127.0.0.1:8765".parse().unwrap()),
            "127.0.0.1_8765"
        );
        assert_eq!(bind_key("0.0.0.0:8765".parse().unwrap()), "0.0.0.0_8765");
        assert_eq!(
            bind_key("127.0.0.1:8766".parse().unwrap()),
            "127.0.0.1_8766"
        );
        assert_eq!(bind_key("[::1]:8765".parse().unwrap()), "__1_8765");
    }

    #[test]
    fn instance_directories_are_named_by_bind_address_or_instance_name() {
        let state = PathBuf::from("/state");
        assert_eq!(
            instance_dir(&state, "127.0.0.1:8765".parse().unwrap(), None).unwrap(),
            Some(state.join("127.0.0.1_8765"))
        );
        assert_eq!(
            instance_dir(&state, "0.0.0.0:8765".parse().unwrap(), None).unwrap(),
            Some(state.join("0.0.0.0_8765"))
        );
        // A host-aliased bind is pinned to one directory with --instance-name,
        // including whitespace around the name.
        assert_eq!(
            instance_dir(&state, "0.0.0.0:8765".parse().unwrap(), Some(" desktop ")).unwrap(),
            Some(state.join("desktop"))
        );
        assert_eq!(
            instance_dir(&state, "127.0.0.1:8765".parse().unwrap(), Some("desktop")).unwrap(),
            Some(state.join("desktop"))
        );
        // An OS-assigned port has no stable identity.
        assert_eq!(
            instance_dir(&state, "127.0.0.1:0".parse().unwrap(), None).unwrap(),
            None
        );
        assert_eq!(
            instance_dir(&state, "0.0.0.0:0".parse().unwrap(), Some("desktop")).unwrap(),
            None
        );
    }

    #[test]
    fn instance_names_cannot_escape_the_state_directory() {
        let state = PathBuf::from("/state");
        let bind = "127.0.0.1:8765".parse().unwrap();
        for name in ["", "   ", ".", "..", "../elsewhere", "a/b", "a\\b"] {
            let error = instance_dir(&state, bind, Some(name))
                .expect_err("instance name should have been rejected");
            assert_eq!(error.code, crate::ErrorCode::InvalidRequest);
            assert!(error.message.contains("--instance-name"), "{error}");
        }
    }

    #[test]
    fn token_path_is_inside_the_state_directory() {
        assert_eq!(
            token_path(Path::new("/state/127.0.0.1_8765")),
            PathBuf::from("/state/127.0.0.1_8765/token")
        );
    }

    #[test]
    fn created_directories_are_owner_only() {
        let root = env::temp_dir().join(format!("loom-core-paths-{}", crate::RunId::new()));
        let nested = root.join("loom").join("127.0.0.1_8765");
        create_private_dir(&nested).unwrap();
        assert!(nested.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "instance directory mode");
        }
        // Creating an existing directory again is not an error and keeps the
        // owner-only mode.
        create_private_dir(&nested).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
