use super::{
    AboutBackend, AboutLink, AgentActivityData, AgentActivityRecord, AgentActivityStatus,
    CompletionKind, FileActivityOperation, LOOM_REPOSITORY_URL, ToolPart, ToolPartStatus,
    about_backend, about_backend_label, about_git_revision, about_links, about_platform_label,
    about_protocol, about_version_label, activity_output, change_kind_label, command_line,
    command_purpose, commands_matching, completion_for_value, composer_height, disconnected_screen,
    endpoint_host, endpoint_label, format_bytes, format_duration, format_percentage,
    has_block_markdown, humanize_tool_output, is_redundant_completion_summary, relative_time,
    replace_command_token, replace_last_token, rgb, run_state_color, run_state_label,
    session_is_active, session_status_pill, source_mount_path, tool_detail, tool_failure_count,
    tool_group_label, tool_group_status, tool_needs_attention, tool_part_from_activity,
    tool_status, tool_title, tool_title_for_activity, tool_usage_label, tool_usage_summary,
};
use loom_core::{ActivityId, AgentSessionState, ProtocolVersion, RunId, Timestamp};
use loom_model::{ModelId, ToolCall};
use loom_protocol::{AgentActivityKind, AgentRunState, ToolResult};
use serde_json::json;

fn activity(data: AgentActivityData) -> AgentActivityRecord {
    AgentActivityRecord {
        id: ActivityId::new(),
        run_id: RunId::new(),
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(0),
        completed_at: None,
        elapsed_ms: None,
        data,
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: loom_core::ToolCallId::new(),
        name: name.to_owned(),
        arguments: json!({"path":"src/lib.rs"}),
    }
}

#[test]
fn source_mount_paths_are_readable_and_unique() {
    assert_eq!(
        source_mount_path("sources", "/home/me/My Project", &[]),
        "sources/my-project"
    );
    assert_eq!(
        source_mount_path("repositories", "owner/loom.git", &[]),
        "repositories/loom"
    );
    assert_eq!(
        source_mount_path("sources", "/tmp/repo/", &[]),
        "sources/repo"
    );
    let existing = vec!["sources/repo".to_owned()];
    assert_eq!(
        source_mount_path("sources", "/tmp/repo", &existing),
        "sources/repo-2"
    );
    let existing = vec!["sources/repo".to_owned(), "sources/repo-2".to_owned()];
    assert_eq!(
        source_mount_path("sources", "/tmp/repo", &existing),
        "sources/repo-3"
    );
    assert_eq!(source_mount_path("sources", "///", &[]), "sources/source");
}

#[test]
fn resource_and_duration_labels_handle_missing_and_boundary_values() {
    assert_eq!(format_bytes(None), "n/a");
    assert_eq!(format_bytes(Some(1024)), "1.0 KiB");
    assert_eq!(format_bytes(Some(1 << 20)), "1.0 MiB");
    assert_eq!(format_bytes(Some(1 << 30)), "1.0 GiB");
    assert_eq!(format_percentage(None), "n/a");
    assert_eq!(format_percentage(Some(100)), "100%");
    assert_eq!(format_percentage(Some(101)), "n/a");
    assert_eq!(format_duration(999), "999ms");
    assert_eq!(format_duration(1_500), "1.5s");
    assert_eq!(format_duration(61_000), "1m 1s");
}

#[test]
fn command_titles_describe_the_work() {
    for (program, args, expected) in [
        ("cargo", vec!["test"], "Run tests"),
        ("cargo", vec!["clippy"], "Check code quality"),
        ("cargo", vec!["fmt"], "Format code"),
        ("cargo", vec!["check"], "Check the build"),
        ("git", vec!["diff"], "Inspect repository changes"),
        ("rg", vec!["needle"], "Search the workspace"),
        ("cat", vec!["src/lib.rs"], "Inspect workspace files"),
        ("/usr/bin/custom", vec![], "Run custom"),
    ] {
        assert_eq!(
            command_purpose(
                program,
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>()
            ),
            expected
        );
    }
    assert_eq!(
        command_purpose(
            "/usr/bin/bash",
            &["-lc".to_owned(), "cargo test --workspace".to_owned()]
        ),
        "Run tests"
    );
    assert_eq!(
        command_line(
            "echo",
            &["two words".to_owned(), "it's".to_owned(), "".to_owned()]
        ),
        "echo 'two words' 'it'\\''s' ''"
    );
}

