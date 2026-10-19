//! Which covering surface is on screen.
//!
//! The conversation can be covered by at most one surface: the settings dialog,
//! the archived-session management surface, the new-project pane, the GitHub
//! login pane, or the rename dialog. The view states which one in a single
//! value, and the openers, the close paths, and the renderer all read that same
//! value. Opening a surface therefore cannot leave another one painted over it,
//! and no close list has to be kept in step with the order children are added
//! in.

use super::*;

impl LoomView {
    /// The covering surface on screen, if any.
    pub(crate) fn covering_surface(&self) -> Option<CoveringSurface> {
        self.covering_surface
    }

    /// Whether the settings dialog is on screen. About and Providers are that
    /// dialog on their own section.
    pub(crate) fn settings_open(&self) -> bool {
        self.settings_section().is_some()
    }

    /// The settings section on screen, or `None` when the settings dialog is not
    /// the covering surface.
    pub(crate) fn settings_section(&self) -> Option<SettingsSection> {
        match self.covering_surface {
            Some(CoveringSurface::Settings(section)) => Some(section),
            _ => None,
        }
    }

    /// Whether the archived-sessions surface is on screen.
    pub(crate) fn archived_sessions_open(&self) -> bool {
        self.covering_surface == Some(CoveringSurface::ArchivedSessions)
    }

    /// Whether the new-project pane is on screen.
    pub(crate) fn source_dialog_open(&self) -> bool {
        self.covering_surface == Some(CoveringSurface::NewProject)
    }

    /// Whether the GitHub login pane is on screen.
    pub(crate) fn github_login_open(&self) -> bool {
        self.covering_surface == Some(CoveringSurface::GitHubLogin)
    }

    /// Shows `surface` and nothing else.
    ///
    /// The value replaces whatever was shown, so the surface that was on screen
    /// is gone whether or not its own close path ran. What the surface left
    /// behind holds is dropped, so opening it again starts from its own state
    /// rather than from the state it had when it was covered.
    pub(crate) fn show_surface(&mut self, surface: CoveringSurface) {
        if let Some(replaced) = self.covering_surface.replace(surface) {
            self.discard_surface_content(replaced);
        }
        // The phone's session drawer and the review column are secondary
        // chrome: a covering surface replaces them, instead of every opener
        // clearing whichever of the two it happened to remember.
        self.session_drawer_open = false;
        self.review.open = false;
    }

    /// Closes the covering surface, dropping what it holds. Returns whether one
    /// was on screen.
    pub(crate) fn close_covering_surface(&mut self) -> bool {
        let Some(shown) = self.covering_surface.take() else {
            return false;
        };
        self.discard_surface_content(shown);
        true
    }

    /// Switches the settings dialog to `section`. A no-op unless the dialog is
    /// the surface on screen, so a stale section control closes nothing.
    pub(crate) fn select_settings_section(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        if self
            .settings_section()
            .is_some_and(|shown| shown != section)
        {
            self.show_surface(CoveringSurface::Settings(section));
        }
        cx.notify();
    }

    /// Drops what a surface holds once it is no longer on screen.
    ///
    /// A surface whose state outlives being covered keeps it: the settings
    /// dialog's provider list is reloaded by its own opener, and the
    /// archived-session entries are a loaded listing rather than a dialog.
    fn discard_surface_content(&mut self, surface: CoveringSurface) {
        match surface {
            CoveringSurface::Settings(_) | CoveringSurface::ArchivedSessions => {}
            CoveringSurface::NewProject => {
                self.source_dialog = None;
                self.source_path_input = None;
                self.repository_filter_input = None;
                self.pending_source_path = None;
            }
            CoveringSurface::GitHubLogin => self.github_login = None,
            CoveringSurface::Rename => {
                self.rename_dialog = None;
                self.rename_input_state = None;
            }
            CoveringSurface::SubProject => {
                self.sub_project_dialog = None;
                self.sub_project_input_state = None;
            }
        }
    }
}
