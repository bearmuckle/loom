//! Window chrome, colour, and decoration helpers for the native shell.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Pixels, Point, ResizeEdge, Styled, Tiling, px};
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

pub(crate) fn state_color(state: AgentSessionState) -> gpui::Rgba {
    match state {
        AgentSessionState::Completed => rgb(0x9ad7bd),
        AgentSessionState::Failed | AgentSessionState::Cancelled => rgb(0xfca5a5),
        AgentSessionState::AwaitingApproval | AgentSessionState::NeedsInput => rgb(0xfef3c7),
        AgentSessionState::Archived => rgb(0x64748b),
        _ => rgb(0x93c5fd),
    }
}

pub(crate) static DARK_THEME_ACTIVE: AtomicBool = AtomicBool::new(true);

pub(crate) fn rgb(value: u32) -> gpui::Rgba {
    let value = if DARK_THEME_ACTIVE.load(Ordering::Relaxed) {
        value
    } else {
        match value {
            0x111318 => 0xf8fafc,
            0x14161a | 0x17191f => 0xf1f5f9,
            0x1b1d24 | 0x20242c => 0xe2e8f0,
            0x202b3b => 0xe5efff,
            0x242833 => 0xcbd5e1,
            0x293244 => 0xbfdbfe,
            0x293b56 => 0xcfe1ff,
            0x3b5d85 => 0x93c5fd,
            0x25334a => 0xdbeafe,
            0x30343f | 0x3b4555 => 0xcbd5e1,
            0x10141b => 0xffffff,
            0x0f1115 => 0xffffff,
            0xe5e7eb | 0xf3f4f6 => 0x0f172a,
            0xb7c0d0 | 0x8f98a6 | 0x94a3b8 => 0x475569,
            0x93c5fd | 0xbfdbfe => 0x1d4ed8,
            0x1d4ed8 => 0x1e40af,
            0x1e293b | 0x1f4f78 => 0xdbeafe,
            0x3a1f24 => 0xfee2e2,
            0x3b2f66 => 0xf3e8ff,
            0x493b1a => 0xfef3c7,
            0x60a5fa => 0x2563eb,
            0x7f1d1d => 0xfecaca,
            0x9ad7bd => 0x047857,
            0xfca5a5 => 0xb91c1c,
            0xfda4af => 0x9f1239,
            0xfef3c7 => 0x92400e,
            0xcbd5e1 | 0xdbeafe => 0x1e3a8a,
            0xd1fae5 => 0x065f46,
            0xe9d5ff => 0x6b21a8,
            _ => value,
        }
    };
    gpui::rgb(value)
}

pub(crate) fn change_color(kind: loom_workspace::WorkspaceChangeKind) -> gpui::Rgba {
    match kind {
        loom_workspace::WorkspaceChangeKind::Created => rgb(0x9ad7bd),
        loom_workspace::WorkspaceChangeKind::Deleted => rgb(0xfca5a5),
        loom_workspace::WorkspaceChangeKind::Modified => rgb(0xfef3c7),
    }
}

pub(crate) fn resize_edge(
    position: Point<Pixels>,
    inset: Pixels,
    size: gpui::Size<Pixels>,
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
