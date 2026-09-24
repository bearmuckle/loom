//! Theme integration and window decoration helpers.

use std::cell::RefCell;

use gpui_kit::component::Theme;
use gpui_kit::{App, Pixels, Point, ResizeEdge, Rgba, Styled, Tiling, WindowAppearance, px};
use loom_core::AgentSessionState;

pub(crate) const CLIENT_DECORATION_ROUNDING: Pixels = px(10.);
pub(crate) const CLIENT_DECORATION_SHADOW: Pixels = px(10.);

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
            0x24543d | 0x064e3b => self.success,
            0xfca5a5 | 0xfda4af | 0xfecaca | 0xfecdd3 => self.danger,
            0x3a1f24 | 0x542936 | 0x7f1d1d => self.danger,
            0xfef3c7 => self.warning,
            0x493b1a => self.warning,
            0xe9d5ff => self.info,
            0x3b2f66 => self.info,
            _ => gpui_kit::rgb(value),
        }
    }
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
