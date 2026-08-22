//! A reusable single-line text field.
//!
//! The implementation is derived from the `input.rs` example shipped with gpui
//! 0.2.2 and extended with the features rulogman needs: a placeholder, password
//! masking, a disabled state and an `Enter` submit callback.
//!
//! All offsets stored in [`TextInput`] are byte offsets into the *real*
//! content. When the field is masked the rendered string is a different byte
//! sequence, so a [`DisplayMap`] translates between the two spaces; that keeps
//! the caret and selection correct for multi-byte text such as Hangul or emoji.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, InspectorElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    actions, div, fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::menu::{ContextMenu, MenuEntry};
use super::theme::theme;
use crate::i18n::ts;

actions!(
    rulogman_input,
    [
        /// Delete the grapheme before the caret, or the selection.
        Backspace,
        /// Delete the grapheme after the caret, or the selection.
        Delete,
        /// Move the caret one grapheme to the left.
        Left,
        /// Move the caret one grapheme to the right.
        Right,
        /// Extend the selection one grapheme to the left.
        SelectLeft,
        /// Extend the selection one grapheme to the right.
        SelectRight,
        /// Select the whole content.
        SelectAll,
        /// Move the caret to the start of the field.
        Home,
        /// Move the caret to the end of the field.
        End,
        /// Extend the selection to the start of the field.
        SelectHome,
        /// Extend the selection to the end of the field.
        SelectEnd,
        /// Open the macOS emoji / character palette.
        ShowCharacterPalette,
        /// Insert the clipboard contents.
        Paste,
        /// Copy the selection to the clipboard.
        Copy,
        /// Copy the selection to the clipboard and delete it.
        Cut,
        /// Confirm the current value, invoking the submit callback.
        Submit,
    ]
);

/// Key context that [`TextInput::init`] binds its keys to.
const KEY_CONTEXT: &str = "TextInput";

/// Character substituted for every grapheme when the field is masked.
const MASK_CHAR: char = '\u{2022}';

/// Modifier named in the shortcut hints of the edit menu.
///
/// Never translated — it is the name printed on the key — and branched on the
/// same `cfg` [`TextInput::init`] binds with, so a hint can never name a chord
/// this field does not answer to.
const SHORTCUT_MODIFIER: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Ctrl"
};

/// Callback invoked when the user presses `Enter` inside a [`TextInput`].
type SubmitHandler = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// Translates byte offsets between the real content and the rendered string.
///
/// Only built for masked fields; unmasked fields render their content verbatim
/// and therefore use identity mapping.
#[derive(Clone, Debug)]
struct DisplayMap {
    /// `(content offset, display offset)` at every grapheme boundary, including
    /// `0` and the end of the string. Sorted ascending on both components.
    boundaries: Vec<(usize, usize)>,
}

impl DisplayMap {
    /// Maps an offset in the real content to the equivalent display offset.
    fn to_display(&self, content_offset: usize) -> usize {
        match self
            .boundaries
            .binary_search_by_key(&content_offset, |(content, _)| *content)
        {
            Ok(ix) => self.boundaries[ix].1,
            Err(ix) => self
                .boundaries
                .get(ix.saturating_sub(1))
                .map_or(0, |(_, display)| *display),
        }
    }

    /// Maps an offset in the rendered string back to the real content.
    fn to_content(&self, display_offset: usize) -> usize {
        match self
            .boundaries
            .binary_search_by_key(&display_offset, |(_, display)| *display)
        {
            Ok(ix) => self.boundaries[ix].0,
            Err(ix) => self
                .boundaries
                .get(ix.saturating_sub(1))
                .map_or(0, |(content, _)| *content),
        }
    }
}

/// Maps `offset` through `map`, or returns it unchanged when there is no map.
fn to_display(map: Option<&DisplayMap>, offset: usize) -> usize {
    map.map_or(offset, |map| map.to_display(offset))
}

/// A single-line, focusable text field rendered as a gpui entity.
///
/// Create one with [`Context::new`](gpui::App::new) and keep the returned
/// [`Entity`] around; rendering it is as simple as passing the entity as a
/// child element.
///
/// ```ignore
/// let host = cx.new(|cx| TextInput::new(cx).placeholder("example.com"));
/// ```
pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    last_display_map: Option<DisplayMap>,
    is_selecting: bool,
    masked: bool,
    disabled: bool,
    invalid: bool,
    /// Where the pointer was when a right-click opened the edit menu. `None`
    /// while no menu is showing.
    context: Option<Point<Pixels>>,
    on_submit: Option<SubmitHandler>,
}

