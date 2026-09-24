use std::{path::Path, path::PathBuf, sync::Arc};

use gpui::{App, AppContext as _};
use zed_client::{Client, UserStore};
use zed_fs::{Fs, RealFs};
use zed_language::LanguageRegistry;
use zed_node_runtime::NodeRuntime;
use zed_project::{LocalProjectFlags, Project};
use zed_session::{AppSession, Session};
use zed_workspace::{AppState, WorkspaceStore};

pub(crate) struct ZedGitHost {
    pub(crate) app_state: Arc<AppState>,
    pub(crate) project: gpui::Entity<Project>,
    pub(crate) workspace_root: PathBuf,
}

impl ZedGitHost {
    pub(crate) fn initialize(root: &Path, cx: &mut App) -> Result<Self, String> {
        if !root.is_dir() {
            return Err("workspace root is not a local directory".to_owned());
        }

        zed_settings::init(cx);
        let fs: Arc<dyn Fs> = RealFs::new(None, cx.background_executor().clone());
        <dyn Fs>::set_global(fs.clone(), cx);

        let languages = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
        let client = Client::production(cx);
        Client::set_global(client.clone(), cx);
        zed_client::init(&client, cx);

        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
        // Loom owns persistence; the embedded Zed workspace is intentionally ephemeral.
        let session = cx.new(|cx| AppSession::new(Session::test(), cx));
        let app_state = Arc::new(AppState {
            languages: languages.clone(),
            client: client.clone(),
            user_store: user_store.clone(),
            workspace_store,
            fs: fs.clone(),
            build_window_options: |_, _| Default::default(),
            node_runtime: NodeRuntime::unavailable(),
            session,
        });
        AppState::set_global(app_state.clone(), cx);

        zed_theme_settings::init(zed_theme::LoadThemes::JustBase, cx);
        zed_component::init();
        zed_languages::init(
            languages.clone(),
            fs.clone(),
            app_state.node_runtime.clone(),
            cx,
        );
        zed_editor::init(cx);
        zed_language_model::init(cx);
        Project::init(&client, cx);
        git_ui::init(cx);

        let project = Project::local(
            client,
            app_state.node_runtime.clone(),
            user_store,
            languages,
            fs,
            None,
            LocalProjectFlags {
                init_worktree_trust: false,
                watch_global_configs: false,
            },
            cx,
        );

        Ok(Self {
            app_state,
            project,
            workspace_root: root.to_path_buf(),
        })
    }
}
