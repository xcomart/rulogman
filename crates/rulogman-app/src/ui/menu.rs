//! Dropdown menus: the toolbar application menu, and the menu a right-click
//! opens at the pointer.
//!
//! Windows and Linux get no native menu bar from gpui — [`gpui::App::set_menus`]
//! only builds one on macOS — so the shell draws its own. [`MenuButton`] is that
//! drawing: a compact glyph button which, while open, paints a list of
//! [`MenuEntry`] rows over the rest of the window. [`ContextMenu`] paints the
//! same list, without a trigger, wherever the caller says.
//!
//! Like every other widget here both are stateless: the parent view owns the
//! open flag — and, for a context menu, the position that goes with it — passes
//! it in on every render, and closes the menu from
//! [`MenuButton::on_open_change`] or [`ContextMenu::on_dismiss`].

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, ElementId, Pixels, Point, SharedString, Size, Window, anchored,
    deferred, div, point, prelude::*, px, svg,
};

use super::theme::{Theme, theme};
use super::tooltip::tooltip_label;

/// Edge length of the trigger button.
const TRIGGER_SIZE: f32 = 28.;

/// Edge length of the icon inside the trigger, when it carries one.
///
/// Matches the toolbar's other icon buttons rather than the glyph it may stand
/// in for: a vector drawn at its own size sits in the row at the same weight as
/// the panel toggle beside it, which a font glyph scaled to the same box would
/// not.
const TRIGGER_ICON: f32 = 16.;

/// Style group of a trigger, so hovering the button recolours the icon in it.
///
/// Shared by every [`MenuButton`] rather than made unique per button: a
/// `group_hover` resolves against the nearest ancestor carrying the name, so
/// two triggers side by side each answer to their own.
const TRIGGER_GROUP: &str = "menu-trigger";

/// Vertical distance from the top of the trigger to the top of the dropdown, so
/// that the panel clears the button it hangs from.
const DROP_OFFSET: f32 = TRIGGER_SIZE + 4.;

/// Width of a dropdown panel.
///
/// Wide enough for the longest row either menu has — the pane commands name the
/// thing they act on ("Split right of current tab") and carry a shortcut hint —
/// with room for a translation of it, since a row neither wraps nor ellipsises.
const PANEL_WIDTH: f32 = 280.;

/// Distance the dropdown keeps from the window edges when it would overflow.
const WINDOW_MARGIN: f32 = 6.;

/// Draw order of the click-catching backdrop, relative to other deferred draws.
const BACKDROP_PRIORITY: usize = 1;

/// Draw order of the dropdown panel; above [`BACKDROP_PRIORITY`] so that the
/// backdrop never eats clicks meant for a menu row.
const PANEL_PRIORITY: usize = 2;

/// Callback fired when a menu row is activated.
type ActivateHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Callback fired when an open menu wants to close itself.
type DismissHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Callback fired when the menu wants to open or close itself.
type OpenChangeHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// One row of a [`MenuButton`] or [`ContextMenu`] dropdown.
///
/// A row is either a command — a label, an optional shortcut hint and a
/// callback, run unless [`MenuEntry::enabled`] says otherwise — or a horizontal
/// rule built with [`MenuEntry::separator`].
pub struct MenuEntry {
    /// Text shown on the left of the row.
    label: SharedString,
    /// Shortcut hint shown right-aligned and muted.
    shortcut: Option<SharedString>,
    /// Invoked when the row is clicked.
    on_activate: Option<ActivateHandler>,
    /// Whether the row is a rule rather than a command.
    separator: bool,
    /// Whether the row may be run at all; see [`MenuEntry::enabled`].
    enabled: bool,
}

