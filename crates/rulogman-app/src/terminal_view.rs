//! The terminal surface: a custom gpui [`Element`] that paints a
//! [`TerminalSnapshot`] plus the input plumbing around it.
//!
//! The element owns no state of its own. Everything it needs comes from the
//! [`Session`] it renders and from the [`TerminalView`] that hosts it; in
//! return it writes back the geometry it measured so that mouse positions can
//! be translated into terminal cells.
//!
//! Layout is a plain character grid: the cell width is measured once per frame
//! from a representative glyph of the configured monospace font, and the line
//! height is derived from the font size. The number of columns and rows the
//! element can show is recomputed during `prepaint` and pushed into the session
//! only when it actually changed.
//!
//! # Text input
//!
//! Keyboard input takes two disjoint routes, and keeping them disjoint is what
//! stops a keystroke from reaching the remote host twice:
//!
//! * **Printable characters** go through the platform input handler. The view
//!   implements [`EntityInputHandler`], the element installs it with
//!   [`Window::handle_input`], and the committed text arrives in
//!   [`TerminalView::replace_text_in_range`]. This is the route that makes IME
//!   composition (Hangul, Kana, Pinyin, dead keys) work.
//! * **Control, navigation and modifier chords** go through `on_key_down` and
//!   [`encode_key`]. That handler consumes the event, which stops the platform
//!   from also translating the key into a character message.
//!
//! [`to_key_input`] is the gate between the two: it returns `None` for anything
//! the input handler should own.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, BorderStyle, Bounds, ClipboardItem, Context, CursorStyle, DragMoveEvent,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Global, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, KeyBinding, KeyDownEvent, Keystroke, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollWheelEvent, ShapedLine,
    SharedString, Size, StrikethroughStyle, Style, Subscription, TextRun, UTF16Selection,
    UnderlineStyle, Window, actions, black, div, fill, font, outline, point, prelude::*, px,
    relative, rgb, size,
};
use rulogman_core::EffectiveTerminal;
use rulogman_term::{
    KeyCode, KeyInput, Rgb, RunFlags, ScrollPosition, StyledRun, TerminalLine, TerminalTheme,
    encode_key, encode_paste,
};

use crate::i18n::ts;
use crate::session::{Session, SessionStatus};
use crate::ui::{
    Button, ButtonVariant, ContextMenu, DraggedThumb, MenuEntry, Scrollbar, ScrollbarAxis,
    ScrollbarState, hide_later, hide_now, theme,
};
use crate::{
    BreakOutPane, CloseSession, DuplicateSplitBelow, DuplicateSplitRight, PANE_SHORTCUT_MODIFIER,
    SHORTCUT_MODIFIER, app_settings,
};

actions!(
    rulogman_terminal,
    [
        /// Copy the selected cells to the clipboard.
        CopySelection,
        /// Insert the clipboard contents into the remote shell.
        PasteClipboard,
    ]
);

/// Key context the terminal bindings are scoped to.
const KEY_CONTEXT: &str = "Terminal";

/// Name of the copy chord as the context menu prints it.
///
/// Never translated — it is what is printed on the keys — and branched on the
/// same `cfg` [`TerminalView::init`] binds with, so the hint and the binding
/// cannot drift apart.
const COPY_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+C"
} else {
    "Ctrl+Shift+C"
};

/// Name of the paste chord, on the same terms as [`COPY_SHORTCUT`].
const PASTE_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+V"
} else {
    "Ctrl+Shift+V"
};

/// Terminal font size used before the session's effective settings are known.
///
/// The real size comes from [`EffectiveTerminal::font_size`]; this only backs
/// the rare code paths (such as a scroll before the first paint) that run
/// without a session snapshot to hand.
pub(crate) const DEFAULT_FONT_SIZE: Pixels = px(14.);

/// Line height as a multiple of the font size.
///
/// Shared with [`crate::editor`] for the same reason its palette is: an editor
/// pane and the terminal pane beside it are one surface, and rows that do not
/// line up across a split are the first thing that gives that away.
pub(crate) const LINE_HEIGHT_RATIO: f32 = 1.3;

/// Padding between the terminal surface and its container.
const SURFACE_PADDING: Pixels = px(6.);

/// Element id of the scrollback's overlay scroll indicator.
///
/// Every pane has one, and each is its own element in its own subtree, so one
/// name serves them all — a drag of one is answered by the view it belongs to.
const SCROLLBAR: &str = "terminal-scrollbar";

/// Glyph measured to derive the width of one cell.
const SAMPLE_GLYPH: &str = "M";

/// Upper bound on the grid size, guarding against absurd window dimensions.
const MAX_GRID: f32 = 1000.;

/// Monospace families to try, most preferred first.
const MONOSPACE_CANDIDATES: &[&str] = if cfg!(target_os = "windows") {
    &["Consolas", "Cascadia Mono", "Courier New"]
} else if cfg!(target_os = "macos") {
    &["Menlo", "Monaco", "Courier New"]
} else {
    &["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono"]
};

/// The monospace font resolved once at start-up by [`TerminalView::init`].
struct TerminalFont(Font);

impl Global for TerminalFont {}

/// Returns the terminal font, falling back to the first candidate when
/// [`TerminalView::init`] has not run yet.
pub(crate) fn terminal_font(cx: &App) -> Font {
    cx.try_global::<TerminalFont>()
        .map(|global| global.0.clone())
        .unwrap_or_else(|| font(MONOSPACE_CANDIDATES[0]))
}

/// Resolves the font a session renders with.
///
/// An explicit [`EffectiveTerminal::font_family`] wins; otherwise the per-OS
/// monospace default resolved by [`TerminalView::init`] is used.
///
/// Shared with [`crate::editor_pane`], which draws its text surface in the same
/// font as the terminal the file was opened out of; one resolver means the two
/// can never disagree about what "the monospace font" is.
pub(crate) fn resolve_font(effective: &EffectiveTerminal, cx: &App) -> Font {
    match &effective.font_family {
        Some(family) => font(family),
        None => terminal_font(cx),
    }
}

/// Converts a terminal color into the color space gpui paints with.
///
/// Shared with [`crate::editor`], which derives its own palette from the same
/// scheme so that an editor pane and the terminal pane beside it read as one
/// surface; one conversion means the two can never drift apart.
pub(crate) fn to_hsla(color: Rgb) -> Hsla {
    rgb(color.to_u32()).into()
}

/// A cell of the visible grid, in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellPos {
    /// Row index, `0` is the topmost visible row.
    line: u16,
    /// Column index.
    col: u16,
}

/// What the last paint measured, used to map mouse positions onto cells and to
/// anchor the IME composition to the cursor.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    /// Bounds of the grid itself, excluding the surrounding padding.
    bounds: Bounds<Pixels>,
    /// Size of a single cell.
    cell: Size<Pixels>,
    /// Number of columns that fit into [`Geometry::bounds`].
    cols: u16,
    /// Number of rows that fit into [`Geometry::bounds`].
    rows: u16,
    /// Cell the terminal cursor occupied, where a composition starts.
    cursor: CellPos,
}

/// The text an IME is currently composing, before the user commits it.
///
/// The remote host knows nothing about composition, so this state is purely
/// local: it lives in [`TerminalView`], is drawn over the grid, and only the
/// committed result is ever written to the session.
///
/// Deliberately free of gpui types so the offset arithmetic — the part that
/// actually breaks, because the platform speaks UTF-16 while Rust strings are
/// UTF-8 — can be unit tested on its own.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Preedit {
    /// The text being composed; empty when no composition is in flight.
    text: String,
    /// Caret or selection inside [`Preedit::text`], as byte offsets.
    selection: Range<usize>,
}

impl Preedit {
    /// Whether a composition is in flight.
    fn is_active(&self) -> bool {
        !self.text.is_empty()
    }

