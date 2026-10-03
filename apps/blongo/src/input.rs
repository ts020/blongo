//! A plain-text input with IME, selection, clipboard and soft wrapping.
//!
//! Built on GPUI's `EntityInputHandler` the way gpui's `examples/input.rs`
//! (Apache-2.0) does, extended to multiple lines: the text is shaped with
//! `shape_text` (one `WrappedLine` per `\n`-separated line), the element
//! grows up to `max_lines` and scrolls to keep the cursor visible. There is
//! no cursor blink and nothing animates, so an idle input never repaints.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions,
    div, fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::theme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Submit,
        Newline,
        Cancel,
    ]
);

const CONTEXT: &str = "TextInput";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("secondary-a", SelectAll, c),
        KeyBinding::new("secondary-v", Paste, c),
        KeyBinding::new("secondary-c", Copy, c),
        KeyBinding::new("secondary-x", Cut, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("enter", Submit, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("escape", Cancel, c),
    ]);
}

pub enum InputEvent {
    /// Enter was pressed.
    Submit,
    /// Escape was pressed.
    Cancel,
}

/// Layout of the last paint, for mouse hit-testing and IME geometry.
struct LastLayout {
    lines: Vec<WrappedLine>,
    /// Byte offset of each logical line in `content`.
    starts: Vec<usize>,
    line_height: Pixels,
    /// Text origin (bounds origin shifted by the scroll offset).
    origin: Point<Pixels>,
}

impl LastLayout {
    /// Position of a byte offset relative to `origin`.
    fn position(&self, offset: usize) -> Point<Pixels> {
        let mut y = px(0.);
        for (ix, line) in self.lines.iter().enumerate() {
            let start = self.starts[ix];
            let end = start + line.len();
            if offset <= end || ix + 1 == self.lines.len() {
                let local = offset.saturating_sub(start).min(line.len());
                let p = line
                    .position_for_index(local, self.line_height)
                    .unwrap_or_default();
                return point(p.x, y + p.y);
            }
            y += line_height_of(line, self.line_height);
        }
        point(px(0.), px(0.))
    }

    /// Closest byte offset to a position relative to `origin`.
    fn offset(&self, position: Point<Pixels>) -> usize {
        let mut y = px(0.);
        for (ix, line) in self.lines.iter().enumerate() {
            let h = line_height_of(line, self.line_height);
            if position.y < y + h || ix + 1 == self.lines.len() {
                let local = point(position.x.max(px(0.)), (position.y - y).max(px(0.)));
                let i = match line.closest_index_for_position(local, self.line_height) {
                    Ok(i) | Err(i) => i,
                };
                return self.starts[ix] + i.min(line.len());
            }
            y += h;
        }
        0
    }
}

