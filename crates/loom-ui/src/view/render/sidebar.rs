//! The left pane: a flat list of projects, grouped by the worker that runs
//! them.
//!
//! Sub agents work autonomously, so they never get a row of their own. A
//! project row summarises its agents on one line and lists them behind that
//! summary's menu.

use super::*;

use gpui_kit::component::menu::ContextMenuExt as _;

/// Prefix of a worker section heading's element id, which the render tests and
/// the pane agree on.
const WORKER_SECTION_ID_PREFIX: &str = "worker-section:";

/// The height of one project row.
///
/// Every row shares it because the list measures a single row and lays the rest
/// out at the same size.
fn project_row_height(layout: ResponsiveLayout) -> Pixels {
    // Two text lines plus the padding the layout gives a navigation row, so a
    // phone row stays a finger-sized target.
    layout.nav_row_padding() + px(28.)
}

/// The presentation of one worker section heading: the connection dot, name,
/// and the second line with live resources or the last load error.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorkerSectionHeader {
    pub(crate) name: String,
    pub(crate) dot_color: Rgba,
    pub(crate) subtitle: String,
    pub(crate) subtitle_color: Rgba,
}

/// The content the project list was last handed, kept so the view can skip
/// redundant updates instead of rebuilding the list on every frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectListView {
    pub(crate) sections: Vec<ProjectListSection>,
    pub(crate) headers: Vec<WorkerSectionHeader>,
    pub(crate) layout: ResponsiveLayout,
}

impl ProjectListView {
    /// The list coordinates of a project row.
    fn index_of(&self, session_id: AgentSessionId) -> Option<IndexPath> {
        for (section, listed) in self.sections.iter().enumerate() {
            if let Some(row) = listed
                .rows
                .iter()
                .position(|row| row.session_id == session_id)
            {
                return Some(IndexPath::new(row).section(section));
            }
        }
        None
    }

    /// The flat number of a row. Element ids and the activity animation are
    /// keyed to it, so a row keeps its id when another worker's section is
    /// present.
    fn row_number(&self, ix: IndexPath) -> usize {
        self.sections
            .iter()
            .take(ix.section)
            .map(|section| section.rows.len())
            .sum::<usize>()
            + ix.row
    }
}

/// The left pane's list delegate: sections are worker groups, items are
/// projects.
pub(crate) struct ProjectListDelegate {
    /// Weak because the view owns the list, so a strong handle would keep the
    /// view alive for as long as the list exists.
    view: WeakEntity<LoomView>,
    content: ProjectListView,
}

impl ProjectListDelegate {
    pub(crate) fn new(view: WeakEntity<LoomView>, layout: ResponsiveLayout) -> Self {
        Self {
            view,
            content: ProjectListView {
                sections: Vec::new(),
                headers: Vec::new(),
                layout,
            },
        }
    }

    /// Replaces what the list renders.
    pub(crate) fn set_content(&mut self, content: ProjectListView) {
        self.content = content;
    }
}

impl ListDelegate for ProjectListDelegate {
    type Item = ListItem;

    fn sections_count(&self, _: &App) -> usize {
        // A list always has at least one section, even when it is empty.
        self.content.sections.len().max(1)
    }

    fn items_count(&self, section: usize, _: &App) -> usize {
        self.content
            .sections
            .get(section)
            .map_or(0, |section| section.rows.len())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let row = self
            .content
            .sections
            .get(ix.section)?
            .rows
            .get(ix.row)?
            .clone();
        let row_index = self.content.row_number(ix);
        let view = self.view.upgrade();
        Some(render_project_row(
            &row,
            row_index,
            self.content.layout,
            view.as_ref(),
            cx,
        ))
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        // Only sections with project rows are rendered, so every heading that
        // reaches this point belongs on screen.
        let node_id = self.content.sections.get(section)?.node_id.clone();
        let header = self.content.headers.get(section).cloned();
        Some(render_worker_section_row(
            header.as_ref(),
            &node_id,
            self.content.layout,
        ))
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        render_empty_project_list()
    }

    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        // The list keeps its own selection; the view only reads it back through
        // `ListEvent` to open the project under it.
    }
}