    /// Converts a byte offset into [`Preedit::text`] to a UTF-16 offset.
    ///
    /// Offsets past the end clamp to the end of the text.
    fn to_utf16(&self, byte_offset: usize) -> usize {
        let mut utf16 = 0;
        let mut bytes = 0;
        for ch in self.text.chars() {
            if bytes >= byte_offset {
                break;
            }
            bytes += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    /// Converts a UTF-16 offset to a byte offset into [`Preedit::text`].
    ///
    /// An offset that lands inside a character — in the middle of a surrogate
    /// pair, for instance — rounds up to the end of that character, so the
    /// result is always a valid slice boundary.
    fn byte_offset(&self, utf16_offset: usize) -> usize {
        let mut utf16 = 0;
        let mut bytes = 0;
        for ch in self.text.chars() {
            if utf16 >= utf16_offset {
                break;
            }
            utf16 += ch.len_utf16();
            bytes += ch.len_utf8();
        }
        bytes
    }

    /// Converts a byte range to the equivalent UTF-16 range.
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.to_utf16(range.start)..self.to_utf16(range.end)
    }

    /// Converts a UTF-16 range to a byte range, ordered and on char boundaries.
    fn byte_range(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.byte_offset(range.start);
        let end = self.byte_offset(range.end);
        start.min(end)..start.max(end)
    }

    /// Length of the composition in UTF-16 code units.
    fn len_utf16(&self) -> usize {
        self.text.chars().map(char::len_utf16).sum()
    }

    /// The range the platform should render as "being composed".
    fn marked_range_utf16(&self) -> Option<Range<usize>> {
        self.is_active().then(|| 0..self.len_utf16())
    }

    /// The caret or selection inside the composition, in UTF-16 units.
    fn selection_utf16(&self) -> Range<usize> {
        self.range_to_utf16(&self.selection)
    }

    /// Replaces the composition, placing the caret at `selection_utf16`.
    ///
    /// `None` puts the caret at the end, which is what platforms send when
    /// they do not track a caret inside the composition.
    fn set(&mut self, text: &str, selection_utf16: Option<Range<usize>>) {
        self.text.clear();
        self.text.push_str(text);
        self.selection = match selection_utf16 {
            Some(range) => self.byte_range(&range),
            None => self.text.len()..self.text.len(),
        };
    }

    /// Ends the composition and returns whatever was being composed.
    fn take(&mut self) -> String {
        self.selection = 0..0;
        std::mem::take(&mut self.text)
    }

    /// Abandons the composition without producing any text.
    fn clear(&mut self) {
        self.take();
    }

    /// The slice covered by a UTF-16 range, empty when the range is degenerate.
    fn slice_utf16(&self, range: &Range<usize>) -> &str {
        let bytes = self.byte_range(range);
        self.text.get(bytes).unwrap_or_default()
    }

    /// The text before `utf16_offset`, used to measure the caret offset.
    fn prefix_utf16(&self, utf16_offset: usize) -> &str {
        self.text
            .get(..self.byte_offset(utf16_offset))
            .unwrap_or_default()
    }
}

/// Which of the workspace's pane commands the active pane could actually run.
///
/// The three rows of the context menu that dispatch a pane action ask questions
/// about a tab tree and a grid size, and a pane can see neither: whether the tab
/// has a second pane to break out, and whether a split would leave halves worth
/// having, are the workspace's to answer. This is that answer, reduced to the
/// three booleans the menu needs and nothing else — so the view neither reaches
/// into the workspace nor keeps a copy of its rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneCaps {
    /// Whether the active pane may be split into a second pane on its right.
    pub split_right: bool,
    /// Whether the active pane may be split into a second pane below it.
    pub split_below: bool,
    /// Whether the active pane may be moved out into a tab of its own.
    pub break_out: bool,
}

/// Asks the workspace, at menu-render time, what the active pane may do.
///
/// Called on every frame the menu is open rather than read once at
/// construction: a split, a break-out or a window resize changes every one of
/// the three answers without the view hearing about it.
///
/// The columns and rows of the asking pane's grid go in as arguments, rather
/// than being read back off the view, because this is called from inside that
/// view's own render: gpui leases an entity out of its map for the duration of
/// an update, so a workspace reaching back for the view mid-render would find
/// it missing and panic. The size is the only thing it would have needed the
/// view for — the tab tree behind the other two answers is its own.
pub type PaneCapsSource = Rc<dyn Fn(u16, u16, &App) -> PaneCaps>;

/// A focusable view rendering one [`Session`].
pub struct TerminalView {
    /// The session being rendered.
    session: Entity<Session>,
    /// What the workspace will let this pane's menu commands do; see
    /// [`PaneCapsSource`].
    caps: PaneCapsSource,
    /// Focus of the grid; keystrokes are only forwarded while it is focused.
    focus_handle: FocusHandle,
    /// Cell the current drag started on.
    anchor: Option<CellPos>,
    /// Selected range, `None` while nothing is selected.
    selection: Option<(CellPos, CellPos)>,
    /// Whether the left mouse button is currently extending a selection.
    selecting: bool,
    /// Sub-line scroll wheel remainder, so slow trackpad scrolls still move.
    scroll_residual: f32,
    /// Text an IME is composing; drawn locally and never sent until committed.
    preedit: Preedit,
    /// Where the pointer was when a right-click opened the pane's context menu.
    /// `None` while no menu is showing.
    context: Option<Point<Pixels>>,
    /// Geometry recorded by the last paint.
    geometry: Option<Geometry>,
    /// Whether the scrollback's overlay scroll indicator is on screen.
    scrollbar: ScrollbarState,
    /// Keeps the view repainting whenever the session changes.
    _observer: Subscription,
    /// Cancels a half-finished composition when the grid loses focus.
    _blur: Subscription,
}

impl TerminalView {
    /// Builds a view for `session` and starts observing it.
    ///
    /// `window` is needed to watch for focus loss: a composition that outlives
    /// the focus would otherwise reappear as a ghost preedit after a tab
    /// switch, because the platform stops asking us about it.
    ///
    /// `caps` is how the right-click menu finds out which of its pane commands
    /// are worth offering; see [`PaneCapsSource`].
    pub fn new(
        session: Entity<Session>,
        caps: PaneCapsSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observer = cx.observe(&session, |_, _, cx| cx.notify());
        let focus_handle = cx.focus_handle();
        let blur = cx.on_blur(&focus_handle, window, |this, _window, cx| {
            // The context menu goes with the focus for the same reason: a
            // right-click takes the focus before it opens one, so a menu that
            // outlived the focus is about a click nobody is following up — and
            // its pane commands would act on whichever pane took over.
            let stale = this.context.take().is_some();
            if this.preedit.is_active() {
                this.preedit.clear();
                cx.notify();
            } else if stale {
                cx.notify();
            }
        });

        Self {
            session,
            caps,
            focus_handle,
            anchor: None,
            selection: None,
            selecting: false,
            scroll_residual: 0.,
            preedit: Preedit::default(),
            context: None,
            geometry: None,
            scrollbar: ScrollbarState::new(),
            _observer: observer,
            _blur: blur,
        }
    }

    /// The session this view renders.
    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// Registers the terminal key bindings and resolves the monospace font.
    ///
    /// Call once during application start-up, after [`crate::ui::init`].
    pub fn init(cx: &mut App) {
        let available = cx.text_system().all_font_names();
        let family = MONOSPACE_CANDIDATES
            .iter()
            .copied()
            .find(|candidate| available.iter().any(|name| name == candidate))
            .unwrap_or(MONOSPACE_CANDIDATES[0]);
        log::debug!("terminal font family: {family}");
        cx.set_global(TerminalFont(font(family)));

        // `ctrl-c` and `ctrl-v` have to stay available to the remote shell, so
        // the clipboard uses the shifted chords everywhere except on macOS.
        let (copy, paste) = if cfg!(target_os = "macos") {
            ("cmd-c", "cmd-v")
        } else {
            ("ctrl-shift-c", "ctrl-shift-v")
        };
        cx.bind_keys([
            KeyBinding::new(copy, CopySelection, Some(KEY_CONTEXT)),
            KeyBinding::new(paste, PasteClipboard, Some(KEY_CONTEXT)),
        ]);
    }

