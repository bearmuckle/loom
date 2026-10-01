use super::*;

#[cfg(test)]
mod review_path_tests {
    use super::belongs_to_repository;
    use loom_core::{RepositoryId, Timestamp};
    use loom_protocol::SessionRepository;

    #[test]
    fn mounted_repository_activity_does_not_appear_as_other_workspace_files() {
        let repositories = ["repositories/first-id", "sources/local/second-id"]
            .into_iter()
            .map(|path| SessionRepository {
                id: RepositoryId::new(),
                source: "/code/project".to_owned(),
                path: path.to_owned(),
                revision: None,
                attached_at: Timestamp::from_unix_millis(0),
            })
            .collect::<Vec<_>>();
        assert!(belongs_to_repository(
            "repositories/first-id/src/lib.rs",
            &repositories
        ));
        assert!(belongs_to_repository(
            "sources/local/second-id/README.md",
            &repositories
        ));
        assert!(!belongs_to_repository(
            "repositories/first-id-extra/file",
            &repositories
        ));
        assert!(!belongs_to_repository("notes/todo.md", &repositories));
    }
}

#[cfg(test)]
mod session_name_tests {
    use super::{SessionCreationSource, session_name_for_path, session_name_for_source};
    use loom_protocol::GitHubRepository;
    use std::path::Path;

    #[test]
    fn local_source_uses_its_folder_name() {
        let source =
            SessionCreationSource::LocalDirectory("/home/user/work/my-project/".to_owned());
        assert_eq!(session_name_for_source(&source), "my-project");
        assert_eq!(
            session_name_for_path(Path::new("/home/user/work/my-project")),
            Some("my-project".to_owned())
        );
    }

    #[test]
    fn github_source_uses_repository_name_without_owner() {
        let source = SessionCreationSource::GitHub(GitHubRepository {
            full_name: "bearmuckle/loom".to_owned(),
            description: None,
            clone_url: "https://github.com/bearmuckle/loom.git".to_owned(),
            private: false,
            default_branch: "main".to_owned(),
        });
        assert_eq!(session_name_for_source(&source), "loom");
    }

    #[test]
    fn sources_without_a_usable_name_receive_a_safe_fallback() {
        assert_eq!(
            session_name_for_source(&SessionCreationSource::LocalDirectory("/".to_owned())),
            "New session"
        );
        assert_eq!(session_name_for_path(Path::new("/")), None);
        assert_eq!(
            session_name_for_source(&SessionCreationSource::GitHub(GitHubRepository {
                full_name: "owner/ ".to_owned(),
                description: None,
                clone_url: "https://github.com/owner/repo.git".to_owned(),
                private: false,
                default_branch: "main".to_owned(),
            })),
            "New session"
        );
    }
}

#[cfg(test)]
mod display_helper_tests {
    use super::{
        AgentActivityData, AgentActivityRecord, AgentActivityStatus, CompletionKind,
        FileActivityOperation, ToolPart, ToolPartStatus, activity_output, change_kind_label,
        command_line, command_purpose, commands_matching, completion_for_value, composer_height,
        format_bytes, format_duration, format_percentage, humanize_tool_output,
        is_redundant_completion_summary, relative_time, replace_command_token, replace_last_token,
        rgb, run_state_color, run_state_label, session_is_active, session_status_pill,
        source_mount_path, tool_detail, tool_group_label, tool_group_status,
        tool_part_from_activity, tool_status, tool_title, tool_title_for_activity,
    };
    use loom_core::{ActivityId, AgentSessionState, RunId, Timestamp};
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
        call.arguments =
            json!({"query": "needle", "path": "src", "glob": "**/*.rs", "regex": true});
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
            "Open fix → main in owner/name"
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
        assert_eq!(status(&[Completed, Failed]), Failed);
        assert_eq!(status(&[Completed, Cancelled]), Cancelled);
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
}

#[cfg(test)]
mod session_header_render_tests {
    use super::{header_tooltip, session_header_actions, session_header_title};
    use gpui_kit::component::button::Button;
    use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    use gpui_kit::{Context, TestAppContext, Window, div, prelude::*, px, size};
    use std::time::Duration;

    struct SessionHeader;

    impl Render for SessionHeader {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    session_header_title()
                        .child("A long session name that must leave room for header actions"),
                )
                .child(
                    session_header_actions()
                        .child(header_tooltip(
                            "session-sources-tooltip",
                            "Session sources",
                            Button::new("session-sources").icon(gpui_kit::component::Icon::new(
                                gpui_kit::assets::IconName::ListTree,
                            )),
                        ))
                        .child(Button::new("toggle-review-sidebar").icon(
                            gpui_kit::component::Icon::new(
                                gpui_kit::component::IconName::PanelRightOpen,
                            ),
                        )),
                )
        }
    }

    #[gpui_kit::test]
    fn header_actions_remain_visible_at_desktop_width(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(950.), px(100.)), |_, _| SessionHeader);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let mut previous_right = px(0.);
            for id in ["session-sources", "toggle-review-sidebar"] {
                let action = window.find(id);
                assert!(action.visible(), "{id} should be visible");
                assert!(action.bounds().size.width > px(0.));
                assert!(action.bounds().right() <= window.viewport_size().width);
                assert!(action.bounds().left() >= previous_right);
                previous_right = action.bounds().right();
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn header_tooltip_appears_on_hover(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(950.), px(100.)), |_, _| SessionHeader);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.hover("session-sources-tooltip", cx);
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_millis(1000), |window, _| {
            window
                .try_find("loom-header-tooltip")
                .is_some_and(|tooltip| tooltip.visible())
        })
        .await;
    }
}

#[cfg(test)]
mod loom_view_render_tests {
    use super::{
        LoomView, SETTINGS_SECTIONS, SessionSourceChoice, SessionSourceDialog,
        SessionSourceDialogPurpose, SettingsSection, WorkerConnectionState, WorkerNodeEntry,
    };
    use crate::state::GitHubLoginState;
    use crate::state::InspectorTab;
    use crate::state::RenameDialogState;
    use crate::state::ReviewRow;
    use crate::state::ThemeChoice;
    use crate::state::TimelineItem;
    use crate::state::{
        AssistantPart, AssistantTurn, EvidenceText, SystemNote, SystemTone, ToolPart,
        ToolPartStatus,
    };
    use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    use gpui_kit::{AppContext, TestAppContext, px, size};
    use loom_core::CapabilitySet;
    use loom_core::UsageSnapshot;
    use loom_core::{ActivityId, AgentSessionId, ErrorCode, RunId, Timestamp, ToolCallId};
    use loom_model::ProviderUsageSummary;
    use loom_model::{
        ModelCapabilities, ModelDescriptor, ModelId, ProviderHealth, ProviderKind, ProviderSummary,
        ToolCall,
    };
    use loom_protocol::ToolResult;
    use loom_protocol::{
        AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus,
        ClientRequest, ContextBudget, ContextInspection, ContextItem, ContextItemKind,
        ContextSummary, EventsResponse, FileActivityOperation, FilesystemRequest,
        FilesystemResponse, GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus,
        GitFileStatusKind, GitHubRepository, GitRepositoryStatus, ProviderRequest, RequestEnvelope,
        RunRequest, RunResponse, ServerResponse, SessionFilesystemChange, SessionFilesystemFile,
        SessionRepository, SessionRequest, SessionResponse, WorkerNodeResources, WorkerNodeStatus,
        WorkspaceChangeKind, WorkspaceEntry, WorkspaceEntryKind, WorkspaceRequest,
    };
    use std::collections::BTreeSet;
    use std::time::Duration;

    fn render_scenario(cx: &mut TestAppContext, configure: impl FnOnce(&mut LoomView)) {
        render_scenario_at(cx, size(px(1280.), px(800.)), configure);
    }