impl MenuEntry {
    /// Creates an enabled command row with no shortcut hint and no callback.
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            shortcut: None,
            on_activate: None,
            separator: false,
            enabled: true,
        }
    }

    /// Creates a horizontal rule between two groups of commands.
    pub fn separator() -> Self {
        Self {
            label: SharedString::default(),
            shortcut: None,
            on_activate: None,
            separator: true,
            enabled: true,
        }
    }

    /// Sets the shortcut hint shown at the right edge of the row.
    ///
    /// The hint is decoration only: the key binding itself is registered by the
    /// application, and the menu dispatches the same action the binding does.
    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// Sets the callback run when the row is clicked.
    ///
    /// The menu closes itself afterwards, so the callback does not have to.
    pub fn on_activate(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_activate = Some(Rc::new(handler));
        self
    }

    /// Shows the row without letting it be run.
    ///
    /// A disabled row is drawn muted, takes no hover and no pointer cursor, and
    /// carries no click handler at all — so a click on it runs nothing *and*
    /// leaves the menu open, since the panel occludes the backdrop a press
    /// otherwise dismisses the menu from. That is the point of showing the row
    /// rather than leaving it out: a command that is missing tells the reader
    /// nothing, while one that is greyed out says the surface has it and this is
    /// not the moment. Menus whose rows come and go — the file panel's, which is
    /// built around what was clicked — are better off leaving them out; menus
    /// that are the same list every time are better off greying them.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// Builds the full-window sheet that sits under an open menu.
///
/// A pointer press anywhere it can see dismisses the menu — either mouse button,
/// so that a right-click outside is not swallowed without effect. The panel is
/// drawn above it and occludes it, so presses on a row never reach here.
///
/// Callers wrap this in `anchored`, whose positions are window-relative by
/// default, so the sheet covers the window rather than the caller's own box.
fn menu_backdrop(
    id: ElementId,
    viewport: Size<Pixels>,
    on_dismiss: Option<DismissHandler>,
) -> AnyElement {
    div()
        .id(id)
        .w(viewport.width)
        .h(viewport.height)
        .occlude()
        .when_some(on_dismiss, |this, dismiss| {
            this.on_any_mouse_down(move |_, window, cx| dismiss(window, cx))
        })
        .into_any_element()
}

/// Builds the floating panel listing `entries`.
///
/// Opaque on purpose: a translucent window allows only one tinted fill per
/// pixel, and the terminal surface underneath already owns it.
///
/// `max_height` caps how tall the panel may grow. A list longer than that —
/// the file panel's breadcrumb dropdown, whose rows are whatever the directory
/// holds — scrolls under the wheel instead of running off the bottom of the
/// window, where snapping back inside would only trade the unreachable rows at
/// the end for unreachable rows at the start.
fn menu_panel(
    id: ElementId,
    entries: Vec<MenuEntry>,
    width: Pixels,
    max_height: Pixels,
    on_dismiss: Option<DismissHandler>,
    theme: &Theme,
) -> AnyElement {
    let row_theme = theme.clone();
    let rows = entries.into_iter().enumerate().map(move |(index, entry)| {
        let theme = &row_theme;
        if entry.separator {
            return div()
                .id(ElementId::from(("menu-separator", index)))
                .flex_none()
                .h(px(1.))
                .my(px(4.))
                .mx(px(6.))
                .bg(theme.border);
        }

        let on_dismiss = on_dismiss.clone();
        let MenuEntry {
            label,
            shortcut,
            on_activate,
            enabled,
            ..
        } = entry;

        div()
            .id(ElementId::from(("menu-entry", index)))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(16.))
            .h(px(28.))
            .px(px(10.))
            .mx(px(4.))
            .rounded_sm()
            .text_size(px(13.))
            .text_color(if enabled {
                theme.text
            } else {
                theme.text_muted
            })
            // Everything that makes a row look and behave like a control hangs
            // off this one condition, so a disabled row is inert by carrying no
            // handler rather than by carrying one that thinks better of it.
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(theme.surface_hover))
                    .on_click(move |_, window, cx| {
                        if let Some(activate) = on_activate.clone() {
                            activate(window, cx);
                        }
                        if let Some(dismiss) = on_dismiss.clone() {
                            dismiss(window, cx);
                        }
                    })
            })
            .child(div().flex_1().min_w_0().whitespace_nowrap().child(label))
            .children(shortcut.map(|shortcut| {
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .whitespace_nowrap()
                    .child(shortcut)
            }))
    });

    div()
        .id(id)
        .occlude()
        .flex()
        .flex_col()
        .flex_none()
        .w(width)
        .max_h(max_height)
        .py(px(4.))
        // The panel carries an id, which is what gpui needs to keep a scroll
        // offset for it between frames; the rows stay `flex_none` so that they
        // scroll past rather than being squeezed to fit.
        .overflow_y_scroll()
        .restrict_scroll_to_axis()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .rounded_lg()
        .shadow_lg()
        .text_color(theme.text)
        .children(rows)
        .into_any_element()
}