    /// The selection with its two ends ordered from top-left to bottom-right.
    fn normalized_selection(&self) -> Option<(CellPos, CellPos)> {
        let (anchor, head) = self.selection?;
        Some(if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }

    /// Maps a window position onto a grid cell, clamped to the visible area.
    fn cell_at(&self, position: Point<Pixels>) -> Option<CellPos> {
        let geometry = self.geometry?;
        if geometry.cell.width <= px(0.) || geometry.cell.height <= px(0.) {
            return None;
        }

        let x = (position.x - geometry.bounds.left()) / geometry.cell.width;
        let y = (position.y - geometry.bounds.top()) / geometry.cell.height;
        Some(CellPos {
            line: clamp_index(y, geometry.rows),
            col: clamp_index(x, geometry.cols),
        })
    }

    /// Forwards a control or chord key press to the remote shell.
    ///
    /// Printable characters are deliberately ignored here: letting the event
    /// propagate is what makes the platform translate it into text and deliver
    /// it to [`TerminalView::replace_text_in_range`]. Handling it in both
    /// places would send every keystroke twice.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = to_key_input(&event.keystroke) else {
            return;
        };
        let (modes, charset) = {
            let term = self.session.read(cx).terminal();
            (term.modes(), term.charset())
        };
        let Some(bytes) = encode_key(input, modes, charset) else {
            return;
        };

        // Claiming the event stops the platform from additionally translating
        // this key into a character message.
        cx.stop_propagation();