    fn render_scenario_at(
        cx: &mut TestAppContext,
        window_size: gpui_kit::Size<gpui_kit::Pixels>,
        configure: impl FnOnce(&mut LoomView),
    ) {
        let handle = cx.open_window(window_size, |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                configure(&mut view);
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn nested_project_view(focus_handle: gpui_kit::FocusHandle) -> LoomView {
        use loom_core::{
            AgentSessionSnapshot, AgentSessionState, DelegatedTaskRecord, DelegatedTaskStatus,
            ProjectAgentPermissions, ProjectAgentRecord, ProjectSnapshot, ProjectWorktreeRecord,
            ProjectWorktreeStatus, RepositoryId, TaskId, WorkspaceId,
        };

        let workspace_id = WorkspaceId::new();
        let root_id = AgentSessionId::new();
        let manager_id = AgentSessionId::new();
        let worker_id = AgentSessionId::new();
        let project_id = loom_core::ProjectId::from_uuid(*root_id.as_uuid());
        let timestamp = Timestamp::from_unix_millis(1);
        let sessions = [
            (root_id, "Project", AgentSessionState::Idle),
            (manager_id, "Manager", AgentSessionState::Executing),
            (worker_id, "Worker", AgentSessionState::Completed),
        ];
        let agents = sessions
            .iter()
            .enumerate()
            .map(|(index, (session_id, _, state))| ProjectAgentRecord {
                session_id: *session_id,
                project_id,
                parent_session_id: match index {
                    0 => None,
                    1 => Some(root_id),
                    _ => Some(manager_id),
                },
                depth: index as u8 + 1,
                state: *state,
                task_summary: None,
                output_cursor: Default::default(),
                updated_at: timestamp,
            })
            .collect();
        let manager_task_id = TaskId::new();
        let worker_task_id = TaskId::new();
        let manager_task = DelegatedTaskRecord {
            task_id: manager_task_id,
            project_id,
            requester_session_id: root_id,
            target_session_id: manager_id,
            child_name: "Manager".to_owned(),
            intent: "Coordinate the delegated work".to_owned(),
            model_id: "deterministic/demo".to_owned(),
            context_references: Vec::new(),
            dependencies: Vec::new(),
            code_change: false,
            permissions: ProjectAgentPermissions {
                delegation: true,
                branch_messaging: true,
                child_control: true,
                inspection: true,
                worktree_creation: true,
                review: true,
                integration: true,
            },
            status: DelegatedTaskStatus::Running,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let worker_task = DelegatedTaskRecord {
            task_id: worker_task_id,
            project_id,
            requester_session_id: manager_id,
            target_session_id: worker_id,
            child_name: "Worker".to_owned(),
            intent: "Implement the requested code change".to_owned(),
            model_id: "deterministic/demo".to_owned(),
            context_references: Vec::new(),
            dependencies: Vec::new(),
            code_change: true,
            permissions: ProjectAgentPermissions::default(),
            status: DelegatedTaskStatus::Completed,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let worker_worktree = ProjectWorktreeRecord {
            project_id,
            task_id: worker_task_id,
            parent_session_id: manager_id,
            child_session_id: worker_id,
            parent_repository_id: RepositoryId::new(),
            child_repository_id: RepositoryId::new(),
            relative_path: "worktrees/worker".to_owned(),
            worktree_name: "worker".to_owned(),
            branch_name: "agent/worker".to_owned(),
            base_revision: "parent-base".to_owned(),
            result_revision: Some("child-result".to_owned()),
            integrated_revision: None,
            status: ProjectWorktreeStatus::Ready,
            conflict_paths: Vec::new(),
            error: None,
            cleanup_disposition: None,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let session_snapshots = sessions
            .iter()
            .map(|(id, name, state)| AgentSessionSnapshot {
                id: *id,
                workspace_id,
                name: (*name).to_owned(),
                state: *state,
                created_at: timestamp,
                updated_at: timestamp,
            })
            .collect::<Vec<_>>();
        let mut view = LoomView::new_for_test(focus_handle);
        view.workspace_id = workspace_id;
        view.active_session = session_snapshots[0].clone();
        for session in &session_snapshots {
            view.session_node_ids
                .insert(session.id, view.default_backend_node_id.clone());
        }
        view.sessions = session_snapshots;
        view.project_snapshot = Some(ProjectSnapshot {
            project_id,
            root_session_id: root_id,
            agents,
            tasks: vec![manager_task, worker_task],
            worktrees: vec![worker_worktree],
        });
        view
    }

    #[gpui_kit::test]
    fn empty_session_view_renders_without_a_backend_round_trip(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |_| {});
    }

    #[gpui_kit::test]
    fn startup_rejects_credential_bearing_remote_urls_and_missing_tokens(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let options = |remote: &str, token: Option<&str>| super::UiOptions {
                project: None,
                task: "startup validation".to_owned(),
                demo: false,
                model: ModelId::new("deterministic/demo"),
                endpoint: None,
                api_key: None,
                remote: Some(remote.to_owned()),
                token: token.map(str::to_owned),
                reset_state: false,
            };
            let error = match LoomView::try_new(
                &options("ws://user:secret@worker.example", Some("token")),
                cx.focus_handle(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("credential-bearing URL was accepted"),
            };
            assert!(error.message.contains("must not contain credentials"));

            let error =
                match LoomView::try_new(&options("ws://worker.example", None), cx.focus_handle()) {
                    Err(error) => error,
                    Ok(_) => panic!("remote connection without a token was accepted"),
                };
            assert!(error.message.contains("require LOOM_TOKEN"));

            let error = match LoomView::try_new(
                &options("not a WebSocket URL", Some("token")),
                cx.focus_handle(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("invalid remote URL was accepted"),
            };
            assert_eq!(error.code, ErrorCode::InvalidRequest);

            let invalid_workspace =
                std::env::temp_dir().join(format!("loom-ui-missing-{}", uuid::Uuid::new_v4()));
            let local_options = super::UiOptions {
                project: Some(invalid_workspace),
                task: "startup validation".to_owned(),
                demo: false,
                model: ModelId::new("deterministic/demo"),
                endpoint: None,
                api_key: None,
                remote: None,
                token: None,
                reset_state: false,
            };
            let error = match LoomView::try_new(&local_options, cx.focus_handle()) {
                Err(error) => error,
                Ok(_) => panic!("missing workspace directory was accepted"),
            };
            assert_eq!(error.code, ErrorCode::WorkspaceAccessDenied);
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn connection_bootstrap_creates_and_attaches_a_local_workspace_session(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let _handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let options = super::UiOptions {
                project: None,
                task: "bootstrap test".to_owned(),
                demo: false,
                model: ModelId::new("deterministic/demo"),
                endpoint: None,
                api_key: None,
                remote: None,
                token: None,
                reset_state: false,
            };
            let workspace_root =
                std::env::temp_dir().join(format!("loom-ui-bootstrap-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&workspace_root).unwrap();
            let local_backend = loom_local::OwnedBackend::new();
            let connection = super::ClientConnection::InProcess(Box::new(local_backend.connect()));
            let connection_after_teardown = connection.clone();
            crate::connection::negotiate(&connection).unwrap();
            let mut view = LoomView::initialize_from_connection(
                &options,
                connection,
                workspace_root.clone(),
                false,
                None,
                cx.focus_handle(),
                false,
            )
            .unwrap();
            view.owned_backend = Some(local_backend);
            assert_eq!(view.workspaces.len(), 1);
            assert_eq!(view.sessions.len(), 1);
            assert!(view.models.contains(&ModelId::new("deterministic/demo")));
            assert_eq!(view.session_directories.len(), 1);
            // Avoid starting the live worker's delayed status poll in this synchronous UI test.
            view.worker_nodes.clear();
            let _ = std::fs::remove_dir_all(workspace_root);
            view.shutdown_owned_backend();
            assert!(
                connection_after_teardown
                    .request(RequestEnvelope::new(ClientRequest::Workspace(
                        WorkspaceRequest::ListWorkspaces
                    )))
                    .result
                    .is_err()
            );
            view
        });
    }

    #[gpui_kit::test]
    fn session_list_renders_owner_and_resource_summary(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "Test session".to_owned();
            view.sessions = vec![view.active_session.clone()];
            view.session_node_ids
                .insert(view.active_session.id, "test-node".to_owned());
            view.node_names
                .insert("test-node".to_owned(), "Local worker".to_owned());
            view.worker_nodes.push(WorkerNodeEntry {
                id: 0,
                status: WorkerNodeStatus {
                    node_id: "test-node".to_owned(),
                    name: "Local worker".to_owned(),
                    online: true,
                    capabilities: CapabilitySet::default(),
                    resources: WorkerNodeResources {
                        cpu_count: 4,
                        cpu_usage_percent: Some(45),
                        memory_usage_percent: Some(61),
                        memory_total_bytes: Some(8 * 1024 * 1024 * 1024),
                        memory_available_bytes: Some(3 * 1024 * 1024 * 1024),
                        disk_total_bytes: Some(64 * 1024 * 1024 * 1024),
                        disk_available_bytes: Some(32 * 1024 * 1024 * 1024),
                    },
                },
                is_local: true,
                url: None,
                connection: None,
                connection_state: WorkerConnectionState::Connected,
                connection_detail: None,
                severe_load_streak: 0,
            });
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find(("session-tree-root", 0usize)).visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn nested_project_tree_selects_grandchild_session_by_click(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
        let rendered_view_for_window = rendered_view.clone();
        let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
            let view = cx.new(|cx| nested_project_view(cx.focus_handle()));
            *rendered_view_for_window.borrow_mut() = Some(view.clone());
            gpui_kit::component::Root::new(view, window, cx)
        });
        let view = rendered_view.borrow().as_ref().unwrap().clone();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            for index in 0usize..3 {
                assert!(window.find(("session-tree-root", index)).visible());
            }
            window.click(("session-tree-root", 2usize), cx);
            assert_eq!(view.read(cx).active_session.name, "Worker");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn archiving_a_project_removes_descendant_sessions_from_the_tree(cx: &mut TestAppContext) {
        // Build the view without opening a window so the project poll (which
        // would run while a live project is rendered) never starts.
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            let root_id = view.project_snapshot.as_ref().unwrap().root_session_id;
            assert_eq!(view.sessions.len(), 3);
            let removed_active = view.forget_archived_session(root_id);
            assert!(removed_active, "the active root session should be removed");
            assert!(
                view.sessions.is_empty(),
                "archived descendants must leave the session list"
            );
            assert!(view.project_snapshot.is_none());
            view
        });
        let _ = view;
    }

    #[gpui_kit::test]
    fn archiving_a_child_removes_only_that_session(cx: &mut TestAppContext) {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            let child_id = view
                .project_snapshot
                .as_ref()
                .unwrap()
                .agents
                .iter()
                .find(|agent| agent.depth == 3)
                .expect("nested project has a depth-three agent")
                .session_id;
            let removed_active = view.forget_archived_session(child_id);
            assert!(
                !removed_active,
                "the active session is the root, not the child"
            );
            assert_eq!(view.sessions.len(), 2);
            let project = view.project_snapshot.as_ref().unwrap();
            assert!(
                project
                    .agents
                    .iter()
                    .all(|agent| agent.session_id != child_id)
            );
            assert!(
                project
                    .tasks
                    .iter()
                    .all(|task| task.target_session_id != child_id)
            );
            assert!(
                project
                    .worktrees
                    .iter()
                    .all(|worktree| worktree.child_session_id != child_id)
            );
            view
        });
        let _ = view;
    }

    #[gpui_kit::test]
    fn project_session_popup_menu_dispatches_child_control_review_and_integration(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (control_window, control_view) = open_nested_project_menu(cx, 1);
        cx.update_window(control_window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("popup-menu").visible());
            let mut menu = window.within("popup-menu");
            assert_eq!(menu.find(2usize).label(), Some("Pause child"));
            assert_eq!(menu.find(3usize).label(), Some("Interrupt child"));
            assert_eq!(
                menu.find(4usize).label(),
                Some("Cancel child and descendants")
            );
            menu.click(2usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(control_window, |_, _, cx| {
            assert!(
                control_view
                    .read(cx)
                    .status_banner
                    .as_ref()
                    .is_some_and(|note| {
                        note.tone == SystemTone::Error
                            && note
                                .heading
                                .as_deref()
                                .is_some_and(|heading| heading.starts_with("control project child"))
                    })
            );
        })
        .unwrap();

        let (review_window, review_view) = open_nested_project_menu(cx, 2);
        cx.update_window(review_window, |_, window, cx| {
            window.render_frame(cx);
            let mut menu = window.within("popup-menu");
            assert_eq!(menu.find(2usize).label(), Some("Review child changes"));
            assert_eq!(
                menu.find(3usize).label(),
                Some("Fast-forward child changes")
            );
            assert_eq!(menu.find(4usize).label(), Some("Keep child checkout"));
            assert_eq!(
                menu.find(5usize).label(),
                Some("Remove clean child checkout")
            );
            menu.click(2usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(review_window, |_, _, cx| {
            assert!(
                review_view
                    .read(cx)
                    .status_banner
                    .as_ref()
                    .is_some_and(|note| {
                        note.tone == SystemTone::Error
                            && note
                                .heading
                                .as_deref()
                                .is_some_and(|heading| heading.starts_with("review project child"))
                    })
            );
        })
        .unwrap();

        let (integration_window, integration_view) = open_nested_project_menu(cx, 2);
        cx.update_window(integration_window, |_, window, cx| {
            window.render_frame(cx);
            window.within("popup-menu").click(3usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(integration_window, |_, _, cx| {
            assert!(
                integration_view
                    .read(cx)
                    .status_banner
                    .as_ref()
                    .is_some_and(|note| {
                        note.tone == SystemTone::Error
                            && note.heading.as_deref().is_some_and(|heading| {
                                heading.starts_with("check child review before integration")
                            })
                    })
            );
        })
        .unwrap();

        drop(control_view);
        drop(review_view);
        drop(integration_view);
        cx.quit();
        cx.run_until_parked();
    }

    fn open_nested_project_menu(
        cx: &mut TestAppContext,
        session_index: usize,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<LoomView>) {
        let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
        let rendered_view_for_window = rendered_view.clone();
        let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
            let view = cx.new(|cx| {
                let mut view = nested_project_view(cx.focus_handle());
                view.project_poll_scheduled = true;
                view
            });
            let session = view.read(cx).sessions[session_index].clone();
            let project = view.read(cx).project_snapshot.clone();
            let menu_view = view.clone();
            let menu =
                gpui_kit::component::menu::PopupMenu::build(window, cx, move |menu, _, _| {
                    LoomView::build_project_session_context_menu(menu, session, project, menu_view)
                });
            *rendered_view_for_window.borrow_mut() = Some(view);
            gpui_kit::component::Root::new(menu, window, cx)
        });
        let view = rendered_view.borrow_mut().take().unwrap();
        (handle.into(), view)
    }

    #[gpui_kit::test]
    fn run_projection_maps_messages_plan_and_completion_evidence(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.sessions = vec![view.active_session.clone()];
            view.apply_run_projection(loom_protocol::AgentRunSnapshotProjection {
                run: loom_protocol::AgentRunSnapshot {
                    id: RunId::new(),
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 0,
                    session_id: view.active_session.id,
                    task: "inspect the repository".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    state: loom_protocol::AgentRunState::Completed,
                    started_at: Timestamp::from_unix_millis(1),
                    updated_at: Timestamp::from_unix_millis(2),
                    completed_at: Some(Timestamp::from_unix_millis(2)),
                    summary: Some("Reviewed the project".to_owned()),
                    evidence: vec![loom_core::EvidenceLink {
                        label: "readme".to_owned(),
                        uri: "file:///README.md".to_owned(),
                    }],
                },
                plan: vec![loom_protocol::AgentPlanStep {
                    id: "step-1".to_owned(),
                    description: "Read the project files".to_owned(),
                }],
                messages: vec![
                    loom_model::ModelMessage::new(loom_model::MessageRole::System, "system"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::User, "inspect"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "first"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "second"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, ""),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Tool, "tool output"),
                ],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
                message_timeline_ordinals: vec![0, 1, 2, 3, 4, 5],
            });
        });
    }

    #[gpui_kit::test]
    fn run_projection_keeps_existing_timeline_and_suppresses_redundant_summary(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.sessions = vec![view.active_session.clone()];
            view.timeline = vec![TimelineItem::Assistant(AssistantTurn::text(
                "existing transcript",
            ))];
            view.apply_run_projection(loom_protocol::AgentRunSnapshotProjection {
                run: loom_protocol::AgentRunSnapshot {
                    id: RunId::new(),
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 0,
                    session_id: view.active_session.id,
                    task: "task".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    state: loom_protocol::AgentRunState::Completed,
                    started_at: Timestamp::from_unix_millis(1),
                    updated_at: Timestamp::from_unix_millis(2),
                    completed_at: Some(Timestamp::from_unix_millis(2)),
                    summary: Some("Completed task: task".to_owned()),
                    evidence: Vec::new(),
                },
                plan: Vec::new(),
                messages: vec![loom_model::ModelMessage::new(
                    loom_model::MessageRole::User,
                    "do not duplicate",
                )],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
                message_timeline_ordinals: vec![0],
            });
        });
    }

    #[gpui_kit::test]
    fn session_activation_resets_projection_and_resolves_backend_ownership(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session_id = loom_core::AgentSessionId::new();
            let model = ModelId::new("deterministic/next");
            view.session_models.insert(session_id, model.clone());
            view.session_auto_approve_actions.insert(session_id, false);
            view.timeline
                .push(TimelineItem::System(SystemNote::status("old status")));
            view.pending_input = Some("old prompt".to_owned());
            view.active_run_id = Some(RunId::new());
            view.review.selected_path = Some("old.rs".to_owned());
            view.session_node_ids
                .insert(session_id, view.default_backend_node_id.clone());

            view.activate_session(loom_core::AgentSessionSnapshot {
                id: session_id,
                workspace_id: view.workspace_id,
                name: "Next session".to_owned(),
                state: loom_core::AgentSessionState::Idle,
                created_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(1),
            });

            assert_eq!(view.model, model);
            assert!(!view.auto_approve_actions);
            assert!(view.timeline.is_empty());
            assert!(view.pending_input.is_none());
            assert!(view.active_run_id.is_none());
            assert!(view.review.selected_path.is_none());
            assert!(
                view.backend_for_request(&loom_protocol::ClientRequest::Session(
                    SessionRequest::GetAgentSessionSnapshot { session_id }
                ))
                .is_ok()
            );
            assert!(
                view.backend_for_request(&loom_protocol::ClientRequest::Provider(
                    ProviderRequest::ListProviders
                ))
                .is_ok()
            );

            view.review.rows = vec![
                ReviewRow::Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                },
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Removed,
                    old_line: Some(1),
                    new_line: None,
                    content: "removed".to_owned(),
                }),
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "added".to_owned(),
                }),
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Context,
                    old_line: Some(2),
                    new_line: Some(2),
                    content: "context".to_owned(),
                }),
            ];
            view.review.hunk_rows = vec![0];
            view.review.collapsed_hunks.insert(0);
            for index in 0..=view.review.rows.len() {
                let _ = view.render_review_row(index);
            }
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn settings_about_and_providers_panes_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| view.settings_open = true);
        render_scenario(cx, |view| {
            view.settings_open = true;
            view.settings_section = SettingsSection::About;
        });
        render_scenario(cx, |view| {
            view.settings_open = true;
            view.settings_section = SettingsSection::Providers;
        });
    }

    #[gpui_kit::test]
    fn settings_section_navigation_renders_every_pane(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("settings-button", cx);
            window.render_frame(cx);
            for index in 0..SETTINGS_SECTIONS.len() {
                window
                    .within("settings-dialog")
                    .click(("settings-section", index), cx);
                window.render_frame(cx);
                assert!(
                    window
                        .within("settings-dialog")
                        .find(("settings-section", index))
                        .visible()
                );
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn settings_dialog_close_control_handles_a_real_click(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("settings-button", cx);
            window.render_frame(cx);
            assert!(
                window
                    .within("settings-dialog")
                    .find("close-settings")
                    .visible()
            );
            window
                .within("settings-dialog")
                .click("cpu-pulse-threshold-decrease", cx);
            window
                .within("settings-dialog")
                .click("cpu-pulse-threshold-increase", cx);
            window
                .within("settings-dialog")
                .click("project-agent-concurrency-decrease", cx);
            window
                .within("settings-dialog")
                .click("project-agent-concurrency-increase", cx);
            window
                .within("settings-dialog")
                .click("session-auto-approve-toggle", cx);
            window.within("settings-dialog").click("close-settings", cx);
            window.render_frame(cx);
            assert!(window.try_find("settings-dialog").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn session_source_dialog_choices_and_close_button_work(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                view.source_dialog = Some(SessionSourceDialog {
                    purpose: SessionSourceDialogPurpose::StartSession,
                    choice: SessionSourceChoice::Empty,
                    local_directory_available: true,
                    filter_subscription: None,
                    repositories: Vec::new(),
                    selected_repository: None,
                    repositories_loading: false,
                    error: None,
                });
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            assert!(window.find("source-local-directory").visible());
            window.click("source-local-directory", cx);
            window.click("source-github", cx);
            window.click("close", cx);
            window.render_frame(cx);
            assert!(window.try_find("source-local-directory").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn local_worker_current_directory_prefills_the_source_dialog(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                view.local_current_directory =
                    Some(std::path::PathBuf::from("/tmp/current-project"));

                // Adding to a session opens on the local folder and prefills it.
                view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                assert_eq!(
                    view.pending_source_path.as_deref(),
                    Some("/tmp/current-project")
                );

                // Choosing the local folder after another choice prefills again.
                view.pending_source_path = None;
                view.choose_source(SessionSourceChoice::GitHub, cx);
                view.choose_source(SessionSourceChoice::LocalDirectory, cx);
                assert_eq!(
                    view.pending_source_path.as_deref(),
                    Some("/tmp/current-project")
                );

                // The explicit action replaces any pending path, and is a no-op
                // when there is no native current directory.
                view.pending_source_path = Some("/somewhere/else".to_owned());
                view.use_current_source_directory(cx);
                assert_eq!(
                    view.pending_source_path.as_deref(),
                    Some("/tmp/current-project")
                );
                view.local_current_directory = None;
                view.pending_source_path = None;
                view.use_current_source_directory(cx);
                assert!(view.pending_source_path.is_none());

                // Restore the dialog for a real render and click.
                view.local_current_directory =
                    Some(std::path::PathBuf::from("/tmp/current-project"));
                view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            assert!(window.find("use-current-session-directory").visible());
            window.click("use-current-session-directory", cx);
            window.render_frame(cx);
            // Confirming without a typed path uses the prefilled current folder.
            window.click("confirm-session-source", cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn phone_source_dialog_stacks_choices_and_keeps_actions_visible(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(390.), px(520.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                view.source_dialog = Some(SessionSourceDialog {
                    purpose: SessionSourceDialogPurpose::StartSession,
                    choice: SessionSourceChoice::GitHub,
                    local_directory_available: true,
                    filter_subscription: None,
                    repositories: (0..40)
                        .map(|index| GitHubRepository {
                            full_name: format!("owner/repository-{index}"),
                            description: Some("example repository".to_owned()),
                            clone_url: format!("https://github.com/owner/repository-{index}.git"),
                            private: false,
                            default_branch: "main".to_owned(),
                        })
                        .collect(),
                    selected_repository: None,
                    repositories_loading: false,
                    error: None,
                });
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            let viewport = window.viewport_size();
            let empty = window.find("source-empty");
            let local = window.find("source-local-directory");
            let github = window.find("source-github");
            assert!(empty.visible() && local.visible() && github.visible());
            // The three source choices stack vertically on a phone-width window
            // instead of overflowing the dialog's right edge.
            assert!(empty.bounds().bottom() <= local.bounds().top());
            assert!(local.bounds().bottom() <= github.bounds().top());
            for id in [
                "source-empty",
                "source-local-directory",
                "source-github",
                "cancel-session-source",
                "confirm-session-source",
            ] {
                let bounds = window.find(id).bounds();
                assert!(
                    bounds.right() <= viewport.width,
                    "{id} ran off the right edge: {bounds:?}"
                );
            }
            // The actions stay pinned and reachable even though the repository
            // list inside the scrollable body is longer than the window.
            assert!(window.find("cancel-session-source").visible());
            assert!(window.find("confirm-session-source").visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn session_source_and_review_actions_cover_empty_invalid_and_missing_states(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());

                view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
                view.choose_source(SessionSourceChoice::LocalDirectory, cx);
                view.confirm_source_dialog(cx);
                assert!(view.source_dialog.is_some());
                assert!(view.status_banner.is_some());

                view.choose_source(SessionSourceChoice::GitHub, cx);
                view.choose_source(SessionSourceChoice::Empty, cx);
                view.confirm_source_dialog(cx);
                assert!(view.source_dialog.is_none());

                view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                view.choose_source(SessionSourceChoice::GitHub, cx);
                view.confirm_source_dialog(cx);
                assert!(view.source_dialog.is_some());

                view.review.open = false;
                view.toggle_review_pane(cx);
                assert!(view.review.open);
                view.jump_review_hunk(true, cx);
                view.review.hunk_rows = vec![2, 5];
                view.jump_review_hunk(true, cx);
                assert_eq!(view.review.selected_hunk, 0);
                view.jump_review_hunk(false, cx);
                assert_eq!(view.review.selected_hunk, 0);
                view.open_review_diff("missing.txt".to_owned(), false, cx);
                assert!(view.review.selected_path.is_none());
                view.open_review_file("missing.txt".to_owned(), cx);
                assert_eq!(view.review.selected_path.as_deref(), Some("missing.txt"));
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn settings_provider_views_and_theme_actions_update_the_view_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.review.open = true;
            view.settings_open = true;
            view.github_login = Some(GitHubLoginState::Starting);
            view.open_providers_from_menu(cx);
            assert!(view.settings_open);
            assert_eq!(view.settings_section, SettingsSection::Providers);
            assert!(!view.review.open);
            assert!(view.github_login.is_none());
            assert_eq!(view.providers_node_id.as_deref(), Some("test-node"));

            view.observe_system_appearance(window, cx);
            view.observe_system_appearance(window, cx);
            view.select_theme(ThemeChoice::Light, window, cx);
            assert_eq!(view.theme_choice, ThemeChoice::Light);
            view.select_theme(ThemeChoice::Dark, window, cx);
            assert_eq!(view.theme_choice, ThemeChoice::Dark);
            view.select_theme(ThemeChoice::System, window, cx);
            assert_eq!(view.theme_choice, ThemeChoice::System);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn parked_runs_keep_polling_until_terminal(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |view, _, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            view.update(cx, |view, _| {
                view.active_run_id = Some(RunId::new());
                view.run_state = Some(loom_protocol::AgentRunState::Executing);
                assert!(view.run_should_poll());
                view.run_state = Some(loom_protocol::AgentRunState::Paused);
                assert!(view.run_should_poll());
                view.run_state = Some(loom_protocol::AgentRunState::Completed);
                assert!(!view.run_should_poll());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn repeated_tool_calls_collapse_into_a_group(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let parts = ["a.rs", "b.rs", "c.rs", "d.rs"]
                .iter()
                .map(|path| {
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: ToolCallId::new(),
                        name: "read_file".to_owned(),
                        title: format!("Read {path}"),
                        status: ToolPartStatus::Completed,
                        detail: Some((*path).to_owned()),
                        output: None,
                        elapsed_ms: Some(1),
                        approval_pending: false,
                    }))
                })
                .collect();
            view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
                parts,
                streaming: false,
            })];
            view
        });
        cx.update_window(handle.into(), |view, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(("tool-group-header", 0u64)).is_some());
            assert!(window.try_find(("tool-header", 0u64)).is_none());
            window.click(("tool-group-header", 0u64), cx);
            window.render_frame(cx);
            assert!(window.try_find(("tool-header", 0u64)).is_some());
            let view = view.downcast::<LoomView>().unwrap();
            view.update(cx, |view, _| {
                assert!(view.expanded_tool_groups.contains(&0));
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn tool_groups_render_pending_active_and_failed_statuses(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.timeline = [
                ToolPartStatus::Queued,
                ToolPartStatus::Running,
                ToolPartStatus::Failed,
                ToolPartStatus::Cancelled,
            ]
            .into_iter()
            .map(|status| {
                let parts = (0..3)
                    .map(|offset| {
                        AssistantPart::Tool(Box::new(ToolPart {
                            id: ToolCallId::new(),
                            name: "run_command".to_owned(),
                            title: format!("Run command {offset}"),
                            status,
                            detail: None,
                            output: None,
                            elapsed_ms: None,
                            approval_pending: false,
                        }))
                    })
                    .collect();
                TimelineItem::Assistant(AssistantTurn {
                    parts,
                    streaming: false,
                })
            })
            .collect();
        });
    }

    #[gpui_kit::test]
    fn command_palette_and_composer_completion_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |view, window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            window.render_frame(cx);
            view.update(cx, |view, cx| view.toggle_command_palette(cx));
            window.render_frame(cx);
            assert!(window.try_find("command-palette").is_some());
            view.update(cx, |view, cx| view.close_command_palette(cx));
            window.render_frame(cx);
            assert!(window.try_find("command-palette").is_none());

            view.update(cx, |view, cx| {
                if let Some(input) = view.composer_input.clone() {
                    input.update(cx, |state, cx| state.set_value("/re", window, cx));
                }
                view.composer_completion = Some(super::ComposerCompletion {
                    kind: super::CompletionKind::Command,
                    query: "/re".to_owned(),
                    selected: 0,
                });
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("composer-completions").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn primary_shift_p_opens_the_command_palette(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            LoomView::new_for_test(cx.focus_handle())
        });
        let window: gpui_kit::AnyWindowHandle = handle.into();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        // Platform key events report the shifted character for `key`, so the
        // binding must match `P` as well as `p`.
        let shortcut = || gpui_kit::Keystroke {
            modifiers: gpui_kit::Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            key: "P".to_owned(),
            key_char: None,
        };
        cx.dispatch_keystroke(window, shortcut());
        cx.update_window(window, |view, window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            assert!(view.read(cx).command_palette_open);
            window.render_frame(cx);
            assert!(window.try_find("command-palette").is_some());
        })
        .unwrap();
        // Pressing it again while the palette's own input is focused must
        // close the palette, not get swallowed by that input.
        cx.dispatch_keystroke(window, shortcut());
        cx.update_window(window, |view, window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            assert!(!view.read(cx).command_palette_open);
            // Tear down the palette input, dropping focus, before pressing again.
            window.render_frame(cx);
        })
        .unwrap();
        cx.dispatch_keystroke(window, shortcut());
        cx.update_window(window, |view, _window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            assert!(view.read(cx).command_palette_open);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn github_login_failures_update_account_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.handle_github_device_code(
                Err(loom_core::LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    "device failed",
                    true,
                )),
                cx,
            );
            assert!(matches!(
                view.github_login,
                Some(GitHubLoginState::Error(_))
            ));
            view.finish_github_login(
                Err(loom_core::LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    "poll failed",
                    true,
                )),
                cx,
            );
            assert!(matches!(
                view.github_login,
                Some(GitHubLoginState::Error(_))
            ));
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn github_repository_login_finishes_by_configuring_repository_access(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.github_login_kind = crate::state::GitHubLoginKind::Repository;
            view.finish_github_login(Ok("gho_repo_token".to_owned()), cx);
            view
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                assert!(view.github_repository_connected);
                assert!(matches!(view.github_login, Some(GitHubLoginState::Success)));
                view.refresh_github_repository_access(view.default_backend_node_id.clone(), cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn worker_connection_rejects_empty_credentialed_and_duplicate_inputs(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.connect_worker_node(cx);
            view.node_input_initial = "wss://user:secret@worker.example/ws token".to_owned();
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 1);
            assert_eq!(
                view.worker_nodes[0].connection_state,
                WorkerConnectionState::Failed
            );
            assert!(
                view.worker_nodes[0]
                    .connection_detail
                    .as_deref()
                    .unwrap()
                    .contains("Do not include credentials")
            );
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 1);
            view.node_input_initial = "wss://worker-without-token.example/ws".to_owned();
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 2);
            assert!(
                view.worker_nodes[1]
                    .connection_detail
                    .as_deref()
                    .unwrap()
                    .contains("URL followed by its access token")
            );
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    async fn worker_connection_failure_after_valid_input_is_reported(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.node_input_initial = "ws://127.0.0.1:1/ws test-token".to_owned();
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 1);
            view
        });
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.worker_nodes.iter().any(|node| {
                    node.url.as_deref() == Some("ws://127.0.0.1:1/ws")
                        && node.connection_state == WorkerConnectionState::Failed
                        && node.connection.is_none()
                })
            })
        })
        .await;
    }

    #[gpui_kit::test]
    fn reconnect_rejects_saved_url_credentials_and_worker_can_be_removed(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.worker_nodes.push(super::connection_placeholder(
                9,
                "wss://user:secret@worker.example/ws".to_owned(),
                WorkerConnectionState::Failed,
                None,
            ));
            view.reconnect_configured_worker_nodes(cx);
            assert_eq!(
                view.worker_nodes[0].connection_state,
                WorkerConnectionState::Failed
            );
            assert!(
                view.worker_nodes[0]
                    .connection_detail
                    .as_deref()
                    .unwrap()
                    .contains("credentials")
            );
            view.remove_worker_node(9, cx);
            assert!(view.worker_nodes.is_empty());
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn tool_blocks_toggle_and_show_inline_approvals(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let run_id = RunId::new();
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "run_command".to_owned(),
            arguments: serde_json::json!({"command": "cargo", "args": ["test"]}),
        };
        let record = AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 0,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::Command,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            elapsed_ms: Some(10),
            data: AgentActivityData::Command {
                call: call.clone(),
                command: "cargo".to_owned(),
                args: vec!["test".to_owned()],
                cwd: Some("repo".to_owned()),
                result: Some(ToolResult::success(&call, "test result: ok".to_owned())),
            },
        };
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.active_run_id = Some(run_id);
            view.consume_agent_event(&loom_protocol::AgentEvent::ActivityRecorded {
                run_id,
                activity: record.clone(),
            });
            view
        });
        cx.update_window(handle.into(), |view, window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            window.render_frame(cx);
            window.click(("tool-header", 0u64), cx);
            window.render_frame(cx);
            view.update(cx, |view, _| {
                assert!(view.expanded_tools.contains(&call.id));
                // Late activity updates keep the block in place and reveal approval.
                view.consume_agent_event(&loom_protocol::AgentEvent::ActivityRecorded {
                    run_id,
                    activity: AgentActivityRecord {
                        status: AgentActivityStatus::AwaitingApproval,
                        ..record.clone()
                    },
                });
                view.pending_approval = Some(call.clone());
            });
            window.render_frame(cx);
            assert!(window.try_find(("approve-tool", 0u64)).is_some());
            assert!(window.try_find(("reject-tool", 0u64)).is_some());
            window.click(("approve-tool", 0u64), cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn action_only_tools_hide_their_result_body(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let read_id = ToolCallId::new();
        let command_id = ToolCallId::new();
        let failed_command_id = ToolCallId::new();
        let search_id = ToolCallId::new();
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
                parts: vec![
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: read_id,
                        name: "read_file".to_owned(),
                        title: "Read src/lib.rs".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: Some("src/lib.rs".to_owned()),
                        output: Some("fn main() {}".to_owned()),
                        elapsed_ms: Some(4),
                        approval_pending: false,
                    })),
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: command_id,
                        name: "run_command".to_owned(),
                        title: "Run tests".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: Some("cargo test".to_owned()),
                        output: Some("test result: ok".to_owned()),
                        elapsed_ms: Some(8),
                        approval_pending: false,
                    })),
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: failed_command_id,
                        name: "run_command".to_owned(),
                        title: "Run tests".to_owned(),
                        status: ToolPartStatus::Failed,
                        detail: Some("cargo test".to_owned()),
                        output: Some("test result: FAILED".to_owned()),
                        elapsed_ms: Some(9),
                        approval_pending: false,
                    })),
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: search_id,
                        name: "web_search".to_owned(),
                        title: "Web search \"needle\"".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: None,
                        output: Some(r#"{"results":[]}"#.to_owned()),
                        elapsed_ms: Some(3),
                        approval_pending: false,
                    })),
                ],
                streaming: false,
            })];
            view.expanded_tools.insert(read_id);
            view.expanded_tools.insert(command_id);
            view.expanded_tools.insert(failed_command_id);
            view.expanded_tools.insert(search_id);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find(("copy-tool-output", 0u64)).is_none(),
                "a successful read shows a pointer, not its contents"
            );
            assert!(
                window.try_find(("copy-tool-output", 1u64)).is_none(),
                "a successful command shows its command, not its stdout"
            );
            assert!(
                window.try_find(("copy-tool-output", 2u64)).is_none(),
                "a failed command keeps its error but not a copyable body"
            );
            assert!(
                window.try_find(("copy-tool-output", 3u64)).is_some(),
                "a web result exists nowhere else and stays visible"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn bodyless_tools_have_no_disclosure_and_failures_start_collapsed(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let read_id = ToolCallId::new();
        let failed_read_id = ToolCallId::new();
        let search_id = ToolCallId::new();
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
                parts: vec![
                    // A successful read with no range has nothing to reveal.
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: read_id,
                        name: "read_file".to_owned(),
                        title: "Read src/lib.rs".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: None,
                        output: Some("fn main() {}".to_owned()),
                        elapsed_ms: Some(4),
                        approval_pending: false,
                    })),
                    // A failed read is the same: the row reports the failure and
                    // the agent decides whether it matters.
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: failed_read_id,
                        name: "read_file".to_owned(),
                        title: "Read missing.rs".to_owned(),
                        status: ToolPartStatus::Failed,
                        detail: None,
                        output: Some("No such file or directory".to_owned()),
                        elapsed_ms: Some(2),
                        approval_pending: false,
                    })),
                    // A successful result that exists nowhere else is shown, but
                    // starts collapsed.
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: search_id,
                        name: "web_search".to_owned(),
                        title: "Web search \"needle\"".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: None,
                        output: Some(r#"{"results":[]}"#.to_owned()),
                        elapsed_ms: Some(3),
                        approval_pending: false,
                    })),
                ],
                streaming: false,
            })];
            view
        });
        cx.update_window(handle.into(), |view, window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            window.render_frame(cx);
            window.click(("tool-header", 0u64), cx);
            window.click(("tool-header", 1u64), cx);
            window.render_frame(cx);
            view.update(cx, |view, _| {
                assert!(
                    !view.expanded_tools.contains(&read_id),
                    "a tool with nothing to reveal has no disclosure to toggle"
                );
                assert!(
                    !view.expanded_tools.contains(&failed_read_id),
                    "a failed tool has nothing to reveal; the agent reports it"
                );
                assert!(
                    !view.expanded_tools.contains(&search_id),
                    "a settled result starts collapsed"
                );
            });
            assert!(window.try_find(("copy-tool-output", 2u64)).is_none());

            // A result with a body can be opened and closed again.
            window.click(("tool-header", 2u64), cx);
            window.render_frame(cx);
            view.update(cx, |view, _| {
                assert!(view.expanded_tools.contains(&search_id))
            });
            assert!(window.try_find(("copy-tool-output", 2u64)).is_some());
            window.click(("tool-header", 2u64), cx);
            window.render_frame(cx);
            view.update(cx, |view, _| {
                assert!(!view.expanded_tools.contains(&search_id))
            });
            assert!(window.try_find(("copy-tool-output", 2u64)).is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn context_events_update_usage_and_only_record_compaction(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let mut inspection = loom_protocol::ContextInspection {
                items: Vec::new(),
                total_tokens: 200,
                included_tokens: 200,
                omitted_tokens: 0,
                budget: loom_protocol::ContextBudget::new(Some(1_000), None, 100).unwrap(),
                compacted: false,
                summary: None,
            };
            let run_id = RunId::new();
            view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected {
                run_id,
                inspection: inspection.clone(),
            });
            assert!(view.timeline.is_empty());
            assert_eq!(
                view.context_inspection.as_ref().unwrap().included_tokens,
                200
            );
            inspection.compacted = true;
            inspection.omitted_tokens = 120;
            inspection.included_tokens = 80;
            view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected {
                run_id,
                inspection: inspection.clone(),
            });
            assert!(
                view.status_banner
                    .as_ref()
                    .is_some_and(|note| note.text.contains("Context compacted")
                        && note.text.contains("lossy excerpts"))
            );
            let count = view.timeline.len();
            inspection.compacted = false;
            view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected {
                run_id,
                inspection,
            });
            assert_eq!(view.timeline.len(), count);
            assert_eq!(
                view.context_inspection.as_ref().unwrap().included_tokens,
                80
            );
            view.reset_projection();
            assert!(view.context_inspection.is_none());
        });
    }

    #[gpui_kit::test]
    fn completing_a_run_stops_the_streaming_cursor(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let run_id = RunId::new();
            view.active_run_id = Some(run_id);
            view.consume_agent_event(&loom_protocol::AgentEvent::AssistantMessageDelta {
                run_id,
                message_id: 1,
                text: "working".to_owned(),
            });
            let streaming = |view: &LoomView| {
                view.timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::Assistant(turn) if turn.streaming))
            };
            assert!(streaming(&view));
            view.consume_agent_event(&loom_protocol::AgentEvent::RunStateChanged {
                run_id,
                state: loom_protocol::AgentRunState::Completed,
            });
            assert!(!streaming(&view));
        });
    }

    #[gpui_kit::test]
    fn agent_event_projection_handles_the_run_lifecycle_and_tool_fallback(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let run_id = RunId::new();
            let call = ToolCall {
                id: ToolCallId::new(),
                name: "write_file".to_owned(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            };
            let approval_interaction_id = loom_core::InteractionId::new();
            let input_interaction_id = loom_core::InteractionId::new();
            let snapshot = loom_protocol::AgentRunSnapshot {
                id: run_id,
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "update the app".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Executing,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            };
            let inspection = loom_protocol::ContextInspection {
                items: Vec::new(),
                total_tokens: 0,
                included_tokens: 0,
                omitted_tokens: 0,
                budget: loom_protocol::ContextBudget {
                    context_window: None,
                    requested_input_tokens: None,
                    reserved_output_tokens: 0,
                    effective_input_tokens: None,
                },
                compacted: false,
                summary: None,
            };
            for event in [
                loom_protocol::AgentEvent::RunStarted {
                    snapshot: snapshot.clone(),
                },
                loom_protocol::AgentEvent::PlanProposed {
                    run_id,
                    plan: loom_protocol::AgentPlan {
                        steps: vec![loom_protocol::AgentPlanStep {
                            id: "edit".to_owned(),
                            description: "Edit the app".to_owned(),
                        }],
                    },
                },
                loom_protocol::AgentEvent::StepStarted {
                    run_id,
                    step_id: loom_core::StepId::new(),
                    index: 0,
                },
                loom_protocol::AgentEvent::StepCompleted {
                    run_id,
                    step_id: loom_core::StepId::new(),
                    index: 0,
                },
                loom_protocol::AgentEvent::ContextInspected { run_id, inspection },
                loom_protocol::AgentEvent::UserMessage {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 1,
                    interaction_id: Some(input_interaction_id),
                    text: "new request".to_owned(),
                },
                loom_protocol::AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "The change ".to_owned(),
                },
                loom_protocol::AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "is ready.".to_owned(),
                },
                loom_protocol::AgentEvent::ToolCallRequested {
                    run_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolApprovalRequired {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 2,
                    interaction_id: approval_interaction_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolPolicyEvaluated {
                    run_id,
                    call: call.clone(),
                    evaluation: loom_core::PolicyEvaluation {
                        action: loom_core::ActionKind::Write,
                        decision: loom_core::PolicyDecision::RequireApproval,
                        reason: "user approval is required".to_owned(),
                    },
                },
                loom_protocol::AgentEvent::ToolCallStarted {
                    run_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolOutputChunk {
                    run_id,
                    tool_call_id: call.id,
                    chunk: "file updated".to_owned(),
                },
                loom_protocol::AgentEvent::ToolCallCompleted {
                    run_id,
                    result: ToolResult::success(&call, "done".to_owned()),
                },
                loom_protocol::AgentEvent::ToolApprovalDecided {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 3,
                    interaction_id: approval_interaction_id,
                    tool_call_id: call.id,
                    decision: loom_protocol::ApprovalDecision::Approved,
                },
                loom_protocol::AgentEvent::NeedsInput {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 4,
                    interaction_id: input_interaction_id,
                    prompt: "Which branch?".to_owned(),
                },
                loom_protocol::AgentEvent::RunUsage {
                    run_id,
                    usage: Default::default(),
                },
                loom_protocol::AgentEvent::RunUsageUpdated {
                    run_id,
                    usage: Default::default(),
                },
                loom_protocol::AgentEvent::RunLimitReached {
                    run_id,
                    status: loom_core::LimitStatus::new(
                        loom_core::SessionLimits::default(),
                        loom_core::UsageSnapshot::default(),
                    ),
                },
                loom_protocol::AgentEvent::RecoveryRequired {
                    run_id,
                    reason: "resume the session".to_owned(),
                },
                loom_protocol::AgentEvent::RunStateChanged {
                    run_id,
                    state: loom_protocol::AgentRunState::Paused,
                },
                loom_protocol::AgentEvent::ProviderError {
                    run_id,
                    error: loom_core::LoomError::new(ErrorCode::Internal, "provider failed", true),
                },
                loom_protocol::AgentEvent::ContextError {
                    run_id,
                    error: loom_core::LoomError::new(ErrorCode::Internal, "context failed", false),
                },
                loom_protocol::AgentEvent::RunCompleted {
                    snapshot: loom_protocol::AgentRunSnapshot {
                        state: loom_protocol::AgentRunState::Completed,
                        summary: Some("Finished the app update".to_owned()),
                        ..snapshot
                    },
                },
            ] {
                view.consume_agent_event(&event);
            }
            assert_eq!(
                view.run_state,
                Some(loom_protocol::AgentRunState::Completed)
            );
            assert!(view.pending_approval.is_none());
            assert_eq!(view.pending_input.as_deref(), Some("Which branch?"));
            assert!(view.timeline.iter().any(|item| matches!(
                item,
                TimelineItem::Assistant(turn) if turn.parts.iter().any(|part| matches!(part, AssistantPart::Text(text) if text == "Finished the app update"))
            )));

            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallRequested {
                run_id,
                call: call.clone(),
            });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallStarted { run_id, call });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolOutputChunk {
                run_id,
                tool_call_id: ToolCallId::new(),
                chunk: "suppressed fallback".to_owned(),
            });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallCompleted {
                run_id,
                result: ToolResult::success(
                    &ToolCall {
                        id: ToolCallId::new(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::Value::Null,
                    },
                    "done".to_owned(),
                ),
            });
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn startup_session_load_restores_snapshot_and_source_lists(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.refresh_models();
            assert!(!view.default_models.is_empty());
            let workspace =
                crate::connection::create_workspace(&view.connection, "Loaded workspace").unwrap();
            let session = crate::connection::create_session_in_workspace(
                &view.connection,
                workspace.id,
                "Loaded session",
            )
            .unwrap();
            let started = view
                .connection
                .request(RequestEnvelope::new(ClientRequest::Run(
                    RunRequest::StartSessionAgentRun {
                        session_id: session.id,
                        task: "startup transcript page".to_owned(),
                        model: ModelId::new("deterministic/demo"),
                        system_instructions: None,
                        repository_instructions: None,
                    },
                )));
            let run_id = match started.result.unwrap() {
                ServerResponse::Run(RunResponse::AgentRunStarted(run)) => run.id,
                response => panic!("unexpected run start response: {response:?}"),
            };
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view.refresh_sessions().unwrap();
            assert_eq!(view.sessions.len(), 1);
            view.load_session(session.clone());
            assert_eq!(view.active_session.id, session.id);
            assert_eq!(view.active_session.name, "Loaded session");
            assert!(view.after_sequence.is_some());
            assert!(view.event_stream_epoch.is_some());
            assert_eq!(view.active_run_id, Some(run_id));
            assert_eq!(view.transcript_before_ordinal, Some(0));
            assert!(view.timeline.iter().any(
                |item| matches!(item, TimelineItem::User(task) if task == "startup transcript page")
            ));
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn async_session_load_falls_back_to_run_projection_and_ignores_stale_responses(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session_id = view.active_session.id;
            let run = loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id,
                task: "recover the transcript".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Completed,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: Some(Timestamp::from_unix_millis(2)),
                summary: Some("Recovered run".to_owned()),
                evidence: Vec::new(),
            };
            let projection = loom_protocol::AgentRunSnapshotProjection {
                run: run.clone(),
                plan: Vec::new(),
                messages: vec![loom_model::ModelMessage::new(
                    loom_model::MessageRole::User,
                    "recover the transcript",
                )],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
                message_timeline_ordinals: vec![0],
            };
            let snapshot = loom_protocol::AgentSessionSnapshotProjection {
                session: view.active_session.clone(),
                active_run: Some(projection),
                latest_sequence: loom_core::EventSequence::new(7),
                approval_policy: Default::default(),
                auto_approve_actions: false,
            };
            view.finish_async_session_load(
                session_id,
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    loom_protocol::ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)),
                ),
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    loom_protocol::ServerResponse::Events(EventsResponse::SessionEvents{
                        events: Vec::new(),
                        stream_epoch: None,
                    }),
                ),
                cx,
            );
            assert_eq!(view.active_run_id, Some(run.id));
            assert!(!view.auto_approve_actions);
            assert!(view.timeline.iter().any(|item| matches!(
                item,
                TimelineItem::Assistant(turn) if turn.parts.iter().any(|part| matches!(part, AssistantPart::Text(text) if text == "Recovered run"))
            )));

            let old_timeline_len = view.timeline.len();
            view.finish_async_session_load(
                AgentSessionId::new(),
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::Internal, "stale", false),
                ),
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::Internal, "stale", false),
                ),
                cx,
            );
            assert_eq!(view.timeline.len(), old_timeline_len);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn composer_commands_and_failed_run_responses_are_projected(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.session_node_ids
                .insert(view.active_session.id, view.default_backend_node_id.clone());
            view.submit_composer(cx);
            view.run_slash_command("/help", cx);
            view.run_slash_command("/unknown", cx);
            view.run_slash_command("/repo", cx);
            assert!(view.source_dialog.is_some());
            view.source_dialog = None;
            view.run_slash_command("/review", cx);
            assert!(view.review.open);
            view.approve_pending_action(cx);
            view.reject_pending_action(cx);

            view.model = ModelId::new("worker/uncached-model");
            view.model_catalog_node_id = None;
            view.send_message("uncached model task".to_owned(), cx);
            assert!(view.status_banner.as_ref().is_some_and(|note| {
                note.tone == SystemTone::Error
                    && note
                        .heading
                        .as_deref()
                        .is_some_and(|heading| heading.starts_with("start run"))
                    && note.text.contains("has not been refreshed")
            }));

            view.model = ModelId::new("deterministic/demo");
            view.send_message("try a task".to_owned(), cx);
            assert!(!view.sending_message);
            assert!(
                !view
                    .timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::User(_)))
            );
            view.sending_message = true;
            view.finish_send_response(
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::ProviderUnavailable, "offline", true),
                ),
                cx,
            );
            assert!(!view.sending_message);
            view.approval_request_in_flight = true;
            view.finish_approval_response(
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::InvalidState, "approval expired", false),
                ),
                cx,
            );
            assert!(!view.approval_request_in_flight);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn server_event_projection_updates_session_and_ignores_service_streams(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            let snapshot = view.active_session.clone();
            let session_id = snapshot.id;
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionCreated {
                snapshot: snapshot.clone(),
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionForked {
                source_session_id: session_id,
                snapshot: snapshot.clone(),
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionStateChanged {
                previous: loom_core::AgentSessionState::Idle,
                current: loom_core::AgentSessionState::Executing,
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionRenamed {
                session_id,
                name: "Renamed from event".to_owned(),
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionArchived { session_id });
            view.consume_event(&loom_protocol::ServerEvent::SessionFilesystemChanged {
                change: SessionFilesystemChange {
                    sequence: loom_core::EventSequence::new(1),
                    session_id,
                    path: "README.md".to_owned(),
                    kind: WorkspaceChangeKind::Modified,
                    revision: None,
                },
            });
            view.consume_event(&loom_protocol::ServerEvent::Terminal {
                event: loom_protocol::TerminalEventRecord {
                    sequence: loom_core::EventSequence::new(1),
                    terminal_id: loom_core::TerminalId::new(),
                    event: loom_protocol::TerminalEvent::StateChanged {
                        status: loom_protocol::TerminalStatus::Exited,
                    },
                },
            });
            view.consume_event(&loom_protocol::ServerEvent::Task {
                event: loom_protocol::TaskEventRecord {
                    sequence: loom_core::EventSequence::new(1),
                    task_id: loom_core::TaskId::new(),
                    event: loom_protocol::TaskEvent::StateChanged {
                        status: loom_protocol::TaskStatus::Completed,
                    },
                },
            });
            view.consume_event(&loom_protocol::ServerEvent::ProviderHealthChanged {
                provider_id: loom_model::ProviderId::new("test-provider"),
                health: ProviderHealth::default(),
            });
            assert_eq!(view.session_state, loom_core::AgentSessionState::Archived);
            assert!(view.status_banner.is_some());
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn settings_and_provider_dialogs_render_configured_entries(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.settings_open = true;
            view.browser_startup_error = Some("Workspace setup failed".to_owned());
            view.worker_nodes.push(WorkerNodeEntry {
                id: 1,
                status: WorkerNodeStatus {
                    node_id: "remote-worker".to_owned(),
                    name: "Remote worker".to_owned(),
                    online: false,
                    capabilities: CapabilitySet::default(),
                    resources: WorkerNodeResources {
                        cpu_count: 0,
                        cpu_usage_percent: None,
                        memory_usage_percent: None,
                        memory_total_bytes: None,
                        memory_available_bytes: None,
                        disk_total_bytes: None,
                        disk_available_bytes: None,
                    },
                },
                is_local: false,
                url: Some("wss://example.test/ws".to_owned()),
                connection: None,
                connection_state: WorkerConnectionState::Failed,
                connection_detail: Some("Connection timed out".to_owned()),
                severe_load_streak: 0,
            });
            for (id, state, online) in [
                (2, WorkerConnectionState::Disconnected, false),
                (3, WorkerConnectionState::Connecting, false),
                (4, WorkerConnectionState::Connected, true),
                (5, WorkerConnectionState::Connected, false),
            ] {
                view.worker_nodes.push(WorkerNodeEntry {
                    id,
                    status: WorkerNodeStatus {
                        node_id: format!("worker-{id}"),
                        name: format!("Worker {id}"),
                        online,
                        capabilities: CapabilitySet::default(),
                        resources: WorkerNodeResources {
                            cpu_count: 2,
                            cpu_usage_percent: Some(50),
                            memory_usage_percent: Some(75),
                            memory_total_bytes: Some(8 * 1024 * 1024),
                            memory_available_bytes: Some(2 * 1024 * 1024),
                            disk_total_bytes: None,
                            disk_available_bytes: None,
                        },
                    },
                    is_local: false,
                    url: Some(format!("wss://worker-{id}.example.test/ws")),
                    connection: None,
                    connection_state: state,
                    connection_detail: None,
                    severe_load_streak: 0,
                });
            }
        });

        render_scenario(cx, |view| {
            let local_provider_id = loom_model::ProviderId::new("company-gateway");
            let github_provider_id = loom_model::ProviderId::new("github-copilot");
            view.settings_open = true;
            view.settings_section = SettingsSection::Providers;
            view.github_connected = true;
            view.providers = vec![
                ProviderSummary {
                    id: local_provider_id.clone(),
                    kind: ProviderKind::OpenAiCompatible,
                    display_name: "Company gateway".to_owned(),
                    models: vec![ModelDescriptor {
                        id: ModelId::new("gateway/model"),
                        provider: local_provider_id,
                        display_name: "Gateway model".to_owned(),
                        context_window: Some(32_000),
                        max_input_tokens: None,
                        max_output_tokens: None,
                        capabilities: ModelCapabilities::default(),
                    }],
                    credential_id: Some("gateway-key".to_owned()),
                    api_key_configurable: true,
                    health: ProviderHealth::default(),
                },
                ProviderSummary {
                    id: github_provider_id,
                    kind: ProviderKind::GitHubCopilot,
                    display_name: "GitHub Copilot".to_owned(),
                    models: Vec::new(),
                    credential_id: None,
                    api_key_configurable: false,
                    health: ProviderHealth::default(),
                },
                ProviderSummary {
                    id: loom_model::ProviderId::new("empty-ollama"),
                    kind: ProviderKind::Ollama,
                    display_name: "Ollama".to_owned(),
                    models: Vec::new(),
                    credential_id: None,
                    api_key_configurable: false,
                    health: ProviderHealth::default(),
                },
            ];
        });
    }

    #[gpui_kit::test]
    fn github_login_states_and_phone_session_drawer_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for state in [
            GitHubLoginState::Starting,
            GitHubLoginState::Awaiting {
                verification_uri: "https://github.com/login/device".to_owned(),
                user_code: "ABCD-EFGH".to_owned(),
                expires_in: 600,
            },
            GitHubLoginState::Success,
            GitHubLoginState::Error("Unable to connect".to_owned()),
        ] {
            render_scenario(cx, |view| view.github_login = Some(state));
        }
        render_scenario_at(cx, size(px(390.), px(844.)), |view| {
            view.session_drawer_open = true;
            view.sessions = vec![view.active_session.clone()];
        });
    }

    #[gpui_kit::test]
    fn phone_drawer_and_review_sidebar_controls_toggle_panels(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("open-session-drawer", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("mobile-session-drawer").visible());
            assert!(
                window
                    .within("mobile-session-drawer")
                    .find(("session-tree-root", 0usize))
                    .visible()
            );
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.session_drawer_open = false;
                    cx.notify();
                });
            window.render_frame(cx);
            assert!(window.try_find("mobile-session-drawer").is_none());
            window.click("toggle-review-sidebar", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("close-inspector").visible());
            window.click("close-inspector", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("close-inspector").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn phone_composer_and_settings_render_at_mobile_width(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("send-message").visible());
            assert!(window.find("open-command-palette").visible());
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.settings_open = true;
                    cx.notify();
                });
            window.render_frame(cx);
            for index in 0..SETTINGS_SECTIONS.len() {
                window
                    .within("settings-dialog")
                    .click(("settings-section", index), cx);
                window.render_frame(cx);
                assert!(
                    window
                        .within("settings-dialog")
                        .find(("settings-section", index))
                        .visible()
                );
                assert!(
                    window
                        .within("settings-dialog")
                        .find("settings-content")
                        .visible()
                );
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn session_creation_uses_worker_model_catalog_and_selects_the_created_session(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            let workspace =
                crate::connection::create_workspace(&view.connection, "Session creation").unwrap();
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.create_session_on_node_with_source(
                        view.default_backend_node_id.clone(),
                        "Created session".to_owned(),
                        None,
                        cx,
                    );
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).sessions.len() == 1)
        })
        .await;
    }

    #[gpui_kit::test]
    async fn asynchronous_model_refresh_updates_the_active_worker_catalog(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.session_node_ids
                .insert(view.active_session.id, view.default_backend_node_id.clone());
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.refresh_models_for_node_async(view.default_backend_node_id.clone(), cx);
                    view.refresh_models_for_node_async("missing-node".to_owned(), cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.model_catalog_node_id.as_deref() == Some("test-node")
                    && !view.models.is_empty()
                    && view.model_refreshes_in_flight.is_empty()
            })
        })
        .await;
    }

    #[gpui_kit::test]
    async fn session_creation_attaches_a_local_source_before_selecting_it(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let source = std::env::temp_dir().join(format!("loom-ui-source-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "local source").unwrap();
        let source_path = source.to_string_lossy().to_string();
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            let workspace =
                crate::connection::create_workspace(&view.connection, "Local source").unwrap();
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.create_session_on_node_with_source(
                        view.default_backend_node_id.clone(),
                        "Source session".to_owned(),
                        Some(super::SessionCreationSource::LocalDirectory(
                            source_path.clone(),
                        )),
                        cx,
                    );
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).sessions.len() == 1)
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.refresh_review(cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.review.repositories_loaded && view.session_directories.len() == 1
            })
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                let path = format!("{}/README.md", view.session_directories[0].path);
                view.open_review_file(path, cx);
            });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                view.read(cx)
                    .review
                    .selected_file
                    .as_ref()
                    .is_some_and(|file| file.content == "local source")
            })
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.add_source_to_active_session(
                        super::SessionCreationSource::LocalDirectory(source_path.clone()),
                        cx,
                    );
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_directories.len() == 2)
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                assert!(
                    view.session_directories[0]
                        .source
                        .contains("loom-ui-source-")
                );
                view.detach_session_directory(view.session_directories[0].path.clone(), cx);
            });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_directories.len() == 1)
        })
        .await;
        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                view.detach_session_directory(view.session_directories[0].path.clone(), cx);
            });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_directories.is_empty())
        })
        .await;
        std::fs::remove_dir_all(source).unwrap();
    }

    #[gpui_kit::test]
    async fn repository_review_loads_git_status_diff_and_detaches_the_repository(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let source = std::env::temp_dir().join(format!("loom-ui-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "before\n").unwrap();
        for arguments in [
            vec!["init", "-q"],
            vec!["config", "user.email", "loom@example.test"],
            vec!["config", "user.name", "Loom Test"],
            vec!["add", "README.md"],
            vec!["commit", "-qm", "initial"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(arguments)
                    .current_dir(&source)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let source_path = source.to_string_lossy().to_string();
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            let workspace =
                crate::connection::create_workspace(&view.connection, "Repository review").unwrap();
            let session = crate::connection::create_session_in_workspace(
                &view.connection,
                workspace.id,
                "Review session",
            )
            .unwrap();
            let repository = crate::connection::attach_session_repository(
                &view.connection,
                session.id,
                &source_path,
                "repo",
            )
            .unwrap();
            let edit = view
                .connection
                .request(RequestEnvelope::new(ClientRequest::Filesystem(
                    FilesystemRequest::ApplySessionFilesystemEdit {
                        session_id: session.id,
                        edit: loom_protocol::WorkspaceEdit {
                            path: "repo/README.md".to_owned(),
                            old_text: "before".to_owned(),
                            new_text: "after".to_owned(),
                            expected_revision: None,
                        },
                    },
                )));
            assert!(matches!(
                edit.result,
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::WorkspaceEditApplied(_)
                ))
            ));
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view.active_session = session.clone();
            view.sessions.push(session.clone());
            view.session_node_ids
                .insert(session.id, view.default_backend_node_id.clone());
            view.session_repositories.push(repository.clone());
            view.selected_repository_id = Some(repository.id);
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    let repository_id = view.selected_repository_id.unwrap();
                    view.select_session_repository(repository_id, cx);
                    view.open_review_diff("README.md".to_owned(), false, cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.review.vcs.is_some()
                    && view
                        .review
                        .selected_diff
                        .as_ref()
                        .is_some_and(|diff| !diff.hunks.is_empty())
            })
        })
        .await;
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.jump_review_hunk(true, cx);
                    view.jump_review_hunk(false, cx);
                    view.detach_session_repository(view.selected_repository_id.unwrap(), cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_repositories.is_empty())
        })
        .await;
        std::fs::remove_dir_all(source).unwrap();
    }

    #[gpui_kit::test]
    async fn github_source_selection_reports_unconfigured_provider_and_requires_a_repository(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                crate::connection::negotiate(&view.connection).unwrap();
                view.session_node_ids
                    .insert(view.active_session.id, view.default_backend_node_id.clone());
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<gpui_kit::component::Root>()
                .unwrap()
                .unwrap()
                .update(cx, |root, cx| {
                    root.view()
                        .clone()
                        .downcast::<LoomView>()
                        .unwrap()
                        .update(cx, |view, cx| {
                            view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                            view.choose_source(SessionSourceChoice::GitHub, cx);
                        });
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<gpui_kit::component::Root>()
                .flatten()
                .is_some_and(|root| {
                    root.read(cx)
                        .view()
                        .clone()
                        .downcast::<LoomView>()
                        .ok()
                        .is_some_and(|view| {
                            view.read(cx).source_dialog.as_ref().is_some_and(|dialog| {
                                !dialog.repositories_loading && dialog.error.is_some()
                            })
                        })
                })
        })
        .await;
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<gpui_kit::component::Root>()
                .unwrap()
                .unwrap()
                .update(cx, |root, cx| {
                    root.view()
                        .clone()
                        .downcast::<LoomView>()
                        .unwrap()
                        .update(cx, |view, cx| {
                            view.confirm_source_dialog(cx);
                            assert!(view.source_dialog.is_some());
                            assert!(
                                view.status_banner
                                    .as_ref()
                                    .is_some_and(|note| note.text == "Choose a GitHub repository")
                            );
                        });
                });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn rename_and_source_dialogs_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.rename_dialog = Some(RenameDialogState {
                session: view.active_session.clone(),
                input: "Renamed session".to_owned(),
                is_project: true,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::Empty,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::AddToSession,
                choice: SessionSourceChoice::LocalDirectory,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: Some("directory does not exist".to_owned()),
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: false,
                filter_subscription: None,
                repositories: vec![GitHubRepository {
                    full_name: "owner/project".to_owned(),
                    description: Some("example repository".to_owned()),
                    clone_url: "https://github.com/owner/project.git".to_owned(),
                    private: false,
                    default_branch: "main".to_owned(),
                }],
                selected_repository: Some("owner/project".to_owned()),
                repositories_loading: false,
                error: None,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: true,
                error: None,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::AddToSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: false,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: Some("GitHub authentication is required".to_owned()),
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
        });
    }

    #[gpui_kit::test]
    fn review_panel_renders_workspace_and_git_changes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.changes = vec![SessionFilesystemChange {
                sequence: loom_core::EventSequence::new(1),
                session_id: view.active_session.id,
                path: "src/new.rs".to_owned(),
                kind: WorkspaceChangeKind::Created,
                revision: Some("revision-1".to_owned()),
            }];
            view.review.vcs = Some(GitRepositoryStatus {
                root: "/workspace".to_owned(),
                branch: Some("main".to_owned()),
                head: Some("abc123".to_owned()),
                files: vec![GitFileStatus {
                    path: "src/lib.rs".to_owned(),
                    original_path: None,
                    index: GitFileStatusKind::Modified,
                    worktree: GitFileStatusKind::Modified,
                    conflicted: false,
                    index_additions: 1,
                    index_deletions: 0,
                    worktree_additions: 2,
                    worktree_deletions: 1,
                }],
                conflicts: Vec::new(),
                clean: false,
                captured_at: Timestamp::from_unix_millis(0),
            });
            view.review.selected_path = Some("src/lib.rs".to_owned());
            view.review.selected_diff = Some(GitDiff {
                path: Some("src/lib.rs".to_owned()),
                staged: false,
                patch: String::new(),
                binary: false,
                hunks: vec![GitDiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 2,
                    lines: vec![GitDiffLine {
                        kind: GitDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        content: "new line".to_owned(),
                    }],
                }],
                truncated: false,
            });
            view.review.rows = vec![
                ReviewRow::Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 2,
                },
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "new line".to_owned(),
                }),
            ];
            view.review.hunk_rows = vec![0];
        });
    }

    #[gpui_kit::test]
    fn review_panel_renders_loading_file_and_binary_diff_states(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = false;
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.selected_path = Some("README.md".to_owned());
            view.review.selected_file = Some(SessionFilesystemFile {
                session_id: view.active_session.id,
                path: "README.md".to_owned(),
                content: "Workspace file contents".to_owned(),
                revision: "revision-2".to_owned(),
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.selected_path = Some("assets/image.png".to_owned());
            view.review.selected_diff = Some(GitDiff {
                path: Some("assets/image.png".to_owned()),
                staged: false,
                patch: String::new(),
                binary: true,
                hunks: Vec::new(),
                truncated: false,
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.selected_path = Some("src/large.rs".to_owned());
            view.review.selected_diff = Some(GitDiff {
                path: Some("src/large.rs".to_owned()),
                staged: false,
                patch: String::new(),
                binary: false,
                hunks: Vec::new(),
                truncated: true,
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.session_repositories = vec![
                SessionRepository {
                    id: loom_core::RepositoryId::new(),
                    source: "https://github.com/owner/first".to_owned(),
                    path: "/workspace/first".to_owned(),
                    revision: None,
                    attached_at: Timestamp::from_unix_millis(1),
                },
                SessionRepository {
                    id: loom_core::RepositoryId::new(),
                    source: "https://github.com/owner/second/".to_owned(),
                    path: "/workspace/second".to_owned(),
                    revision: None,
                    attached_at: Timestamp::from_unix_millis(2),
                },
            ];
            view.selected_repository_id = view.session_repositories.first().map(|repo| repo.id);
            view.review.changes = vec![SessionFilesystemChange {
                sequence: loom_core::EventSequence::new(2),
                session_id: view.active_session.id,
                path: "notes/todo.md".to_owned(),
                kind: WorkspaceChangeKind::Modified,
                revision: None,
            }];
        });
    }

    #[gpui_kit::test]
    fn inspector_renders_agent_context_and_files_tabs(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.tab = InspectorTab::Agent;
            view.active_run = Some(loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "review the right pane".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Executing,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: None,
                summary: None,
                evidence: vec![loom_core::EvidenceLink {
                    label: "pr".to_owned(),
                    uri: "https://example.com/pr/1".to_owned(),
                }],
            });
            view.review.usage.session = Some(UsageSnapshot {
                input_tokens: 1_200,
                output_tokens: 400,
                cached_input_tokens: 100,
                tool_calls: 3,
                cost_micros: 12_500,
                elapsed_ms: 65_000,
            });
            view.review.usage.session_provider = Some(ProviderUsageSummary {
                requests: 2,
                cost_micros: 12_500,
                ..ProviderUsageSummary::default()
            });
            view.timeline = vec![
                TimelineItem::Plan {
                    steps: vec!["Inspect".to_owned(), "Edit".to_owned()],
                    completed: BTreeSet::from([0]),
                    active: Some(1),
                },
                TimelineItem::Assistant(AssistantTurn {
                    parts: vec![AssistantPart::Tool(Box::new(ToolPart {
                        id: ToolCallId::new(),
                        name: "read_file".to_owned(),
                        title: "Read src/lib.rs".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: Some("src/lib.rs".to_owned()),
                        output: Some("contents".to_owned()),
                        elapsed_ms: Some(12),
                        approval_pending: false,
                    }))],
                    streaming: false,
                }),
            ];
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.tab = InspectorTab::Context;
            view.context_inspection = Some(ContextInspection {
                items: vec![
                    ContextItem {
                        kind: ContextItemKind::Task,
                        label: "Task".to_owned(),
                        estimated_tokens: 10,
                        included: true,
                        omission_reason: None,
                    },
                    ContextItem {
                        kind: ContextItemKind::Conversation,
                        label: "Conversation".to_owned(),
                        estimated_tokens: 90,
                        included: false,
                        omission_reason: Some("over budget".to_owned()),
                    },
                ],
                total_tokens: 100,
                included_tokens: 120,
                omitted_tokens: 90,
                budget: ContextBudget::new(Some(100), None, 50).unwrap(),
                compacted: true,
                summary: Some(ContextSummary {
                    text: "Earlier turns were summarised.".to_owned(),
                    source_message_count: 4,
                    projection_version: 1,
                    source_digest: "digest".to_owned(),
                    created_at: Timestamp::from_unix_millis(3),
                }),
            });
            view.review.usage.run = Some(UsageSnapshot {
                input_tokens: 30,
                output_tokens: 10,
                ..UsageSnapshot::default()
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.tab = InspectorTab::Files;
            view.review.files.loaded = true;
            view.review.files.entries = vec![
                WorkspaceEntry {
                    path: "src".to_owned(),
                    kind: WorkspaceEntryKind::Directory,
                    size: 0,
                    modified_at: None,
                    revision: "dir".to_owned(),
                },
                WorkspaceEntry {
                    path: "src/main.rs".to_owned(),
                    kind: WorkspaceEntryKind::File,
                    size: 42,
                    modified_at: None,
                    revision: "rev".to_owned(),
                },
            ];
            view.review.files.selected_path = Some("src/main.rs".to_owned());
            view.review.files.selected_file = Some(SessionFilesystemFile {
                session_id: view.active_session.id,
                path: "src/main.rs".to_owned(),
                content: "fn main() {}\n".to_owned(),
                revision: "rev".to_owned(),
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.wrap_lines = true;
            view.review.selected_path = Some("README.md".to_owned());
            view.review.selected_file = Some(SessionFilesystemFile {
                session_id: view.active_session.id,
                path: "README.md".to_owned(),
                content: "line one\nline two".to_owned(),
                revision: "rev".to_owned(),
            });
        });
    }

    #[gpui_kit::test]
    fn run_usage_events_update_inspector_usage(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let run_id = RunId::new();
            view.consume_agent_event(&loom_protocol::AgentEvent::RunUsage {
                run_id,
                usage: loom_model::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cached_input_tokens: 10,
                },
            });
            let run = view.review.usage.run.as_ref().unwrap();
            assert_eq!(run.input_tokens, 100);
            assert_eq!(run.output_tokens, 50);
            view.consume_agent_event(&loom_protocol::AgentEvent::RunUsageUpdated {
                run_id,
                usage: UsageSnapshot {
                    input_tokens: 200,
                    output_tokens: 60,
                    cached_input_tokens: 10,
                    tool_calls: 2,
                    cost_micros: 5_000,
                    elapsed_ms: 1_000,
                },
            });
            let run = view.review.usage.run.as_ref().unwrap();
            assert_eq!(run.tool_calls, 2);
            assert_eq!(run.cost_micros, 5_000);
        });
    }

    #[gpui_kit::test]
    fn transcript_renders_all_message_and_activity_variants(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let call = ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "write_file".to_owned(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        };
        let activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id: RunId::new(),
            timeline_ordinal: 0,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::File,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(0),
            completed_at: None,
            elapsed_ms: Some(1500),
            data: AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::Write,
                path: Some("src/lib.rs".to_owned()),
                result: Some(ToolResult::success(&call, "updated file".to_owned())),
            },
        };
        render_scenario(cx, |view| {
            view.expanded_tools.insert(call.id);
            view.activity_records.insert(activity.id, activity);
            view.timeline = vec![
                TimelineItem::User("Please update the file".to_owned()),
                TimelineItem::Assistant(AssistantTurn {
                    parts: vec![
                        AssistantPart::Reasoning("The file needs a small edit.".to_owned()),
                        AssistantPart::Text("# Done\nThe file is updated.".to_owned()),
                        AssistantPart::Tool(Box::new(ToolPart {
                            id: call.id,
                            name: "write_file".to_owned(),
                            title: "Edit src/lib.rs".to_owned(),
                            status: ToolPartStatus::Completed,
                            detail: Some("src/lib.rs".to_owned()),
                            output: Some("updated file".to_owned()),
                            elapsed_ms: Some(1500),
                            approval_pending: false,
                        })),
                        AssistantPart::Evidence(vec![EvidenceText {
                            label: "src/lib.rs".to_owned(),
                            uri: "https://example.com/diff".to_owned(),
                        }]),
                    ],
                    streaming: true,
                }),
                TimelineItem::Plan {
                    steps: vec!["Inspect".to_owned(), "Edit".to_owned()],
                    completed: BTreeSet::from([0]),
                    active: Some(1),
                },
                TimelineItem::System(SystemNote::status("Working".to_owned())),
                TimelineItem::System(SystemNote {
                    tone: SystemTone::Error,
                    heading: Some("save · persistence".to_owned()),
                    text: "write failed".to_owned(),
                    retryable: true,
                }),
                TimelineItem::System(SystemNote {
                    tone: SystemTone::Input,
                    heading: Some("Agent needs input".to_owned()),
                    text: "Which branch should I use?".to_owned(),
                    retryable: false,
                }),
            ];
            view.pending_input = Some("Which branch should I use?".to_owned());
            view.pending_approval = Some(call);
            view.model = ModelId::new("deterministic/demo");
        });
    }
}

