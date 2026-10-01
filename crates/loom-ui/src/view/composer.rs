use super::*;

impl LoomView {
    pub(crate) fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let text = self
            .composer_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if text.is_empty() {
            return;
        }
        self.clear_composer_on_render = true;
        #[cfg(target_family = "wasm")]
        if self.browser_demo_mode {
            self.send_message(text, cx);
            return;
        }
        if text.starts_with('/') {
            self.run_slash_command(&text, cx);
            return;
        }
        self.send_message(text, cx);
    }

    pub(crate) fn run_slash_command(&mut self, text: &str, cx: &mut Context<Self>) {
        let mut parts = text.split_whitespace();
        let command = parts.next().unwrap_or_default();
        let name = command.trim_start_matches('/');
        let argument = parts.next();
        self.run_command(name, argument, cx);
    }

    /// Runs a command by name, shared by slash commands and the palette.
    pub(crate) fn run_command(
        &mut self,
        name: &str,
        argument: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        match name {
            "" | "help" => self.record_status(
                "Commands: /new, /repo, /review, /stop, /providers, /settings, /about, /help",
            ),
            "new" => self.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx),
            "repo" | "repository" => {
                self.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx)
            }
            "review" => self.toggle_review_pane(cx),
            "stop" => {
                if self.run_can_interrupt() {
                    self.interrupt_active_run(cx);
                } else {
                    self.record_status("No active run to stop.");
                }
            }
            "providers" => self.open_providers_from_menu(cx),
            "settings" => self.open_settings_from_menu(cx),
            "about" => self.open_about_from_menu(cx),
            "model" => match argument {
                Some(model) => {
                    self.record_status(format!("Use the model picker to switch to '{model}'."))
                }
                None => self.record_status(format!("Current model: {}", self.model.as_str())),
            },
            command => self.record_status(format!("Unknown command '{command}'. Try /help.")),
        }
        cx.notify();
    }

    /// Recomputes the composer's inline completion from the current text.
    pub(crate) fn refresh_composer_completion(&mut self, cx: &mut Context<Self>) {
        if self.suppress_completion_once {
            self.suppress_completion_once = false;
            self.composer_completion = None;
            return;
        }
        let value = self
            .composer_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        self.composer_completion = completion_for_value(&value);
        cx.notify();
    }

    /// Applies the highlighted completion to the composer text.
    pub(crate) fn accept_composer_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) = self.composer_completion.take() else {
            return;
        };
        let Some(input) = self.composer_input.clone() else {
            return;
        };
        let value = input.read(cx).value().to_string();
        let replacement = match completion.kind {
            CompletionKind::Command => {
                let matches = commands_matching(&completion.query);
                let Some(command) = matches.get(completion.selected).copied() else {
                    return;
                };
                replace_command_token(&value, command.name)
            }
            CompletionKind::File => {
                let matches = self.file_completion_candidates();
                let filtered = matches
                    .iter()
                    .filter(|path| path.contains(&completion.query))
                    .collect::<Vec<_>>();
                let Some(path) = filtered.get(completion.selected) else {
                    return;
                };
                replace_last_token(&value, &format!("@{path} "))
            }
        };
        self.suppress_completion_once = true;
        input.update(cx, |state, cx| state.set_value(replacement, window, cx));
        input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Paths offered for `@` completion: changed files and attached sources.
    pub(crate) fn file_completion_candidates(&self) -> Vec<String> {
        let mut candidates = self
            .review
            .vcs
            .as_ref()
            .map(|status| {
                status
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        candidates.extend(self.review.changes.iter().map(|change| change.path.clone()));
        candidates.extend(
            self.session_directories
                .iter()
                .map(|directory| directory.path.clone()),
        );
        candidates.sort();
        candidates.dedup();
        candidates
    }

    pub(crate) fn toggle_command_palette(&mut self, cx: &mut Context<Self>) {
        self.command_palette_open = !self.command_palette_open;
        self.command_palette_selection = 0;
        cx.notify();
    }

    pub(crate) fn close_command_palette(&mut self, cx: &mut Context<Self>) {
        self.command_palette_open = false;
        self.command_palette_selection = 0;
        cx.notify();
    }

    /// Runs the highlighted palette command and closes the palette.
    pub(crate) fn confirm_command_palette(&mut self, cx: &mut Context<Self>) {
        let query = self
            .command_palette_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let matches = commands_matching(&query);
        let name = matches
            .get(self.command_palette_selection)
            .or_else(|| matches.first())
            .map(|command| command.name);
        self.command_palette_open = false;
        self.command_palette_selection = 0;
        if let Some(name) = name {
            self.run_command(name, None, cx);
        }
        cx.notify();
    }
}
