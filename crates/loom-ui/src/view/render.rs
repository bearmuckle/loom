use super::*;

impl LoomView {
    pub(crate) fn render_new_session_button(
        &self,
        view: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let _ = view;
        Button::new("new-session")
            .icon(Icon::new(IconName::Plus))
            .ghost()
            .small()
            .tooltip("New project")
            .on_click(cx.listener(Self::new_session))
            .into_any_element()
    }

    pub(crate) fn render_session_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                .py_2()
                .text_size(gpui_kit::rems(0.8125))
                .child(
                    div()
                        .pl(px(depth as f32 * 14.))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(10.))
                                .text_color(rgb(0x8f98a6))
                                .child(tree_indicator),
                        )
                        .child(Icon::new(icon).size_4().text_color(icon_color))
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
                        this.select_session(click_session.clone(), cx);
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

    pub(crate) fn render_model_picker(&self, phone: bool) -> impl IntoElement {
        let mut picker = div().flex().items_center();
        if let Some(state) = &self.model_select {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .hover(|style| style.bg(rgb(0x293244)))
                    .child(
                        Select::new(state)
                            .id("session-model-select")
                            .max_w(if phone { px(150.) } else { px(220.) })
                            .menu_width(px(320.))
                            .small()
                            .appearance(false)
                            .accessibility_label("Model for this session")
                            .placeholder("No model is configured")
                            .search_placeholder("Search models"),
                    ),
            );
        }
        picker
    }

    pub(crate) fn render_agent_mode_picker(&self, phone: bool) -> impl IntoElement {
        let mut picker = div().flex().items_center();
        if let Some(state) = &self.agent_mode_select {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .hover(|style| style.bg(rgb(0x293244)))
                    .child(
                        Select::new(state)
                            .id("agent-mode-select")
                            .max_w(if phone { px(110.) } else { px(140.) })
                            .small()
                            .appearance(false)
                            .accessibility_label("Agent mode")
                            .placeholder("Select agent mode"),
                    ),
            );
        }
        picker
    }

    pub(crate) fn render_assistant_turn(
        &self,
        turn: &AssistantTurn,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let role = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x9ad7bd)))
            .child(div().text_xs().text_color(rgb(0x9ad7bd)).child("Agent"));
        let mut body = div().px_3().py_2().flex().flex_col().gap_1().child(role);
        let mut part_index = 0;
        while part_index < turn.parts.len() {
            match &turn.parts[part_index] {
                AssistantPart::Reasoning(text) => {
                    if !text.trim().is_empty() {
                        let key = tool_element_id(index, part_index);
                        let expanded = self.expanded_reasoning.contains(&key);
                        let parent_for_toggle = parent.clone();
                        let mut block = div()
                            .id(("reasoning", key))
                            .test_support()
                            .flex()
                            .flex_col()
                            .pl_2()
                            .border_l_2()
                            .border_color(rgb(0x3b4555))
                            .child(
                                Button::new(("reasoning-header", key))
                                    .ghost()
                                    .small()
                                    .w_full()
                                    .accessibility_label("Reasoning")
                                    .on_click(move |_, _, cx| {
                                        parent_for_toggle
                                            .update(cx, |this, cx| this.toggle_reasoning(key, cx));
                                    })
                                    .child(
                                        div()
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .text_xs()
                                            .child(
                                                div()
                                                    .text_color(rgb(0x64748b))
                                                    .child(if expanded { "⌄" } else { "›" }),
                                            )
                                            .child(
                                                div().text_color(rgb(0x94a3b8)).child("Reasoning"),
                                            )
                                            .when(!expanded, |element| {
                                                element.child(
                                                    div()
                                                        .flex_1()
                                                        .min_w(px(0.))
                                                        .truncate()
                                                        .text_color(rgb(0x64748b))
                                                        .child(reasoning_preview(text)),
                                                )
                                            }),
                                    ),
                            );
                        if expanded {
                            block = block.child(div().mt_1().child(render_timeline_text(
                                format!("transcript-reasoning-{index}-{part_index}"),
                                text.clone(),
                                0x94a3b8,
                            )));
                        }
                        body = body.child(block);
                    }
                    part_index += 1;
                }
                AssistantPart::Text(text) => {
                    if !text.trim().is_empty() {
                        let mut display = text.clone();
                        if turn.streaming && part_index + 1 == turn.parts.len() {
                            display.push('▍');
                        }
                        body = body.child(render_timeline_text(
                            format!("transcript-assistant-{index}-{part_index}"),
                            display,
                            0xf3f4f6,
                        ));
                    }
                    part_index += 1;
                }
                AssistantPart::Tool(part) => {
                    let mut end = part_index + 1;
                    while end < turn.parts.len() {
                        match &turn.parts[end] {
                            AssistantPart::Tool(next) if next.name == part.name => end += 1,
                            _ => break,
                        }
                    }
                    if end - part_index >= TOOL_GROUP_THRESHOLD {
                        body = body.child(self.render_tool_group(
                            &turn.parts[part_index..end],
                            index,
                            part_index,
                            parent,
                        ));
                    } else {
                        for offset in part_index..end {
                            if let Some(AssistantPart::Tool(part)) = turn.parts.get(offset) {
                                body =
                                    body.child(self.render_tool_part(part, index, offset, parent));
                            }
                        }
                    }
                    part_index = end;
                }
                AssistantPart::Evidence(links) => {
                    body = body.child(Self::render_evidence(links, index, part_index));
                    part_index += 1;
                }
            }
        }
        let last_is_text = matches!(
            turn.parts.last(),
            Some(AssistantPart::Text(text)) if !text.trim().is_empty()
        );
        if turn.streaming && !last_is_text {
            body = body.child(streaming_caret(index));
        }
        body.into_any()
    }

    /// One collapsed row for a run of same-kind tool calls, expandable to the
    /// individual blocks.
    pub(crate) fn render_tool_group(
        &self,
        parts: &[AssistantPart],
        index: usize,
        first_part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let tools = parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Tool(tool) => Some(tool.as_ref()),
                _ => None,
            })
            .collect::<Vec<&ToolPart>>();
        let Some(first) = tools.first() else {
            return div().into_any();
        };
        let failed = tools
            .iter()
            .any(|tool| tool.status == ToolPartStatus::Failed);
        let running = tools.iter().any(|tool| {
            matches!(
                tool.status,
                ToolPartStatus::Running
                    | ToolPartStatus::AwaitingApproval
                    | ToolPartStatus::AwaitingInput
            )
        });
        let status_color = if failed {
            rgb(0xfca5a5)
        } else if running {
            rgb(0x93c5fd)
        } else {
            rgb(0x9ad7bd)
        };
        let status_label = if failed {
            "failed"
        } else if running {
            "running"
        } else {
            "done"
        };
        let key = tool_element_id(index, first_part_index);
        let expanded = running || failed || self.expanded_tool_groups.contains(&key);
        let total_ms = tools.iter().filter_map(|tool| tool.elapsed_ms).sum::<u64>();
        let duration = (total_ms > 0).then(|| format_duration(total_ms));
        let label = tool_group_label(&first.name, tools.len());
        let parent_for_toggle = parent.clone();
        let mut group = div()
            .id(("tool-group", key))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-group-header", key))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!("{label}, {status_label}"))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| this.toggle_tool_group(key, cx));
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(tool_icon(&first.name))
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(div().text_color(rgb(0x64748b)).child(if expanded {
                                "⌄"
                            } else {
                                "›"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(rgb(0xdbeafe))
                                    .child(label),
                            )
                            .child(div().text_color(status_color).child(status_label))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if expanded {
            for offset in first_part_index..(first_part_index + tools.len()) {
                if let Some(AssistantPart::Tool(part)) = parts.get(offset - first_part_index) {
                    group = group.child(self.render_tool_part(part, index, offset, parent));
                }
            }
        }
        group.into_any()
    }

    pub(crate) fn render_evidence(
        links: &[EvidenceText],
        index: usize,
        part_index: usize,
    ) -> gpui_kit::AnyElement {
        let mut list = div().flex().flex_col().gap_1().pt_1();
        for (link_index, link) in links.iter().enumerate() {
            let uri = link.uri.clone();
            let label = if link.label.trim().is_empty() {
                link.uri.clone()
            } else {
                link.label.clone()
            };
            list = list.child(
                Button::new((
                    "evidence",
                    ((index as u64) << 32) | ((part_index as u64) << 16) | link_index as u64,
                ))
                .ghost()
                .small()
                .accessibility_label(format!("Open evidence: {label}"))
                .on_click(move |_, _, _| {
                    let _ = open_external_url(&uri);
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(rgb(0x93c5fd))
                        .child(
                            Icon::new(AssetIconName::ExternalLink)
                                .size_3()
                                .text_color(rgb(0x93c5fd)),
                        )
                        .child(label),
                ),
            );
        }
        list.into_any()
    }

    pub(crate) fn render_tool_part(
        &self,
        part: &ToolPart,
        index: usize,
        part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let status_color = match part.status {
            ToolPartStatus::Failed => rgb(0xfca5a5),
            ToolPartStatus::Completed => rgb(0x9ad7bd),
            ToolPartStatus::AwaitingApproval | ToolPartStatus::AwaitingInput => rgb(0xfef3c7),
            ToolPartStatus::Running => rgb(0x93c5fd),
            ToolPartStatus::Queued | ToolPartStatus::Cancelled => rgb(0x94a3b8),
        };
        let duration = part.elapsed_ms.map(format_duration);
        // Active, failed, and approval-gated work stays open; finished successes
        // collapse so the transcript stays scannable.
        let expanded = self.expanded_tools.contains(&part.id)
            || matches!(
                part.status,
                ToolPartStatus::Running
                    | ToolPartStatus::AwaitingApproval
                    | ToolPartStatus::AwaitingInput
                    | ToolPartStatus::Failed
            );
        let call_id = part.id;
        let parent_for_toggle = parent.clone();
        let awaiting = part.status == ToolPartStatus::AwaitingApproval
            && self
                .pending_approval
                .as_ref()
                .is_some_and(|call| call.id == part.id);
        let patch_summary = part.output.as_deref().and_then(syntax::patch_summary);
        let mut block = div()
            .id(("tool", tool_element_id(index, part_index)))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-header", tool_element_id(index, part_index)))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!("{}: {}", part.title, part.status.label()))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| this.toggle_tool(call_id, cx));
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(tool_icon(&part.name))
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(div().text_color(rgb(0x64748b)).child(if expanded {
                                "⌄"
                            } else {
                                "›"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(mono_font())
                                    .text_color(rgb(0xdbeafe))
                                    .child(part.title.clone()),
                            )
                            .when_some(patch_summary, |element, summary| {
                                element.child(
                                    div()
                                        .font_family(mono_font())
                                        .text_color(rgb(0x64748b))
                                        .child(summary),
                                )
                            })
                            .child(div().text_color(status_color).child(part.status.label()))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if expanded {
            if let Some(detail) = &part.detail {
                block = block.child(
                    div()
                        .ml(px(20.))
                        .pt_1()
                        .font_family(mono_font())
                        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
                        .text_color(rgb(0x94a3b8))
                        .child(detail.clone()),
                );
            }
            if part.output.is_some() {
                let output_id = tool_element_id(index, part_index);
                block = block.child(
                    div()
                        .ml(px(20.))
                        .mt_1()
                        .w_full()
                        .min_w(px(0.))
                        .border_l_1()
                        .border_color(rgb(0x30343f))
                        .pl_2()
                        .text_color(rgb(0x8f98a6))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .py_1()
                                .child(
                                    div()
                                        .font_family(mono_font())
                                        .text_xs()
                                        .text_color(rgb(0x64748b))
                                        .child(tool_output_language(part).label()),
                                )
                                .child(
                                    Button::new(("copy-tool-output", output_id))
                                        .icon(Icon::new(AssetIconName::Copy))
                                        .ghost()
                                        .xsmall()
                                        .accessibility_label("Copy tool output")
                                        .tooltip("Copy output")
                                        .on_click({
                                            let output = part.output.clone().unwrap_or_default();
                                            move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    output.clone(),
                                                ));
                                            }
                                        }),
                                ),
                        )
                        .child(render_tool_output(("tool-output", output_id), part)),
                );
            }
        }
        if awaiting && !self.approval_request_in_flight {
            let parent_for_approve = parent.clone();
            let parent_for_reject = parent.clone();
            block = block.child(
                div()
                    .ml(px(20.))
                    .mt_1()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(("approve-tool", tool_element_id(index, part_index)))
                            .label("Approve")
                            .success()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_approve
                                    .update(cx, |this, cx| this.approve_pending_action(cx));
                            }),
                    )
                    .child(
                        Button::new(("reject-tool", tool_element_id(index, part_index)))
                            .label("Reject")
                            .danger()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_reject
                                    .update(cx, |this, cx| this.reject_pending_action(cx));
                            }),
                    ),
            );
        } else if awaiting && self.approval_request_in_flight {
            block = block.child(
                div()
                    .ml(px(20.))
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x94a3b8))
                    .child("Submitting approval..."),
            );
        }
        block.into_any()
    }

    pub(crate) fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        match item {
            TimelineItem::User(text) => div()
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x60a5fa)))
                        .child(div().text_xs().text_color(rgb(0xbfdbfe)).child("You")),
                )
                .child(div().mt_1().child(render_timeline_text(
                    format!("transcript-user-{index}"),
                    text.clone(),
                    0xf3f4f6,
                )))
                .into_any(),
            TimelineItem::Assistant(turn) => self.render_assistant_turn(turn, index, parent),
            TimelineItem::System(note) => {
                let (surface, accent) = match note.tone {
                    SystemTone::Error => (rgb(ERROR_CARD_SURFACE), rgb(ERROR_CARD_ACCENT)),
                    SystemTone::Input => (rgb(0x241f3b), rgb(0xc4b5fd)),
                    SystemTone::Neutral => (rgb(0x191c22), rgb(0x64748b)),
                };
                let text_color = match note.tone {
                    SystemTone::Error => ERROR_CARD_FOREGROUND,
                    SystemTone::Input => 0xe9d5ff,
                    SystemTone::Neutral => 0x94a3b8,
                };
                let mut card = div()
                    .px_3()
                    .py_2()
                    .rounded_sm()
                    .bg(surface)
                    .text_color(rgb(text_color));
                if let Some(heading) = &note.heading {
                    card = card.child(div().text_xs().text_color(accent).child(heading.clone()));
                }
                card = card.child(render_timeline_text(
                    format!("timeline-system-{index}"),
                    note.text.clone(),
                    text_color,
                ));
                if note.retryable {
                    card = card.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(accent)
                            .child("This operation can be retried."),
                    );
                }
                card.into_any()
            }
            TimelineItem::ProjectMessageContext(text) => div()
                .mx_3()
                .my_1()
                .px_3()
                .py_2()
                .border_l_2()
                .border_color(rgb(0x3b4555))
                .text_color(rgb(0xcbd5e1))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Project agent context"),
                )
                .child(render_timeline_text(
                    format!("transcript-project-context-{index}"),
                    text.clone(),
                    0xcbd5e1,
                ))
                .into_any(),
            TimelineItem::ProjectMessage(message) => {
                let kind = match message.kind {
                    loom_core::AgentMessageKind::Progress => "Progress",
                    loom_core::AgentMessageKind::Result => "Result",
                    loom_core::AgentMessageKind::Question => "Question",
                    loom_core::AgentMessageKind::Blocker => "Blocker",
                    loom_core::AgentMessageKind::Direction => "Direction",
                    loom_core::AgentMessageKind::Answer => "Answer",
                };
                let participant = |session_id: AgentSessionId| {
                    self.sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.name.clone())
                        .unwrap_or_else(|| session_id.to_string())
                };
                let accent = if message.kind == loom_core::AgentMessageKind::Blocker {
                    rgb(0xfbbf24)
                } else if message.kind == loom_core::AgentMessageKind::Result {
                    rgb(0x86efac)
                } else {
                    rgb(0x93c5fd)
                };
                let mut card = div()
                    .mx_3()
                    .my_1()
                    .px_3()
                    .py_1()
                    .border_l_2()
                    .border_color(accent.opacity(0.45))
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                        "{kind} · {} → {} · #{}",
                        participant(message.sender_session_id),
                        participant(message.target_session_id),
                        message.project_sequence
                    )));
                if !message.body.trim().is_empty() {
                    card = card.child(render_timeline_text(
                        format!("project-message-{}", message.message_id),
                        message.body.clone(),
                        0xb7c0d0,
                    ));
                }
                card.into_any()
            }
            TimelineItem::Plan {
                steps,
                completed,
                active,
            } => {
                let mut card = div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Plan"));
                for (index, step) in steps.iter().enumerate() {
                    let index = index as u32;
                    let marker = if completed.contains(&index) {
                        "✓"
                    } else if active == &Some(index) {
                        ">"
                    } else {
                        "○"
                    };
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(if completed.contains(&index) {
                                rgb(0x9ad7bd)
                            } else {
                                rgb(0xb7c0d0)
                            })
                            .child(format!("{marker} {}", step)),
                    );
                }
                card.into_any()
            }
        }
    }

    pub(crate) fn timeline_entity(&mut self, cx: &mut Context<Self>) -> Entity<TimelineView> {
        if let Some(timeline_view) = &self.timeline_view {
            return timeline_view.clone();
        }

        let parent = cx.entity();
        let timeline_view = cx.new(|cx| {
            let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
            TimelineView::new(parent, scroller)
        });
        self.timeline_view = Some(timeline_view.clone());
        timeline_view
    }

    pub(crate) fn render_review(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let title = if self.project_child_review.is_some() {
            "Project child review"
        } else {
            "Changes"
        };
        let mut body = div()
            .when(!layout.phone, |element| {
                element
                    .w(px(220.))
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(0x30343f))
            })
            .when(layout.phone, |element| {
                element
                    .h(px(170.))
                    .w_full()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
            })
            .id("changes-sidebar-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        match self.review.panel {
            ReviewPanel::Changes => {
                if self.project_child_review.is_none() && self.session_repositories.len() > 1 {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child("REPOSITORIES"),
                    );
                    for (index, repository) in self.session_repositories.iter().enumerate() {
                        let selected = self.selected_repository_id == Some(repository.id);
                        let repository_id = repository.id;
                        let name = repository
                            .source
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("Repository");
                        body = body.child(
                            div()
                                .id(("review-repository", index))
                                .p_1()
                                .cursor_pointer()
                                .when(selected, |element| element.bg(rgb(0x293244)))
                                .text_sm()
                                .child(name.to_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_session_repository(repository_id, cx);
                                })),
                        );
                    }
                }
                if let Some(status) = &self.review.vcs {
                    body = body.child(div().mt_2().text_xs().text_color(rgb(0x93c5fd)).child(
                        if self.project_child_review.is_some() {
                            "CHILD WORKTREE CHANGES"
                        } else {
                            "REPOSITORY CHANGES"
                        },
                    ));
                    for (index, file) in status.files.iter().enumerate() {
                        let path = file.path.clone();
                        let project_child_review = self.project_child_review.is_some();
                        let staged = matches!(
                            file.worktree,
                            GitFileStatusKind::Unknown | GitFileStatusKind::Ignored
                        );
                        let selected = self.review.selected_path.as_deref() == Some(&file.path)
                            && self.review.selected_staged == staged;
                        let (additions, deletions) = if staged {
                            (file.index_additions, file.index_deletions)
                        } else {
                            (file.worktree_additions, file.worktree_deletions)
                        };
                        body = body.child(
                            div()
                                .id(("git-file", index))
                                .p_1()
                                .when(selected, |element| element.bg(rgb(0x293244)))
                                .text_sm()
                                .text_color(rgb(0xfef3c7))
                                .cursor_pointer()
                                .child(format!(
                                    "{:?}  {}  +{} −{}",
                                    if staged { file.index } else { file.worktree },
                                    file.path,
                                    additions,
                                    deletions
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !project_child_review {
                                        this.open_review_diff(path.clone(), staged, cx);
                                    }
                                })),
                        );
                        if !staged && file.index != GitFileStatusKind::Unknown {
                            let path = file.path.clone();
                            let project_child_review = self.project_child_review.is_some();
                            body = body.child(
                                div()
                                    .id(("git-staged-file", index))
                                    .p_1()
                                    .pl_3()
                                    .text_xs()
                                    .text_color(rgb(0x93c5fd))
                                    .cursor_pointer()
                                    .child(format!(
                                        "Staged  +{} −{}",
                                        file.index_additions, file.index_deletions
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !project_child_review {
                                            this.open_review_diff(path.clone(), true, cx);
                                        }
                                    })),
                            );
                        }
                    }
                }
                let workspace_changes = if self.project_child_review.is_some() {
                    Vec::new()
                } else {
                    self.review
                        .changes
                        .iter()
                        .enumerate()
                        .filter(|(_, change)| {
                            self.review.repositories_loaded
                                && !belongs_to_repository(&change.path, &self.session_repositories)
                        })
                        .collect::<Vec<_>>()
                };
                if !workspace_changes.is_empty() {
                    body = body.child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child("OTHER WORKSPACE FILES"),
                    );
                }
                for (index, change) in workspace_changes {
                    let path = change.path.clone();
                    body = body.child(
                        div()
                            .id(("review-file", index))
                            .text_sm()
                            .text_color(change_color(change.kind))
                            .cursor_pointer()
                            .child(format!(
                                "{}  {}",
                                change_kind_label(change.kind),
                                change.path
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_review_file(path.clone(), cx);
                                cx.notify();
                            })),
                    );
                }
                if self.project_child_review.is_none() && !self.review.repositories_loaded {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("Loading repositories…"),
                    );
                } else if self.review.changes.is_empty()
                    && self
                        .review
                        .vcs
                        .as_ref()
                        .is_none_or(|status| status.files.is_empty())
                {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No changed files"),
                    );
                }
            }
        }
        let parent = cx.entity();
        let diff_list = list(self.review.list_state.clone(), move |index, _window, cx| {
            let view = parent.read(cx);
            view.render_review_row(index).into_any()
        })
        .size_full();
        let mut detail = div().flex_1().min_w(px(0.)).flex().flex_col();
        if let Some(path) = &self.review.selected_path {
            detail = detail.child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(path.clone()))
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                if self.project_child_review.is_some() {
                                    "Project child checkout · read only"
                                } else if self.review.selected_file.is_some() {
                                    "Current file · no repository diff"
                                } else if self.review.selected_staged {
                                    "Staged changes · read only"
                                } else {
                                    "Working changes · read only"
                                },
                            )),
                    )
                    .when(!self.review.hunk_rows.is_empty(), |header| {
                        header.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new("previous-review-hunk")
                                        .label("Previous hunk")
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_review_hunk(false, cx)
                                        })),
                                )
                                .child(
                                    Button::new("next-review-hunk")
                                        .label("Next hunk")
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_review_hunk(true, cx)
                                        })),
                                ),
                        )
                    }),
            );
        }
        if self.review.loading_diff {
            detail = detail.child(div().p_3().text_sm().child("Loading diff…"));
        } else if let Some(error) = &self.review.diff_error {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        } else if let Some(diff) = &self.review.selected_diff {
            if diff.binary {
                detail = detail.child(
                    div()
                        .p_3()
                        .text_sm()
                        .child("Binary file: no text diff is available."),
                );
            } else if self.review.rows.is_empty() {
                detail = detail.child(div().p_3().text_sm().child(if diff.truncated {
                    "The first changed line exceeds the review size limit."
                } else {
                    "No line changes in this version of the file."
                }));
            } else {
                detail = detail.child(diff_list);
            }
            if diff.truncated {
                detail =
                    detail.child(
                        div().p_2().text_xs().text_color(rgb(0xfef3c7)).child(
                            "Diff exceeds the review size limit; showing the beginning only.",
                        ),
                    );
            }
        } else if let Some(file) = &self.review.selected_file {
            detail = detail.child(
                div()
                    .flex_1()
                    .id("review-file-scroll")
                    .overflow_y_scroll()
                    .p_3()
                    .child(SelectableText::new(
                        "review-file-content",
                        file.content.clone(),
                    )),
            );
        } else {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("Select a changed file to review its diff."),
            );
        }
        let content = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .when(layout.phone, |element| element.flex_col())
            .child(body)
            .child(detail);
        div()
            .when(layout.phone, |element| {
                element.size_full().absolute().top(px(0.)).left(px(0.))
            })
            .when(!layout.phone, |element| element.size_full())
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(0x293244))
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child(title))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!layout.phone, |element| {
                                element.child(
                                    div().text_xs().text_color(rgb(0x8f98a6)).child(
                                        self.review
                                            .vcs
                                            .as_ref()
                                            .map(|status| {
                                                format!(
                                                    "{}  {}",
                                                    status.branch.as_deref().unwrap_or("detached"),
                                                    if status.clean { "clean" } else { "modified" }
                                                )
                                            })
                                            .unwrap_or_else(|| "VCS unavailable".to_owned()),
                                    ),
                                )
                            })
                            .when(!layout.phone, |element| {
                                element.child(
                                    Button::new("close-review")
                                        .icon(Icon::new(IconName::FileText))
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Show changes")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review.panel = ReviewPanel::Changes;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("toggle-review-sidebar-close")
                                    .label("Close")
                                    .ghost()
                                    .small()
                                    .tooltip("Close review panel")
                                    .on_click(cx.listener(Self::close_review)),
                            ),
                    ),
            )
            .child(content)
    }

    pub(crate) fn render_review_row(&self, index: usize) -> gpui_kit::Div {
        let Some(row) = self.review.rows.get(index) else {
            return div().w_full().min_h(px(22.));
        };
        match row {
            ReviewRow::Hunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
            } => div()
                .w_full()
                .px_2()
                .py_1()
                .bg(rgb(0x293244))
                .font_family(mono_font())
                .text_xs()
                .text_color(rgb(0x93c5fd))
                .child(format!(
                    "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
                )),
            ReviewRow::Line(line) => {
                let (marker, background, foreground) = match line.kind {
                    GitDiffLineKind::Added => ("+", 0x24543d, 0xbbf7d0),
                    GitDiffLineKind::Removed => ("−", 0x542936, 0xfecaca),
                    GitDiffLineKind::Context => (" ", 0x17191f, 0xcbd5e1),
                };
                div()
                    .w_full()
                    .min_h(px(22.))
                    .flex()
                    .items_start()
                    .bg(rgb(background))
                    .font_family(mono_font())
                    .text_xs()
                    .text_color(rgb(foreground))
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .text_color(rgb(0x8f98a6))
                            .child(line.old_line.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .text_color(rgb(0x8f98a6))
                            .child(line.new_line.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(div().w(px(18.)).flex_shrink_0().child(marker))
                    .child(div().flex_1().min_w(px(0.)).child(SelectableText::new(
                        ("review-line", index),
                        line.content.clone(),
                    )))
            }
        }
    }

    pub(crate) fn render_composer(
        &mut self,
        layout: ResponsiveLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let placeholder = if self.pending_input.is_some() {
            "Answer the agent question..."
        } else if self.sending_message {
            "Sending direction..."
        } else if self.active_run_id.is_some() {
            "Send follow-up direction..."
        } else {
            "Describe a task, or type / for commands"
        };
        let composer = self
            .composer_input
            .as_ref()
            .expect("composer input initialized before rendering");
        if self.composer_placeholder.as_deref() != Some(placeholder) {
            composer.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
            self.composer_placeholder = Some(placeholder.to_owned());
            // Store the last presented placeholder so updating the component
            // does not keep invalidating the view on every render.
            // This is presentation state only; the text stays in InputState.
        }
        let value = composer.read(cx).value().to_string();
        let height = composer_height(&value);
        let view = cx.entity();
        let completion_rows = match &self.composer_completion {
            Some(completion) if completion.kind == CompletionKind::Command => {
                commands_matching(&completion.query)
                    .into_iter()
                    .map(|command| {
                        (
                            format!("/{}", command.name),
                            command.title.to_owned(),
                            command.description.to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            }
            Some(completion) => self
                .file_completion_candidates()
                .into_iter()
                .filter(|path| path.contains(&completion.query))
                .map(|path| (format!("@{path}"), path, String::new()))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        div()
            .w_full()
            .p_3()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .child(self.render_run_status())
            .when_some(self.context_inspection.as_ref(), |element, inspection| {
                let budget = inspection.budget.effective_input_tokens.map_or_else(
                    || "unknown budget".to_owned(),
                    |limit| format!("{limit} input tokens"),
                );
                let fallback = if inspection
                    .items
                    .iter()
                    .any(|item| item.label.contains("fallback"))
                {
                    " · model limit unknown; conservative estimate"
                } else {
                    ""
                };
                element.child(
                    div()
                        .id("context-usage")
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .mb_2()
                        .child(format!(
                            "Context ≈ {} / {budget} · {} reserved for output{fallback}",
                            inspection.included_tokens, inspection.budget.reserved_output_tokens
                        )),
                )
            })
            .when_some(self.composer_completion.clone(), |element, completion| {
                element.child(
                    div()
                        .id("composer-completions")
                        .test_support()
                        .w_full()
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .rounded_lg()
                        .bg(rgb(0x10141b))
                        .border_1()
                        .border_color(rgb(0x3b4555))
                        .mb_2()
                        .children(completion_rows.into_iter().enumerate().map(
                            |(index, (insert, title, description))| {
                                let selected = index == completion.selected;
                                let view = view.clone();
                                div()
                                    .id(("completion-row", index))
                                    .px_3()
                                    .py_1()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .cursor_pointer()
                                    .when(selected, |element| element.bg(rgb(0x202b3b)))
                                    .hover(|style| style.bg(rgb(0x202b3b)))
                                    .child(
                                        div()
                                            .font_family(mono_font())
                                            .text_sm()
                                            .text_color(rgb(0x93c5fd))
                                            .child(insert),
                                    )
                                    .child(div().text_xs().text_color(rgb(0xe5e7eb)).child(title))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x64748b))
                                            .child(description),
                                    )
                                    .on_click(move |_, window, cx| {
                                        view.update(cx, |this, cx| {
                                            if let Some(completion) =
                                                this.composer_completion.as_mut()
                                            {
                                                completion.selected = index;
                                            }
                                            this.accept_composer_completion(window, cx);
                                        });
                                    })
                            },
                        )),
                )
            })
            .child(
                div()
                    .id("composer-input-box")
                    .w_full()
                    .rounded_lg()
                    .bg(rgb(0x10141b))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .child(
                        div().p_3().child(
                            Textarea::new(composer)
                                .aria_label(placeholder)
                                .h(px(height))
                                .appearance(false)
                                .bordered(false),
                        ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(rgb(0x20242c))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(self.render_agent_mode_picker(layout.phone))
                                    .child(self.render_model_picker(layout.phone))
                                    .child(
                                        Button::new("open-command-palette")
                                            .icon(Icon::new(AssetIconName::Command))
                                            .label("K")
                                            .ghost()
                                            .xsmall()
                                            .tooltip("Open the command palette")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_command_palette(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .when(self.run_can_interrupt(), |element| {
                                        element.child(
                                            Button::new("interrupt-run")
                                                .label("Stop")
                                                .danger()
                                                .small()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.interrupt_active_run(cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("send-message")
                                            .icon(Icon::new(AssetIconName::ArrowUp))
                                            .primary()
                                            .small()
                                            .tooltip("Send (↵)")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_composer(cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }

    /// The single line that carries run progress: spinner, state, elapsed, and
    /// the interrupt hint. Run state lives here instead of the session header.
    pub(crate) fn render_run_status(&self) -> gpui_kit::Div {
        let running = self.run_is_active();
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .mb_2()
            .min_h(px(16.))
            .text_xs();
        if running || self.sending_message || self.pending_input.is_some() {
            let color = self
                .run_state
                .map(run_state_color)
                .unwrap_or_else(|| rgb(0x93c5fd));
            row = row.child(Spinner::new().small().color(color.into()));
        }
        if let Some(state) = self.run_state {
            let label = if state == AgentRunState::Paused && self.project_has_live_children() {
                "Waiting for sub-agents".to_owned()
            } else {
                run_state_label(Some(state)).to_owned()
            };
            row = row.child(div().text_color(run_state_color(state)).child(label));
        } else if self.sending_message {
            row = row.child(div().text_color(rgb(0x93c5fd)).child("Sending"));
        } else if self.pending_input.is_some() {
            row = row.child(
                div()
                    .text_color(rgb(0xfbbf24))
                    .child("Waiting for your answer"),
            );
        } else {
            row = row.child(div().text_color(rgb(0x64748b)).child("Ready"));
        }
        if let Some(elapsed) = self.run_elapsed_label() {
            row = row.child(
                div()
                    .font_family(mono_font())
                    .text_color(rgb(0x64748b))
                    .child(elapsed),
            );
        }
        row = row.child(div().flex_1());
        if running {
            row = row.child(div().text_color(rgb(0x64748b)).child("esc to interrupt"));
        }
        row
    }

    pub(crate) fn run_elapsed_label(&self) -> Option<String> {
        let run = self.active_run.as_ref()?;
        let end = run.completed_at.unwrap_or_else(Timestamp::now);
        let milliseconds = end
            .as_unix_millis()
            .saturating_sub(run.started_at.as_unix_millis());
        Some(format_duration(milliseconds))
    }

    pub(crate) fn render_command_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self
            .command_palette_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let matches = commands_matching(&query);
        let view = cx.entity();
        let rows = matches
            .into_iter()
            .enumerate()
            .map(|(index, command)| {
                let selected = index == self.command_palette_selection;
                let name = command.name;
                let view = view.clone();
                div()
                    .id(("palette-row", index))
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .when(selected, |element| element.bg(rgb(0x202b3b)))
                    .hover(|style| style.bg(rgb(0x202b3b)))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child(command.title),
                    )
                    .when_some(command.shortcut, |element, shortcut| {
                        element.child(div().text_xs().text_color(rgb(0x64748b)).child(shortcut))
                    })
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.command_palette_open = false;
                            this.command_palette_selection = 0;
                            this.run_command(name, None, cx);
                        });
                    })
            })
            .collect::<Vec<_>>();
        div()
            .id("command-palette-backdrop")
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(110.))
            .bg(gpui_kit::hsla(0., 0., 0., 0.5))
            .on_click(cx.listener(|this, _, _, cx| this.close_command_palette(cx)))
            .child(
                div()
                    .id("command-palette")
                    .test_support()
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .bg(rgb(0x1b1d24))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .shadow_lg()
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(rgb(0x293244))
                            .child(
                                Icon::new(AssetIconName::Command)
                                    .size_4()
                                    .text_color(rgb(0x64748b)),
                            )
                            .child(div().flex_1().when_some(
                                self.command_palette_input.as_ref(),
                                |element, input| {
                                    element.child(KitInput::new(input).id("command-palette-input"))
                                },
                            )),
                    )
                    .child(
                        div()
                            .id("command-palette-list")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            )
    }

    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.rename_dialog else {
            return div().into_any();
        };
        div()
            .id("rename-dialog")
            .absolute()
            .top(px(120.))
            .left(px(280.))
            .w(px(420.))
            .p_3()
            .rounded_lg()
            .bg(rgb(0x1b1d24))
            .border_1()
            .border_color(rgb(0x3b4555))
            .shadow_lg()
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child(if dialog.is_project {
                        "Rename project"
                    } else {
                        "Rename session"
                    }),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(format!("Current name: {}", dialog.session.name)),
            )
            .child(
                div().mt_3().child(
                    KitInput::new(
                        self.rename_input_state
                            .as_ref()
                            .expect("rename input initialized before rendering"),
                    )
                    .id("rename-session-input")
                    .small(),
                ),
            )
            .child(
                div()
                    .mt_3()
                    .flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        div()
                            .id("cancel-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x242833))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_sm()
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("confirm-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x2563eb))
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child("Rename")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_rename(cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_source_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.source_dialog else {
            return div().into_any();
        };
        let is_start = dialog.purpose == SessionSourceDialogPurpose::StartSession;
        let selected_repo = dialog.selected_repository.as_deref();
        let repository_query = self
            .repository_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let mut repository_rows = div().mt_2().flex().flex_col().gap_1();
        let mut filtered_repository_count = 0;
        for repository in &dialog.repositories {
            let searchable = format!(
                "{} {}",
                repository.full_name,
                repository.description.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            if !searchable.contains(&repository_query) {
                continue;
            }
            filtered_repository_count += 1;
            let name = repository.full_name.clone();
            let selected = selected_repo == Some(name.as_str());
            repository_rows = repository_rows.child(
                div()
                    .id(format!("github-repository-{name}"))
                    .p_2()
                    .rounded_sm()
                    .bg(if selected {
                        rgb(0x263b58)
                    } else {
                        rgb(0x171c25)
                    })
                    .border_1()
                    .border_color(if selected {
                        rgb(0x2563eb)
                    } else {
                        rgb(0x293244)
                    })
                    .cursor_pointer()
                    .child(div().text_sm().child(name.clone()))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                        repository.description.clone().unwrap_or_else(|| {
                            if repository.private {
                                "Private repository"
                            } else {
                                "Public repository"
                            }
                            .to_owned()
                        }),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(dialog) = &mut this.source_dialog {
                            dialog.selected_repository = Some(name.clone());
                        }
                        cx.notify();
                    })),
            );
        }

        let mut dialog_body = div().mt_3();
        if dialog.choice == SessionSourceChoice::LocalDirectory {
            dialog_body = dialog_body
                .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                    "Attach a local folder in place. Git repositories in that folder are available for review.",
                ))
                .child(
                    div()
                        .mt_2()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(34.))
                                .px_2()
                                .rounded_sm()
                                .bg(rgb(0x0f1115))
                                .border_1()
                                .border_color(rgb(0x3b4555))
                                .child(
                                    KitInput::new(
                                        self.source_path_input
                                            .as_ref()
                                            .expect("source input initialized before rendering"),
                                    )
                                    .id("local-session-directory-path")
                                    .appearance(false)
                                    .bordered(false),
                                ),
                        )
                        .when(cfg!(not(target_family = "wasm")), |row| {
                            row.child(
                                Button::new("browse-local-session-directory")
                                    .label("Browse…")
                                    .small()
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.browse_local_directory(cx);
                                    })),
                            )
                        }),
                );
        } else if dialog.choice == SessionSourceChoice::GitHub {
            dialog_body = if dialog.repositories_loading {
                dialog_body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("Loading repositories…"),
                )
            } else if let Some(error) = &dialog.error {
                dialog_body
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xfca5a5))
                            .child(error.clone()),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("Connect GitHub in Settings to browse repositories."),
                    )
                    .child(
                        Button::new("connect-github-from-repository-picker")
                            .label("Open GitHub settings")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_dialog = None;
                                this.open_settings_from_menu(cx);
                            })),
                    )
            } else if dialog.repositories.is_empty() {
                dialog_body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("No repositories found."),
                )
            } else {
                dialog_body
                    .child(
                        KitInput::new(
                            self.repository_filter_input
                                .as_ref()
                                .expect("repository filter initialized before rendering"),
                        )
                        .id("github-repository-filter")
                        .small()
                        .into_any_element(),
                    )
                    .child(if filtered_repository_count == 0 {
                        div()
                            .mt_2()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No repositories match this filter.")
                            .into_any_element()
                    } else {
                        div()
                            .max_h(px(280.))
                            .overflow_y_scrollbar()
                            .child(repository_rows)
                            .into_any_element()
                    })
            };
        } else {
            dialog_body = dialog_body
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Start a new project with no files or repositories.");
        }
        if let Some(error) = &dialog.error
            && dialog.choice != SessionSourceChoice::GitHub
        {
            dialog_body = dialog_body.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        }

        Dialog::new(cx)
            .title(if is_start {
                "New project"
            } else {
                "Add to this session"
            })
            .on_close(cx.listener(|this, _, _, cx| {
                this.source_dialog = None;
                cx.notify();
            }))
            .keyboard(false)
            .overlay_closable(false)
            .w(px(560.))
            .max_h(px(600.))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(if is_start {
                                "Choose what the new project starts with."
                            } else {
                                "Choose a repository or folder to add to the active session."
                            }),
                    )
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .gap_1()
                            .when(is_start, |row| {
                                row.child(
                                    Button::new("source-empty")
                                        .label("Empty project")
                                        .small()
                                        .when(
                                            dialog.choice == SessionSourceChoice::Empty,
                                            |button| button.primary(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_source(SessionSourceChoice::Empty, cx)
                                        })),
                                )
                            })
                            .when(dialog.local_directory_available, |row| {
                                row.child(
                                    Button::new("source-local-directory")
                                        .label("Local folder")
                                        .small()
                                        .when(
                                            dialog.choice == SessionSourceChoice::LocalDirectory,
                                            |button| button.primary(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_source(
                                                SessionSourceChoice::LocalDirectory,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .child(
                                Button::new("source-github")
                                    .label("GitHub repository")
                                    .small()
                                    .when(dialog.choice == SessionSourceChoice::GitHub, |button| {
                                        button.primary()
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_source(SessionSourceChoice::GitHub, cx)
                                    })),
                            ),
                    )
                    .child(dialog_body)
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .justify_end()
                            .gap_1()
                            .child(
                                Button::new("cancel-session-source")
                                    .label("Cancel")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.source_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-session-source")
                                    .label(if is_start {
                                        "Create project"
                                    } else {
                                        "Add to session"
                                    })
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_source_dialog(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_github_login_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = &self.github_login else {
            return div().into_any();
        };
        let body = match state {
            GitHubLoginState::Starting => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Requesting a GitHub device code..."),
            GitHubLoginState::Awaiting {
                verification_uri,
                user_code,
                expires_in,
            } => div()
                .text_sm()
                .text_color(rgb(0xe5e7eb))
                .child("Open this URL in a browser:")
                .child(
                    div()
                        .mt_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0x93c5fd))
                                .child(verification_uri.clone()),
                        )
                        .child(
                            div()
                                .id("open-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x2563eb))
                                .hover(|style| style.bg(rgb(0x1d4ed8)))
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .cursor_pointer()
                                .child("Open")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        if let Err(error) = open_external_url(&verification_uri) {
                                            this.record_status(format!(
                                                "Could not open GitHub URL: {error}"
                                            ));
                                        }
                                        cx.notify();
                                    })
                                }),
                        )
                        .child(
                            div()
                                .id("copy-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            verification_uri.clone(),
                                            "verification URL",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0xfef3c7))
                                .child(format!("Enter code: {user_code}")),
                        )
                        .child(
                            div()
                                .id("copy-github-user-code")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy code")
                                .on_click({
                                    let user_code = user_code.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            user_code.clone(),
                                            "device code",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(format!(
                            "Waiting for authorization (expires in {expires_in}s)"
                        )),
                ),
            #[cfg(not(target_family = "wasm"))]
            GitHubLoginState::Completing => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Authorization received. Saving credentials..."),
            GitHubLoginState::Success => div()
                .text_sm()
                .text_color(rgb(0x9ad7bd))
                .child("GitHub is connected. Repository browsing and the GitHub Copilot provider are available."),
            GitHubLoginState::Error(error) => div()
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(error.clone()),
        };
        div()
            .id("github-login-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("Connect GitHub account"),
                    )
                    .child(
                        div()
                            .id("close-github-login")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close GitHub connection".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.github_login = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().mt_4().child(body))
            .into_any()
    }

    pub(crate) fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let default_model_body = self.default_model_select.as_ref().map_or_else(
            || div().into_any_element(),
            |state| {
                Select::new(state)
                    .id("default-model-select")
                    .w_full()
                    .small()
                    .accessibility_label("Default model for new sessions")
                    .placeholder("No configured models are available")
                    .search_placeholder("Search models")
                    .into_any_element()
            },
        );

        let section = self.settings_section;
        let content: gpui_kit::AnyElement = match section {
            SettingsSection::Agents => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("AGENTS"))
                .child(
                    settings_card()
                        .child(settings_row(
                            "Auto-approve non-destructive actions",
                            "In Agent and Edit, writes, commands, and network won't prompt",
                            Switch::new("session-auto-approve-toggle")
                                .checked(self.auto_approve_actions)
                                .disabled(
                                    !self.is_connected()
                                        || self.approval_settings_request_in_flight,
                                )
                                .accessibility_label("Auto-approve non-destructive actions")
                                .on_change({
                                    let view = cx.entity();
                                    move |_checked, _window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.toggle_auto_approve_actions(cx);
                                        });
                                    }
                                }),
                            true,
                        ))
                        .child(settings_row(
                            "Default model for new sessions",
                            "Used when a session has no model of its own",
                            div().w(px(320.)).child(default_model_body),
                            false,
                        ))
                        .child(settings_row(
                            "Session indicator pulse threshold",
                            "Pulse when CPU usage is above this value",
                            settings_stepper(
                                Button::new("cpu-pulse-threshold-decrease")
                                    .label("-")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.cpu_pulse_threshold_percent
                                                == 0,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_cpu_pulse_threshold(-1, cx);
                                    })),
                                format!(
                                    "{}%",
                                    self.workspace_config.cpu_pulse_threshold_percent.min(100)
                                ),
                                Button::new("cpu-pulse-threshold-increase")
                                    .label("+")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.cpu_pulse_threshold_percent
                                                >= 100,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_cpu_pulse_threshold(1, cx);
                                    })),
                            ),
                            false,
                        ))
                        .child(settings_row(
                            "Parallel project agents",
                            "Maximum delegated agents running at once",
                            settings_stepper(
                                Button::new("project-agent-concurrency-decrease")
                                    .label("-")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.project_agent_concurrency
                                                <= loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_project_agent_concurrency(-1, cx);
                                    })),
                                self.workspace_config.project_agent_concurrency.to_string(),
                                Button::new("project-agent-concurrency-increase")
                                    .label("+")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.project_agent_concurrency
                                                >= loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_project_agent_concurrency(1, cx);
                                    })),
                            ),
                            false,
                        )),
                )
                .into_any_element(),
            SettingsSection::Providers => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("PROVIDERS"))
                .child(
                    settings_card().child(
                        div()
                            .w_full()
                            .flex()
                            .items_start()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .child(
                                div()
                                    .w(px(30.))
                                    .h(px(30.))
                                    .flex_shrink_0()
                                    .rounded_md()
                                    .bg(rgb(0x20242c))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(AssetIconName::Globe)
                                            .size_4()
                                            .text_color(rgb(0x93c5fd)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(div().text_sm().child("GitHub"))
                                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                        "Browse and clone repositories, and add GitHub \
                                                 Copilot as a model provider.",
                                    )),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(if self.github_connected {
                                                rgb(0x9ad7bd)
                                            } else {
                                                rgb(0xfef3c7)
                                            })
                                            .child(if self.github_connected {
                                                "Connected"
                                            } else {
                                                "Not connected"
                                            }),
                                    )
                                    .when(
                                        !self.github_connected && self.login_enabled,
                                        |element| {
                                            element.child(
                                                Button::new("connect-github-account")
                                                    .label("Connect GitHub")
                                                    .small()
                                                    .on_click(
                                                        cx.listener(Self::toggle_github_login),
                                                    ),
                                            )
                                        },
                                    ),
                            ),
                    ),
                )
                .into_any_element(),
            SettingsSection::Workers => {
                let mut card = settings_card();
                if self.worker_nodes.is_empty() {
                    card = card.child(
                        div()
                            .px_4()
                            .py_3()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("No worker connected. Add one below to load your sessions."),
                    );
                }
                card = card.children(self.worker_nodes.iter().enumerate().map(|(index, node)| {
                    let id = node.id;
                    let status = &node.status;
                    let node_id = status.node_id.clone();
                    let resources = &status.resources;
                    let connection_label = match node.connection_state {
                        WorkerConnectionState::Disconnected => "not connected",
                        WorkerConnectionState::Connecting => "connecting",
                        WorkerConnectionState::Connected if status.online => "connected · online",
                        WorkerConnectionState::Connected => "connected · offline",
                        WorkerConnectionState::Failed => "connection failed",
                    };
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_3()
                        .when(index > 0, |element| {
                            element.border_t_1().border_color(rgb(0x242833))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(div().text_sm().text_color(rgb(0xe5e7eb)).child(format!(
                                    "{} · {}",
                                    worker_node_display_name(node),
                                    connection_label,
                                )))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x8f98a6))
                                        .child(format_worker_node_resources(resources)),
                                )
                                .when_some(node.connection_detail.as_deref(), |element, detail| {
                                    element.child(
                                        div()
                                            .mt_1()
                                            .text_xs()
                                            .text_color(
                                                if node.connection_state
                                                    == WorkerConnectionState::Failed
                                                {
                                                    rgb(0xfca5a5)
                                                } else {
                                                    rgb(0xfcd34d)
                                                },
                                            )
                                            .child(detail.to_owned()),
                                    )
                                }),
                        )
                        .when(!node.is_local, |element| {
                            element.child(
                                Button::new(format!("remove-worker-node-{id}"))
                                    .label("Remove")
                                    .small()
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.remove_worker_node(id, cx)
                                    })),
                            )
                        })
                        .when(
                            !node.is_local && self.node_backends.contains_key(&node_id),
                            |element| {
                                element.child(
                                    Button::new(format!("worker-node-providers-{id}"))
                                        .label("Providers")
                                        .small()
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.open_providers_for_node(node_id.clone(), cx)
                                        })),
                                )
                            },
                        )
                }));

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_section_heading("WORKERS"))
                    .when_some(self.browser_startup_error.as_deref(), |element, error| {
                        element.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xfca5a5))
                                .child(error.to_owned()),
                        )
                    })
                    .child(card)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div().flex_1().child(
                                    KitInput::new(self.node_input_state.as_ref().expect(
                                        "worker connection input initialized before rendering",
                                    ))
                                    .id("worker-node-connection-input")
                                    .small(),
                                ),
                            )
                            .child(
                                Button::new("connect-worker-node")
                                    .label("Connect")
                                    .small()
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.connect_worker_node(cx)),
                                    ),
                            ),
                    )
                    .child(div().text_xs().text_color(rgb(0x64748b)).child(
                        "Use: ws://host:port/ws token · URLs are shared; access tokens are not",
                    ))
                    .into_any_element()
            }
            SettingsSection::Appearance => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("APPEARANCE"))
                .child(
                    settings_card()
                        .child(settings_row(
                            "Theme",
                            "Follow the system or choose a palette",
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .p_1()
                                .rounded_md()
                                .bg(rgb(0x20242c))
                                .children(ThemeChoice::ALL.into_iter().enumerate().map(
                                    |(index, choice)| {
                                        let selected = choice == self.theme_choice;
                                        div()
                                            .id(("theme-choice", index))
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .bg(if selected {
                                                rgb(0x263b58)
                                            } else {
                                                rgb(0x20242c)
                                            })
                                            .text_xs()
                                            .text_color(if selected {
                                                rgb(0xe5e7eb)
                                            } else {
                                                rgb(0xb7c0d0)
                                            })
                                            .child(choice.label())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.select_theme(choice, window, cx);
                                            }))
                                    },
                                )),
                            true,
                        ))
                        .child(settings_row(
                            "Font size",
                            "Relative to the system display scale",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Button::new("font-scale-decrease")
                                        .label("−")
                                        .small()
                                        .disabled(self.font_scale_percent <= MIN_FONT_SCALE_PERCENT)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.adjust_font_scale(
                                                -FONT_SCALE_STEP_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    div()
                                        .w(px(52.))
                                        .text_center()
                                        .text_sm()
                                        .child(format!("{}%", self.font_scale_percent)),
                                )
                                .child(
                                    Button::new("font-scale-increase")
                                        .label("+")
                                        .small()
                                        .disabled(self.font_scale_percent >= MAX_FONT_SCALE_PERCENT)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.adjust_font_scale(
                                                FONT_SCALE_STEP_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new("font-scale-reset")
                                        .label("Reset")
                                        .ghost()
                                        .small()
                                        .disabled(
                                            self.font_scale_percent == DEFAULT_FONT_SCALE_PERCENT,
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.set_font_scale_percent(
                                                DEFAULT_FONT_SCALE_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                            false,
                        )),
                )
                .into_any_element(),
        };

        div()
            .id("settings-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(rgb(0x242833))
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Settings"))
                    .child(
                        div()
                            .id("close-settings")
                            .test_support()
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close settings".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_settings)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(settings_nav(section, cx))
                    .child(
                        div()
                            .id("settings-content")
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .p_6()
                            .overflow_y_scroll()
                            .child(content),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_about_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("about-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("About Loom"),
                    )
                    .child(
                        div()
                            .id("close-about")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close about".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_about)),
                    ),
            )
            .child(
                div()
                    .mt_8()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().text_color(rgb(0xf3f4f6)).child("Loom"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("Local agent"),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x64748b))
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_providers_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let node_name = self
            .providers_node_id
            .as_ref()
            .and_then(|node_id| self.node_names.get(node_id))
            .map_or("worker", String::as_str);
        let github_provider = self
            .providers
            .iter()
            .find(|provider| provider.kind == ProviderKind::GitHubCopilot);
        let local_providers = self
            .providers
            .iter()
            .filter(|provider| provider.kind != ProviderKind::GitHubCopilot)
            .collect::<Vec<_>>();
        let github_status = if self.github_connected {
            "Connected"
        } else {
            "Not connected"
        };
        let github_models = github_provider.map_or(0, |provider| provider.models.len());

        let mut local_body = div().flex().flex_col().gap_1();
        if local_providers.is_empty() {
            local_body = local_body.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x171c25))
                    .border_1()
                    .border_color(rgb(0x293244))
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No other providers are configured."),
            );
        } else {
            for provider in local_providers {
                let api_key_configurable = provider.api_key_configurable;
                let provider_id = provider.id.clone();
                let mut provider_card = div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x171c25))
                    .border_1()
                    .border_color(rgb(0x293244))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child(provider.display_name.clone()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(format!(
                                "{} model{} available",
                                provider.models.len(),
                                if provider.models.len() == 1 { "" } else { "s" }
                            )),
                    )
                    .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                        if provider.credential_id.is_some() {
                            "API key configured".to_owned()
                        } else {
                            "API key not configured".to_owned()
                        },
                    ));
                if api_key_configurable {
                    if let Some(input) = self.provider_api_key_inputs.get(&provider.id) {
                        provider_card = provider_card.child(
                            div().mt_2().child(
                                KitInput::new(input)
                                    .id(format!("provider-api-key-input-{}", provider.id.as_str()))
                                    .small(),
                            ),
                        );
                    }
                    if let Some(status) = self.provider_setup_status.get(&provider.id) {
                        provider_card = provider_card.child(
                            div()
                                .id(format!("provider-setup-status-{}", provider.id.as_str()))
                                .mt_2()
                                .text_xs()
                                .text_color(rgb(0x8f98a6))
                                .child(status.clone()),
                        );
                    }
                    provider_card = provider_card.child(
                        div().mt_2().child(
                            Button::new(format!("configure-api-key-{}", provider.id.as_str()))
                                .label("Save API key")
                                .small()
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.configure_api_key_provider(
                                        provider_id.clone(),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                    );
                    if provider.credential_id.is_some() {
                        let node_id = self
                            .providers_node_id
                            .clone()
                            .unwrap_or_else(|| self.default_backend_node_id.clone());
                        provider_card = provider_card.child(
                            div()
                                .id(format!("refresh-provider-models-{}", provider.id.as_str()))
                                .mt_1()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x242833))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_sm()
                                .cursor_pointer()
                                .child("Refresh models")
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.refresh_models_for_node_async(node_id.clone(), cx);
                                })),
                        );
                    }
                }
                local_body = local_body.child(provider_card);
            }
        }

        let github_card =
            div()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x171c25))
                .border_1()
                .border_color(rgb(0x293244))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xf3f4f6))
                                .child("GitHub Copilot"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if self.github_connected {
                                    rgb(0x9ad7bd)
                                } else {
                                    rgb(0xfef3c7)
                                })
                                .child(github_status),
                        ),
                )
                .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                    if github_models == 0 {
                        "Connect your GitHub account to use Copilot models.".to_owned()
                    } else {
                        format!(
                            "{} model{} available",
                            github_models,
                            if github_models == 1 { "" } else { "s" }
                        )
                    },
                ))
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Manage GitHub authentication in Settings. Connecting GitHub also adds this model provider."),
                );

        div()
            .id("providers-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Providers"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Configured on {node_name}")),
                            ),
                    )
                    .child(
                        div()
                            .id("close-providers")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close providers".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_providers)),
                    ),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("GITHUB COPILOT"),
            )
            .child(div().mt_2().child(github_card))
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("LOCAL PROVIDER"),
            )
            .child(div().mt_2().child(local_body))
            .into_any()
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
                                .small()
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
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Projects"))
                    .child(self.render_new_session_button(view, cx)),
            )
            .when_some(self.session_filter_input.as_ref(), |element, input| {
                element.child(KitInput::new(input).id("session-filter").small())
            })
            .child(
                div()
                    .flex_1()
                    .id("session-list")
                    .overflow_y_scroll()
                    .child(self.render_session_list(cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(0x30343f))
                    .pt_2()
                    .child(
                        Button::new("account-menu")
                            .icon(Icon::new(IconName::User))
                            .ghost()
                            .small()
                            .accessibility_label("Account")
                            .dropdown_menu({
                                let view = view.clone();
                                move |menu, _, _| {
                                    let providers_view = view.clone();
                                    let about_view = view.clone();
                                    menu.item(PopupMenuItem::new("Providers").on_click(
                                        move |_, _, cx| {
                                            providers_view.update(cx, |view, cx| {
                                                view.open_providers_from_menu(cx);
                                            });
                                        },
                                    ))
                                    .item(
                                        PopupMenuItem::new("About Loom").on_click(
                                            move |_, _, cx| {
                                                about_view.update(cx, |view, cx| {
                                                    view.open_about_from_menu(cx);
                                                });
                                            },
                                        ),
                                    )
                                }
                            }),
                    )
                    .child(
                        Button::new("settings-button")
                            .icon(Icon::new(IconName::Settings))
                            .ghost()
                            .small()
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
            )
    }

    #[cfg(target_family = "wasm")]
    #[cfg(target_family = "wasm")]
    pub(crate) fn render_disconnected(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let layout = responsive_layout(window.bounds().size.width);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .h(px(30.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(rgb(0x1b1d24))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Loom")),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .relative()
                    .overflow_hidden()
                    .when(!layout.phone, |row| {
                        row.child(
                            div()
                                .w(layout.sidebar_width)
                                .h_full()
                                .p_2()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .bg(rgb(0x17191f))
                                .border_r_1()
                                .border_color(rgb(0x30343f))
                                .child(div().flex().items_center().justify_between().child(
                                    div().text_sm().text_color(rgb(0xf3f4f6)).child("Projects"),
                                ))
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(0x8f98a6))
                                        .child("Projects"),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .p_3()
                                        .rounded_lg()
                                        .bg(rgb(0x111318))
                                        .border_1()
                                        .border_color(rgb(0x293244))
                                        .text_sm()
                                        .text_color(rgb(0x64748b))
                                        .child("Connect a worker to load projects."),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .justify_end()
                                        .border_t_1()
                                        .border_color(rgb(0x30343f))
                                        .pt_2()
                                        .child(
                                            Button::new("disconnected-settings")
                                                .icon(Icon::new(IconName::Settings))
                                                .ghost()
                                                .xsmall()
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.open_settings_from_menu(cx);
                                                })),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(
                                div()
                                    .w_full()
                                    .px_3()
                                    .py_2()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .bg(rgb(0x14161a))
                                    .border_b_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div().flex().flex_col().child("No worker connected").child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0x8f98a6))
                                                .child("Connect a worker in Settings to begin."),
                                        ),
                                    )
                                    .child(
                                        Button::new("disconnected-open-settings")
                                            .icon(Icon::new(IconName::Settings))
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.open_settings_from_menu(cx);
                                            })),
                                    ),
                            )
                            .child(div().flex_1().flex().items_center().justify_center())
                            .child(
                                div()
                                    .px_4()
                                    .py_3()
                                    .border_t_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div()
                                            .p_3()
                                            .rounded_lg()
                                            .bg(rgb(0x171c25))
                                            .border_1()
                                            .border_color(rgb(0x293244))
                                            .text_sm()
                                            .text_color(rgb(0x64748b))
                                            .child("Connect a worker to create a project."),
                                    ),
                            )
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(cx))
                            }),
                    ),
            )
            .child(
                div()
                    .h(px(24.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(rgb(0x1b1d24))
                    .border_t_1()
                    .border_color(rgb(0x30343f))
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child("Not connected  ·  Connect a worker in Settings"),
            )
            .when(
                !self.settings_open && !self.welcome_dialog_dismissed,
                |element| {
                    element.child(
                    Dialog::new(cx)
                        .title("You’re ready to go")
                        .on_close(cx.listener(|view, _, _, cx| {
                            view.welcome_dialog_dismissed = true;
                            cx.notify();
                        }))
                        .keyboard(false)
                        .overlay_closable(false)
                        .w(px(460.))
                        .child(div().text_sm().text_color(rgb(0x8f98a6)).child(
                            "Connect a Loom worker from Settings to load your sessions and models.",
                        ))
                        .child(
                            Button::new("disconnected-connect-worker")
                                .label("Open Settings")
                                .small()
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.open_settings_from_menu(cx);
                                })),
                        ),
                )
                },
            )
            .into_any()
    }
}

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(px(
            BASE_FONT_SIZE * self.font_scale_percent as f32 / DEFAULT_FONT_SCALE_PERCENT as f32
        ));
        window.set_window_title(&format!("Loom - {}", self.active_session.name));
        #[cfg(target_family = "wasm")]
        if !self.browser_window_initialized {
            self.browser_window_initialized = true;
            self.observe_system_appearance(window, cx);
            self.composer_focus_handle.focus(window, cx);
            self.select_theme(ThemeChoice::System, window, cx);
        }
        if self.composer_input.is_none() {
            let input = cx.new(|cx| TextareaState::new(window, cx));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => view.refresh_composer_completion(cx),
                    InputEvent::PressEnter { shift: false, .. } => {
                        if view.composer_completion.is_some() {
                            view.pending_completion_accept = true;
                            cx.notify();
                        } else {
                            view.submit_composer(cx);
                        }
                    }
                    _ => {}
                },
            ));
            self.composer_input = Some(input);
        }
        if self.clear_composer_on_render {
            if let Some(input) = self.composer_input.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.clear_composer_on_render = false;
        }
        if self.pending_completion_accept {
            self.pending_completion_accept = false;
            self.accept_composer_completion(window, cx);
        }
        if self.command_palette_open && self.command_palette_input.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Type a command or search…"));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => view.confirm_command_palette(cx),
                    _ => {}
                },
            ));
            self.command_palette_input = Some(input);
        } else if !self.command_palette_open && self.command_palette_input.is_some() {
            if let Some(input) = self.command_palette_input.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.command_palette_input = None;
        }
        if self.session_filter_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects…"));
            self.input_subscriptions
                .push(cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
            self.session_filter_input = Some(input);
        }
        if self.node_input_state.is_none() {
            let initial_value = self.node_input_initial.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(initial_value));
            if self.settings_open {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.connect_worker_node(cx);
                    }
                },
            ));
            self.node_input_state = Some(input);
        }
        if self.clear_node_on_render {
            if let Some(input) = self.node_input_state.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.clear_node_on_render = false;
        }
        #[cfg(target_family = "wasm")]
        if !self.connected {
            return self.render_disconnected(window, cx);
        }
        if self.rename_dialog.is_some() && self.rename_input_state.is_none() {
            let initial_value = self
                .rename_dialog
                .as_ref()
                .map(|dialog| dialog.input.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(initial_value)
                    .placeholder("Session name")
            });
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_rename(cx);
                    }
                },
            ));
            self.rename_input_state = Some(input);
        }
        if self.providers_open {
            for provider in self
                .providers
                .iter()
                .filter(|provider| provider.api_key_configurable)
            {
                if !self.provider_api_key_inputs.contains_key(&provider.id) {
                    let placeholder = format!("{} API key", provider.display_name);
                    self.provider_api_key_inputs.insert(
                        provider.id.clone(),
                        cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(placeholder)
                                .masked(true)
                        }),
                    );
                }
            }
        } else {
            for input in self.provider_api_key_inputs.values() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.provider_api_key_inputs.clear();
            self.provider_setup_status.clear();
        }
        if let Some(path) = self.pending_source_path.take() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(path)
                    .placeholder("Absolute folder path")
            });
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_source_dialog(cx);
                    }
                },
            ));
            self.source_path_input = Some(input);
        }
        if self
            .source_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.choice == SessionSourceChoice::LocalDirectory)
            && self.source_path_input.is_none()
            && self.pending_source_path.is_none()
        {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Absolute folder path"));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_source_dialog(cx);
                    }
                },
            ));
            self.source_path_input = Some(input);
        }
        if self.source_dialog.as_ref().is_some_and(|dialog| {
            dialog.choice == SessionSourceChoice::GitHub && dialog.filter_subscription.is_none()
        }) {
            let filter = cx.new(|cx| {
                InputState::new(window, cx).placeholder("Filter by repository name or description")
            });
            filter.update(cx, |state, cx| state.focus(window, cx));
            let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
            self.repository_filter_input = Some(filter);
            if let Some(dialog) = &mut self.source_dialog {
                dialog.filter_subscription = Some(subscription);
            }
        }
        self.schedule_worker_node_poll(cx);
        self.sync_model_select_states(window, cx);
        self.sync_agent_mode_select_state(window, cx);
        self.schedule_run_poll(cx);
        if self.project_messages_stale {
            self.refresh_project_messages(cx);
        }
        self.schedule_project_poll(cx);
        let view = cx.entity();
        let layout = responsive_layout(window.bounds().size.width);
        let review_panel_visible = review_panel_is_visible(
            layout,
            self.review.open,
            self.sessions.len(),
            self.settings_open,
            self.about_open,
            self.providers_open,
            self.github_login.is_some(),
        );
        let panel_layout = h_resizable("loom-workspace-panels")
            .with_handle_appearance(Rc::new(|handle, _, _| {
                let active = handle.is_active();
                let line = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(2.))
                    .w(px(1.))
                    .bg(rgb(0x30343f));
                let grip = div()
                    .w(px(5.))
                    .h(px(28.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(0x60a5fa))
                    .opacity(0.)
                    .group_hover("handle", |element| element.opacity(1.))
                    .when(active, |element| element.opacity(1.).h(px(40.)));
                Some(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .relative()
                                .w(px(5.))
                                .h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(line)
                                .child(grip),
                        )
                        .into_any(),
                )
            }))
            .children([
                resizable_panel()
                    .size(layout.sidebar_width)
                    .size_range(px(150.)..px(420.))
                    .flex_none()
                    .visible(!layout.phone)
                    .child(div().size_full().when(!layout.phone, |element| {
                        element.child(self.render_session_sidebar(&view, layout, cx))
                    })),
                resizable_panel().min_w(px(0.)).child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(0x14161a))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(layout.phone, |element| {
                                element.child(
                                    Button::new("open-session-drawer")
                                        .label("Projects")
                                        .small()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.session_drawer_open = true;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex_col()
                                    .child(self.active_session.name.clone())
                                    .when(!layout.phone, |element| {
                                        element.child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0x8f98a6))
                                                .child(
                                                    self.review
                                                        .vcs
                                                        .as_ref()
                                                        .map(|status| {
                                                            format!(
                                                                "{}  ·  {}",
                                                                status
                                                                    .branch
                                                                    .as_deref()
                                                                    .unwrap_or("detached"),
                                                                if status.clean {
                                                                    "clean"
                                                                } else {
                                                                    "modified"
                                                                }
                                                            )
                                                        })
                                                        .unwrap_or_else(|| {
                                                            let sources = self
                                                                .session_directories
                                                                .len()
                                                                + self.session_repositories.len();
                                                            match sources {
                                                                0 => "No source attached".to_owned(),
                                                                1 => "1 source".to_owned(),
                                                                count => format!("{count} sources"),
                                                            }
                                                        }),
                                                ),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        session_header_actions()
                            .child(header_tooltip("session-sources-tooltip", "Session sources",
                        Button::new("session-sources")
                            .icon(Icon::new(AssetIconName::ListTree))
                            .ghost()
                            .small()
                            .accessibility_label("Session sources")
                            .dropdown_menu({
                                let view = view.clone();
                                let repositories = self.session_repositories.clone();
                                let directories = self.session_directories.clone();
                                let selected_repository_id = self.selected_repository_id;
                                move |mut menu, window, cx| {
                                    menu = menu.label("Session sources");
                                    let add_view = view.clone();
                                    menu = menu.item(PopupMenuItem::new("Add source…").on_click(
                                        move |_, _, cx| {
                                            add_view.update(cx, |this, cx| {
                                                this.begin_source_dialog(
                                                    SessionSourceDialogPurpose::AddToSession,
                                                    cx,
                                                );
                                            });
                                        },
                                    ));
                                    if directories.is_empty() && repositories.is_empty() {
                                        menu = menu.separator().label("No sources attached");
                                    } else {
                                        menu = menu.separator();
                                    }
                                    for directory in &directories {
                                        let detach_view = view.clone();
                                        let path = directory.path.clone();
                                        let root_repository = repositories.iter().find(|repository| repository.path == directory.path);
                                        let name = Path::new(&directory.source)
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| directory.source.clone());
                                        let label = if root_repository.is_some() {
                                            format!("Git repository: {name}")
                                        } else {
                                            format!("Folder: {name}")
                                        };
                                        let source_menu = PopupMenu::build(window, cx, |mut menu, _, _| {
                                            menu = menu.label(directory.source.clone());
                                            if let Some(repository) = root_repository {
                                                let repository_id = repository.id;
                                                let select_view = view.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new("Select for review")
                                                        .checked(selected_repository_id == Some(repository_id))
                                                        .on_click(move |_, _, cx| {
                                                            select_view.update(cx, |this, cx| {
                                                                this.select_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                );
                                            }
                                            menu.item(PopupMenuItem::new("Detach source")
                                                .on_click(move |_, _, cx| {
                                                    detach_view.update(cx, |this, cx| {
                                                        this.detach_session_directory(path.clone(), cx);
                                                    });
                                                }))
                                        });
                                        menu = menu.item(PopupMenuItem::submenu(
                                            label, source_menu,
                                        ));
                                    }
                                    let other_repositories = repositories.iter().filter(|repository| {
                                        !directories.iter().any(|directory| directory.path == repository.path)
                                    }).collect::<Vec<_>>();
                                    if !other_repositories.is_empty() {
                                        menu = menu.separator().label("Git repositories");
                                        for repository in other_repositories {
                                            let repository_id = repository.id;
                                            let select_view = view.clone();
                                            let detach_view = view.clone();
                                            let source_menu = PopupMenu::build(window, cx, |menu, _, _| {
                                                menu.label(repository.source.clone()).item(
                                                    PopupMenuItem::new("Select for review")
                                                        .checked(selected_repository_id == Some(repository_id))
                                                        .on_click(move |_, _, cx| {
                                                            select_view.update(cx, |this, cx| {
                                                                this.select_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                )
                                                .item(
                                                    PopupMenuItem::new("Detach source")
                                                        .on_click(move |_, _, cx| {
                                                            detach_view.update(cx, |this, cx| {
                                                                this.detach_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                )
                                            });
                                            let name = Path::new(&repository.source)
                                                .file_name()
                                                .map(|name| name.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| repository.source.clone());
                                            menu = menu.item(PopupMenuItem::submenu(name, source_menu));
                                        }
                                    }
                                    menu
                                }
                            }),
                            ))
                            .child(header_tooltip("toggle-review-sidebar-tooltip", "Toggle side panel",
                        Button::new("toggle-review-sidebar")
                            .icon(Icon::new(if self.review.open {
                                IconName::PanelRightClose
                            } else {
                                IconName::PanelRightOpen
                            }))
                            .ghost()
                            .small()
                            .accessibility_label("Toggle side panel")
                            .when(self.review.open, |button| button.secondary())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_review_pane(cx);
                            })),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .id("timeline-scroll")
                    .overflow_hidden()
                    .child(self.timeline_entity(cx)),
            )
            .child(self.render_composer(layout, window, cx))
            .when(self.sessions.is_empty(), |element| {
                element.child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .right(px(0.))
                        .bottom(px(0.))
                        .left(px(0.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_4()
                        .bg(rgb(0x111318))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(AssetIconName::Sparkles)
                                        .size_8()
                                        .text_color(rgb(0x93c5fd)),
                                )
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(1.25))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Work with agents, keep the trail"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(0x8f98a6))
                                        .child("Start a session over a repository or folder."),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_xs()
                                .text_color(rgb(0x64748b))
                                .child("/ commands")
                                .child("@ files")
                                .child("⌘K command palette"),
                        )
                        .child(
                            Button::new("start-first-session")
                                .label("Create a project")
                                .primary()
                                .on_click(cx.listener(Self::new_session)),
                        ),
                )
            })
            .when(self.rename_dialog.is_some(), |element| {
                element.child(self.render_rename_dialog(cx))
            })
            .when(self.settings_open, |element| {
                element.child(self.render_settings_dialog(cx))
            })
            .when(self.about_open, |element| {
                element.child(self.render_about_dialog(cx))
            })
            .when(
                self.providers_open && self.github_login.is_none(),
                |element| element.child(self.render_providers_dialog(cx)),
            )
            .when(self.github_login.is_some(), |element| {
                element.child(self.render_github_login_dialog(cx))
            }),
            ),
                resizable_panel()
                    .size(layout.review_width)
                    .size_range(px(340.)..px(900.))
                    .flex_none()
                    .visible(review_panel_visible)
                    .child(div().size_full().when(review_panel_visible, |element| {
                        element.child(self.render_review(window, cx))
                    })),
            ]);
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .text_size(gpui_kit::rems(0.8125))
            // Initializes the per-frame selection registry before selectable
            // text participants prepaint and register themselves.
            .child(TextSelectionLayer)
            .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, _, cx| {
                let modifiers = event.keystroke.modifiers;
                if (modifiers.platform || modifiers.control) && event.keystroke.key == "k" {
                    this.toggle_command_palette(cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" && this.command_palette_open {
                    this.close_command_palette(cx);
                    cx.stop_propagation();
                }
            }))
            .when(self.source_dialog.is_some(), |element| {
                element.child(self.render_source_dialog(cx))
            })
            .child(
                div()
                    .h(px(30.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(0x1b1d24))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .when(cfg!(target_os = "macos"), |element| element.pl(px(72.)))
                    .child(
                        div()
                            .id("window-titlebar-drag")
                            .flex()
                            .flex_1()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .window_control_area(WindowControlArea::Drag)
                            .on_mouse_down(MouseButton::Left, |event, window, _| {
                                if event.click_count == 2 {
                                    window.zoom_window();
                                } else {
                                    window.start_window_move();
                                }
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Loom"))
                                    .child(
                                        div().text_xs().text_color(rgb(0x64748b)).child(
                                            if self.demo_workspace { "Demo" } else { "Local" },
                                        ),
                                    ),
                            ),
                    )
                    .when(!cfg!(target_family = "wasm"), |element| {
                        element.child(
                            div().flex().items_center().gap_1().ml_2().child(
                                div()
                                    .id("window-close")
                                    .w(px(22.))
                                    .h(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_sm()
                                    .text_sm()
                                    .text_color(rgb(0xb7c0d0))
                                    .hover(|style| {
                                        style.bg(rgb(0x7f1d1d)).text_color(rgb(0xffffff))
                                    })
                                    .cursor_pointer()
                                    .tooltip(|_, cx| {
                                        cx.new(|_| LoomTooltip {
                                            text: "Close window".into(),
                                        })
                                        .into()
                                    })
                                    .window_control_area(WindowControlArea::Close)
                                    .child(Icon::new(IconName::Close).size_4())
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    })
                                    .on_click(|_, window, cx| {
                                        cx.stop_propagation();
                                        window.remove_window();
                                    }),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .relative()
                    .overflow_hidden()
                    .child(panel_layout)
                    .when(layout.phone && self.session_drawer_open, |row| {
                        row.child(
                            div()
                                .id("mobile-session-backdrop")
                                .size_full()
                                .absolute()
                                .top(px(0.))
                                .left(px(0.))
                                .bg(gpui_kit::hsla(0., 0., 0., 0.55))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_drawer_open = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("mobile-session-drawer")
                                .test_support()
                                .absolute()
                                .top(px(0.))
                                .bottom(px(0.))
                                .left(px(0.))
                                .shadow_lg()
                                .child(self.render_session_sidebar(&view, layout, cx)),
                        )
                    })
                    .when(
                        layout.phone
                            && self.review.open
                            && !self.sessions.is_empty()
                            && !self.settings_open
                            && !self.about_open
                            && !self.providers_open
                            && self.github_login.is_none(),
                        |element| element.child(self.render_review(window, cx)),
                    ),
            )
            .when(self.command_palette_open, |element| {
                element.child(self.render_command_palette(cx))
            });
        content.into_any()
    }
}