        self.selection = None;
        self.send(bytes, "key", cx);
    }

    /// Writes already encoded bytes to the session.
    ///
    /// `source` names the route the bytes took; the trace is deliberately
    /// length-only, because keystrokes routinely carry passwords typed at a
    /// remote prompt and must never reach a log file.
    fn send(&mut self, bytes: Vec<u8>, source: &str, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        log::trace!("terminal input via {source}: {} bytes", bytes.len());
        self.session
            .update(cx, |session, cx| session.send_input(bytes, cx));
        cx.notify();
    }

    /// Scrolls the viewport through the scrollback.
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .geometry
            .map_or(DEFAULT_FONT_SIZE * LINE_HEIGHT_RATIO, |geometry| {
                geometry.cell.height
            });
        let pixels = event.delta.pixel_delta(line_height).y;
        let lines = pixels / line_height + self.scroll_residual;
        let whole = lines.trunc();
        self.scroll_residual = lines - whole;
        if whole == 0. {
            return;
        }

        self.session.update(cx, |session, cx| {
            session.terminal_mut().scroll_lines(whole as i32);
            cx.notify();
        });
    }

    /// The scrollback's overlay scroll indicator, as it stands.
    ///
    /// Measured in lines rather than pixels: the scrollback has no gpui scroll
    /// container behind it, only a viewport that the model slides up and down a
    /// buffer of rows. A bar cares about ratios alone, so lines do as well as
    /// anything — but the track is still the grid's own box, which is only
    /// known once a frame has been painted.
    fn scrollbar(&self, position: ScrollPosition) -> Option<Scrollbar> {
        let bounds = self.geometry?.bounds;
        Some(
            Scrollbar::new(
                SCROLLBAR,
                ScrollbarAxis::Vertical,
                bounds,
                position.rows as f32,
                position.history as f32,
                (position.history - position.display_offset) as f32,
            )
            .fade(self.scrollbar.fade()),
        )
    }

    /// Puts the bar up whenever the viewport has moved through the scrollback,
    /// and starts the clock that takes it down again.
    ///
    /// Watched by the display offset rather than by the position the thumb is
    /// drawn at, so that output arriving while the viewport sits at the bottom
    /// — which grows the scrollback under a motionless viewport — does not
    /// flash a bar on every line the remote host prints.
    fn watch_scroll(&mut self, position: ScrollPosition, cx: &mut Context<Self>) {
        if let Some(epoch) = self.scrollbar.moved(position.display_offset as f32) {
            hide_later(epoch, cx, |view| Some(&mut view.scrollbar));
        }
    }

    /// Scrolls the viewport to wherever the thumb has been dragged.
    ///
    /// Converted back to a line count and handed to the same
    /// `scroll_lines` the wheel uses, so a drag lands on the same
    /// whole lines a wheel does and is clamped by the same model.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        let position = self.session.read(cx).terminal().scroll_position();
        let Some(progress) = self
            .scrollbar(position)
            .and_then(|bar| bar.dragged(event, cx))
        else {
            return;
        };

        self.scrollbar.hold();
        // The bar runs top to bottom and the offset counts up from the bottom,
        // so the two run opposite ways.
        let target = ((1. - progress) * position.history as f32).round() as isize;
        let delta = target - position.display_offset as isize;
        if delta != 0 {
            self.session.update(cx, |session, cx| {
                session.terminal_mut().scroll_lines(delta as i32);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Lets go of the thumb, and starts the clock on the bar again.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) {
        if let Some(epoch) = self.scrollbar.release() {
            hide_later(epoch, cx, |view| Some(&mut view.scrollbar));
            cx.notify();
        }
    }

    /// Puts the bar up while the pointer rests on the edge it rides, and starts
    /// it going the moment the pointer leaves.
    fn hover_scrollbar(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            if self.scrollbar.hover_enter() {
                cx.notify();
            }
            return;
        }

        if let Some(epoch) = self.scrollbar.hover_leave() {
            hide_now(self, epoch, cx, |view| Some(&mut view.scrollbar));
        }
    }

    /// Focuses the grid and starts a selection drag.
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        cx.emit(PaneFocused);
        self.anchor = self.cell_at(event.position);
        self.selecting = self.anchor.is_some();
        self.selection = None;
        cx.notify();
    }

    /// Focuses the grid and opens its context menu at the pointer.
    ///
    /// Focus first, exactly as the left button takes it: the pane commands in
    /// the menu are dispatched as actions and the workspace answers them for
    /// whichever pane holds the keyboard, so the pane that was clicked has to be
    /// that pane before any row can run.
    ///
    /// Nothing else about the click is consumed — no caret moves and, crucially,
    /// the selection stands, because copying it is the first thing the menu
    /// offers.
    fn on_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        cx.emit(PaneFocused);
        self.context = Some(event.position);
        cx.notify();
    }

    /// Puts the context menu away, if one is open.
    fn close_context(&mut self, cx: &mut Context<Self>) {
        if self.context.take().is_some() {
            cx.notify();
        }
    }

    /// Selects every cell of the visible grid.
    ///
    /// The viewport, not the whole scrollback: a selection is addressed in
    /// viewport rows, so this is exactly as much as a copy afterwards can
    /// reach — the rows on screen, and whatever the viewport is scrolled to.
    fn select_visible(&mut self, cx: &mut Context<Self>) {
        let (cols, rows) = self.session.read(cx).terminal().size();
        self.anchor = None;
        self.selecting = false;
        self.selection = Some((
            CellPos { line: 0, col: 0 },
            CellPos {
                line: rows.saturating_sub(1),
                col: cols.saturating_sub(1),
            },
        ));
        cx.notify();
    }

    /// Forgets the scrollback of this pane, leaving the screen alone.
    fn clear_scrollback(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            session.terminal_mut().clear_scrollback();
            cx.notify();
        });
    }

    /// Brings the viewport back to the bottom of the scrollback.
    fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            session.terminal_mut().scroll_to_bottom();
            cx.notify();
        });
    }

    /// Extends an in-flight selection.
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        let (Some(anchor), Some(cell)) = (self.anchor, self.cell_at(event.position)) else {
            return;
        };
        let selection = (cell != anchor).then_some((anchor, cell));
        if selection != self.selection {
            self.selection = selection;
            cx.notify();
        }
    }

    /// Ends a selection drag, copying the selection to the clipboard when the
    /// session has copy-on-select enabled.
    ///
    /// The selection is left in place: copy-on-select mirrors it to the
    /// clipboard, it does not consume it.
    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        self.release_scrollbar(cx);
        if self.session.read(cx).effective(cx).copy_on_select {
            self.write_selection_to_clipboard(cx);
        }
    }

    /// Copies the selected cells to the clipboard.
    fn copy_selection(&mut self, _: &CopySelection, _window: &mut Window, cx: &mut Context<Self>) {
        self.write_selection_to_clipboard(cx);
    }

    /// Writes the current selection to the system clipboard, leaving it
    /// selected. A no-op when nothing is selected.
    fn write_selection_to_clipboard(&self, cx: &mut Context<Self>) {
        let Some((start, end)) = self.normalized_selection() else {
            return;
        };
        let snapshot = self.session.read(cx).terminal().snapshot();

        let mut text = String::new();
        for row in start.line..=end.line {
            let Some(line) = snapshot.lines.get(usize::from(row)) else {
                break;
            };
            if row > start.line {
                text.push('\n');
            }
            let (from, to) = span_for_row(row, start, end, snapshot.cols);
            text.push_str(&row_text(line, from, to));
        }

        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Sends the clipboard contents to the remote shell.
    fn paste_clipboard(
        &mut self,
        _: &PasteClipboard,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }

        let (modes, charset) = {
            let term = self.session.read(cx).terminal();
            (term.modes(), term.charset())
        };
        let bytes = encode_paste(&text, modes, charset);
        self.session
            .update(cx, |session, cx| session.send_input(bytes, cx));
    }

    /// Builds the menu a right-click on the grid opens, if one is open.
    ///
    /// Three groups of rows, separated in that order: the clipboard, the
    /// scrollback, and the pane itself. The first two act on this view and call
    /// straight into it; the third dispatches the same actions the keyboard
    /// shortcuts do, which the workspace answers for whichever pane holds the
    /// focus — the one that was just right-clicked.
    ///
    /// The rows are the same list every time, so a command that cannot run now
    /// is greyed rather than dropped; see [`MenuEntry::enabled`]. For the pane
    /// commands that takes an answer this view does not hold, and [`PaneCaps`]
    /// is where it comes from: the workspace is asked while the menu renders,
    /// and it answers about its *active* pane. That is this pane — a right-click
    /// focuses the grid before the menu opens, and the workspace follows the
    /// focus — so its verdict is about the pane the menu is standing over.
    ///
    /// The reconnect row is the exception that stays conditional: it appears
    /// only on a dead session, and its label depends on what died, so it is an
    /// alternative command rather than a fixed row that happens to be unusable.
    fn render_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.context?;
        let this = cx.entity();

        let session = self.session.read(cx);
        let scrolled = session.terminal().scroll_position().display_offset > 0;
        let live = session.status().is_live();
        let local = session.is_local();
        let (cols, rows) = session.terminal().size();
        let caps = (self.caps)(cols, rows, cx);

        let mut clipboard = vec![
            // Copy takes the selection, so it wants one; paste needs nothing
            // from this end — an empty clipboard is the platform's answer and
            // not a question asked here — and select-all always has a screen
            // to take.
            MenuEntry::new(ts!("terminal.menu_copy"))
                .shortcut(COPY_SHORTCUT)
                .enabled(self.selection.is_some())
                .on_activate({
                    let this = this.clone();
                    move |window, cx| {
                        this.update(cx, |view, cx| {
                            view.copy_selection(&CopySelection, window, cx);
                        });
                    }
                }),
        ];
        clipboard.push(
            MenuEntry::new(ts!("terminal.menu_paste"))
                .shortcut(PASTE_SHORTCUT)
                .on_activate({
                    let this = this.clone();
                    move |window, cx| {
                        this.update(cx, |view, cx| {
                            view.paste_clipboard(&PasteClipboard, window, cx);
                        });
                    }
                }),
        );
        clipboard.push(
            MenuEntry::new(ts!("terminal.menu_select_all")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |view, cx| view.select_visible(cx));
                }
            }),
        );

        let scrollback = vec![
            MenuEntry::new(ts!("terminal.menu_clear_scrollback")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |view, cx| view.clear_scrollback(cx));
                }
            }),
            // Already at the bottom, the jump has nowhere to go — greyed, so
            // that the row below the clear stays where the eye last found it.
            MenuEntry::new(ts!("terminal.menu_scroll_bottom"))
                .enabled(scrolled)
                .on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |view, cx| view.scroll_to_bottom(cx));
                    }
                }),
        ];

        let mut pane = vec![
            MenuEntry::new(ts!("menu.duplicate_right"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+D"))
                .enabled(caps.split_right)
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitRight), cx)
                }),
            MenuEntry::new(ts!("menu.duplicate_below"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+S"))
                .enabled(caps.split_below)
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitBelow), cx)
                }),
            MenuEntry::new(ts!("menu.break_out_pane"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+B"))
                .enabled(caps.break_out)
                .on_activate(|window, cx| window.dispatch_action(Box::new(BreakOutPane), cx)),
        ];
        if !live {
            // The overlay covering the grid carries the same command, worded the
            // same way: a local shell is started again, not reconnected to.
            let label = if local {
                ts!("session.restart")
            } else {
                ts!("session.reconnect")
            };
            let this = this.clone();
            pane.push(MenuEntry::new(label).on_activate(move |_window, cx| {
                this.update(cx, |_view, cx| cx.emit(ReconnectRequested));
            }));
        }

        let close = vec![
            MenuEntry::new(ts!("terminal.menu_close_pane"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+W"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(CloseSession), cx)),
        ];

        let mut entries = Vec::new();
        for group in [clipboard, scrollback, pane, close] {
            if !entries.is_empty() {
                entries.push(MenuEntry::separator());
            }
            entries.extend(group);
        }

        Some(
            ContextMenu::new("terminal-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |view, cx| view.close_context(cx));
                }),
        )
    }

    /// Builds the connection overlay shown while the session is not live.
    fn render_overlay(&self, status: &SessionStatus, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = theme(cx);
        let session_ref = self.session.read(cx);
        let label = session_ref.label();
        // A local session has no host to name and nothing to *re*connect to, so
        // it gets its own wording throughout rather than the remote sentences
        // with a shell name substituted into them.
        let local = session_ref.is_local();

        // The detail line quotes the transport verbatim, so it stays English;
        // only the headline around it follows the locale.
        let (headline, detail, retry): (SharedString, SharedString, bool) = match status {
            SessionStatus::Connected => return None,
            SessionStatus::Connecting => (
                if local {
                    ts!("session.overlay.local_starting", shell = label)
                } else {
                    ts!("session.overlay.connecting", host = label)
                },
                SharedString::default(),
                false,
            ),
            SessionStatus::Disconnected { reason } => (
                if local {
                    ts!("session.overlay.local_exited", shell = label)
                } else {
                    ts!("session.overlay.disconnected", host = label)
                },
                reason.clone().into(),
                true,
            ),
            SessionStatus::Failed { kind, message } => (
                if local {
                    ts!("session.overlay.local_failed", shell = label)
                } else {
                    ts!("session.overlay.failed", host = label)
                },
                ts!("session.failed", kind = kind, message = message),
                true,
            ),
        };

        // Restarting a shell is not reconnecting to anything, and the button is
        // the only thing on the card the user can act on.
        let retry_label = if local {
            ts!("session.restart")
        } else {
            ts!("session.reconnect")
        };

        let view = cx.entity();
        // `occlude` keeps drags on the card from selecting text underneath,
        // but it also hides the card's area from the grid's own mouse-down
        // hitbox — without a handler of its own, clicking the card of a split
        // pane would not focus the pane.
        let card = div()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    window.focus(&this.focus_handle);
                    cx.emit(PaneFocused);
                }),
            )
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.))
            .px(px(24.))
            .py(px(20.))
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .text_color(theme.text)
            .child(div().text_size(px(14.)).child(headline))
            .when(!detail.is_empty(), |this| {
                this.child(
                    div()
                        .max_w(px(420.))
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child(detail),
                )
            })
            .when(retry, |this| {
                this.child(
                    Button::new("terminal-reconnect", retry_label)
                        .variant(ButtonVariant::Primary)
                        .on_click(move |_, _window, cx| {
                            view.update(cx, |_view, cx| cx.emit(ReconnectRequested));
                        }),
                )
            });

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.overlay)
                .child(card)
                .into_any_element(),
        )
    }
}