impl TextInput {
    /// Creates an empty text field owned by `cx`.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: SharedString::default(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            last_display_map: None,
            is_selecting: false,
            masked: false,
            disabled: false,
            invalid: false,
            context: None,
            on_submit: None,
        }
    }

    /// Registers the key bindings every `TextInput` relies on.
    ///
    /// Call once during application start-up. Bindings are scoped to the
    /// `TextInput` key context so they never leak into the rest of the app, and
    /// the clipboard / select-all chords follow platform conventions (`cmd` on
    /// macOS, `ctrl` elsewhere).
    pub fn init(cx: &mut App) {
        let modifier = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };

        let mut bindings = vec![
            KeyBinding::new("backspace", Backspace, Some(KEY_CONTEXT)),
            KeyBinding::new("delete", Delete, Some(KEY_CONTEXT)),
            KeyBinding::new("left", Left, Some(KEY_CONTEXT)),
            KeyBinding::new("right", Right, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-left", SelectLeft, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-right", SelectRight, Some(KEY_CONTEXT)),
            KeyBinding::new("home", Home, Some(KEY_CONTEXT)),
            KeyBinding::new("end", End, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-home", SelectHome, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-end", SelectEnd, Some(KEY_CONTEXT)),
            KeyBinding::new("enter", Submit, Some(KEY_CONTEXT)),
            KeyBinding::new(&format!("{modifier}-a"), SelectAll, Some(KEY_CONTEXT)),
            KeyBinding::new(&format!("{modifier}-c"), Copy, Some(KEY_CONTEXT)),
            KeyBinding::new(&format!("{modifier}-v"), Paste, Some(KEY_CONTEXT)),
            KeyBinding::new(&format!("{modifier}-x"), Cut, Some(KEY_CONTEXT)),
        ];

        if cfg!(target_os = "macos") {
            bindings.push(KeyBinding::new(
                "ctrl-cmd-space",
                ShowCharacterPalette,
                Some(KEY_CONTEXT),
            ));
        }

        cx.bind_keys(bindings);
    }

    /// Sets the text shown while the field is empty.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Replaces the text shown while the field is empty.
    ///
    /// The builder above covers a hint that is fixed for the life of the field.
    /// This is for the ones that have to follow a language switch, since the
    /// field entity outlives the locale it was created under.
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Renders every grapheme as a bullet, for password entry.
    ///
    /// The stored content is untouched; only the rendered string is masked.
    /// Copy and cut are disabled while masked so secrets cannot leak into the
    /// clipboard.
    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// Makes the field read-only and visually muted.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Marks the content as refused, outlining the field in the danger color.
    ///
    /// The field itself has no notion of what a valid value is — only its owner
    /// does — so this is a setter rather than a builder: whoever is checking
    /// the content keeps the flag in step with it. The outline wins over the
    /// focus ring, since a field one is typing into is exactly the field whose
    /// refusal has to stay visible. Setting the flag to the value it already
    /// holds is a no-op, which is what keeps an observer that sets it from
    /// waking itself up again.
    pub fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if self.invalid != invalid {
            self.invalid = invalid;
            cx.notify();
        }
    }

    /// Places the field at `index` in the window's tab order and makes it a tab
    /// stop.
    ///
    /// Fields without an explicit index stay out of the tab ring entirely, which
    /// is what keeps `Tab` from wandering into views that never opted in.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.focus_handle = self.focus_handle.clone().tab_index(index).tab_stop(true);
        self
    }

    /// Sets the callback invoked when the user presses `Enter`.
    pub fn on_submit(mut self, handler: impl Fn(&str, &mut Window, &mut App) + 'static) -> Self {
        self.on_submit = Some(Rc::new(handler));
        self
    }

    /// The current value of the field.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Replaces the value, collapsing the caret to the end of the new text.
    pub fn set_content(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    /// Clears the value and the selection.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_content(SharedString::default(), cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_submit.clone() {
            let content = self.content.clone();
            handler(&content, window, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.masked || self.selected_range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.content[self.selected_range.clone()].to_string(),
        ));
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.masked || self.selected_range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.content[self.selected_range.clone()].to_string(),
        ));
        self.replace_text_in_range(None, "", window, cx);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    /// Focuses the field and opens the edit menu at the pointer.
    ///
    /// The caret and the selection are deliberately left where they are: the
    /// menu's first two rows are about the selection, so moving it first would
    /// take away what the user right-clicked to act on.
    fn on_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        if self.menu_entries(cx).is_empty() {
            return;
        }
        self.context = Some(event.position);
        cx.notify();
    }

    /// Puts the edit menu away, if one is open.
    fn close_context(&mut self, cx: &mut Context<Self>) {
        if self.context.take().is_some() {
            cx.notify();
        }
    }

    /// Builds the rows of the edit menu, in display order.
    ///
    /// A row whose command would refuse is left out rather than shown doing
    /// nothing — cut and copy over a masked field would leak a password into
    /// the clipboard and are refused outright, and neither has anything to take
    /// with an empty selection; select-all has nothing to select in an empty
    /// field. Every row calls the very handler its key binding calls, so the
    /// menu adds a way in rather than a second implementation of anything.
    ///
    /// # Why this widget holds strings
    ///
    /// Everything else under `ui` is handed its text by the view that builds
    /// it, so that the localised sentences stay with the screens they belong
    /// to. These four labels are the exception, and translating them here is
    /// what keeps that rule honest rather than breaking it: a field is created
    /// at some twenty call sites across the dialogs, none of which has any
    /// interest in a clipboard, and threading four identical labels through all
    /// of them would scatter one menu's wording over the whole application
    /// instead of containing it. The menu is also the same menu in every one of
    /// those fields — there is nothing for a caller to say about it.
    fn menu_entries(&self, cx: &mut Context<Self>) -> Vec<MenuEntry> {
        let this = cx.entity();
        let has_selection = !self.selected_range.is_empty();

        let mut clipboard = Vec::new();
        if !self.masked && has_selection {
            clipboard.push(
                MenuEntry::new(ts!("input.menu_cut"))
                    .shortcut(format!("{SHORTCUT_MODIFIER}+X"))
                    .on_activate({
                        let this = this.clone();
                        move |window, cx| {
                            this.update(cx, |input, cx| input.cut(&Cut, window, cx));
                        }
                    }),
            );
            clipboard.push(
                MenuEntry::new(ts!("input.menu_copy"))
                    .shortcut(format!("{SHORTCUT_MODIFIER}+C"))
                    .on_activate({
                        let this = this.clone();
                        move |window, cx| {
                            this.update(cx, |input, cx| input.copy(&Copy, window, cx));
                        }
                    }),
            );
        }
        clipboard.push(
            MenuEntry::new(ts!("input.menu_paste"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+V"))
                .on_activate({
                    let this = this.clone();
                    move |window, cx| {
                        this.update(cx, |input, cx| input.paste(&Paste, window, cx));
                    }
                }),
        );

        let mut select = Vec::new();
        if !self.content.is_empty() {
            select.push(
                MenuEntry::new(ts!("input.menu_select_all"))
                    .shortcut(format!("{SHORTCUT_MODIFIER}+A"))
                    .on_activate(move |window, cx| {
                        this.update(cx, |input, cx| input.select_all(&SelectAll, window, cx));
                    }),
            );
        }

        let mut entries = Vec::new();
        for group in [clipboard, select] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::separator());
            }
            entries.extend(group);
        }
        entries
    }

    /// Builds the menu a right-click on the field opens, if one is open.
    ///
    /// Positioned in window coordinates, which is what lets one menu serve a
    /// field wherever it sits — including inside a modal dialog, where the
    /// pointer position the field stored is already the position the menu
    /// wants.
    fn render_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.context?;
        let this = cx.entity();
        Some(
            ContextMenu::new("text-input-context")
                .position(position)
                .entries(self.menu_entries(cx))
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |input, cx| input.close_context(cx));
                }),
        )
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }

        let display_index = line.closest_index_for_x(position.x - bounds.left());
        self.last_display_map
            .as_ref()
            .map_or(display_index, |map| map.to_content(display_index))
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

    /// Builds the string that is actually shaped, plus the offset map needed to
    /// place the caret when the content is masked.
    fn display_text(&self) -> (SharedString, Option<DisplayMap>) {
        if !self.masked || self.content.is_empty() {
            return (self.content.clone(), None);
        }

        let mut display = String::with_capacity(self.content.len());
        let mut boundaries = Vec::new();
        for (offset, _) in self.content.grapheme_indices(true) {
            boundaries.push((offset, display.len()));
            display.push(MASK_CHAR);
        }
        boundaries.push((self.content.len(), display.len()));

        (SharedString::from(display), Some(DisplayMap { boundaries }))
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
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
        if self.disabled {
            return;
        }

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let map = self.last_display_map.as_ref();
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(to_display(map, range.start)),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(to_display(map, range.end)),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let display_index = last_layout.index_for_x(point.x - line_point.x)?;
        let content_index = self
            .last_display_map
            .as_ref()
            .map_or(display_index, |map| map.to_content(display_index));
        Some(self.offset_to_utf16(content_index))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let focused = !self.disabled && self.focus_handle.is_focused(window);
        let disabled = self.disabled;
        // A menu belongs to the field the right-click focused, so one that has
        // outlived the focus — a dialog dismissed from under it, a `Tab` to the
        // next field — is about a click nobody is following up. Dropped here
        // rather than from a blur subscription because the field is built with
        // no window to hand; silently, because this frame is being built anyway.
        if self.context.is_some() && !focused {
            self.context = None;
        }
        let context = self.render_context(cx);

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .flex()
            .items_center()
            .w_full()
            .h(px(32.))
            .px(px(8.))
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(match (self.invalid, focused) {
                (true, _) => theme.danger,
                (false, true) => theme.accent,
                (false, false) => theme.border,
            })
            .bg(if disabled {
                theme.surface.opacity(0.6)
            } else {
                theme.surface
            })
            .text_size(px(14.))
            .line_height(px(20.))
            .cursor(if disabled {
                CursorStyle::Arrow
            } else {
                CursorStyle::IBeam
            })
            .when(!disabled, |this| {
                this.on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::left))
                    .on_action(cx.listener(Self::right))
                    .on_action(cx.listener(Self::select_left))
                    .on_action(cx.listener(Self::select_right))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::home))
                    .on_action(cx.listener(Self::end))
                    .on_action(cx.listener(Self::select_home))
                    .on_action(cx.listener(Self::select_end))
                    .on_action(cx.listener(Self::submit))
                    .on_action(cx.listener(Self::show_character_palette))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::cut))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    // Wired with the rest of the editing gestures, which is what
                    // leaves a disabled field with no menu at all: there is no
                    // row on it a read-only field could honour.
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
            })
            .child(TextElement { input: cx.entity() })
            .children(context)
    }
}