#[cfg(test)]
mod responsive_layout_tests {
    use super::{
        COMPACT_REVIEW_WIDTH, COMPACT_SIDEBAR_WIDTH, FULL_REVIEW_WIDTH, FULL_SIDEBAR_WIDTH,
        PHONE_SIDEBAR_WIDTH, responsive_layout, review_panel_is_visible,
    };
    use gpui_kit::px;

    #[test]
    fn compact_windows_use_narrower_navigation_panels() {
        let layout = responsive_layout(px(959.));
        assert_eq!(layout.sidebar_width, COMPACT_SIDEBAR_WIDTH);
        assert_eq!(layout.review_width, COMPACT_REVIEW_WIDTH);
    }

    #[test]
    fn wide_windows_keep_full_navigation_panels() {
        let layout = responsive_layout(px(960.));
        assert!(!layout.phone);
        assert_eq!(layout.sidebar_width, FULL_SIDEBAR_WIDTH);
        assert_eq!(layout.review_width, px(430.));
        let wide_layout = responsive_layout(px(1400.));
        assert_eq!(wide_layout.review_width, FULL_REVIEW_WIDTH);
    }

    #[test]
    fn phone_windows_show_single_column_and_full_width_review() {
        let layout = responsive_layout(px(390.));
        assert!(layout.phone);
        assert_eq!(layout.sidebar_width, PHONE_SIDEBAR_WIDTH);
        assert_eq!(layout.review_width, px(390.));
    }

