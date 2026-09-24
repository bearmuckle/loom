//! Theme integration and window decoration helpers.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::{App, Pixels, Point, ResizeEdge, Rgba, Styled, Tiling, WindowAppearance, px};
use loom_core::AgentSessionState;

pub(crate) const CLIENT_DECORATION_ROUNDING: Pixels = px(10.);
pub(crate) const CLIENT_DECORATION_SHADOW: Pixels = px(10.);
pub(crate) const ERROR_CARD_SURFACE: u32 = 0x171c25;
pub(crate) const ERROR_CARD_FOREGROUND: u32 = 0xe5e7eb;
pub(crate) const ERROR_CARD_ACCENT: u32 = 0xfca5a5;
const SUCCESS_SURFACE: u32 = 0x263d36;
const DANGER_SURFACE: u32 = 0x452b36;
const WARNING_SURFACE: u32 = 0x433a2d;
const INFO_SURFACE: u32 = 0x392f4b;

/// GPUI content masks are axis-aligned rectangles, so a rounded parent cannot clip a
/// square child. Every element that paints a background into a window corner therefore
/// has to carry the corner radius itself.
pub(crate) trait ClientCorners: Styled + Sized {
    fn rounded_client_top(mut self, decorated: bool, tiling: Tiling) -> Self {
        if decorated && !tiling.top && !tiling.left {
            self = self.rounded_tl(CLIENT_DECORATION_ROUNDING);
        }
        if decorated && !tiling.top && !tiling.right {
            self = self.rounded_tr(CLIENT_DECORATION_ROUNDING);
        }
        self
    }

    fn rounded_client_bottom(mut self, decorated: bool, tiling: Tiling) -> Self {
        if decorated && !tiling.bottom && !tiling.left {
            self = self.rounded_bl(CLIENT_DECORATION_ROUNDING);
        }
        if decorated && !tiling.bottom && !tiling.right {
            self = self.rounded_br(CLIENT_DECORATION_ROUNDING);
        }
        self
    }

    fn rounded_client_corners(self, decorated: bool, tiling: Tiling) -> Self {
        self.rounded_client_top(decorated, tiling)
            .rounded_client_bottom(decorated, tiling)
    }
}

impl<T: Styled + Sized> ClientCorners for T {}

#[derive(Clone, Copy)]
struct ThemePalette {
    background: Rgba,
    surface: Rgba,
    control: Rgba,
    control_hover: Rgba,
    active: Rgba,
    border: Rgba,
    border_strong: Rgba,
    foreground: Rgba,
    muted_foreground: Rgba,
    accent: Rgba,
    accent_hover: Rgba,
    success: Rgba,
    danger: Rgba,
    warning: Rgba,
    info: Rgba,
    success_surface: Rgba,
    danger_surface: Rgba,
    warning_surface: Rgba,
    info_surface: Rgba,
    selection: Rgba,
}

impl ThemePalette {
    fn from_theme(theme: &Theme) -> Self {
        let colors = &theme.colors;
        Self {
            background: colors.background.into(),
            surface: colors.sidebar.into(),
            control: colors.secondary.into(),
            control_hover: colors.secondary_hover.into(),
            active: colors.list_active.into(),
            border: colors.border.into(),
            border_strong: colors.input.into(),
            foreground: colors.foreground.into(),
            muted_foreground: colors.muted_foreground.into(),
            accent: colors.primary.into(),
            accent_hover: colors.primary_hover.into(),
            success: colors.success.into(),
            danger: colors.danger.into(),
            warning: colors.warning.into(),
            info: colors.info.into(),
            success_surface: if theme.is_dark() {
                gpui_kit::rgb(SUCCESS_SURFACE)
            } else {
                colors.success.into()
            },
            danger_surface: if theme.is_dark() {
                gpui_kit::rgb(DANGER_SURFACE)
            } else {
                colors.danger.into()
            },
            warning_surface: if theme.is_dark() {
                gpui_kit::rgb(WARNING_SURFACE)
            } else {
                colors.warning.into()
            },
            info_surface: if theme.is_dark() {
                gpui_kit::rgb(INFO_SURFACE)
            } else {
                colors.info.into()
            },
            selection: colors.selection.into(),
        }
    }

