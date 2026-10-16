use super::*;

/// Prefix that marks a worker section heading inside the shared session tree,
/// so the row renderer can tell a section header apart from a session row.
const WORKER_SECTION_ID_PREFIX: &str = "worker-section:";

/// The presentation of one worker section heading: the connection dot, name,
/// and the second line with live resources or the last load error.
pub(crate) struct WorkerSectionHeader {
    pub(crate) name: String,
    pub(crate) dot_color: Rgba,
    pub(crate) subtitle: String,
    pub(crate) subtitle_color: Rgba,
}

/// Numbers every session in the order it appears in the fully expanded tree, so
/// a session keeps the same element id when its ancestors are collapsed or when
/// another worker's section is present.
fn assign_session_ordinals(
    node: &SessionTreeNode,
    ordinals: &mut BTreeMap<String, usize>,
    next: &mut usize,
) {
    ordinals.insert(node.session_id.to_string(), *next);
    *next += 1;
    for child in &node.children {
        assign_session_ordinals(child, ordinals, next);
    }
}

/// A disabled tree item that renders a worker section heading. It has no
/// children and no interaction, so it is never expandable or selectable.
fn worker_section_tree_item(node_id: &str) -> TreeItem {
    TreeItem::new(format!("{WORKER_SECTION_ID_PREFIX}{node_id}"), node_id).disabled(true)
}

