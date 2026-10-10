//! Deployment-context diagnostics for the standalone server.
//!
//! An operator has to be able to tell where agent commands execute, as which
//! account, with which environment, and what that implies for persistence and
//! the intended network policy. `loom-server --diagnostics` prints that context
//! before anything is created or opened, and the standalone server logs the same
//! facts at startup, so `journald` and `docker logs` answer it afterwards.
//!
//! Detection is split in two: the rules are pure functions over injected inputs,
//! and the thin wrappers above them read the real environment and filesystem, so
//! every rule is unit-testable without running inside a container.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use crate::instance::{InstanceLayout, InstanceOptions};

/// The environment variable that names the part this deployment plays.
pub const DEPLOYMENT_ROLE_ENV: &str = "LOOM_DEPLOYMENT_ROLE";

/// The role reported when [`DEPLOYMENT_ROLE_ENV`] is unset or blank.
///
/// The container image sets the variable to this value, and a one-off deployment
/// such as the release pipeline's image smoke test overrides it, which is what
/// keeps a smoke-test container distinguishable from a container that executes
/// agent commands.
pub const DEFAULT_DEPLOYMENT_ROLE: &str = "agent-worker";

/// The intended outbound network policy, printed verbatim by `--diagnostics`.
///
/// Loom does not filter the backend's own egress: DNS and HTTPS to model
/// providers, forges, and package registries are what a deployment has to allow.
/// What Loom does control is the tool actions an agent asks for, and a
/// network-classified action requires approval under the default policy.
pub const EGRESS_POLICY: &str = "outbound DNS and HTTPS from the backend account to model providers, forges, and package \
registries is expected and not filtered by Loom; network-classified tool actions require approval under the default \
policy";

/// How agent commands inherit their environment, printed verbatim.
const AGENT_COMMAND_ENVIRONMENT: &str = "inherited from the backend process; each command runs with the session \
filesystem root as its working directory";

/// The line printed when no container runtime was detected.
const RUNTIME_NOT_DETECTED: &str = "not detected";

/// Where agent commands execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionContext {
    /// On the host that runs the backend.
    Host,
    /// Inside the container that runs the backend.
    Container,
}

impl ExecutionContext {
    /// The name used in diagnostics: `host` or `container`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Container => "container",
        }
    }

    /// How a report describes the execution of agent commands.
    fn agent_commands(self, account: &str) -> String {
        match self {
            Self::Host => format!("on the host as account {account}"),
            Self::Container => format!("inside the container as account {account}"),
        }
    }
}

/// The markers one container detection reads.
///
/// Podman sets `container=podman`, Docker creates `/.dockerenv`, and container
/// managers name themselves in the cgroup of the init process, so no single
/// marker covers every runtime.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContainerMarkers {
    /// Whether the Docker marker file `/.dockerenv` exists.
    pub docker_marker: bool,
    /// The `container` environment variable, which podman sets.
    pub container_env: Option<String>,
    /// The contents of `/proc/1/cgroup`, when it could be read.
    pub cgroup: Option<String>,
}

impl ContainerMarkers {
    /// Whether any marker shows that this process runs in a container.
    pub fn is_container(&self) -> bool {
        self.docker_marker || self.container_env_is_podman() || self.cgroup_runtime().is_some()
    }

    /// Whether the `container` variable carries podman's value.
    fn container_env_is_podman(&self) -> bool {
        self.container_env
            .as_deref()
            .is_some_and(|value| value.trim() == "podman")
    }

    /// The runtime named by the init process's cgroup.
    ///
    /// `kubepods` is checked first, because a Kubernetes cgroup path also names
    /// the container runtime underneath it.
    fn cgroup_runtime(&self) -> Option<&'static str> {
        let cgroup = self.cgroup.as_deref()?.to_ascii_lowercase();
        [
            ("kubepods", "kubernetes"),
            ("docker", "docker"),
            ("containerd", "containerd"),
            ("podman", "podman"),
            ("lxc", "lxc"),
        ]
        .into_iter()
        .find_map(|(marker, runtime)| cgroup.contains(marker).then_some(runtime))
    }
}

