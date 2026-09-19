//! Hand-written text input element and its buffer state.

use std::ops::Range;

use gpui::{
    App, Bounds, Context, Element, ElementId, ElementInputHandler, Entity, GlobalElementId,
    LayoutId, PaintQuad, Pixels, Render, ShapedLine, SharedString, Style, TextAlign, TextRun,
    Window, actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::{
    theme::{rgb, selection},
    view::LoomView,
};

actions!(
    loom_composer,
    [
        Backspace, Delete, Left, Right, SelectAll, Home, End, Paste, Copy, Submit
    ]
);

#[derive(Clone, Debug)]
pub(crate) struct TextBufferState {
    pub(crate) text: String,
    pub(crate) selected_range: Range<usize>,
    pub(crate) selection_reversed: bool,
    pub(crate) marked_range: Option<Range<usize>>,
}

impl TextBufferState {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selected_range: end..end,
            selection_reversed: false,
            marked_range: None,
        }
    }

    pub(crate) fn set_text(&mut self, text: impl Into<String>) {
        *self = Self::new(text);
    }

    pub(crate) fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(crate) fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text
            .get(..offset.min(self.text.len()))
            .unwrap_or_default()
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    pub(crate) fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_offset = 0;
        for character in self.text.chars() {
            if utf16_offset >= offset {
                break;
            }
            utf16_offset += character.len_utf16();
            utf8_offset += character.len_utf8();
        }
        utf8_offset
    }

    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(crate) fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .char_indices()
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .char_indices()
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.text.len())
    }

    pub(crate) fn line_ranges(&self) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        let mut start = 0;
        for (index, character) in self.text.char_indices() {
            if character == '\n' {
                ranges.push(start..index);
                start = index + character.len_utf8();
            }
        }
        ranges.push(start..self.text.len());
        ranges
    }

    pub(crate) fn line_and_column(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.text.len());
        let ranges = self.line_ranges();
        let line = ranges
            .iter()
            .position(|range| offset <= range.end)
            .unwrap_or_else(|| ranges.len().saturating_sub(1));
        (line, offset.saturating_sub(ranges[line].start))
    }

    pub(crate) fn move_to(&mut self, offset: usize, extend: bool) {
        let offset = offset.min(self.text.len());
        if extend {
            self.select_to(offset);
        } else {
            self.selected_range = offset..offset;
            self.selection_reversed = false;
        }
    }

    pub(crate) fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        let cursor = self.cursor_offset();
        let anchor = if self.selected_range.is_empty() {
            cursor
        } else if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        self.selected_range = anchor.min(offset)..anchor.max(offset);
        self.selection_reversed = offset < anchor;
    }

    pub(crate) fn replace_utf16(
        &mut self,
        range_utf16: Option<Range<usize>>,
        replacement: &str,
    ) -> Range<usize> {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let replacement = replacement.replace("\r\n", "\n").replace('\r', "\n");
        self.text.replace_range(range.clone(), &replacement);
        let cursor = range.start + replacement.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        range.start..cursor
    }

    pub(crate) fn select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_ranges().len().max(1)
    }
}

pub(crate) struct TextInputElement {
    pub(crate) view: Entity<LoomView>,
    pub(crate) field: InputField,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputField {
    Composer,
    Rename,
}

pub(crate) struct TextInputPrepaint {
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) lines: Vec<(Range<usize>, ShapedLine)>,
    pub(crate) selection: Option<PaintQuad>,
    pub(crate) cursor: Option<PaintQuad>,
}

pub(crate) struct LoomTooltip {
    pub(crate) text: SharedString,
}

impl Render for LoomTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555))
            .text_sm()
            .text_color(rgb(0xe5e7eb))
            .child(self.text.clone())
    }
}

impl IntoElement for TextInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = TextInputPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let line_count = self
            .view
            .read(cx)
            .input_state(self.field)
            .map_or(1, TextBufferState::line_count);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * line_count).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.view.read(cx);
        let Some(input) = input.input_state(self.field) else {
            return TextInputPrepaint {
                bounds,
                lines: Vec::new(),
                selection: None,
                cursor: None,
            };
        };
        let text = input.text.clone();
        let ranges = input.line_ranges();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut lines = Vec::with_capacity(ranges.len());
        for range in ranges {
            let line_text = text[range.clone()].to_owned();
            let run = TextRun {
                len: line_text.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                SharedString::from(line_text),
                font_size,
                &[run],
                None,
            );
            lines.push((range, line));
        }

        let line_height = window.line_height();
        let selection = if input.selected_range.is_empty() {
            None
        } else {
            let range = &input.selected_range;
            lines
                .iter()
                .enumerate()
                .find_map(|(index, (line_range, line))| {
                    let start = range.start.max(line_range.start);
                    let end = range.end.min(line_range.end);
                    (start < end).then(|| {
                        fill(
                            Bounds::from_corners(
                                point(
                                    bounds.left() + line.x_for_index(start - line_range.start),
                                    bounds.top() + line_height * index,
                                ),
                                point(
                                    bounds.left() + line.x_for_index(end - line_range.start),
                                    bounds.top() + line_height * (index + 1),
                                ),
                            ),
                            selection(),
                        )
                    })
                })
        };
        let cursor = if input.selected_range.is_empty() {
            let offset = input.cursor_offset();
            lines.iter().enumerate().find_map(|(index, (range, line))| {
                (offset >= range.start && offset <= range.end).then(|| {
                    fill(
                        Bounds::new(
                            point(
                                bounds.left() + line.x_for_index(offset - range.start),
                                bounds.top() + line_height * index,
                            ),
                            size(px(2.), line_height),
                        ),
                        rgb(0x60a5fa),
                    )
                })
            })
        } else {
            None
        };
        TextInputPrepaint {
            bounds,
            lines,
            selection,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).input_focus_handle(self.field);
        self.view.update(cx, |view, _| {
            view.input_field = self.field;
        });
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(prepaint.bounds, self.view.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line_height = window.line_height();
        for (index, (_, line)) in prepaint.lines.iter().enumerate() {
            let _ = line.paint(
                point(
                    prepaint.bounds.left(),
                    prepaint.bounds.top() + line_height * index,
                ),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_buffer_input_transforms_utf16_ranges_and_selection() {
        let mut buffer = TextBufferState::new("a😀c\nsecond");
        let replacement = buffer.replace_utf16(Some(1..3), "x");
        assert_eq!(replacement, 1..2);
        assert_eq!(buffer.text, "axc\nsecond");

        buffer.move_to(2, false);
        buffer.select_to(4);
        assert_eq!(buffer.selected_range, 2..4);
        buffer.replace_utf16(None, "😀");
        assert_eq!(buffer.text, "ax😀second");
        assert_eq!(buffer.cursor_offset(), 6);
        assert_eq!(buffer.offset_to_utf16(6), 4);
        assert_eq!(buffer.offset_from_utf16(4), 6);
    }
}