/// Emitted the moment a click gives this view keyboard focus.
///
/// The workspace could learn the same thing from `cx.on_focus`, but gpui runs
/// focus listeners at the tail of a draw — after the frame was already built
/// from the old state — so anything repainted from there lags one input event
/// behind: the active-pane frame would only catch up when the mouse next
/// moved. An event emitted from the mouse handler itself is processed before
/// the click's frame is drawn, which keeps the frame, the tab label and the
/// status bar in step with the click.
pub struct PaneFocused;

impl EventEmitter<PaneFocused> for TerminalView {}

/// Emitted when the user asks for this pane's session to be opened again.
///
/// Raised by both places that offer it — the button on the connection overlay
/// and the row in the pane's own context menu — rather than either of them
/// calling [`Session::reconnect`] outright, because reconnecting is not this
/// session's business alone. Whether it may take its profile's port forwardings
/// back depends on what the *other* open sessions are holding, and the
/// workspace is the only thing that can see them; it answers that question and
/// then reconnects, in [`crate::Workspace::reconnect_session`].
pub struct ReconnectRequested;

impl EventEmitter<ReconnectRequested> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Bridges the platform IME to [`Preedit`].
///
/// The "document" this exposes is just the composition in flight: the terminal
/// grid itself is not editable, and the remote host is the only thing that can
/// change it. That is enough for every IME, which only ever asks about the
/// marked range and the caret.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = self.preedit.byte_range(&range_utf16);
        adjusted_range.replace(self.preedit.range_to_utf16(&bytes));
        Some(self.preedit.slice_utf16(&range_utf16).to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.preedit.selection_utf16(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.preedit.marked_range_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.preedit.is_active() {
            self.preedit.clear();
            cx.notify();
        }
    }

    /// Commits text: the composition ends and the result goes to the remote.
    ///
    /// Windows also routes a *cancelled* composition here, as an empty string,
    /// so the composition has to be dropped either way and only a non-empty
    /// result may be sent.
    ///
    /// The text goes through the session's charset but not through
    /// [`encode_paste`]: bracketed paste is for the clipboard, and wrapping
    /// typed text in it would make the remote application treat every
    /// composed word as a paste.
    ///
    /// This is the route every composed character takes — all Korean, Japanese
    /// and Chinese typing arrives here rather than at
    /// [`TerminalView::on_key_down`] — so it is the one that decides whether a
    /// legacy-charset host receives anything it can read at all.
    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_composing = self.preedit.is_active();
        self.preedit.clear();

        if text.is_empty() {
            if was_composing {
                cx.notify();
            }
            return;
        }

        self.selection = None;
        let charset = self.session.read(cx).terminal().charset();
        self.send(charset.encode(text), "ime", cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit.set(new_text, new_selected_range);
        cx.notify();
    }

    /// Where the composition sits on screen, so the candidate window can follow
    /// the caret instead of parking itself in the window corner.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry?;
        let effective = self.session.read(cx).effective(cx);
        let base_font = resolve_font(&effective, cx);
        let font_size = px(effective.font_size);

        let total = measure_text(&self.preedit.text, &base_font, font_size, window);
        let before = measure_text(
            self.preedit.prefix_utf16(range_utf16.start),
            &base_font,
            font_size,
            window,
        );
        let width = measure_text(
            self.preedit.slice_utf16(&range_utf16),
            &base_font,
            font_size,
            window,
        );

        let origin = preedit_origin(&geometry, element_bounds, total);
        Some(Bounds::new(
            point(origin.x + before, origin.y),
            size(width.max(geometry.cell.width), geometry.cell.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // The grid is not an editable document, so there is no offset to
        // report for an arbitrary point.
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let (background, status) = {
            let session = self.session.read(cx);
            (
                to_hsla(session.terminal().theme().background),
                session.status().clone(),
            )
        };
        // The window itself is transparent or blurred whenever the opacity is
        // below 1.0, so tinting the surface background lets the desktop show
        // through the default-background cells. Non-default cell backgrounds,
        // text and the cursor stay opaque; only this base fill carries the alpha.
        let background = app_settings::window_tint(background, cx);
        let overlay = self.render_overlay(&status, cx);
        let context = self.render_context(cx);
        let position = self.session.read(cx).terminal().scroll_position();
        self.watch_scroll(position, cx);
        let scrollbar = self.scrollbar(position).and_then(|bar| {
            bar.on_hover(cx.listener(|view, hovered: &bool, _window, cx| {
                view.hover_scrollbar(*hovered, cx);
            }))
            .render(&theme(cx))
        });
        let element = TerminalElement {
            view: cx.entity(),
            session: self.session.clone(),
            focused,
        };

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            // Answered from the root because gpui hands a drag move to every
            // listener of that type wherever it sits, and the root is what stays
            // mounted while the thumb slides out from under the pointer.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |view, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    view.drag_scrollbar(event, cx);
                },
            ))
            .child(
                div().size_full().p(SURFACE_PADDING).child(
                    // A box of exactly the grid's own size, with no padding of
                    // its own, so the bar drawn against it lines up with the
                    // rows rather than with the surface around them.
                    div()
                        .relative()
                        .size_full()
                        .child(element)
                        .children(scrollbar),
                ),
            )
            .children(overlay)
            // Deferred inside, so it paints above the grid and the connection
            // overlay whatever its place in this list.
            .children(context)
    }
}

/// Clamps `value` to a valid index into `len` cells.
fn clamp_index(value: f32, len: u16) -> u16 {
    if !value.is_finite() || value < 0. || len == 0 {
        return 0;
    }
    (value.floor() as u32).min(u32::from(len - 1)) as u16
}

/// The inclusive column span selected on `row`.
fn span_for_row(row: u16, start: CellPos, end: CellPos, cols: u16) -> (u16, u16) {
    let last = cols.saturating_sub(1);
    let from = if row == start.line { start.col } else { 0 };
    let to = if row == end.line { end.col } else { last };
    (from, to)
}

/// Reconstructs the text of `line` between the inclusive columns `from..=to`.
///
/// Gaps between runs are padded with spaces so that the extracted text keeps
/// its column alignment; trailing blanks are dropped, which is what a user
/// pasting a copied block expects.
fn row_text(line: &TerminalLine, from: u16, to: u16) -> String {
    let mut text = String::new();
    let mut col = 0u16;

    for run in &line.runs {
        while col < run.start_col {
            if (from..=to).contains(&col) {
                text.push(' ');
            }
            col = col.saturating_add(1);
        }
        if run.text.is_ascii() {
            for ch in run.text.chars() {
                if (from..=to).contains(&col) {
                    text.push(ch);
                }
                col = col.saturating_add(1);
            }
        } else {
            // A cluster is copied whole - its combining marks belong to the
            // base character and consume no column of their own - and the
            // columns it covers are skipped in one step.
            if (from..=to).contains(&col) {
                text.push_str(&run.text);
            }
            col = col.saturating_add(run.cells);
        }
    }

    text.trim_end().to_owned()
}

/// The character rendered at `col`, if any.
fn char_at(line: &TerminalLine, col: u16) -> Option<char> {
    let run = line
        .runs
        .iter()
        .rev()
        .find(|run| run.start_col <= col && col < run.start_col.saturating_add(run.cells))?;
    if run.text.is_ascii() {
        run.text.chars().nth(usize::from(col - run.start_col))
    } else {
        // The trailing column of a double width character reports the base
        // character rather than nothing.
        run.text.chars().next()
    }
}

/// Applies the bold and italic attributes of a run to `base`.
fn styled_font(base: &Font, flags: RunFlags) -> Font {
    let mut font = base.clone();
    if flags.contains(RunFlags::BOLD) {
        font.weight = FontWeight::BOLD;
    }
    if flags.contains(RunFlags::ITALIC) {
        font.style = FontStyle::Italic;
    }
    font
}