/// Renders a worker section heading: status dot and name on one line, then the
/// worker's resources or the last load error beneath.
fn render_worker_section_row(
    header: Option<&WorkerSectionHeader>,
    node_id: &str,
    layout: ResponsiveLayout,
) -> gpui_kit::AnyElement {
    let name = header
        .map(|header| header.name.clone())
        .unwrap_or_else(|| node_id.to_owned());
    let dot_color = header.map_or(rgb(0x64748b), |header| header.dot_color);
    let subtitle = header
        .map(|header| header.subtitle.clone())
        .unwrap_or_default();
    let subtitle_color = header.map_or(rgb(0x64748b), |header| header.subtitle_color);
    div()
        .id(format!("{WORKER_SECTION_ID_PREFIX}{node_id}"))
        .test_support()
        .w_full()
        .px_2()
        .pt_3()
        .pb_1()
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(8.))
                                .h(px(8.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(dot_color),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(layout.nav_row_font_size())
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xe5e7eb))
                                .child(name),
                        ),
                )
                .child(
                    div()
                        .pl(px(16.))
                        .min_w(px(0.))
                        .truncate()
                        .text_xs()
                        .text_color(subtitle_color)
                        .child(subtitle),
                ),
        )
        .into_any_element()
}

/// Renders the pane's empty state: no project rows to list.
fn render_empty_project_list() -> impl IntoElement {
    div()
        .id("project-list-empty")
        .test_support()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .px_4()
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("No projects yet"),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x64748b))
                .child("Create a project to start the list"),
        )
}

/// Renders one project row: its name over its source, with the agent summary
/// beside the second line.
fn render_project_row(
    row: &ProjectListRow,
    row_index: usize,
    layout: ResponsiveLayout,
    view: Option<&Entity<LoomView>>,
    cx: &App,
) -> ListItem {
    let session = view.and_then(|view| {
        view.read(cx)
            .sessions
            .iter()
            .find(|session| session.id == row.session_id)
            .cloned()
    });
    let project = view.and_then(|view| view.read(cx).project_snapshot_for(row.session_id));

    let mut title_line = div()
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .min_w(px(0.))
        .child(
            div()
                .id(("project-name", row_index))
                .test_support()
                .aria_label(row.name.clone())
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(layout.project_row_font_size())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0xe5e7eb))
                .child(row.name.clone()),
        );
    // The activity dots cover the project and every agent it delegated to; the
    // pill stays quiet unless the project's own session needs attention.
    if row.active {
        title_line = title_line.child(task_activity_indicator(row_index, row.status));
    }
    if let Some(pill) = row.attention {
        title_line = title_line.child(
            div()
                .id(("project-status", row_index))
                .test_support()
                .aria_label(pill.label)
                .flex_shrink_0()
                .px(px(7.))
                .py(px(1.))
                .rounded_full()
                .bg(rgb(pill.background))
                .text_size(layout.nav_row_font_size())
                .text_color(rgb(pill.foreground))
                .child(pill.label),
        );
    }

    let mut detail_line = div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .w_full()
        .min_w(px(0.))
        .child(
            div()
                .id(("project-detail", row_index))
                .test_support()
                .aria_label(row.detail.clone())
                .flex_shrink_1()
                .min_w(px(0.))
                .truncate()
                .text_xs()
                .text_color(rgb(0x8f98a6))
                .child(row.detail.clone()),
        );
    // The row's right-hand controls share line two and shrink together, so a
    // long summary never squeezes the detail out of the row.
    let mut detail_trailing = div()
        .flex()
        .items_center()
        .justify_end()
        .flex_shrink_1()
        .min_w(px(0.))
        .gap_1()
        .overflow_hidden();
    if let (Some(summary), Some(view)) = (row.summary.clone(), view) {
        let agents = row.agents.clone();
        let menu_view = view.clone();
        detail_trailing = detail_trailing.child(
            Button::new(("project-agents", row_index))
                .label(summary.clone())
                .ghost()
                .small()
                .accessibility_label(summary)
                .dropdown_menu(move |menu, _, _| project_agent_menu(menu, &agents, &menu_view)),
        );
    }
    if let (true, Some(view), Some(session)) = (layout.phone, view, session.clone()) {
        let menu_view = view.clone();
        let menu_session = session.clone();
        let menu_project = project.clone();
        detail_trailing = detail_trailing.child(
            Button::new(("project-menu", row_index))
                .icon(Icon::new(IconName::Ellipsis))
                .ghost()
                .small()
                .accessibility_label("Project actions")
                .dropdown_menu(move |menu, _, _| {
                    LoomView::build_project_session_context_menu(
                        menu,
                        menu_session.clone(),
                        menu_project.clone(),
                        menu_view.clone(),
                    )
                }),
        );
    }
    detail_line = detail_line.child(detail_trailing);

    let content = div()
        .id(("project-row-content", row_index))
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(1.))
        .child(title_line)
        .child(detail_line);
    // Right-click reaches the same menu the phone's ellipsis opens.
    let content = match (view, session) {
        (Some(view), Some(session)) => {
            let menu_view = view.clone();
            content
                .context_menu(move |menu, _, _| {
                    LoomView::build_project_session_context_menu(
                        menu,
                        session.clone(),
                        project.clone(),
                        menu_view.clone(),
                    )
                })
                .into_any_element()
        }
        _ => content.into_any_element(),
    };
    ListItem::new(("project-row", row_index))
        .mx_1()
        .rounded_md()
        .px_2()
        .py_0()
        .h(project_row_height(layout))
        .child(content)
}

