//! One followed file, in a pane of its own.
//!
//! A tail pane is a [`TerminalView`] with a strip above it. That is the whole
//! of the type: the session on the other end runs a `tail -F` rather than a
//! shell (see [`Session::new_tail`]), and everything a terminal already does —
//! decoding the output, keeping the scrollback, selecting and copying out of
//! it, drawing the connection overlay and its *Reconnect* button — is exactly
//! what a reader of a log wants and is not reimplemented here.
//!
//! What the strip is for is the one thing the grid cannot say: *which* file.
//! The tab strip says it too, but only as a file name — `access.log` tells two
//! panes apart until the day the user follows `/var/log/nginx/access.log` and
//! `/srv/app/log/access.log` at once. So the pane carries the path, and carries
//! as much of it as will fit: [`abbreviate_path`] shortens directory names to
//! their first letter, from the left, until the line fits the pane, which keeps
//! the end of the path — the part that identifies the file — on screen at every
//! width. The whole path is always one hover away, in the tooltip.
//!
//! And the strip says *whose* file, at its right end and subdued: a dashboard
//! puts panes from several connections in one tab, at which point two panes
//! both reading `/var/log/nginx/access.log` are told apart by nothing but the
//! name of the connection each one reads it over. Subdued, and second in the
//! reading order, because the path is what a reader is looking for and the
//! host is the tiebreak — and absent altogether when the caller has no name to
//! give, so a pane opened on its own looks exactly as it always did.
//!
//! The strip is also the pane's handle. It can be painted a color of the
//! user's choosing — one of a fixed palette, or any `#rrggbb` — which is what
//! makes one pane of a crowded dashboard findable at a glance; and it
//! can be dragged onto another pane of the same tab, where the two trade
//! places. Neither is decided here: a right-click asks the workspace for the
//! menu ([`HeaderMenuRequested`]), and a drop is the workspace's to answer,
//! because only the workspace knows which tab the two panes are in.
//!
//! # Filtering
//!
//! The strip also carries the pane's line filter: a regular expression, the
//! highlight rules' kind, that either keeps only the lines it matches or hides
//! them. [`FILTER_KEY`] or the funnel at the strip's right end opens a bar
//! under it; the filter applies as it is typed, to the scrollback already there
//! as much as to the lines still to come, because the terminal keeps the
//! file's lines and draws them again (see
//! [`TerminalModel::set_line_filter`](rulogman_term::TerminalModel::set_line_filter)).
//! While one is in force the strip says so, so a pane that shows three lines
//! is never mistaken for a quiet log. The filter belongs to the pane: a
//! reconnect keeps it, closing the pane forgets it, and nothing is saved.
//!
//! # Why the width is a frame old
//!
//! Whether a candidate fits can only be answered by the text shaper, and the
//! shaper needs a [`Window`]; how much room there is to fit into can only be
//! answered by the layout, which has not run when [`Render::render`] is called.
//! The two never meet in one pass, so the pane measures its own header with a
//! [`canvas`] probe and abbreviates the *next* frame against what it found —
//! and asks for that frame itself, but only when the width actually changed, so
//! a resize settles in one extra frame and a steady pane repaints no more often
//! than the log gives it reason to.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Font,
    Hsla, KeyBinding, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription,
    TextRun, Window, actions, canvas, div, prelude::*, px, rgb,
};
use regex::Regex;
use rugpui::{
    Button, ButtonVariant, Checkbox, Segmented, TextInput, TextSize, Theme, parse_hex, theme,
    tooltip_label,
};
use rulogman_term::LinePredicate;

use crate::highlight::Highlighter;
use crate::i18n::{input_menu_labels, ts};
use crate::icons;
use crate::session::Session;
use crate::terminal_view::{
    EditConnectionRequested, PaneCapsSource, PaneFocused, ReconnectRequested, TerminalView,
};

actions!(
    rulogman_tail,
    [
        /// Open the line filter bar, or put the keyboard in the open one.
        ToggleFilter,
        /// Close the line filter bar, keeping the filter it set.
        CloseFilter,
    ]
);

/// Key context of a followed-file pane.
const KEY_CONTEXT: &str = "TailView";

/// Key context of the filter bar, where `Escape` closes it.
const FILTER_CONTEXT: &str = "TailFilter";

/// The chord that opens the filter bar: `G` for grep, shifted off macOS for the
/// reason every terminal chord is, so a shell would have lost nothing to it.
const FILTER_KEY: &str = if cfg!(target_os = "macos") {
    "cmd-shift-g"
} else {
    "ctrl-shift-g"
};

/// Name of [`FILTER_KEY`] as tooltips and the shortcut sheet print it.
pub(crate) const FILTER_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+G"
} else {
    "Ctrl+Shift+G"
};

/// Width of the pattern field in the filter bar.
const FILTER_INPUT_WIDTH: f32 = 220.;

/// What a filter does with the lines its pattern matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    /// Show those lines and no others.
    Show,
    /// Show every line but those.
    Hide,
}

