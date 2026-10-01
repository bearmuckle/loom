//! In-process tests: filesystem.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn github_repository_access_configures_the_account_token() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    let before = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::GetGitHubRepositoryAccess,
    )));
    assert!(matches!(
        before.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryAccess { connected: false }
        ))
    ));

    let configured = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ConfigureGitHubRepository {
            access_token: "gho_repository_secret".to_owned(),
        },
    )));
    assert!(matches!(
        configured.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));
    assert_eq!(
        backend
            .provider_registry()
            .github_repository_token()
            .unwrap(),
        "gho_repository_secret"
    );
    assert_eq!(
        backend.provider_registry().github_account_token().unwrap(),
        "gho_repository_secret"
    );

    let after = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::GetGitHubRepositoryAccess,
    )));
    assert!(matches!(
        after.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryAccess { connected: true }
        ))
    ));
}

#[test]
fn github_repository_login_status_reports_progress() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    let now = std::time::Instant::now();
    backend
        .credentials
        .begin_pending(
            "repository-login".to_owned(),
            now,
            Duration::from_secs(300),
            8,
            now + Duration::from_secs(600),
        )
        .unwrap();

    let status_request = |login_id: &str| {
        connection.request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::GetGitHubRepositoryLoginStatus {
                login_id: login_id.to_owned(),
            },
        )))
    };
    let pending = status_request("repository-login");
    assert!(matches!(
        pending.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryLoginStatus {
                status: GitHubCopilotLoginStatus::Pending
            }
        ))
    ));

    backend
        .credentials
        .finish("repository-login", GitHubCopilotLoginStatus::Configured);
    let configured = status_request("repository-login");
    assert!(matches!(
        configured.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::GitHubRepositoryLoginStatus {
                status: GitHubCopilotLoginStatus::Configured
            }
        ))
    ));

    assert!(status_request("missing-login").result.is_err());
}

#[test]
fn filesystem_change_response_detects_pruned_client_cursors() {
    let session_id = AgentSessionId::new();
    let changes = vec![SessionFilesystemChange {
        sequence: EventSequence::new(5),
        session_id,
        path: "src/main.rs".to_owned(),
        kind: loom_protocol::WorkspaceChangeKind::Modified,
        revision: Some("revision".to_owned()),
    }];
    assert!(filesystem_history_pruned(
        Some(EventSequence::new(1)),
        &changes
    ));
    assert!(!filesystem_history_pruned(
        Some(EventSequence::new(4)),
        &changes
    ));
    assert!(!filesystem_history_pruned(None, &changes));
}

#[test]
fn github_repository_search_filters_accessible_repositories_by_rank() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // `GET /user/repos` returns a bare array rather than a search wrapper.
        let repositories = serde_json::json!([
            github_repository_json("zed/loom"),
            github_repository_json("aaa/loom"),
            github_repository_json("owner/loom-extra"),
            github_repository_json("owner/preloom"),
            github_repository_json("loom-owner/thing"),
            serde_json::json!({
                "full_name": "acme/tools",
                "description": "Loom build tool",
                "clone_url": "https://github.com/acme/tools.git",
                "private": false,
                "default_branch": "main"
            }),
            github_repository_json("acme/unrelated"),
            github_repository_json("zed/loom"),
        ]);
        respond_http(stream, "200 OK", &repositories.to_string());
    });

    let repositories = search_github_repositories_at(
        "fixture-token",
        "loom",
        &format!("http://{address}/user/repos"),
    )
    .unwrap();
    server.join().unwrap();

    // Exact names first, then prefix, substring, owner, and finally a
    // description-only match. Duplicates are dropped.
    let names = repositories
        .iter()
        .map(|repository| repository.full_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "aaa/loom",
            "zed/loom",
            "owner/loom-extra",
            "owner/preloom",
            "loom-owner/thing",
            "acme/tools",
        ]
    );
    assert_eq!(
        repositories.first().unwrap().clone_url,
        "https://github.com/aaa/loom.git"
    );
    assert_eq!(repositories.first().unwrap().default_branch, "main");
}