/// The dropdown menu of a project row: one item per delegated agent, each
/// opening that agent's transcript.
fn project_agent_menu(
    mut menu: PopupMenu,
    agents: &[ProjectAgentRow],
    view: &Entity<LoomView>,
) -> PopupMenu {
    for agent in agents {
        let session_id = agent.session_id;
        let menu_view = view.clone();
        let label = format!(
            "{} · {}",
            agent.name,
            session_status_pill(agent.state, None).label
        );
        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
            menu_view.update(cx, |view, cx| view.open_project_agent(session_id, cx));
        }));
    }
    menu
}

impl LoomView {
    pub(crate) fn render_new_session_button(
        &self,
        view: &Entity<Self>,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let _ = view;
        Button::new("new-session")
            .icon(Icon::new(IconName::Plus))
            .ghost()
            .when(layout.phone, |button| button.large())
            .h(layout.pane_action_size())
            .w(layout.pane_action_size())
            .accessibility_label("New project")
            .on_click(cx.listener(Self::new_session))
            .into_any_element()
    }

    /// Opens the archived-session management surface. The icon sits beside the
    /// new-project action because the surface manages sessions, not settings.
    pub(crate) fn render_archived_sessions_button(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        Button::new("archived-sessions")
            .icon(Icon::new(AssetIconName::Archive))
            .ghost()
            .when(layout.phone, |button| button.large())
            .h(layout.pane_action_size())
            .w(layout.pane_action_size())
            .accessibility_label("Archived sessions")
            .tooltip("Archived sessions")
            .on_click(cx.listener(|view, _, _, cx| {
                view.open_archived_sessions(cx);
            }))
            .into_any_element()
    }

