use super::*;

impl LoomView {
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
}