/// Renders a worker section heading row: status dot and name on one line, then
/// the worker's resources or the last load error beneath.
fn render_worker_section_row(
    header: Option<&WorkerSectionHeader>,
    node_id: &str,
    layout: ResponsiveLayout,
) -> ListItem {
    let name = header
        .map(|header| header.name.clone())
        .unwrap_or_else(|| node_id.to_owned());
    let dot_color = header.map_or(rgb(0x64748b), |header| header.dot_color);
    let subtitle = header
        .map(|header| header.subtitle.clone())
        .unwrap_or_default();
    let subtitle_color = header.map_or(rgb(0x64748b), |header| header.subtitle_color);
    ListItem::new(format!("{WORKER_SECTION_ID_PREFIX}{node_id}"))
        .mx_1()
        .rounded_md()
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

    pub(crate) fn render_session_list(
        &mut self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        // Group sessions by their owning worker. A connected worker with no
        // projects keeps an empty group so "no projects" is distinguishable
        // from "failed to load"; disconnected or removed workers only stay in
        // the list while they still own a session.
        let groups = group_sessions_by_worker(
            &self.sessions,
            &self.session_node_ids,
            &node_order,
            &self.default_backend_node_id,
        )
        .into_iter()
        .filter(|(node_id, sessions)| {
            !sessions.is_empty()
                || self.worker_nodes.iter().any(|node| {
                    &node.status.node_id == node_id
                        && matches!(node.connection_state, WorkerConnectionState::Connected)
                })
        })
        .collect::<Vec<_>>();

        // One tree for the whole pane: every worker becomes a section in a
        // single scrolling list, so there is no per-worker scroll region and
        // no per-worker height allocation.
        let mut rows = Vec::new();
        let mut headers = BTreeMap::new();
        for (node_id, sessions) in groups {
            headers.insert(node_id.clone(), self.worker_section_header(&node_id));
            rows.push(SessionTreeRow::WorkerHeader {
                node_id: node_id.clone(),
            });
            let mut tree_projects = self.project_tree_snapshots.iter().collect::<Vec<_>>();
            if let Some(active_project) = self.project_snapshot.as_ref()
                && !tree_projects
                    .iter()
                    .any(|known| known.project_id == active_project.project_id)
            {
                tree_projects.push(active_project);
            }
            let projection = project_session_list_projection_for_projects(
                &sessions,
                self.active_session.id,
                tree_projects,
            );
            let tree_nodes = if filter.is_empty() {
                projection.tree
            } else {
                filter_session_tree(projection.tree, &filter)
            };
            rows.extend(tree_nodes.into_iter().map(SessionTreeRow::Project));
        }

        self.render_session_tree(rows, headers, layout, cx)
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

    /// Builds (or updates) the single session tree and renders it. Worker
    /// headers and every worker's projects share one virtualized tree, so the
    /// whole pane scrolls as one region.
    fn render_session_tree(
        &mut self,
        rows: Vec<SessionTreeRow>,
        headers: BTreeMap<String, WorkerSectionHeader>,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        // Each session keeps a stable row id regardless of whether its
        // ancestors are expanded or which worker sections are present, so the
        // rendered element ids do not shuffle as the list changes.
        let mut ordinals = BTreeMap::new();
        let mut next_ordinal = 0usize;
        for row in &rows {
            if let SessionTreeRow::Project(node) = row {
                assign_session_ordinals(node, &mut ordinals, &mut next_ordinal);
            }
        }
        let session_tasks = self
            .project_tree_snapshots
            .iter()
            .flat_map(|project| project.tasks.iter())
            .map(|task| (task.target_session_id, task.status))
            .collect::<BTreeMap<_, _>>();
        let descendant_counts = rows
            .iter()
            .filter_map(|row| match row {
                SessionTreeRow::Project(node) => {
                    Some((node.session_id, session_tree_descendant_count(node)))
                }
                SessionTreeRow::WorkerHeader { .. } => None,
            })
            .collect::<BTreeMap<_, _>>();
        // Sessions actually running work, and the projects whose subtree
        // contains one, so a project shows activity for its delegated agents
        // too. Session state is authoritative; persisted task records can lag
        // behind and would keep a finished project animating.
        let active_sessions = self
            .sessions
            .iter()
            .filter(|session| session_is_active(session.state))
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let active_roots = rows
            .iter()
            .filter_map(|row| match row {
                SessionTreeRow::Project(node) => Some((
                    node.session_id,
                    session_subtree_is_active(node, &active_sessions),
                )),
                SessionTreeRow::WorkerHeader { .. } => None,
            })
            .collect::<BTreeMap<_, _>>();
        let tree_items = rows
            .iter()
            .map(|row| match row {
                SessionTreeRow::WorkerHeader { node_id } => worker_section_tree_item(node_id),
                SessionTreeRow::Project(node) => session_tree_item(node),
            })
            .collect::<Vec<_>>();
        let selected_session_id = self.active_session.id.to_string();
        let selected_item = find_session_tree_item(&tree_items, &selected_session_id);
        let tree = if let Some(tree) = self.session_tree.clone() {
            let structure_changed = self.session_tree_entries.as_ref() != Some(&rows);
            if structure_changed {
                tree.update(cx, |state, cx| state.set_items(tree_items.clone(), cx));
                self.session_tree_entries = Some(rows.clone());
            }
            let active_changed = self.session_tree_active != Some(self.active_session.id);
            if structure_changed || active_changed {
                // Rebuilt items start collapsed, and navigating to a session
                // should reveal it, so re-apply the selection (which expands
                // the active session's ancestors).
                tree.update(cx, |state, cx| state.set_selected_item(selected_item, cx));
                self.session_tree_active = Some(self.active_session.id);
            } else {
                // A chevron toggle moves the tree's own highlight without
                // changing the active session. Restore the highlight to the
                // active row, but do not re-open ancestors the user collapsed.
                let current_ix = tree.read(cx).selected_index();
                let active_id: gpui_kit::SharedString = selected_session_id.clone().into();
                let active_ix = tree.read(cx).index_of(&active_id);
                if current_ix != active_ix {
                    tree.update(cx, |state, cx| state.set_selected_index(active_ix, cx));
                }
            }
            tree
        } else {
            self.session_tree_entries = Some(rows);
            self.session_tree_active = Some(self.active_session.id);
            let tree = cx.new(|cx| TreeState::new(cx).items(tree_items.clone()));
            tree.update(cx, |state, cx| state.set_selected_item(selected_item, cx));
            self.session_tree = Some(tree.clone());
            tree
        };

        let view = cx.entity();
        // The row render closure must own a clone it can focus on demand; the
        // tree itself stays borrowed by `KitTree::new` below.
        let tree_for_rows = tree.clone();
        let sessions = self.sessions.clone();
        let menu_sessions = sessions.clone();
        let menu_view = view.clone();
        let menu_project = self.project_snapshot.clone();
        // The main source of every project, shown as the second line of a
        // project row. Cloned because the row closure cannot borrow the view.
        let source_labels = self.session_sources.clone();
        // Shared so the per-row overflow menu can build the session context
        // menu lazily without cloning the project snapshot every frame.
        let row_project = std::rc::Rc::new(self.project_snapshot.clone());
        KitTree::new(&tree, move |index, entry, selected, _, _app| {
            let entry_id = entry.item().id.to_string();
            if let Some(node_id) = entry_id.strip_prefix(WORKER_SECTION_ID_PREFIX) {
                return render_worker_section_row(headers.get(node_id), node_id, layout);
            }
            let session_id = entry_id;
            let row_index = ordinals.get(&session_id).copied().unwrap_or(index);
            let Some(session) = sessions
                .iter()
                .find(|session| session.id.to_string() == session_id)
                .cloned()
            else {
                return ListItem::new(("session-tree-root", row_index));
            };
            let label = entry.item().label.to_string();
            let depth = entry.depth();
            let is_root = entry.is_root();
            let tree_indicator = entry
                .is_folder()
                .then(|| disclosure_chevron(entry.is_expanded(), rgb(0x8f98a6)).size(px(12.)));
            let descendant_count = descendant_counts.get(&session.id).copied().unwrap_or(0);
            let count_badge =
                (is_root && descendant_count > 0).then(|| descendant_count.to_string());
            let active = if is_root {
                active_roots.get(&session.id).copied().unwrap_or(false)
            } else {
                active_sessions.contains(&session.id)
            };
            let status =
                session_status_pill(session.state, session_tasks.get(&session.id).copied());
            // Project rows show a radar activity light only while the project or
            // one of its agents is working; child rows show a status chip.
            let activity = (is_root && active).then(|| task_activity_indicator(row_index, status));
            let pill = (!is_root).then_some(status);
            let root_active = is_root && active;
            let icon = if is_root {
                AssetIconName::Folder
            } else {
                AssetIconName::BotMessageSquare
            };
            let icon_color = if root_active || selected {
                rgb(0x93c5fd)
            } else {
                rgb(0x8f98a6)
            };
            let label_color = if is_root || selected {
                rgb(0xe5e7eb)
            } else {
                rgb(0xb7c0d0)
            };
            let focus_tree = tree_for_rows.clone();
            // A plain click on the session name selects it. The chevron is a
            // separate sibling that keeps the tree's built-in toggle, so
            // selecting and expanding never happen in the same gesture.
            let select_view = view.clone();
            let select_session = session.clone();
            let select = move |cx: &mut App| {
                select_view.update(cx, |this, cx| {
                    // Selecting from the phone drawer navigates, so dismiss the
                    // drawer even when the session is already active.
                    this.session_drawer_open = false;
                    // Re-selecting the session already on screen would reload and
                    // briefly blank the conversation. It must still dismiss
                    // whatever covers the conversation; otherwise a click on the
                    // open project looks like it did nothing while Settings or
                    // the new-project pane is in front of it.
                    if this.active_session.id != select_session.id {
                        this.select_session(select_session.clone(), cx);
                    } else {
                        this.dismiss_surfaces();
                    }
                    cx.notify();
                });
            };
            // A project row carries two lines: its name and the main source it
            // was created from, so the list says where each project lives
            // without opening it. Child rows keep the single-line label.
            let label_element = if is_root {
                let mut label_column = div().flex_1().min_w(px(0.)).flex().flex_col().child(
                    div()
                        .id(("session-tree-label", row_index))
                        .test_support()
                        .aria_label(label.clone())
                        .min_w(px(0.))
                        .truncate()
                        .text_size(layout.project_row_font_size())
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(label_color)
                        .child(label),
                );
                let context = source_labels.get(&session.id).cloned().unwrap_or_else(|| {
                    format!(
                        "updated {}",
                        relative_time(
                            session.updated_at.as_unix_millis(),
                            Timestamp::now().as_unix_millis()
                        )
                    )
                });
                label_column = label_column.child(
                    div()
                        .id(("session-tree-source", row_index))
                        .test_support()
                        .aria_label(context.clone())
                        .min_w(px(0.))
                        .truncate()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(context),
                );
                label_column
            } else {
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(label_color)
                    .child(label)
            };
            let row_content = div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap_2()
                // Swallow the click so it selects the session without also
                // expanding or collapsing the project.
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    // The tree's own mouse-down listener normally focuses it;
                    // restore that now that propagation is stopped.
                    focus_tree.update(cx, |state, cx| state.focus(window, cx));
                    select(cx);
                })
                // A leaf has no chevron; its alignment spacer selects with the
                // rest of the row.
                .when(!entry.is_folder(), |element| {
                    element.child(
                        div()
                            .w(px(if layout.phone { 16. } else { 12. }))
                            .flex_shrink_0(),
                    )
                })
                .child(
                    Icon::new(icon)
                        .size(px(15.))
                        .flex_shrink_0()
                        .text_color(icon_color),
                )
                .child(label_element)
                .child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap_2()
                        .when_some(count_badge, |element, count| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(6.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(if root_active {
                                        rgb(0x2563eb)
                                    } else {
                                        rgb(0x30343f)
                                    })
                                    .text_xs()
                                    .text_color(if root_active {
                                        rgb(0x93c5fd)
                                    } else {
                                        rgb(0x64748b)
                                    })
                                    .child(count),
                            )
                        })
                        .when_some(activity, |element, activity| element.child(activity))
                        .when_some(pill, |element, pill| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(7.))
                                    .py(px(1.))
                                    .rounded_full()
                                    .bg(rgb(pill.background))
                                    .text_xs()
                                    .text_color(rgb(pill.foreground))
                                    .child(pill.label),
                            )
                        })
                        .when(layout.phone, |element| {
                            // Touch has no right-click, so expose the
                            // session context menu (rename/archive) as a
                            // row action.
                            let menu_session = session.clone();
                            let menu_project = row_project.clone();
                            let menu_view = view.clone();
                            element.child(
                                Button::new(("session-tree-menu", row_index))
                                    .icon(Icon::new(IconName::Ellipsis))
                                    .ghost()
                                    .small()
                                    .accessibility_label("Project actions")
                                    .dropdown_menu(move |menu, _window, _cx| {
                                        Self::build_project_session_context_menu(
                                            menu,
                                            menu_session.clone(),
                                            (*menu_project).clone(),
                                            menu_view.clone(),
                                        )
                                    }),
                            )
                        }),
                );
            let row = div()
                .pl(px(depth as f32 * if layout.phone { 16. } else { 12. }))
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                // The chevron stays outside the click guard so `TreeState` still
                // expands or collapses this row when it is clicked.
                .when(entry.is_folder(), |element| {
                    element.child(
                        div()
                            .id(("session-tree-disclosure", row_index))
                            .test_support()
                            .w(px(if layout.phone { 16. } else { 12. }))
                            .flex_shrink_0()
                            .when_some(tree_indicator, |element, icon| element.child(icon)),
                    )
                })
                .child(row_content);
            ListItem::new(("session-tree-root", row_index))
                .selected(selected)
                .mx_1()
                .rounded_md()
                .px_2()
                .py(layout.nav_row_padding())
                .text_size(layout.nav_row_font_size())
                .child(row)
        })
        .context_menu(move |_, entry, menu, _window, _cx| {
            let session_id = entry.item().id.to_string();
            let Some(session) = menu_sessions
                .iter()
                .find(|session| session.id.to_string() == session_id)
                .cloned()
            else {
                return menu;
            };
            Self::build_project_session_context_menu(
                menu,
                session,
                menu_project.clone(),
                menu_view.clone(),
            )
        })
        .w_full()
        .into_any_element()
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
                    .child(self.render_session_list(layout, cx)),
            )
    }
}
