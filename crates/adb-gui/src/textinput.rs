//! A single-line text field.
//!
//! GPUI 0.2.2 has no text-input widget, so this is a small `InputHandler`
//! implementation plus a custom element that paints the shaped line, the
//! selection and the caret. It backs the search box, the Ctrl+L path bar and
//! every dialog that asks for a name.
//!
//! Based on the `input` example shipped with gpui, trimmed to what a file
//! manager needs and restyled with the app palette.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, SharedString, Style,
    TextRun, UTF16Selection, Window, actions, div, fill, hsla, point, px, relative, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::{self, Themed};

actions!(
    text_field,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Submit,
    ]
);

/// The key context [`TextField`] elements register under.
pub const TEXT_CONTEXT: &str = "TextField";

/// Register the key bindings a text field needs.
///
/// Called once from `main`. GPUI key bindings are app-global, and without these
/// a keystroke inside a field would fall through to the browser's shortcuts.
pub fn install_key_bindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(TEXT_CONTEXT)),
        KeyBinding::new("delete", Delete, Some(TEXT_CONTEXT)),
        KeyBinding::new("left", Left, Some(TEXT_CONTEXT)),
        KeyBinding::new("right", Right, Some(TEXT_CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(TEXT_CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(TEXT_CONTEXT)),
        KeyBinding::new("secondary-a", SelectAll, Some(TEXT_CONTEXT)),
        KeyBinding::new("secondary-v", Paste, Some(TEXT_CONTEXT)),
        KeyBinding::new("home", Home, Some(TEXT_CONTEXT)),
        KeyBinding::new("end", End, Some(TEXT_CONTEXT)),
        KeyBinding::new("enter", Submit, Some(TEXT_CONTEXT)),
    ]);
}

/// What a [`TextField`] tells its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextFieldEvent {
    /// The user pressed Enter.
    Submitted(SharedString),
    /// The value changed, by typing or by pasting.
    Changed(SharedString),
}

/// A single-line editable text field.
pub struct TextField {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// Bounds of the painted line, kept for mouse hit-testing. Recomputing the
    /// shaped line on demand is cheaper than stashing a `ShapedLine` across the
    /// prepaint/paint boundary.
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// Monospace, used by the path bar and the diagnostics report.
    mono: bool,
}