/// The markers of this process.
pub fn container_markers() -> ContainerMarkers {
    ContainerMarkers {
        docker_marker: Path::new("/.dockerenv").exists(),
        container_env: env::var("container").ok(),
        cgroup: fs::read_to_string("/proc/1/cgroup")
            .ok()
            .filter(|contents| !contents.trim().is_empty()),
    }
}

/// Classifies the injected markers, so the rule itself needs no container.
pub fn detect_execution_context(markers: &ContainerMarkers) -> ExecutionContext {
    if markers.is_container() {
        ExecutionContext::Container
    } else {
        ExecutionContext::Host
    }
}

/// Names the runtime responsible for the injected markers, most specific
/// evidence first: the cgroup of the init process, then podman's own `container`
/// variable, then the anonymous `/.dockerenv` marker, which is mapped to Docker
/// because that is the runtime known to create it.
pub fn detect_container_runtime(markers: &ContainerMarkers) -> Option<&'static str> {
    if let Some(runtime) = markers.cgroup_runtime() {
        return Some(runtime);
    }
    if markers.container_env_is_podman() {
        return Some("podman");
    }
    markers.docker_marker.then_some("docker")
}

/// The account the backend process runs as, as far as the process can tell.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessAccount {
    /// `USER`, then `LOGNAME`.
    pub name: Option<String>,
    /// The effective user id, with the real id as the fallback.
    pub uid: Option<u32>,
    /// The effective group id, with the real id as the fallback.
    pub gid: Option<u32>,
}

impl ProcessAccount {
    /// The account name for a report, or `unknown` when neither variable is set,
    /// as in a minimal container image.
    pub fn name_or_unknown(&self) -> &str {
        self.name.as_deref().unwrap_or("unknown")
    }

    /// The account as a report names it: `loom (uid 100, gid 101)`.
    pub fn describe(&self) -> String {
        format!(
            "{} (uid {}, gid {})",
            self.name_or_unknown(),
            id_or_unknown(self.uid),
            id_or_unknown(self.gid)
        )
    }
}

/// Reads the `Uid:` and `Gid:` fields of a `/proc/<pid>/status` body, preferring
/// the effective id and falling back to the real one.
///
/// The effective id is what the process acts as. A body that omits the lines, or
/// reports something unparsable, yields `None` instead of a guessed id, so a
/// diagnostics run never fails on an unfamiliar kernel.
pub fn parse_proc_status(body: &str) -> (Option<u32>, Option<u32>) {
    (status_id(body, "Uid:"), status_id(body, "Gid:"))
}

/// One `Uid:`/`Gid:` line of a `/proc/<pid>/status` body, as the effective id.
fn status_id(body: &str, field: &str) -> Option<u32> {
    let line = body.lines().find(|line| line.starts_with(field))?;
    let mut ids = line[field.len()..].split_whitespace();
    let real = ids.next()?;
    ids.next().unwrap_or(real).parse().ok()
}

/// The account this process runs as, from the environment and `/proc/self/status`.
pub fn process_account() -> ProcessAccount {
    let (uid, gid) = fs::read_to_string("/proc/self/status")
        .map(|body| parse_proc_status(&body))
        .unwrap_or((None, None));
    ProcessAccount {
        name: account_name(),
        uid,
        gid,
    }
}

/// `USER`, then `LOGNAME`. Neither is set in every service manager or image, so a
/// report falls back to the ids instead of inventing a name.
fn account_name() -> Option<String> {
    ["USER", "LOGNAME"].into_iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

/// The deployment role this process reports.
pub fn deployment_role() -> String {
    resolved_deployment_role(env::var(DEPLOYMENT_ROLE_ENV).ok().as_deref())
}

/// [`deployment_role`] over an injected value, so the precedence is testable.
fn resolved_deployment_role(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_DEPLOYMENT_ROLE)
        .to_owned()
}