    fn color(self, value: u32) -> Rgba {
        match value {
            0x111318 | 0x10141b | 0x0f1115 => self.background,
            0x14161a | 0x17191f | 0x171c25 => self.surface,
            0x1b1d24 | 0x20242c => self.control,
            0x202b3b | 0x25334a | 0x293244 => self.active,
            0x293b56 => self.control_hover,
            0x242833 | 0x30343f => self.border,
            0x3b4555 | 0x3b5d85 => self.border_strong,
            0xe5e7eb | 0xf3f4f6 | 0xcbd5e1 | 0xdbeafe | 0xffffff => self.foreground,
            0x64748b | 0x8f98a6 | 0x94a3b8 | 0xb7c0d0 => self.muted_foreground,
            0x93c5fd | 0xbfdbfe | 0x60a5fa | 0x2563eb => self.accent,
            0x1d4ed8 => self.accent_hover,
            0x9ad7bd | 0xd1fae5 | 0xbbf7d0 => self.success,
            0x24543d | 0x064e3b => self.success_surface,
            0x4ade80 => self.success,
            0xfca5a5 | 0xfda4af | 0xfecaca | 0xfecdd3 => self.danger,
            0xef4444 => self.danger,
            0x3a1f24 | 0x542936 | 0x7f1d1d => self.danger_surface,
            0xfef3c7 | 0xfcd34d => self.warning,
            0x493b1a => self.warning_surface,
            0xe9d5ff => self.info,
            0x3b2f66 => self.info_surface,
            _ => gpui_kit::rgb(value),
        }
    }
}