#[test]
fn github_search_needle_normalizes_references_and_urls() {
    use crate::util::github_search_needle;

    assert_eq!(github_search_needle("loom"), "loom");
    assert_eq!(github_search_needle("  Loom  "), "loom");
    assert_eq!(
        github_search_needle("flatgeobuf/flatgeobuf"),
        "flatgeobuf/flatgeobuf"
    );
    assert_eq!(
        github_search_needle("https://github.com/flatgeobuf/flatgeobuf.git"),
        "flatgeobuf/flatgeobuf"
    );
    assert_eq!(
        github_search_needle("https://github.com/flatgeobuf/flatgeobuf/tree/main"),
        "flatgeobuf/flatgeobuf"
    );
    assert_eq!(
        github_search_needle("git@github.com:flatgeobuf/flatgeobuf.git"),
        "flatgeobuf/flatgeobuf"
    );
    assert_eq!(github_search_needle("flatgeobuf/"), "flatgeobuf/");
}

#[test]
fn github_repository_rank_prefers_name_over_description() {
    use crate::util::github_repository_rank;

    let repository = |full_name: &str, description: Option<&str>| crate::GitHubApiRepository {
        full_name: full_name.to_owned(),
        description: description.map(str::to_owned),
        clone_url: format!("https://github.com/{full_name}.git"),
        private: false,
        default_branch: "main".to_owned(),
    };

    // An exact `owner/name` outranks a bare name, which outranks a prefix, a
    // substring, an owner match, and finally a description-only match.
    assert_eq!(
        github_repository_rank(&repository("owner/loom", None), "owner/loom"),
        Some(0)
    );
    assert_eq!(
        github_repository_rank(&repository("owner/loom", None), "loom"),
        Some(1)
    );
    assert_eq!(
        github_repository_rank(&repository("owner/loom-extra", None), "loom"),
        Some(2)
    );
    assert_eq!(
        github_repository_rank(&repository("owner/preloom", None), "loom"),
        Some(3)
    );
    assert_eq!(
        github_repository_rank(&repository("loom/other", None), "loom"),
        Some(4)
    );
    assert_eq!(
        github_repository_rank(&repository("owner/other", Some("Loom tool")), "loom"),
        Some(5)
    );
    assert_eq!(
        github_repository_rank(&repository("owner/other", Some("Unrelated")), "loom"),
        None
    );
}

#[test]
fn github_repository_search_requires_two_characters_before_any_request() {
    for query in ["", " ", "a"] {
        let error =
            search_github_repositories_at("fixture-token", query, "http://127.0.0.1:1/user/repos")
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
}

#[test]
fn importing_a_local_git_directory_attaches_it_as_a_repository() {
    let source = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Import".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("expected workspace creation");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Import".to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) = created.result
    else {
        panic!("expected session creation");
    };

    let imported = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ImportSessionDirectory {
            session_id: session.id,
            source: source.display().to_string(),
            path: "imported".to_owned(),
        },
    )));
    assert!(matches!(
        imported.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryImported {
                repository: Some(_),
                ..
            }
        ))
    ));

    let _ = fs::remove_dir_all(&source);
}

#[test]
fn attach_can_reuse_a_cached_github_clone_without_the_network() {
    let source = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);

    // Seed the node cache with a mirror of a GitHub-shaped repository.
    let url = "https://github.com/owner/cached.git";
    let mirror = backend.repository_mirror_path(url).unwrap();
    GitService::create_mirror(&source, &mirror, url).unwrap();
    backend
        .register_cloned_repository(&ClonedRepository {
            full_name: "owner/cached".to_owned(),
            clone_url: url.to_owned(),
            branch: Some("main".to_owned()),
            last_used_at: Timestamp::from_unix_millis(1),
        })
        .unwrap();
    assert_eq!(backend.cached_repositories().unwrap().len(), 1);

    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Reuse".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("expected workspace creation");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Reuse".to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) = created.result
    else {
        panic!("expected session creation");
    };

    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id: session.id,
            source: url.to_owned(),
            path: "repo".to_owned(),
            revision: None,
            reuse_local: true,
        },
    )));
    let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository))) =
        attached.result
    else {
        panic!("expected cached attachment: {:?}", attached.result.err());
    };
    assert_eq!(repository.source, "cached");

    // A checkout cloned from the node mirror still points `origin` at GitHub.
    let filesystem = backend.restore_session_filesystem(session.id).unwrap();
    let checkout = filesystem.directory_path(&repository.path).unwrap();
    let config = fs::read_to_string(checkout.join(".git/config")).unwrap();
    assert!(
        config.contains(url),
        "cached checkout origin should be the GitHub URL: {config}"
    );

    let _ = fs::remove_dir_all(&source);
}