fn line_height_of(line: &WrappedLine, line_height: Pixels) -> Pixels {
    line_height * (line.wrap_boundaries.len() + 1) as f32
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    multiline: bool,
    max_lines: usize,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<LastLayout>,
    scroll_y: Pixels,
    is_selecting: bool,
    /// Column kept while moving up/down.
    preferred_x: Option<Pixels>,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, multiline: bool, cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            multiline,
            max_lines: if multiline { 10 } else { 1 },
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            scroll_y: px(0.),
            is_selecting: false,
            preferred_x: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content = if self.multiline {
            text.to_owned()
        } else {
            text.replace('\n', " ")
        };
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_y = px(0.);
        cx.notify();
    }

    // ------------------------------------------------------------- actions

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1.);
        self.move_to_keep_x(target, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1.);
        self.move_to_keep_x(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1.);
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1.);
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.content[..self.cursor_offset()]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        self.move_to(offset, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let offset = self.content[cursor..]
            .find('\n')
            .map_or(self.content.len(), |i| cursor + i);
        self.move_to(offset, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submit);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Cancel);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n");
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else if event.click_count >= 2 {
            let (start, end) = self.word_at(offset);
            self.selected_range = start..end;
            self.selection_reversed = false;
            cx.notify();
        } else {
            self.move_to(offset, cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    // ------------------------------------------------------------- helpers

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify()
    }

    fn move_to_keep_x(&mut self, offset: usize, cx: &mut Context<Self>) {
        let keep = self.preferred_x;
        self.move_to(offset, cx);
        self.preferred_x = keep;
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// Offset one visual row above (-1) or below (+1) the cursor.
    fn vertical_target(&mut self, direction: f32) -> usize {
        let cursor = self.cursor_offset();
        let Some(layout) = self.last_layout.as_ref() else {
            return cursor;
        };
        let pos = layout.position(cursor);
        let x = *self.preferred_x.get_or_insert(pos.x);
        let y = pos.y + layout.line_height * direction + layout.line_height / 2.;
        if y < px(0.) {
            return 0;
        }
        let total: Pixels = layout
            .lines
            .iter()
            .map(|l| line_height_of(l, layout.line_height))
            .fold(px(0.), |a, b| a + b);
        if y >= total {
            return self.content.len();
        }
        self.clamp(layout.offset(point(x, y)))
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = self.last_layout.as_ref() else {
            return self.content.len();
        };
        self.clamp(layout.offset(position - layout.origin))
    }

    /// A valid offset into `content`. The last layout may be the
    /// placeholder's (when empty) or stale by one frame.
    fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.content.len());
        while !self.content.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn word_at(&self, offset: usize) -> (usize, usize) {
        for (start, word) in self.content.unicode_word_indices() {
            if start <= offset && offset <= start + word.len() {
                return (start, start + word.len());
            }
        }
        (offset, offset)
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
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

    fn replace(&mut self, range: Range<usize>, new_text: &str) -> usize {
        let range = self.clamp(range.start)..self.clamp(range.end.max(range.start));
        let new_text = if self.multiline {
            std::borrow::Cow::Borrowed(new_text)
        } else {
            std::borrow::Cow::Owned(new_text.replace('\n', " "))
        };
        self.content.replace_range(range.clone(), &new_text);
        self.preferred_x = None;
        range.start + new_text.len()
    }
}

impl EntityInputHandler for TextInput {
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
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let end = self.replace(range, new_text);
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range.take();
        cx.notify();
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
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = self.clamp(range.start)..self.clamp(range.end);
        let end = self.replace(range.clone(), new_text);
        self.marked_range = (!new_text.is_empty()).then_some(range.start..end);
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| end..end);
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.origin + layout.position(range.start);
        let end = layout.origin + layout.position(range.end);
        let right = if end.y == start.y {
            end.x
        } else {
            start.x + px(2.)
        };
        Some(Bounds::from_corners(
            start,
            point(right.max(start.x + px(1.)), start.y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let offset = self.clamp(layout.offset(point - layout.origin));
        Some(self.offset_to_utf16(offset))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    origin: Point<Pixels>,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Shape the input's display text (placeholder when empty).
fn shape(
    input: &TextInput,
    width: Option<Pixels>,
    window: &mut Window,
) -> (Vec<WrappedLine>, bool) {
    let style = window.text_style();
    let placeholder = input.content.is_empty();
    let (text, color) = if placeholder {
        (input.placeholder.clone(), theme::text_faint())
    } else {
        (SharedString::from(input.content.clone()), style.color)
    };
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match input.marked_range.as_ref().filter(|_| !placeholder) {
        Some(marked) => vec![
            TextRun {
                len: marked.start,
                ..run.clone()
            },
            TextRun {
                len: marked.end - marked.start,
                underline: Some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text.len() - marked.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|run| run.len > 0)
        .collect(),
        None => vec![run],
    };
    let font_size = style.font_size.to_pixels(window.rem_size());
    let lines = window
        .text_system()
        .shape_text(text, font_size, &runs, width, None)
        .map(|lines| lines.into_vec())
        .unwrap_or_default();
    (lines, placeholder)
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

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
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.clone();
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                });
                let line_height = window.line_height();
                let input = input.read(cx);
                let max_lines = input.max_lines;
                let (lines, _) = shape(input, width, window);
                let rows: usize = lines.iter().map(|l| l.wrap_boundaries.len() + 1).sum();
                size(
                    width.unwrap_or(px(100.)),
                    line_height * rows.clamp(1, max_lines) as f32,
                )
            });
        (layout_id, ())
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
        let line_height = window.line_height();
        let input = self.input.read(cx);
        let (lines, placeholder) = shape(input, Some(bounds.size.width), window);
        let mut starts = Vec::with_capacity(lines.len());
        let mut start = 0;
        for line in &lines {
            starts.push(start);
            start += line.len() + 1;
        }
        let selected = input.selected_range.clone();
        let cursor_offset = if placeholder {
            0
        } else {
            input.cursor_offset()
        };
        let mut layout = LastLayout {
            lines,
            starts,
            line_height,
            origin: bounds.origin,
        };
        // Scroll so the cursor row is visible.
        let cursor_pos = layout.position(cursor_offset);
        let mut scroll_y = input.scroll_y;
        if cursor_pos.y < scroll_y {
            scroll_y = cursor_pos.y;
        } else if cursor_pos.y + line_height > scroll_y + bounds.size.height {
            scroll_y = cursor_pos.y + line_height - bounds.size.height;
        }
        let total: Pixels = layout
            .lines
            .iter()
            .map(|l| line_height_of(l, line_height))
            .fold(px(0.), |a, b| a + b);
        scroll_y = scroll_y
            .min((total - bounds.size.height).max(px(0.)))
            .max(px(0.));
        let origin = point(bounds.origin.x, bounds.origin.y - scroll_y);
        layout.origin = origin;

        let mut selections = Vec::new();
        let mut cursor = None;
        if selected.is_empty() || placeholder {
            let p = origin + cursor_pos;
            cursor = Some(fill(
                Bounds::new(p, size(px(1.5), line_height)),
                theme::accent(),
            ));
        } else {
            let a = layout.position(selected.start);
            let b = layout.position(selected.end);
            let right = bounds.size.width;
            let sel = theme::selection();
            if a.y == b.y {
                selections.push(fill(
                    Bounds::from_corners(origin + a, origin + point(b.x, b.y + line_height)),
                    sel,
                ));
            } else {
                selections.push(fill(
                    Bounds::from_corners(origin + a, origin + point(right, a.y + line_height)),
                    sel,
                ));
                if b.y > a.y + line_height {
                    selections.push(fill(
                        Bounds::from_corners(
                            origin + point(px(0.), a.y + line_height),
                            origin + point(right, b.y),
                        ),
                        sel,
                    ));
                }
                selections.push(fill(
                    Bounds::from_corners(
                        origin + point(px(0.), b.y),
                        origin + point(b.x, b.y + line_height),
                    ),
                    sel,
                ));
            }
        }
        let LastLayout { lines, starts, .. } = layout;
        let _ = input;
        self.input.update(cx, |input, _| input.scroll_y = scroll_y);
        PrepaintState {
            lines,
            starts,
            origin,
            cursor,
            selections,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let line_height = window.line_height();
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for selection in prepaint.selections.drain(..) {
                window.paint_quad(selection);
            }
            let mut y = prepaint.origin.y;
            for line in &prepaint.lines {
                let h = line_height_of(line, line_height);
                if y + h >= bounds.top() && y <= bounds.bottom() {
                    let _ = line.paint(
                        point(prepaint.origin.x, y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
                y += h;
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        let origin = prepaint.origin;
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(LastLayout {
                lines,
                starts,
                line_height,
                origin,
            });
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