    #[test]
    fn very_narrow_phones_keep_the_session_drawer_in_view() {
        let layout = responsive_layout(px(280.));
        assert!(layout.phone);
        assert_eq!(layout.sidebar_width, px(280.));
        assert_eq!(layout.review_width, px(280.));
    }

    #[test]
    fn review_panel_visibility_is_a_pure_layout_decision() {
        let desktop = responsive_layout(px(1280.));
        assert!(review_panel_is_visible(desktop, true, 1, false, false));
        assert!(!review_panel_is_visible(desktop, false, 1, false, false));
        assert!(!review_panel_is_visible(desktop, true, 0, false, false));
        for modal_open in 0..2 {
            let mut blockers = [false; 2];
            blockers[modal_open] = true;
            assert!(!review_panel_is_visible(
                desktop,
                true,
                1,
                blockers[0],
                blockers[1]
            ));
        }
        assert!(!review_panel_is_visible(
            responsive_layout(px(390.)),
            true,
            1,
            false,
            false
        ));
    }
}

#[cfg(test)]
mod worker_node_tests {
    use super::{
        ACTIVE_BACKEND_NODE_ENTRY_ID, SessionNodeIndicatorState, SessionSourceChoice,
        SessionSourceDialogPurpose, SessionTreeNode, WorkerConnectionStage, WorkerConnectionState,
        WorkerNodeEntry, adjusted_cpu_pulse_threshold, adjusted_project_agent_concurrency,
        assigned_node_id, connection_placeholder, filter_session_tree, format_percentage,
        format_session_resource_percentages, format_worker_node_resources, initial_worker_nodes,
        local_source_available, mark_worker_connection_failed, merge_node_sessions,
        next_severe_load_streak, order_session_nodes, project_child_control_actions,
        project_session_list_projection, project_session_list_projection_for_projects,
        project_snapshot_has_unloaded_agent_sessions, remove_worker_node_entry,
        safe_worker_url_label, session_id_for_request, session_list_projection,
        session_node_indicator_state, session_node_pulse, session_owner_status,
        session_status_pill, session_tree_descendant_count, source_choice_is_allowed,
        source_dialog_initial_state, transition_worker_connection_to_connecting,
        update_worker_node_status, validate_model_for_node, worker_connection_failure_detail,
        worker_node_display_name, worker_node_name_for_id, worker_url_embeds_credential,
    };
    use loom_core::{
        AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, EventSequence,
        RunId, Timestamp, WorkspaceId,
    };
    use loom_core::{ErrorCode, LoomError};
    use loom_model::ModelId;
    use loom_protocol::{
        ClientRequest, EventsRequest, FilesystemRequest, ProjectChildControlAction,
        RepositoryRequest, RunRequest, SessionRequest, TaskRequest, TerminalRequest, UsageRequest,
        WorkerNodeResources, WorkerNodeStatus, WorkspaceRequest,
    };
    use std::collections::BTreeMap;