#[test]
fn state_labels_and_tool_projections_cover_every_variant() {
    let session_states = [
        (AgentSessionState::Idle, "Ready"),
        (AgentSessionState::Queued, "Queued"),
        (AgentSessionState::Planning, "Planning"),
        (AgentSessionState::AwaitingApproval, "Approval"),
        (AgentSessionState::Paused, "Paused"),
        (AgentSessionState::Executing, "Working"),
        (AgentSessionState::Evaluating, "Reviewing"),
        (AgentSessionState::NeedsInput, "Input"),
        (AgentSessionState::Completed, "Done"),
        (AgentSessionState::Failed, "Failed"),
        (AgentSessionState::Cancelled, "Cancelled"),
        (AgentSessionState::Archived, "Ready"),
    ];
    for (state, label) in session_states {
        assert_eq!(session_status_pill(state, None).label, label);
    }
    assert_eq!(run_state_label(None), "Ready");
    for (state, label) in [
        (AgentRunState::Planning, "Planning"),
        (AgentRunState::Executing, "Working"),
        (AgentRunState::AwaitingApproval, "Needs approval"),
        (AgentRunState::Paused, "Paused"),
        (AgentRunState::NeedsInput, "Needs your input"),
        (AgentRunState::Evaluating, "Reviewing"),
        (AgentRunState::Completed, "Complete"),
        (AgentRunState::Failed, "Something went wrong"),
        (AgentRunState::Cancelled, "Cancelled"),
    ] {
        assert_eq!(run_state_label(Some(state)), label);
    }
    for (kind, label) in [
        (loom_protocol::WorkspaceChangeKind::Created, "New"),
        (loom_protocol::WorkspaceChangeKind::Deleted, "Removed"),
        (loom_protocol::WorkspaceChangeKind::Modified, "Updated"),
    ] {
        assert_eq!(change_kind_label(kind), label);
    }
    for (status, expected) in [
        (AgentActivityStatus::Started, ToolPartStatus::Running),
        (AgentActivityStatus::Completed, ToolPartStatus::Completed),
        (AgentActivityStatus::Failed, ToolPartStatus::Failed),
        (
            AgentActivityStatus::AwaitingApproval,
            ToolPartStatus::AwaitingApproval,
        ),
        (
            AgentActivityStatus::AwaitingInput,
            ToolPartStatus::AwaitingInput,
        ),
        (AgentActivityStatus::Cancelled, ToolPartStatus::Cancelled),
    ] {
        assert_eq!(tool_status(status), expected);
    }

    let model = activity(AgentActivityData::ModelTurn {
        model: ModelId::new("test-model"),
    });
    assert!(tool_part_from_activity(&model).is_none());
    assert_eq!(activity_output(&model), None);

    assert_eq!(tool_title("apply_patch", &json!({})), "Apply patch");

    let file = activity(AgentActivityData::File {
        call: call("read_file"),
        operation: FileActivityOperation::Read,
        path: None,
        result: None,
    });
    assert_eq!(tool_title_for_activity(&file), "Read file");
    let listed = activity(AgentActivityData::File {
        call: call("list_files"),
        operation: FileActivityOperation::List,
        path: None,
        result: Some(ToolResult::success(
            &call("list_files"),
            "src/lib.rs".to_owned(),
        )),
    });
    assert_eq!(tool_title_for_activity(&listed), "List files");
    assert_eq!(activity_output(&listed), Some("src/lib.rs"));
    let empty_result = activity(AgentActivityData::ToolCall {
        call: call("inspect"),
        result: Some(ToolResult::success(&call("inspect"), String::new())),
    });
    assert_eq!(activity_output(&empty_result), None);

    let write = activity(AgentActivityData::File {
        call: call("write_file"),
        operation: FileActivityOperation::Write,
        path: Some("src/main.rs".to_owned()),
        result: None,
    });
    assert_eq!(
        tool_part_from_activity(&write).unwrap().title,
        "Edit src/main.rs"
    );

    let search = activity(AgentActivityData::Search {
        call: call("search"),
        query: "needle".to_owned(),
        path: Some("src".to_owned()),
        result: None,
    });
    assert_eq!(tool_title_for_activity(&search), "Search \"needle\" in src");

    let command = activity(AgentActivityData::Command {
        call: call("run"),
        command: "cargo".to_owned(),
        args: vec!["test".to_owned()],
        cwd: Some("repo".to_owned()),
        result: None,
    });
    assert_eq!(tool_title_for_activity(&command), "Run tests");
    let command_part = tool_part_from_activity(&command).unwrap();
    assert_eq!(
        command_part.detail.as_deref(),
        Some("cargo test\nDirectory: repo")
    );
    for data in [
        AgentActivityData::File {
            call: call("read_file"),
            operation: FileActivityOperation::Read,
            path: Some("src/lib.rs".to_owned()),
            result: Some(ToolResult::success(
                &call("read_file"),
                "file output".to_owned(),
            )),
        },
        AgentActivityData::Search {
            call: call("search"),
            query: "needle".to_owned(),
            path: None,
            result: Some(ToolResult::success(
                &call("search"),
                "search output".to_owned(),
            )),
        },
        AgentActivityData::Command {
            call: call("run"),
            command: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            cwd: None,
            result: Some(ToolResult::success(
                &call("run"),
                "command output".to_owned(),
            )),
        },
    ] {
        assert!(activity_output(&activity(data)).is_some());
    }
    assert!(is_redundant_completion_summary(
        "Completed task: fixed the bug"
    ));
    assert!(!is_redundant_completion_summary("The task was completed"));
}

