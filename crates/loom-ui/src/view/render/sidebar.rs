use super::*;

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
            .when(layout.phone, |button| {
                button
                    .large()
                    .h(layout.control_size())
                    .w(layout.control_size())
            })
            .when(!layout.phone, |button| button.small())
            .accessibility_label("New project")
            .on_click(cx.listener(Self::new_session))
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
        let sessions = self.sessions.clone();
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
        let session_tasks = self
            .project_tree_snapshots
            .iter()
            .flat_map(|project| project.tasks.iter())
            .map(|task| (task.target_session_id, task.status))
            .collect::<BTreeMap<_, _>>();
        let descendant_counts = tree_nodes
            .iter()
            .map(|node| (node.session_id, session_tree_descendant_count(node)))
            .collect::<BTreeMap<_, _>>();
        // Sessions actually running work, and the projects whose subtree
        // contains one, so a project shows activity for its delegated agents
        // too. Session state is authoritative; persisted task records can lag
        // behind and would keep a finished project animating.
        let active_sessions = sessions
            .iter()
            .filter(|session| session_is_active(session.state))
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let active_roots = tree_nodes
            .iter()
            .map(|node| {
                (
                    node.session_id,
                    session_subtree_is_active(node, &active_sessions),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let tree_items = tree_nodes.iter().map(session_tree_item).collect::<Vec<_>>();
        let selected_session_id = self.active_session.id.to_string();
        let selected_item = find_session_tree_item(&tree_items, &selected_session_id);
        let tree = if let Some(tree) = self.session_tree.clone() {
            if self.session_tree_entries != tree_nodes {
                tree.update(cx, |state, cx| state.set_items(tree_items.clone(), cx));
                self.session_tree_entries = tree_nodes.clone();
            }
            let current_selected_id = tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string());
            if current_selected_id.as_deref() != Some(selected_session_id.as_str()) {
                tree.update(cx, |state, cx| state.set_selected_item(selected_item, cx));
            }
            tree
        } else {
            self.session_tree_entries = tree_nodes;
            let tree = cx.new(|cx| TreeState::new(cx).items(tree_items.clone()));
            tree.update(cx, |state, cx| state.set_selected_item(selected_item, cx));
            self.session_tree = Some(tree.clone());
            tree
        };

        let view = cx.entity();
        let menu_sessions = sessions.clone();
        let menu_view = view.clone();
        let menu_project = self.project_snapshot.clone();
        // Shared so the per-row overflow menu can build the session context
        // menu lazily without cloning the project snapshot every frame.
        let row_project = std::rc::Rc::new(self.project_snapshot.clone());
        KitTree::new(&tree, move |index, entry, selected, _, _app| {
            let session_id = entry.item().id.to_string();
            let Some(session) = sessions
                .iter()
                .find(|session| session.id.to_string() == session_id)
                .cloned()
            else {
                return ListItem::new(("session-tree-root", index));
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
            let activity = (is_root && active).then(|| task_activity_indicator(index, status));
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
            let click_view = view.clone();
            let click_session = session.clone();
            ListItem::new(("session-tree-root", index))
                .selected(selected)
                .mx_1()
                .rounded_md()
                .px_2()
                .py(layout.nav_row_padding())
                .text_size(layout.nav_row_font_size())
                .child(
                    div()
                        .pl(px(depth as f32 * if layout.phone { 16. } else { 12. }))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(if layout.phone { 16. } else { 12. }))
                                .flex_shrink_0()
                                .when_some(tree_indicator, |element, icon| element.child(icon)),
                        )
                        .child(
                            Icon::new(icon)
                                .size(px(15.))
                                .flex_shrink_0()
                                .text_color(icon_color),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(label_color)
                                .when(is_root, |element| element.font_weight(FontWeight::SEMIBOLD))
                                .child(label),
                        )
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
                                        Button::new(("session-tree-menu", index))
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
                        ),
                )
                .on_click(move |_, _, cx| {
                    click_view.update(cx, |this, cx| {
                        // Selecting from the phone drawer navigates, so dismiss
                        // the drawer even when the session is already active.
                        this.session_drawer_open = false;
                        // Re-selecting the session already on screen would
                        // reload and briefly blank the conversation.
                        if this.active_session.id != click_session.id {
                            this.select_session(click_session.clone(), cx);
                        }
                        cx.notify();
                    });
                })
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
        .size_full()
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
                            .child(
                                Button::new("settings-button")
                                    .icon(Icon::new(IconName::Settings))
                                    .ghost()
                                    .when(layout.phone, |button| {
                                        button
                                            .large()
                                            .h(layout.control_size())
                                            .w(layout.control_size())
                                    })
                                    .when(!layout.phone, |button| button.small())
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
                    .id("session-list")
                    .overflow_y_scroll()
                    .child(self.render_session_list(layout, cx)),
            )
    }
}