/// Whether the nearest existing ancestor of `path` sits on a different
/// filesystem than that ancestor's parent, by comparing device numbers.
///
/// `Some(true)` means the state directory is a mount of its own, so what it holds
/// outlives the container. `Some(false)` means it is the writable layer of the
/// container's own filesystem, which is removed together with the container.
/// `None` means it could not be determined, and a report says `unknown` rather
/// than guessing.
pub fn state_directory_on_separate_filesystem(path: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let ancestor = path
            .ancestors()
            .find(|candidate| candidate.is_dir())
            .filter(|candidate| !candidate.as_os_str().is_empty())?;
        let parent = ancestor
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())?;
        let ancestor_device = fs::metadata(ancestor).ok()?.dev();
        let parent_device = fs::metadata(parent).ok()?.dev();
        Some(ancestor_device != parent_device)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The deployment facts only this process's environment can answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessContext {
    /// Whether agent commands execute on the host or in a container.
    pub execution_context: ExecutionContext,
    /// The container runtime, when one was detected.
    pub container_runtime: Option<&'static str>,
    /// The deployment role this process reports.
    pub role: String,
    /// The account this process runs as.
    pub account: ProcessAccount,
    /// The directory the process was started in.
    pub working_directory: PathBuf,
}

impl ProcessContext {
    /// Detects the context of this process.
    pub fn detect() -> Self {
        let markers = container_markers();
        Self {
            execution_context: detect_execution_context(&markers),
            container_runtime: detect_container_runtime(&markers),
            role: deployment_role(),
            account: process_account(),
            working_directory: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// The TLS material `--tls-cert` and `--tls-key` amount to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsState {
    /// Neither flag was given, so a start would serve plain `ws://`.
    None,
    /// Both flags were given, so a start would serve `wss://`.
    Configured,
    /// Only one flag was given, which a start refuses.
    Incomplete,
}

impl TlsState {
    /// Classifies the presence of the two flags.
    pub fn from_flags(certificate: bool, private_key: bool) -> Self {
        match (certificate, private_key) {
            (false, false) => Self::None,
            (true, true) => Self::Configured,
            _ => Self::Incomplete,
        }
    }
}

/// The resolved settings a report describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsSettings<'a> {
    /// The `--version` text, printed as the report's first line.
    pub version: String,
    /// The settings the operator passed, including the bind address.
    pub options: &'a InstanceOptions,
    /// The paths resolved from those settings.
    pub layout: &'a InstanceLayout,
    /// Loom's state root, which `LOOM_STATE_DIR` or `XDG_STATE_HOME` may name.
    pub state_root: PathBuf,
    /// Loom's own state directory below that root.
    pub state_directory: PathBuf,
    /// The TLS material the operator passed.
    pub tls: TlsState,
    /// `--allow-insecure-remote` was given.
    pub allow_insecure_remote: bool,
}

/// The `key: value` lines `loom-server --diagnostics` prints.
///
/// Every line describes the resolved deployment, and none of them carries a
/// credential: the token line names its source and path only.
pub fn diagnostics_lines(
    context: &ProcessContext,
    settings: &DiagnosticsSettings<'_>,
) -> Vec<String> {
    let state_directory = settings.state_directory.as_path();
    let mut lines = vec![
        settings.version.clone(),
        format!("execution-context: {}", context.execution_context.as_str()),
        format!(
            "container-runtime: {}",
            context.container_runtime.unwrap_or(RUNTIME_NOT_DETECTED)
        ),
        format!("deployment-role: {}", context.role),
        format!("process-account: {}", context.account.describe()),
        format!("working-directory: {}", context.working_directory.display()),
        format!("state-root: {}", settings.state_root.display()),
        format!("state-directory: {}", state_directory.display()),
        format!(
            "state-persistence: {}",
            if settings.layout.state_db().is_some() {
                "durable"
            } else {
                "in-memory"
            }
        ),
        format!(
            "state-directory-filesystem: {}",
            describe_filesystem(state_directory_on_separate_filesystem(state_directory))
        ),
        describe_instance_directory(settings.layout),
        describe_state_database(settings.layout),
        format!(
            "agent-commands: {}",
            context
                .execution_context
                .agent_commands(context.account.name_or_unknown())
        ),
        format!("agent-command-environment: {AGENT_COMMAND_ENVIRONMENT}"),
        format!("bind-address: {}", settings.options.bind),
        format!("remote-transport: {}", describe_transport(settings)),
        format!(
            "remote-exposure: {}",
            if settings.options.bind.ip().is_loopback() {
                "loopback only"
            } else {
                "beyond loopback"
            }
        ),
        format!(
            "bearer-token: {}",
            describe_token(settings.options, settings.layout.token_file())
        ),
        format!("network-egress: {EGRESS_POLICY}"),
    ];
    if let Some(warning) = container_state_warning(context, state_directory) {
        lines.push(warning);
    }
    lines
}

/// The single line the standalone server logs at startup, so `journald` and
/// `docker logs` answer the questions `--diagnostics` prints.
pub fn startup_summary(context: &ProcessContext, settings: &DiagnosticsSettings<'_>) -> String {
    let database = match settings.layout.state_db() {
        Some(path) => path.display().to_string(),
        None => "in memory".to_owned(),
    };
    format!(
        "Deployment context: agent commands execute {} (role '{}'); state database {}; bind {}; {}",
        context
            .execution_context
            .agent_commands(context.account.name_or_unknown()),
        context.role,
        database,
        settings.options.bind,
        describe_transport(settings)
    )
}

/// The `state-directory-filesystem` line.
fn describe_filesystem(separate: Option<bool>) -> &'static str {
    match separate {
        Some(true) => "separate filesystem",
        Some(false) => "same filesystem as its parent",
        None => "unknown",
    }
}