/// A pane's line filter, as the user set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFilter {
    /// The regular expression; empty means no filter.
    pub pattern: String,
    /// Whether matching lines are kept or hidden.
    pub mode: FilterMode,
    /// Match without regard to case — on by default, as a highlight rule is,
    /// and for the same reason: `ERROR`, `Error` and `error` are one word to a
    /// reader.
    pub ignore_case: bool,
}

impl Default for LineFilter {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            mode: FilterMode::Show,
            ignore_case: true,
        }
    }
}

impl LineFilter {
    /// Whether the filter hides anything at all.
    pub fn is_active(&self) -> bool {
        !self.pattern.is_empty()
    }

    /// The predicate the terminal is to judge lines by — `None` for no filter
    /// — or the reason the pattern is not a regular expression.
    ///
    /// Compiled the way a highlight rule is, `(?i)` in front for a filter that
    /// ignores case, so one pattern means the same thing in both places.
    pub fn compile(&self) -> Result<Option<LinePredicate>, regex::Error> {
        if !self.is_active() {
            return Ok(None);
        }
        let source = if self.ignore_case {
            format!("(?i){}", self.pattern)
        } else {
            self.pattern.clone()
        };
        let regex = Regex::new(&source)?;
        let keep = self.mode == FilterMode::Show;
        Ok(Some(Rc::new(move |line: &str| {
            regex.is_match(line) == keep
        })))
    }
}

/// The filter bar, while it is open.
struct FilterBar {
    /// The pattern field.
    input: Entity<TextInput>,
    /// What the field held when it was last read, so that a notification for
    /// anything but an edit — the caret, the focus — redraws nothing.
    typed: String,
    /// Applies the pattern as it is typed.
    _changed: Subscription,
}

/// Height of the strip above the grid.
///
/// The same height an open file's header has, and deliberately: the two are the
/// same piece of furniture — a line of chrome naming what the pane below is
/// showing — and a tab strip whose panes disagreed by two pixels about where
/// their content starts would look like a mistake rather than like a choice.
const HEADER_HEIGHT: f32 = 26.;

/// Size of the path text in the strip.
///
/// Chrome-sized rather than terminal-sized: the strip belongs to the
/// application, and the font it is measured in has to be the font it is drawn
/// in, which is why this is a constant both [`TailView::render`] and its probe
/// read rather than something the style sets on its own.
const HEADER_TEXT_SIZE: f32 = 11.;

/// The palette a header can be painted from: the name each color is stored
/// under, the color itself, and the translation key it is listed by.
///
/// Fixed colors rather than the slots of the terminal's color scheme, so a
/// header looks the same whatever scheme the session behind it runs: it is
/// chrome, like the strip it replaces, and a scheme's idea of "blue" is not
/// the application's. Mid-tone, so that either black or white text reads on
/// every one of them.
pub const HEADER_COLORS: [(&str, u32, &str); 9] = [
    ("red", 0xdc2626, "tab.color.red"),
    ("orange", 0xea580c, "tab.color.orange"),
    ("yellow", 0xca8a04, "tab.color.yellow"),
    ("green", 0x16a34a, "tab.color.green"),
    ("teal", 0x0d9488, "tab.color.teal"),
    ("blue", 0x2563eb, "tab.color.blue"),
    ("purple", 0x7c3aed, "tab.color.purple"),
    ("pink", 0xdb2777, "tab.color.pink"),
    ("gray", 0x6b7280, "tab.color.gray"),
];

/// What the stored header color `color` paints: a palette name, or a
/// `#rrggbb` of the user's own. `None` for anything else, which the header
/// draws as no color at all.
///
/// The one resolution, shared by the header itself and by every list that
/// shows the colors on offer, so a swatch is exactly the color the header
/// comes out in.
pub fn header_fill(color: &str) -> Option<Hsla> {
    if let Some(index) = header_preset(color) {
        return Some(rgb(HEADER_COLORS[index].1).into());
    }
    let color = color.trim();
    color.starts_with('#').then(|| parse_hex(color)).flatten()
}

/// The palette entry `color` names, if it is one.
pub fn header_preset(color: &str) -> Option<usize> {
    let color = color.trim();
    HEADER_COLORS
        .iter()
        .position(|(name, ..)| color.eq_ignore_ascii_case(name))
}

/// The `#rrggbb` the stored `color` paints, for a field that edits it as a
/// color of the user's own — a palette name included, so that switching to a
/// custom color starts from the one already on the header.
pub fn header_hex(color: &str) -> Option<String> {
    let fill = header_fill(color)?.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(fill.r),
        channel(fill.g),
        channel(fill.b)
    ))
}

/// The palette as swatches, for a color picker's preset row.
pub fn header_swatches() -> Vec<Hsla> {
    HEADER_COLORS
        .iter()
        .map(|(_, value, _)| rgb(*value).into())
        .collect()
}