/// The custom element that shapes, measures and paints the field's single line.
struct TextElement {
    input: Entity<TextInput>,
}

/// Everything [`TextElement::prepaint`] hands over to `paint`.
struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    display_map: Option<DisplayMap>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
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
        _inspector_id: Option<&InspectorElementId>,
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
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = theme(cx);
        let input = self.input.read(cx);
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let marked_range = input.marked_range.clone();
        let is_empty = input.content.is_empty();
        let disabled = input.disabled;
        let style = window.text_style();

        let (display_text, display_map, text_color) = if is_empty {
            (input.placeholder.clone(), None, theme.text_muted)
        } else {
            let (text, map) = input.display_text();
            let color = if disabled {
                theme.text_muted
            } else {
                style.color
            };
            (text, map, color)
        };
        let map = display_map.as_ref();

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = marked_range.filter(|_| !is_empty) {
            let start = to_display(map, marked_range.start);
            let end = to_display(map, marked_range.end);
            vec![
                TextRun {
                    len: start,
                    ..run.clone()
                },
                TextRun {
                    len: end - start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let (selection, cursor) = if selected_range.is_empty() {
            let cursor_x = line.x_for_index(to_display(map, cursor));
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_x, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    theme.accent,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(to_display(map, selected_range.start)),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(to_display(map, selected_range.end)),
                            bounds.bottom(),
                        ),
                    ),
                    theme.accent.opacity(0.3),
                )),
                None,
            )
        };

        PrepaintState {
            line: Some(line),
            cursor,
            selection,
            display_map,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        let disabled = self.input.read(cx).disabled;

        if !disabled {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let line = prepaint.line.take().expect("prepaint always shapes a line");
        line.paint(
            bounds.origin,
            window.line_height(),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();

        if !disabled
            && focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let display_map = prepaint.display_map.take();
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.last_display_map = display_map;
        });
    }
}