#[test]
fn tool_detail_omits_empty_arguments() {
    let mut call = call("list_files");
    call.arguments = json!(null);
    assert_eq!(tool_detail(&call), None);
    call.arguments = json!({});
    assert_eq!(tool_detail(&call), None);
    // A plain path is already in the title, so no detail is needed.
    call.arguments = json!({"path": "src"});
    assert_eq!(tool_detail(&call), None);
    call.arguments = json!({"path": "src", "depth": 2, "glob": "**/*.rs"});
    assert_eq!(tool_detail(&call).as_deref(), Some("glob **/*.rs, depth 2"));
    // Unknown/extension tools still fall back to their JSON arguments.
    call.name = "mystery_tool".to_owned();
    call.arguments = json!({"path": "src"});
    assert_eq!(tool_detail(&call).as_deref(), Some("{\"path\":\"src\"}"));
}

#[test]
fn core_tools_get_readable_details_instead_of_json() {
    let mut call = call("read_file");
    call.arguments = json!({"path": "src/lib.rs"});
    assert_eq!(tool_detail(&call), None);
    call.arguments = json!({"path": "src/lib.rs", "line_start": 10, "line_end": 20});
    assert_eq!(tool_detail(&call).as_deref(), Some("Lines 10–20"));

    call.name = "search_text".to_owned();
    call.arguments = json!({"query": "needle", "path": "src", "glob": "**/*.rs", "regex": true});
    assert_eq!(
        tool_detail(&call).as_deref(),
        Some("in src, glob **/*.rs, regex")
    );

    call.name = "web_search".to_owned();
    call.arguments = json!({"query": "rust", "domains": ["doc.rust-lang.org"]});
    assert_eq!(
        tool_detail(&call).as_deref(),
        Some("Domains: doc.rust-lang.org")
    );

    call.name = "propose_plan".to_owned();
    call.arguments = json!({"steps": ["Do a", "Do b"]});
    assert_eq!(tool_detail(&call).as_deref(), Some("1. Do a\n2. Do b"));

    call.name = "ask_user".to_owned();
    call.arguments = json!({"prompt": "Which one?"});
    assert_eq!(tool_detail(&call).as_deref(), Some("Which one?"));

    call.name = "apply_patch".to_owned();
    call.arguments = json!({
        "path": "src/lib.rs",
        "edits": [{"old_text": "a", "new_text": "b"}, {"old_text": "c", "new_text": "d"}],
    });
    assert_eq!(tool_detail(&call).as_deref(), Some("2 edits"));

    call.name = "run_command".to_owned();
    call.arguments = json!({"command": "cargo", "args": ["test"], "cwd": "repo"});
    assert_eq!(
        tool_detail(&call).as_deref(),
        Some("cargo test\nDirectory: repo")
    );
}