/// The text color that reads on a header filled `fill`.
fn header_text(fill: Hsla) -> Hsla {
    let fill = fill.to_rgb();
    // Rec. 601 luma, which is all a choice between two text colors needs.
    let luma = 0.299 * fill.r + 0.587 * fill.g + 0.114 * fill.b;
    if luma > 0.55 {
        gpui::black()
    } else {
        gpui::white()
    }
}

/// A right-click on the header, asking for its menu at `position`, in window
/// coordinates.
pub struct HeaderMenuRequested {
    /// Where the pointer was when the button went down.
    pub position: Point<Pixels>,
}

/// What a header carries while it is dragged: the pane it belongs to, by the
/// entity the workspace knows it by.
///
/// Dropped onto another pane of the same tab, it asks the two to trade places;
/// see `Workspace::swap_panes`. Anywhere else it is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DraggedTail {
    /// The [`TailView`] whose header was picked up.
    pub view: EntityId,
}

/// The chip that follows the pointer while a header is dragged.
///
/// Drawn the way the tab strip draws a dragged tab, since it is the same
/// gesture on a different handle.
struct TailGhost {
    /// The file name the dragged pane is following.
    name: SharedString,
    /// Palette the ghost is drawn with.
    theme: Theme,
}

impl Render for TailGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(4.))
            .bg(self.theme.surface_active)
            .border_1()
            .border_color(self.theme.accent)
            .rounded_md()
            .text_size(rugpui::scaled(HEADER_TEXT_SIZE, cx))
            .text_color(self.theme.text)
            .child(self.name.clone())
    }
}

/// A pane following one file on a remote host.
pub struct TailView {
    /// The grid the file scrolls through, and everything that goes with it.
    terminal: Entity<TerminalView>,
    /// The session behind that grid.
    ///
    /// Held here as well as inside the terminal view, so that the workspace can
    /// ask a leaf which session it is without reading two entities to find out;
    /// it is a handle into the entity map, so the second copy costs a count.
    session: Entity<Session>,
    /// The file being followed, as the profile spells it.
    ///
    /// The path is fixed for the life of the pane — a followed file is what the
    /// session *is* — so it is kept rather than re-read from the session on
    /// every frame.
    path: SharedString,
    /// Name of the connection the file is being read over, or empty when the
    /// caller had none to give.
    ///
    /// Kept here rather than read off the session for the reason [`Self::path`]
    /// is: it cannot change while the pane lives, and the header is drawn on
    /// every frame the log gives it.
    connection: SharedString,
    /// The color the header is painted in, as stored, or `None` for the
    /// theme's own strip. See [`header_fill`] for the spellings.
    color: Option<String>,
    /// How wide the path had to fit when it was last drawn, or `None` until the
    /// probe has reported once. See the module header for why this is a frame
    /// behind.
    path_width: Option<Pixels>,
    /// The filter in force — the last one that compiled.
    filter: LineFilter,
    /// The filter bar, while it is open.
    filter_bar: Option<FilterBar>,
    /// Whether what the bar holds failed to compile, leaving [`Self::filter`]
    /// as it was.
    filter_error: bool,
    /// Passes the grid's *this pane was clicked* on to the workspace.
    _clicked: Subscription,
    /// Passes the grid's *Reconnect* on to the workspace.
    _reconnect: Subscription,
    /// Re-emits the grid's *Edit connection…*, for the same reason.
    _edit: Subscription,
}

impl TailView {
    /// Wraps `terminal` — which must be showing a session built by
    /// [`Session::new_tail`] — in a header naming `path`, and `connection`
    /// after it.
    ///
    /// `connection` is the profile's name, and may be empty: a pane that is the
    /// only one in its tab has nothing to be told apart from, and the strip
    /// then carries the path alone.
    ///
    /// The two events the workspace wires a pane up with are re-emitted rather
    /// than listened for on the inner view, because the workspace knows this
    /// pane by *this* entity: a focus reported by the grid would name a surface
    /// no leaf answers to. Re-emitting keeps [`crate::workspace::Workspace::new_tail_pane`]
    /// a copy of its terminal counterpart, event for event.
    pub fn new(
        terminal: Entity<TerminalView>,
        session: Entity<Session>,
        path: String,
        connection: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let clicked = cx.subscribe(&terminal, |_this, _view, _: &PaneFocused, cx| {
            cx.emit(PaneFocused);
        });
        let reconnect = cx.subscribe(&terminal, |_this, _view, _: &ReconnectRequested, cx| {
            cx.emit(ReconnectRequested);
        });
        let edit = cx.subscribe(
            &terminal,
            |_this, _view, _: &EditConnectionRequested, cx| {
                cx.emit(EditConnectionRequested);
            },
        );

        // Before the first frame, so the log the session has already begun
        // pouring in is colored from its first line rather than from the
        // moment the settings are next applied.
        let highlighter = compile_highlights(&session, cx);
        terminal.update(cx, |view, cx| view.set_highlights(highlighter, cx));

        Self {
            terminal,
            session,
            path: SharedString::from(path),
            connection,
            color: None,
            path_width: None,
            filter: LineFilter::default(),
            filter_bar: None,
            filter_error: false,
            _clicked: clicked,
            _reconnect: reconnect,
            _edit: edit,
        }
    }