#[test]
fn github_repository_search_reports_transport_failures() {
    let error =
        search_github_repositories_at("fixture-token", "loom", "http://127.0.0.1:1/user/repos")
            .unwrap_err();
    assert_eq!(error.code, ErrorCode::ProviderAuthentication);
    assert!(error.retryable);
}

#[test]
fn github_repository_search_normalizes_transport_and_payload_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (invalid_json, _) = listener.accept().unwrap();
        respond_http(invalid_json, "200 OK", "not-json");
        let (unauthorized, _) = listener.accept().unwrap();
        respond_http(unauthorized, "401 Unauthorized", "{}");
    });

    let endpoint = format!("http://{address}/user/repos");
    let malformed = search_github_repositories_at("fixture-token", "loom", &endpoint).unwrap_err();
    assert_eq!(malformed.code, ErrorCode::ProviderInvalidResponse);
    let unauthorized =
        search_github_repositories_at("fixture-token", "loom", &endpoint).unwrap_err();
    assert_eq!(unauthorized.code, ErrorCode::ProviderAuthentication);
    assert!(unauthorized.retryable);
    server.join().unwrap();
}

#[test]
fn filesystem_and_repository_helpers_reject_unsafe_inputs_and_copy_trees() {
    assert_eq!(
        checked_session_relative_path("nested/file.txt").unwrap(),
        PathBuf::from("nested/file.txt")
    );
    for invalid in [
        "",
        "  ",
        ".",
        "..",
        "../secret",
        "/absolute",
        "nested\\file",
    ] {
        assert!(
            checked_session_relative_path(invalid).is_err(),
            "{invalid:?}"
        );
    }

    for (url, safe) in [
        ("wss://worker.example/ws", true),
        ("ws://localhost:9000/", true),
        ("https://worker.example/ws", false),
        ("wss://", false),
        ("wss://user@worker.example/ws", false),
        ("wss://user:secret@worker.example/ws", false),
        ("wss://worker.example/ws#fragment", false),
        ("wss://worker.example/ws?access_TOKEN=secret", false),
    ] {
        assert_eq!(worker_node_url_is_safe(url), safe, "{url}");
    }

    assert_eq!(
        repository_display_name("https://github.com/owner/project.git").unwrap(),
        "project"
    );
    assert_eq!(
        repository_display_name("ssh://git@github.com/owner/project.git").unwrap(),
        "project"
    );
    for unsafe_source in [
        "relative/path",
        "http://github.com/owner/project",
        "https://user:secret@github.com/owner/project",
        "https://github.com/owner/project?access_token=secret",
    ] {
        assert!(
            repository_display_name(unsafe_source).is_err(),
            "{unsafe_source}"
        );
    }

    let source = workspace();
    let destination = workspace();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/file.txt"), "copy me").unwrap();
    copy_filesystem_tree(&source, &destination).unwrap();
    assert_eq!(
        fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
        "copy me"
    );
    assert_eq!(
        checked_session_path(&destination, "nested/file.txt").unwrap(),
        fs::canonicalize(destination.join("nested/file.txt")).unwrap()
    );
    assert!(checked_session_path(&destination, "../outside").is_err());
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(destination).unwrap();

    let repository = git_repository();
    assert_eq!(
        repository_display_name(repository.to_str().unwrap()).unwrap(),
        repository.file_name().unwrap().to_string_lossy()
    );
    fs::remove_dir_all(repository).unwrap();
}

