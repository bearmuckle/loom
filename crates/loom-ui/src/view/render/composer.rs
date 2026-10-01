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
        let height = composer_height(&value, layout.phone);
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
            .px_3()
            .pt_3()
            // Reserve the space hidden by an on-screen keyboard so the field
            // and send action stay visible while typing on mobile.
            .pb(px(12.) + bottom_occlusion(window))
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .when_some(self.status_banner.clone(), |element, banner| {
                element.child(self.render_status_banner(&banner, cx))
            })
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
                        .max_h(if layout.phone { px(180.) } else { px(240.) })
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
                    .test_support()
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
                                    .min_w(px(0.))
                                    // On phone the pickers share the remaining
                                    // width so the send action stays on the same
                                    // row instead of dropping onto a mostly empty
                                    // second row.
                                    .when(layout.phone, |element| element.flex_1())
                                    .child(self.render_agent_mode_picker(layout.phone))
                                    .child(self.render_model_picker(layout.phone, cx))
                                    .child(
                                        Button::new("open-command-palette")
                                            .icon(Icon::new(command_palette_icon()))
                                            .ghost()
                                            .when(layout.phone, |button| {
                                                button
                                                    .large()
                                                    .h(layout.control_size())
                                                    .w(layout.control_size())
                                            })
                                            .when(!layout.phone, |button| button.xsmall())
                                            .tooltip(format!(
                                                "Command palette ({})",
                                                command_palette_shortcut_label()
                                            ))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_command_palette(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap_2()
                                    .when(self.run_can_interrupt(), |element| {
                                        element.child(
                                            Button::new("interrupt-run")
                                                .label("Stop")
                                                .danger()
                                                .when(layout.phone, |button| {
                                                    button.small().h(layout.control_size())
                                                })
                                                .when(!layout.phone, |button| button.small())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.interrupt_active_run(cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("send-message")
                                            .icon(Icon::new(AssetIconName::ArrowUp))
                                            .primary()
                                            .when(layout.phone, |button| {
                                                button
                                                    .large()
                                                    .h(layout.control_size())
                                                    .w(layout.control_size())
                                            })
                                            .when(!layout.phone, |button| button.small())
                                            .tooltip("Send (↵)")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_composer(cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }

    /// A dismissible banner for the latest operational status or backend error.
    /// Kept out of the conversation timeline so it does not read as a message.
    fn render_status_banner(
        &self,
        note: &SystemNote,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let (surface, accent, text_color) = match note.tone {
            SystemTone::Error => (ERROR_CARD_SURFACE, ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND),
            SystemTone::Input => (0x241f3b, 0xc4b5fd, 0xe9d5ff),
            SystemTone::Neutral => (0x191c22, 0x64748b, 0x94a3b8),
        };
        div()
            .id("status-banner")
            .test_support()
            .w_full()
            .mb_2()
            .px_3()
            .py_2()
            .rounded_sm()
            .bg(rgb(surface))
            .text_color(rgb(text_color))
            .flex()
            .items_start()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .when_some(note.heading.clone(), |element, heading| {
                        element.child(div().text_xs().text_color(rgb(accent)).child(heading))
                    })
                    .child(div().text_sm().child(note.text.clone()))
                    .when(note.retryable, |element| {
                        element.child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(rgb(accent))
                                .child("This operation can be retried."),
                        )
                    }),
            )
            .child(
                Button::new("dismiss-status-banner")
                    .icon(Icon::new(IconName::Close))
                    .ghost()
                    .xsmall()
                    .accessibility_label("Dismiss message")
                    .on_click(cx.listener(|view, _, _, cx| view.dismiss_status_banner(cx))),
            )
            .into_any_element()
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
                        element.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x64748b))
                                .child(command_shortcut_label(shortcut)),
                        )
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
            .occlude()
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
                                Icon::new(command_palette_icon())
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