impl TextField {
    /// An empty field.
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_bounds: None,
            is_selecting: false,
            mono: false,
        }
    }

    /// A field pre-filled with `value` and all of it selected, so the user types
    /// straight over the old name. This is what a rename wants.
    pub fn with_value(
        cx: &mut Context<Self>,
        value: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
    ) -> Self {
        let value = value.into();
        let end = value.len();
        let mut field = Self::new(cx, placeholder);
        field.content = value;
        field.selected_range = 0..end;
        field
    }

    /// Render the value in a monospace face.
    pub fn monospace(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    /// Take the caret, so a keystroke lands here without a click.
    ///
    /// `Ctrl+F` and `Ctrl+L` reveal their field and then have to call this;
    /// revealing alone left the user with a visible field they still had to
    /// click before they could type.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Replace the value, moving the caret to the end.
    pub fn set_value(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        let value = value.into();
        let end = value.len();
        self.content = value;
        self.selected_range = end..end;
        cx.notify();
    }

    /// Select the whole value, for a field the user is about to retype.
    pub fn select_everything(&mut self, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    // ── Editing actions ─────────────────────────────────────────────────────

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.previous_boundary(self.cursor_offset());
            self.move_to(target, cx);
        } else {
            let target = self.selected_range.start;
            self.move_to(target, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.next_boundary(self.selected_range.end);
            self.move_to(target, cx);
        } else {
            let target = self.selected_range.end;
            self.move_to(target, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.selected_range.end);
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        let end = self.content.len();
        self.select_to(end, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.content.len();
        self.move_to(end, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.previous_boundary(self.cursor_offset());
            self.select_to(target, cx);
        }
        self.replace(None, "", cx);
        let _ = window;
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.next_boundary(self.selected_range.end);
            self.select_to(target, cx);
        }
        self.replace(None, "", cx);
        let _ = window;
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // A path cannot contain a newline, so collapse any to spaces rather
            // than letting one paste insert a line break.
            self.replace(None, &text.replace('\n', " "), cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextFieldEvent::Submitted(self.content.clone()));
    }

    // ── Mouse ───────────────────────────────────────────────────────────────

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        let index = self.index_for_position(event.position, window, cx);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            let index = self.index_for_position(event.position, window, cx);
            self.select_to(index, cx);
        }
    }

    /// Byte offset in the value nearest to `position`.
    fn index_for_position(&self, position: Point<Pixels>, window: &mut Window, cx: &App) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.shape(window, cx)
            .closest_index_for_x(position.x - bounds.left())
    }

    /// Shape the current value with the resolved text style.
    fn shape(&self, window: &mut Window, _cx: &App) -> gpui::ShapedLine {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let run = TextRun {
            len: self.content.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(self.content.clone(), font_size, &[run], None)
    }

    // ── Selection bookkeeping ───────────────────────────────────────────────

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    /// Replace a range (or the selection) and report the new value.
    fn replace(&mut self, range: Option<Range<usize>>, new_text: &str, cx: &mut Context<Self>) {
        let range = range
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        let end = range.start + new_text.len();
        self.selected_range = end..end;
        self.marked_range = None;
        cx.notify();
        cx.emit(TextFieldEvent::Changed(self.content.clone()));
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf8_to_utf16(&self.content, range.start)..utf8_to_utf16(&self.content, range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf16_to_utf8(&self.content, range.start)..utf16_to_utf8(&self.content, range.end)
    }
}

impl EventEmitter<TextFieldEvent> for TextField {}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.as_ref().map(|r| self.range_from_utf16(r));
        self.replace(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.end)
            .unwrap_or_else(|| {
                let end = range.start + new_text.len();
                end..end
            });
        cx.notify();
        cx.emit(TextFieldEvent::Changed(self.content.clone()));
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.shape(window, cx);
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start), bounds.top()),
            point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let local = bounds.localize(&at)?;
        let line = self.shape(window, cx);
        let utf8_index = line.index_for_x(at.x - local.x)?;
        Some(utf16_to_utf8(&self.content, utf8_index))
    }
}

// ── UTF-8 <-> UTF-16 bridging ────────────────────────────────────────────────
//
// GPUI's input protocol counts UTF-16 code units; Rust slices count bytes.
// Android filenames are full of non-ASCII, so getting this wrong shows up as
// panics or a caret in the wrong place.

/// Convert a byte offset into `text` to a UTF-16 code-unit offset.
pub fn utf8_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in text.chars() {
        if utf8_count >= offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

/// Convert a UTF-16 code-unit offset into a byte offset in `text`.
pub fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

// ── Painting ─────────────────────────────────────────────────────────────────

struct TextRunElement {
    field: Entity<TextField>,
}

struct TextRunPrepaint {
    line: Option<gpui::ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextRunElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextRunElement {
    type RequestLayoutState = ();
    type PrepaintState = TextRunPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let field = self.field.read(cx);
        let content = field.content.clone();
        let selected_range = field.selected_range.clone();
        let cursor = field.cursor_offset();
        let focused = field.focus_handle.is_focused(window);
        let style = window.text_style();

        // Show the placeholder, dimmed, while the field is empty.
        let (display_text, text_color) = if content.is_empty() {
            (
                field.placeholder.clone(),
                style.color.blend(hsla(0., 0., 0., 0.5)),
            )
        } else {
            (content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text.clone(), font_size, &[run], None);

        let x_for = |index: usize| bounds.left() + line.x_for_index(index);
        let full_height = bounds.bottom() - bounds.top();
        let (selection, cursor) = if !selected_range.is_empty() {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(x_for(selected_range.start), bounds.top()),
                        point(x_for(selected_range.end), bounds.bottom()),
                    ),
                    rgba(0x3340A0FF),
                )),
                None,
            )
        } else if focused {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(x_for(cursor), bounds.top()),
                        size(px(1.5), full_height),
                    ),
                    style.color,
                )),
            )
        } else {
            (None, None)
        };

        TextRunPrepaint {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.field.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        line.paint(bounds.origin, window.line_height(), window, cx)
            .ok();
        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }

        // Keep the bounds for click hit-testing and IME queries.
        self.field
            .update(cx, |field, _| field.last_bounds = Some(bounds));
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let focused = self.focus_handle.is_focused(window);

        div()
            .flex()
            .key_context(TEXT_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .h(px(32.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .bg(rgba(0x0000004D))
            .border_1()
            .border_color(if focused { t.text_dim } else { t.border })
            .text_sm()
            .when(self.mono, |d| d.font_family(theme::MONO))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(TextRunElement { field: cx.entity() }),
            )
    }
}