    fn node(id: u64, is_local: bool) -> WorkerNodeEntry {
        WorkerNodeEntry {
            id,
            status: WorkerNodeStatus {
                node_id: format!("node-{id}"),
                name: format!("Node {id}"),
                online: true,
                capabilities: CapabilitySet::default(),
                resources: WorkerNodeResources {
                    cpu_count: 0,
                    cpu_usage_percent: None,
                    memory_usage_percent: None,
                    memory_total_bytes: None,
                    memory_available_bytes: None,
                    disk_total_bytes: None,
                    disk_available_bytes: None,
                },
            },
            is_local,
            url: (!is_local).then(|| format!("ws://worker-{id}/ws")),
            connection: None,
            connection_state: if is_local {
                WorkerConnectionState::Connected
            } else {
                WorkerConnectionState::Disconnected
            },
            connection_detail: None,
            severe_load_streak: 0,
        }
    }

    #[test]
    fn connection_error_details_are_actionable_and_do_not_echo_tokens() {
        let invalid_url = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::invalid_request("invalid url with secret-token"),
            Some("secret-token"),
        );
        assert!(invalid_url.contains("Invalid worker URL"));
        assert!(!invalid_url.contains("secret-token"));

        let refused = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::new(ErrorCode::Internal, "connection refused", true),
            None,
        );
        assert!(refused.contains("server is running"));

        let timeout = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::new(ErrorCode::DeadlineExceeded, "timeout", true),
            None,
        );
        assert!(timeout.contains("timed out"));

        let authentication = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::new(ErrorCode::AuthenticationFailed, "denied", false),
            None,
        );
        assert!(authentication.contains("access token"));

        let negotiation = worker_connection_failure_detail(
            WorkerConnectionStage::Negotiation,
            &LoomError::new(ErrorCode::UnsupportedProtocol, "mismatch", false),
            None,
        );
        assert!(negotiation.contains("protocol negotiation failed"));

        let status = worker_connection_failure_detail(
            WorkerConnectionStage::Status,
            &LoomError::new(ErrorCode::Internal, "bad status", false),
            None,
        );
        assert!(status.contains("status request failed"));

        let credential = worker_connection_failure_detail(
            WorkerConnectionStage::CredentialSave,
            &LoomError::new(
                ErrorCode::Persistence,
                "could not persist secret%2Fvalue",
                false,
            ),
            Some("secret/value"),
        );
        assert!(credential.contains("could not save reconnect credentials"));
        assert!(!credential.contains("secret/value"));
        assert!(!credential.contains("secret%2Fvalue"));

        let credential_read = worker_connection_failure_detail(
            WorkerConnectionStage::CredentialRead,
            &LoomError::new(ErrorCode::AuthenticationRequired, "missing", false),
            None,
        );
        assert!(credential_read.contains("OS credential store"));

        let bootstrap_save = worker_connection_failure_detail(
            WorkerConnectionStage::BootstrapSave,
            &LoomError::new(ErrorCode::Persistence, "failed with secret", false),
            Some("secret"),
        );
        assert!(bootstrap_save.contains("browser could not save"));
        assert!(!bootstrap_save.contains("secret"));

        let input = worker_connection_failure_detail(
            WorkerConnectionStage::InputValidation,
            &LoomError::invalid_request("empty input"),
            None,
        );
        assert!(input.contains("URL followed by its access token"));
        let cancelled = worker_connection_failure_detail(
            WorkerConnectionStage::Negotiation,
            &LoomError::new(ErrorCode::RequestCancelled, "closed", false),
            None,
        );
        assert!(cancelled.contains("before negotiation completed"));
        let cancelled_status = worker_connection_failure_detail(
            WorkerConnectionStage::Status,
            &LoomError::new(ErrorCode::RequestCancelled, "closed", false),
            None,
        );
        assert!(cancelled_status.contains("before returning status"));
        let token = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::invalid_request("invalid bearer token"),
            None,
        );
        assert!(token.contains("unsupported characters"));
        let timeout_text = worker_connection_failure_detail(
            WorkerConnectionStage::Negotiation,
            &LoomError::new(ErrorCode::Internal, "gateway timeout", false),
            None,
        );
        assert!(timeout_text.contains("timed out"));
        #[cfg(target_family = "wasm")]
        let bootstrap_stage = worker_connection_failure_detail(
            WorkerConnectionStage::Bootstrap,
            &LoomError::new(ErrorCode::Internal, "failed with secret", false),
            Some("secret"),
        );
        #[cfg(target_family = "wasm")]
        {
            assert!(bootstrap_stage.contains("could not open its workspace"));
            assert!(!bootstrap_stage.contains("secret"));
        }
    }

    #[test]
    fn duplicate_connection_attempts_are_blocked_and_url_labels_hide_credentials() {
        let mut state = WorkerConnectionState::Connecting;
        assert!(transition_worker_connection_to_connecting(&mut state, false).is_err());
        assert_eq!(state, WorkerConnectionState::Connecting);

        state = WorkerConnectionState::Failed;
        assert!(transition_worker_connection_to_connecting(&mut state, false).is_ok());
        assert_eq!(state, WorkerConnectionState::Connecting);

        state = WorkerConnectionState::Connected;
        assert!(transition_worker_connection_to_connecting(&mut state, true).is_err());
        assert_eq!(state, WorkerConnectionState::Connected);

        assert!(worker_url_embeds_credential(
            "wss://user:password@worker.example/ws?access_token=sample"
        ));
        assert_eq!(
            safe_worker_url_label(
                "wss://user:password@worker.example/ws?access_token=sample&keep=hidden"
            ),
            "wss://worker.example/ws"
        );
        assert_eq!(
            safe_worker_url_label("user@host/path?secret=value"),
            "host/path"
        );
        assert_eq!(
            safe_worker_url_label("wss://user:pass@worker.example"),
            "wss://worker.example"
        );
        assert_eq!(
            safe_worker_url_label("wss://worker.example/ws#secret"),
            "wss://worker.example/ws"
        );
        assert!(!worker_url_embeds_credential(
            "worker.example/ws?token=hidden"
        ));
        for key in [
            "token",
            "access_token",
            "auth",
            "authorization",
            "bearer",
            "key",
            "api_key",
            "password",
            "secret",
            "client_secret",
        ] {
            assert!(
                worker_url_embeds_credential(&format!("wss://worker.example/ws?{key}=hidden")),
                "{key}"
            );
        }
        assert!(!worker_url_embeds_credential(
            "wss://worker.example/ws?theme=dark"
        ));
    }

    #[test]
    fn worker_node_fixtures_preserve_local_connection_and_hide_url_credentials() {
        let placeholder = connection_placeholder(
            7,
            "wss://user:secret@worker.example/ws?token=hidden".to_owned(),
            WorkerConnectionState::Disconnected,
            Some("offline".to_owned()),
        );
        assert_eq!(placeholder.status.name, "wss://worker.example/ws");
        assert_eq!(
            placeholder.status.node_id,
            "wss://user:secret@worker.example/ws?token=hidden"
        );
        assert!(!placeholder.status.online);
        assert_eq!(placeholder.connection_detail.as_deref(), Some("offline"));

        let local_status = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true).status;
        let local_connection =
            super::ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
        let config = loom_protocol::WorkspaceConfig {
            worker_nodes: vec![
                loom_protocol::WorkerNodeConfig {
                    url: "ws://local-worker/ws".to_owned(),
                },
                loom_protocol::WorkerNodeConfig {
                    url: "wss://remote-worker/ws".to_owned(),
                },
            ],
            ..loom_protocol::WorkspaceConfig::default()
        };
        let nodes = initial_worker_nodes(
            local_status,
            local_connection,
            &config,
            Some("ws://local-worker/ws"),
        );
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0].is_local);
        assert_eq!(nodes[0].connection_state, WorkerConnectionState::Connected);
        assert_eq!(nodes[1].id, 1);
        assert_eq!(nodes[1].url.as_deref(), Some("wss://remote-worker/ws"));
        assert_eq!(
            nodes[1].connection_state,
            WorkerConnectionState::Disconnected
        );
        assert!(nodes[1].connection.is_none());
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn failed_connection_state_clears_transport_and_keeps_node_url() {
        let mut node = node(7, false);
        let backend = loom_local::OwnedBackend::new();
        node.connection = Some(super::ClientConnection::InProcess(Box::new(
            backend.connect(),
        )));
        node.status.online = true;
        node.connection_state = WorkerConnectionState::Connected;

        let cleanup_failed =
            mark_worker_connection_failed(&mut node, "protocol negotiation failed".to_owned());

        assert!(!cleanup_failed);
        assert!(node.connection.is_none());
        assert_eq!(node.connection_state, WorkerConnectionState::Failed);
        assert!(!node.status.online);
        assert_eq!(node.url.as_deref(), Some("ws://worker-7/ws"));
        assert_eq!(
            node.connection_detail.as_deref(),
            Some("protocol negotiation failed")
        );
    }

    fn session(id: AgentSessionId, name: &str) -> AgentSessionSnapshot {
        AgentSessionSnapshot {
            id,
            workspace_id: WorkspaceId::new(),
            name: name.to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
        }
    }

    #[test]
    fn session_list_projection_preserves_order_and_selects_active_session() {
        let first = AgentSessionId::new();
        let active = AgentSessionId::new();
        let sessions = vec![session(first, "First"), session(active, "Active")];

        let projection = session_list_projection(&sessions, active);

        assert_eq!(
            projection.entries,
            vec![(first, "First".to_owned()), (active, "Active".to_owned())]
        );
        assert_eq!(projection.selected_index, Some(1));
    }

    #[test]
    fn session_list_projection_has_no_selection_when_active_session_is_missing() {
        let sessions = vec![session(AgentSessionId::new(), "Only session")];

        let projection = session_list_projection(&sessions, AgentSessionId::new());

        assert_eq!(projection.selected_index, None);
    }

    #[test]
    fn project_session_list_projects_nested_children_and_selects_them() {
        let root_id = AgentSessionId::new();
        let child_id = AgentSessionId::new();
        let grandchild_id = AgentSessionId::new();
        let other_id = AgentSessionId::new();
        let sessions = vec![
            session(root_id, "Project"),
            session(child_id, "Researcher"),
            session(grandchild_id, "Analyst"),
            session(other_id, "Other session"),
        ];
        let project_id = loom_core::ProjectId::from_uuid(*root_id.as_uuid());
        let project = loom_core::ProjectSnapshot {
            project_id,
            root_session_id: root_id,
            agents: vec![
                loom_core::ProjectAgentRecord {
                    session_id: root_id,
                    project_id,
                    parent_session_id: None,
                    depth: 1,
                    state: AgentSessionState::Idle,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::from_unix_millis(1),
                },
                loom_core::ProjectAgentRecord {
                    session_id: child_id,
                    project_id,
                    parent_session_id: Some(root_id),
                    depth: 2,
                    state: AgentSessionState::Executing,
                    task_summary: Some("Review protocol changes".to_owned()),
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::from_unix_millis(2),
                },
                loom_core::ProjectAgentRecord {
                    session_id: grandchild_id,
                    project_id,
                    parent_session_id: Some(child_id),
                    depth: 3,
                    state: AgentSessionState::Queued,
                    task_summary: Some("Check one detail".to_owned()),
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::from_unix_millis(3),
                },
            ],
            tasks: vec![
                loom_core::DelegatedTaskRecord {
                    task_id: loom_core::TaskId::new(),
                    project_id,
                    requester_session_id: root_id,
                    target_session_id: child_id,
                    child_name: "Researcher".to_owned(),
                    intent: "Review protocol changes".to_owned(),
                    model_id: "test-model".to_owned(),
                    context_references: Vec::new(),
                    dependencies: Vec::new(),
                    code_change: false,
                    permissions: loom_core::ProjectAgentPermissions::default(),
                    status: loom_core::DelegatedTaskStatus::Blocked,
                    created_at: Timestamp::from_unix_millis(1),
                    updated_at: Timestamp::from_unix_millis(2),
                },
                loom_core::DelegatedTaskRecord {
                    task_id: loom_core::TaskId::new(),
                    project_id,
                    requester_session_id: child_id,
                    target_session_id: grandchild_id,
                    child_name: "Analyst".to_owned(),
                    intent: "Check one detail".to_owned(),
                    model_id: "test-model".to_owned(),
                    context_references: Vec::new(),
                    dependencies: Vec::new(),
                    code_change: false,
                    permissions: loom_core::ProjectAgentPermissions::default(),
                    status: loom_core::DelegatedTaskStatus::Queued,
                    created_at: Timestamp::from_unix_millis(2),
                    updated_at: Timestamp::from_unix_millis(3),
                },
            ],
            worktrees: vec![],
        };

        assert!(project_snapshot_has_unloaded_agent_sessions(
            &project,
            &sessions[..1]
        ));
        assert!(!project_snapshot_has_unloaded_agent_sessions(
            &project, &sessions
        ));
        let projection = project_session_list_projection(&sessions, child_id, Some(&project));

        assert_eq!(
            projection.entries,
            vec![
                (root_id, "Project".to_owned()),
                (child_id, "Researcher".to_owned()),
                (grandchild_id, "Analyst".to_owned()),
                (other_id, "Other session".to_owned()),
            ]
        );
        assert_eq!(projection.selected_index, Some(1));
        assert_eq!(
            projection.tree[0].children[0].children[0].session_id,
            grandchild_id
        );
        let grandchild_projection =
            project_session_list_projection(&sessions, grandchild_id, Some(&project));
        assert_eq!(grandchild_projection.selected_index, Some(2));

        let other_project_id = loom_core::ProjectId::from_uuid(*other_id.as_uuid());
        let other_project = loom_core::ProjectSnapshot {
            project_id: other_project_id,
            root_session_id: other_id,
            agents: vec![loom_core::ProjectAgentRecord {
                session_id: other_id,
                project_id: other_project_id,
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(1),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        };
        let switched_project_projection = project_session_list_projection_for_projects(
            &sessions,
            other_id,
            vec![&project, &other_project],
        );
        assert_eq!(switched_project_projection.selected_index, Some(3));
        assert_eq!(
            switched_project_projection.tree[0].children[0].children[0].session_id,
            grandchild_id
        );
        assert_eq!(switched_project_projection.tree[1].session_id, other_id);
    }

    #[test]
    fn project_filter_keeps_matching_projects_whole_and_matches_descendants() {
        let root_id = AgentSessionId::new();
        let child_id = AgentSessionId::new();
        let grandchild_id = AgentSessionId::new();
        let other_id = AgentSessionId::new();
        let tree = vec![
            SessionTreeNode {
                session_id: root_id,
                label: "My API refactor".to_owned(),
                children: vec![SessionTreeNode {
                    session_id: child_id,
                    label: "Add auth tests".to_owned(),
                    children: vec![SessionTreeNode {
                        session_id: grandchild_id,
                        label: "Fix token refresh".to_owned(),
                        children: Vec::new(),
                    }],
                }],
            },
            SessionTreeNode {
                session_id: other_id,
                label: "Docs cleanup".to_owned(),
                children: Vec::new(),
            },
        ];
        assert_eq!(session_tree_descendant_count(&tree[0]), 2);
        assert_eq!(session_tree_descendant_count(&tree[1]), 0);

        let root_match = filter_session_tree(tree.clone(), "api");
        assert_eq!(root_match.len(), 1);
        assert_eq!(root_match[0].session_id, root_id);

        // A descendant match keeps the whole project and its path.
        let child_match = filter_session_tree(tree.clone(), "auth");
        assert_eq!(child_match.len(), 1);
        assert_eq!(child_match[0].session_id, root_id);
        assert_eq!(child_match[0].children.len(), 1);
        assert_eq!(child_match[0].children[0].session_id, child_id);

        assert!(filter_session_tree(tree.clone(), "missing").is_empty());

        let other_match = filter_session_tree(tree, "docs");
        assert_eq!(other_match.len(), 1);
        assert_eq!(other_match[0].session_id, other_id);
    }

    #[test]
    fn task_status_pills_cover_every_delegated_task_state() {
        use loom_core::DelegatedTaskStatus as Task;
        for (task, label) in [
            (Task::Queued, "Queued"),
            (Task::Running, "Running"),
            (Task::Blocked, "Blocked"),
            (Task::Completed, "Done"),
            (Task::Failed, "Failed"),
            (Task::Cancelled, "Cancelled"),
        ] {
            assert_eq!(
                session_status_pill(AgentSessionState::Executing, Some(task)).label,
                label
            );
        }
        // A durable task takes precedence over the session state.
        assert_eq!(
            session_status_pill(AgentSessionState::Executing, Some(Task::Blocked)).label,
            "Blocked"
        );
    }

    #[test]
    fn project_child_controls_follow_run_and_task_state() {
        use AgentSessionState as SessionState;
        use ProjectChildControlAction as Action;
        use loom_core::DelegatedTaskStatus as TaskStatus;

        assert_eq!(
            project_child_control_actions(SessionState::Executing, TaskStatus::Running),
            vec![Action::Pause, Action::Interrupt, Action::Cancel]
        );
        assert_eq!(
            project_child_control_actions(SessionState::Paused, TaskStatus::Blocked),
            vec![Action::Continue, Action::Cancel]
        );
        assert_eq!(
            project_child_control_actions(SessionState::Queued, TaskStatus::Queued),
            vec![Action::Continue, Action::Cancel]
        );
        assert_eq!(
            project_child_control_actions(SessionState::Failed, TaskStatus::Failed),
            vec![Action::RetryFailedStep, Action::Cancel]
        );
        assert!(
            project_child_control_actions(SessionState::Completed, TaskStatus::Completed)
                .is_empty()
        );
    }

    #[test]
    fn project_worktree_updates_are_included_in_project_workspace_refreshes() {
        let project_id = loom_core::ProjectId::new();
        let root_session_id = AgentSessionId::new();
        let worktree = loom_core::ProjectWorktreeRecord {
            project_id,
            task_id: loom_core::TaskId::new(),
            parent_session_id: root_session_id,
            child_session_id: AgentSessionId::new(),
            parent_repository_id: loom_core::RepositoryId::new(),
            child_repository_id: loom_core::RepositoryId::new(),
            relative_path: "project-worktrees/example".to_owned(),
            worktree_name: "loom-child-example".to_owned(),
            branch_name: "loom/project-child-example".to_owned(),
            base_revision: "base".to_owned(),
            result_revision: None,
            integrated_revision: None,
            status: loom_core::ProjectWorktreeStatus::Ready,
            conflict_paths: Vec::new(),
            error: None,
            cleanup_disposition: None,
            created_at: loom_core::Timestamp::from_unix_millis(1),
            updated_at: loom_core::Timestamp::from_unix_millis(1),
        };
        let event =
            loom_protocol::WorkspaceFeedEvent::Session(loom_protocol::ServerEventEnvelope {
                protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(3),
                session_id: root_session_id,
                event: loom_protocol::ServerEvent::ProjectChildWorktreeUpdated { worktree },
            });
        let members = std::collections::BTreeSet::from([root_session_id]);

        assert!(super::is_project_workspace_event(
            &event, project_id, &members
        ));
        assert!(!super::is_project_workspace_event(
            &event,
            loom_core::ProjectId::new(),
            &members
        ));
    }

    #[test]
    fn source_dialog_initial_choice_tracks_purpose_and_local_availability() {
        assert_eq!(
            source_dialog_initial_state(SessionSourceDialogPurpose::StartSession, true),
            SessionSourceChoice::Empty
        );
        assert_eq!(
            source_dialog_initial_state(SessionSourceDialogPurpose::AddToSession, true),
            SessionSourceChoice::LocalDirectory
        );
        assert_eq!(
            source_dialog_initial_state(SessionSourceDialogPurpose::AddToSession, false),
            SessionSourceChoice::GitHub
        );
    }

    #[test]
    fn local_source_availability_respects_backend_and_session_ownership() {
        use SessionSourceDialogPurpose::{AddToSession, StartSession};

        assert!(local_source_available(
            StartSession,
            true,
            Some("remote"),
            "local"
        ));
        assert!(!local_source_available(StartSession, false, None, "local"));
        assert!(local_source_available(AddToSession, true, None, "local"));
        assert!(local_source_available(
            AddToSession,
            true,
            Some("local"),
            "local"
        ));
        assert!(!local_source_available(
            AddToSession,
            true,
            Some("remote"),
            "local"
        ));
        assert!(!local_source_available(
            AddToSession,
            false,
            Some("local"),
            "local"
        ));
    }

    #[test]
    fn source_choice_validation_rejects_empty_additions_and_unavailable_local_sources() {
        use SessionSourceChoice::{Empty, GitHub, LocalDirectory};
        use SessionSourceDialogPurpose::{AddToSession, StartSession};

        assert!(source_choice_is_allowed(StartSession, false, Empty));
        assert!(!source_choice_is_allowed(AddToSession, true, Empty));
        assert!(source_choice_is_allowed(AddToSession, true, LocalDirectory));
        assert!(!source_choice_is_allowed(
            AddToSession,
            false,
            LocalDirectory
        ));
        assert!(source_choice_is_allowed(AddToSession, false, GitHub));
    }

    #[test]
    fn local_worker_node_cannot_be_removed() {
        let mut nodes = vec![node(0, true)];

        assert!(remove_worker_node_entry(&mut nodes, 0).is_none());
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].is_local());
    }

    #[test]
    fn configured_worker_node_can_be_removed_without_removing_local_node() {
        let mut nodes = vec![node(0, true), node(1, false)];

        let removed = remove_worker_node_entry(&mut nodes, 1).unwrap();

        assert_eq!(removed.id, 1);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].is_local());
        assert!(remove_worker_node_entry(&mut nodes, 999).is_none());
    }

    #[test]
    fn unavailable_and_available_resource_percentages_are_formatted() {
        assert_eq!(format_percentage(None), "n/a");
        assert_eq!(format_percentage(Some(0)), "0%");
        assert_eq!(format_percentage(Some(73)), "73%");
        assert_eq!(format_percentage(Some(101)), "n/a");
    }

    #[test]
    fn resource_summary_keeps_cpu_cores_and_total_ram_across_samples() {
        let initial = WorkerNodeResources {
            cpu_count: 8,
            cpu_usage_percent: None,
            memory_usage_percent: None,
            memory_total_bytes: Some(16 << 30),
            memory_available_bytes: Some(8 << 30),
            disk_total_bytes: Some(1 << 30),
            disk_available_bytes: Some(512 << 20),
        };
        let initial_summary = format_worker_node_resources(&initial);
        assert!(initial_summary.contains("CPU n/a of 8 cores"));
        assert!(initial_summary.contains("RAM n/a of 16.0 GiB"));
        assert!(initial_summary.contains("disk 512.0 MiB available"));

        let updated = WorkerNodeResources {
            cpu_usage_percent: Some(31),
            memory_usage_percent: Some(50),
            ..initial
        };
        let updated_summary = format_worker_node_resources(&updated);
        assert!(updated_summary.contains("CPU 31% of 8 cores"));
        assert!(updated_summary.contains("RAM 50% of 16.0 GiB"));
        assert!(updated_summary.contains("disk 512.0 MiB available"));
        assert!(
            format_worker_node_resources(&WorkerNodeResources {
                cpu_count: 0,
                cpu_usage_percent: None,
                memory_usage_percent: None,
                memory_total_bytes: None,
                memory_available_bytes: None,
                disk_total_bytes: None,
                disk_available_bytes: None,
            })
            .starts_with("CPU n/a · RAM")
        );
    }

    #[test]
    fn refreshed_worker_status_replaces_initial_unavailable_percentages() {
        let mut nodes = vec![node(1, false)];
        nodes[0].status.resources = WorkerNodeResources {
            cpu_count: 4,
            cpu_usage_percent: None,
            memory_usage_percent: None,
            memory_total_bytes: Some(8 << 30),
            memory_available_bytes: Some(4 << 30),
            disk_total_bytes: Some(100 << 30),
            disk_available_bytes: Some(50 << 30),
        };
        assert!(
            format_worker_node_resources(&nodes[0].status.resources)
                .contains("CPU n/a of 4 cores · RAM n/a of 8.0 GiB")
        );

        let mut refreshed = nodes[0].status.clone();
        refreshed.resources.cpu_usage_percent = Some(25);
        refreshed.resources.memory_usage_percent = Some(50);
        assert_eq!(
            update_worker_node_status(&mut nodes, 1, refreshed.clone()),
            None
        );

        let summary = format_worker_node_resources(&nodes[0].status.resources);
        assert!(summary.contains("CPU 25% of 4 cores · RAM 50% of 8.0 GiB"));
        assert!(summary.contains("disk 50.0 GiB available"));
        assert_eq!(update_worker_node_status(&mut nodes, 999, refreshed), None);
    }

    #[test]
    fn session_status_uses_the_assigned_node_not_the_active_backend() {
        let mut active_backend = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true);
        active_backend.status.resources.cpu_usage_percent = Some(22);
        active_backend.status.resources.memory_usage_percent = Some(38);

        let mut peer = node(1, false);
        peer.status.resources.cpu_usage_percent = Some(99);
        peer.status.resources.memory_usage_percent = Some(97);
        let nodes = vec![active_backend, peer];
        let session_id = AgentSessionId::new();
        let owners = BTreeMap::from([(session_id, "node-1".to_owned())]);

        let owner = session_owner_status(&nodes, &owners, session_id).unwrap();
        assert_eq!(worker_node_display_name(owner), "External worker · Node 1");
        assert_eq!(
            format_session_resource_percentages(Some(&owner.status)),
            "CPU 99% · RAM 97%"
        );
        let node_names =
            BTreeMap::from([("node-1".to_owned(), "External worker · Node 1".to_owned())]);
        assert_eq!(
            worker_node_name_for_id(&nodes[..1], &node_names, Some("node-1")),
            "External worker · Node 1"
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot { session_id }),
                AgentSessionId::new()
            ),
            Some(session_id)
        );
    }

    #[test]
    fn run_requests_route_to_the_active_session_owner() {
        let active_session_id = AgentSessionId::new();
        let owners = BTreeMap::from([(active_session_id, "peer-node".to_owned())]);
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Run(RunRequest::SendAgentMessage {
                    run_id: RunId::new(),
                    attempt_id: loom_core::RunAttemptId::new(),
                    expected_control_revision: 0,
                    message: "hello".to_owned(),
                }),
                active_session_id
            ),
            Some(active_session_id)
        );
        assert_eq!(
            assigned_node_id(&owners, active_session_id),
            Ok("peer-node")
        );
        assert!(assigned_node_id(&BTreeMap::new(), active_session_id).is_err());
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
                active_session_id
            ),
            None
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint {
                    session_id: active_session_id,
                    label: "checkpoint".to_owned(),
                }),
                AgentSessionId::new()
            ),
            Some(active_session_id)
        );
    }

    #[test]
    fn session_request_routing_covers_explicit_active_and_non_session_requests() {
        let session = AgentSessionId::new();
        let active = AgentSessionId::new();
        let repository = loom_core::RepositoryId::new();
        let terminal = loom_core::TerminalId::new();
        let task = loom_core::TaskId::new();
        let checkpoint = loom_core::CheckpointId::new();
        let explicit_requests = vec![
            ClientRequest::Session(SessionRequest::GetAgentSession {
                session_id: session,
            }),
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot {
                session_id: session,
            }),
            ClientRequest::Session(SessionRequest::RenameAgentSession {
                session_id: session,
                name: "renamed".into(),
            }),
            ClientRequest::Session(SessionRequest::ArchiveAgentSession {
                session_id: session,
            }),
            ClientRequest::Events(EventsRequest::GetRecentSessionEvents {
                session_id: session,
                limit: 5,
            }),
            ClientRequest::Run(RunRequest::StartSessionAgentRun {
                session_id: session,
                task: "task".into(),
                model: ModelId::new("model"),
                system_instructions: None,
                repository_instructions: None,
            }),
            ClientRequest::Run(RunRequest::StartSessionAgentRunWithOptions {
                session_id: session,
                task: "task".into(),
                model: ModelId::new("model"),
                system_instructions: None,
                repository_instructions: None,
                limits: loom_core::SessionLimits::default(),
                context: loom_protocol::ContextAssemblyOptions::default(),
            }),
            ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                session_id: session,
                source: "/repo".into(),
                path: "repo".into(),
                revision: None,
            }),
            ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                session_id: session,
                source: "/folder".into(),
                path: "folder".into(),
            }),
            ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories {
                session_id: session,
            }),
            ClientRequest::Filesystem(FilesystemRequest::DetachSessionDirectory {
                session_id: session,
                path: "folder".into(),
            }),
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                session_id: session,
            }),
            ClientRequest::Repository(RepositoryRequest::DetachSessionRepository {
                session_id: session,
                repository_id: repository,
            }),
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot {
                session_id: session,
            }),
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
                session_id: session,
                after_sequence: None,
            }),
            ClientRequest::Filesystem(FilesystemRequest::ReadSessionFile {
                session_id: session,
                path: "file".into(),
            }),
            ClientRequest::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit {
                session_id: session,
                edit: loom_protocol::WorkspaceEdit {
                    path: "file".into(),
                    old_text: "old".into(),
                    new_text: "new".into(),
                    expected_revision: None,
                },
            }),
            ClientRequest::Filesystem(FilesystemRequest::TakeSessionFilesystemControl {
                session_id: session,
                control: loom_protocol::WorkspaceControl::Agent,
            }),
            ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint {
                session_id: session,
                label: "checkpoint".into(),
            }),
            ClientRequest::Filesystem(FilesystemRequest::RevertSessionCheckpoint {
                session_id: session,
                checkpoint_id: checkpoint,
            }),
            ClientRequest::Filesystem(FilesystemRequest::UndoSessionEdit {
                session_id: session,
            }),
            ClientRequest::Filesystem(FilesystemRequest::GetSessionContextFiles {
                session_id: session,
            }),
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsStatus {
                session_id: session,
                repository_id: repository,
            }),
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsDiff {
                session_id: session,
                repository_id: repository,
                path: None,
                staged: false,
            }),
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsBranches {
                session_id: session,
                repository_id: repository,
            }),
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsConflicts {
                session_id: session,
                repository_id: repository,
            }),
            ClientRequest::Terminal(TerminalRequest::OpenSessionTerminal {
                session_id: session,
                command: "sh".into(),
                args: Vec::new(),
                cwd: None,
            }),
            ClientRequest::Terminal(TerminalRequest::WriteSessionTerminalInput {
                session_id: session,
                terminal_id: terminal,
                input: "exit".into(),
            }),
            ClientRequest::Terminal(TerminalRequest::ResizeSessionTerminal {
                session_id: session,
                terminal_id: terminal,
                rows: 24,
                columns: 80,
            }),
            ClientRequest::Terminal(TerminalRequest::GetSessionTerminalEvents {
                session_id: session,
                terminal_id: terminal,
                after_sequence: None,
            }),
            ClientRequest::Terminal(TerminalRequest::CancelSessionTerminal {
                session_id: session,
                terminal_id: terminal,
            }),
            ClientRequest::Task(TaskRequest::StartSessionTask {
                session_id: session,
                spec: loom_protocol::TaskSpec {
                    kind: loom_protocol::TaskKind::Test,
                    label: "test".into(),
                    command: "cargo".into(),
                    args: vec!["test".into()],
                    cwd: None,
                    output_limit_bytes: None,
                    artifact_paths: Vec::new(),
                },
            }),
            ClientRequest::Task(TaskRequest::ListSessionTasks {
                session_id: session,
            }),
            ClientRequest::Task(TaskRequest::GetSessionTask {
                session_id: session,
                task_id: task,
            }),
            ClientRequest::Task(TaskRequest::GetSessionTaskEvents {
                session_id: session,
                task_id: task,
                after_sequence: None,
            }),
            ClientRequest::Task(TaskRequest::CancelSessionTask {
                session_id: session,
                task_id: task,
            }),
            ClientRequest::Task(TaskRequest::GetSessionTaskEvidence {
                session_id: session,
                task_id: task,
            }),
            ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy {
                session_id: session,
                policy: loom_core::ApprovalPolicy::default(),
                auto_approve_actions: None,
            }),
            ClientRequest::Session(SessionRequest::ForkAgentSession {
                session_id: session,
                name: "fork".into(),
            }),
            ClientRequest::Usage(UsageRequest::GetSessionUsage {
                session_id: session,
            }),
        ];
        assert!(
            explicit_requests
                .iter()
                .all(|request| { session_id_for_request(request, active) == Some(session) })
        );

        assert_eq!(
            session_id_for_request(
                &ClientRequest::Events(EventsRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }),
                active
            ),
            Some(active)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Events(EventsRequest::GetSessionEvents {
                    session_id: Some(session),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }),
                active
            ),
            Some(session)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Run(RunRequest::GetAgentRun {
                    run_id: RunId::new()
                }),
                active
            ),
            Some(active)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Usage(UsageRequest::GetRunUsage {
                    run_id: RunId::new()
                }),
                active
            ),
            Some(active)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
                active
            ),
            None
        );
    }

    #[test]
    fn session_aggregation_tracks_node_owners_and_preserves_disconnected_sessions() {
        let primary_id = AgentSessionId::new();
        let peer_id = AgentSessionId::new();
        let disconnected_id = AgentSessionId::new();
        let primary_session = session(primary_id, "Primary");
        let peer_session = session(peer_id, "Peer");
        let disconnected_session = session(disconnected_id, "Disconnected");
        let current = vec![primary_session.clone(), disconnected_session.clone()];
        let owners = BTreeMap::from([
            (primary_id, "node-0".to_owned()),
            (disconnected_id, "removed-node".to_owned()),
        ]);

        let (sessions, owners) = merge_node_sessions(
            &current,
            &owners,
            vec![
                ("node-0".to_owned(), vec![primary_session]),
                ("node-1".to_owned(), vec![peer_session]),
            ],
        );

        assert_eq!(sessions.len(), 3);
        assert_eq!(owners.get(&primary_id).map(String::as_str), Some("node-0"));
        assert_eq!(owners.get(&peer_id).map(String::as_str), Some("node-1"));
        assert_eq!(
            owners.get(&disconnected_id).map(String::as_str),
            Some("removed-node")
        );
    }

    #[test]
    fn session_aggregation_rebinds_returned_sessions_to_a_restarted_node_identity() {
        let session_id = AgentSessionId::new();
        let current = vec![session(session_id, "Existing")];
        let current_owners = BTreeMap::from([(session_id, "old-node-id".to_owned())]);

        let (sessions, owners) = merge_node_sessions(
            &current,
            &current_owners,
            vec![(
                "new-node-id".to_owned(),
                vec![session(session_id, "Existing")],
            )],
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            owners.get(&session_id).map(String::as_str),
            Some("new-node-id")
        );
    }

    #[test]
    fn new_session_node_choices_keep_the_default_backend_first() {
        let nodes = vec![
            ("peer".to_owned(), "External worker".to_owned()),
            ("default".to_owned(), "Local backend".to_owned()),
        ];
        let ordered = order_session_nodes(nodes, "default");

        assert_eq!(ordered[0].0, "default");
        assert_eq!(ordered[1].0, "peer");
    }

    #[test]
    fn model_selection_uses_the_chosen_workers_catalog() {
        let local_model = ModelId::new("local/provider-model");
        let worker_model = ModelId::new("worker/provider-model");
        let catalogs = BTreeMap::from([
            ("local".to_owned(), vec![local_model.clone()]),
            ("worker".to_owned(), vec![worker_model.clone()]),
        ]);

        assert!(validate_model_for_node(&catalogs, "local", &local_model).is_ok());
        assert!(validate_model_for_node(&catalogs, "worker", &local_model).is_err());
        assert!(validate_model_for_node(&catalogs, "worker", &worker_model).is_ok());
        assert_eq!(
            validate_model_for_node(&catalogs, "missing", &worker_model),
            Err("model availability has not been checked".to_owned())
        );
    }

    #[test]
    fn assigned_node_status_drives_dot_pulse_speed_and_intensity() {
        let mut low_load = node(1, false).status;
        low_load.resources.cpu_usage_percent = Some(6);
        low_load.resources.memory_usage_percent = Some(10);
        let mut high_load = low_load.clone();
        high_load.resources.cpu_usage_percent = Some(80);
        high_load.resources.memory_usage_percent = Some(60);
        let unknown_load = node(2, false).status;

        let (low_period, low_amplitude) = session_node_pulse(Some(&low_load), 5).unwrap();
        let (high_period, high_amplitude) = session_node_pulse(Some(&high_load), 5).unwrap();

        assert!(high_period < low_period);
        assert!(high_amplitude > low_amplitude);
        assert_eq!(session_node_pulse(Some(&unknown_load), 5), None);
        assert_eq!(session_node_pulse(None, 5), None);
        assert_eq!(session_node_pulse(Some(&low_load), 6), None);
        high_load.online = false;
        assert_eq!(session_node_pulse(Some(&high_load), 5), None);
    }

    #[test]
    fn pulse_threshold_adjustment_is_bounded_and_uses_five_percent_by_default() {
        assert_eq!(
            loom_protocol::WorkspaceConfig::default().cpu_pulse_threshold_percent,
            5
        );
        assert_eq!(adjusted_cpu_pulse_threshold(5, -1), 4);
        assert_eq!(adjusted_cpu_pulse_threshold(0, -1), 0);
        assert_eq!(adjusted_cpu_pulse_threshold(100, 1), 100);
        assert_eq!(adjusted_cpu_pulse_threshold(99, 1), 100);
        assert_eq!(adjusted_cpu_pulse_threshold(255, 0), 100);
    }

    #[test]
    fn project_agent_concurrency_adjustment_is_bounded() {
        assert_eq!(
            loom_protocol::WorkspaceConfig::default().project_agent_concurrency,
            4
        );
        assert_eq!(adjusted_project_agent_concurrency(4, -1), 3);
        assert_eq!(
            adjusted_project_agent_concurrency(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY, -1),
            loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY
        );
        assert_eq!(
            adjusted_project_agent_concurrency(loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY, 1),
            loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY
        );
    }

    #[test]
    fn severe_load_red_requires_three_consecutive_dual_threshold_samples() {
        let mut status = node(1, false).status;
        status.resources.cpu_usage_percent = Some(91);
        status.resources.memory_usage_percent = Some(91);

        let first = next_severe_load_streak(0, &status.resources);
        let second = next_severe_load_streak(first, &status.resources);
        assert_eq!(
            session_node_indicator_state(Some(&status), second),
            SessionNodeIndicatorState::Online
        );
        let third = next_severe_load_streak(second, &status.resources);
        assert_eq!(third, 3);
        assert_eq!(
            session_node_indicator_state(Some(&status), third),
            SessionNodeIndicatorState::Severe
        );
        assert_eq!(next_severe_load_streak(u8::MAX, &status.resources), u8::MAX);

        status.resources.cpu_usage_percent = Some(90);
        assert_eq!(next_severe_load_streak(third, &status.resources), 0);
        status.resources.cpu_usage_percent = Some(91);
        status.resources.memory_usage_percent = None;
        assert_eq!(next_severe_load_streak(third, &status.resources), 0);
        status.resources.memory_usage_percent = Some(91);
        status.online = false;
        assert_eq!(
            session_node_indicator_state(Some(&status), third),
            SessionNodeIndicatorState::Offline
        );
    }

    #[test]
    fn active_backend_and_external_worker_labels_do_not_collide() {
        let mut active_backend = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true);
        active_backend.status.name = "local".to_owned();
        let mut external_worker = node(1, false);
        external_worker.status.name = "local".to_owned();

        assert_eq!(
            worker_node_display_name(&active_backend),
            "Local backend · local"
        );
        assert_eq!(
            worker_node_display_name(&external_worker),
            "External worker · local"
        );
    }
}