#[test]
fn local_directory_import_copies_tree_and_rejects_unsafe_sources() {
    let source = workspace();
    let target_root = workspace();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/file.txt"), "copied content").unwrap();
    let destination = target_root.join("imported");
    copy_directory_contents(&source, &destination).unwrap();
    assert_eq!(
        fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
        "copied content"
    );
    assert_eq!(
        copy_directory_contents(&source, &destination)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );

    let inside_source = source.join("session/imported");
    assert_eq!(
        copy_directory_contents(&source, &inside_source)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    let file_source = source.join("nested/file.txt");
    assert_eq!(
        copy_directory_contents(&file_source, &target_root.join("file"))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        copy_directory_contents(&source.join("missing"), &target_root.join("missing"))
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(target_root).unwrap();
}

#[cfg(unix)]
#[test]
fn local_directory_import_rejects_symlinks_that_escape_source() {
    use std::os::unix::fs::symlink;

    let source = workspace();
    let outside = workspace();
    let destination_root = workspace();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    symlink(outside.join("secret.txt"), source.join("escape")).unwrap();
    assert_eq!(
        copy_directory_contents(&source, &destination_root.join("import"))
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    assert_eq!(fs::read_dir(&destination_root).unwrap().count(), 0);
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(outside).unwrap();
    fs::remove_dir_all(destination_root).unwrap();
}

#[cfg(unix)]
#[test]
fn filesystem_copy_preserves_symlinks_and_checked_paths_reject_escape() {
    use std::os::unix::fs::symlink;

    let source = workspace();
    let destination = workspace();
    let outside = workspace();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    symlink(outside.join("secret.txt"), source.join("outside-link")).unwrap();
    copy_filesystem_tree(&source, &destination).unwrap();
    assert!(
        fs::symlink_metadata(destination.join("outside-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(checked_session_path(&destination, "outside-link").is_err());

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(destination).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[cfg(unix)]
#[test]
fn attaching_local_directory_uses_original_and_discovers_immediate_repositories() {
    let source = workspace();
    fs::write(source.join("note.txt"), "original").unwrap();
    fs::rename(git_repository(), source.join("child-repo")).unwrap();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Local source".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("expected workspace creation");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Local source".to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) = created.result
    else {
        panic!("expected session creation");
    };
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::AttachSessionDirectory {
            session_id: session.id,
            source: source.display().to_string(),
            path: "sources/local".to_owned(),
        },
    )));
    let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
        directory,
        repositories,
    })) = attached.result
    else {
        panic!("expected directory attachment: {:?}", attached.result);
    };
    assert_eq!(directory.source, source.display().to_string());
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].path, "sources/local/child-repo");
    let edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: session.id,
            edit: WorkspaceEdit {
                path: "sources/local/note.txt".to_owned(),
                old_text: "original".to_owned(),
                new_text: "changed".to_owned(),
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
    assert_eq!(
        fs::read_to_string(source.join("note.txt")).unwrap(),
        "changed"
    );
    let detached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::DetachSessionDirectory {
            session_id: session.id,
            path: directory.path,
        },
    )));
    assert!(matches!(
        detached.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryDetached
        ))
    ));
    assert!(source.join("child-repo/.git").exists());
    let repository_root = git_repository();
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::AttachSessionDirectory {
            session_id: session.id,
            source: repository_root.display().to_string(),
            path: "sources/repo-root".to_owned(),
        },
    )));
    let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
        directory,
        repositories,
    })) = attached.result
    else {
        panic!("expected repository root attachment");
    };
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].path, "sources/repo-root");
    let detached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::DetachSessionDirectory {
            session_id: session.id,
            path: directory.path,
        },
    )));
    assert!(matches!(
        detached.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryDetached
        ))
    ));
    fs::remove_dir_all(repository_root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn workspace_sessions_get_independent_filesystems_and_repository_clones() {
    let source = git_repository();

    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let capabilities = CapabilitySet::new([
        Capability::ManageWorkspaces,
        Capability::ReadAgentSession,
        Capability::CreateAgentSession,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
        Capability::ManageSessionRepositories,
        Capability::ForkAgentSession,
        Capability::ReadVcsStatus,
    ]);
    let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        },
    )));
    assert!(matches!(
        negotiated.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));

    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Isolation test".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        workspace.result
    else {
        panic!("expected workspace creation");
    };
    let create_session = |name: &str| {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: name.to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) =
            response.result
        else {
            panic!("expected session creation");
        };
        session
    };
    let first = create_session("First");
    let second = create_session("Second");
    let attach_repository = |session_id| {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
                reuse_local: false,
            },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(
            repository,
        ))) = response.result
        else {
            panic!("expected repository attachment");
        };
        repository
    };
    let first_repository = attach_repository(first.id);
    let second_repository = attach_repository(second.id);
    assert_ne!(first_repository.id, second_repository.id);

    let edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: first.id,
            edit: WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source".to_owned(),
                new_text: "first session".to_owned(),
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

    let fork = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ForkAgentSession {
            session_id: first.id,
            name: "Forked first".to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionForked(fork))) = fork.result else {
        panic!("expected forked session");
    };
    let repositories = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::ListSessionRepositories {
            session_id: fork.id,
        },
    )));
    let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories { repositories })) =
        repositories.result
    else {
        panic!("expected forked repositories");
    };
    let fork_repository = repositories.first().expect("repository was copied");
    assert_ne!(fork_repository.id, first_repository.id);
    let fork_edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: fork.id,
            edit: WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "first session".to_owned(),
                new_text: "forked session".to_owned(),
                expected_revision: None,
            },
        },
    )));
    assert!(matches!(
        fork_edit.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    for (session_id, expected_content) in
        [(first.id, "first session\n"), (fork.id, "forked session\n")]
    {
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
            file.result
        else {
            panic!("expected session file");
        };
        assert_eq!(file.content, expected_content);
    }

    for (session_id, expected_content, repository_id, expected_clean) in [
        (first.id, "first session\n", first_repository.id, false),
        (second.id, "source\n", second_repository.id, true),
    ] {
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
            file.result
        else {
            panic!("expected session file");
        };
        assert_eq!(file.content, expected_content);

        let status = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(status))) = status.result
        else {
            panic!("expected repository status");
        };
        assert_eq!(status.clean, expected_clean);
    }
    assert_eq!(
        fs::read_to_string(source.join("README.md")).unwrap(),
        "source\n"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn session_filesystem_and_repository_metadata_survive_restart() {
    let source = git_repository();
    let state_dir = workspace();
    let persistence = state_dir.join("backend.sqlite");
    let (workspace_id, session_id, checkpoint_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        let capabilities = CapabilitySet::new([
            Capability::ManageWorkspaces,
            Capability::ReadAgentSession,
            Capability::CreateAgentSession,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
            Capability::ManageCheckpoints,
            Capability::ManageSessionRepositories,
            Capability::ReadVcsStatus,
        ]);
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            },
        )));
        assert!(matches!(
            negotiated.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ));
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Persistent workspace".to_owned(),
            },
        )));
        let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
            created.result
        else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Persistent session".to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) =
            created.result
        else {
            panic!("expected session creation");
        };
        let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: session.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
                reuse_local: false,
            },
        )));
        assert!(
            matches!(
                attached.result,
                Ok(ServerResponse::Repository(
                    RepositoryResponse::SessionRepositoryAttached(_)
                ))
            ),
            "{:?}",
            attached.result
        );
        let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::CreateSessionCheckpoint {
                session_id: session.id,
                label: "before persistent edit".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint))) =
            checkpoint.result
        else {
            panic!("expected persisted checkpoint, got {:?}", checkpoint.result);
        };
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .apply_edit(WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source".to_owned(),
                new_text: "persisted session edit".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .apply_edit(WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "persisted session edit".to_owned(),
                new_text: "temporary edit to undo".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .undo_last_agent_edit()
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&session.id)
            .unwrap()
            .poll_changes()
            .unwrap();
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (workspace.id, session.id, checkpoint.id)
    };

    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        let capabilities = CapabilitySet::new([
            Capability::ReadAgentSession,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
            Capability::ManageCheckpoints,
            Capability::ReadVcsStatus,
        ]);
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            },
        )));
        assert!(matches!(
            negotiated.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ));
        let sessions = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: false,
            },
        )));
        assert!(matches!(
            sessions.result,
            Ok(ServerResponse::Session(SessionResponse::AgentSessions{ sessions }))
                if sessions.iter().any(|session| session.id == session_id)
        ));
        let repositories = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories { session_id },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories,
        })) = repositories.result
        else {
            panic!("expected restored repository metadata");
        };
        let repository = repositories.first().expect("repository was restored");
        let persisted_filesystem = backend
            .persistence
            .as_ref()
            .unwrap()
            .load_filesystem_record(session_id)
            .unwrap()
            .expect("filesystem record remains inspectable");
        assert!(persisted_filesystem.edits.iter().any(|edit| {
            edit.path == "repo/README.md" && edit.before.as_deref() == Some("source\n")
        }));
        assert_eq!(
            persisted_filesystem.edits.len(),
            1,
            "undone edit ID is deleted from durable history"
        );
        assert!(
            backend
                .persistence
                .as_ref()
                .unwrap()
                .load_filesystem_changes_page(session_id, None, 512)
                .unwrap()
                .changes
                .iter()
                .any(|change| {
                    change.path == "repo/README.md"
                        && change.session_id == session_id
                        && change.sequence.value() > 0
                })
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
            file.result
        else {
            panic!("expected restored session file");
        };
        assert_eq!(file.content, "persisted session edit\n");
        assert_eq!(
            persisted_filesystem.checkpoints[0].files["repo/README.md"].expected_revision,
            file.revision
        );
        let reverted = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            },
        )));
        assert!(
            matches!(
                reverted.result,
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::CheckpointReverted(_)
                ))
            ),
            "checkpoint revert failed: {:?}",
            reverted.result
        );
        let restored = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(restored))) =
            restored.result
        else {
            panic!("expected checkpoint file contents after revert");
        };
        assert_eq!(restored.content, "source\n");
        let status = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::GetSessionVcsStatus {
                session_id,
                repository_id: repository.id,
            },
        )));
        let Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(status))) = status.result
        else {
            panic!("expected restored repository status");
        };
        assert!(status.clean);
        backend.shutdown().unwrap();
    }

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
    fs::remove_dir_all(state_dir).unwrap();
}

