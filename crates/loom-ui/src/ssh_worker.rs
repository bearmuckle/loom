//! Workers reached over SSH.
//!
//! A worker added as `ssh://[user@]host[:port]` is provisioned on that host,
//! then reached through a local port forward. The node keeps the `ssh://` URL as
//! its identity, so the saved node list, the credential store and the remembered
//! sign-in all key on the address the person typed instead of a tunnel port that
//! changes on every run.
//!
//! [`open`] is called for every connection to such a node, the first and each
//! reconnect. A live tunnel is reused; otherwise the host is provisioned again,
//! which is idempotent and cheap when this version already runs, and a new
//! tunnel replaces the dead one.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use loom_core::LoomError;
use loom_local::ssh::{self, SshTarget, SshTunnel};

/// A forwarded port and the token that opens the worker behind it.
struct Open {
    tunnel: SshTunnel,
    token: String,
}

static TUNNELS: LazyLock<Mutex<HashMap<String, Open>>> = LazyLock::new(Default::default);

/// Whether `url` names an SSH worker.
pub(crate) fn is_ssh_url(url: &str) -> bool {
    url.starts_with("ssh://")
}

/// The WebSocket URL and token for the SSH worker `url`, provisioning the host
/// and opening the tunnel when no live one exists.
pub(crate) fn open(url: &str) -> Result<(String, String), LoomError> {
    let target = SshTarget::parse(url)?;
    let key = target.display();
    let mut tunnels = TUNNELS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(open) = tunnels.get_mut(&key)
        && open.tunnel.is_alive()
    {
        return Ok((open.tunnel.websocket_url(), open.token.clone()));
    }
    tunnels.remove(&key);
    let worker = ssh::provision_with_system_ssh(&target, &mut |step| {
        log::info!("[loom-ui] {key}: {}", step.label());
    })?;
    log::info!(
        "[loom-ui] {key}: loom-server {} is running{}",
        worker.tag,
        if worker.supervised {
            " under systemd"
        } else {
            " as a detached process; it will not survive a reboot"
        }
    );
    let tunnel = ssh::open_tunnel(&target, None)?;
    let result = (tunnel.websocket_url(), worker.token.clone());
    tunnels.insert(
        key,
        Open {
            tunnel,
            token: worker.token,
        },
    );
    Ok(result)
}

/// Closes the tunnel to the SSH worker `url`, if one is open.
pub(crate) fn close(url: &str) {
    let Ok(target) = SshTarget::parse(url) else {
        return;
    };
    TUNNELS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&target.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_ssh_urls() {
        assert!(is_ssh_url("ssh://dev@box"));
        assert!(!is_ssh_url("wss://worker.example/ws"));
        assert!(!is_ssh_url("dev@box"));
    }

    #[test]
    fn an_invalid_target_fails_before_any_connection() {
        let error = open("ssh://-oProxyCommand=x").unwrap_err();
        assert!(error.message.contains("not a valid SSH destination"));
        close("ssh://-oProxyCommand=x");
        close("ssh://never-opened");
    }
}
