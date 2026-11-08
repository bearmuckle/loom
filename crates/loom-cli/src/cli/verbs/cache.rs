//! `cache prune`: the per-session build caches of the connected worker's
//! instance.
//!
//! Every agent session builds into `<instance directory>/state.build-cache/
//! <session-id>`, outside its checkout, so archiving or deleting a session can
//! reclaim the output. Archiving and deleting already do, and a worker sweeps
//! the caches of sessions that no longer exist when it starts, so this verb is
//! the repair tool for what a failed removal or an interrupted process left
//! behind: it lists the caches with their sizes and the state of the session
//! that owns each one, and removes only the ones no live session can use.
//!
//! Two authorities meet here. The directory is read by this client, because the
//! build cache is ordinary build output on the worker's machine; the session
//! states come from the worker, because only the worker knows whether a session
//! still exists and whether it is archived. A dry run lists everything and
//! removes nothing, and even `--yes` keeps the cache of a session that still
//! exists, so a prune can never take the cache out from under a running build.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use loom_core::{AgentSessionId, AgentSessionState};
use loom_protocol::{
    ClientRequest, ServerResponse, SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{self, CachePruneArgs, Invocation};
use crate::cli::client::Client;
use crate::cli::exit;
use crate::cli::text::{escape, session_state_name};

use super::{Caller, Failure, Outcome, emit, handshake};

/// The directory one session's build output lives in, named exactly as the
/// worker names it: the instance's state database with its extension replaced by
/// `build-cache`, as `loom-server` derives it.
const BUILD_CACHE_DIRECTORY: &str = "state.build-cache";

/// What the worker reports for the session that owns one cache directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    /// The worker lists a session with this id, in this state.
    Session(AgentSessionState),
    /// The worker lists no session with this id.
    Missing,
    /// The directory name is not a session id, so no session owns it.
    Unrecognized,
}

impl Owner {
    /// The state word the listing prints.
    fn word(self) -> &'static str {
        match self {
            Self::Session(state) => session_state_name(state),
            Self::Missing => "missing",
            Self::Unrecognized => "unrecognized",
        }
    }

    /// Whether the cache belongs to a session that can never build again.
    fn reclaimable(self) -> bool {
        matches!(
            self,
            Self::Session(AgentSessionState::Archived) | Self::Missing
        )
    }
}

/// One cache directory the listing found.
struct Cache {
    /// The directory name, which is the session id of a recognized entry.
    name: String,
    session_id: Option<AgentSessionId>,
    owner: Owner,
    bytes: u64,
}

/// `cache prune`: list the per-session build caches, and remove the ones of
/// archived or vanished sessions under `--yes`.
///
/// Every cache is listed with its size and its owner's state, so the dry run
/// answers both "how much is there" and "which of it is dead", which is what the
/// worker's own archive and start-time reclamation already decided for the caches
/// they reached. `--yes` removes exactly the reclaimable entries: the archived
/// and the vanished. A failure to remove one is reported and leaves the rest of
/// the round to run, and the command exits `1` because the operator asked for a
/// removal that did not happen.
pub(super) async fn prune<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &CachePruneArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    // The directory and the worker are resolved before anything is read, so a
    // usage error cannot leave a half-reported listing behind.
    let root = cache_root(invocation, args)?;
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let owners = session_states(&mut caller).await?;
    let mut caches = read_caches(&root, &owners)?;
    // Largest first, so the listing answers "what is taking the space" before
    // the tie-break of the name makes the order stable.
    caches.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.name.cmp(&right.name))
    });
    let total_bytes = caches.iter().map(|cache| cache.bytes).sum::<u64>();
    let reclaimable = caches
        .iter()
        .filter(|cache| cache.owner.reclaimable())
        .collect::<Vec<_>>();
    let reclaimable_bytes = reclaimable.iter().map(|cache| cache.bytes).sum::<u64>();

    let mut removed: Vec<&Cache> = Vec::new();
    let mut failed: Vec<(&Cache, String)> = Vec::new();
    if args.yes {
        for cache in &reclaimable {
            match remove_cache(&root.join(&cache.name)) {
                Ok(()) => removed.push(cache),
                Err(error) => failed.push((cache, error.to_string())),
            }
        }
    }
    let removed_bytes = removed.iter().map(|cache| cache.bytes).sum::<u64>();

    if !invocation.globals.json {
        emit(out, &format!("root {}\n", escape(&root.to_string_lossy())))?;
        for cache in &caches {
            emit(
                out,
                &format!(
                    "cache {} {} {} {}\n",
                    escape(&cache.name),
                    cache.owner.word(),
                    cache.bytes,
                    if cache.owner.reclaimable() {
                        "remove"
                    } else {
                        "keep"
                    }
                ),
            )?;
        }
        emit(
            out,
            &format!("total caches {} bytes {total_bytes}\n", caches.len()),
        )?;
        emit(
            out,
            &format!(
                "reclaimable {} bytes {reclaimable_bytes}\n",
                reclaimable.len()
            ),
        )?;
        if args.yes {
            for (cache, message) in &failed {
                emit(
                    out,
                    &format!("failed {} {}\n", escape(&cache.name), escape(message)),
                )?;
            }
            emit(
                out,
                &format!("removed {} bytes {removed_bytes}\n", removed.len()),
            )?;
        } else {
            emit(
                out,
                &format!(
                    "dry-run; pass --yes to remove {} {}\n",
                    reclaimable.len(),
                    plural(reclaimable.len())
                ),
            )?;
        }
    }

    let data = json!({
        "root": root.to_string_lossy(),
        "dry_run": !args.yes,
        "caches": caches
            .iter()
            .map(|cache| json!({
                "name": cache.name,
                "session_id": cache.session_id.map(|id| id.to_string()),
                "state": cache.owner.word(),
                "bytes": cache.bytes,
                "action": if cache.owner.reclaimable() { "remove" } else { "keep" },
            }))
            .collect::<Vec<Value>>(),
        "totals": {
            "caches": caches.len(),
            "bytes": total_bytes,
            "reclaimable": reclaimable.len(),
            "reclaimable_bytes": reclaimable_bytes,
        },
        "removed": removed
            .iter()
            .map(|cache| json!({
                "name": cache.name,
                "session_id": cache.session_id.map(|id| id.to_string()),
                "bytes": cache.bytes,
            }))
            .collect::<Vec<Value>>(),
        "removed_bytes": removed_bytes,
        "failed": failed
            .iter()
            .map(|(cache, message)| json!({"name": cache.name, "error": message}))
            .collect::<Vec<Value>>(),
    });
    let code = match failed.is_empty() {
        true => exit::OK,
        false => exit::INTERNAL,
    };
    Ok(Outcome::new(data, caller.primary).with_code(code))
}