    /// The same pane, with its header painted `color`; see [`Self::set_color`].
    pub fn with_color(mut self, color: Option<String>) -> Self {
        self.color = color;
        self
    }

    /// The grid under the header.
    #[cfg(test)]
    pub(crate) fn terminal_for_test(&self) -> Entity<TerminalView> {
        self.terminal.clone()
    }

    /// Binds the filter chord and the bar's `Escape`.
    pub fn init(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new(FILTER_KEY, ToggleFilter, Some(KEY_CONTEXT)),
            KeyBinding::new("escape", CloseFilter, Some(FILTER_CONTEXT)),
        ]);
    }

    /// The session this pane is following a file over.
    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// The filter bar's field, while the bar is open.
    #[cfg(test)]
    pub(crate) fn filter_input_for_test(&self) -> Option<Entity<TextInput>> {
        self.filter_bar.as_ref().map(|bar| bar.input.clone())
    }

    /// The filter in force, and whether the bar holds a pattern that failed.
    #[cfg(test)]
    pub(crate) fn filter_for_test(&self) -> (LineFilter, bool) {
        (self.filter.clone(), self.filter_error)
    }

    /// Switches the filter between keeping and hiding its matches.
    #[cfg(test)]
    pub(crate) fn set_filter_mode_for_test(&mut self, mode: FilterMode, cx: &mut Context<Self>) {
        self.set_filter_mode(mode, cx);
    }

    /// Puts `filter` in force, unless its pattern does not compile — then the
    /// filter in force stays, and the bar says why. Returns whether it took.
    pub fn set_filter(&mut self, filter: LineFilter, cx: &mut Context<Self>) -> bool {
        match filter.compile() {
            Ok(predicate) => {
                self.filter_error = false;
                if filter != self.filter {
                    self.filter = filter;
                    self.terminal
                        .update(cx, |view, cx| view.set_line_filter(predicate, cx));
                }
                cx.notify();
                true
            }
            Err(error) => {
                log::debug!("not filtering by {:?}: {error}", filter.pattern);
                self.filter_error = true;
                cx.notify();
                false
            }
        }
    }

    /// The filter chord, or the strip's funnel: opens the bar on the pattern in
    /// force, or puts the keyboard back in the bar already open.
    fn toggle_filter(&mut self, _: &ToggleFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.open_filter(window, cx);
    }

    /// Opens the filter bar, or focuses it.
    pub fn open_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_bar.is_none() {
            let pattern = self.filter.pattern.clone();
            let input = cx.new(|cx| {
                let mut input = TextInput::new(cx)
                    .context_menu(input_menu_labels)
                    .placeholder(ts!("tail_filter.placeholder"));
                input.set_content(pattern, cx);
                input
            });
            let changed = cx.observe(&input, |view, input, cx| {
                let pattern = input.read(cx).content().to_owned();
                let Some(bar) = &mut view.filter_bar else {
                    return;
                };
                if bar.typed == pattern {
                    return;
                }
                bar.typed.clone_from(&pattern);
                let filter = LineFilter {
                    pattern,
                    ..view.filter.clone()
                };
                view.set_filter(filter, cx);
            });
            self.filter_bar = Some(FilterBar {
                input,
                typed: self.filter.pattern.clone(),
                _changed: changed,
            });
        }
        if let Some(bar) = &self.filter_bar {
            let handle = bar.input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// `Escape` in the bar, or its close button: the bar goes, the filter
    /// stays, and the keyboard goes back to the grid.
    fn close_filter(&mut self, _: &CloseFilter, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_bar.take().is_some() {
            self.filter_error = false;
            let handle = self.terminal.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            cx.notify();
        }
    }

    /// Switches between keeping and hiding the matches.
    fn set_filter_mode(&mut self, mode: FilterMode, cx: &mut Context<Self>) {
        let filter = LineFilter {
            mode,
            ..self.filter.clone()
        };
        self.set_filter(filter, cx);
    }

    /// Turns case-insensitive matching on or off.
    fn set_filter_ignore_case(&mut self, ignore_case: bool, cx: &mut Context<Self>) {
        let filter = LineFilter {
            ignore_case,
            ..self.filter.clone()
        };
        self.set_filter(filter, cx);
    }

    /// Empties the pattern, which shows every line again.
    fn clear_filter(&mut self, cx: &mut Context<Self>) {
        match &self.filter_bar {
            // Through the field, so the field and the filter cannot disagree:
            // its observer applies the empty pattern.
            Some(bar) => bar.input.update(cx, |input, cx| input.clear(cx)),
            None => {
                self.set_filter(
                    LineFilter {
                        pattern: String::new(),
                        ..self.filter.clone()
                    },
                    cx,
                );
            }
        }
    }

    /// The bar under the strip, while it is open.
    fn render_filter_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.filter_bar.as_ref()?;
        let theme = theme(cx);
        let this = cx.entity();
        let mode = Segmented::new("tail-filter-mode")
            .options([
                ("show", ts!("tail_filter.show")),
                ("hide", ts!("tail_filter.hide")),
            ])
            .selected(match self.filter.mode {
                FilterMode::Show => 0,
                FilterMode::Hide => 1,
            })
            .on_select({
                let this = this.clone();
                move |picked, _window, cx| {
                    let mode = if picked == 0 {
                        FilterMode::Show
                    } else {
                        FilterMode::Hide
                    };
                    this.update(cx, |view, cx| view.set_filter_mode(mode, cx));
                }
            });
        let ignore_case = Checkbox::new(
            "tail-filter-ignore-case",
            ts!("settings.highlights.ignore_case"),
        )
        .checked(self.filter.ignore_case)
        .on_toggle({
            let this = this.clone();
            move |checked, _window, cx| {
                this.update(cx, |view, cx| view.set_filter_ignore_case(checked, cx));
            }
        });
        let clear = Button::new("tail-filter-clear", ts!("tail_filter.clear"))
            .variant(ButtonVariant::Ghost)
            .compact()
            .disabled(!self.filter.is_active() && !self.filter_error)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| this.update(cx, |view, cx| view.clear_filter(cx))
            });
        let close = Button::new("tail-filter-close", "\u{00d7}")
            .variant(ButtonVariant::Ghost)
            .compact()
            .on_click({
                let this = this.clone();
                move |_, window, cx| {
                    this.update(cx, |view, cx| view.close_filter(&CloseFilter, window, cx));
                }
            });

        Some(
            div()
                .key_context(FILTER_CONTEXT)
                .on_action(cx.listener(Self::close_filter))
                .flex()
                .flex_row()
                .flex_none()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(4.))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .text_color(theme.text)
                .text_size(TextSize::Xs.size(cx))
                .child(div().w(px(FILTER_INPUT_WIDTH)).child(bar.input.clone()))
                .child(mode)
                .child(ignore_case)
                .children(self.filter_error.then(|| {
                    div()
                        .text_color(theme.danger)
                        .child(ts!("tail_filter.bad_pattern"))
                }))
                .child(div().flex_1())
                .child(clear)
                .child(close)
                .into_any_element(),
        )
    }

    /// The strip's right end: what the filter is doing, when it is doing
    /// anything, and the funnel that opens the bar.
    fn render_filter_button(
        &self,
        muted: Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let active = self.filter.is_active();
        let color = if active { theme.accent } else { muted };
        let badge = active.then(|| {
            let pattern = self.filter.pattern.as_str();
            let words = match self.filter.mode {
                FilterMode::Show => ts!("tail_filter.badge_show", pattern = pattern),
                FilterMode::Hide => ts!("tail_filter.badge_hide", pattern = pattern),
            };
            div()
                .min_w_0()
                .max_w(px(180.))
                .truncate()
                .pl(px(8.))
                .child(words)
        });
        div()
            .id("tail-filter")
            .flex()
            .flex_row()
            .flex_shrink_0()
            .min_w_0()
            .items_center()
            .text_color(color)
            .cursor_pointer()
            .tooltip(tooltip_label(ts!(
                "tail_filter.button",
                shortcut = FILTER_SHORTCUT
            )))
            // The press belongs to the funnel, not to the strip's drag.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|view, _, window, cx| {
                if view.filter_bar.is_some() {
                    view.close_filter(&CloseFilter, window, cx);
                } else {
                    view.open_filter(window, cx);
                }
            }))
            .children(badge)
            .child(
                gpui::svg()
                    .flex_none()
                    .ml(px(6.))
                    .size(px(12.))
                    .text_color(color)
                    .path(icons::FILTER),
            )
    }

    /// The color the header is painted in, as stored.
    pub fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }

    /// Paints the header `color` — a spelling [`header_fill`] reads — or gives
    /// it back the theme's strip with `None`.
    pub fn set_color(&mut self, color: Option<String>, cx: &mut Context<Self>) {
        if self.color == color {
            return;
        }
        self.color = color;
        cx.notify();
    }

    /// The header's fill and the text color that reads on it, or `None` when
    /// the header has no color of its own — or one this build cannot read,
    /// which is drawn the same way.
    fn header_colors(&self) -> Option<(Hsla, Hsla)> {
        let fill = header_fill(self.color.as_deref()?)?;
        Some((fill, header_text(fill)))
    }

    /// Re-reads the highlight rules and hands the grid the compiled result.
    ///
    /// Called when the settings are applied, which is the one thing that can
    /// change a live pane's rules: the global list is read fresh every time
    /// [`Session::highlight_rules`] is asked, so a rule added in the dialog
    /// reaches every pane on screen at once. The per-file list is read off the
    /// profile copy the session was opened with and so does *not* change under
    /// a running pane; see that method for why.
    pub fn refresh_highlights(&mut self, cx: &mut Context<Self>) {
        let highlighter = compile_highlights(&self.session, cx);
        self.terminal
            .update(cx, |view, cx| view.set_highlights(highlighter, cx));
    }

    /// Rebinds the grid to the window this pane has been moved into.
    ///
    /// Pure delegation: everything window-bound in a tail pane belongs to the
    /// terminal underneath it — see [`TerminalView::rebind`] for what and why —
    /// and the header holds nothing but a path and a width.
    pub fn rebind(&mut self, caps: PaneCapsSource, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal
            .update(cx, |view, cx| view.rebind(caps, window, cx));
    }

    /// Records the width the header's path slot was given, and asks for a
    /// repaint if it has changed.
    ///
    /// Only on a change, which is what keeps this from being a loop: the frame
    /// this notification asks for measures the same width and stops here.
    fn measured(&mut self, width: Pixels, cx: &mut Context<Self>) {
        if self.path_width == Some(width) {
            return;
        }
        self.path_width = Some(width);
        cx.notify();
    }
}

