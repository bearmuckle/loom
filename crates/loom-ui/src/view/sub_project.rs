use super::*;

impl LoomView {
    /// The project the conversation on screen belongs to.
    ///
    /// A delegated agent is part of its project, so the project a header action
    /// targets is the root of the project that covers the active session, or the
    /// active session itself when no known project covers it.
    pub(crate) fn header_project_session_id(&self) -> AgentSessionId {
        self.project_snapshot
            .as_ref()
            .filter(|project| {
                project
                    .agents
                    .iter()
                    .any(|agent| agent.session_id == self.active_session.id)
            })
            .map_or(self.active_session.id, |project| project.root_session_id)
    }

    /// Whether a header action can target a project: the session it would act on
    /// is one the client lists.
    pub(crate) fn has_header_project(&self) -> bool {
        let project_session_id = self.header_project_session_id();
        self.sessions
            .iter()
            .any(|session| session.id == project_session_id)
    }

    /// Opens the dialog that names a sub project of the project on screen.
    pub(crate) fn begin_sub_project_dialog(&mut self, cx: &mut Context<Self>) {
        let parent_session_id = self.header_project_session_id();
        let Some(parent_name) = self
            .sessions
            .iter()
            .find(|session| session.id == parent_session_id)
            .map(|session| session.name.clone())
        else {
            self.record_status("Open a project to branch a sub project from it");
            cx.notify();
            return;
        };
        self.show_surface(CoveringSurface::SubProject);
        self.sub_project_dialog = Some(SubProjectDialogState {
            parent_session_id,
            parent_name,
        });
        self.sub_project_input_state = None;
        cx.notify();
    }

    /// Closes the sub project dialog without creating anything.
    pub(crate) fn close_sub_project_dialog(&mut self, cx: &mut Context<Self>) {
        if self.covering_surface() == Some(CoveringSurface::SubProject) {
            self.close_covering_surface();
        } else {
            self.sub_project_dialog = None;
            self.sub_project_input_state = None;
        }
        cx.notify();
    }

    /// Archives the project the conversation on screen belongs to.
    ///
    /// The archive is the same durable transition the project's own menu
    /// performs: the backend archives the tree, the client drops it from the
    /// list, and retention decides when its history is discarded.
    pub(crate) fn archive_header_project(&mut self, cx: &mut Context<Self>) {
        let project_session_id = self.header_project_session_id();
        if !self
            .sessions
            .iter()
            .any(|session| session.id == project_session_id)
        {
            self.record_status("No project to archive");
            cx.notify();
            return;
        }
        self.archive_session(project_session_id, cx);
    }

    /// Creates the sub project the dialog names.
    ///
    /// The branch is a project of its own: the worker creates its session and
    /// records the link to the parent project, and the client then attaches the
    /// parent's sources to it. A source that cannot be attached is reported and
    /// does not discard the new project, which the user can still work in and
    /// add a source to.
    pub(crate) fn confirm_sub_project(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.sub_project_dialog.take() else {
            return;
        };
        let name = self
            .sub_project_input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if name.is_empty() {
            self.record_status("Sub project name cannot be empty");
            self.sub_project_dialog = Some(dialog);
            cx.notify();
            return;
        }
        let backend = match self.backend_for_session(dialog.parent_session_id) {
            Ok(backend) => backend,
            Err(error) => {
                self.sub_project_dialog = Some(dialog);
                self.record_backend_error("create sub project", error);
                cx.notify();
                return;
            }
        };
        if self.covering_surface() == Some(CoveringSurface::SubProject) {
            self.close_covering_surface();
        } else {
            self.sub_project_dialog = None;
            self.sub_project_input_state = None;
        }
        let parent_session_id = dialog.parent_session_id;
        let parent_name = dialog.parent_name.clone();
        // The branch keeps working on the worker, model, and sources the parent
        // project already uses, so nothing has to be chosen again.
        let node_id = self.session_node_ids.get(&parent_session_id).cloned();
        let model = self
            .session_models
            .get(&parent_session_id)
            .cloned()
            .unwrap_or_else(|| self.default_model.clone());
        self.record_status(format!("Creating sub project of {parent_name}…"));
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = create_sub_project_from_project(backend, parent_session_id, name).await;
            view.update(cx, |view, cx| match result {
                Ok(created) => {
                    let session = created.session;
                    view.session_models.insert(session.id, model);
                    if let Some(node_id) = node_id {
                        view.session_node_ids.insert(session.id, node_id);
                    }
                    if !view.sessions.iter().any(|known| known.id == session.id) {
                        view.sessions.push(session.clone());
                    }
                    // The snapshot carries the parent link, so the sidebar nests
                    // the new project under the one it branched from. A branch
                    // whose snapshot could not be read stays listed and is asked
                    // for again, so it never becomes a top-level project row.
                    match created.snapshot {
                        Some(snapshot) => {
                            view.project_tree_snapshots
                                .retain(|known| known.project_id != snapshot.project_id);
                            view.project_tree_snapshots.push(snapshot);
                        }
                        None => view.refresh_missing_project_snapshots(cx),
                    }
                    view.select_session(session, cx);
                    match created.source_warnings.first() {
                        Some(warning) => view.record_status(format!(
                            "Sub project created, but its sources could not be attached: {warning}"
                        )),
                        None => view.record_status(format!("Sub project of {parent_name} created")),
                    }
                    view.refresh_session_source_labels(cx);
                }
                Err(error) => view.record_backend_error("create sub project", error),
            })
            .ok();
        })
        .detach();
    }
}