/// Builds the gpui text run that paints `run`.
fn text_run_for(run: &StyledRun, base: &Font) -> TextRun {
    let color = to_hsla(run.fg);
    TextRun {
        len: run.text.len(),
        font: styled_font(base, run.flags),
        color,
        // Backgrounds are painted as cell aligned quads instead, so that
        // adjacent runs never leave a seam between them.
        background_color: None,
        underline: run
            .flags
            .contains(RunFlags::UNDERLINE)
            .then(|| UnderlineStyle {
                thickness: px(1.),
                color: Some(color),
                wavy: false,
            }),
        strikethrough: run
            .flags
            .contains(RunFlags::STRIKEOUT)
            .then(|| StrikethroughStyle {
                thickness: px(1.),
                color: Some(color),
            }),
    }
}

/// Lowers a gpui keystroke into the layout independent input the terminal
/// encoder expects.
///
/// Returns `None` for every keystroke the platform input handler owns, which
/// is the guard against sending a key twice:
///
/// * printable characters without `ctrl` or `alt` — the IME turns those into
///   committed text, including a bare `space`;
/// * bare modifiers, and anything carrying the platform (`cmd` / `win`) key.
///
/// Named control and navigation keys always take this route, because no
/// platform turns them into text, and `ctrl` / `alt` chords do too, because an
/// IME never claims them.
fn to_key_input(keystroke: &Keystroke) -> Option<KeyInput> {
    let modifiers = keystroke.modifiers;
    if modifiers.platform || modifiers.function {
        return None;
    }
    // A chord can never produce text, so it belongs to the encoder. Note that
    // gpui already reports AltGr as neither `control` nor `alt` on the layouts
    // that use it, so AltGr characters correctly stay on the IME route.
    let chord = modifiers.control || modifiers.alt;

    let key = keystroke.key.as_str();
    let code = match key {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "escape" => KeyCode::Escape,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        // `Ctrl+Space` has to reach the encoder as NUL; a bare space is text.
        "space" if chord => KeyCode::Char(' '),
        "space" => return None,
        "shift" | "control" | "alt" | "platform" | "function" | "capslock" => return None,
        _ => match key
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
        {
            Some(number) => KeyCode::F(number),
            None if chord => KeyCode::Char(character_for(keystroke)?),
            None => return None,
        },
    };

    Some(KeyInput {
        code,
        ctrl: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
    })
}

/// The character a keystroke types.
///
/// `key_char` is preferred because it already went through the keyboard layout
/// and therefore carries the shifted or AltGr variant; `key` is the fallback
/// for chords where the platform reports no printable character, which is what
/// happens for every `ctrl`-modified key.
fn character_for(keystroke: &Keystroke) -> Option<char> {
    let typed = keystroke
        .key_char
        .as_deref()
        .filter(|text| !text.is_empty())
        .and_then(|text| single_char(text).filter(|ch| !ch.is_control()));
    typed.or_else(|| single_char(&keystroke.key))
}

/// Returns the only character of `text`, or `None` when it holds none or more.
fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// The custom element that measures, shapes and paints the terminal grid.
struct TerminalElement {
    /// View owning the selection and the cached geometry.
    view: Entity<TerminalView>,
    /// Session supplying the snapshot to paint.
    session: Entity<Session>,
    /// Whether the grid currently has keyboard focus.
    focused: bool,
}

/// Everything [`TerminalElement::prepaint`] hands over to `paint`.
struct TerminalPrepaint {
    /// Cell backgrounds followed by the selection highlight.
    quads: Vec<PaintQuad>,
    /// Shaped style runs together with their top-left corner.
    runs: Vec<(Point<Pixels>, ShapedLine)>,
    /// The cursor block or outline.
    cursor: Option<PaintQuad>,
    /// The glyph painted on top of a filled cursor.
    cursor_glyph: Option<(Point<Pixels>, ShapedLine)>,
    /// Backdrop behind the IME composition.
    preedit_background: Option<PaintQuad>,
    /// The composition text, drawn over the grid.
    preedit_text: Option<(Point<Pixels>, ShapedLine)>,
    /// Caret inside the composition.
    preedit_caret: Option<PaintQuad>,
    /// Height of one row.
    line_height: Pixels,
    /// Geometry to hand back to the view.
    geometry: Geometry,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaint;

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
        style.size.height = relative(1.).into();
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
        let effective = self.session.read(cx).effective(cx);
        let base_font = resolve_font(&effective, cx);
        let font_size = px(effective.font_size);
        let line_height = font_size * LINE_HEIGHT_RATIO;
        let cell_width = measure_cell(&base_font, font_size, window);
        let cell = size(cell_width, line_height);

        let cols = grid_extent(bounds.size.width, cell_width);
        let rows = grid_extent(bounds.size.height, line_height);
        self.session
            .update(cx, |session, cx| session.resize(cols, rows, cx));

        let (snapshot, palette) = {
            let session = self.session.read(cx);
            (
                session.terminal().snapshot(),
                session.terminal().theme().clone(),
            )
        };
        let (selection, preedit) = {
            let view = self.view.read(cx);
            (view.normalized_selection(), view.preedit.clone())
        };

        let mut quads = Vec::new();
        let mut runs = Vec::new();
        for (row, line) in snapshot.lines.iter().enumerate() {
            let y = bounds.origin.y + line_height * row as f32;
            // Every run starts on a real grid column, and the model keeps each
            // non-ASCII cluster in a run of its own, so shaping per run snaps
            // the whole row back onto the grid. A cluster whose glyph is wider
            // than its cells still spills over its neighbour, exactly as other
            // terminals let it.
            for run in &line.runs {
                let origin = point(bounds.origin.x + cell_width * f32::from(run.start_col), y);
                let text_run = text_run_for(run, &base_font);
                let shaped = window.text_system().shape_line(
                    SharedString::from(run.text.clone()),
                    font_size,
                    &[text_run],
                    None,
                );
                if run.bg != palette.background {
                    // Sized from the columns the run owns rather than from the
                    // shaped width, which a fallback font may report wider or
                    // narrower than the cells it was given.
                    quads.push(fill(
                        Bounds::new(origin, size(cell_width * f32::from(run.cells), line_height)),
                        to_hsla(run.bg),
                    ));
                }
                runs.push((origin, shaped));
            }
        }

        if let Some((start, end)) = selection {
            let selection_color = to_hsla(palette.selection);
            for row in start.line..=end.line.min(snapshot.rows.saturating_sub(1)) {
                let (from, to) = span_for_row(row, start, end, snapshot.cols);
                let left = bounds.origin.x + cell_width * f32::from(from);
                let right = bounds.origin.x + cell_width * f32::from(to + 1);
                let top = bounds.origin.y + line_height * f32::from(row);
                quads.push(fill(
                    Bounds::from_corners(point(left, top), point(right, top + line_height)),
                    selection_color,
                ));
            }
        }

        let cursor_cell = CellPos {
            line: snapshot.cursor.line,
            col: snapshot.cursor.col,
        };