#[test]
fn forked_session_filesystem_and_policy_survive_restart_and_checkpoint_revert() {
    let source = git_repository();
    let state_dir = workspace();
    let persistence = state_dir.join("backend.sqlite");
    let (workspace_id, source_session_id, fork_session_id, checkpoint_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Persistent fork workspace".to_owned(),
            },
        )));
        let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
            created.result
        else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Persistent source".to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) =
            created.result
        else {
            panic!("expected source session creation");
        };
        let policy = ApprovalPolicy::auto_approve();
        let configured = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: policy.clone(),
                auto_approve_actions: Some(true),
            },
        )));
        assert!(matches!(
            configured.result,
            Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(configured))) if configured == policy
        ));
        let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: session.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
                reuse_local: false,
            },
        )));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::Repository(
                RepositoryResponse::SessionRepositoryAttached(_)
            ))
        ));
        let edited = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ApplySessionFilesystemEdit {
                session_id: session.id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "source branch".to_owned(),
                    expected_revision: None,
                },
            },
        )));
        assert!(matches!(
            edited.result,
            Ok(ServerResponse::Filesystem(
                FilesystemResponse::WorkspaceEditApplied(_)
            ))
        ));
        let forked = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ForkAgentSession {
                session_id: session.id,
                name: "Persistent fork".to_owned(),
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionForked(forked))) =
            forked.result
        else {
            panic!("expected fork creation: {:?}", forked.result);
        };
        let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::CreateSessionCheckpoint {
                session_id: forked.id,
                label: "fork baseline".to_owned(),
            },
        )));
        let Ok(ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint))) =
            checkpoint.result
        else {
            panic!("expected fork checkpoint: {:?}", checkpoint.result);
        };
        backend
            .session_filesystems()
            .unwrap()
            .get(&forked.id)
            .unwrap()
            .apply_edit(WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source branch".to_owned(),
                new_text: "fork-only change".to_owned(),
                expected_revision: None,
            })
            .unwrap();
        backend
            .session_filesystems()
            .unwrap()
            .get(&forked.id)
            .unwrap()
            .poll_changes()
            .unwrap();
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (workspace.id, session.id, forked.id, checkpoint.id)
    };

    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let sessions = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: false,
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessions { sessions })) =
            sessions.result
        else {
            panic!("expected restored sessions");
        };
        assert!(
            sessions
                .iter()
                .any(|session| session.id == source_session_id)
        );
        assert!(sessions.iter().any(|session| session.id == fork_session_id));

        let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot {
                session_id: fork_session_id,
            },
        )));
        let Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot))) =
            snapshot.result
        else {
            panic!("expected restored fork snapshot");
        };
        assert!(snapshot.auto_approve_actions);
        assert_eq!(snapshot.approval_policy, ApprovalPolicy::auto_approve());

        let source_repositories = connection.request(RequestEnvelope::new(
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                session_id: source_session_id,
            }),
        ));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories: source_repositories,
        })) = source_repositories.result
        else {
            panic!("expected restored source repository metadata");
        };
        let fork_repositories = connection.request(RequestEnvelope::new(
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                session_id: fork_session_id,
            }),
        ));
        let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories: fork_repositories,
        })) = fork_repositories.result
        else {
            panic!("expected restored fork repository metadata");
        };
        assert_ne!(
            source_repositories.first().unwrap().id,
            fork_repositories.first().unwrap().id
        );

        let read_file = |session_id| {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::ReadSessionFile {
                    session_id,
                    path: "repo/README.md".to_owned(),
                },
            )));
            let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) =
                response.result
            else {
                panic!("expected restored session file: {:?}", response.result);
            };
            file.content
        };
        assert_eq!(read_file(source_session_id), "source branch\n");
        assert_eq!(read_file(fork_session_id), "fork-only change\n");

        let events = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(fork_session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        let Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. })) =
            events.result
        else {
            panic!("expected restored fork event stream");
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                loom_protocol::ServerEvent::AgentSessionForked {
                    source_session_id: source_id,
                    snapshot,
                } if *source_id == source_session_id && snapshot.id == fork_session_id
            )
        }));

        let reverted = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::RevertSessionCheckpoint {
                session_id: fork_session_id,
                checkpoint_id,
            },
        )));
        assert!(
            matches!(
                reverted.result,
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::CheckpointReverted(_)
                ))
            ),
            "fork checkpoint revert failed: {:?}",
            reverted.result
        );
        assert_eq!(read_file(fork_session_id), "source branch\n");
        assert_eq!(read_file(source_session_id), "source branch\n");
        backend.shutdown().unwrap();
    }

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
    fs::remove_dir_all(state_dir).unwrap();
}