/// A menu opened at a point of the caller's choosing, with no trigger of its
/// own.
///
/// Rendered by the view that owns the pointer position — typically from an
/// `on_mouse_down(MouseButton::Right, …)` handler that stored the event's
/// window-space position. The element takes no space in its parent's layout, so
/// it can be dropped in anywhere the view already renders:
///
/// ```ignore
/// ContextMenu::new("tab-context")
///     .position(position)
///     .entries(vec![MenuEntry::new("Close tab")])
///     .on_dismiss(|_window, cx| { /* clear the stored position */ })
/// ```
#[derive(IntoElement)]
pub struct ContextMenu {
    id: ElementId,
    position: Point<Pixels>,
    anchor: Anchor,
    width: Pixels,
    entries: Vec<MenuEntry>,
    on_dismiss: Option<DismissHandler>,
}

impl ContextMenu {
    /// Creates an empty menu anchored at the window's top-left corner.
    ///
    /// `id` must be unique among the siblings of the menu.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            position: point(px(0.), px(0.)),
            anchor: Anchor::TopLeft,
            width: px(PANEL_WIDTH),
            entries: Vec::new(),
            on_dismiss: None,
        }
    }

    /// Puts the panel's [`ContextMenu::anchor`] corner at `position`, in window
    /// coordinates.
    ///
    /// A panel that would hang off an edge is pulled back inside the window
    /// instead.
    pub fn position(mut self, position: Point<Pixels>) -> Self {
        self.position = position;
        self
    }

    /// Chooses which corner of the panel sits at the position, and so which way
    /// the menu grows from it.
    ///
    /// [`Anchor::TopLeft`] by default, which is what a right-click wants: the
    /// list hangs down and to the right of the pointer, away from it. A trigger
    /// along the bottom of the window — the status bar — wants
    /// [`Anchor::BottomLeft`] instead, so the list stands *on* the trigger and
    /// opens upward into the window rather than being snapped back over the
    /// thing it was opened from.
    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Narrows or widens the panel.
    ///
    /// The default is the width of the application's own menus, which is set by
    /// their longest row — a pane command naming what it acts on, plus a
    /// shortcut hint. A menu of one-word rows, such as a list of file formats,
    /// reads as a mis-sized dialog at that width and takes its own.
    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    /// Sets the rows of the menu, in display order.
    pub fn entries(mut self, entries: Vec<MenuEntry>) -> Self {
        self.entries = entries;
        self
    }

    /// Called when the menu should go away: after a row is activated, or when
    /// the pointer goes down outside the panel.
    pub fn on_dismiss(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_dismiss = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for ContextMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let viewport = window.viewport_size();
        let backdrop = menu_backdrop(
            ElementId::from((self.id.clone(), "backdrop")),
            viewport,
            self.on_dismiss.clone(),
        );
        let panel = menu_panel(
            ElementId::from((self.id.clone(), "panel")),
            self.entries,
            self.width,
            // What is left of the window once the margin `anchored` snaps to is
            // taken off both edges: the tallest the panel can be and still be
            // reachable wherever it ends up.
            viewport.height - px(2. * WINDOW_MARGIN),
            self.on_dismiss,
            &theme,
        );

        // Absolutely positioned and zero-sized: both children are `anchored` in
        // window coordinates, so this box only has to stay out of the way of the
        // layout it is dropped into.
        div()
            .id(self.id)
            .absolute()
            .w(px(0.))
            .h(px(0.))
            .child(
                deferred(anchored().position(point(px(0.), px(0.))).child(backdrop))
                    .with_priority(BACKDROP_PRIORITY),
            )
            .child(
                deferred(
                    anchored()
                        .anchor(self.anchor)
                        .position(self.position)
                        .snap_to_window_with_margin(px(WINDOW_MARGIN))
                        .child(panel),
                )
                .with_priority(PANEL_PRIORITY),
            )
    }
}