        // While an IME is composing, the composition owns the caret: the block
        // cursor would otherwise sit underneath the composed text.
        let (cursor, cursor_glyph) = if snapshot.cursor_visible && !preedit.is_active() {
            let origin = point(
                bounds.origin.x + cell_width * f32::from(snapshot.cursor.col),
                bounds.origin.y + line_height * f32::from(snapshot.cursor.line),
            );
            let rect = Bounds::new(origin, cell);
            let color = to_hsla(palette.cursor);
            if self.focused {
                let glyph = snapshot
                    .lines
                    .get(usize::from(snapshot.cursor.line))
                    .and_then(|line| char_at(line, snapshot.cursor.col))
                    .filter(|ch| !ch.is_whitespace())
                    .map(|ch| {
                        let text = SharedString::from(ch.to_string());
                        let run = TextRun {
                            len: text.len(),
                            font: base_font.clone(),
                            color: to_hsla(palette.background),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let shaped = window
                            .text_system()
                            .shape_line(text, font_size, &[run], None);
                        (origin, shaped)
                    });
                (Some(fill(rect, color)), glyph)
            } else {
                (Some(outline(rect, color, BorderStyle::Solid)), None)
            }
        } else {
            (None, None)
        };

        let geometry = Geometry {
            bounds,
            cell,
            cols,
            rows,
            cursor: cursor_cell,
        };

        let (preedit_background, preedit_text, preedit_caret) = if preedit.is_active() {
            let shaped = shape_preedit(&preedit.text, &base_font, font_size, &palette, window);
            let origin = preedit_origin(&geometry, bounds, shaped.width);
            let caret_x = origin.x
                + measure_text(
                    preedit
                        .text
                        .get(..preedit.selection.end)
                        .unwrap_or_default(),
                    &base_font,
                    font_size,
                    window,
                );
            (
                Some(fill(
                    Bounds::new(origin, size(shaped.width, line_height)),
                    to_hsla(palette.selection),
                )),
                Some((origin, shaped)),
                Some(fill(
                    Bounds::new(point(caret_x, origin.y), size(px(2.), line_height)),
                    to_hsla(palette.cursor),
                )),
            )
        } else {
            (None, None, None)
        };

        TerminalPrepaint {
            quads,
            runs,
            cursor,
            cursor_glyph,
            preedit_background,
            preedit_text,
            preedit_caret,
            line_height,
            geometry,
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
        // Route platform text input — IME composition included — at the grid.
        // `handle_input` installs nothing unless the handle is focused.
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        let line_height = prepaint.line_height;

        for quad in prepaint.quads.drain(..) {
            window.paint_quad(quad);
        }
        for (origin, run) in &prepaint.runs {
            run.paint(*origin, line_height, window, cx).ok();
        }
        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }
        if let Some((origin, glyph)) = prepaint.cursor_glyph.take() {
            glyph.paint(origin, line_height, window, cx).ok();
        }

        if let Some(background) = prepaint.preedit_background.take() {
            window.paint_quad(background);
        }
        if let Some((origin, text)) = prepaint.preedit_text.take() {
            text.paint(origin, line_height, window, cx).ok();
        }
        if let Some(caret) = prepaint.preedit_caret.take() {
            window.paint_quad(caret);
        }

        let geometry = prepaint.geometry;
        self.view
            .update(cx, |view, _cx| view.geometry = Some(geometry));
    }
}

/// Measures the advance of one cell in `base_font` at `font_size`.
fn measure_cell(base_font: &Font, font_size: Pixels, window: &mut Window) -> Pixels {
    let run = TextRun {
        len: SAMPLE_GLYPH.len(),
        font: base_font.clone(),
        color: black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line =
        window
            .text_system()
            .shape_line(SharedString::from(SAMPLE_GLYPH), font_size, &[run], None);
    line.width.max(px(1.))
}

/// How many whole cells of `cell` fit into `available`.
fn grid_extent(available: Pixels, cell: Pixels) -> u16 {
    if cell <= px(0.) {
        return 1;
    }
    (available / cell).floor().clamp(1., MAX_GRID) as u16
}

/// Shapes the composition, underlined so it reads as "not committed yet".
fn shape_preedit(
    text: &str,
    base_font: &Font,
    font_size: Pixels,
    palette: &TerminalTheme,
    window: &mut Window,
) -> ShapedLine {
    let color = to_hsla(palette.foreground);
    let run = TextRun {
        len: text.len(),
        font: base_font.clone(),
        color,
        background_color: None,
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(to_hsla(palette.cursor)),
            wavy: false,
        }),
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), font_size, &[run], None)
}

/// Width `text` occupies in the terminal font.
///
/// Composed text is usually full width (Hangul, Kana, Han), so the advance has
/// to come from the shaper rather than from a cell count.
fn measure_text(text: &str, base_font: &Font, font_size: Pixels, window: &mut Window) -> Pixels {
    if text.is_empty() {
        return px(0.);
    }
    let run = TextRun {
        len: text.len(),
        font: base_font.clone(),
        color: black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), font_size, &[run], None)
        .width
}

