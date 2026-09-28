//! Theme integration and window decoration helpers.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::{App, Rgba, WindowAppearance};

pub(crate) const ERROR_CARD_SURFACE: u32 = 0x171c25;
pub(crate) const ERROR_CARD_FOREGROUND: u32 = 0xe5e7eb;
pub(crate) const ERROR_CARD_ACCENT: u32 = 0xfca5a5;
const SUCCESS_SURFACE: u32 = 0x263d36;
const DANGER_SURFACE: u32 = 0x452b36;
const WARNING_SURFACE: u32 = 0x433a2d;
const INFO_SURFACE: u32 = 0x392f4b;
const SUCCESS_SURFACE_LATTE: u32 = 0xdcebd8;
const DANGER_SURFACE_LATTE: u32 = 0xf5dce1;
const WARNING_SURFACE_LATTE: u32 = 0xf6e8ce;
const INFO_SURFACE_LATTE: u32 = 0xe9defa;

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
    accent_foreground: Rgba,
    accent_hover: Rgba,
    success: Rgba,
    danger: Rgba,
    warning: Rgba,
    info: Rgba,
    success_surface: Rgba,
    danger_surface: Rgba,
    warning_surface: Rgba,
    info_surface: Rgba,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColorRole {
    Background,
    Surface,
    Control,
    ControlHover,
    Active,
    Border,
    BorderStrong,
    Foreground,
    AccentForeground,
    MutedForeground,
    Accent,
    AccentHover,
    Success,
    SuccessSurface,
    Danger,
    DangerSurface,
    Warning,
    WarningSurface,
    Info,
    InfoSurface,
}