impl EventEmitter<PaneFocused> for TailView {}

impl EventEmitter<ReconnectRequested> for TailView {}

impl EventEmitter<EditConnectionRequested> for TailView {}

impl EventEmitter<HeaderMenuRequested> for TailView {}

impl Focusable for TailView {
    /// The grid's own handle, so that the keyboard lands where the selection,
    /// the scrollback and the copy shortcut already are. The header is chrome
    /// and takes no focus of its own.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal.read(cx).focus_handle(cx)
    }
}

impl Render for TailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let full = self.path.clone();
        // The font the strip is drawn in, taken off the style stack rather than
        // named here: the application's UI font is the settings' business, and
        // a measurement made in another one would fit text that then does not.
        let font = window.text_style().font();
        let font_size = rugpui::scaled(HEADER_TEXT_SIZE, cx);
        let shown = match self.path_width {
            Some(width) => SharedString::from(abbreviate_path(&full, |candidate| {
                measure(candidate, &font, font_size, window) <= width
            })),
            // The opening frame, with nothing measured yet: the whole path,
            // truncated by the style if it is too long. The probe reports
            // during that very frame's prepaint, so the abbreviated form is on
            // screen by the next one.
            None => full.clone(),
        };

        let this = cx.entity();
        // Sized and positioned to be exactly the slot it measures, and painting
        // nothing: it is here for its bounds alone.
        let probe = canvas(
            move |bounds: Bounds<Pixels>, _window, cx| {
                this.update(cx, |view, cx| view.measured(bounds.size.width, cx));
            },
            |_bounds, (), _window, _cx| {},
        )
        .absolute()
        .size_full();

        let (fill, text, muted) = match self.header_colors() {
            Some((fill, text)) => (fill, text, text.opacity(0.75)),
            None => (theme.surface, theme.text, theme.text_muted),
        };
        let dragged = DraggedTail {
            view: cx.entity_id(),
        };
        let name = SharedString::from(crate::session::remote_file_name(&full).to_owned());

        let header = div()
            .id("tail-header")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(rugpui::scaled(HEADER_HEIGHT, cx))
            .px(px(8.))
            .bg(fill)
            .border_b_1()
            .border_color(theme.border)
            .text_size(font_size)
            .text_color(text)
            // Says the strip can be picked up — the grip on its left edge
            // shows it too, and the path's tooltip says what for.
            .cursor_grab()
            // The handle a pane is moved by: dropped on another pane of the
            // tab, the two trade places. The workspace answers the drop.
            .on_drag(dragged, move |_, _, _, cx| {
                // The function, not the palette this frame was drawn with: the
                // local `theme` above shadows it.
                let theme = rugpui::theme(cx);
                let name = name.clone();
                cx.new(|_| TailGhost { name, theme })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_view, event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    cx.emit(HeaderMenuRequested {
                        position: event.position,
                    });
                }),
            )
            // The grip: six dots, the mark every toolkit draws on something
            // that can be dragged, in the strip's muted tone.
            .child(
                div()
                    .flex_none()
                    .pr(px(6.))
                    .text_color(muted)
                    .child("\u{283f}"),
            )
            .child(
                div()
                    .id("tail-path")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    // The one place the path is never shortened, whatever the
                    // pane's width: a tooltip is one line by construction and a
                    // path is one line by nature, so the two fit each other —
                    // with what the strip itself answers to after it.
                    .tooltip(tooltip_label(ts!(
                        "tab.header_tip",
                        path = full.to_string()
                    )))
                    .child(div().min_w_0().truncate().child(shown))
                    .child(probe),
            )
            .child(self.render_filter_button(muted, cx))
            // Never abbreviated and never given up: it is short by nature — a
            // profile name, not a path — and it is the whole of what the right
            // end of the strip is for. `flex_none` is what makes the path the
            // side that yields when the pane is narrow.
            .children((!self.connection.is_empty()).then(|| {
                div()
                    .flex_none()
                    .pl(px(8.))
                    .text_color(muted)
                    .child(self.connection.clone())
            }));

        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::toggle_filter))
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            // A press anywhere on the pane — the header included — belongs to
            // the grid, which is where every command this pane answers to
            // lives. Propagation is left alone, so a press on the grid itself
            // still starts a selection there.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _: &MouseDownEvent, window, cx| {
                    let handle = view.terminal.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                    cx.emit(PaneFocused);
                }),
            )
            .child(header)
            .children(self.render_filter_bar(cx))
            .child(div().flex_1().min_h_0().child(self.terminal.clone()))
    }
}

