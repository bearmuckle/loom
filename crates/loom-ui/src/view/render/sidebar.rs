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
                button.with_size(layout.control_size())
            })
            .when(!layout.phone, |button| button.small())
            .tooltip("New project")
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
        KitTree::new(&tree, move |index, entry, selected, _, app| {
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
            let tree_indicator = if entry.is_folder() {
                if entry.is_expanded() { "⌄" } else { "›" }
            } else {
                " "
            };
            let node_indicator = is_root.then(|| {
                view.read(app)
                    .render_session_node_indicator(session.id, index)
            });
            let updated = is_root.then(|| {
                relative_time(
                    session.updated_at.as_unix_millis(),
                    Timestamp::now().as_unix_millis(),
                )
            });
            let descendant_count = descendant_counts.get(&session.id).copied().unwrap_or(0);
            let count_badge =
                (is_root && descendant_count > 0).then(|| descendant_count.to_string());
            let root_active = is_root && session_is_active(session.state);
            let pill = (!is_root).then(|| {
                session_status_pill(session.state, session_tasks.get(&session.id).copied())
            });
            let icon = if is_root {
                if entry.is_folder() {
                    AssetIconName::Workflow
                } else {
                    AssetIconName::MessageSquare
                }
            } else {
                AssetIconName::BotMessageSquare
            };
            let icon_color = if is_root || selected {
                rgb(0x93c5fd)
            } else {
                rgb(0x8f98a6)
            };
            let label_color = if is_root {
                rgb(0xe5e7eb)
            } else {
                rgb(0xb7c0d0)
            };
            let click_view = view.clone();
            let click_session = session.clone();
            ListItem::new(("session-tree-root", index))
                .selected(selected)
                .px_2()
                .py(layout.nav_row_padding())
                .text_size(layout.nav_row_font_size())
                .child(
                    div()
                        .pl(px(depth as f32 * if layout.phone { 18. } else { 14. }))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(if layout.phone { 16. } else { 10. }))
                                .text_color(rgb(0x8f98a6))
                                .child(tree_indicator),
                        )
                        .child(
                            Icon::new(icon)
                                .when(layout.phone, |icon| icon.size_5())
                                .when(!layout.phone, |icon| icon.size_4())
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
                        .when_some(updated, |element, updated| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child(updated),
                            )
                        })
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
                        .when_some(node_indicator, |element, indicator| {
                            element.child(indicator)
                        }),
                )
                .on_click(move |_, _, cx| {
                    click_view.update(cx, |this, cx| {
                        // Re-selecting the session already on screen would
                        // reload and briefly blank the conversation.
                        if this.active_session.id != click_session.id {
                            this.select_session(click_session.clone(), cx);
                        }
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

    pub(crate) fn render_session_node_indicator(
        &self,
        session_id: AgentSessionId,
        index: usize,
    ) -> gpui_kit::AnyElement {
        let node_id = self.session_node_ids.get(&session_id).map(String::as_str);
        let node = session_owner_status(&self.worker_nodes, &self.session_node_ids, session_id);
        let status = node.map(|node| &node.status);
        let indicator_state =
            session_node_indicator_state(status, node.map_or(0, |node| node.severe_load_streak));
        let online = indicator_state != SessionNodeIndicatorState::Offline;
        let pulse = session_node_pulse(status, self.workspace_config.cpu_pulse_threshold_percent);
        let color = match indicator_state {
            SessionNodeIndicatorState::Offline => rgb(0x64748b),
            SessionNodeIndicatorState::Online => rgb(0x4ade80),
            SessionNodeIndicatorState::Severe => rgb(0xef4444),
        };
        let name = worker_node_name_for_id(&self.worker_nodes, &self.node_names, node_id);
        let metrics = status.filter(|status| status.online).map_or_else(
            || "CPU n/a · RAM n/a".to_owned(),
            |status| format_session_resource_percentages(Some(status)),
        );
        let tooltip_text = format!(
            "{}{}\n{}",
            name,
            if online { "" } else { " · Offline" },
            metrics
        );
        let dot = if let Some((period, amplitude)) = pulse {
            div()
                .w(px(6.))
                .h(px(6.))
                .rounded_full()
                .bg(color)
                .with_animation(
                    ("session-node-status-pulse", index),
                    Animation::new(period).repeat_synced().with_max_fps(24.),
                    move |element, progress| {
                        let eased_progress = progress * progress * (3. - 2. * progress);
                        let pulse = 0.5 - 0.5 * (eased_progress * std::f32::consts::TAU).cos();
                        let size = 6. + amplitude * pulse;
                        element.w(px(size)).h(px(size))
                    },
                )
                .into_any_element()
        } else {
            div()
                .w(px(6.))
                .h(px(6.))
                .rounded_full()
                .bg(color)
                .into_any_element()
        };
        div()
            .id(("session-node-indicator", index))
            .w(px(12.))
            .h(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .tooltip(move |_, cx| {
                cx.new(|_| LoomTooltip {
                    text: tooltip_text.clone(),
                })
                .into()
            })
            .child(dot)
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
                                .label("Close")
                                .ghost()
                                .h(layout.control_size())
                                .tooltip("Close session drawer")
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
                    .child(
                        div()
                            .when(layout.phone, |element| {
                                element.text_size(layout.nav_row_font_size())
                            })
                            .when(!layout.phone, |element| element.text_xs())
                            .text_color(rgb(0x8f98a6))
                            .child("Projects"),
                    )
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
                                        button.with_size(layout.control_size())
                                    })
                                    .when(!layout.phone, |button| button.small())
                                    .accessibility_label("Settings")
                                    .tooltip("Settings")
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
                element.child(KitInput::new(input).id("session-filter").small())
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
