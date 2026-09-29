use super::*;

impl LoomView {
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