#[cfg(test)]
mod transcript_paging_tests {
    use super::{
        TimelineItem, project_message_transcript_content,
        remove_project_message_context_duplicates, timeline_items_from_messages,
        tool_display_title, unseen_transcript_messages,
    };
    use crate::state::AssistantPart;
    use loom_core::{
        ActivityId, AgentMessageId, AgentMessageKind, AgentMessageRecord, ProjectId, RunId,
        Timestamp, ToolCallId,
    };
    use loom_model::{MessageRole, ModelId, ModelMessage};
    use loom_protocol::{
        AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, ToolResult,
    };
    use std::collections::BTreeSet;
    #[test]
    fn transcript_pages_merge_assistant_text_and_attach_tool_output() {
        let messages = vec![
            (0, 0, ModelMessage::new(MessageRole::User, "task")),
            (1, 1, ModelMessage::new(MessageRole::Assistant, "first")),
            (2, 2, ModelMessage::new(MessageRole::Assistant, "second")),
            (3, 3, ModelMessage::new(MessageRole::Tool, "tool output")),
            (4, 4, ModelMessage::new(MessageRole::System, "hidden")),
            (5, 5, ModelMessage::new(MessageRole::User, "follow-up")),
        ];

        let timeline = timeline_items_from_messages(messages, Vec::new());
        assert_eq!(timeline.len(), 3);
        assert!(matches!(&timeline[0], TimelineItem::User(task) if task == "task"));
        let TimelineItem::Assistant(turn) = &timeline[1] else {
            panic!("expected assistant turn");
        };
        assert_eq!(
            turn.parts[0],
            AssistantPart::Text("first\n\nsecond".to_owned())
        );
        assert!(matches!(
            &turn.parts[1],
            AssistantPart::Tool(part) if part.output.as_deref() == Some("tool output")
        ));
        assert!(matches!(&timeline[2], TimelineItem::User(follow_up) if follow_up == "follow-up"));
    }

