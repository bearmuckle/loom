use super::*;

impl LoomView {
    pub(crate) fn render_model_picker(
        &self,
        phone: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut picker = div().flex().items_center().min_w(px(0.));
        if phone {
            // Let the model picker take the space left by the other composer
            // actions so the full model name stays readable on a phone, and keep
            // a finger-sized tap target.
            picker = picker.flex_1().h(px(44.));
        }
        if let Some(state) = &self.model_select {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .min_w(px(0.))
                    .when(phone, |element| element.flex_1())
                    .rounded_md()
                    .hover(|style| style.bg(rgb(0x293244)))
                    // Opening the picker is the on-demand trigger for provider
                    // model discovery; the refresh itself is throttled and runs
                    // off the UI thread.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, _, cx| {
                            view.refresh_models_on_demand(cx);
                        }),
                    )
                    .child(
                        Select::new(state)
                            .id("session-model-select")
                            .max_w(if phone { px(180.) } else { px(220.) })
                            .when(phone, |select| select.w_full())
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
        let mut picker = div()
            .flex()
            .items_center()
            .when(phone, |element| element.h(px(44.)));
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
                            .max_w(if phone { px(100.) } else { px(140.) })
                            .small()
                            .appearance(false)
                            .accessibility_label("Agent mode")
                            .placeholder("Select agent mode"),
                    ),
            );
        }
        picker
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn render_disconnected(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let layout = responsive_layout(window.bounds().size.width);
        let screen = disconnected_screen(self.browser_connection_lost.as_deref());
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
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
                                        .child(screen.sidebar),
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
                                        div().flex().flex_col().child(screen.heading).child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0x8f98a6))
                                                .child(screen.detail),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .when(screen.reconnect, |row| {
                                                row.child(
                                                    Button::new("disconnected-reconnect")
                                                        .label("Reconnect")
                                                        .small()
                                                        .on_click(cx.listener(|view, _, _, cx| {
                                                            view.reconnect_browser(cx);
                                                        })),
                                                )
                                            })
                                            .child(
                                                Button::new("disconnected-open-settings")
                                                    .icon(Icon::new(IconName::Settings))
                                                    .ghost()
                                                    .xsmall()
                                                    .on_click(cx.listener(|view, _, _, cx| {
                                                        view.open_settings_from_menu(cx);
                                                    })),
                                            ),
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
                                            .child(screen.empty),
                                    ),
                            )
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(layout, cx))
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
                    .child(screen.footer),
            )
            .when(
                !self.settings_open
                    && !self.welcome_dialog_dismissed
                    && self.browser_connection_lost.is_none(),
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