/// Top-left corner the composition is drawn at.
///
/// It starts at the terminal cursor, but slides left when it would otherwise
/// run past the right edge of the grid, so a long composition near the margin
/// stays readable instead of being clipped away.
fn preedit_origin(geometry: &Geometry, bounds: Bounds<Pixels>, width: Pixels) -> Point<Pixels> {
    let x = bounds.left() + geometry.cell.width * f32::from(geometry.cursor.col);
    let y = bounds.top() + geometry.cell.height * f32::from(geometry.cursor.line);
    let rightmost = (bounds.right() - width).max(bounds.left());
    point(x.min(rightmost), y)
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::Modifiers;

    /// A keystroke with no modifiers, as the platform reports a plain key.
    fn key(name: &str, typed: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::default(),
            key: name.to_owned(),
            key_char: typed.map(str::to_owned),
        }
    }

    /// A keystroke carrying `modifiers`.
    fn chord(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: name.to_owned(),
            key_char: None,
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Default::default()
        }
    }

    // --- to_key_input: the guard against sending a keystroke twice ---------

    #[test]
    fn printable_keys_are_left_to_the_input_handler() {
        // Anything that can become text must not also be encoded here, or the
        // remote host receives it once from `on_key_down` and once from the
        // platform input handler.
        for keystroke in [
            key("a", Some("a")),
            key("1", Some("1")),
            key("-", Some("-")),
            key("space", None),
        ] {
            assert_eq!(
                to_key_input(&keystroke),
                None,
                "{:?} must take the IME route",
                keystroke.key
            );
        }
    }

    #[test]
    fn shifted_printable_keys_are_left_to_the_input_handler() {
        let shifted = Keystroke {
            modifiers: Modifiers {
                shift: true,
                ..Default::default()
            },
            key: "a".to_owned(),
            key_char: Some("A".to_owned()),
        };
        assert_eq!(to_key_input(&shifted), None);
    }

    #[test]
    fn control_and_navigation_keys_are_encoded() {
        let cases = [
            ("enter", KeyCode::Enter),
            ("tab", KeyCode::Tab),
            ("backspace", KeyCode::Backspace),
            ("escape", KeyCode::Escape),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
            ("home", KeyCode::Home),
            ("end", KeyCode::End),
            ("pageup", KeyCode::PageUp),
            ("pagedown", KeyCode::PageDown),
            ("insert", KeyCode::Insert),
            ("delete", KeyCode::Delete),
            ("f1", KeyCode::F(1)),
            ("f12", KeyCode::F(12)),
        ];
        for (name, expected) in cases {
            let input = to_key_input(&key(name, None))
                .unwrap_or_else(|| panic!("{name} should be encoded"));
            assert_eq!(input.code, expected, "{name}");
        }
    }

    #[test]
    fn modifier_chords_are_encoded() {
        let ctrl_c = to_key_input(&chord("c", ctrl())).expect("ctrl-c is a chord");
        assert_eq!(ctrl_c.code, KeyCode::Char('c'));
        assert!(ctrl_c.ctrl);

        let alt_b = to_key_input(&chord("b", alt())).expect("alt-b is a chord");
        assert_eq!(alt_b.code, KeyCode::Char('b'));
        assert!(alt_b.alt);
    }

    #[test]
    fn control_space_is_encoded_but_plain_space_is_text() {
        let ctrl_space = to_key_input(&chord("space", ctrl())).expect("ctrl-space is a chord");
        assert_eq!(ctrl_space.code, KeyCode::Char(' '));
        assert!(ctrl_space.ctrl);

        assert_eq!(to_key_input(&key("space", None)), None);
    }

    #[test]
    fn bare_modifiers_and_platform_chords_are_ignored() {
        for name in [
            "shift", "control", "alt", "platform", "function", "capslock",
        ] {
            assert_eq!(to_key_input(&key(name, None)), None, "{name}");
        }

        let platform = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(to_key_input(&chord("c", platform)), None);
    }

    // --- Preedit: UTF-8 / UTF-16 offset arithmetic ------------------------

    #[test]
    fn a_fresh_preedit_is_inactive() {
        let preedit = Preedit::default();
        assert!(!preedit.is_active());
        assert_eq!(preedit.marked_range_utf16(), None);
        assert_eq!(preedit.selection_utf16(), 0..0);
    }

    #[test]
    fn hangul_marks_one_utf16_unit_per_syllable() {
        // Each syllable is 3 bytes of UTF-8 but a single UTF-16 unit, so a
        // marked range reported in bytes would be three times too wide.
        let mut preedit = Preedit::default();
        preedit.set("한글", None);

        assert!(preedit.is_active());
        assert_eq!(preedit.text.len(), 6);
        assert_eq!(preedit.len_utf16(), 2);
        assert_eq!(preedit.marked_range_utf16(), Some(0..2));
        // No caret was supplied, so it sits at the end.
        assert_eq!(preedit.selection, 6..6);
        assert_eq!(preedit.selection_utf16(), 2..2);
    }

    #[test]
    fn emoji_occupies_two_utf16_units() {
        let mut preedit = Preedit::default();
        preedit.set("😀", None);

        assert_eq!(preedit.text.len(), 4);
        assert_eq!(preedit.len_utf16(), 2);
        assert_eq!(preedit.marked_range_utf16(), Some(0..2));
        assert_eq!(preedit.byte_offset(2), 4);
        assert_eq!(preedit.to_utf16(4), 2);
    }

    #[test]
    fn an_offset_inside_a_surrogate_pair_rounds_to_a_char_boundary() {
        let mut preedit = Preedit::default();
        preedit.set("😀a", None);

        // Offset 1 is the low half of the surrogate pair; slicing there would
        // panic, so it has to round up to the end of the emoji.
        assert_eq!(preedit.byte_offset(1), 4);
        assert_eq!(preedit.byte_offset(3), 5);
        assert_eq!(preedit.slice_utf16(&(0..1)), "😀");
        assert_eq!(preedit.slice_utf16(&(2..3)), "a");
    }

    #[test]
    fn the_caret_is_placed_from_utf16_offsets() {
        let mut preedit = Preedit::default();
        preedit.set("한글", Some(1..1));
        assert_eq!(preedit.selection, 3..3);
        assert_eq!(preedit.selection_utf16(), 1..1);

        preedit.set("😀😀", Some(2..2));
        assert_eq!(preedit.selection, 4..4);
        assert_eq!(preedit.selection_utf16(), 2..2);
    }

    #[test]
    fn a_reversed_caret_range_is_ordered() {
        let mut preedit = Preedit::default();
        // Built field-wise because a literal `2..0` is a compile-time lint.
        preedit.set("한글", Some(Range { start: 2, end: 0 }));
        assert_eq!(preedit.selection, 0..6);
    }

    #[test]
    fn updating_a_composition_replaces_it_wholesale() {
        let mut preedit = Preedit::default();
        preedit.set("ㅎ", None);
        assert_eq!(preedit.len_utf16(), 1);

        preedit.set("하", None);
        assert_eq!(preedit.text, "하");
        assert_eq!(preedit.len_utf16(), 1);

        preedit.set("한", None);
        assert_eq!(preedit.text, "한");
        assert_eq!(preedit.marked_range_utf16(), Some(0..1));
    }

    #[test]
    fn committing_returns_the_text_and_resets_the_state() {
        let mut preedit = Preedit::default();
        preedit.set("한글", None);

        assert_eq!(preedit.take(), "한글");
        assert!(!preedit.is_active());
        assert_eq!(preedit.marked_range_utf16(), None);
        assert_eq!(preedit.selection, 0..0);
    }

    #[test]
    fn cancelling_drops_the_text() {
        let mut preedit = Preedit::default();
        preedit.set("한", None);
        preedit.clear();

        assert!(!preedit.is_active());
        assert_eq!(preedit.text, "");
        assert_eq!(preedit.marked_range_utf16(), None);
    }

    #[test]
    fn slicing_and_prefixing_never_panic_on_out_of_range_offsets() {
        let mut preedit = Preedit::default();
        preedit.set("한😀", None);

        assert_eq!(preedit.slice_utf16(&(0..999)), "한😀");
        assert_eq!(preedit.prefix_utf16(999), "한😀");
        assert_eq!(preedit.prefix_utf16(0), "");
        assert_eq!(preedit.prefix_utf16(1), "한");
        assert_eq!(Preedit::default().slice_utf16(&(0..4)), "");
    }

    // --- composition placement --------------------------------------------

    #[test]
    fn the_composition_slides_left_to_stay_inside_the_grid() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(36.)));
        let geometry = Geometry {
            bounds,
            cell: size(px(8.), px(18.)),
            cols: 12,
            rows: 2,
            cursor: CellPos { line: 1, col: 10 },
        };

        // A narrow composition starts right at the cursor.
        let snug = preedit_origin(&geometry, bounds, px(16.));
        assert_eq!(snug, point(px(80.), px(18.)));

        // A wide one is pulled back so its right edge lands on the margin.
        let wide = preedit_origin(&geometry, bounds, px(40.));
        assert_eq!(wide, point(px(60.), px(18.)));

        // One wider than the grid clamps to the left edge instead of going
        // negative.
        let huge = preedit_origin(&geometry, bounds, px(400.));
        assert_eq!(huge, point(px(0.), px(18.)));
    }

    // --- reading the grid back out of the runs -----------------------------

    /// A run of `cells` columns starting at `start_col`, in the default style.
    fn styled(text: &str, start_col: u16, cells: u16) -> StyledRun {
        StyledRun {
            text: text.to_owned(),
            start_col,
            cells,
            fg: Rgb::new(255, 255, 255),
            bg: Rgb::new(0, 0, 0),
            flags: RunFlags::empty(),
        }
    }

    /// `한글x`, as the model splits it: one run per wide cluster.
    fn wide_line() -> TerminalLine {
        TerminalLine {
            runs: vec![styled("한", 0, 2), styled("글", 2, 2), styled("x", 4, 1)],
        }
    }

    #[test]
    fn char_at_reports_the_base_character_on_a_spacer_column() {
        let line = wide_line();
        assert_eq!(char_at(&line, 0), Some('한'));
        // Column one is the trailing half of the same character.
        assert_eq!(char_at(&line, 1), Some('한'));
        assert_eq!(char_at(&line, 2), Some('글'));
        assert_eq!(char_at(&line, 3), Some('글'));
        assert_eq!(char_at(&line, 4), Some('x'));
        assert_eq!(char_at(&line, 5), None);
    }

    #[test]
    fn char_at_indexes_into_an_ascii_run_and_stops_at_its_end() {
        let line = TerminalLine {
            runs: vec![styled("ab", 0, 2), styled("cd", 5, 2)],
        };
        assert_eq!(char_at(&line, 1), Some('b'));
        assert_eq!(char_at(&line, 6), Some('d'));
        // The gap between the runs holds no character at all.
        assert_eq!(char_at(&line, 2), None);
        assert_eq!(char_at(&line, 4), None);
        assert_eq!(char_at(&line, 7), None);
    }

    #[test]
    fn row_text_keeps_the_columns_of_wide_characters() {
        let line = wide_line();
        assert_eq!(row_text(&line, 0, 4), "한글x");
        // Selecting from the trailing half of the first character starts at the
        // next cluster instead of repeating it.
        assert_eq!(row_text(&line, 1, 4), "글x");
        assert_eq!(row_text(&line, 2, 3), "글");
        assert_eq!(row_text(&line, 4, 4), "x");
    }

    #[test]
    fn row_text_lets_combining_marks_ride_along_with_their_base() {
        // `e` plus a combining acute accent is one column, so `f` sits at
        // column one and the gap padding stays correct.
        let line = TerminalLine {
            runs: vec![styled("e\u{0301}", 0, 1), styled("f", 3, 1)],
        };
        assert_eq!(row_text(&line, 0, 3), "e\u{0301}  f");
        assert_eq!(row_text(&line, 1, 3), "  f");
    }

    // --- PaneCaps: what a pane may do when nobody answers -------------------

    #[test]
    fn pane_caps_default_to_offering_nothing() {
        // The source falls back to this when the workspace is gone, so the
        // default has to be the safe answer rather than merely a tidy one: a
        // menu drawn during teardown greys all three rows instead of promising
        // commands there is no workspace left to run.
        let caps = PaneCaps::default();
        assert!(!caps.split_right);
        assert!(!caps.split_below);
        assert!(!caps.break_out);
    }
}