/// The rules `session` is to be colored by, compiled, or `None` when nothing
/// would ever be recolored.
///
/// Compiled per pane rather than shared between the panes running the same
/// list: a tab of tails is a handful of panes and a rule list a handful of
/// regexes, so a cache keyed by rule list would cost more to keep honest — a
/// pane's rules change when the settings do — than the compiles it saves.
///
/// `None` for all three of the ways there can be nothing to do: a session that
/// is not following a file, an empty rule list (which is how a user switches
/// highlighting off), and a list whose every rule was disabled or unparseable.
/// The grid checks for exactly that `None` and skips the whole pass, so the
/// three arrive at the same place by design.
fn compile_highlights(session: &Entity<Session>, cx: &App) -> Option<Rc<Highlighter>> {
    let rules = session.read(cx).highlight_rules(cx)?;
    let highlighter = Highlighter::compile(&rules);
    (!highlighter.is_empty()).then(|| Rc::new(highlighter))
}

/// Width `text` occupies in `font` at `font_size`.
///
/// The shaper rather than a character count, because the strip is drawn in a
/// proportional font and a path is full of the narrowest glyphs there are —
/// `/`, `.`, `l`, `i` — which a count would charge the same as an `m`.
fn measure(text: &str, font: &Font, font_size: Pixels, window: &mut Window) -> Pixels {
    if text.is_empty() {
        return px(0.);
    }
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), font_size, &[run], None)
        .width
}