#[test]
fn project_agent_tools_get_readable_titles_details_and_outputs() {
    let mut delegate = call("delegate_project_task");
    delegate.arguments = json!({
        "child_name": "five-second-sleep",
        "intent": "Run a single 5-second sleep command.",
    });
    assert_eq!(
        tool_title("delegate_project_task", &delegate.arguments),
        "Delegate sub-agent \"five-second-sleep\""
    );
    assert_eq!(
        tool_detail(&delegate).as_deref(),
        Some("Run a single 5-second sleep command.")
    );

    let mut wait = call("wait_for_project_children");
    wait.arguments = json!({"task_ids": ["6ee93097-0000-0000-0000-000000000000"]});
    assert_eq!(
        tool_title("wait_for_project_children", &wait.arguments),
        "Wait for 1 sub-agent"
    );
    assert_eq!(tool_detail(&wait).as_deref(), Some("Waiting on 6ee93097"));

    let output = humanize_tool_output(
        "delegate_project_task",
        r#"{"child_name":"five-second-sleep","status":"running","task_id":"6ee93097-078a-4ad1-86b7-d2cd8d0d3226"}"#,
    );
    assert!(output.contains("Created sub-agent \"five-second-sleep\" · running"));
    assert!(output.contains("6ee93097"));

    let wait_output = humanize_tool_output(
        "wait_for_project_children",
        r#"{"return_ready":true,"children":[{"child_name":"five-second-sleep","status":"completed","code_change":false}]}"#,
    );
    assert!(wait_output.contains("All selected children are return-ready"));
    assert!(wait_output.contains("five-second-sleep: completed"));

    assert_eq!(
        humanize_tool_output("run_command", "plain text output"),
        "plain text output"
    );
}

#[test]
fn tool_group_labels_describe_repeated_calls() {
    assert_eq!(tool_group_label("read_file", 4), "Read 4 files");
    assert_eq!(tool_group_label("run_command", 3), "Ran 3 commands");
    assert_eq!(tool_group_label("search_text", 5), "Searched 5 times");
    assert_eq!(
        tool_group_label("delegate_project_task", 3),
        "Delegated 3 sub-agents"
    );
    assert_eq!(
        tool_group_label("review_project_child", 2),
        "Reviewed 2 sub-agents"
    );
    assert_eq!(tool_group_label("ask_user", 2), "Asked the user 2 times");
    assert_eq!(tool_group_label("mystery_tool", 2), "mystery_tool × 2");
}

#[test]
fn tool_usage_summary_counts_calls_by_type_in_first_seen_order() {
    let part = |name: &str| ToolPart {
        id: loom_core::ToolCallId::new(),
        name: name.to_owned(),
        title: String::new(),
        status: ToolPartStatus::Completed,
        detail: None,
        output: None,
        elapsed_ms: None,
        approval_pending: false,
    };
    let parts = [
        part("read_file"),
        part("search_text"),
        part("read_file"),
        part("run_command"),
        part("read_file"),
    ];
    let refs = parts.iter().collect::<Vec<_>>();
    assert_eq!(tool_usage_summary(&refs), "Read ×3 · Search ×1 · Run ×1");
    assert!(tool_usage_summary(&[]).is_empty());

    assert_eq!(tool_usage_label("read_file"), "Read");
    assert_eq!(tool_usage_label("mystery_tool"), "mystery_tool");
}

#[test]
fn glob_tool_gets_a_readable_title_and_no_json_detail() {
    let mut call = call("glob");
    call.arguments = json!({"pattern": "**/*.rs", "path": "crates", "max_entries": 1000});
    assert_eq!(
        tool_title("glob", &call.arguments),
        "Find \"**/*.rs\" in crates"
    );
    assert_eq!(tool_detail(&call), None);

    call.arguments = json!({"pattern": "**/*.rs"});
    assert_eq!(tool_title("glob", &call.arguments), "Find \"**/*.rs\"");
    assert_eq!(tool_detail(&call), None);

    assert_eq!(tool_title("glob", &json!({})), "Find files");
    assert_eq!(tool_group_label("glob", 2), "Matched 2 globs");
}