/// The plural word the dry-run trailer uses, so a one-cache run reads correctly.
fn plural(count: usize) -> &'static str {
    match count {
        1 => "cache",
        _ => "caches",
    }
}

/// The build-cache directory to inspect: `--cache-root`, or the instance
/// directory the connected URL names below the state directory.
///
/// The default derivation is the same one the default token file uses, and it
/// only makes sense for a worker on this machine: a non-loopback URL names a
/// worker whose caches are on another filesystem, so the caller has to say where
/// they are instead of having this client guess a local path from a remote
/// address.
fn cache_root(invocation: &Invocation, args: &CachePruneArgs) -> Result<PathBuf, Failure> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(root) = &args.root {
        return Ok(args::expand_home(root.clone(), home.as_deref()));
    }
    let connect = invocation
        .globals
        .connect
        .as_deref()
        .unwrap_or(args::DEFAULT_CONNECT_URL);
    let (host, port) = args::ws_authority(connect).ok_or_else(|| {
        Failure::Usage(format!(
            "could not derive a build-cache directory from the worker URL '{connect}': it names \
             no host and port, so pass --cache-root <path>"
        ))
    })?;
    if !args::is_loopback_host(&host) {
        return Err(Failure::Usage(format!(
            "the build caches of the worker at '{connect}' are not on this machine: pass \
             --cache-root <path> naming its state.build-cache directory"
        )));
    }
    Ok(loom_core::state_dir()
        .join(args::authority_key(&host, port))
        .join(BUILD_CACHE_DIRECTORY))
}

/// The state of every session the worker's workspaces hold, archived ones
/// included: archived is exactly the state a cache is reclaimable in, and a
/// session id the listing does not carry is one that no longer exists.
async fn session_states<C: Client>(
    caller: &mut Caller<'_, C>,
) -> Result<BTreeMap<AgentSessionId, AgentSessionState>, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces))
        .await?;
    let workspaces = match response {
        ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => workspaces,
        _ => return Err(Failure::unexpected(request_id, "workspace listing")),
    };
    let mut states = BTreeMap::new();
    for workspace in workspaces {
        let (request_id, response) = caller
            .call(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id: workspace.id,
                    include_archived: true,
                },
            ))
            .await?;
        match response {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => {
                states.extend(
                    sessions
                        .into_iter()
                        .map(|session| (session.id, session.state)),
                );
            }
            _ => return Err(Failure::unexpected(request_id, "workspace sessions")),
        }
    }
    Ok(states)
}

/// The caches under `root`, each with the state of the session that owns it.
///
/// A missing root is an empty listing rather than a failure: an instance that
/// never let an agent build anything has no cache directory, and that is nothing
/// to report as an error. An entry whose name is not a session id is listed as
/// unrecognized and never removed, because nothing here can say what it is.
fn read_caches(
    root: &Path,
    owners: &BTreeMap<AgentSessionId, AgentSessionState>,
) -> Result<Vec<Cache>, Failure> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Failure::Internal(format!(
                "could not read the build-cache directory '{}': {error}",
                root.display()
            )));
        }
    };
    let mut caches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            Failure::Internal(format!(
                "could not read the build-cache directory '{}': {error}",
                root.display()
            ))
        })?;
        // A name that is not UTF-8 cannot be a session id, and is never removed.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let session_id = name.parse::<AgentSessionId>().ok();
        let owner = match session_id {
            None => Owner::Unrecognized,
            Some(id) => match owners.get(&id) {
                Some(state) => Owner::Session(*state),
                None => Owner::Missing,
            },
        };
        let bytes = path_size(&entry.path());
        caches.push(Cache {
            name,
            session_id,
            owner,
            bytes,
        });
    }
    Ok(caches)
}

