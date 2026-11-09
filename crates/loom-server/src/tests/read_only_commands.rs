//! The operator opt-in that lets a `run_command` call which provably only reads
//! be classified as a read action instead of a command action.
//!
//! The backend owns the setting; every run reads it when it builds its tool
//! executor. These tests cover both directions: the shipped backend keeps every
//! command a command, and the opt-in moves only the calls the allowlist
//! recognises.

use loom_core::{ActionKind, ReadOnlyCommandPolicy};
use loom_sessionfs::Workspace;

use super::*;

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        name: name.to_owned(),
        arguments,
    }
}

fn run_command(program: &str, args: &[&str]) -> ToolCall {
    tool_call(
        "run_command",
        serde_json::json!({ "command": program, "args": args }),
    )
}

/// A backend that starts without the opt-in classifies every command as a
/// command, which is the shipped policy.
#[test]
fn a_new_backend_keeps_every_command_a_command() {
    let backend = InProcessBackend::new();
    assert!(!backend.read_only_commands().is_enabled());
    let read_only = run_command("git", &["status", "--short"]);
    let root = workspace();
    let tools = backend.with_read_only_command_policy(ToolExecutor::new_with_workspace(
        Workspace::open(AgentSessionId::new(), &root).unwrap(),
    ));
    assert_eq!(tools.action_kind(&read_only), Some(ActionKind::Command));
    let evaluation = tools
        .policy_evaluation(&read_only, &ApprovalPolicy::default())
        .expect("run_command is classified");
    assert_eq!(evaluation.decision, PolicyDecision::RequireApproval);
    fs::remove_dir_all(root).unwrap();
    backend.shutdown().unwrap();
}

/// The setting the operator makes reaches the executors the backend builds, and
/// it moves only the calls the allowlist recognises.
#[test]
fn the_opt_in_reaches_the_executors_the_backend_builds() {
    let backend = InProcessBackend::new();
    let root = workspace();
    let tools = |backend: &InProcessBackend| {
        backend.with_read_only_command_policy(ToolExecutor::new_with_workspace(
            Workspace::open(AgentSessionId::new(), &root).unwrap(),
        ))
    };

    let log = run_command("git", &["log", "--oneline", "-n", "3"]);
    let push = run_command("git", &["push"]);

    let before = tools(&backend);
    assert_eq!(before.action_kind(&log), Some(ActionKind::Command));

    backend.set_read_only_commands(ReadOnlyCommandPolicy::enabled());
    assert!(backend.read_only_commands().is_enabled());
    let after = tools(&backend);
    assert_eq!(after.action_kind(&log), Some(ActionKind::Read));
    assert_eq!(
        after
            .policy_evaluation(&log, &ApprovalPolicy::default())
            .expect("run_command is classified")
            .decision,
        PolicyDecision::Allow
    );
    // The opt-in never widens a command that can write.
    assert_eq!(after.action_kind(&push), Some(ActionKind::Command));
    assert_eq!(
        after
            .policy_evaluation(&push, &ApprovalPolicy::default())
            .expect("run_command is classified")
            .decision,
        PolicyDecision::RequireApproval
    );

    // Turning it back off restores the shipped classification for the next run.
    backend.set_read_only_commands(ReadOnlyCommandPolicy::disabled());
    assert_eq!(tools(&backend).action_kind(&log), Some(ActionKind::Command));
    fs::remove_dir_all(root).unwrap();
    backend.shutdown().unwrap();
}