    /// Derives the pane's project rows and hands them to the list, creating it
    /// on the first frame.
    ///
    /// The list is only rebuilt when the derived rows, the headings, or the
    /// layout change, and the highlight is only moved when the project that
    /// owns the active session changes, so this cannot notify in a loop.
    pub(crate) fn sync_project_list(
        &mut self,
        layout: ResponsiveLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let filter = self
            .session_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let node_order = self
            .worker_nodes
            .iter()
            .map(|node| node.status.node_id.clone())
            .collect::<Vec<_>>();
        // The snapshots of every project the client knows about, plus the one on
        // screen, so a project keeps its row while the list is being fetched.
        let mut projects = self.project_tree_snapshots.iter().collect::<Vec<_>>();
        if let Some(active_project) = self.project_snapshot.as_ref()
            && !projects
                .iter()
                .any(|known| known.project_id == active_project.project_id)
        {
            projects.push(active_project);
        }
        let model = project_list_model(ProjectListInput {
            sessions: &self.sessions,
            owners: &self.session_node_ids,
            node_order: &node_order,
            default_node_id: &self.default_backend_node_id,
            projects: &projects,
            active_session_id: self.active_session.id,
            source_labels: &self.session_sources,
            filter: &filter,
            now_millis: Timestamp::now().as_unix_millis(),
        });
        let headers = model
            .sections
            .iter()
            .map(|section| self.worker_section_header(&section.node_id))
            .collect::<Vec<_>>();
        let view_content = ProjectListView {
            sections: model.sections,
            headers,
            layout,
        };

        let list = match self.project_list.clone() {
            Some(list) => list,
            None => {
                let delegate = ProjectListDelegate::new(cx.entity().downgrade(), layout);
                let list: Entity<ListState<ProjectListDelegate>> =
                    cx.new(|cx| ListState::new(delegate, window, cx));
                self.project_list_subscription = Some(cx.subscribe(
                    &list,
                    |view: &mut Self,
                     _list: Entity<ListState<ProjectListDelegate>>,
                     event: &ListEvent,
                     cx: &mut Context<Self>| {
                        view.on_project_list_event(event, cx);
                    },
                ));
                self.project_list = Some(list.clone());
                self.project_list_active = None;
                list
            }
        };

        let changed = self.project_list_view.as_ref() != Some(&view_content);
        if changed {
            list.update(cx, |state, cx| {
                state.delegate_mut().set_content(view_content.clone());
                cx.notify();
            });
            self.project_list_view = Some(view_content);
        }

        // The highlight follows the project that owns the active session, so a
        // sub agent keeps its project's row selected.
        if self.project_list_active != model.selected {
            self.project_list_active = model.selected;
            let index = self
                .project_list_view
                .as_ref()
                .zip(model.selected)
                .and_then(|(view, selected)| view.index_of(selected));
            list.update(cx, |state, cx| {
                state.set_selected_index(index, window, cx);
                cx.notify();
            });
        }
    }

    /// Handles the list's own events: a click or Enter opens the project under
    /// the highlight, while Up/Down only move it.
    fn on_project_list_event(&mut self, event: &ListEvent, cx: &mut Context<Self>) {
        let ListEvent::Confirm(ix) = event else {
            return;
        };
        let session_id = self
            .project_list_view
            .as_ref()
            .and_then(|view| view.sections.get(ix.section))
            .and_then(|section| section.rows.get(ix.row))
            .map(|row| row.session_id);
        let Some(session) = session_id.and_then(|session_id| {
            self.sessions
                .iter()
                .find(|session| session.id == session_id)
                .cloned()
        }) else {
            return;
        };
        self.navigate_to_session(session, cx);
    }

    /// Opens a session from the left pane. A project row, an agent summary item,
    /// and the row's context menu all take this path.
    pub(crate) fn navigate_to_session(
        &mut self,
        session: AgentSessionSnapshot,
        cx: &mut Context<Self>,
    ) {
        // Selecting from the phone drawer navigates, so dismiss the drawer even
        // when the session is already active.
        self.session_drawer_open = false;
        // Re-selecting the session already on screen would reload and briefly
        // blank the conversation. It must still dismiss whatever covers the
        // conversation; otherwise a click on the open project looks like it did
        // nothing while Settings or the new-project pane is in front of it.
        if self.active_session.id != session.id {
            self.select_session(session, cx);
        } else {
            self.dismiss_surfaces();
        }
        cx.notify();
    }

