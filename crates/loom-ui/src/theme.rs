//! Theme integration and window decoration helpers.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::{App, Rgba, SharedString, WindowAppearance};

pub(crate) const ERROR_CARD_SURFACE: u32 = 0x171c25;
pub(crate) const ERROR_CARD_FOREGROUND: u32 = 0xe5e7eb;
pub(crate) const ERROR_CARD_ACCENT: u32 = 0xfca5a5;

/// The window rem base. GPUI's text, spacing, and sizing utilities are all
/// rem-based, so this single value sets the UI's density. It stays close to
/// Zed's compact feel while leaving room for the user's font-scale setting.
///
/// The effective default is 5% larger than the compact base of 14, so the
/// Appearance control's 100% renders what previously was 105%.
pub(crate) const BASE_FONT_SIZE: f32 = 14.7;

/// The monospace size used for code, diffs, and commands.
pub(crate) const MONO_FONT_SIZE: f32 = 12.6;

/// The body size for conversation text in the timeline.
pub(crate) const CONVERSATION_FONT_SIZE: f32 = 13.65;

/// Alpha-blends `tint` over `base`, used to derive tinted status surfaces from
/// the active palette instead of hardcoding a second set of colors.
fn blend(base: Rgba, tint: Rgba, amount: f32) -> Rgba {
    Rgba {
        r: base.r + (tint.r - base.r) * amount,
        g: base.g + (tint.g - base.g) * amount,
        b: base.b + (tint.b - base.b) * amount,
        a: base.a,
    }
}

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
        0x14161a | 0x17191f | 0x171c25 | 0x181c26 => Some(ColorRole::Surface),
        0x191c22 | 0x1b1d24 | 0x20242c => Some(ColorRole::Control),
        0x202b3b | 0x25334a | 0x263b58 | 0x293244 => Some(ColorRole::Active),
        0x293b56 => Some(ColorRole::ControlHover),
        0x242833 | 0x30343f | 0x232c39 => Some(ColorRole::Border),
        0x3b4555 | 0x3b5d85 => Some(ColorRole::BorderStrong),
        0xe5e7eb | 0xf3f4f6 | 0xcbd5e1 | 0xdbeafe => Some(ColorRole::Foreground),
        0xffffff => Some(ColorRole::AccentForeground),
        0x5c6572 | 0x64748b | 0x7f8b9c | 0x8f98a6 | 0x94a3b8 | 0xb7c0d0 => {
            Some(ColorRole::MutedForeground)
        }
        0x93c5fd | 0xbfdbfe | 0x60a5fa | 0x2563eb => Some(ColorRole::Accent),
        0x1d4ed8 => Some(ColorRole::AccentHover),
        0x34d399 | 0x86efac | 0x9ad7bd | 0xd1fae5 | 0xbbf7d0 | 0x4ade80 => Some(ColorRole::Success),
        0x1f6b4d => Some(ColorRole::Success),
        0x064e3b | 0x24543d | 0x10291f => Some(ColorRole::SuccessSurface),
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
        let background: Rgba = colors.background.into();
        let success: Rgba = colors.success.into();
        let danger: Rgba = colors.danger.into();
        let warning: Rgba = colors.warning.into();
        let info: Rgba = colors.info.into();
        let tint = if theme.is_dark() { 0.18 } else { 0.12 };
        Self {
            background,
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
            success,
            danger,
            warning,
            info,
            success_surface: blend(background, success, tint),
            danger_surface: blend(background, danger, tint),
            warning_surface: blend(background, warning, tint),
            info_surface: blend(background, info, tint),
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

/// A Catppuccin flavor: the 26 named palette colors plus the foreground used
/// on filled accent surfaces.
#[derive(Clone, Copy)]
struct Catppuccin {
    base: u32,
    mantle: u32,
    crust: u32,
    surface0: u32,
    surface1: u32,
    surface2: u32,
    overlay0: u32,
    overlay1: u32,
    overlay2: u32,
    text: u32,
    subtext1: u32,
    subtext0: u32,
    lavender: u32,
    blue: u32,
    sapphire: u32,
    sky: u32,
    teal: u32,
    green: u32,
    yellow: u32,
    peach: u32,
    maroon: u32,
    red: u32,
    mauve: u32,
    pink: u32,
    flamingo: u32,
    rosewater: u32,
    /// The foreground for filled accent buttons, which must contrast with the
    /// saturated accent colors rather than the page background.
    on_accent: u32,
}

/// Catppuccin Mocha (dark), the palette the dark theme is built from.
const MOCHA: Catppuccin = Catppuccin {
    base: 0x1e1e2e,
    mantle: 0x181825,
    crust: 0x11111b,
    surface0: 0x313244,
    surface1: 0x45475a,
    surface2: 0x585b70,
    overlay0: 0x6c7086,
    overlay1: 0x7f849c,
    overlay2: 0x9399b2,
    text: 0xcdd6f4,
    subtext1: 0xbac2de,
    subtext0: 0xa6adc8,
    lavender: 0xb4befe,
    blue: 0x89b4fa,
    sapphire: 0x74c7ec,
    sky: 0x89dceb,
    teal: 0x94e2d5,
    green: 0xa6e3a1,
    yellow: 0xf9e2af,
    peach: 0xfab387,
    maroon: 0xeba0ac,
    red: 0xf38ba8,
    mauve: 0xcba6f7,
    pink: 0xf5c2e7,
    flamingo: 0xf2cdcd,
    rosewater: 0xf5e0dc,
    on_accent: 0x1e1e2e,
};

/// Catppuccin Latte (light), the palette the light theme is built from.
const LATTE: Catppuccin = Catppuccin {
    base: 0xeff1f5,
    mantle: 0xe6e9ef,
    crust: 0xdce0e8,
    surface0: 0xccd0da,
    surface1: 0xbcc0cc,
    surface2: 0xacb0be,
    overlay0: 0x9ca0b0,
    overlay1: 0x8c8fa1,
    overlay2: 0x7c7f93,
    text: 0x4c4f69,
    subtext1: 0x5c5f77,
    subtext0: 0x6c6f85,
    lavender: 0x7287fd,
    blue: 0x1e66f5,
    sapphire: 0x209fb5,
    sky: 0x04a5e5,
    teal: 0x179299,
    green: 0x40a02b,
    yellow: 0xdf8e1d,
    peach: 0xfe640b,
    maroon: 0xe64553,
    red: 0xd20f39,
    mauve: 0x8839ef,
    pink: 0xea76cb,
    flamingo: 0xdd7878,
    rosewater: 0xdc8a78,
    on_accent: 0xeff1f5,
};

fn hex(value: u32) -> SharedString {
    format!("#{value:06x}").into()
}

/// Builds a complete component theme from a Catppuccin flavor so both modes
/// stay in sync and every role resolves to a canonical palette color.
fn catppuccin_theme(name: &str, mode: ThemeMode, p: Catppuccin) -> Rc<ThemeConfig> {
    let mut c = ThemeConfigColors::default();
    // `accent` is gpui-component's neutral highlight fill (ghost-button hover,
    // menu/list selection). The blue action color lives in `primary`/`ring`/
    // `link`, so `accent` stays a surface color instead of painted blue.
    c.accent = Some(hex(p.surface0));
    c.accent_foreground = Some(hex(p.text));
    c.accordion = Some(hex(p.surface0));
    c.background = Some(hex(p.base));
    c.border = Some(hex(p.surface0));
    c.button = Some(hex(p.surface0));
    c.button_active = Some(hex(p.surface1));
    c.button_foreground = Some(hex(p.text));
    c.button_hover = Some(hex(p.surface1));
    c.button_danger = Some(hex(p.red));
    c.button_danger_active = Some(hex(p.maroon));
    c.button_danger_foreground = Some(hex(p.on_accent));
    c.button_danger_hover = Some(hex(p.flamingo));
    c.button_info = Some(hex(p.mauve));
    c.button_info_active = Some(hex(p.lavender));
    c.button_info_foreground = Some(hex(p.on_accent));
    c.button_info_hover = Some(hex(p.pink));
    c.button_primary = Some(hex(p.blue));
    c.button_primary_active = Some(hex(p.sapphire));
    c.button_primary_foreground = Some(hex(p.on_accent));
    c.button_primary_hover = Some(hex(p.lavender));
    c.button_secondary = Some(hex(p.surface0));
    c.button_secondary_active = Some(hex(p.surface1));
    c.button_secondary_foreground = Some(hex(p.text));
    c.button_secondary_hover = Some(hex(p.surface1));
    c.button_success = Some(hex(p.green));
    c.button_success_active = Some(hex(p.teal));
    c.button_success_foreground = Some(hex(p.on_accent));
    c.button_success_hover = Some(hex(p.teal));
    c.button_warning = Some(hex(p.yellow));
    c.button_warning_active = Some(hex(p.peach));
    c.button_warning_foreground = Some(hex(p.on_accent));
    c.button_warning_hover = Some(hex(p.peach));
    c.caret = Some(hex(p.blue));
    c.chart_bullish = Some(hex(p.green));
    c.chart_bearish = Some(hex(p.red));
    c.chart_grid = Some(hex(p.surface1));
    c.danger = Some(hex(p.red));
    c.danger_active = Some(hex(p.maroon));
    c.danger_foreground = Some(hex(p.on_accent));
    c.danger_hover = Some(hex(p.flamingo));
    c.description_list_label = Some(hex(p.subtext1));
    c.description_list_label_foreground = Some(hex(p.text));
    c.drag_border = Some(hex(p.blue));
    c.drop_target = Some(hex(p.surface2));
    c.foreground = Some(hex(p.text));
    c.group_box = Some(hex(p.surface0));
    c.group_box_foreground = Some(hex(p.text));
    c.group_box_title_foreground = Some(hex(p.text));
    c.info = Some(hex(p.mauve));
    c.info_active = Some(hex(p.lavender));
    c.info_foreground = Some(hex(p.on_accent));
    c.info_hover = Some(hex(p.pink));
    // A restrained input/border tone keeps fields and composer outlines subtle
    // instead of drawing a second, stronger border color across the window.
    c.input = Some(hex(p.surface0));
    c.link = Some(hex(p.blue));
    c.link_active = Some(hex(p.sapphire));
    c.link_hover = Some(hex(p.sky));
    c.list = Some(hex(p.base));
    // `list_active` is stronger than `list_hover` so the selected row reads
    // clearly against the sidebar instead of matching a hovered row.
    c.list_active = Some(hex(p.surface1));
    c.list_active_border = Some(hex(p.blue));
    c.list_even = Some(hex(p.mantle));
    c.list_head = Some(hex(p.mantle));
    c.list_hover = Some(hex(p.surface0));
    c.muted = Some(hex(p.surface0));
    c.muted_foreground = Some(hex(p.subtext0));
    c.overlay = Some(hex(p.crust));
    c.popover = Some(hex(p.mantle));
    c.popover_foreground = Some(hex(p.text));
    c.primary = Some(hex(p.blue));
    c.primary_active = Some(hex(p.sapphire));
    c.primary_foreground = Some(hex(p.on_accent));
    c.primary_hover = Some(hex(p.lavender));
    c.progress_bar = Some(hex(p.blue));
    c.ring = Some(hex(p.blue));
    c.scrollbar = Some(hex(p.mantle));
    c.scrollbar_thumb = Some(hex(p.overlay0));
    c.scrollbar_thumb_hover = Some(hex(p.overlay1));
    c.secondary = Some(hex(p.surface0));
    c.secondary_active = Some(hex(p.surface1));
    c.secondary_foreground = Some(hex(p.text));
    c.secondary_hover = Some(hex(p.surface1));
    c.selection = Some(hex(p.surface1));
    c.sidebar = Some(hex(p.mantle));
    c.sidebar_accent = Some(hex(p.surface0));
    c.sidebar_accent_foreground = Some(hex(p.text));
    c.sidebar_border = Some(hex(p.surface0));
    c.sidebar_foreground = Some(hex(p.text));
    c.sidebar_primary = Some(hex(p.blue));
    c.sidebar_primary_foreground = Some(hex(p.on_accent));
    c.skeleton = Some(hex(p.surface0));
    c.slider_bar = Some(hex(p.blue));
    c.slider_thumb = Some(hex(p.rosewater));
    c.status_bar = Some(hex(p.mantle));
    c.status_bar_border = Some(hex(p.surface0));
    c.success = Some(hex(p.green));
    c.success_active = Some(hex(p.teal));
    c.success_foreground = Some(hex(p.on_accent));
    c.success_hover = Some(hex(p.teal));
    c.switch = Some(hex(p.surface1));
    c.switch_thumb = Some(hex(p.rosewater));
    c.tab = Some(hex(p.mantle));
    c.tab_active = Some(hex(p.surface0));
    c.tab_active_foreground = Some(hex(p.text));
    c.tab_bar = Some(hex(p.mantle));
    c.tab_bar_segmented = Some(hex(p.surface0));
    c.tab_foreground = Some(hex(p.subtext1));
    c.table = Some(hex(p.base));
    c.table_active = Some(hex(p.surface0));
    c.table_active_border = Some(hex(p.blue));
    c.table_even = Some(hex(p.mantle));
    c.table_foot = Some(hex(p.mantle));
    c.table_foot_foreground = Some(hex(p.text));
    c.table_head = Some(hex(p.mantle));
    c.table_head_foreground = Some(hex(p.text));
    c.table_hover = Some(hex(p.surface0));
    c.table_row_border = Some(hex(p.surface0));
    c.title_bar = Some(hex(p.mantle));
    c.title_bar_border = Some(hex(p.surface0));
    c.warning = Some(hex(p.yellow));
    c.warning_active = Some(hex(p.peach));
    c.warning_foreground = Some(hex(p.on_accent));
    c.warning_hover = Some(hex(p.peach));
    c.window_border = Some(hex(p.overlay2));
    Rc::new(ThemeConfig {
        name: name.into(),
        mode,
        // Compact, Zed-like density: a smaller base and monospace size, a
        // tight corner radius, and no drop shadows on controls. The view sets
        // the window rem size from `BASE_FONT_SIZE`, so these sizes keep the
        // component library and the hand-rolled chrome in step.
        font_size: Some(BASE_FONT_SIZE),
        mono_font_size: Some(MONO_FONT_SIZE),
        radius: Some(4),
        radius_lg: Some(6),
        shadow: Some(false),
        colors: c,
        ..Default::default()
    })
}

fn mocha_theme() -> Rc<ThemeConfig> {
    catppuccin_theme("Loom Mocha", ThemeMode::Dark, MOCHA)
}

fn latte_theme() -> Rc<ThemeConfig> {
    catppuccin_theme("Loom Latte", ThemeMode::Light, LATTE)
}

thread_local! {
    static ACTIVE_THEME: RefCell<Option<ThemePalette>> = const { RefCell::new(None) };
    static MONO_FONT: RefCell<Option<SharedString>> = const { RefCell::new(None) };
    static MONO_SIZE: std::cell::Cell<f32> = const { std::cell::Cell::new(13.) };
}

/// Synchronizes the app-local color helper with gpui-component's active theme.
pub(crate) fn sync_palette(cx: &App) {
    let theme = Theme::global(cx);
    let palette = ThemePalette::from_theme(theme);
    ACTIVE_THEME.with(|active| *active.borrow_mut() = Some(palette));
    MONO_FONT.with(|font| *font.borrow_mut() = Some(theme.mono_font_family.clone()));
    MONO_SIZE.with(|size| size.set(f32::from(theme.mono_font_size)));
}

/// The active theme's monospace family, used for code, diffs, and commands.
pub(crate) fn mono_font() -> SharedString {
    MONO_FONT.with(|font| {
        font.borrow()
            .clone()
            .unwrap_or_else(|| SharedString::from("monospace"))
    })
}

/// The active theme's monospace size in pixels.
pub(crate) fn mono_size() -> f32 {
    MONO_SIZE.with(std::cell::Cell::get)
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

pub(crate) fn change_color(kind: loom_protocol::WorkspaceChangeKind) -> Rgba {
    match kind {
        loom_protocol::WorkspaceChangeKind::Created => rgb(0x9ad7bd),
        loom_protocol::WorkspaceChangeKind::Deleted => rgb(0xfca5a5),
        loom_protocol::WorkspaceChangeKind::Modified => rgb(0xfef3c7),
    }
}

#[cfg(test)]
mod tests {
    use super::{change_color, latte_theme, legacy_color_role, mocha_theme};

    #[test]
    fn theme_configs_define_canonical_catppuccin_palettes() {
        let mocha = mocha_theme();
        let latte = latte_theme();
        assert_eq!(mocha.name, "Loom Mocha");
        assert_eq!(mocha.mode, gpui_kit::component::ThemeMode::Dark);
        assert_eq!(latte.name, "Loom Latte");
        assert_eq!(latte.mode, gpui_kit::component::ThemeMode::Light);

        // Mocha uses the dark flavor's base, text, and accent colors.
        assert_eq!(mocha.colors.background.as_deref(), Some("#1e1e2e"));
        assert_eq!(mocha.colors.sidebar.as_deref(), Some("#181825"));
        assert_eq!(mocha.colors.border.as_deref(), Some("#313244"));
        assert_eq!(mocha.colors.foreground.as_deref(), Some("#cdd6f4"));
        assert_eq!(mocha.colors.primary.as_deref(), Some("#89b4fa"));
        assert_eq!(mocha.colors.success.as_deref(), Some("#a6e3a1"));
        assert_eq!(mocha.colors.danger.as_deref(), Some("#f38ba8"));
        assert_eq!(mocha.colors.warning.as_deref(), Some("#f9e2af"));
        assert_eq!(mocha.colors.info.as_deref(), Some("#cba6f7"));

        // Latte uses the light flavor's base, text, and accent colors.
        assert_eq!(latte.colors.background.as_deref(), Some("#eff1f5"));
        assert_eq!(latte.colors.sidebar.as_deref(), Some("#e6e9ef"));
        assert_eq!(latte.colors.border.as_deref(), Some("#ccd0da"));
        assert_eq!(latte.colors.foreground.as_deref(), Some("#4c4f69"));
        assert_eq!(latte.colors.primary.as_deref(), Some("#1e66f5"));
        assert_eq!(latte.colors.success.as_deref(), Some("#40a02b"));
        assert_eq!(latte.colors.danger.as_deref(), Some("#d20f39"));
        assert_eq!(latte.colors.warning.as_deref(), Some("#df8e1d"));
        assert_eq!(latte.colors.info.as_deref(), Some("#8839ef"));

        for theme in [&mocha, &latte] {
            // A selected row must not look identical to a hovered row.
            assert_ne!(theme.colors.list_active, theme.colors.list_hover);
            // `accent` is the neutral highlight fill; the blue action color
            // lives in `primary`, so ghost hovers are not painted blue.
            assert_ne!(theme.colors.accent, theme.colors.primary);
        }
    }

    #[test]
    fn every_legacy_color_literal_used_by_the_view_is_mapped() {
        let sources = [
            include_str!("view.rs"),
            include_str!("syntax.rs"),
            include_str!("view/helpers.rs"),
            include_str!("view/timeline.rs"),
            include_str!("view/render/composer.rs"),
            include_str!("view/render/dialogs.rs"),
            include_str!("view/render/pickers.rs"),
            include_str!("view/render/inspector/mod.rs"),
            include_str!("view/render/inspector/agent.rs"),
            include_str!("view/render/inspector/changes.rs"),
            include_str!("view/render/inspector/context.rs"),
            include_str!("view/render/inspector/files.rs"),
            include_str!("view/render/root.rs"),
            include_str!("view/render/sidebar.rs"),
            include_str!("view/render/tool.rs"),
        ];
        for source in sources {
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
                "ui source uses colors that are not mapped to a theme role: {unmapped:06x?}"
            );
        }
    }

    #[test]
    fn workspace_change_colors_keep_their_semantic_defaults() {
        assert_eq!(
            change_color(loom_protocol::WorkspaceChangeKind::Created),
            gpui_kit::rgb(0x9ad7bd)
        );
        assert_eq!(
            change_color(loom_protocol::WorkspaceChangeKind::Deleted),
            gpui_kit::rgb(0xfca5a5)
        );
        assert_eq!(
            change_color(loom_protocol::WorkspaceChangeKind::Modified),
            gpui_kit::rgb(0xfef3c7)
        );
    }
}