#[test]
fn github_tools_get_readable_titles_and_no_json_detail() {
    let mut call = call("github_list_pull_requests");
    call.arguments = json!({"repository": "owner/name", "state": "all"});
    assert_eq!(
        tool_title("github_list_pull_requests", &call.arguments),
        "List pull requests in owner/name"
    );
    assert_eq!(tool_detail(&call), None);

    call.arguments = json!({"repository": "owner/name", "number": 42});
    assert_eq!(
        tool_title("github_get_pull_request", &call.arguments),
        "Read owner/name#42"
    );
    assert_eq!(tool_detail(&call), None);

    call.arguments =
        json!({"repository": "owner/name", "title": "Fix", "head": "fix", "base": "main"});
    assert_eq!(
        tool_title("github_create_pull_request", &call.arguments),
        "Open fix -> main in owner/name"
    );
    assert_eq!(tool_detail(&call), None);

    call.arguments = json!({"repository": "owner/name", "branch": "fix"});
    assert_eq!(
        tool_title("github_push_branch", &call.arguments),
        "Push fix to owner/name"
    );
    assert_eq!(tool_detail(&call), None);

    assert_eq!(
        tool_title("github_get_pull_request", &json!({})),
        "Read pull request"
    );
    assert_eq!(
        tool_group_label("github_create_pull_request", 2),
        "Opened 2 pull requests"
    );
}

fn grouped_part(status: ToolPartStatus) -> ToolPart {
    ToolPart {
        id: loom_core::ToolCallId::new(),
        name: "run_command".to_owned(),
        title: "Run command".to_owned(),
        status,
        detail: None,
        output: None,
        elapsed_ms: None,
        approval_pending: false,
    }
}

#[test]
fn tool_group_status_reflects_unfinished_children() {
    use ToolPartStatus::{AwaitingApproval, Cancelled, Completed, Failed, Queued, Running};

    let status = |statuses: &[ToolPartStatus]| {
        let parts = statuses
            .iter()
            .copied()
            .map(grouped_part)
            .collect::<Vec<_>>();
        let refs = parts.iter().collect::<Vec<_>>();
        tool_group_status(&refs)
    };

    assert_eq!(status(&[]), Completed);
    assert_eq!(status(&[Completed, Completed]), Completed);
    assert_eq!(status(&[Completed, Queued, Queued]), Queued);
    assert_eq!(status(&[Queued, Running]), Running);
    assert_eq!(status(&[Completed, AwaitingApproval]), Running);
    // A run that recovered from a failed call reads as done, not failed.
    assert_eq!(status(&[Completed, Failed]), Completed);
    assert_eq!(status(&[Completed, Cancelled]), Completed);
    // Nothing succeeded, so the failure (or cancellation) settles the group.
    assert_eq!(status(&[Failed, Failed]), Failed);
    assert_eq!(status(&[Failed, Cancelled]), Failed);
    assert_eq!(status(&[Cancelled, Cancelled]), Cancelled);
    assert_eq!(status(&[Failed, Running]), Running);
    assert_eq!(status(&[Failed, Queued]), Queued);
}

#[test]
fn tool_failure_count_only_counts_failed_calls() {
    let failed = grouped_part(ToolPartStatus::Failed);
    let completed = grouped_part(ToolPartStatus::Completed);
    let parts = [&failed, &completed, &failed];
    assert_eq!(tool_failure_count(&parts), 2);
    assert_eq!(tool_failure_count(&[&completed]), 0);
}

#[test]
fn tool_needs_attention_only_for_blocking_statuses() {
    let running = grouped_part(ToolPartStatus::Running);
    let queued = grouped_part(ToolPartStatus::Queued);
    let completed = grouped_part(ToolPartStatus::Completed);
    let failed = grouped_part(ToolPartStatus::Failed);
    let awaiting_approval = grouped_part(ToolPartStatus::AwaitingApproval);
    let awaiting_input = grouped_part(ToolPartStatus::AwaitingInput);

    assert!(!tool_needs_attention(&[]));
    assert!(!tool_needs_attention(&[
        &running, &queued, &completed, &failed
    ]));
    assert!(tool_needs_attention(&[&completed, &awaiting_approval]));
    assert!(tool_needs_attention(&[&awaiting_input]));
}

