use super::*;

mod archive;
mod composer;
mod dialogs;
mod inspector;
mod pickers;
mod root;
pub(super) mod sidebar;
mod tool;

/// Wraps a single-line `KitInput` so a tap or click anywhere in the field —
/// including `BaseInput`'s frame padding and any outer wrapper padding —
/// focuses `input` and requests the virtual keyboard synchronously inside the
/// user gesture.
///
/// `gpui-component`'s `Input` paints its padding on an outer `BaseInput` frame
/// that tracks its own `frame_focus_handle`. Without this wrapper, a tap in
/// that padding focuses the frame instead of the inner `InputState`, leaving
/// the field uneditable and causing the web platform to hide the software
/// keyboard (`inputmode="none"`).
pub(super) fn focusable_input(
    input: &Entity<InputState>,
    child: impl IntoElement,
) -> gpui_kit::Div {
    let input = input.clone();
    div()
        .cursor_text()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            focus_input_on_tap(&input, window, cx);
        })
        .child(child)
}

/// Focuses `input` and requests the virtual keyboard when a tap on its wrapper
/// did not already land on the inner text element.
///
/// When the tap landed inside the inner text element, `gpui-base` already
/// focused `input`, placed the caret, set `window.default_prevented()` via
/// `track_focus`, and (for touch) requested the virtual keyboard. When the tap
/// landed in `BaseInput`'s frame padding, `BaseInput` moved focus onto its
/// `frame_focus_handle`, so `input` is not focused even if it held focus
/// before the tap. When the tap landed in outer wrapper padding around an
/// already-focused input, `default_prevented` remains false.
pub(super) fn focus_input_on_tap(input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    use gpui_kit::Focusable as _;

    if !input.read(cx).focus_handle(cx).is_focused(window) {
        input.update(cx, |state, cx| state.focus(window, cx));
        window.request_virtual_keyboard();
    } else if !window.default_prevented() {
        window.request_virtual_keyboard();
    }
}