/// A toolbar button with a dropdown menu.
///
/// ```ignore
/// MenuButton::new("app-menu")
///     .open(self.menu_open)
///     .entries(vec![MenuEntry::new("New session").shortcut("Ctrl+T")])
///     .on_open_change(|open, _window, cx| { /* store `open` */ })
/// ```
#[derive(IntoElement)]
pub struct MenuButton {
    id: ElementId,
    glyph: SharedString,
    icon: Option<SharedString>,
    tooltip: Option<SharedString>,
    open: bool,
    entries: Vec<MenuEntry>,
    on_open_change: Option<OpenChangeHandler>,
}

impl MenuButton {
    /// Creates a closed menu button showing the default hamburger glyph.
    ///
    /// `id` must be unique among the siblings of the button.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            glyph: SharedString::new_static("\u{2630}"),
            icon: None,
            tooltip: None,
            open: false,
            entries: Vec::new(),
            on_open_change: None,
        }
    }

    /// Replaces the glyph drawn on the trigger button.
    pub fn glyph(mut self, glyph: impl Into<SharedString>) -> Self {
        self.glyph = glyph.into();
        self
    }

    /// Draws the asset at `path` on the trigger instead of the glyph.
    ///
    /// A second way to dress the same button rather than a replacement for
    /// [`MenuButton::glyph`], because the two triggers in the application want
    /// different things: the application menu's `☰` is a character every font
    /// has and needs no asset, while a chevron drawn as text lands at whatever
    /// size and baseline the font feels like. Callers hand over the path rather
    /// than an element, so this module keeps knowing nothing about the icon set.
    pub fn icon(mut self, path: impl Into<SharedString>) -> Self {
        self.icon = Some(path.into());
        self
    }

    /// Sets the label shown when the pointer rests on the trigger.
    ///
    /// Taken as text rather than looked up here: this layer holds no strings of
    /// its own, so the localised sentence comes from the view that builds the
    /// button.
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Sets whether the dropdown is currently shown.
    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// Sets the rows of the dropdown, in display order.
    pub fn entries(mut self, entries: Vec<MenuEntry>) -> Self {
        self.entries = entries;
        self
    }

    /// Called with the open state the menu would like to be in.
    ///
    /// Fires with `true` when the trigger is clicked while closed, and with
    /// `false` when the trigger is clicked again, when a row is activated, or
    /// when the pointer goes down anywhere outside the panel.
    pub fn on_open_change(
        mut self,
        handler: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for MenuButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let viewport = window.viewport_size();
        let open = self.open;
        let on_open_change = self.on_open_change;

        let on_dismiss: Option<DismissHandler> = on_open_change.clone().map(|handler| {
            Rc::new(move |window: &mut Window, cx: &mut App| handler(false, window, cx))
                as DismissHandler
        });

        // A trigger only ever wears a mark — an icon, or the hamburger glyph
        // standing in for one — never a word, so its resting colour is the
        // theme's icon tint rather than the muted text a label would take.
        let tint = if open { theme.text } else { theme.icon };
        let hover_tint = theme.text;
        // An SVG takes its colour from its own `text_color`, which — unlike a
        // glyph's — does not inherit from the button, so the open and hover
        // shades have to be handed to it directly.
        let face = match self.icon.clone() {
            Some(path) => svg()
                .size(px(TRIGGER_ICON))
                .flex_none()
                .path(path)
                .text_color(tint)
                .group_hover(TRIGGER_GROUP, move |style| style.text_color(hover_tint))
                .into_any_element(),
            None => self.glyph.clone().into_any_element(),
        };

        let trigger = div()
            .id(ElementId::from((self.id.clone(), "trigger")))
            // The trigger may sit inside a window drag area — the toolbar
            // doubles as the title bar in the custom style — and occluding is
            // what keeps a click on it from being read as "move the window".
            .occlude()
            .group(TRIGGER_GROUP)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(TRIGGER_SIZE))
            .rounded_md()
            .text_size(px(14.))
            .text_color(tint)
            .bg(if open {
                theme.surface_active
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
            .when_some(self.tooltip.clone(), |this, tooltip| {
                this.tooltip(tooltip_label(tooltip))
            })
            .when_some(on_open_change.clone(), |this, handler| {
                this.on_click(move |_, window, cx| handler(!open, window, cx))
            })
            .child(face);

        // A full-window sheet under the panel, deferred so that it covers the
        // whole window rather than just the toolbar row this button sits in.
        let backdrop = menu_backdrop(
            ElementId::from((self.id.clone(), "backdrop")),
            viewport,
            on_dismiss.clone(),
        );

        let panel = menu_panel(
            ElementId::from((self.id.clone(), "panel")),
            self.entries,
            px(PANEL_WIDTH),
            viewport.height - px(2. * WINDOW_MARGIN),
            on_dismiss,
            &theme,
        );

        // The dropdown hangs off a zero-width box laid out *before* the
        // trigger, not off the trigger itself. An `anchored` element is
        // absolutely positioned, and an absolutely positioned box is aligned by
        // its parent's `align-items`; hanging it directly in the `items_center`
        // row would centre the whole panel on the 28px button instead of
        // starting it at the button's top-left corner. This box neither centres
        // its children nor takes up space, so the panel starts exactly one
        // [`DROP_OFFSET`] below the top-left of the trigger.
        let overlays = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(0.))
            .h(px(TRIGGER_SIZE))
            .child(
                deferred(anchored().position(point(px(0.), px(0.))).child(backdrop))
                    .with_priority(BACKDROP_PRIORITY),
            )
            .child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.), px(DROP_OFFSET)))
                        .snap_to_window_with_margin(px(WINDOW_MARGIN))
                        .child(panel),
                )
                .with_priority(PANEL_PRIORITY),
            );

        div()
            .id(self.id)
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .children(open.then_some(overlays))
            .child(trigger)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::ops::Deref;

    use gpui::{
        Context, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Render, ScrollDelta,
        ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext,
    };

    use super::*;

    /// Where the harness anchors the menu: far enough from every edge that
    /// nothing is snapped back inside, so the row arithmetic below holds.
    const MENU_X: f32 = 100.;

    /// Top of the panel, for the same reason.
    const MENU_Y: f32 = 50.;

    /// Height of one command row, as [`menu_panel`] lays it out.
    const ROW_HEIGHT: f32 = 28.;

    /// What the panel puts above its first row: its border and its padding.
    const PANEL_TOP: f32 = 5.;

    /// A column inside the panel, which is [`PANEL_WIDTH`] wide.
    const INSIDE_THE_PANEL: f32 = MENU_X + 60.;

    /// A point the panel does not cover, so a press there reaches the backdrop.
    const OUTSIDE: f32 = 10.;

    /// A view holding one open menu, as the surface that owns a right-click
    /// would.
    ///
    /// The rows are kept as descriptions rather than as entries: a
    /// [`MenuEntry`] owns callbacks and cannot be cloned, so the harness builds
    /// them again on every draw, the way a real view rebuilds its menu from its
    /// own state.
    struct Harness {
        rows: Vec<(SharedString, bool)>,
        position: Point<Pixels>,
        anchor: Anchor,
        activated: Rc<RefCell<Vec<usize>>>,
        dismissed: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let dismissed = self.dismissed.clone();
            let entries = self
                .rows
                .iter()
                .enumerate()
                .map(|(index, (label, enabled))| {
                    let activated = self.activated.clone();
                    MenuEntry::new(label.clone())
                        .enabled(*enabled)
                        .on_activate(move |_, _| activated.borrow_mut().push(index))
                })
                .collect();

            div().size_full().child(
                ContextMenu::new("menu")
                    .position(self.position)
                    .anchor(self.anchor)
                    .entries(entries)
                    .on_dismiss(move |_, _| dismissed.set(dismissed.get() + 1)),
            )
        }
    }

    /// What a test reads back out of a running harness.
    struct Handles {
        activated: Rc<RefCell<Vec<usize>>>,
        dismissed: Rc<Cell<usize>>,
    }

    impl Handles {
        /// The rows run since the last look.
        fn drain(&self) -> Vec<usize> {
            self.activated.borrow_mut().drain(..).collect()
        }

        /// How many times the menu has asked to close.
        fn dismissals(&self) -> usize {
            self.dismissed.get()
        }
    }

    /// Opens a window on a menu of `rows`, each a label and whether it is
    /// enabled, hanging down and to the right of the anchor.
    fn open(
        rows: Vec<(SharedString, bool)>,
        cx: &mut TestAppContext,
    ) -> (Handles, VisualTestContext) {
        open_anchored(rows, point(px(MENU_X), px(MENU_Y)), Anchor::TopLeft, cx)
    }

    /// The same, with the panel's `anchor` corner at `position`.
    fn open_anchored(
        rows: Vec<(SharedString, bool)>,
        position: Point<Pixels>,
        anchor: Anchor,
        cx: &mut TestAppContext,
    ) -> (Handles, VisualTestContext) {
        cx.update(super::super::init);

        let handles = Handles {
            activated: Rc::new(RefCell::new(Vec::new())),
            dismissed: Rc::new(Cell::new(0)),
        };
        let window = cx.add_window({
            let activated = handles.activated.clone();
            let dismissed = handles.dismissed.clone();
            move |_, _| Harness {
                rows,
                position,
                anchor,
                activated,
                dismissed,
            }
        });
        let cx = VisualTestContext::from_window(*window.deref(), cx);
        cx.run_until_parked();

        (handles, cx)
    }

    /// The middle of row `index`, on its label.
    fn row_middle(index: usize) -> Point<Pixels> {
        point(
            px(INSIDE_THE_PANEL),
            px(MENU_Y + PANEL_TOP + ROW_HEIGHT * index as f32 + ROW_HEIGHT / 2.),
        )
    }

    /// Rolls the wheel over `position` by `delta` pixels, negative being the
    /// direction that brings later rows into view.
    fn scroll(cx: &mut VisualTestContext, position: Point<Pixels>, delta: f32) {
        cx.simulate_event(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.run_until_parked();
    }

    /// Presses and releases the left button over `position`.
    fn click(cx: &mut VisualTestContext, position: Point<Pixels>) {
        cx.simulate_event(MouseDownEvent {
            position,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 1,
        });
        cx.run_until_parked();
    }

    /// The two halves of what a greyed row means: the callback never runs, and
    /// the menu is still there afterwards — a row that did nothing *and* closed
    /// the menu would read as a command that silently failed.
    #[gpui::test]
    fn a_disabled_row_runs_nothing_and_leaves_the_menu_open(cx: &mut TestAppContext) {
        let (menu, mut cx) = open(
            vec![
                (SharedString::new_static("Copy"), true),
                (SharedString::new_static("Cut"), false),
            ],
            cx,
        );

        click(&mut cx, row_middle(1));
        assert_eq!(menu.drain(), Vec::<usize>::new());
        assert_eq!(menu.dismissals(), 0, "the menu stays where it is");

        // The row above it, which is the same in every way but enabled, still
        // runs and still closes the menu.
        click(&mut cx, row_middle(0));
        assert_eq!(menu.drain(), vec![0]);
        assert_eq!(menu.dismissals(), 1);

        // And the backdrop under the panel still dismisses, as it did before any
        // of this: only the panel swallows presses.
        click(&mut cx, point(px(OUTSIDE), px(OUTSIDE)));
        assert_eq!(menu.drain(), Vec::<usize>::new());
        assert_eq!(menu.dismissals(), 2);
    }

    /// What the status bar's file-type picker needs: a menu that *stands on*
    /// its anchor instead of hanging from it, because the anchor is in the last
    /// two dozen pixels of the window and a list hanging down from there would
    /// be snapped back over the thing it was opened from.
    #[gpui::test]
    fn a_bottom_anchored_menu_opens_upward_from_its_position(cx: &mut TestAppContext) {
        // Far enough down the window that a two-row panel standing on it still
        // clears the top edge, so nothing is snapped and the arithmetic holds.
        let foot = px(300.);
        let (menu, mut cx) = open_anchored(
            vec![
                (SharedString::new_static("JSON"), true),
                (SharedString::new_static("YAML"), true),
            ],
            point(px(MENU_X), foot),
            Anchor::BottomLeft,
            cx,
        );

        // The last row sits directly above the anchor, under the panel's own
        // padding and border; the first is a row further up again.
        let last = px(f32::from(foot) - PANEL_TOP - ROW_HEIGHT / 2.);
        let first = px(f32::from(last) - ROW_HEIGHT);
        click(&mut cx, point(px(INSIDE_THE_PANEL), last));
        assert_eq!(menu.drain(), vec![1]);

        click(&mut cx, point(px(INSIDE_THE_PANEL), first));
        assert_eq!(menu.drain(), vec![0]);

        // And nothing of the panel hangs below the anchor: a press just under
        // it reaches the backdrop, which is what the bar it opened from would
        // otherwise be covered by.
        click(
            &mut cx,
            point(px(INSIDE_THE_PANEL), px(f32::from(foot) + 4.)),
        );
        assert_eq!(menu.drain(), Vec::<usize>::new());
        assert!(menu.dismissals() > 0);
    }

    /// What the file panel's breadcrumb dropdown needs: a directory of more
    /// entries than the window is tall still reaches its last one. Before the
    /// panel took a maximum height the overflow was simply unreachable — the
    /// rows past the bottom edge were drawn outside the window, and snapping
    /// the panel back inside would only have moved the problem to the top.
    #[gpui::test]
    fn a_menu_taller_than_the_window_scrolls_under_the_wheel(cx: &mut TestAppContext) {
        let rows: Vec<(SharedString, bool)> = (0..60)
            .map(|index| (SharedString::from(format!("Row {index}")), true))
            .collect();
        let (menu, mut cx) = open(rows, cx);

        // A panel capped at the window's height, snapped back inside, starts one
        // margin below the top edge — wherever it was anchored.
        let first_row = px(WINDOW_MARGIN + PANEL_TOP + ROW_HEIGHT / 2.);
        let top_of_list = point(px(INSIDE_THE_PANEL), first_row);

        click(&mut cx, top_of_list);
        assert_eq!(
            menu.drain(),
            vec![0],
            "unscrolled, the list starts at row 0"
        );
        assert_eq!(menu.dismissals(), 1);

        // Far enough to be clamped to the end of the list rather than landing on
        // a row this test would have to predict.
        scroll(&mut cx, top_of_list, -10_000.);
        click(&mut cx, top_of_list);
        let activated = menu.drain();
        assert_eq!(activated.len(), 1, "the same click still runs one row");
        assert!(
            activated[0] > 0,
            "the wheel brought a later row under the pointer, got {activated:?}"
        );
    }
}