#[test]
fn composer_completion_tracks_commands_and_files() {
    assert!(completion_for_value("").is_none());
    assert!(completion_for_value("hello").is_none());
    let command = completion_for_value("/rev").expect("slash opens commands");
    assert_eq!(command.kind, CompletionKind::Command);
    assert_eq!(command.query, "/rev");
    // Once the command has an argument the menu closes.
    assert!(completion_for_value("/review now").is_none());
    let file = completion_for_value("explain @src/li").expect("@ opens files");
    assert_eq!(file.kind, CompletionKind::File);
    assert_eq!(file.query, "src/li");
}

#[test]
fn command_completion_helpers_rewrite_the_expected_token() {
    assert_eq!(replace_command_token("/rev", "review"), "/review ");
    assert_eq!(replace_command_token("/rev arg", "review"), "/review arg");
    assert_eq!(
        replace_last_token("explain @src/lib", "@src/lib.rs "),
        "explain @src/lib.rs "
    );
    assert_eq!(replace_last_token("@src", "@src/lib.rs "), "@src/lib.rs ");
}

#[test]
fn command_search_matches_names_titles_and_empty_query() {
    assert!(commands_matching("").len() >= 5);
    assert!(
        commands_matching("rev")
            .iter()
            .any(|command| command.name == "review")
    );
    assert!(
        commands_matching("settings")
            .iter()
            .any(|command| command.name == "settings")
    );
    assert!(commands_matching("nonsense").is_empty());
}

#[test]
fn composer_grows_with_lines_and_is_bounded() {
    assert_eq!(composer_height("", false), 28.);
    assert_eq!(composer_height("one\ntwo", false), 48.);
    assert_eq!(composer_height(&"x\n".repeat(20), false), 168.);
    assert_eq!(composer_height("", true), 44.);
    assert_eq!(composer_height(&"x\n".repeat(20), true), 184.);
}

#[test]
fn relative_times_cover_every_bucket() {
    let now = 10_000_000_000u64;
    assert_eq!(relative_time(now, now), "just now");
    assert_eq!(relative_time(now - 60_000, now), "1m ago");
    assert_eq!(relative_time(now - 3_600_000, now), "1h ago");
    assert_eq!(relative_time(now - 172_800_000, now), "2d ago");
    assert_eq!(relative_time(now - 1_209_600_000, now), "2w ago");
}

#[test]
fn run_state_colors_and_activity_classification_are_distinct() {
    assert_eq!(run_state_color(AgentRunState::Executing), rgb(0x93c5fd));
    assert_eq!(run_state_color(AgentRunState::Failed), rgb(0xfca5a5));
    assert!(session_is_active(AgentSessionState::Executing));
    assert!(session_is_active(AgentSessionState::NeedsInput));
    assert!(!session_is_active(AgentSessionState::Idle));
    assert!(!session_is_active(AgentSessionState::Archived));
}

#[test]
fn about_backend_labels_describe_every_route() {
    assert_eq!(about_backend(false, false, None), AboutBackend::Local);
    assert_eq!(
        about_backend(false, false, Some("ws://worker.example:8443/ws")),
        AboutBackend::Remote
    );
    assert_eq!(about_backend(true, false, None), AboutBackend::Demo);
    assert_eq!(
        about_backend(false, true, Some("wss://worker.example:8443/ws")),
        AboutBackend::Browser
    );
    // A demo workspace wins over any configured endpoint because nothing real
    // is connected in that mode.
    assert_eq!(
        about_backend(true, true, Some("wss://worker.example:8443/ws")),
        AboutBackend::Demo
    );

    assert_eq!(
        about_backend_label(false, false, None),
        "Local · in-process backend"
    );
    assert_eq!(
        about_backend_label(false, false, Some("ws://worker.example:8443/ws")),
        "Remote · ws://worker.example:8443"
    );
    assert_eq!(
        about_backend_label(true, false, None),
        "Demo · deterministic provider"
    );
    assert_eq!(
        about_backend_label(false, true, Some("wss://worker.example:8443/ws")),
        "Browser · wss://worker.example:8443"
    );
    assert_eq!(
        about_backend_label(false, true, None),
        "Browser · not connected"
    );
}