/// The most of `path` that `fits` will take, shortening directory names from
/// the left.
///
/// `/var/log/nginx/access.log` becomes `/v/log/nginx/access.log`, then
/// `/v/l/nginx/access.log`, then `/v/l/n/access.log` — one component per step,
/// and the first candidate that fits is the answer. Leftmost first because the
/// end of a path is the part that identifies the file: `/v/l/n/access.log` is
/// still recognizable, `/var/log/nginx/a` is not. The last component is never
/// touched at all, for the same reason.
///
/// A path that already fits comes back untouched, and one that does not fit
/// even fully abbreviated comes back fully abbreviated — the caller draws it
/// with an ellipsis, which is the honest answer when there is genuinely no room.
///
/// `fits` rather than a width, so that the rule can be asserted against a
/// counted-characters stand-in without a window, a font or a shaper; see the
/// tests below. It is called at most once per directory component, which
/// matters because each call shapes a line.
fn abbreviate_path<F>(path: &str, mut fits: F) -> String
where
    F: FnMut(&str) -> bool,
{
    if fits(path) {
        return path.to_owned();
    }

    let mut parts: Vec<&str> = path.split('/').collect();
    // Everything but the last component, which is the file name. A path with no
    // separator in it is one component, so this is zero and nothing is done to
    // it — there is no directory to give up.
    let directories = parts.len().saturating_sub(1);
    for index in 0..directories {
        let Some(initial) = first_char(parts[index]) else {
            // An empty component: the leading one of an absolute path, or a
            // doubled separator. Neither has a letter to keep, and neither is
            // any shorter for being visited.
            continue;
        };
        if initial.len() == parts[index].len() {
            // Already one character. Shortening it to itself would cost a
            // measurement and change nothing.
            continue;
        }
        parts[index] = initial;
        let candidate = parts.join("/");
        if fits(&candidate) {
            return candidate;
        }
    }

    parts.join("/")
}