#[test]
fn persisted_session_filesystems_restore_lazily_and_survive_unrelated_writes() {
    let persistence =
        std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
    let session_root_base;
    let (workspace_id, session_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Lazy restore workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            workspace.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Archived history".to_owned(),
            },
        )));
        let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
            session.result.unwrap()
        else {
            panic!("unexpected session response");
        };
        let root = session_root_base
            .join(workspace.id.to_string())
            .join(session.id.to_string())
            .join("fs");
        fs::write(root.join("retained.txt"), "retained content\n").unwrap();
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (workspace.id, session.id)
    };

    let filesystem_root = session_root_base
        .join(workspace_id.to_string())
        .join(session_id.to_string())
        .join("fs");
    let parked_root = filesystem_root.with_extension("parked");
    fs::rename(&filesystem_root, &parked_root).unwrap();
    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id,
                name: "Still lazy".to_owned(),
            },
        )));
        assert!(matches!(
            renamed.result,
            Ok(ServerResponse::Session(
                SessionResponse::AgentSessionRenamed(_)
            ))
        ));
        backend.shutdown().unwrap();
    }
    fs::rename(&parked_root, &filesystem_root).unwrap();
    {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: "retained.txt".to_owned(),
            },
        )));
        let ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file)) =
            file.result.unwrap()
        else {
            panic!("unexpected filesystem response");
        };
        assert_eq!(file.content, "retained content\n");
        backend.shutdown().unwrap();
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }
    let _ = fs::remove_file(&persistence);
}

