use super::{
    COMPACT_REVIEW_WIDTH, COMPACT_SIDEBAR_WIDTH, FULL_REVIEW_WIDTH, FULL_SIDEBAR_WIDTH,
    PHONE_TOUCH_TARGET, responsive_layout, review_panel_is_visible,
};
use gpui_kit::px;

#[test]
fn compact_windows_use_narrower_navigation_panels() {
    let layout = responsive_layout(px(959.));
    assert_eq!(layout.sidebar_width, COMPACT_SIDEBAR_WIDTH);
    assert_eq!(layout.review_width, COMPACT_REVIEW_WIDTH);
}

#[test]
fn wide_windows_keep_full_navigation_panels() {
    let layout = responsive_layout(px(960.));
    assert!(!layout.phone);
    assert_eq!(layout.sidebar_width, FULL_SIDEBAR_WIDTH);
    assert_eq!(layout.review_width, px(430.));
    let wide_layout = responsive_layout(px(1400.));
    assert_eq!(wide_layout.review_width, FULL_REVIEW_WIDTH);
}

#[test]
fn phone_windows_show_single_column_and_full_width_panels() {
    let layout = responsive_layout(px(390.));
    assert!(layout.phone);
    assert_eq!(layout.sidebar_width, px(390.));
    assert_eq!(layout.review_width, px(390.));
}

#[test]
fn very_narrow_phones_keep_the_session_drawer_in_view() {
    let layout = responsive_layout(px(280.));
    assert!(layout.phone);
    assert_eq!(layout.sidebar_width, px(280.));
    assert_eq!(layout.review_width, px(280.));
}

#[test]
fn phone_layouts_expand_interactive_targets_for_touch() {
    let phone = responsive_layout(px(390.));
    let desktop = responsive_layout(px(1280.));
    assert_eq!(phone.control_size(), PHONE_TOUCH_TARGET);
    assert!(phone.control_size() > desktop.control_size());
    assert!(phone.nav_row_padding() > desktop.nav_row_padding());
    assert!(phone.nav_row_font_size().0 > desktop.nav_row_font_size().0);
}

#[test]
fn review_panel_visibility_is_a_pure_layout_decision() {
    let desktop = responsive_layout(px(1280.));
    assert!(review_panel_is_visible(desktop, true, 1, false, false));
    assert!(!review_panel_is_visible(desktop, false, 1, false, false));
    assert!(!review_panel_is_visible(desktop, true, 0, false, false));
    for modal_open in 0..2 {
        let mut blockers = [false; 2];
        blockers[modal_open] = true;
        assert!(!review_panel_is_visible(
            desktop,
            true,
            1,
            blockers[0],
            blockers[1]
        ));
    }
    assert!(!review_panel_is_visible(
        responsive_layout(px(390.)),
        true,
        1,
        false,
        false
    ));
}