fn mocha_theme() -> Rc<ThemeConfig> {
    let mut colors = ThemeConfigColors::default();
    colors.accent = Some("#89b4fa".into());
    colors.accent_foreground = Some("#1e1e2e".into());
    colors.background = Some("#1e1e2e".into());
    colors.border = Some("#313244".into());
    colors.button = Some("#313244".into());
    colors.button_active = Some("#45475a".into());
    colors.button_foreground = Some("#cdd6f4".into());
    colors.button_hover = Some("#45475a".into());
    colors.button_danger = Some("#f38ba8".into());
    colors.button_danger_active = Some("#eba0ac".into());
    colors.button_danger_foreground = Some("#1e1e2e".into());
    colors.button_danger_hover = Some("#eba0ac".into());
    colors.button_info = Some("#cba6f7".into());
    colors.button_info_active = Some("#b4befe".into());
    colors.button_info_foreground = Some("#1e1e2e".into());
    colors.button_info_hover = Some("#b4befe".into());
    colors.button_primary = Some("#89b4fa".into());
    colors.button_primary_active = Some("#74c7ec".into());
    colors.button_primary_foreground = Some("#1e1e2e".into());
    colors.button_primary_hover = Some("#b4befe".into());
    colors.button_secondary = Some("#313244".into());
    colors.button_secondary_active = Some("#45475a".into());
    colors.button_secondary_foreground = Some("#cdd6f4".into());
    colors.button_secondary_hover = Some("#45475a".into());
    colors.button_success = Some("#a6e3a1".into());
    colors.button_success_active = Some("#94e2d5".into());
    colors.button_success_foreground = Some("#1e1e2e".into());
    colors.button_success_hover = Some("#94e2d5".into());
    colors.button_warning = Some("#f9e2af".into());
    colors.button_warning_active = Some("#f5e0a7".into());
    colors.button_warning_foreground = Some("#1e1e2e".into());
    colors.button_warning_hover = Some("#f5e0a7".into());
    colors.danger = Some("#f38ba8".into());
    colors.danger_active = Some("#eba0ac".into());
    colors.danger_foreground = Some("#1e1e2e".into());
    colors.danger_hover = Some("#eba0ac".into());
    colors.foreground = Some("#cdd6f4".into());
    colors.info = Some("#cba6f7".into());
    colors.info_active = Some("#b4befe".into());
    colors.info_foreground = Some("#1e1e2e".into());
    colors.info_hover = Some("#b4befe".into());
    colors.input = Some("#45475a".into());
    colors.link = Some("#89b4fa".into());
    colors.link_active = Some("#b4befe".into());
    colors.link_hover = Some("#74c7ec".into());
    colors.list = Some("#1e1e2e".into());
    colors.list_active = Some("#313244".into());
    colors.list_active_border = Some("#89b4fa".into());
    colors.list_hover = Some("#242436".into());
    colors.muted = Some("#242436".into());
    colors.muted_foreground = Some("#a6adc8".into());
    colors.popover = Some("#181825".into());
    colors.popover_foreground = Some("#cdd6f4".into());
    colors.primary = Some("#89b4fa".into());
    colors.primary_active = Some("#74c7ec".into());
    colors.primary_foreground = Some("#1e1e2e".into());
    colors.primary_hover = Some("#b4befe".into());
    colors.ring = Some("#89b4fa".into());
    colors.scrollbar = Some("#181825".into());
    colors.scrollbar_thumb = Some("#45475a".into());
    colors.scrollbar_thumb_hover = Some("#585b70".into());
    colors.secondary = Some("#313244".into());
    colors.secondary_active = Some("#45475a".into());
    colors.secondary_foreground = Some("#cdd6f4".into());
    colors.secondary_hover = Some("#45475a".into());
    colors.selection = Some("#45475a".into());
    colors.sidebar = Some("#181825".into());
    colors.sidebar_accent = Some("#313244".into());
    colors.sidebar_accent_foreground = Some("#cdd6f4".into());
    colors.sidebar_border = Some("#313244".into());
    colors.sidebar_foreground = Some("#cdd6f4".into());
    colors.sidebar_primary = Some("#89b4fa".into());
    colors.sidebar_primary_foreground = Some("#1e1e2e".into());
    colors.success = Some("#a6e3a1".into());
    colors.success_active = Some("#94e2d5".into());
    colors.success_foreground = Some("#1e1e2e".into());
    colors.success_hover = Some("#94e2d5".into());
    colors.warning = Some("#f9e2af".into());
    colors.warning_active = Some("#f5e0a7".into());
    colors.warning_foreground = Some("#1e1e2e".into());
    colors.warning_hover = Some("#f5e0a7".into());
    colors.title_bar = Some("#181825".into());
    colors.title_bar_border = Some("#313244".into());
    colors.status_bar = Some("#181825".into());
    colors.status_bar_border = Some("#313244".into());
    colors.window_border = Some("#313244".into());

    Rc::new(ThemeConfig {
        name: "Loom Mocha".into(),
        mode: ThemeMode::Dark,
        colors,
        ..Default::default()
    })
}

thread_local! {
    static ACTIVE_THEME: RefCell<Option<ThemePalette>> = const { RefCell::new(None) };
}

/// Synchronizes the app-local color helper with gpui-component's active theme.
pub(crate) fn sync_palette(cx: &App) {
    let palette = ThemePalette::from_theme(Theme::global(cx));
    ACTIVE_THEME.with(|active| *active.borrow_mut() = Some(palette));
}

/// Resolves a legacy color role through the active component theme.
///
/// The numeric values are retained at call sites to keep the dense view code
/// readable; they are aliases for semantic component-theme roles, not a second
/// light/dark palette.
pub(crate) fn rgb(value: u32) -> Rgba {
    ACTIVE_THEME.with(|active| {
        active
            .borrow()
            .map_or_else(|| gpui_kit::rgb(value), |palette| palette.color(value))
    })
}

pub(crate) fn selection() -> Rgba {
    ACTIVE_THEME.with(|active| {
        active
            .borrow()
            .map_or_else(|| gpui_kit::rgba(0x335b8def), |palette| palette.selection)
    })
}