/// The bytes one entry holds, without following a symbolic link: a link is
/// counted as the entry it is, so a cache is never measured through something
/// that lies outside it, and an unreadable entry counts as the zero bytes it can
/// be shown to hold.
fn path_size(path: &Path) -> u64 {
    let mut total = 0_u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&next) else {
            continue;
        };
        if metadata.is_dir() {
            let Ok(entries) = fs::read_dir(&next) else {
                continue;
            };
            pending.extend(entries.flatten().map(|entry| entry.path()));
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

/// Removes one cache: a directory with everything below it, or the single file an
/// interrupted process left. A symbolic link is removed as the link, never as
/// its target.
fn remove_cache(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Globals, Verb};

    fn invocation(connect: Option<&str>) -> Invocation {
        Invocation {
            globals: Globals {
                connect: connect.map(str::to_owned),
                ..Globals::default()
            },
            verb: Verb::CachePrune(CachePruneArgs::default()),
        }
    }

    /// A scratch directory below the developer's cache directory, never the live
    /// state directory, so a test can never touch the caches of a running worker.
    fn scratch(label: &str) -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = home.join(".cache").join(format!(
            "loom-cli-cache-{label}-{}",
            loom_core::WorkspaceId::new()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn the_default_root_is_the_instance_directory_of_a_loopback_worker() {
        let root = cache_root(&invocation(None), &CachePruneArgs::default()).unwrap();
        assert!(
            root.ends_with("127.0.0.1_8765/state.build-cache"),
            "the default URL must name its own instance: {}",
            root.display()
        );
        let named = cache_root(
            &invocation(Some("ws://localhost:9000/control")),
            &CachePruneArgs::default(),
        )
        .unwrap();
        assert!(
            named.ends_with("localhost_9000/state.build-cache"),
            "{}",
            named.display()
        );
    }

    #[test]
    fn a_named_root_wins_and_a_remote_worker_needs_one() {
        let explicit = CachePruneArgs {
            yes: false,
            root: Some(PathBuf::from("/caches/worker")),
        };
        let remote = invocation(Some("wss://worker.example/ws"));
        assert_eq!(
            cache_root(&remote, &explicit).unwrap(),
            PathBuf::from("/caches/worker")
        );
        let error = cache_root(&remote, &CachePruneArgs::default()).unwrap_err();
        assert!(
            error.message().contains("--cache-root"),
            "{}",
            error.message()
        );

        let no_authority = cache_root(
            &invocation(Some("worker.example")),
            &CachePruneArgs::default(),
        )
        .unwrap_err();
        assert!(
            no_authority.message().contains("--cache-root"),
            "{}",
            no_authority.message()
        );
    }

    #[test]
    fn only_an_archived_or_vanished_session_is_reclaimable() {
        assert!(Owner::Session(AgentSessionState::Archived).reclaimable());
        assert!(Owner::Missing.reclaimable());
        for state in [
            AgentSessionState::Idle,
            AgentSessionState::Executing,
            AgentSessionState::Paused,
            AgentSessionState::Completed,
            AgentSessionState::Failed,
        ] {
            assert!(
                !Owner::Session(state).reclaimable(),
                "{state:?} is a live session"
            );
        }
        assert!(!Owner::Unrecognized.reclaimable());
        assert_eq!(Owner::Missing.word(), "missing");
        assert_eq!(Owner::Unrecognized.word(), "unrecognized");
        assert_eq!(
            Owner::Session(AgentSessionState::Archived).word(),
            "archived"
        );
    }

    #[test]
    fn a_cache_is_measured_without_following_a_link() {
        let root = scratch("size");
        let cache = root.join("cache");
        fs::create_dir_all(cache.join("debug/deps")).unwrap();
        fs::write(cache.join("debug/artifact"), vec![b'x'; 10]).unwrap();
        fs::write(cache.join("debug/deps/object"), vec![b'x'; 25]).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, vec![b'x'; 1_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, cache.join("linked")).unwrap();

        assert_eq!(path_size(&cache), 35);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_cache_takes_a_directory_or_the_file_an_interrupted_writer_left() {
        let root = scratch("remove");
        let directory = root.join("directory");
        fs::create_dir_all(directory.join("debug")).unwrap();
        fs::write(directory.join("debug/artifact"), b"build output").unwrap();
        let file = root.join("file");
        fs::write(&file, b"partial").unwrap();

        remove_cache(&directory).unwrap();
        remove_cache(&file).unwrap();
        assert!(!directory.exists());
        assert!(!file.exists());
        // A name that is already gone is reported, so a caller can count it.
        assert!(remove_cache(&directory).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