/// The `instance-directory` line.
fn describe_instance_directory(layout: &InstanceLayout) -> String {
    match layout.directory() {
        Some(directory) => format!("instance-directory: {}", directory.display()),
        None => "instance-directory: none (bind port 0 keeps state in memory)".to_owned(),
    }
}

/// The `state-database` line.
fn describe_state_database(layout: &InstanceLayout) -> String {
    match layout.state_db() {
        Some(database) => format!("state-database: {}", database.display()),
        None => "state-database: none (in-memory)".to_owned(),
    }
}

/// The `remote-transport` line, which depends on the TLS material and on how far
/// the bind address reaches.
fn describe_transport(settings: &DiagnosticsSettings<'_>) -> String {
    let loopback = settings.options.bind.ip().is_loopback();
    match settings.tls {
        TlsState::Configured => "tls wss://".to_owned(),
        TlsState::Incomplete => {
            "plaintext ws:// until --tls-cert and --tls-key are both given".to_owned()
        }
        TlsState::None if loopback => "plaintext ws:// on loopback".to_owned(),
        TlsState::None if settings.allow_insecure_remote => {
            "plaintext ws:// beyond loopback (--allow-insecure-remote given)".to_owned()
        }
        TlsState::None => {
            "plaintext ws:// beyond loopback (refused without TLS or --allow-insecure-remote)"
                .to_owned()
        }
    }
}

/// The `bearer-token` line: where a start would take the token from, never the
/// value. An existing file is recognized by its metadata alone.
fn describe_token(options: &InstanceOptions, token_file: &Path) -> String {
    match (&options.token, &options.token_file) {
        (Some(_), _) => "from --token (the value is never printed)".to_owned(),
        (None, Some(path)) => format!("from the file '{}' named by --token-file", path.display()),
        (None, None) => match fs::metadata(token_file) {
            Ok(metadata) if metadata.is_file() => {
                format!("from the existing file '{}'", token_file.display())
            }
            _ => format!(
                "no file at '{}' yet; a start generates one there",
                token_file.display()
            ),
        },
    }
}

/// The warning a containerized deployment gets when its state directory is the
/// container's own writable layer, so an operator is told the persistence
/// boundary rather than having to infer it.
fn container_state_warning(context: &ProcessContext, state_directory: &Path) -> Option<String> {
    if context.execution_context != ExecutionContext::Container
        || state_directory_on_separate_filesystem(state_directory) != Some(false)
    {
        return None;
    }
    Some(format!(
        "warning: '{}' is the container's own filesystem, so workspaces are removed with the container unless a \
volume is mounted there",
        state_directory.display()
    ))
}

