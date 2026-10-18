use crate::collaboration::addressing_for;

use super::*;

impl LoomView {
    pub(crate) fn render_composer(
        &mut self,
        layout: ResponsiveLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.awaits_sign_in() {
            return self.render_sign_in_prompt(cx).into_any_element();
        }
        let permissions = self.session_permissions();
        let composer = self
            .composer_input
            .as_ref()
            .expect("composer input initialized before rendering");
        let value = composer.read(cx).value().to_string();
        // A leading `@` mention decides the recipient, so the draft itself is
        // the control and no mode can be left set from an earlier message.
        let addressing = addressing_for(&value, &self.collaboration.roster);
        let placeholder = if !permissions.can_act {
            "Observers can read this session but not send messages"
        } else if addressing.to_people {
            "Message the people in this session (not sent to the agent)..."
        } else if self.pending_input.is_some() {
            "Answer the agent question..."
        } else if self.sending_message {
            "Sending direction..."
        } else if self.active_run_id.is_some() {
            "Send follow-up direction, or @name to message people..."
        } else {
            "Describe a task, @name to message people, or type / for commands"
        };
        if self.composer_placeholder.as_deref() != Some(placeholder) {
            composer.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
            self.composer_placeholder = Some(placeholder.to_owned());
            // Store the last presented placeholder so updating the component
            // does not keep invalidating the view on every render.
            // This is presentation state only; the text stays in InputState.
        }
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
            Some(completion) if completion.kind == CompletionKind::Person => self
                .people_completion_candidates(&completion.query)
                .into_iter()
                .map(|(name, detail)| (format!("@{name}"), name, detail))
                .collect::<Vec<_>>(),
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
            .when_some(self.collaboration.conflict.clone(), |element, conflict| {
                element.child(
                    div()
                        .id("run-conflict-notice")
                        .test_support()
                        .w_full()
                        .mb_2()
                        .px_3()
                        .py_2()
                        .rounded_sm()
                        .bg(rgb(0x241f3b))
                        .text_color(rgb(0xe9d5ff))
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0xfbbf24))
                                        .child(conflict.heading()),
                                )
                                .child(div().text_sm().child(conflict.message())),
                        )
                        .child(
                            Button::new("dismiss-run-conflict")
                                .label("Dismiss")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.collaboration.conflict = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(self.render_run_status())
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
                        div()
                            .id("composer-field")
                            .test_support()
                            .p_3()
                            // The wrapper, not only the text element, is the
                            // field's tap target: a tap on the padding must
                            // start editing, and the keyboard request has to
                            // happen inside that tap's user gesture because
                            // the web platform raises the virtual keyboard
                            // only from its own `pointerup` heuristic, which
                            // requires the tap to be inside the text element.
                            .cursor_text()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| this.focus_composer(window, cx)),
                            )
                            .child(
                                Textarea::new(composer)
                                    .aria_label(placeholder)
                                    .disabled(!permissions.can_act)
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
                                    .when(permissions.can_act, |element| {
                                        element.when_some(addressing.hint(), |element, hint| {
                                            element.child(self.render_recipient_hint(hint))
                                        })
                                    })
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
                                    .when(permissions.can_act, |element| {
                                        element
                                            .children(self.render_run_control_buttons(layout, cx))
                                    })
                                    .when(
                                        self.run_can_interrupt() && permissions.can_act,
                                        |element| {
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
                                        },
                                    )
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
                                            .tooltip(if addressing.to_people {
                                                "Send to people (Enter)"
                                            } else {
                                                "Send to agent (Enter)"
                                            })
                                            .disabled(!permissions.can_act)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_composer(cx);
                                            })),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The sign-in prompt that stands in for the composer on a pre-login
    /// connection, which may read, sign in, and do nothing else.
    fn render_sign_in_prompt(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let node_id = self.active_node_id().to_owned();
        let mut body = div().flex().flex_col().gap_2().child(
            div()
                .text_sm()
                .text_color(rgb(0xe5e7eb))
                .child("Sign in to this worker to send messages and start runs."),
        );
        if self.offers_github_sign_in() {
            body = body.child(
                Button::new("composer-sign-in-github")
                    .label("Sign in with GitHub")
                    .primary()
                    .small()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_identity_sign_in(node_id.clone(), cx);
                    })),
            );
        } else {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child("This worker does not offer GitHub sign-in."),
            );
        }
        div()
            .id("composer-sign-in-prompt")
            .test_support()
            .w_full()
            .px_3()
            .pt_3()
            .pb(px(12.))
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .when_some(self.status_banner.clone(), |element, banner| {
                element.child(self.render_status_banner(&banner, cx))
            })
            .child(body)
            .into_any_element()
    }

    /// Pause, Resume, and Retry for the active run, shown only in the states
    /// the server accepts them.
    fn render_run_control_buttons(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> Vec<Button> {
        let mut buttons = Vec::new();
        let candidates = [
            (
                self.run_can_pause(),
                "pause-run",
                "Pause",
                loom_core::RunControlAction::Pause,
            ),
            (
                self.run_can_resume(),
                "resume-run",
                "Resume",
                loom_core::RunControlAction::Resume,
            ),
            (
                self.run_can_retry(),
                "retry-run",
                "Retry",
                loom_core::RunControlAction::Retry,
            ),
        ];
        for (visible, id, label, action) in candidates {
            if !visible {
                continue;
            }
            buttons.push(
                Button::new(id)
                    .label(label)
                    .outline()
                    .when(layout.phone, |button| {
                        button.small().h(layout.control_size())
                    })
                    .when(!layout.phone, |button| button.small())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.control_run(action.clone(), cx);
                    })),
            );
        }
        buttons
    }

    /// The recipient the draft's leading mention selects, shown as a passive
    /// hint so peer chat is never a hidden mode: the text decides.
    ///
    /// Only a people route reaches this, because a draft to the agent is the
    /// default and announces nothing, so the hint always reads as the people
    /// colour.
    fn render_recipient_hint(&self, hint: String) -> impl IntoElement {
        div()
            .id("composer-recipient-hint")
            .test_support()
            .text_xs()
            .text_color(rgb(0xc4b5fd))
            .child(hint)
    }

    /// A dismissible banner for the latest operational status or backend error.
    /// Kept out of the conversation timeline so it does not read as a message.
    ///
    /// A connectivity failure reads as plain language here, with its raw code
    /// and message one disclosure away and a real retry control beside it. An
    /// operation error keeps the operation and code in its heading, because
    /// there the worker did answer.
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
        let details = note.details.clone();
        // The label follows the reconnecting state: a spent schedule asks for a
        // retry rather than offering one that is already running.
        let retry_label = self
            .reconnecting
            .as_ref()
            .map_or("Retry", Reconnecting::retry_label);
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
                    .when_some(note.progress.clone(), |element, progress| {
                        element.child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(rgb(accent))
                                .child(progress),
                        )
                    })
                    .when_some(details.clone(), |element, raw| {
                        element.child(
                            div()
                                .mt_1()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new("status-banner-details")
                                        .label(if self.status_details_open {
                                            "Hide details"
                                        } else {
                                            "Details"
                                        })
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.toggle_status_details(cx)
                                        })),
                                )
                                .child(
                                    Button::new("status-banner-copy-details")
                                        .icon(Icon::new(AssetIconName::Copy))
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Copy error details")
                                        .accessibility_label("Copy error details")
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                raw.clone(),
                                            ));
                                        }),
                                )
                                .when(self.status_details_open, |row| {
                                    row.child(
                                        div()
                                            .text_xs()
                                            .font_family(mono_font())
                                            .child(details.unwrap_or_default()),
                                    )
                                }),
                        )
                    })
                    .when(note.retryable, |element| {
                        element.child(
                            div().mt_1().child(
                                Button::new("status-banner-retry")
                                    .label(retry_label)
                                    .small()
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.retry_status_action(cx)),
                                    ),
                            ),
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