/// Maps a legacy dark-palette literal to the semantic role it represents.
///
/// A table rather than an inline match so a test can assert that every literal
/// used by the view resolves to a role instead of falling through to a dark
/// color that would break the light theme.
const fn legacy_color_role(value: u32) -> Option<ColorRole> {
    match value {
        0x111318 | 0x10141b | 0x0f1115 => Some(ColorRole::Background),
        0x14161a | 0x17191f | 0x171c25 => Some(ColorRole::Surface),
        0x191c22 | 0x1b1d24 | 0x20242c => Some(ColorRole::Control),
        0x202b3b | 0x25334a | 0x263b58 | 0x293244 => Some(ColorRole::Active),
        0x293b56 => Some(ColorRole::ControlHover),
        0x242833 | 0x30343f => Some(ColorRole::Border),
        0x3b4555 | 0x3b5d85 => Some(ColorRole::BorderStrong),
        0xe5e7eb | 0xf3f4f6 | 0xcbd5e1 | 0xdbeafe => Some(ColorRole::Foreground),
        0xffffff => Some(ColorRole::AccentForeground),
        0x64748b | 0x8f98a6 | 0x94a3b8 | 0xb7c0d0 => Some(ColorRole::MutedForeground),
        0x93c5fd | 0xbfdbfe | 0x60a5fa | 0x2563eb => Some(ColorRole::Accent),
        0x1d4ed8 => Some(ColorRole::AccentHover),
        0x86efac | 0x9ad7bd | 0xd1fae5 | 0xbbf7d0 | 0x4ade80 => Some(ColorRole::Success),
        0x064e3b | 0x24543d => Some(ColorRole::SuccessSurface),
        0xef4444 | 0xfca5a5 | 0xfda4af | 0xfecaca | 0xfecdd3 => Some(ColorRole::Danger),
        0x3a1f24 | 0x542936 | 0x7f1d1d => Some(ColorRole::DangerSurface),
        0xfbbf24 | 0xfcd34d | 0xfef3c7 => Some(ColorRole::Warning),
        0x493b1a => Some(ColorRole::WarningSurface),
        0xc4b5fd | 0xcba6f7 | 0xe9d5ff => Some(ColorRole::Info),
        0x241f3b | 0x3b2f66 => Some(ColorRole::InfoSurface),
        _ => None,
    }
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
            accent_foreground: colors.primary_foreground.into(),
            accent_hover: colors.primary_hover.into(),
            success: colors.success.into(),
            danger: colors.danger.into(),
            warning: colors.warning.into(),
            info: colors.info.into(),
            success_surface: if theme.is_dark() {
                gpui_kit::rgb(SUCCESS_SURFACE)
            } else {
                gpui_kit::rgb(SUCCESS_SURFACE_LATTE)
            },
            danger_surface: if theme.is_dark() {
                gpui_kit::rgb(DANGER_SURFACE)
            } else {
                gpui_kit::rgb(DANGER_SURFACE_LATTE)
            },
            warning_surface: if theme.is_dark() {
                gpui_kit::rgb(WARNING_SURFACE)
            } else {
                gpui_kit::rgb(WARNING_SURFACE_LATTE)
            },
            info_surface: if theme.is_dark() {
                gpui_kit::rgb(INFO_SURFACE)
            } else {
                gpui_kit::rgb(INFO_SURFACE_LATTE)
            },
        }
    }

    fn role(self, role: ColorRole) -> Rgba {
        match role {
            ColorRole::Background => self.background,
            ColorRole::Surface => self.surface,
            ColorRole::Control => self.control,
            ColorRole::ControlHover => self.control_hover,
            ColorRole::Active => self.active,
            ColorRole::Border => self.border,
            ColorRole::BorderStrong => self.border_strong,
            ColorRole::Foreground => self.foreground,
            ColorRole::AccentForeground => self.accent_foreground,
            ColorRole::MutedForeground => self.muted_foreground,
            ColorRole::Accent => self.accent,
            ColorRole::AccentHover => self.accent_hover,
            ColorRole::Success => self.success,
            ColorRole::SuccessSurface => self.success_surface,
            ColorRole::Danger => self.danger,
            ColorRole::DangerSurface => self.danger_surface,
            ColorRole::Warning => self.warning,
            ColorRole::WarningSurface => self.warning_surface,
            ColorRole::Info => self.info,
            ColorRole::InfoSurface => self.info_surface,
        }
    }

    fn color(self, value: u32) -> Rgba {
        match legacy_color_role(value) {
            Some(role) => self.role(role),
            None => gpui_kit::rgb(value),
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

fn latte_theme() -> Rc<ThemeConfig> {
    let mut colors = ThemeConfigColors::default();
    colors.accent = Some("#1e66f5".into());
    colors.accent_foreground = Some("#ffffff".into());
    colors.background = Some("#eff1f5".into());
    colors.border = Some("#ccd0da".into());
    colors.button = Some("#e6e9ef".into());
    colors.button_active = Some("#bcc0cc".into());
    colors.button_foreground = Some("#4c4f69".into());
    colors.button_hover = Some("#ccd0da".into());
    colors.button_danger = Some("#f5dce1".into());
    colors.button_danger_active = Some("#edc8d0".into());
    colors.button_danger_foreground = Some("#4c4f69".into());
    colors.button_danger_hover = Some("#edc8d0".into());
    colors.button_info = Some("#e9defa".into());
    colors.button_info_active = Some("#dfcff7".into());
    colors.button_info_foreground = Some("#4c4f69".into());
    colors.button_info_hover = Some("#dfcff7".into());
    colors.button_primary = Some("#1e66f5".into());
    colors.button_primary_active = Some("#1e5fcc".into());
    colors.button_primary_foreground = Some("#ffffff".into());
    colors.button_primary_hover = Some("#1e5fcc".into());
    colors.button_secondary = Some("#e6e9ef".into());
    colors.button_secondary_active = Some("#bcc0cc".into());
    colors.button_secondary_foreground = Some("#4c4f69".into());
    colors.button_secondary_hover = Some("#ccd0da".into());
    colors.button_success = Some("#dcebd8".into());
    colors.button_success_active = Some("#c8dfc3".into());
    colors.button_success_foreground = Some("#4c4f69".into());
    colors.button_success_hover = Some("#c8dfc3".into());
    colors.button_warning = Some("#f6e8ce".into());
    colors.button_warning_active = Some("#efddb8".into());
    colors.button_warning_foreground = Some("#4c4f69".into());
    colors.button_warning_hover = Some("#efddb8".into());
    colors.danger = Some("#b01234".into());
    colors.danger_active = Some("#a70f30".into());
    colors.danger_foreground = Some("#ffffff".into());
    colors.danger_hover = Some("#b01234".into());
    colors.foreground = Some("#4c4f69".into());
    colors.info = Some("#7733d1".into());
    colors.info_active = Some("#6d2fc1".into());
    colors.info_foreground = Some("#ffffff".into());
    colors.info_hover = Some("#7733d1".into());
    colors.input = Some("#bcc0cc".into());
    colors.link = Some("#1e66f5".into());
    colors.link_active = Some("#1e5fcc".into());
    colors.link_hover = Some("#7287fd".into());
    colors.list = Some("#eff1f5".into());
    colors.list_active = Some("#dce0e8".into());
    colors.list_active_border = Some("#1e66f5".into());
    colors.list_hover = Some("#e6e9ef".into());
    colors.muted = Some("#e6e9ef".into());
    colors.muted_foreground = Some("#6c6f85".into());
    colors.popover = Some("#eff1f5".into());
    colors.popover_foreground = Some("#4c4f69".into());
    colors.primary = Some("#1e66f5".into());
    colors.primary_active = Some("#1e5fcc".into());
    colors.primary_foreground = Some("#ffffff".into());
    colors.primary_hover = Some("#1e5fcc".into());
    colors.ring = Some("#1e66f5".into());
    colors.scrollbar = Some("#e6e9ef".into());
    colors.scrollbar_thumb = Some("#bcc0cc".into());
    colors.scrollbar_thumb_hover = Some("#acb0be".into());
    colors.secondary = Some("#e6e9ef".into());
    colors.secondary_active = Some("#bcc0cc".into());
    colors.secondary_foreground = Some("#4c4f69".into());
    colors.secondary_hover = Some("#ccd0da".into());
    colors.selection = Some("#bcc0cc".into());
    colors.sidebar = Some("#e6e9ef".into());
    colors.sidebar_accent = Some("#dce0e8".into());
    colors.sidebar_accent_foreground = Some("#4c4f69".into());
    colors.sidebar_border = Some("#ccd0da".into());
    colors.sidebar_foreground = Some("#4c4f69".into());
    colors.sidebar_primary = Some("#1e66f5".into());
    colors.sidebar_primary_foreground = Some("#ffffff".into());
    colors.success = Some("#2c7025".into());
    colors.success_active = Some("#24621e".into());
    colors.success_foreground = Some("#ffffff".into());
    colors.success_hover = Some("#327a2b".into());
    colors.warning = Some("#8a5a00".into());
    colors.warning_active = Some("#784e00".into());
    colors.warning_foreground = Some("#ffffff".into());
    colors.warning_hover = Some("#9a6700".into());
    colors.title_bar = Some("#e6e9ef".into());
    colors.title_bar_border = Some("#ccd0da".into());
    colors.status_bar = Some("#e6e9ef".into());
    colors.status_bar_border = Some("#ccd0da".into());
    colors.window_border = Some("#ccd0da".into());

    Rc::new(ThemeConfig {
        name: "Loom Latte".into(),
        mode: ThemeMode::Light,
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

pub(crate) fn apply_theme(appearance: WindowAppearance, cx: &mut App) {
    gpui_kit::component::Theme::change(appearance, None, cx);
    let dark = Theme::global(cx).is_dark();
    let theme = if dark { mocha_theme() } else { latte_theme() };
    let mode = if dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::global_mut(cx).apply_config(&theme);
    Theme::change(mode, None, cx);
    sync_palette(cx);
}

pub(crate) fn change_color(kind: loom_workspace::WorkspaceChangeKind) -> Rgba {
    match kind {
        loom_workspace::WorkspaceChangeKind::Created => rgb(0x9ad7bd),
        loom_workspace::WorkspaceChangeKind::Deleted => rgb(0xfca5a5),
        loom_workspace::WorkspaceChangeKind::Modified => rgb(0xfef3c7),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DANGER_SURFACE, DANGER_SURFACE_LATTE, ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND,
        ERROR_CARD_SURFACE, INFO_SURFACE, INFO_SURFACE_LATTE, SUCCESS_SURFACE,
        SUCCESS_SURFACE_LATTE, WARNING_SURFACE, WARNING_SURFACE_LATTE, change_color, latte_theme,
        legacy_color_role, mocha_theme,
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
        assert!(contrast_ratio(0x2c7025, SUCCESS_SURFACE_LATTE) >= 4.5);
        assert!(contrast_ratio(0xb01234, DANGER_SURFACE_LATTE) >= 4.5);
        assert!(contrast_ratio(0x8a5a00, WARNING_SURFACE_LATTE) >= 4.5);
        assert!(contrast_ratio(0x7733d1, INFO_SURFACE_LATTE) >= 4.5);
    }

    #[test]
    fn theme_configs_define_distinct_dark_and_light_palettes() {
        let mocha = mocha_theme();
        let latte = latte_theme();
        assert_eq!(mocha.name, "Loom Mocha");
        assert_eq!(mocha.mode, gpui_kit::component::ThemeMode::Dark);
        assert_eq!(latte.name, "Loom Latte");
        assert_eq!(latte.mode, gpui_kit::component::ThemeMode::Light);
        assert_eq!(mocha.colors.background.as_deref(), Some("#1e1e2e"));
        assert_eq!(latte.colors.background.as_deref(), Some("#eff1f5"));
        assert_eq!(mocha.colors.primary.as_deref(), Some("#89b4fa"));
        assert_eq!(latte.colors.primary.as_deref(), Some("#1e66f5"));
        assert_eq!(mocha.colors.success.as_deref(), Some("#a6e3a1"));
        assert_eq!(latte.colors.success.as_deref(), Some("#2c7025"));
    }

    #[test]
    fn every_legacy_color_literal_used_by_the_view_is_mapped() {
        let source = include_str!("view.rs");
        let mut unmapped = Vec::new();
        let mut rest = source;
        while let Some(index) = rest.find("rgb(0x") {
            let after = &rest[index + "rgb(0x".len()..];
            let digits = after
                .chars()
                .take_while(|character| character.is_ascii_hexdigit())
                .take(6)
                .collect::<String>();
            if let Ok(value) = u32::from_str_radix(&digits, 16)
                && legacy_color_role(value).is_none()
            {
                unmapped.push(value);
            }
            rest = after;
        }
        unmapped.sort_unstable();
        unmapped.dedup();
        assert!(
            unmapped.is_empty(),
            "view.rs uses colors that are not mapped to a theme role: {unmapped:06x?}"
        );
    }

    #[test]
    fn workspace_change_colors_keep_their_semantic_defaults() {
        assert_eq!(
            change_color(loom_workspace::WorkspaceChangeKind::Created),
            gpui_kit::rgb(0x9ad7bd)
        );
        assert_eq!(
            change_color(loom_workspace::WorkspaceChangeKind::Deleted),
            gpui_kit::rgb(0xfca5a5)
        );
        assert_eq!(
            change_color(loom_workspace::WorkspaceChangeKind::Modified),
            gpui_kit::rgb(0xfef3c7)
        );
    }
}