#[test]
fn endpoint_labels_strip_credentials_paths_and_queries() {
    let url = "wss://user:secret@worker.example:8443/ws?token=hidden";
    assert_eq!(endpoint_host(url), "worker.example:8443");
    assert_eq!(endpoint_label(url), "wss://worker.example:8443");
    assert!(!endpoint_label(url).contains("secret"));
    assert!(!endpoint_label(url).contains("hidden"));

    assert_eq!(
        endpoint_host("worker.example:8443/ws"),
        "worker.example:8443"
    );
    assert_eq!(endpoint_label("worker.example:8443"), "worker.example:8443");
    assert_eq!(endpoint_host("ws://[::1]:8443/ws"), "[::1]:8443");
    assert_eq!(endpoint_label("ws://[::1]:8443/ws"), "ws://[::1]:8443");
    assert_eq!(endpoint_host("  ws://host:1234/  "), "host:1234");
    assert_eq!(endpoint_host(""), "");
    assert_eq!(endpoint_label(""), "");
}

#[test]
fn version_and_platform_labels_handle_missing_build_metadata() {
    assert_eq!(about_version_label("0.1.0", "a1b2c3d"), "0.1.0 · a1b2c3d");
    assert_eq!(about_version_label("0.1.0", "unknown"), "0.1.0");
    assert_eq!(about_version_label("0.1.0", ""), "0.1.0");
    assert_eq!(about_version_label("0.1.0", "   "), "0.1.0");
    assert_eq!(
        about_platform_label("linux", "x86_64", false),
        "linux x86_64 · native"
    );
    assert_eq!(
        about_platform_label("macos", "aarch64", true),
        "macos aarch64 · browser"
    );
    assert!(!about_git_revision().is_empty());
}

#[test]
fn protocol_labels_report_matches_mismatches_and_unknown_servers() {
    let client = ProtocolVersion::new(11, 1);

    let matched = about_protocol(client, Some(ProtocolVersion::new(11, 1)));
    assert_eq!(matched.text, "client 11.1 · server 11.1");
    assert!(!matched.mismatch);

    let older = about_protocol(client, Some(ProtocolVersion::new(11, 0)));
    assert_eq!(older.text, "client 11.1 · server 11.0");
    assert!(older.mismatch);

    let unknown = about_protocol(client, None);
    assert_eq!(unknown.text, "client 11.1 · server unknown");
    assert!(!unknown.mismatch);
}

#[test]
fn about_link_urls_are_derived_from_the_repository_constant() {
    let links = about_links();
    let urls = links.iter().map(AboutLink::url).collect::<Vec<_>>();
    assert_eq!(
        urls,
        vec![
            "https://github.com/bearmuckle/loom/tree/main/docs",
            "https://github.com/bearmuckle/loom",
            "https://github.com/bearmuckle/loom/releases",
            "https://github.com/bearmuckle/loom/security/advisories/new",
        ]
    );
    assert!(urls.iter().all(|url| url.starts_with(LOOM_REPOSITORY_URL)));
    let ids = links.iter().map(|link| link.id).collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            "about-link-docs",
            "about-link-source",
            "about-link-releases",
            "about-link-security",
        ]
    );
}

#[test]
fn disconnected_screen_copy_distinguishes_first_connect_from_connection_loss() {
    let fresh = disconnected_screen(None);
    assert_eq!(fresh.heading, "No worker connected");
    assert!(!fresh.reconnect);
    assert_eq!(
        fresh.footer,
        "Not connected  ·  Connect a worker in Settings"
    );

    let lost = disconnected_screen(Some("the connection closed (code 1006)"));
    assert_eq!(lost.heading, "Connection lost");
    assert!(lost.reconnect);
    assert!(lost.detail.contains("code 1006"));
    assert_eq!(
        lost.empty,
        "Reconnect to reload your projects and sessions."
    );
    assert_eq!(lost.footer, "Disconnected  ·  connection lost");
}

#[test]
fn block_markdown_is_detected_for_definite_bubble_widths() {
    assert!(has_block_markdown("1. first\n2. second"));
    assert!(has_block_markdown("3) third"));
    assert!(has_block_markdown("- bullet"));
    assert!(has_block_markdown("```\ncode\n```"));
    assert!(has_block_markdown("| a | b |\n| - | - |"));
    assert!(!has_block_markdown("Run the tests"));
    assert!(!has_block_markdown("Release 2.0 shipped"));
    assert!(!has_block_markdown("inline **bold** only"));
}