/// An id as a report names it, or `unknown` when it is not known.
fn id_or_unknown(id: Option<u32>) -> String {
    id.map_or_else(|| "unknown".to_owned(), |id| id.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use loom_core::RunId;

    use super::{
        ContainerMarkers, DEFAULT_DEPLOYMENT_ROLE, DiagnosticsSettings, ExecutionContext,
        ProcessAccount, ProcessContext, TlsState, detect_container_runtime,
        detect_execution_context, diagnostics_lines, parse_proc_status, resolved_deployment_role,
        startup_summary, state_directory_on_separate_filesystem,
    };
    use crate::instance::{InstanceLayout, InstanceOptions};

    fn temporary_directory(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("loom-server-deployment-{label}-{}", RunId::new()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn context(execution_context: ExecutionContext) -> ProcessContext {
        ProcessContext {
            execution_context,
            container_runtime: match execution_context {
                ExecutionContext::Container => Some("docker"),
                ExecutionContext::Host => None,
            },
            role: DEFAULT_DEPLOYMENT_ROLE.to_owned(),
            account: ProcessAccount {
                name: Some("loom".to_owned()),
                uid: Some(100),
                gid: Some(101),
            },
            working_directory: PathBuf::from("/var/lib/loom"),
        }
    }

    fn settings<'a>(
        options: &'a InstanceOptions,
        layout: &'a InstanceLayout,
        state_directory: &Path,
        tls: TlsState,
        allow_insecure_remote: bool,
    ) -> DiagnosticsSettings<'a> {
        DiagnosticsSettings {
            version: "loom-server 0.1.0".to_owned(),
            options,
            layout,
            state_root: state_directory.to_path_buf(),
            state_directory: state_directory.to_path_buf(),
            tls,
            allow_insecure_remote,
        }
    }

    #[test]
    fn container_markers_classify_the_host_and_each_runtime() {
        let host = ContainerMarkers::default();
        assert_eq!(detect_execution_context(&host), ExecutionContext::Host);
        assert_eq!(detect_container_runtime(&host), None);

        let docker = ContainerMarkers {
            docker_marker: true,
            ..ContainerMarkers::default()
        };
        assert_eq!(
            detect_execution_context(&docker),
            ExecutionContext::Container
        );
        assert_eq!(detect_container_runtime(&docker), Some("docker"));

        let podman = ContainerMarkers {
            container_env: Some(" podman ".to_owned()),
            ..ContainerMarkers::default()
        };
        assert_eq!(detect_container_runtime(&podman), Some("podman"));

        // A value podman does not set is not a container marker.
        let other_env = ContainerMarkers {
            container_env: Some("systemd-nspawn".to_owned()),
            ..ContainerMarkers::default()
        };
        assert_eq!(detect_execution_context(&other_env), ExecutionContext::Host);
        assert_eq!(detect_container_runtime(&other_env), None);

        // A Kubernetes path also names the runtime underneath it, so the more
        // specific manager wins.
        let kubernetes = ContainerMarkers {
            cgroup: Some("1:name=systemd:/kubepods/besteffort/pod4/docker-9.scope\n".to_owned()),
            ..ContainerMarkers::default()
        };
        assert_eq!(detect_container_runtime(&kubernetes), Some("kubernetes"));

        for (cgroup, runtime) in [
            ("0::/docker/9", "docker"),
            ("0::/CONTAINERD/9", "containerd"),
            ("0::/lxc.payload.9", "lxc"),
        ] {
            let markers = ContainerMarkers {
                cgroup: Some(cgroup.to_owned()),
                ..ContainerMarkers::default()
            };
            assert_eq!(
                detect_execution_context(&markers),
                ExecutionContext::Container
            );
            assert_eq!(
                detect_container_runtime(&markers),
                Some(runtime),
                "{cgroup}"
            );
        }
    }

    #[test]
    fn proc_status_ids_prefer_the_effective_id() {
        let body = "Name:\tloom-server\nUid:\t1000\t999\t999\t999\nGid:\t1000\t998\t998\t998\n";
        assert_eq!(parse_proc_status(body), (Some(999), Some(998)));
        // A single id is the real one, and stands in for the effective one.
        assert_eq!(parse_proc_status("Uid:\t7\n"), (Some(7), None));
        assert_eq!(parse_proc_status(""), (None, None));
        assert_eq!(
            parse_proc_status("Uid:\tnot-a-number\tnot-a-number\n"),
            (None, None)
        );
    }

    #[test]
    fn deployment_role_defaults_and_ignores_blank_overrides() {
        assert_eq!(resolved_deployment_role(None), DEFAULT_DEPLOYMENT_ROLE);
        assert_eq!(
            resolved_deployment_role(Some("  ")),
            DEFAULT_DEPLOYMENT_ROLE
        );
        assert_eq!(
            resolved_deployment_role(Some(" server-image-smoke-test ")),
            "server-image-smoke-test"
        );
    }

    #[test]
    fn a_plain_directory_is_not_a_mount_of_its_own() {
        let state = temporary_directory("filesystem");
        assert_eq!(state_directory_on_separate_filesystem(&state), Some(false));
        // A state directory that has not been created yet is judged by the
        // nearest ancestor that exists, so a first start still gets an answer.
        let nested = state.join("loom").join("127.0.0.1_8765");
        assert_eq!(state_directory_on_separate_filesystem(&nested), Some(false));
        // The filesystem root has no parent to compare against.
        assert_eq!(state_directory_on_separate_filesystem(Path::new("/")), None);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn report_describes_a_containerized_deployment_without_the_token_value() {
        let state = temporary_directory("report-container");
        let options = InstanceOptions::new("0.0.0.0:8765".parse().unwrap());
        let layout = InstanceLayout::resolve(&options, &state).unwrap();
        let token_file = layout.token_file().to_path_buf();
        fs::create_dir_all(token_file.parent().unwrap()).unwrap();
        fs::write(&token_file, "top-secret-token\n").unwrap();

        let settings = settings(&options, &layout, &state, TlsState::None, true);
        let report = diagnostics_lines(&context(ExecutionContext::Container), &settings).join("\n");

        assert!(report.starts_with("loom-server 0.1.0\n"), "{report}");
        assert!(report.contains("execution-context: container"), "{report}");
        assert!(report.contains("container-runtime: docker"), "{report}");
        assert!(report.contains("deployment-role: agent-worker"), "{report}");
        assert!(
            report.contains("process-account: loom (uid 100, gid 101)"),
            "{report}"
        );
        assert!(report.contains("state-persistence: durable"), "{report}");
        assert!(
            report.contains(&format!(
                "state-database: {}",
                layout.state_db().unwrap().display()
            )),
            "{report}"
        );
        assert!(
            report.contains("agent-commands: inside the container as account loom"),
            "{report}"
        );
        assert!(
            report.contains(
                "remote-transport: plaintext ws:// beyond loopback (--allow-insecure-remote given)"
            ),
            "{report}"
        );
        assert!(
            report.contains("remote-exposure: beyond loopback"),
            "{report}"
        );
        assert!(
            report.contains("bearer-token: from the existing file"),
            "{report}"
        );
        assert!(report.contains("network-egress: "), "{report}");
        // The whole point of the report: it can be pasted into a bug report.
        assert!(!report.contains("top-secret-token"), "{report}");
        // A containerized deployment whose state directory is not a mount of its
        // own loses its workspaces with the container, and is told so.
        assert!(report.contains("warning: "), "{report}");
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn report_omits_the_container_warning_on_a_host() {
        let state = temporary_directory("report-host");
        let options = InstanceOptions::new("127.0.0.1:8765".parse().unwrap());
        let layout = InstanceLayout::resolve(&options, &state).unwrap();
        let settings = settings(&options, &layout, &state, TlsState::None, false);
        let lines = diagnostics_lines(&context(ExecutionContext::Host), &settings);

        assert!(lines.iter().any(|line| line == "execution-context: host"));
        assert!(
            lines
                .iter()
                .any(|line| line == "agent-commands: on the host as account loom")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "remote-transport: plaintext ws:// on loopback")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "state-directory-filesystem: same filesystem as its parent")
        );
        assert!(lines.iter().any(|line| line.contains("state.db")));
        // No token file yet, so the report says what a start would do.
        assert!(
            lines
                .iter()
                .any(|line| line.contains("yet; a start generates one there"))
        );
        assert!(!lines.iter().any(|line| line.starts_with("warning:")));
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn report_describes_an_in_memory_instance_and_incomplete_tls() {
        let state = temporary_directory("report-memory");
        let options = InstanceOptions::new("127.0.0.1:0".parse().unwrap());
        let layout = InstanceLayout::resolve(&options, &state).unwrap();
        let settings = settings(&options, &layout, &state, TlsState::Incomplete, false);
        let lines = diagnostics_lines(&context(ExecutionContext::Host), &settings);

        assert!(
            lines
                .iter()
                .any(|line| line == "state-persistence: in-memory")
        );
        assert!(lines.iter().any(|line| {
            line == "instance-directory: none (bind port 0 keeps state in memory)"
        }));
        assert!(
            lines
                .iter()
                .any(|line| line == "state-database: none (in-memory)")
        );
        assert!(lines.iter().any(|line| {
            line == "remote-transport: plaintext ws:// until --tls-cert and --tls-key are both given"
        }));
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn report_names_a_plaintext_bind_the_server_would_refuse() {
        let state = temporary_directory("report-refused");
        let options = InstanceOptions::new("0.0.0.0:8765".parse().unwrap());
        let layout = InstanceLayout::resolve(&options, &state).unwrap();
        let settings = settings(&options, &layout, &state, TlsState::None, false);
        let lines = diagnostics_lines(&context(ExecutionContext::Host), &settings);

        assert!(lines.iter().any(|line| {
            line == "remote-transport: plaintext ws:// beyond loopback \
                     (refused without TLS or --allow-insecure-remote)"
        }));
        assert!(
            lines
                .iter()
                .any(|line| line == "remote-exposure: beyond loopback")
        );
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn report_names_the_explicit_token_sources_without_the_value() {
        let state = temporary_directory("report-flags");
        let mut flagged = InstanceOptions::new("127.0.0.1:8765".parse().unwrap());
        flagged.token = Some("flag-secret".to_owned());
        let layout = InstanceLayout::resolve(&flagged, &state).unwrap();
        let report = diagnostics_lines(
            &context(ExecutionContext::Host),
            &settings(&flagged, &layout, &state, TlsState::None, false),
        )
        .join("\n");
        assert!(
            report.contains("bearer-token: from --token (the value is never printed)"),
            "{report}"
        );
        assert!(!report.contains("flag-secret"), "{report}");

        let mut named = InstanceOptions::new("127.0.0.1:8765".parse().unwrap());
        named.token_file = Some(state.join("deployment").join("token"));
        let layout = InstanceLayout::resolve(&named, &state).unwrap();
        let report = diagnostics_lines(
            &context(ExecutionContext::Host),
            &settings(&named, &layout, &state, TlsState::None, false),
        )
        .join("\n");
        assert!(report.contains("bearer-token: from the file '"), "{report}");
        assert!(report.contains("named by --token-file"), "{report}");
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn tls_state_classifies_the_two_flags() {
        assert_eq!(TlsState::from_flags(false, false), TlsState::None);
        assert_eq!(TlsState::from_flags(true, true), TlsState::Configured);
        assert_eq!(TlsState::from_flags(true, false), TlsState::Incomplete);
        assert_eq!(TlsState::from_flags(false, true), TlsState::Incomplete);
    }

    #[test]
    fn startup_summary_names_the_context_and_the_state() {
        let state = temporary_directory("startup-summary");
        let options = InstanceOptions::new("127.0.0.1:8765".parse().unwrap());
        let layout = InstanceLayout::resolve(&options, &state).unwrap();
        let settings = settings(&options, &layout, &state, TlsState::Configured, false);
        let summary = startup_summary(&context(ExecutionContext::Host), &settings);

        assert!(
            summary.contains("agent commands execute on the host as account loom"),
            "{summary}"
        );
        assert!(summary.contains("(role 'agent-worker')"), "{summary}");
        assert!(
            summary.contains(&layout.state_db().unwrap().display().to_string()),
            "{summary}"
        );
        assert!(summary.contains("bind 127.0.0.1:8765"), "{summary}");
        assert!(summary.contains("tls wss://"), "{summary}");
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn detection_reads_this_process_rather_than_failing() {
        let detected = ProcessContext::detect();
        assert!(matches!(
            detected.execution_context,
            ExecutionContext::Host | ExecutionContext::Container
        ));
        if let Some(runtime) = detected.container_runtime {
            assert!(matches!(
                runtime,
                "docker" | "podman" | "kubernetes" | "containerd" | "lxc"
            ));
        }
        assert_eq!(detected.role, super::deployment_role());
        assert!(!detected.account.name_or_unknown().is_empty());
        assert!(!detected.working_directory.as_os_str().is_empty());
        // The markers come from this process, so a runtime is only named when a
        // marker backs it.
        assert_eq!(
            super::container_markers().is_container(),
            detected.execution_context == ExecutionContext::Container
        );
        assert_eq!(super::process_account().name, detected.account.name);
        assert_eq!(ProcessAccount::default().name_or_unknown(), "unknown");
    }
}