    #[test]
    fn restored_project_tool_output_is_humanized() {
        let mut tool = ModelMessage::new(
            MessageRole::Tool,
            r#"{"task_id":"6ee93097-078a-4ad1-86b7-d2cd8d0d3226","child_session_id":"158c02af","status":"running","child_name":"five-second-sleep"}"#,
        );
        tool.name = Some("delegate_project_task".to_owned());
        let timeline = timeline_items_from_messages(vec![(0, 0, tool)], Vec::new());
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        let AssistantPart::Tool(part) = &turn.parts[0] else {
            panic!("expected tool part");
        };
        let output = part.output.as_deref().unwrap();
        assert!(output.contains("Created sub-agent \"five-second-sleep\" · running"));
        assert!(!output.contains("child_session_id"));
    }

    #[test]
    fn restored_project_message_is_not_misattributed_to_the_user_or_duplicated() {
        let sender_session_id = loom_core::AgentSessionId::new();
        let record = AgentMessageRecord {
            message_id: AgentMessageId::new(),
            project_id: ProjectId::new(),
            task_id: None,
            sender_session_id,
            target_session_id: loom_core::AgentSessionId::new(),
            kind: AgentMessageKind::Result,
            project_sequence: 1,
            accepted_at: Timestamp::from_unix_millis(1),
            body: "The child finished.".to_owned(),
        };
        let exact_context = project_message_transcript_content(&record);
        let mut project_message = ModelMessage::new(MessageRole::User, exact_context.clone());
        project_message.name = Some("loom_project_message".to_owned());
        let mut timeline = timeline_items_from_messages(vec![(0, 0, project_message)], Vec::new());
        assert!(matches!(
            timeline.as_slice(),
            [TimelineItem::ProjectMessageContext(content)] if content == &exact_context
        ));

        remove_project_message_context_duplicates(&mut timeline, &[record]);
        assert!(timeline.is_empty());
    }

    #[test]
    fn restores_messages_and_activities_by_their_shared_order() {
        let messages = vec![
            (0, 0, ModelMessage::new(MessageRole::User, "task")),
            (
                1,
                1,
                ModelMessage::new(MessageRole::Assistant, "I found the relevant files."),
            ),
            (
                2,
                3,
                ModelMessage::new(MessageRole::User, "Check one more thing"),
            ),
            (
                3,
                5,
                ModelMessage::new(MessageRole::Assistant, "The answer is complete."),
            ),
        ];
        let activities = [2, 4]
            .into_iter()
            .map(|timeline_ordinal| AgentActivityRecord {
                id: ActivityId::new(),
                run_id: RunId::new(),
                timeline_ordinal,
                parent_id: None,
                step_id: None,
                kind: AgentActivityKind::ModelTurn,
                status: AgentActivityStatus::Completed,
                started_at: Timestamp::from_unix_millis(timeline_ordinal),
                completed_at: None,
                elapsed_ms: None,
                data: AgentActivityData::ModelTurn {
                    model: ModelId::new("deterministic/demo"),
                },
            })
            .collect();
        let timeline = timeline_items_from_messages(messages, activities);

        assert_eq!(timeline.len(), 4);
        assert!(matches!(&timeline[0], TimelineItem::User(task) if task == "task"));
        let TimelineItem::Assistant(turn) = &timeline[1] else {
            panic!("expected assistant turn");
        };
        assert_eq!(
            turn.parts,
            vec![AssistantPart::Text(
                "I found the relevant files.".to_owned()
            )]
        );
        assert!(
            matches!(&timeline[2], TimelineItem::User(follow_up) if follow_up == "Check one more thing")
        );
        let TimelineItem::Assistant(turn) = &timeline[3] else {
            panic!("expected assistant turn");
        };
        assert_eq!(
            turn.parts,
            vec![AssistantPart::Text("The answer is complete.".to_owned())]
        );
    }