/// The first character of `component`, as a slice of it, or `None` when there
/// is none.
///
/// A `char` and not a byte, so a path whose directories are spelled in Korean
/// or Greek is abbreviated to a letter rather than to the first third of one.
fn first_char(component: &str) -> Option<&str> {
    let first = component.chars().next()?;
    Some(&component[..first.len_utf8()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path with room to spare on every side.
    const LOG: &str = "/var/log/nginx/access.log";

    /// A stand-in for the shaper: every character one unit wide.
    ///
    /// The rule under test is about which components are given up and in what
    /// order, which is the same rule at any font; counting characters is what
    /// lets the expectations below be read as the strings they are.
    fn narrower_than(limit: usize) -> impl FnMut(&str) -> bool {
        move |candidate: &str| candidate.chars().count() <= limit
    }

    #[test]
    fn a_header_color_is_a_palette_name_or_a_hex_of_the_users_own() {
        assert_eq!(header_fill("blue"), Some(rgb(0x2563eb).into()));
        assert_eq!(header_fill(" Blue "), header_fill("blue"));
        assert_eq!(header_fill("#102030"), parse_hex("#102030"));
        // Neither a palette name nor a `#` color: drawn as no color.
        assert_eq!(header_fill("102030"), None);
        assert_eq!(header_fill("bright_red"), None);
        assert_eq!(header_preset("PINK"), Some(7));
        assert_eq!(header_preset("#db2777"), None);
    }

    #[test]
    fn a_header_color_is_spelled_as_hex_for_the_picker() {
        assert_eq!(header_hex("blue").as_deref(), Some("#2563eb"));
        assert_eq!(header_hex("#A1B2C3").as_deref(), Some("#a1b2c3"));
        assert_eq!(header_hex("nonsense"), None);
    }

    #[test]
    fn header_text_is_dark_on_light_fills_and_light_on_dark_ones() {
        assert_eq!(header_text(rgb(0xfde68a).into()), gpui::black());
        assert_eq!(header_text(rgb(0x1e3a8a).into()), gpui::white());
    }

    fn passes(filter: &LineFilter, line: &str) -> bool {
        match filter.compile().expect("compiles") {
            Some(predicate) => predicate(line),
            None => true,
        }
    }

    #[test]
    fn a_filter_keeps_or_hides_its_matches_ignoring_case_by_default() {
        let show = LineFilter {
            pattern: "error|warn".to_owned(),
            ..LineFilter::default()
        };
        assert!(passes(&show, "2026-10-31 ERROR disk full"));
        assert!(!passes(&show, "2026-10-31 INFO started"));

        let hide = LineFilter {
            mode: FilterMode::Hide,
            ..show.clone()
        };
        assert!(!passes(&hide, "WARN low memory"));
        assert!(passes(&hide, "INFO started"));

        let exact = LineFilter {
            ignore_case: false,
            ..show
        };
        assert!(!passes(&exact, "ERROR disk full"));
        assert!(passes(&exact, "an error"));
    }

    #[test]
    fn an_empty_filter_is_no_filter_and_a_broken_one_says_so() {
        assert!(LineFilter::default().compile().expect("compiles").is_none());
        let broken = LineFilter {
            pattern: "(".to_owned(),
            ..LineFilter::default()
        };
        assert!(broken.compile().is_err());
    }

    #[test]
    fn a_path_that_fits_is_left_exactly_as_it_is() {
        assert_eq!(abbreviate_path(LOG, |_| true), LOG);
    }

    #[test]
    fn components_are_given_up_one_at_a_time_from_the_left() {
        // Just too narrow for the whole path (25), wide enough for the first
        // component's initial (23).
        assert_eq!(
            abbreviate_path(LOG, narrower_than(23)),
            "/v/log/nginx/access.log"
        );
        // One more step, and no further: `nginx` is still whole.
        assert_eq!(
            abbreviate_path(LOG, narrower_than(21)),
            "/v/l/nginx/access.log"
        );
    }

    #[test]
    fn a_pane_with_no_room_gets_every_directory_abbreviated() {
        assert_eq!(
            abbreviate_path(LOG, narrower_than(20)),
            "/v/l/n/access.log",
            "the last candidate that fits was not the shortest one"
        );
        // And when even that does not fit, the shortest form is what comes
        // back: the caller draws it with an ellipsis rather than being handed
        // a path it has already been told is too wide.
        assert_eq!(abbreviate_path(LOG, |_| false), "/v/l/n/access.log");
    }

    #[test]
    fn the_file_name_is_never_abbreviated() {
        // Not even when it is the only component there is, and not even when
        // nothing fits: a pane naming `a` instead of `access.log` names
        // nothing.
        assert_eq!(abbreviate_path("access.log", |_| false), "access.log");
        assert_eq!(abbreviate_path("/access.log", |_| false), "/access.log");
        assert!(
            abbreviate_path(LOG, |_| false).ends_with("/access.log"),
            "the file name was shortened away"
        );
    }

    #[test]
    fn a_component_spelled_in_another_script_keeps_its_first_letter() {
        // The bytes of `데` are three; the character is one. Slicing by byte
        // would panic here, and slicing by the wrong count would produce a
        // component that is not text.
        assert_eq!(
            abbreviate_path("/데이터/로그/서버.log", |_| false),
            "/데/로/서버.log"
        );
    }

    #[test]
    fn nothing_is_spent_on_a_component_that_cannot_get_shorter() {
        // A doubled separator and a one-letter directory: neither has anything
        // to give, and the run of candidates must not stall on them.
        assert_eq!(abbreviate_path("//a/opt/x.log", |_| false), "//a/o/x.log");
    }
}