impl Focusable for TextField {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_bridging_handles_astral_characters() {
        // "a" + an emoji (2 UTF-16 units, 4 UTF-8 bytes) + "b".
        let s = "a\u{1F600}b";
        assert_eq!(s.len(), 6, "bytes");
        assert_eq!(s.encode_utf16().count(), 4, "utf-16 units");

        assert_eq!(utf8_to_utf16(s, 0), 0);
        assert_eq!(utf8_to_utf16(s, 1), 1, "the ascii char is one unit each");
        assert_eq!(utf8_to_utf16(s, 5), 3, "the emoji is two units");
        assert_eq!(utf8_to_utf16(s, 6), 4);

        assert_eq!(utf16_to_utf8(s, 0), 0);
        assert_eq!(utf16_to_utf8(s, 1), 1);
        assert_eq!(utf16_to_utf8(s, 3), 5, "landing after the emoji");
        assert_eq!(utf16_to_utf8(s, 4), 6);
    }

    #[test]
    fn utf16_bridging_handles_combining_marks_and_cjk() {
        // 'e' (1 unit, 1 byte) + a combining acute (1 unit, 2 bytes) + a CJK
        // ideograph (1 unit, 3 bytes) = 3 UTF-16 units over 6 bytes.
        let s = "e\u{0301}\u{6587}";
        assert_eq!(s.len(), 6, "bytes");
        assert_eq!(s.encode_utf16().count(), 3, "utf-16 units");

        assert_eq!(utf8_to_utf16(s, 0), 0);
        assert_eq!(utf8_to_utf16(s, 1), 1, "just past 'e'");
        assert_eq!(utf8_to_utf16(s, 3), 2, "the combining mark is its own unit");
        assert_eq!(utf8_to_utf16(s, 6), 3);

        assert_eq!(utf16_to_utf8(s, 0), 0);
        assert_eq!(utf16_to_utf8(s, 1), 1);
        assert_eq!(utf16_to_utf8(s, 2), 3, "after the combining mark");
        assert_eq!(utf16_to_utf8(s, 3), 6);
    }

    #[test]
    fn utf16_offsets_past_the_end_clamp_to_the_length() {
        let s = "abc";
        assert_eq!(utf8_to_utf16(s, 99), 3);
        assert_eq!(utf16_to_utf8(s, 99), 3);
    }

    #[test]
    fn round_tripping_every_offset_is_stable() {
        let s = "a\u{1F600}b\u{6587}c";
        for byte in 0..=s.len() {
            if !s.is_char_boundary(byte) {
                continue;
            }
            let via_utf16 = utf16_to_utf8(s, utf8_to_utf16(s, byte));
            assert_eq!(via_utf16, byte, "round trip failed at byte {byte}");
        }
    }
}
