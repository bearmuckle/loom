//! Embedded in-process backend tests, split by domain.
//!
//! Shared setup lives in [`support`]; each domain module is a thin set of
//! tests over that fixture.

use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use loom_context::ContextAssemblyOptions;
use loom_core::{
    AgentSessionId, Capability, CapabilitySet, EventSequence, PolicyDecision, ProtocolVersion,
    ToolCallId, WorkspaceId,
};
use loom_process::{TaskEvent, TaskKind, TaskSpec, TaskStatus, TerminalEvent};
use loom_protocol::{
    AgentActivityStatus, AgentInteractionStatus, ApprovalDecision, ClientRequest, ContextRequest,
    ControlRequest, ControlResponse, EventsRequest, EventsResponse, FilesystemRequest,
    FilesystemResponse, ProjectRequest, ProjectResponse, ProviderRequest, ProviderResponse,
    RepositoryRequest, RepositoryResponse, RequestEnvelope, RunRequest, RunResponse, ServerEvent,
    ServerResponse, SessionRequest, SessionResponse, TaskRequest, TaskResponse, TerminalRequest,
    TerminalResponse, UsageRequest, UsageResponse, WorkerNodeConfig, WorkspaceConfig,
    WorkspaceRequest, WorkspaceResponse,
};
use loom_providers::CredentialStore;
use loom_workspace::{WorkspaceControl, WorkspaceEdit};

use super::*;
#[allow(unused_imports)]
use support::*;

mod feed;
mod filesystem;
mod project_child;
mod project_child_reliability;
mod project_integration;
mod project_manager;
mod project_misc;
mod providers;
mod resources;
mod retention;
mod runs;
mod session;
mod session_workspace;
mod support;
mod util;