pub(crate) fn apply_theme(appearance: WindowAppearance, cx: &mut App) {
    gpui_kit::component::Theme::change(appearance, None, cx);
    if Theme::global(cx).is_dark() {
        let theme = mocha_theme();
        Theme::global_mut(cx).apply_config(&theme);
        Theme::change(ThemeMode::Dark, None, cx);
    }
    sync_palette(cx);
}

pub(crate) fn state_color(state: AgentSessionState) -> Rgba {
    match state {
        AgentSessionState::Completed => rgb(0x9ad7bd),
        AgentSessionState::Failed | AgentSessionState::Cancelled => rgb(0xfca5a5),
        AgentSessionState::AwaitingApproval | AgentSessionState::NeedsInput => rgb(0xfef3c7),
        AgentSessionState::Archived => rgb(0x64748b),
        _ => rgb(0x93c5fd),
    }
}

pub(crate) fn change_color(kind: loom_workspace::WorkspaceChangeKind) -> Rgba {
    match kind {
        loom_workspace::WorkspaceChangeKind::Created => rgb(0x9ad7bd),
        loom_workspace::WorkspaceChangeKind::Deleted => rgb(0xfca5a5),
        loom_workspace::WorkspaceChangeKind::Modified => rgb(0xfef3c7),
    }
}

pub(crate) fn resize_edge(
    position: Point<Pixels>,
    inset: Pixels,
    size: gpui_kit::Size<Pixels>,
) -> Option<ResizeEdge> {
    let edge = if position.y < inset && position.x < inset {
        ResizeEdge::TopLeft
    } else if position.y < inset && position.x > size.width - inset {
        ResizeEdge::TopRight
    } else if position.y < inset {
        ResizeEdge::Top
    } else if position.y > size.height - inset && position.x < inset {
        ResizeEdge::BottomLeft
    } else if position.y > size.height - inset && position.x > size.width - inset {
        ResizeEdge::BottomRight
    } else if position.y > size.height - inset {
        ResizeEdge::Bottom
    } else if position.x < inset {
        ResizeEdge::Left
    } else if position.x > size.width - inset {
        ResizeEdge::Right
    } else {
        return None;
    };
    Some(edge)
}

#[cfg(test)]
mod tests {
    use super::{
        DANGER_SURFACE, ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND, ERROR_CARD_SURFACE, INFO_SURFACE,
        SUCCESS_SURFACE, WARNING_SURFACE,
    };

    fn luminance(value: f32) -> f32 {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    fn contrast_ratio(foreground: u32, background: u32) -> f32 {
        let foreground = gpui_kit::rgb(foreground);
        let background = gpui_kit::rgb(background);
        let luminance = |color: gpui_kit::Rgba| {
            0.2126 * luminance(color.r) + 0.7152 * luminance(color.g) + 0.0722 * luminance(color.b)
        };
        let foreground = luminance(foreground);
        let background = luminance(background);
        let (lighter, darker) = if foreground > background {
            (foreground, background)
        } else {
            (background, foreground)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn error_card_text_and_accent_have_readable_dark_surface_contrast() {
        assert!(contrast_ratio(ERROR_CARD_FOREGROUND, ERROR_CARD_SURFACE) >= 4.5);
        assert!(contrast_ratio(ERROR_CARD_ACCENT, ERROR_CARD_SURFACE) >= 4.5);
    }

    #[test]
    fn status_colors_have_readable_contrast_on_tinted_surfaces() {
        assert!(contrast_ratio(0xa6e3a1, SUCCESS_SURFACE) >= 4.5);
        assert!(contrast_ratio(0xf38ba8, DANGER_SURFACE) >= 4.5);
        assert!(contrast_ratio(0xf9e2af, WARNING_SURFACE) >= 4.5);
        assert!(contrast_ratio(0xcba6f7, INFO_SURFACE) >= 4.5);
    }
}