    /// Opens a delegated agent's transcript, the way its project row's summary
    /// menu offers it.
    pub(crate) fn open_project_agent(
        &mut self,
        session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            return;
        };
        self.navigate_to_session(session, cx);
    }

    /// The known project snapshot that covers a session, for its row's context
    /// menu.
    fn project_snapshot_for(
        &self,
        session_id: AgentSessionId,
    ) -> Option<loom_core::ProjectSnapshot> {
        let covers = |project: &&loom_core::ProjectSnapshot| {
            project.root_session_id == session_id
                || project
                    .agents
                    .iter()
                    .any(|agent| agent.session_id == session_id)
        };
        self.project_tree_snapshots
            .iter()
            .find(covers)
            .or_else(|| self.project_snapshot.as_ref().filter(covers))
            .cloned()
    }

    /// Renders the pane's project list: one scrolling region for every worker
    /// section.
    pub(crate) fn render_session_list(&self) -> gpui_kit::AnyElement {
        match self.project_list.clone() {
            Some(list) => List::new(&list).w_full().h_full().into_any_element(),
            None => div().w_full().h_full().into_any_element(),
        }
    }

    /// Builds a worker's section header: status dot and name on one line, then
    /// live resources or the last load error beneath, so a long worker name is
    /// never clipped. The heading is a section title, not a tree node, so it
    /// carries no expand/collapse control.
    pub(crate) fn worker_section_header(&self, node_id: &str) -> WorkerSectionHeader {
        let node = self
            .worker_nodes
            .iter()
            .find(|node| node.status.node_id == node_id);
        let name = self
            .node_names
            .get(node_id)
            .cloned()
            .or_else(|| node.map(worker_node_display_name))
            .unwrap_or_else(|| "Removed worker".to_owned());
        let online = node.is_some_and(|node| {
            matches!(node.connection_state, WorkerConnectionState::Connected) && node.status.online
        });
        let dot_color = if online { rgb(0x34d399) } else { rgb(0x64748b) };
        let load_error = self.node_session_load_errors.get(node_id);
        let (subtitle, subtitle_color) = if let Some(error) = load_error {
            (error.clone(), rgb(0xfca5a5))
        } else if let Some(node) = node {
            if online {
                (
                    format_worker_node_resources(&node.status.resources),
                    rgb(0x64748b),
                )
            } else {
                ("offline".to_owned(), rgb(0x64748b))
            }
        } else {
            ("No worker is connected".to_owned(), rgb(0x64748b))
        };
        WorkerSectionHeader {
            name,
            dot_color,
            subtitle,
            subtitle_color,
        }
    }

    pub(crate) fn render_session_sidebar(
        &mut self,
        view: &Entity<Self>,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .h_full()
            .relative()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(0x17191f))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .when(layout.phone, |element| {
                        element.child(
                            Button::new("close-session-drawer")
                                .icon(Icon::new(IconName::ChevronLeft))
                                .ghost()
                                .large()
                                .h(layout.control_size())
                                .w(layout.control_size())
                                .accessibility_label("Back")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_drawer_open = false;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Projects"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(self.render_new_session_button(view, layout, cx))
                            .child(self.render_archived_sessions_button(layout, cx))
                            .child(
                                Button::new("settings-button")
                                    .icon(Icon::new(IconName::Settings))
                                    .ghost()
                                    .when(layout.phone, |button| button.large())
                                    .h(layout.pane_action_size())
                                    .w(layout.pane_action_size())
                                    .accessibility_label("Settings")
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            view.update(cx, |view, cx| {
                                                view.open_settings_from_menu(cx);
                                            });
                                        }
                                    }),
                            ),
                    ),
            )
            .when_some(self.session_filter_input.as_ref(), |element, input| {
                let field = KitInput::new(input).id("session-filter");
                // The phone drawer opts into the medium field so the filter
                // reads at the same scale as the list rows; desktop keeps the
                // compact field used across the rest of the sidebar.
                let field = if layout.phone { field } else { field.small() };
                element.child(field)
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .id("session-list")
                    .child(self.render_session_list()),
            )
    }
}