/// A sub project the worker created, with the snapshot that groups it under its
/// parent and any sources that could not be attached.
struct CreatedSubProject {
    session: AgentSessionSnapshot,
    /// The new project's snapshot, or `None` when the worker created the
    /// project but its snapshot could not be read. The branch is listed either
    /// way, and a missing snapshot is fetched again by the sidebar's own scan.
    snapshot: Option<loom_core::ProjectSnapshot>,
    source_warnings: Vec<String>,
}

async fn create_sub_project_from_project(
    backend: BackendWorker,
    parent_session_id: AgentSessionId,
    name: String,
) -> Result<CreatedSubProject, LoomError> {
    let created = backend
        .submit(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::CreateSubProject {
                parent_session_id,
                name,
            },
        )))
        .wait()
        .await;
    let (project_id, session) = match created.result? {
        ServerResponse::Project(ProjectResponse::SubProjectCreated {
            project_id,
            session,
        }) => (project_id, *session),
        response => return Err(unexpected_response("sub project creation", response)),
    };
    log::info!("[loom-ui] created sub project {project_id} from {parent_session_id}");
    let source_warnings = inherit_parent_sources(&backend, parent_session_id, session.id).await;
    let snapshot = backend
        .submit(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession {
                session_id: session.id,
            },
        )))
        .wait()
        .await;
    let snapshot = match snapshot.result {
        Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) => Some(snapshot),
        Ok(response) => {
            log::warn!("[loom-ui] unexpected sub project snapshot: {response:?}");
            None
        }
        Err(error) => {
            log::warn!(
                "[loom-ui] could not read the snapshot of sub project {}: {}",
                session.id,
                error.message
            );
            None
        }
    };
    Ok(CreatedSubProject {
        session,
        snapshot,
        source_warnings,
    })
}

/// Attaches the sources of the project a branch came from to that branch.
///
/// The parent's mounted directories go first: a mount already carries the
/// checkouts inside it, so cloning those repositories a second time would put a
/// checkout where the mount has to go, and mounting over a checkout fails the
/// same way in the other order. A repository is therefore inherited on its own
/// only when no mount covers its path; the directory attach discovers and
/// registers the checkouts of the folder it mounts.
///
/// A repository is attached from its own source instead of the parent's
/// revision: the parent's head may be a commit that exists only in the parent's
/// checkout, which no clone of the source can resolve, and a branch that cannot
/// be cloned is worse than one that starts from the source's own head.
///
/// A source that cannot be attached is reported and does not discard the new
/// project, which the user can still work in and add a source to.
async fn inherit_parent_sources(
    backend: &BackendWorker,
    parent_session_id: AgentSessionId,
    session_id: AgentSessionId,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut mounted_paths = Vec::new();
    let directories = backend
        .submit(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ListSessionDirectories {
                session_id: parent_session_id,
            },
        )))
        .wait()
        .await;
    match directories.result {
        Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectories { directories })) => {
            for directory in directories {
                let source = directory.source.clone();
                let attached = backend
                    .submit(RequestEnvelope::new(ClientRequest::Filesystem(
                        FilesystemRequest::AttachSessionDirectory {
                            session_id,
                            source: directory.source,
                            path: directory.path.clone(),
                        },
                    )))
                    .wait()
                    .await;
                match attached.result {
                    Ok(ServerResponse::Filesystem(
                        FilesystemResponse::SessionDirectoryAttached { .. },
                    )) => mounted_paths.push(directory.path),
                    Ok(response) => {
                        warnings.push(unexpected_response("directory attachment", response).message)
                    }
                    Err(error) => {
                        log::warn!(
                            "[loom-ui] could not attach {source} to sub project {session_id}: {}",
                            error.message
                        );
                        warnings.push(error.message);
                    }
                }
            }
        }
        Ok(response) => warnings.push(unexpected_response("directory listing", response).message),
        Err(error) => warnings.push(error.message),
    }
    let repositories = backend
        .submit(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories {
                session_id: parent_session_id,
            },
        )))
        .wait()
        .await;
    match repositories.result {
        Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories,
        })) => {
            for repository in repositories {
                if mounted_paths
                    .iter()
                    .any(|mount| path_is_within(&repository.path, mount))
                {
                    log::info!(
                        "[loom-ui] sub project {session_id} inherits {} through a mounted directory",
                        repository.path
                    );
                    continue;
                }
                let source = repository.source.clone();
                let attached = backend
                    .submit(RequestEnvelope::new(ClientRequest::Repository(
                        RepositoryRequest::AttachSessionRepository {
                            session_id,
                            source: repository.source,
                            path: repository.path,
                            revision: None,
                            // The worker already holds the parent's clone, so the
                            // branch starts from it instead of downloading again.
                            reuse_local: true,
                        },
                    )))
                    .wait()
                    .await;
                match attached.result {
                    Ok(ServerResponse::Repository(
                        RepositoryResponse::SessionRepositoryAttached(_),
                    )) => {}
                    Ok(response) => warnings
                        .push(unexpected_response("repository attachment", response).message),
                    Err(error) => {
                        log::warn!(
                            "[loom-ui] could not attach {source} to sub project {session_id}: {}",
                            error.message
                        );
                        warnings.push(error.message);
                    }
                }
            }
        }
        Ok(response) => warnings.push(unexpected_response("repository listing", response).message),
        Err(error) => warnings.push(error.message),
    }
    warnings
}

/// Whether a session path is a mounted directory or lies inside one.
fn path_is_within(path: &str, mount: &str) -> bool {
    path == mount || path.starts_with(&format!("{mount}/"))
}