#[test]
fn protocol_filesystem_requests_cover_snapshots_edits_checkpoints_and_undo() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Filesystem protocol".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Filesystem session".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };

    let created = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            edit: WorkspaceEdit {
                path: "notes/plan.md".to_owned(),
                old_text: String::new(),
                new_text: "first version".to_owned(),
                expected_revision: None,
            },
        },
    )));
    assert!(matches!(
        created.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    let checkpoint_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::CreateSessionCheckpoint {
                session_id,
                label: "before update".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint)) => {
            checkpoint.id
        }
        response => panic!("unexpected checkpoint response: {response:?}"),
    };
    let read = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ReadSessionFile {
            session_id,
            path: "notes/plan.md".to_owned(),
        },
    )));
    let revision = match read.result.unwrap() {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file)) => {
            assert_eq!(file.content, "first version");
            file.revision
        }
        response => panic!("unexpected file response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::RevertSessionCheckpoint {
                    session_id,
                    checkpoint_id,
                }
            ),))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::CheckpointReverted(_)
        ))
    ));
    let updated = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            edit: WorkspaceEdit {
                path: "notes/plan.md".to_owned(),
                old_text: "first".to_owned(),
                new_text: "second".to_owned(),
                expected_revision: Some(revision),
            },
        },
    )));
    assert!(matches!(
        updated.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    let undo = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::UndoSessionEdit { session_id },
    )));
    assert_eq!(undo.result.unwrap_err().code, ErrorCode::InvalidState);
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::GetSessionFilesystemSnapshot { session_id }
            ),))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemSnapshot(_)
        ))
    ));
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::GetSessionFilesystemChanges {
                    session_id,
                    after_sequence: None,
                }
            ),))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemChanges { .. }
        ))
    ));
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::GetSessionContextFiles { session_id }
            )))
            .result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::ContextFiles { .. }
        ))
    ));
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}
