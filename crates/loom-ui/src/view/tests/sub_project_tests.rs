//! The project header's two shortcuts: branching a sub project from the project
//! on screen, and archiving that project.

#[allow(unused_imports)]
use super::*;

#[cfg(not(target_family = "wasm"))]
mod header_shortcuts {
    use super::super::loom_view_render_tests::nested_project_view;
    use super::*;
    use crate::backend_host::{BackendWorker, Job};
    use gpui_kit::TestAppContext;
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{px, size};
    use loom_core::{ProjectId, SessionRepository};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A view over one project with the agent tree `nested_project_view` builds,
    /// whose worker answers nothing until the test replies.
    fn setup(cx: &mut TestAppContext) -> (Entity<LoomView>, std::sync::mpsc::Receiver<Job>) {
        cx.update(gpui_kit::init);
        let (backend, requests) = BackendWorker::deferred_for_test();
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            let node_id = view.default_backend_node_id.clone();
            view.node_backends.insert(node_id, backend);
            view
        });
        (view, requests)
    }

    /// A window over the same fixture, which the sub project dialog needs: its
    /// input is created by the frame that renders it.
    #[allow(clippy::type_complexity)]
    fn open_view_window(
        cx: &mut TestAppContext,
        backend: BackendWorker,
    ) -> (gpui_kit::WindowHandle<Root>, Entity<LoomView>) {
        cx.update(gpui_kit::init);
        let rendered = Rc::new(RefCell::new(None));
        let rendered_for_window = rendered.clone();
        let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
            let view = cx.new(|cx| {
                let mut view = nested_project_view(cx.focus_handle());
                let node_id = view.default_backend_node_id.clone();
                view.node_backends.insert(node_id, backend);
                view
            });
            *rendered_for_window.borrow_mut() = Some(view.clone());
            Root::new(view, window, cx)
        });
        let view = rendered
            .borrow_mut()
            .take()
            .expect("the window built the view");
        (handle, view)
    }

    fn reply(job: Job, response: ServerResponse) {
        job.reply
            .send(ResponseEnvelope::success(job.request.request_id, response))
            .unwrap();
    }

    fn refuse(job: Job) {
        let _ = job.reply.send(ResponseEnvelope::failure(
            job.request.request_id,
            LoomError::new(ErrorCode::NotFound, "not part of this test", false),
        ));
    }

    /// A header action acts on the project on screen, not on the agent selected
    /// inside it.
    #[gpui_kit::test]
    fn the_header_targets_the_project_that_owns_the_active_session(cx: &mut TestAppContext) {
        let (view, _requests) = setup(cx);
        let root = view.read_with(cx, |view, _| {
            let root = view
                .project_snapshot
                .as_ref()
                .expect("the fixture is a project")
                .root_session_id;
            assert_eq!(view.header_project_session_id(), root);
            assert!(view.has_header_project());
            root
        });

        // Selecting a delegated agent keeps the project as the target.
        view.update(cx, |view, cx| {
            view.active_session = view.sessions[1].clone();
            cx.notify();
        });
        view.read_with(cx, |view, _| {
            assert_ne!(view.active_session.id, root);
            assert_eq!(view.header_project_session_id(), root);
        });
    }

    /// The archive shortcut archives the project root, so the whole tree goes
    /// even while an agent of it is selected.
    #[gpui_kit::test]
    fn the_archive_shortcut_archives_the_project_root(cx: &mut TestAppContext) {
        let (view, requests) = setup(cx);
        let root = view.update(cx, |view, cx| {
            let root = view
                .project_snapshot
                .as_ref()
                .expect("the fixture is a project")
                .root_session_id;
            view.active_session = view.sessions[1].clone();
            view.archive_header_project(cx);
            root
        });
        let job = requests.try_recv().unwrap();
        assert!(
            matches!(
                job.request.request,
                ClientRequest::Session(SessionRequest::ArchiveAgentSession { session_id })
                    if session_id == root
            ),
            "the archive targets the project root"
        );
        refuse(job);
    }

    /// The sub project shortcut names a branch of the project on screen: the
    /// worker creates the project, and the client attaches the parent's sources
    /// to it so the branch starts where the project is.
    #[gpui_kit::test]
    fn the_sub_project_shortcut_branches_the_project_on_screen(cx: &mut TestAppContext) {
        let (backend, requests) = BackendWorker::deferred_for_test();
        let (handle, view) = open_view_window(cx, backend);
        let root = view.update(cx, |view, cx| {
            let root = view
                .project_snapshot
                .as_ref()
                .expect("the fixture is a project")
                .root_session_id;
            view.session_models
                .insert(root, ModelId::new("deterministic/parent"));
            view.begin_sub_project_dialog(cx);
            root
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let input = view
                .read(cx)
                .sub_project_input_state
                .clone()
                .expect("the dialog renders its input");
            input.update(cx, |state, cx| state.set_value("Branch out", window, cx));
        })
        .unwrap();

        let branch_id = AgentSessionId::new();
        let (workspace_id, parent_model) = view.read_with(cx, |view, _| {
            (
                view.sessions[0].workspace_id,
                view.session_models
                    .get(
                        &view
                            .project_snapshot
                            .as_ref()
                            .expect("the fixture is a project")
                            .root_session_id,
                    )
                    .cloned()
                    .expect("the parent has a model"),
            )
        });
        let branch = AgentSessionSnapshot {
            id: branch_id,
            workspace_id,
            name: "Branch out".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(10),
            updated_at: Timestamp::from_unix_millis(10),
            title_source: loom_core::SessionTitleSource::Auto,
        };
        let snapshot = loom_core::ProjectSnapshot {
            project_id: ProjectId::from_uuid(*branch_id.as_uuid()),
            root_session_id: branch_id,
            parent_project_id: Some(ProjectId::from_uuid(*root.as_uuid())),
            agents: vec![loom_core::ProjectAgentRecord {
                session_id: branch_id,
                project_id: ProjectId::from_uuid(*branch_id.as_uuid()),
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(10),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        };
        let repository = SessionRepository {
            id: RepositoryId::new(),
            // A listed record carries the source it was attached from, not its
            // display name, so the branch can attach it again.
            source: "https://github.com/owner/parent.git".to_owned(),
            path: "repositories/parent".to_owned(),
            revision: None,
            attached_at: Timestamp::from_unix_millis(1),
        };

        // Submitting the dialog sends the one request that creates the branch.
        view.update(cx, |view, cx| view.confirm_sub_project(cx));
        assert_eq!(
            view.read_with(cx, |view, _| view.covering_surface()),
            None,
            "the dialog steps aside while the branch is created"
        );
        let mut created = false;
        let mut attached = false;
        for _ in 0..40 {
            let Ok(job) = requests.try_recv() else {
                break;
            };
            match job.request.request.clone() {
                ClientRequest::Project(ProjectRequest::CreateSubProject {
                    parent_session_id,
                    name,
                }) => {
                    assert_eq!(parent_session_id, root);
                    assert_eq!(name, "Branch out");
                    created = true;
                    reply(
                        job,
                        ServerResponse::Project(ProjectResponse::SubProjectCreated {
                            project_id: snapshot.project_id,
                            session: Box::new(branch.clone()),
                        }),
                    );
                }
                ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                    session_id,
                }) if session_id == root => {
                    reply(
                        job,
                        ServerResponse::Repository(RepositoryResponse::SessionRepositories {
                            repositories: vec![repository.clone()],
                        }),
                    );
                }
                ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                    session_id,
                    source,
                    revision,
                    reuse_local,
                    ..
                }) if session_id == branch_id => {
                    assert_eq!(source, repository.source);
                    assert_eq!(
                        revision, None,
                        "the branch clones its own source instead of pinning the parent's commit"
                    );
                    assert!(reuse_local, "the parent's clone is reused");
                    attached = true;
                    reply(
                        job,
                        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(
                            repository.clone(),
                        )),
                    );
                }
                ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories {
                    session_id,
                }) if session_id == root => {
                    reply(
                        job,
                        ServerResponse::Filesystem(FilesystemResponse::SessionDirectories {
                            directories: Vec::new(),
                        }),
                    );
                }
                ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                    session_id,
                }) if session_id == branch_id => {
                    reply(
                        job,
                        ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot.clone())),
                    );
                }
                _ => refuse(job),
            }
            cx.run_until_parked();
        }
        assert!(created, "the shortcut creates the sub project");
        assert!(attached, "the branch inherits the parent's repository");
        view.read_with(cx, |view, _| {
            assert_eq!(view.active_session.id, branch_id, "the branch opens");
            assert!(
                view.sessions.iter().any(|session| session.id == branch_id),
                "the branch joins the session list"
            );
            assert_eq!(view.session_models.get(&branch_id), Some(&parent_model));
            assert!(
                view.project_tree_snapshots.iter().any(|known| {
                    known.root_session_id == branch_id
                        && known.parent_project_id == Some(ProjectId::from_uuid(*root.as_uuid()))
                }),
                "the sidebar learns where the branch came from"
            );
        });
    }

    /// A parent that mounted a folder keeps that folder in its branch: the mount
    /// carries the checkouts inside it, so they are not cloned a second time,
    /// and only a repository outside every mount is attached on its own. Doing
    /// it the other way round collides, because a checkout and a mount cannot
    /// share a session path.
    #[gpui_kit::test]
    fn a_branch_mounts_the_parent_folder_without_cloning_inside_it(cx: &mut TestAppContext) {
        let (backend, requests) = BackendWorker::deferred_for_test();
        let (handle, view) = open_view_window(cx, backend);
        let root = view.update(cx, |view, cx| {
            let root = view
                .project_snapshot
                .as_ref()
                .expect("the fixture is a project")
                .root_session_id;
            view.begin_sub_project_dialog(cx);
            root
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let input = view
                .read(cx)
                .sub_project_input_state
                .clone()
                .expect("the dialog renders its input");
            input.update(cx, |state, cx| state.set_value("Folder branch", window, cx));
        })
        .unwrap();

        let branch_id = AgentSessionId::new();
        let workspace_id = view.read_with(cx, |view, _| view.sessions[0].workspace_id);
        let branch = AgentSessionSnapshot {
            id: branch_id,
            workspace_id,
            name: "Folder branch".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(10),
            updated_at: Timestamp::from_unix_millis(10),
            title_source: loom_core::SessionTitleSource::Auto,
        };
        let snapshot = loom_core::ProjectSnapshot {
            project_id: ProjectId::from_uuid(*branch_id.as_uuid()),
            root_session_id: branch_id,
            parent_project_id: Some(ProjectId::from_uuid(*root.as_uuid())),
            agents: vec![loom_core::ProjectAgentRecord {
                session_id: branch_id,
                project_id: ProjectId::from_uuid(*branch_id.as_uuid()),
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(10),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        };
        // The parent attached a folder, and the worker registered the checkout
        // that folder contains as a repository of its own.
        let mounted = loom_core::SessionDirectory {
            source: "/tmp/loom-folder".to_owned(),
            path: "sources/loom-folder".to_owned(),
        };
        let inside_mount = SessionRepository {
            id: RepositoryId::new(),
            source: "/tmp/loom-folder".to_owned(),
            path: "sources/loom-folder".to_owned(),
            revision: Some("a1b2c3".to_owned()),
            attached_at: Timestamp::from_unix_millis(1),
        };
        let outside_mount = SessionRepository {
            id: RepositoryId::new(),
            source: "https://github.com/owner/other.git".to_owned(),
            path: "repositories/owner/other".to_owned(),
            revision: Some("d4e5f6".to_owned()),
            attached_at: Timestamp::from_unix_millis(1),
        };

        view.update(cx, |view, cx| view.confirm_sub_project(cx));
        let mut mounted_folder = false;
        let mut cloned = Vec::new();
        for _ in 0..40 {
            let Ok(job) = requests.try_recv() else {
                break;
            };
            match job.request.request.clone() {
                ClientRequest::Project(ProjectRequest::CreateSubProject { name, .. }) => {
                    assert_eq!(name, "Folder branch");
                    reply(
                        job,
                        ServerResponse::Project(ProjectResponse::SubProjectCreated {
                            project_id: snapshot.project_id,
                            session: Box::new(branch.clone()),
                        }),
                    );
                }
                ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories {
                    session_id,
                }) if session_id == root => {
                    reply(
                        job,
                        ServerResponse::Filesystem(FilesystemResponse::SessionDirectories {
                            directories: vec![mounted.clone()],
                        }),
                    );
                }
                ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                    session_id,
                    source,
                    path,
                }) if session_id == branch_id => {
                    assert_eq!(source, mounted.source);
                    assert_eq!(path, mounted.path);
                    mounted_folder = true;
                    reply(
                        job,
                        ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
                            directory: mounted.clone(),
                            repositories: vec![inside_mount.clone()],
                        }),
                    );
                }
                ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                    session_id,
                }) if session_id == root => {
                    reply(
                        job,
                        ServerResponse::Repository(RepositoryResponse::SessionRepositories {
                            repositories: vec![inside_mount.clone(), outside_mount.clone()],
                        }),
                    );
                }
                ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                    session_id,
                    path,
                    ..
                }) if session_id == branch_id => {
                    cloned.push(path);
                    reply(
                        job,
                        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(
                            outside_mount.clone(),
                        )),
                    );
                }
                ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                    session_id,
                }) if session_id == branch_id => {
                    reply(
                        job,
                        ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot.clone())),
                    );
                }
                _ => refuse(job),
            }
            cx.run_until_parked();
        }
        assert!(mounted_folder, "the branch mounts the parent's folder");
        assert_eq!(
            cloned,
            vec![outside_mount.path],
            "only the repository outside the mount is cloned for the branch"
        );
    }

    /// A source that cannot be attached is reported, not fatal: the branch stays
    /// listed under its parent and the user can add a source to it later.
    #[gpui_kit::test]
    fn a_branch_stays_listed_when_its_sources_cannot_be_attached(cx: &mut TestAppContext) {
        let (backend, requests) = BackendWorker::deferred_for_test();
        let (handle, view) = open_view_window(cx, backend);
        let root = view.update(cx, |view, cx| {
            let root = view
                .project_snapshot
                .as_ref()
                .expect("the fixture is a project")
                .root_session_id;
            view.begin_sub_project_dialog(cx);
            root
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let input = view
                .read(cx)
                .sub_project_input_state
                .clone()
                .expect("the dialog renders its input");
            input.update(cx, |state, cx| {
                state.set_value("Refused sources", window, cx)
            });
        })
        .unwrap();

        let branch_id = AgentSessionId::new();
        let workspace_id = view.read_with(cx, |view, _| view.sessions[0].workspace_id);
        let branch = AgentSessionSnapshot {
            id: branch_id,
            workspace_id,
            name: "Refused sources".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(10),
            updated_at: Timestamp::from_unix_millis(10),
            title_source: loom_core::SessionTitleSource::Auto,
        };
        let snapshot = loom_core::ProjectSnapshot {
            project_id: ProjectId::from_uuid(*branch_id.as_uuid()),
            root_session_id: branch_id,
            parent_project_id: Some(ProjectId::from_uuid(*root.as_uuid())),
            agents: vec![loom_core::ProjectAgentRecord {
                session_id: branch_id,
                project_id: ProjectId::from_uuid(*branch_id.as_uuid()),
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(10),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        };
        let directory = loom_core::SessionDirectory {
            source: "/tmp/loom-refused".to_owned(),
            path: "sources/loom-refused".to_owned(),
        };

        view.update(cx, |view, cx| view.confirm_sub_project(cx));
        for _ in 0..40 {
            let Ok(job) = requests.try_recv() else {
                break;
            };
            match job.request.request.clone() {
                ClientRequest::Project(ProjectRequest::CreateSubProject { .. }) => reply(
                    job,
                    ServerResponse::Project(ProjectResponse::SubProjectCreated {
                        project_id: snapshot.project_id,
                        session: Box::new(branch.clone()),
                    }),
                ),
                ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories {
                    session_id,
                }) if session_id == root => reply(
                    job,
                    ServerResponse::Filesystem(FilesystemResponse::SessionDirectories {
                        directories: vec![directory.clone()],
                    }),
                ),
                // Every attach fails, and so does the listing that follows a
                // failed directory attach.
                ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                    session_id,
                    ..
                }) if session_id == branch_id => refuse(job),
                ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                    session_id,
                }) if session_id == root => refuse(job),
                ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                    session_id,
                }) if session_id == branch_id => reply(
                    job,
                    ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot.clone())),
                ),
                _ => refuse(job),
            }
            cx.run_until_parked();
        }
        view.read_with(cx, |view, _| {
            assert_eq!(view.active_session.id, branch_id, "the branch still opens");
            assert!(
                view.sessions.iter().any(|session| session.id == branch_id),
                "a source failure must not drop the branch from the list"
            );
            assert!(
                view.project_tree_snapshots.iter().any(|known| {
                    known.root_session_id == branch_id
                        && known.parent_project_id == Some(ProjectId::from_uuid(*root.as_uuid()))
                }),
                "the branch is still nested under its parent"
            );
        });
    }
}