    #[test]
    fn transcript_pages_ignore_ordinals_already_loaded() {
        let mut loaded = BTreeSet::new();
        let first_page = unseen_transcript_messages(
            vec![
                (
                    3,
                    8,
                    ModelMessage::new(MessageRole::User, "current question"),
                ),
                (4, 9, ModelMessage::new(MessageRole::Assistant, "answer")),
            ],
            &mut loaded,
        );
        assert_eq!(first_page.len(), 2);

        let overlapping_page = unseen_transcript_messages(
            vec![
                (2, 7, ModelMessage::new(MessageRole::User, "older question")),
                (
                    3,
                    8,
                    ModelMessage::new(MessageRole::User, "current question"),
                ),
                (4, 9, ModelMessage::new(MessageRole::Assistant, "answer")),
            ],
            &mut loaded,
        );
        assert_eq!(overlapping_page.len(), 1);
        assert_eq!(overlapping_page[0].2.content, "older question");
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn asynchronous_page_loader_uses_the_bounded_transcript_endpoint() {
        use super::{BackendWorker, ClientConnection, load_transcript_page};
        use loom_protocol::{
            ClientRequest, RequestEnvelope, RunRequest, RunResponse, ServerResponse,
        };

        let backend = loom_local::OwnedBackend::new();
        let connection = ClientConnection::InProcess(Box::new(backend.connect()));
        crate::connection::negotiate(&connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&connection, "Transcript pages").unwrap();
        let session = crate::connection::create_session_in_workspace(
            &connection,
            workspace.id,
            "Paged session",
        )
        .unwrap();
        let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "load only a transcript page".to_owned(),
                model: loom_model::ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            },
        )));
        let run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(run)) => run.id,
            response => panic!("unexpected run start response: {response:?}"),
        };
        let worker = BackendWorker::spawn(connection);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (messages, next_before, has_older) = runtime
            .block_on(load_transcript_page(worker, run_id, None))
            .unwrap();
        assert_eq!(next_before, Some(0));
        assert!(!has_older);
        assert!(messages.iter().any(|(_, _, message)| {
            message.role == MessageRole::User && message.content == "load only a transcript page"
        }));
    }

    #[test]
    fn restored_search_keeps_its_query_and_hit_count() {
        let call = loom_model::ToolCall {
            id: ToolCallId::new(),
            name: "search_text".to_owned(),
            arguments: serde_json::json!({"query": "SampleCount"}),
        };
        let body = "src/lib.rs:12:let count = SampleCount::new();\nsrc/main.rs:4:SampleCount\n";
        let activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id: RunId::new(),
            timeline_ordinal: 0,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::Search,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::Search {
                call: call.clone(),
                query: "SampleCount".to_owned(),
                path: None,
                result: Some(ToolResult::success(&call, body.to_owned())),
            },
        };
        let mut tool = ModelMessage::new(MessageRole::Tool, body);
        tool.name = Some("search_text".to_owned());
        tool.tool_call_id = Some(call.id);

        // The activity lands before the result message, and the message carries
        // no arguments; the activity title must survive.
        let timeline = timeline_items_from_messages(vec![(1, 1, tool)], vec![activity]);
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        let AssistantPart::Tool(part) = &turn.parts[0] else {
            panic!("expected tool part");
        };
        assert_eq!(part.title, "Search \"SampleCount\"");
        assert_eq!(part.name, "search_text");
        assert_eq!(tool_display_title(part), "Search \"SampleCount\" · 2 hits");
    }

    #[test]
    fn restored_assistant_reasoning_is_shown_above_its_text() {
        let mut message = ModelMessage::new(MessageRole::Assistant, "the answer");
        message.reasoning_content = Some("weighed the options".to_owned());
        let timeline = timeline_items_from_messages(vec![(0, 0, message)], Vec::new());
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        assert_eq!(
            turn.parts,
            vec![
                AssistantPart::Reasoning("weighed the options".to_owned()),
                AssistantPart::Text("the answer".to_owned()),
            ]
        );

        // An empty reasoning field is not a visible part.
        let mut empty = ModelMessage::new(MessageRole::Assistant, "plain");
        empty.reasoning_content = Some(String::new());
        let timeline = timeline_items_from_messages(vec![(0, 0, empty)], Vec::new());
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        assert_eq!(turn.parts, vec![AssistantPart::Text("plain".to_owned())]);
    }
}

#[cfg(test)]
mod provider_control_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn provider_model_and_mode_controls_update_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sync_model_select_states(window, cx);
            view.sync_agent_mode_select_state(window, cx);
            assert!(view.model_select.is_some());
            assert!(view.agent_mode_select.is_some());

            view.select_model(ModelId::new("deterministic/demo"), cx);
            assert_eq!(view.model, ModelId::new("deterministic/demo"));
            view.select_model(ModelId::new("missing/model"), cx);

            view.select_default_model(ModelId::new("deterministic/demo"), cx);
            assert_eq!(view.default_model, ModelId::new("deterministic/demo"));
            view.select_default_model(ModelId::new("missing/model"), cx);

            view.select_agent_mode(AgentMode::Edit, cx);
            view.toggle_auto_approve_actions(cx);
            view.observe_system_appearance(window, cx);
            view.observe_system_appearance(window, cx);

            view.open_settings_from_menu(cx);
            assert!(view.settings_open);
            view.open_providers_for_node("test-node".to_owned(), cx);
            assert!(view.settings_open);
            assert_eq!(view.settings_section, SettingsSection::Providers);

            view.handle_github_provider_configured("test-node".to_owned(), cx);
            assert!(view.github_connected);

            let appearance = window.appearance();
            view.apply_appearance(appearance, window, cx);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn github_write_access_toggle_round_trips(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            assert!(!view.github_write_access);
            view.github_repository_connected = true;
            view.settings_open = true;
            view.settings_section = SettingsSection::Providers;
            view
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window
                .within("settings-dialog")
                .click("github-write-access-toggle", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                assert!(view.github_write_access);
                view.toggle_github_write_access(false, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                assert!(!view.github_write_access);
                view.refresh_github_write_access("missing-node".to_owned(), cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[test]
    fn github_login_kind_secure_messages_differ() {
        assert!(
            crate::state::GitHubLoginKind::Copilot
                .secure_connection_message()
                .contains("Copilot")
        );
        assert!(
            crate::state::GitHubLoginKind::Repository
                .secure_connection_message()
                .contains("repository")
        );
    }

    #[gpui_kit::test]
    fn begin_github_login_clears_finished_states(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.github_login = Some(GitHubLoginState::Success);
            view.begin_github_login(crate::state::GitHubLoginKind::Repository, cx);
            assert!(view.github_login.is_none());
            view.github_login = Some(GitHubLoginState::Error("failed".to_owned()));
            view.begin_github_login(crate::state::GitHubLoginKind::Copilot, cx);
            assert!(view.github_login.is_none());
            view
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn github_repository_access_row_renders_when_disconnected(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.login_enabled = true;
            view.github_repository_connected = false;
            view.settings_open = true;
            view.settings_section = SettingsSection::Providers;
            view
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn github_copilot_login_finishes_by_registering_the_provider(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.github_login_kind = crate::state::GitHubLoginKind::Copilot;
            view.finish_github_login(Ok("ghu_copilot_token".to_owned()), cx);
            view
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, _| {
                assert!(view.github_connected);
                assert!(matches!(view.github_login, Some(GitHubLoginState::Success)));
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod state_toggle_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn review_composer_and_worker_controls_toggle(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.toggle_review_pane(cx);
            assert!(view.review.open);
            view.toggle_review_pane(cx);
            assert!(!view.review.open);

            let tool_id = loom_core::ToolCallId::new();
            view.toggle_tool(tool_id, cx);
            view.toggle_tool(tool_id, cx);
            view.toggle_tool_group(7, cx);
            view.toggle_tool_group(7, cx);
            view.toggle_reasoning(9, cx);
            view.toggle_reasoning(9, cx);

            view.toggle_command_palette(cx);
            assert!(view.command_palette_open);
            view.close_command_palette(cx);
            assert!(!view.command_palette_open);
            view.run_slash_command("/help", cx);
            view.run_command("unknown-command", None, cx);

            view.adjust_cpu_pulse_threshold(5, cx);
            view.adjust_project_agent_concurrency(1, cx);
            view.adjust_font_scale(1, window, cx);
            view.set_font_scale_percent(120, window, cx);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod session_run_action_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn session_and_run_actions_update_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session = empty_session_snapshot(view.workspace_id);
            view.activate_session(session.clone());
            view.load_session(session.clone());
            view.select_session(session.clone(), cx);
            view.begin_session_rename(session.clone(), false, window, cx);
            assert!(view.rename_dialog.is_some());
            view.confirm_rename(cx);
            let archive_id = view.active_session.id;
            view.archive_session(archive_id, cx);
            view.select_session_repository(loom_core::RepositoryId::new(), cx);
            view.detach_session_repository(loom_core::RepositoryId::new(), cx);
            view.detach_session_directory("dir".to_owned(), cx);
            view.send_message("hello".to_owned(), cx);
            view.approve_pending_action(cx);
            view.reject_pending_action(cx);
            view.interrupt_active_run(cx);
            view.begin_transcript_page(None, cx);
            view.ensure_session_task_message(view.active_session.id);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod project_action_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn project_actions_update_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.rebuild_project_message_timeline();
            view.refresh_active_project_snapshot(cx);
            view.refresh_project_messages(cx);
            let _ = view.project_root_is_active();
            let _ = view.project_has_live_children();
            let manager = AgentSessionId::new();
            view.control_project_child_from_ui(
                manager,
                loom_core::ProjectId::new(),
                loom_core::TaskId::new(),
                loom_protocol::ProjectChildControlAction::Continue,
                cx,
            );
            view.review_project_child_from_ui(
                manager,
                loom_core::ProjectId::new(),
                loom_core::TaskId::new(),
                cx,
            );
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod render_state_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    fn render_with(cx: &mut TestAppContext, configure: impl FnOnce(&mut LoomView)) {
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                configure(&mut view);
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn settings_sections_and_dialogs_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for section in [
            SettingsSection::Agents,
            SettingsSection::Providers,
            SettingsSection::Workers,
            SettingsSection::Appearance,
        ] {
            render_with(cx, move |view| {
                view.settings_open = true;
                view.settings_section = section;
            });
        }
        render_with(cx, |view| {
            view.settings_open = true;
            view.settings_section = SettingsSection::Providers;
        });
        render_with(cx, |view| {
            view.settings_open = true;
            view.settings_section = SettingsSection::About;
        });
        render_with(cx, |view| {
            view.command_palette_open = true;
        });
        render_with(cx, |view| {
            view.review.open = true;
        });
        render_with(cx, |view| {
            view.github_login = Some(GitHubLoginState::Starting);
        });
        render_with(cx, |view| {
            view.github_login = Some(GitHubLoginState::Awaiting {
                verification_uri: "https://github.com/login/device".to_owned(),
                user_code: "ABCD-1234".to_owned(),
                expires_in: 900,
            });
        });
        render_with(cx, |view| {
            view.github_login = Some(GitHubLoginState::Error("failed".to_owned()));
        });
        render_with(cx, |view| {
            view.github_login = Some(GitHubLoginState::Success);
        });
    }
}

#[cfg(test)]
mod lifecycle_source_action_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn lifecycle_source_and_worker_actions(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let _ = view.refresh_sessions();
            view.reload_sessions(cx);
            view.reset_projection();
            view.update_session_list();
            view.refresh_models();
            view.apply_models(vec![ModelId::new("deterministic/demo")]);
            view.schedule_project_poll(cx);
            view.schedule_run_poll(cx);
            view.schedule_worker_node_poll(cx);
            view.poll_run_once(cx);

            view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
            view.choose_source(SessionSourceChoice::LocalDirectory, cx);
            view.choose_source(SessionSourceChoice::GitHub, cx);
            view.choose_source(SessionSourceChoice::Empty, cx);
            view.confirm_source_dialog(cx);
            view.set_worker_node_connection_failure(999, "wss://none", "detail".to_owned());
            view.add_source_to_active_session(
                SessionCreationSource::LocalDirectory("/tmp".to_owned()),
                cx,
            );
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod review_project_action_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn review_and_project_child_actions(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.refresh_review(cx);
            view.open_review_file("src/lib.rs".to_owned(), cx);
            view.open_review_diff("src/lib.rs".to_owned(), false, cx);
            view.jump_review_hunk(true, cx);
            view.jump_review_hunk(false, cx);
            let manager = AgentSessionId::new();
            let project = loom_core::ProjectId::new();
            let task = loom_core::TaskId::new();
            view.integrate_project_child_from_ui(
                manager,
                project,
                task,
                "parent".to_owned(),
                "child".to_owned(),
                cx,
            );
            view.cleanup_project_child_from_ui(
                manager,
                project,
                task,
                loom_core::ProjectWorktreeCleanupDisposition::RemoveClean,
                cx,
            );
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod worker_failure_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};
    use loom_core::CapabilitySet;

    fn remote_node(id: u64, url: &str, state: WorkerConnectionState) -> WorkerNodeEntry {
        WorkerNodeEntry {
            id,
            status: WorkerNodeStatus {
                node_id: format!("worker-{id}"),
                name: format!("Worker {id}"),
                online: false,
                capabilities: CapabilitySet::default(),
                resources: WorkerNodeResources {
                    cpu_count: 2,
                    cpu_usage_percent: Some(50),
                    memory_usage_percent: Some(75),
                    memory_total_bytes: Some(8),
                    memory_available_bytes: Some(2),
                    disk_total_bytes: None,
                    disk_available_bytes: None,
                },
            },
            is_local: false,
            url: Some(url.to_owned()),
            connection: None,
            connection_state: state,
            connection_detail: None,
            severe_load_streak: 0,
        }
    }

    #[gpui_kit::test]
    fn worker_failures_and_removal_update_nodes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.worker_nodes.push(remote_node(
                1,
                "wss://one.example/ws",
                WorkerConnectionState::Connecting,
            ));
            view.worker_nodes.push(remote_node(
                2,
                "wss://two.example/ws",
                WorkerConnectionState::Connected,
            ));
            view.set_worker_node_connection_failure(1, "wss://one.example/ws", "boom".to_owned());
            assert_eq!(
                view.worker_nodes[0].connection_state,
                WorkerConnectionState::Failed
            );
            let error = loom_core::LoomError::new(
                loom_core::ErrorCode::ProviderUnavailable,
                "connection refused",
                true,
            );
            view.fail_worker_node_connection(
                2,
                "wss://two.example/ws",
                WorkerConnectionStage::Transport,
                &error,
                Some("secret-token"),
                false,
            );
            assert!(view.worker_nodes[1].connection_detail.is_some());
            view.remove_worker_node(1, cx);
            assert!(view.worker_nodes.iter().all(|node| node.id != 1));
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod provider_mode_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    #[gpui_kit::test]
    fn agent_modes_and_provider_selection(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.open_providers_for_node("test-node".to_owned(), cx);
            view.configure_api_key_provider(loom_model::ProviderId::new("openai"), window, cx);
            view.providers_node_id = Some("test-node".to_owned());
            view.copy_github_login_value("ABCD".to_owned(), "device code", cx);

            for mode in [
                AgentMode::Ask,
                AgentMode::Edit,
                AgentMode::Agent,
                AgentMode::AutoApprove,
            ] {
                view.select_agent_mode(mode, cx);
                view.sync_agent_mode_select_state(window, cx);
            }
            view.toggle_auto_approve_actions(cx);
            view.toggle_auto_approve_actions(cx);
            view.sync_model_select_states(window, cx);
            view.select_model(ModelId::new("deterministic/demo"), cx);
            view.select_default_model(ModelId::new("deterministic/demo"), cx);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod provider_error_tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    use super::*;

    #[gpui_kit::test]
    fn github_login_error_paths(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.providers_node_id = Some("test-node".to_owned());
            let error = loom_core::LoomError::new(
                loom_core::ErrorCode::ProviderAuthentication,
                "denied",
                false,
            );
            view.handle_github_device_code(Err(error), cx);
            assert!(matches!(
                view.github_login,
                Some(GitHubLoginState::Error(_))
            ));
            view.finish_github_login(
                Err(loom_core::LoomError::new(
                    loom_core::ErrorCode::ProviderAuthentication,
                    "denied",
                    false,
                )),
                cx,
            );
            view.persist_and_distribute_workspace_config(None, cx);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod session_load_tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    use super::*;

    #[gpui_kit::test]
    fn async_session_load_and_creation_paths(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session = empty_session_snapshot(view.workspace_id);
            view.activate_session(session.clone());
            view.create_session_on_node_with_source(
                "test-node".to_owned(),
                "New session".to_owned(),
                Some(SessionCreationSource::LocalDirectory("/tmp".to_owned())),
                cx,
            );
            let session_id = view.active_session.id;
            let failure = loom_protocol::ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::new(loom_core::ErrorCode::Internal, "stale", false),
            );
            view.finish_async_session_load(session_id, failure.clone(), failure, cx);
            view.begin_transcript_page(Some(5), cx);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod transcript_action_tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, px, size};

    use super::*;

    #[gpui_kit::test]
    fn transcript_pages_and_messages_update_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let run_id = loom_core::RunId::new();
            view.active_run_id = Some(run_id);
            view.apply_transcript_page(
                run_id,
                None,
                vec![
                    (
                        0,
                        1,
                        ModelMessage {
                            role: MessageRole::User,
                            content: "hello".to_owned(),
                            name: None,
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        },
                    ),
                    (
                        1,
                        2,
                        ModelMessage {
                            role: MessageRole::Assistant,
                            content: "hi".to_owned(),
                            name: None,
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        },
                    ),
                ],
                Some(0),
                true,
            );
            view.apply_transcript_page(run_id, Some(0), Vec::new(), None, false);
            view.ensure_session_task_message(AgentSessionId::new());
            view.ensure_session_task_message(view.active_session.id);
            view.send_message("follow up".to_owned(), cx);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

#[cfg(test)]
mod local_source_path_tests {
    use super::resolve_local_source_path;
    use std::path::Path;

    #[test]
    fn explicit_absolute_entry_wins_over_the_current_directory() {
        assert_eq!(
            resolve_local_source_path("/typed/project", Some(Path::new("/current/project"))),
            Some("/typed/project".to_owned())
        );
    }

    #[test]
    fn empty_entry_falls_back_to_the_current_directory() {
        assert_eq!(
            resolve_local_source_path("   ", Some(Path::new("/current/project"))),
            Some("/current/project".to_owned())
        );
    }

    #[test]
    fn relative_entries_are_rejected() {
        assert_eq!(
            resolve_local_source_path("relative/project", Some(Path::new("/current/project"))),
            None
        );
    }

    #[test]
    fn empty_entry_without_a_current_directory_is_rejected() {
        assert_eq!(resolve_local_source_path("", None), None);
    }
}
