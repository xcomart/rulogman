//! The application settings dialog.
//!
//! Edits [`AppSettings`] and nothing else: it reads the current snapshot from
//! [`crate::app_settings`] when it opens, writes the edited copy to disk when
//! the user saves, and replaces the global so the rest of the app picks the
//! change up. Range checking is deliberately *not* duplicated here — the form
//! collects whatever the user typed and [`AppSettings::sanitize`] clamps it once
//! on the way out, which keeps one definition of "valid" in `rulogman-core`.

use std::sync::{Arc, Once};

use gpui::{
    App, Context, DragMoveEvent, Entity, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement,
    KeyBinding, KeyDownEvent, MouseButton, MouseUpEvent, Render, ScrollHandle, SharedString,
    Subscription, Window, actions, div, prelude::*, px, rgb,
};
use rulogman_core::{AppSettings, TitlebarStyle};
use rulogman_term::TerminalTheme;

use crate::app_settings;
use crate::i18n::{self, input_menu_labels, ts};
use crate::scheme_catalog::SchemeCatalog;
use crate::theme_store;
use ruui::{
    Button, ButtonVariant, Checkbox, DraggedThumb, SchemePreview, SchemeSelect, SchemeSwatch,
    Scrollbar, ScrollbarAxis, ScrollbarState, Segmented, Select, TextInput, Theme, ThemeRegistry,
    form_row, hide_later, hide_now, modal, scroll_to, scrolled, theme,
};
use ruui_shell::form::{
    format_number, installed_fonts, parse_number, restrict_to_number, section, set_text, suffixed,
    text,
};
use ruui_shell::{
    CatalogActionEvent, CatalogActions, ThemeCatalog, ThemeEditor, ThemeEditorEvent, UiThemeCatalog,
};

/// The dialog's scrolling surfaces, and the element id of each one's overlay
/// scroll indicator.
///
/// One drag listener answers them all, so it has to be able to say which bar a
/// drag belongs to; these ids are how, and pairing each with the handle and the
/// state it goes with keeps them from being wired up crosswise.
const SCROLLBARS: [(&str, Surface); 5] = [
    ("settings-body-scrollbar", Surface::Body),
    ("settings-font-scrollbar", Surface::Font),
    ("settings-language-scrollbar", Surface::Language),
    ("settings-ui-theme-scrollbar", Surface::UiTheme),
    ("settings-scheme-scrollbar", Surface::Scheme),
];

/// Which of the dialog's scrolling surfaces is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    /// The dialog body, which scrolls behind the footer.
    Body,
    /// The open font list.
    Font,
    /// The open language list.
    Language,
    /// The open UI theme list.
    UiTheme,
    /// The open terminal color scheme list.
    Scheme,
}

/// Width of the dialog panel.
const DIALOG_WIDTH: f32 = 760.;

/// Height at which the form body starts scrolling.
const BODY_MAX_HEIGHT: f32 = 520.;

/// ANSI slots previewed on each scheme row: red, green, yellow, blue, magenta,
/// cyan. Black and white are skipped because they vanish into the background.
const PREVIEW_ANSI_SLOTS: [usize; 6] = [1, 2, 3, 4, 5, 6];

/// Segments of the title bar style picker, in [`TitlebarStyle`] order.
///
/// The first half of each pair is an element id and is never translated; only
/// the label is. Built per call rather than declared as a `const` because the
/// labels come out of the active locale.
fn titlebar_options() -> [(&'static str, SharedString); 2] {
    [
        ("custom", ts!("settings.titlebar_custom")),
        ("system", ts!("settings.titlebar_system")),
    ]
}

/// Label of the entry that hands the choice back to the operating system.
///
/// Heads both dropdowns in the dialog, and doubles as their placeholder so a
/// trigger reads the same whether or not its list is open.
fn system_default() -> SharedString {
    ts!("settings.system_default")
}

/// Key context the dialog's own shortcuts are scoped to.
///
/// `Tab` stays scoped here for the same reason it does in the connection
/// dialog: a global binding would stop the terminal from sending `\t`.
const KEY_CONTEXT: &str = "SettingsDialog";

/// Guards the one-time registration of the dialog's key bindings.
static BIND_KEYS: Once = Once::new();

actions!(
    rulogman_settings,
    [
        /// Move focus to the next control in the dialog.
        FocusNext,
        /// Move focus to the previous control in the dialog.
        FocusPrev,
    ]
);

/// Tab order of the form, in visual order, spaced so controls can be inserted
/// later without renumbering.
mod tab {
    /// UI theme picker.
    pub const UI_THEME: isize = 10;
    /// First index of the management row under the UI theme picker.
    pub const UI_THEME_ACTIONS: isize = 11;
    /// Title bar style picker.
    pub const TITLEBAR: isize = 18;
    /// Interface language picker.
    pub const LANGUAGE: isize = 20;
    /// Background opacity, in percent.
    pub const OPACITY: isize = 30;
    /// Background blur toggle.
    pub const BLUR: isize = 40;
    /// Terminal color scheme picker.
    pub const SCHEME: isize = 50;
    /// First index of the management row under the scheme picker.
    pub const SCHEME_ACTIONS: isize = 51;
    /// Terminal font family.
    pub const FONT_FAMILY: isize = 60;
    /// Terminal font size.
    pub const FONT_SIZE: isize = 70;
    /// Scrollback depth.
    pub const SCROLLBACK: isize = 80;
    /// `TERM` advertised to the remote host.
    pub const TERM: isize = 90;
    /// Copy-on-select toggle.
    pub const COPY_ON_SELECT: isize = 100;
    /// Default SSH port for new connections.
    pub const DEFAULT_PORT: isize = 110;
    /// Default login name for new connections.
    pub const DEFAULT_USERNAME: isize = 120;
    /// Keepalive interval.
    pub const KEEPALIVE: isize = 130;
    /// Connect timeout.
    pub const TIMEOUT: isize = 140;
    /// Cancel.
    pub const CANCEL: isize = 150;
    /// Save.
    pub const SAVE: isize = 160;
}

/// Emitted by [`SettingsDialog`] when the user acts on it.
pub enum SettingsDialogEvent {
    /// The user saved: the settings global has been replaced and persisted.
    /// The shell should re-apply settings to the window and open sessions.
    Applied,
    /// A theme or scheme file was written, imported or removed while the dialog
    /// stayed open. The settings themselves have not changed, but what their
    /// ids resolve to has, so the shell has to re-apply them — without taking
    /// the focus back off the dialog, which is still on screen.
    ThemesChanged,
    /// The dialog was dismissed without saving.
    Dismissed,
}

/// Which of the two colour catalogues is meant.
///
/// The catalogues are parallel in every way that matters to this dialog — each
/// is a list of built-in entries plus a directory of files, each is picked from
/// a grid of cards, each has a management row under it — so every action is
/// written once against this enum instead of twice against the two registries.
/// What each of them *is* lives behind
/// [`ThemeCatalog`](ruui_shell::ThemeCatalog): `ruui`'s own
/// [`UiThemeCatalog`](ruui_shell::UiThemeCatalog) for the chrome themes, and
/// [`SchemeCatalog`] for the terminal palettes, which is rulogman's because a
/// widget kit has no terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Catalog {
    /// The chrome themes of [`ThemeRegistry`].
    UiTheme,
    /// The terminal colour schemes of [`TerminalTheme`].
    Scheme,
}

/// Builds the preview colors for the scheme with the given id.
///
/// Returns `None` for an id `rulogman-term` does not know, which is how a
/// hand-edited `settings.json` naming a removed scheme degrades to a plain card
/// instead of a panic.
fn preview_for(id: &str) -> Option<SchemePreview> {
    let scheme = TerminalTheme::by_name(id)?;
    Some(SchemePreview {
        background: rgb(scheme.background.to_u32()).into(),
        foreground: rgb(scheme.foreground.to_u32()).into(),
        accents: PREVIEW_ANSI_SLOTS
            .iter()
            .map(|slot| rgb(scheme.ansi[*slot].to_u32()).into())
            .collect(),
    })
}

/// Every scheme as a picker entry, each with a live preview.
///
/// Built from [`TerminalTheme::all_schemes`] rather than from the built-in
/// table, so a scheme file the user has dropped into the `schemes` directory
/// shows up here and in the per-profile picker alike.
///
/// A scheme whose colors cannot be resolved falls back to the muted placeholder
/// card, so it is given the translated label that card draws.
pub(crate) fn scheme_swatches() -> Vec<SchemeSwatch> {
    TerminalTheme::all_schemes()
        .into_iter()
        .map(|entry| {
            let swatch = SchemeSwatch::new(entry.id.clone(), entry.name);
            match preview_for(&entry.id) {
                Some(preview) => swatch.preview(preview),
                None => swatch.placeholder_label(ts!("common.inherits")),
            }
        })
        .collect()
}

/// The UI themes as picker entries, each previewing its own chrome.
///
/// The same widget the terminal schemes use: a theme is a background, a text
/// color and a handful of accents, which is exactly what a scheme card draws.
/// The chips are the colors a user actually judges a theme by — the accent, the
/// two status colors and the two raised surfaces.
fn ui_theme_swatches(cx: &App) -> Vec<SchemeSwatch> {
    ThemeRegistry::all(cx)
        .into_iter()
        .map(|entry| {
            let palette = ThemeRegistry::resolve(&entry.id, cx);
            SchemeSwatch::new(entry.id, entry.name).preview(SchemePreview {
                background: palette.background,
                foreground: palette.text,
                accents: vec![
                    palette.accent,
                    palette.success,
                    palette.danger,
                    palette.surface_active,
                    palette.border,
                ],
            })
        })
        .collect()
}

/// Which of the dialog's dropdown lists is currently showing.
///
/// A single field rather than one flag per dropdown, so that no two can be open
/// at once — their lists are drawn deferred and would overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenList {
    /// The interface language picker.
    Language,
    /// The terminal font picker.
    Font,
    /// The UI theme picker.
    UiTheme,
    /// The terminal color scheme picker.
    Scheme,
}

/// Severity of the message strip at the bottom of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusLevel {
    /// Something went wrong and the settings were not written.
    Error,
}

impl StatusLevel {
    /// Color of the message text under the active theme.
    fn color(self, theme: &Theme) -> Hsla {
        match self {
            Self::Error => theme.danger,
        }
    }
}

/// Modal dialog editing [`rulogman_core::AppSettings`].
///
/// Create it once with [`SettingsDialog::new`], keep the handle, subscribe to
/// [`SettingsDialogEvent`], and render it as the last child of a `relative()`
/// root. It renders nothing while [`SettingsDialog::is_open`] is `false`, so it
/// is safe to render unconditionally.
pub struct SettingsDialog {
    /// Whether the dialog is currently visible.
    open: bool,
    /// The snapshot the form was populated from. Saving starts from this value
    /// so fields the dialog does not edit — the schema version, for one —
    /// survive a round trip.
    base: AppSettings,
    /// UI chrome theme id currently selected in the form.
    ui_theme: SharedString,
    /// Title bar style currently selected in the form.
    titlebar: TitlebarStyle,
    /// BCP 47 tag of the interface language; `None` follows the system locale.
    /// Holds the tag rather than the label, because the label is what the
    /// dropdown shows and the tag is what gets persisted.
    language: Option<String>,
    /// Whether the window should be blurred behind.
    background_blur: bool,
    /// Terminal color scheme id currently selected in the form.
    scheme: SharedString,
    /// Whether the selection is copied to the clipboard on mouse release.
    copy_on_select: bool,
    /// The management row under the UI theme picker.
    ui_theme_actions: Entity<CatalogActions>,
    /// The management row under the color scheme picker.
    scheme_actions: Entity<CatalogActions>,
    /// Keeps the two rows' subscriptions alive.
    _catalog_events: [Subscription; 2],
    /// The colour editor, while one is open. The dialog renders it *instead of*
    /// the form rather than over it; see [`ruui_shell::theme_editor`] for why.
    editor: Option<Entity<ThemeEditor>>,
    /// Keeps the open editor's subscription alive.
    editor_events: Option<Subscription>,
    /// Message strip shown above the buttons.
    status: Option<SharedString>,
    /// Focus of the dialog root; also the anchor for the `Escape` handler.
    focus_handle: FocusHandle,
    /// Whether focus should move into the form on the next render.
    pending_focus: bool,
    /// Scroll position of the form body, so `Tab` can reveal the section it
    /// just moved into.
    body_scroll: ScrollHandle,
    /// Whether the body's overlay scroll indicator is on screen.
    body_scrollbar: ScrollbarState,
    /// Whether the font list's overlay scroll indicator is on screen.
    font_scrollbar: ScrollbarState,
    /// Whether the language list's overlay scroll indicator is on screen.
    language_scrollbar: ScrollbarState,
    /// Whether the UI theme list's overlay scroll indicator is on screen.
    ui_theme_scrollbar: ScrollbarState,
    /// Whether the scheme list's overlay scroll indicator is on screen.
    scheme_scrollbar: ScrollbarState,
    /// Index of the section currently scrolled into view. Kept so that tabbing
    /// between two controls of the same section does not re-scroll it.
    visible_section: usize,
    /// Terminal font family; `None` means the per-OS default.
    font_family: Option<SharedString>,
    /// Which dropdown, if any, is showing its list.
    open_list: Option<OpenList>,
    /// Font families installed on the machine, read once per opening of the
    /// dialog rather than on every render.
    fonts: Vec<SharedString>,
    /// Scroll position of the font list, so opening it reveals the current
    /// font instead of the top of the alphabet.
    font_scroll: ScrollHandle,
    /// Scroll position of the language list, kept for the same reason.
    language_scroll: ScrollHandle,
    /// Scroll position of the UI theme list, kept for the same reason.
    ui_theme_scroll: ScrollHandle,
    /// Scroll position of the color scheme list, kept for the same reason.
    scheme_scroll: ScrollHandle,
    /// Window background opacity, in whole percent.
    opacity_input: Entity<TextInput>,
    /// Terminal font size.
    font_size_input: Entity<TextInput>,
    /// Scrollback depth in lines.
    scrollback_input: Entity<TextInput>,
    /// `TERM` advertised to the remote host.
    term_input: Entity<TextInput>,
    /// Default SSH port for new connections.
    port_input: Entity<TextInput>,
    /// Default login name for new connections.
    username_input: Entity<TextInput>,
    /// Seconds between keepalive probes.
    keepalive_input: Entity<TextInput>,
    /// Seconds to wait for the TCP connection.
    timeout_input: Entity<TextInput>,
}

impl SettingsDialog {
    /// Build the dialog.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();

        BIND_KEYS.call_once(|| {
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, Some(KEY_CONTEXT)),
                KeyBinding::new("shift-tab", FocusPrev, Some(KEY_CONTEXT)),
            ]);
        });

        // `Enter` saves from any field, matching the connection dialog. The
        // deferred call is load-bearing: `on_submit` runs while gpui has the
        // TextInput leased, and saving reads every field back.
        let field = {
            let weak = weak.clone();
            move |cx: &mut Context<Self>, placeholder: SharedString, tab_index: isize| {
                let weak = weak.clone();
                cx.new(move |cx| {
                    TextInput::new(cx)
                        .context_menu(input_menu_labels)
                        .placeholder(placeholder)
                        .tab_index(tab_index)
                        .on_submit(move |_, _window, cx| {
                            let weak = weak.clone();
                            cx.defer(move |cx| {
                                weak.update(cx, |this, cx| this.save(cx)).ok();
                            });
                        })
                })
            }
        };

        // Every placeholder but one is a sample *value* — a number, or the
        // default `TERM` — and reads the same in every language. The username
        // hint is a word, so it is translated; it is also the only placeholder
        // `refresh_placeholders` has to revisit after a language switch.
        let opacity_input = field(cx, "100".into(), tab::OPACITY);
        let font_size_input = field(cx, "14".into(), tab::FONT_SIZE);
        let scrollback_input = field(cx, "5000".into(), tab::SCROLLBACK);
        let term_input = field(cx, "xterm-256color".into(), tab::TERM);
        let port_input = field(cx, "22".into(), tab::DEFAULT_PORT);
        let username_input = field(
            cx,
            ts!("settings.username_placeholder"),
            tab::DEFAULT_USERNAME,
        );
        let keepalive_input = field(cx, "30".into(), tab::KEEPALIVE);
        let timeout_input = field(cx, "15".into(), tab::TIMEOUT);

        // Numeric fields have no input filter of their own, so each one is
        // sanitised after the fact by an observer.
        restrict_to_number(cx, &opacity_input, false, 3);
        restrict_to_number(cx, &font_size_input, true, 5);
        restrict_to_number(cx, &scrollback_input, false, 6);
        restrict_to_number(cx, &port_input, false, 5);
        restrict_to_number(cx, &keepalive_input, false, 5);
        restrict_to_number(cx, &timeout_input, false, 5);

        let base = AppSettings::default();

        // The two catalogues, with the ids to fall back on when the selected
        // entry is deleted. Built once: the directories are fixed for the run,
        // and a row that had to be rebuilt would drop the confirmation it was
        // in the middle of asking.
        let ui_catalog: Arc<dyn ThemeCatalog> = Arc::new(UiThemeCatalog::new(
            theme_store::theme_dirs_or_empty(),
            base.ui_theme.clone(),
        ));
        let scheme_catalog: Arc<dyn ThemeCatalog> = Arc::new(SchemeCatalog);
        let ui_theme_actions = cx.new(|_| CatalogActions::new(ui_catalog, tab::UI_THEME_ACTIONS));
        let scheme_actions = cx.new(|_| CatalogActions::new(scheme_catalog, tab::SCHEME_ACTIONS));
        let catalog_events = [
            cx.subscribe(&ui_theme_actions, |dialog, _row, event, cx| {
                dialog.on_catalog_event(Catalog::UiTheme, event, cx);
            }),
            cx.subscribe(&scheme_actions, |dialog, _row, event, cx| {
                dialog.on_catalog_event(Catalog::Scheme, event, cx);
            }),
        ];

        Self {
            open: false,
            ui_theme: base.ui_theme.clone().into(),
            titlebar: base.window.titlebar,
            language: base.language.clone(),
            background_blur: base.window.background_blur,
            scheme: base.terminal.scheme.clone().into(),
            copy_on_select: base.terminal.copy_on_select,
            font_family: base.terminal.font_family.clone().map(SharedString::from),
            base,
            ui_theme_actions,
            scheme_actions,
            _catalog_events: catalog_events,
            editor: None,
            editor_events: None,
            status: None,
            focus_handle: cx.focus_handle(),
            pending_focus: false,
            body_scroll: ScrollHandle::new(),
            body_scrollbar: ScrollbarState::new(),
            font_scrollbar: ScrollbarState::new(),
            language_scrollbar: ScrollbarState::new(),
            ui_theme_scrollbar: ScrollbarState::new(),
            scheme_scrollbar: ScrollbarState::new(),
            visible_section: 0,
            open_list: None,
            fonts: Vec::new(),
            font_scroll: ScrollHandle::new(),
            language_scroll: ScrollHandle::new(),
            ui_theme_scroll: ScrollHandle::new(),
            scheme_scroll: ScrollHandle::new(),
            opacity_input,
            font_size_input,
            scrollback_input,
            term_input,
            port_input,
            username_input,
            keepalive_input,
            timeout_input,
        }
    }

    /// The handle and bar state of one scrolling surface.
    fn surface(&mut self, surface: Surface) -> (&ScrollHandle, &mut ScrollbarState) {
        match surface {
            Surface::Body => (&self.body_scroll, &mut self.body_scrollbar),
            Surface::Font => (&self.font_scroll, &mut self.font_scrollbar),
            Surface::Language => (&self.language_scroll, &mut self.language_scrollbar),
            Surface::UiTheme => (&self.ui_theme_scroll, &mut self.ui_theme_scrollbar),
            Surface::Scheme => (&self.scheme_scroll, &mut self.scheme_scrollbar),
        }
    }

    /// The same pair, for the renders that only read them.
    fn surface_ref(&self, surface: Surface) -> (&ScrollHandle, &ScrollbarState) {
        match surface {
            Surface::Body => (&self.body_scroll, &self.body_scrollbar),
            Surface::Font => (&self.font_scroll, &self.font_scrollbar),
            Surface::Language => (&self.language_scroll, &self.language_scrollbar),
            Surface::UiTheme => (&self.ui_theme_scroll, &self.ui_theme_scrollbar),
            Surface::Scheme => (&self.scheme_scroll, &self.scheme_scrollbar),
        }
    }

    /// The overlay scroll indicator of one surface, as it stands.
    fn scrollbar(&self, id: &'static str, surface: Surface) -> Scrollbar {
        let (handle, state) = self.surface_ref(surface);
        Scrollbar::for_handle(id, ScrollbarAxis::Vertical, handle).fade(state.fade())
    }

    /// The same bar, listening for the pointer reaching the edge it rides.
    ///
    /// Only the bars that are drawn need it: the one the drag path builds is
    /// there to be measured, and never reaches an element tree.
    fn hovering_scrollbar(
        &self,
        id: &'static str,
        surface: Surface,
        cx: &mut Context<Self>,
    ) -> Scrollbar {
        self.scrollbar(id, surface).on_hover(cx.listener(
            move |dialog, hovered: &bool, _window, cx| {
                dialog.hover_scrollbar(surface, *hovered, cx);
            },
        ))
    }

    /// Puts each surface's bar up whenever it has been scrolled, and starts the
    /// clock that takes it down again.
    fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            let (handle, state) = self.surface(surface);
            let scrolled = scrolled(handle, ScrollbarAxis::Vertical);
            if let Some(epoch) = state.moved(scrolled) {
                hide_later(epoch, cx, move |dialog| Some(dialog.surface(surface).1));
            }
        }
    }

    /// Scrolls whichever surface's thumb has been dragged.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        for (id, surface) in SCROLLBARS {
            let Some(progress) = self.scrollbar(id, surface).dragged(event, cx) else {
                continue;
            };

            let (handle, state) = self.surface(surface);
            state.hold();
            scroll_to(handle, ScrollbarAxis::Vertical, progress);
            cx.notify();
            return;
        }
    }

    /// Lets go of whichever thumb was being held, and starts its clock again.
    fn release_scrollbars(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            if let Some(epoch) = self.surface(surface).1.release() {
                hide_later(epoch, cx, move |dialog| Some(dialog.surface(surface).1));
                cx.notify();
            }
        }
    }

    /// Puts one surface's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    ///
    /// Told which surface rather than asked to work it out: three bars are on
    /// screen at once at most, and each strip knows only its own.
    fn hover_scrollbar(&mut self, surface: Surface, hovered: bool, cx: &mut Context<Self>) {
        let state = self.surface(surface).1;
        if hovered {
            if state.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = state.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, move |dialog| {
            Some(dialog.surface(surface).1)
        });
    }

    /// Show the dialog, re-reading the current settings into the form.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        let settings = app_settings::current(cx);
        self.fonts = installed_fonts(cx);
        self.refresh_placeholders(cx);
        self.fill_form(&settings, cx);
        self.base = settings;
        self.status = None;
        for row in [&self.ui_theme_actions, &self.scheme_actions] {
            row.clone().update(cx, |row, cx| row.clear_status(cx));
        }
        self.editor = None;
        self.editor_events = None;
        self.open = true;
        self.open_list = None;
        self.pending_focus = true;
        self.visible_section = 0;
        self.body_scroll.scroll_to_item(0);
        cx.notify();
    }

    /// Whether the dialog is visible.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Hide the dialog without saving.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.open_list = None;
        self.pending_focus = false;
        self.status = None;
        self.editor = None;
        self.editor_events = None;
        cx.notify();
    }

    /// The management row under one catalogue's picker.
    fn actions(&self, catalog: Catalog) -> &Entity<CatalogActions> {
        match catalog {
            Catalog::UiTheme => &self.ui_theme_actions,
            Catalog::Scheme => &self.scheme_actions,
        }
    }

    /// The catalogue behind one of the two rows.
    fn catalog_of(&self, catalog: Catalog, cx: &App) -> Arc<dyn ThemeCatalog> {
        self.actions(catalog).read(cx).catalog().clone()
    }

    /// Highlights `id` in one catalogue's picker.
    ///
    /// Only the form is touched; nothing is persisted until the dialog is
    /// saved, exactly as when the user clicks a card. The management row is
    /// told too, because everything it offers is about the selection and a row
    /// that had not been told would grey the wrong buttons out.
    fn select(&mut self, catalog: Catalog, id: impl Into<SharedString>, cx: &mut Context<Self>) {
        let id = id.into();
        match catalog {
            Catalog::UiTheme => self.ui_theme = id.clone(),
            Catalog::Scheme => self.scheme = id.clone(),
        }
        self.actions(catalog)
            .clone()
            .update(cx, |row, cx| row.set_selection(id.to_string(), cx));
        cx.notify();
    }

    /// What one of the two management rows asked the dialog to do.
    ///
    /// The row owns the files, the confirmation and everything it reports; the
    /// dialog owns the form field the selection lives in and the body the
    /// editor is drawn instead of. See [`ruui_shell::catalog_ui`] for why the
    /// line is where it is.
    fn on_catalog_event(
        &mut self,
        catalog: Catalog,
        event: &CatalogActionEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            CatalogActionEvent::Select(id) => self.select(catalog, id.clone(), cx),
            // The files on disk moved under a palette that may already be in
            // use, so what the window is wearing has to be resolved again —
            // without taking the focus off the dialog, which is still open.
            CatalogActionEvent::Changed => cx.emit(SettingsDialogEvent::ThemesChanged),
            // The file travels in the event, but a gpui subscription only ever
            // borrows one and `CatalogFile` is not `Clone` — a catalogue of an
            // application's own may hold anything at all — so it is resolved
            // again from the id. That is the same registry lookup the row
            // itself made, over the registry it has just reloaded.
            CatalogActionEvent::Edit { id, .. } => {
                let source = self.catalog_of(catalog, cx);
                if let Some(file) = source.load(id, cx) {
                    let editor = cx.new(|cx| ThemeEditor::new(source, id.clone(), &file, cx));
                    self.open_editor(editor, cx);
                }
            }
        }
    }

    /// Puts the editor in front of the form.
    fn open_editor(&mut self, editor: Entity<ThemeEditor>, cx: &mut Context<Self>) {
        self.editor_events = Some(cx.subscribe(&editor, |dialog, _editor, event, cx| {
            let saved = matches!(event, ThemeEditorEvent::Saved);
            dialog.close_editor(saved, cx);
        }));
        self.editor = Some(editor);
        self.close_lists(cx);
        cx.notify();
    }

    /// Takes the editor back down and returns to the form.
    ///
    /// When something was written the shell is told, so that a palette already
    /// in use repaints under its new colours without the settings themselves
    /// having to be saved.
    fn close_editor(&mut self, saved: bool, cx: &mut Context<Self>) {
        self.editor = None;
        self.editor_events = None;
        self.pending_focus = true;
        if saved {
            cx.emit(SettingsDialogEvent::ThemesChanged);
        }
        cx.notify();
    }

    /// Re-translate the placeholders of the fields that have a worded one.
    ///
    /// The text fields are built once, when the dialog is created, so their
    /// hints would otherwise still be in whatever language was active at
    /// start-up after the user switches — including right after switching it
    /// here.
    fn refresh_placeholders(&self, cx: &mut Context<Self>) {
        self.username_input.update(cx, |input, cx| {
            input.set_placeholder(ts!("settings.username_placeholder"), cx);
        });
    }

    /// Copy `settings` into every control.
    fn fill_form(&mut self, settings: &AppSettings, cx: &mut Context<Self>) {
        // Through `select`, not by assignment: the management row under each
        // picker has to be told which entry it is acting on, and a row that had
        // not been told would grey the wrong buttons out.
        self.select(Catalog::UiTheme, settings.ui_theme.clone(), cx);
        self.select(Catalog::Scheme, settings.terminal.scheme.clone(), cx);
        self.titlebar = settings.window.titlebar;
        self.language = settings.language.clone();
        self.background_blur = settings.window.background_blur;
        self.copy_on_select = settings.terminal.copy_on_select;
        self.font_family = settings
            .terminal
            .font_family
            .clone()
            .map(SharedString::from);

        let percent = (settings.window.background_opacity * 100.0).round() as i32;
        set_text(&self.opacity_input, percent.to_string(), cx);
        set_text(
            &self.font_size_input,
            format_number(settings.terminal.font_size),
            cx,
        );
        set_text(
            &self.scrollback_input,
            settings.terminal.scrollback_lines.to_string(),
            cx,
        );
        set_text(&self.term_input, settings.terminal.term.clone(), cx);
        set_text(
            &self.port_input,
            settings.connection.default_port.to_string(),
            cx,
        );
        set_text(
            &self.username_input,
            settings
                .connection
                .default_username
                .clone()
                .unwrap_or_default(),
            cx,
        );
        set_text(
            &self.keepalive_input,
            settings.connection.keepalive_secs.to_string(),
            cx,
        );
        set_text(
            &self.timeout_input,
            settings.connection.connect_timeout_secs.to_string(),
            cx,
        );
    }

    /// Assemble the form into settings, starting from the snapshot the dialog
    /// opened with so untouched fields survive.
    ///
    /// A field the user emptied or made unparseable keeps the value it had when
    /// the dialog opened; nothing here clamps, because
    /// [`AppSettings::sanitize`] does that once for the whole struct.
    fn collect(&self, cx: &App) -> AppSettings {
        let mut settings = self.base.clone();

        settings.ui_theme = self.ui_theme.to_string();
        settings.language = self.language.clone();
        settings.window.titlebar = self.titlebar;
        settings.window.background_blur = self.background_blur;
        if let Some(percent) = parse_number::<f32>(&self.opacity_input, cx) {
            settings.window.background_opacity = percent / 100.0;
        }

        settings.terminal.scheme = self.scheme.to_string();
        settings.terminal.font_family = self.font_family.as_ref().map(ToString::to_string);
        settings.terminal.copy_on_select = self.copy_on_select;
        if let Some(size) = parse_number::<f32>(&self.font_size_input, cx) {
            settings.terminal.font_size = size;
        }
        if let Some(lines) = parse_number::<usize>(&self.scrollback_input, cx) {
            settings.terminal.scrollback_lines = lines;
        }
        let term = text(&self.term_input, cx);
        if !term.is_empty() {
            settings.terminal.term = term;
        }

        if let Some(port) = parse_number::<u16>(&self.port_input, cx) {
            settings.connection.default_port = port;
        }
        settings.connection.default_username = optional_text(&self.username_input, cx);
        if let Some(secs) = parse_number::<u64>(&self.keepalive_input, cx) {
            settings.connection.keepalive_secs = secs;
        }
        if let Some(secs) = parse_number::<u64>(&self.timeout_input, cx) {
            settings.connection.connect_timeout_secs = secs;
        }

        settings
    }

    /// Persist the form and apply it, or report why it could not be written.
    ///
    /// A failed write leaves the dialog open with the message showing, so the
    /// user never believes a setting took effect when it did not.
    fn save(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.collect(cx);
        settings.sanitize();

        if let Err(err) = settings.save() {
            log::error!("could not write settings.json: {err:#}");
            self.status = Some(ts!("settings.save_failed", error = format!("{err:#}")));
            // Show the clamped values so the user sees what would be stored.
            self.fill_form(&settings, cx);
            cx.notify();
            return;
        }

        app_settings::replace(settings, cx);
        cx.emit(SettingsDialogEvent::Applied);
        self.close(cx);
    }

    /// Close the dialog and report that nothing was saved.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(SettingsDialogEvent::Dismissed);
        self.close(cx);
    }

    /// `Tab`: move focus to the next control. gpui's tab ring wraps on its own.
    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        self.close_lists(cx);
        window.focus_next(cx);
        self.reveal_focused(window, cx);
    }

    /// `Shift+Tab`: move focus to the previous control, wrapping to the last.
    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.close_lists(cx);
        window.focus_prev(cx);
        self.reveal_focused(window, cx);
    }

    /// Scroll the section holding the focused control into view.
    ///
    /// Without this a focus ring below the fold would be invisible, which is
    /// the same as having no focus indicator at all. The section is derived
    /// from the focused handle's tab index, so no per-control bookkeeping is
    /// needed for the controls whose focus handles gpui creates itself.
    ///
    /// Silent while the editor is up: the tab indices then belong to *its*
    /// ring, and reading them as sections would scroll a form nobody can see
    /// to wherever the editor's last field happened to land.
    fn reveal_focused(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.editor.is_some() {
            return;
        }
        let Some(handle) = window.focused(cx) else {
            return;
        };
        let section = match handle.tab_index {
            index if index <= tab::BLUR => 0,
            index if index <= tab::COPY_ON_SELECT => 1,
            _ => 2,
        };
        if section != self.visible_section {
            self.visible_section = section;
            self.body_scroll.scroll_to_item(section);
            cx.notify();
        }
    }

    /// `Escape` dismisses the dialog from anywhere inside it.
    ///
    /// Anything layered on top of the form takes the key first and only undoes
    /// itself, so that backing out of a list, a question or the colour editor
    /// does not also throw away the whole form. The editor comes first because
    /// it replaces the form outright: while it is up there is no list to close.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.open || event.keystroke.key != "escape" {
            return;
        }
        cx.stop_propagation();
        if let Some(editor) = self.editor.clone() {
            editor.update(cx, |editor, cx| editor.cancel(cx));
            return;
        }
        if self.open_list.is_some() {
            self.close_lists(cx);
            return;
        }
        self.dismiss(cx);
    }

    /// Hide whichever dropdown list is showing.
    ///
    /// Called whenever focus leaves a dropdown, so that a list nobody is
    /// driving any more does not stay painted over the rest of the form.
    fn close_lists(&mut self, cx: &mut Context<Self>) {
        if self.open_list.take().is_some() {
            cx.notify();
        }
    }

    /// The entries of the font dropdown: the "leave it to the OS" row first,
    /// then every installed family.
    ///
    /// A saved font that is not installed — a hand-edited `settings.json`, or a
    /// family that has since been removed — is spliced in after the first row,
    /// so the trigger keeps showing it instead of silently falling back.
    fn font_options(&self) -> Vec<SharedString> {
        let mut options = Vec::with_capacity(self.fonts.len() + 2);
        options.push(system_default());
        options.extend(
            self.font_family
                .clone()
                .filter(|family| !self.fonts.contains(family)),
        );
        options.extend(self.fonts.iter().cloned());
        options
    }

    /// The entries of the language dropdown: "follow the system" first, then
    /// every shipped translation named in its own language.
    fn language_options() -> Vec<SharedString> {
        let supported = i18n::supported();
        let mut options = Vec::with_capacity(supported.len() + 1);
        options.push(system_default());
        options.extend(supported.iter().map(|(_, name)| name.clone()));
        options
    }

    /// Show or hide `list`, revealing the current entry as it opens.
    ///
    /// Opening one list closes the other, since both are drawn deferred and
    /// two open at once would paint over each other.
    fn set_list_open(&mut self, list: OpenList, open: bool, cx: &mut Context<Self>) {
        self.open_list = open.then_some(list);
        if open {
            let (scroll, current) = match list {
                OpenList::Font => {
                    let options = self.font_options();
                    let current = self
                        .font_family
                        .as_ref()
                        .and_then(|family| options.iter().position(|option| option == family));
                    (&self.font_scroll, current)
                }
                OpenList::Language => (&self.language_scroll, self.language_index()),
                // Asked of the catalogues rather than of the swatches the list
                // is drawn from: the two are built from the same entries in the
                // same order, and this way the colors of every scheme are not
                // resolved twice over just to find one row.
                OpenList::UiTheme => {
                    let selected: &str = &self.ui_theme;
                    let current = ThemeRegistry::all(cx)
                        .iter()
                        .position(|entry| entry.id == selected);
                    (&self.ui_theme_scroll, current)
                }
                OpenList::Scheme => {
                    let selected: &str = &self.scheme;
                    let current = TerminalTheme::all_schemes()
                        .iter()
                        .position(|entry| entry.id == selected);
                    (&self.scheme_scroll, current)
                }
            };
            scroll.scroll_to_item(current.unwrap_or(0));
        }
        cx.notify();
    }

    /// Position of the selected language in [`Self::language_options`], or
    /// `None` while the language follows the system — or names a tag rulogman
    /// has no translation for, which the app treats the same way.
    fn language_index(&self) -> Option<usize> {
        let tag = self.language.as_deref()?;
        let index = i18n::supported()
            .iter()
            .position(|(code, _)| *code == tag)?;
        Some(index + 1)
    }

    /// Move focus into the first control when the dialog opens.
    ///
    /// Skipped while an editor is up: the editor moves focus into its own name
    /// field, and two views claiming the focus in one frame would leave it
    /// wherever the second one happened to run.
    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pending_focus || self.editor.is_some() {
            return;
        }
        self.pending_focus = false;
        let handle = self.opacity_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// The "Appearance" section.
    fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();
        let language_bar = self.hovering_scrollbar(SCROLLBARS[2].0, Surface::Language, cx);
        let theme_bar = self.hovering_scrollbar(SCROLLBARS[3].0, Surface::UiTheme, cx);
        // Built before the section is assembled, because `section` borrows the
        // context to read the theme and this borrows it mutably to listen.
        // A view of its own, drawn under the picker it manages; the tab
        // indices it takes start at `tab::UI_THEME_ACTIONS` and were fixed at
        // construction.
        let theme_actions = self.ui_theme_actions.clone();

        let theme_picker = SchemeSelect::new("settings-ui-theme")
            .options(ui_theme_swatches(cx))
            .selected(Some(self.ui_theme.clone()))
            .open(self.open_list == Some(OpenList::UiTheme))
            .tab_index(tab::UI_THEME)
            .scroll_handle(self.ui_theme_scroll.clone())
            .scrollbar(theme_bar)
            .on_select({
                let this = this.clone();
                move |id, _window, cx| {
                    let id = SharedString::from(id.to_owned());
                    this.update(cx, |dialog, cx| {
                        // A scheme answering to the same id follows along, so
                        // picking "Dracula" up here dresses the terminal to
                        // match in one gesture. One-way on purpose: the scheme
                        // picker below never touches the UI theme, so the
                        // terminal can still be recolored independently.
                        if TerminalTheme::by_name(&id).is_some() {
                            dialog.scheme = id.clone();
                        }
                        dialog.ui_theme = id;
                        cx.notify();
                    });
                }
            })
            .on_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_list_open(OpenList::UiTheme, open, cx);
                    });
                }
            });

        let titlebar_picker = Segmented::new("settings-titlebar")
            .options(titlebar_options())
            .selected(match self.titlebar {
                TitlebarStyle::Custom => 0,
                TitlebarStyle::System => 1,
            })
            .tab_index(tab::TITLEBAR)
            .on_select({
                let this = this.clone();
                move |index, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.titlebar = if index == 1 {
                            TitlebarStyle::System
                        } else {
                            TitlebarStyle::Custom
                        };
                        cx.notify();
                    });
                }
            });

        let language = Select::new("settings-language")
            .options(Self::language_options())
            .selected(self.language.as_deref().and_then(i18n::display_name))
            .placeholder(system_default())
            .open(self.open_list == Some(OpenList::Language))
            .tab_index(tab::LANGUAGE)
            .scroll_handle(self.language_scroll.clone())
            .scrollbar(language_bar)
            .on_select({
                let this = this.clone();
                // By index, not by label: row 0 is "follow the system" and the
                // rest line up with `i18n::supported`, whereas the labels are
                // endonyms that say nothing about their position.
                move |index, _label, _window, cx| {
                    let tag = index
                        .checked_sub(1)
                        .and_then(|index| i18n::supported().get(index))
                        .map(|(code, _)| (*code).to_owned());
                    this.update(cx, |dialog, cx| {
                        dialog.language = tag;
                        cx.notify();
                    });
                }
            })
            .on_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_list_open(OpenList::Language, open, cx);
                    });
                }
            });

        let blur = Checkbox::new("settings-blur", ts!("settings.blur"))
            .checked(self.background_blur)
            .tab_index(tab::BLUR)
            .on_toggle({
                let this = this.clone();
                move |checked, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.background_blur = checked;
                        cx.notify();
                    });
                }
            });

        section(
            ts!("settings.section.appearance"),
            cx,
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(form_row(
                    ts!("settings.ui_theme"),
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(theme_picker)
                        .child(theme_actions),
                ))
                .child(form_row(ts!("settings.titlebar"), titlebar_picker))
                .child(form_row(ts!("settings.language"), language))
                .child(form_row(
                    ts!("settings.opacity"),
                    suffixed(self.opacity_input.clone(), ts!("settings.opacity_hint"), cx),
                ))
                .child(form_row("", blur)),
        )
    }

    /// The "Terminal" section.
    fn render_terminal(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let font_bar = self.hovering_scrollbar(SCROLLBARS[1].0, Surface::Font, cx);
        let scheme_bar = self.hovering_scrollbar(SCROLLBARS[4].0, Surface::Scheme, cx);
        let this = cx.entity();
        // Hoisted for the same reason the appearance section's row is.
        // As above, from `tab::SCHEME_ACTIONS`.
        let scheme_actions = self.scheme_actions.clone();

        let picker = SchemeSelect::new("settings-scheme")
            .options(scheme_swatches())
            .selected(Some(self.scheme.clone()))
            .open(self.open_list == Some(OpenList::Scheme))
            .tab_index(tab::SCHEME)
            .scroll_handle(self.scheme_scroll.clone())
            .scrollbar(scheme_bar)
            .on_select({
                let this = this.clone();
                move |id, _window, cx| {
                    let id = SharedString::from(id.to_owned());
                    this.update(cx, |dialog, cx| {
                        dialog.scheme = id;
                        cx.notify();
                    });
                }
            })
            .on_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_list_open(OpenList::Scheme, open, cx);
                    });
                }
            });

        let font = Select::new("settings-font")
            .options(self.font_options())
            .selected(self.font_family.clone())
            .placeholder(system_default())
            .open(self.open_list == Some(OpenList::Font))
            .tab_index(tab::FONT_FAMILY)
            .scroll_handle(self.font_scroll.clone())
            .scrollbar(font_bar)
            .on_select({
                let this = this.clone();
                // Row 0 is the "leave it to the OS" entry; comparing its label
                // against the picked text would only work in one language.
                move |index, family, _window, cx| {
                    let family = (index > 0).then(|| SharedString::from(family.to_owned()));
                    this.update(cx, |dialog, cx| {
                        dialog.font_family = family;
                        cx.notify();
                    });
                }
            })
            .on_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_list_open(OpenList::Font, open, cx);
                    });
                }
            });

        let copy_on_select =
            Checkbox::new("settings-copy-on-select", ts!("settings.copy_on_select"))
                .checked(self.copy_on_select)
                .tab_index(tab::COPY_ON_SELECT)
                .on_toggle({
                    let this = this.clone();
                    move |checked, _window, cx| {
                        this.update(cx, |dialog, cx| {
                            dialog.copy_on_select = checked;
                            cx.notify();
                        });
                    }
                });

        section(
            ts!("settings.section.terminal"),
            cx,
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(form_row(
                    ts!("settings.scheme"),
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(picker)
                        .child(scheme_actions),
                ))
                .child(form_row(ts!("settings.font"), font))
                .child(form_row(
                    ts!("settings.font_size"),
                    suffixed(
                        self.font_size_input.clone(),
                        ts!("settings.font_size_hint"),
                        cx,
                    ),
                ))
                .child(form_row(
                    ts!("settings.scrollback"),
                    suffixed(
                        self.scrollback_input.clone(),
                        ts!("settings.scrollback_hint"),
                        cx,
                    ),
                ))
                .child(form_row(ts!("settings.term"), self.term_input.clone()))
                .child(form_row("", copy_on_select)),
        )
    }

    /// The "New connections" section.
    fn render_connection(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        section(
            ts!("settings.section.connection"),
            cx,
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(form_row(ts!("settings.port"), self.port_input.clone()))
                .child(form_row(
                    ts!("settings.username"),
                    self.username_input.clone(),
                ))
                .child(form_row(
                    ts!("settings.keepalive"),
                    suffixed(
                        self.keepalive_input.clone(),
                        ts!("settings.keepalive_hint"),
                        cx,
                    ),
                ))
                .child(form_row(
                    ts!("settings.timeout"),
                    suffixed(self.timeout_input.clone(), ts!("settings.timeout_hint"), cx),
                )),
        )
    }

    /// The scrolling form and the footer under it — the dialog's own body.
    ///
    /// Takes the body's overlay bar and the resolved theme rather than fetching
    /// them, because the caller has already had to work both out to decide
    /// whether this is what the modal is showing at all.
    fn render_form(
        &self,
        body_bar: Scrollbar,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // The `min_h_0` chain lets the scroll area shrink below its cap when
        // the modal hits the window height, keeping the footer on screen.
        div()
            .flex()
            .flex_col()
            .min_h_0()
            .gap(px(12.))
            .child(
                // The middle box exists only to hold the overlay bar: a
                // scrolling box cannot, because its children are what scroll
                // away underneath it.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(
                        div()
                            .id("settings-body")
                            .track_scroll(&self.body_scroll)
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .gap(px(14.))
                            .max_h(px(BODY_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .restrict_scroll_to_axis()
                            .child(self.render_appearance(cx))
                            .child(self.render_terminal(cx))
                            .child(self.render_connection(cx)),
                    )
                    .children(body_bar.render(theme)),
            )
            .child(self.render_footer(cx))
    }

    /// The message strip and the action buttons.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();

        let status = self.status.clone().map(|message| {
            div()
                .text_size(px(12.))
                .text_color(StatusLevel::Error.color(&theme))
                .child(message)
        });

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(10.))
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .children(status)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("settings-cancel", ts!("common.cancel"))
                            .variant(ButtonVariant::Secondary)
                            .tab_index(tab::CANCEL)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.dismiss(cx));
                                }
                            }),
                    )
                    .child(
                        Button::new("settings-save", ts!("common.save"))
                            .variant(ButtonVariant::Primary)
                            .tab_index(tab::SAVE)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.save(cx));
                                }
                            }),
                    ),
            )
    }
}

impl EventEmitter<SettingsDialogEvent> for SettingsDialog {}

impl Focusable for SettingsDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().id("settings-dialog");
        }

        self.apply_pending_focus(window, cx);
        self.watch_scroll(cx);
        let theme = theme(cx);
        let body_bar = self.hovering_scrollbar(SCROLLBARS[0].0, Surface::Body, cx);

        // While a colour is being edited the form steps aside entirely rather
        // than being covered up, so that the window's tab ring holds only the
        // controls that are actually on screen; see [`ruui_shell::theme_editor`].
        // The form is not even built in that case — it would be built afresh on
        // every keystroke in the editor and thrown away again.
        let (title, body) = match self.editor.clone() {
            Some(editor) => (editor.read(cx).title(cx), editor.into_any_element()),
            None => (
                ts!("settings.title"),
                self.render_form(body_bar, &theme, cx).into_any_element(),
            ),
        };

        // A click on the backdrop backs out of whatever is in front: the editor
        // while one is open, otherwise the dialog itself. Anything else would
        // discard an unsaved palette by way of a stray click.
        let on_dismiss = {
            let this = cx.entity();
            move |_window: &mut Window, cx: &mut App| {
                this.update(cx, |dialog, cx| match dialog.editor.clone() {
                    Some(editor) => editor.update(cx, |editor, cx| editor.cancel(cx)),
                    None => dialog.dismiss(cx),
                });
            }
        };

        // Absolute and full-size for the same reason as the connection dialog:
        // an absolutely positioned child is laid out against its direct parent.
        div()
            .id("settings-dialog")
            .key_context(KEY_CONTEXT)
            .absolute()
            .inset_0()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_key_down(cx.listener(Self::on_key_down))
            // Every overlay bar is answered from here: gpui hands a drag move
            // to every listener of that type wherever it sits, and this is the
            // one element mounted for the whole of any of them — the open list
            // a thumb belongs to is torn down the moment the pointer picks an
            // option, and the body scrolls away under its own.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |dialog, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    dialog.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbars(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbars(cx);
                }),
            )
            .child(modal(
                "settings-modal",
                title,
                px(DIALOG_WIDTH),
                body,
                on_dismiss,
            ))
    }
}

/// Trimmed content of `input`, or `None` when it is blank.
///
/// The one field helper that is not [`ruui_shell::form`]'s: only rulogman has a
/// settings field whose empty state is a meaningful `None` — the default login
/// name, where blank means "do not offer one".
fn optional_text(input: &Entity<TextInput>, cx: &App) -> Option<String> {
    let value = text(input, cx);
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_the_management_rows_ask_for_has_a_translation() {
        // The shell looks its words up by key as it draws, so a key that is not
        // in `locales/*.yml` reaches the screen as the key path and nothing else
        // would notice. The rows and the editor are the shell's; the files are
        // ours, which is why the assertion is here.
        let translated = |label: SharedString| {
            assert!(!label.is_empty(), "empty label");
            assert!(!label.contains("settings."), "untranslated label {label:?}");
        };

        for key in [
            "settings.manage.duplicate",
            "settings.manage.edit",
            "settings.manage.delete",
            "settings.manage.import",
            "settings.manage.export",
            "settings.manage.import_select",
            "settings.editor.theme_title",
            "settings.editor.scheme_title",
            "settings.editor.name",
            "settings.editor.dark",
            "settings.editor.invalid",
            "settings.editor.automatic",
            "settings.editor.grid_group",
        ] {
            translated(ts!(key));
        }
        translated(ts!("settings.manage.import_skipped", count = 2));
        translated(ts!(
            "settings.manage.import_unreadable",
            file = "f",
            error = "e"
        ));
        translated(ts!(
            "settings.manage.import_bad_color",
            file = "f",
            slot = "s"
        ));
        translated(ts!("settings.manage.import_not_a_theme", file = "f"));
        translated(ts!("settings.manage.import_not_a_scheme", file = "f"));
        translated(ts!("settings.manage.delete_theme_confirm", name = "X"));
        translated(ts!("settings.manage.delete_scheme_confirm", name = "X"));
        translated(ts!("settings.manage.write_failed", error = "e"));
        translated(ts!("settings.manage.delete_failed", error = "e"));
        translated(ts!("settings.editor.automatic_slot", name = "Accent"));

        // The copy's name has to carry the original's, or duplicating twice
        // would produce two entries that read identically.
        let copy = ts!("settings.manage.copy_name", name = "One Dark");
        assert!(copy.contains("One Dark"), "{copy:?}");
        assert_ne!(copy, "One Dark");
    }

    #[test]
    fn every_slot_of_both_catalogues_has_a_label_to_draw() {
        // The editor looks a slot's label up by key as it draws, so a key that
        // is not in `locales/*.yml` reaches the screen as the key path. The
        // chrome catalogue's slots are the shell's — including the five grid
        // ones rulogman has nowhere to draw but still offers — and the scheme's
        // are `crate::scheme_catalog`'s; the files are ours either way.
        let ui = UiThemeCatalog::new(theme_store::theme_dirs_or_empty(), "one-dark");
        let scheme = SchemeCatalog;
        for slot in ui.slots().iter().chain(scheme.slots()) {
            let label = ts!(slot.label_key);
            assert!(!label.is_empty(), "{} has an empty label", slot.key);
            assert!(
                !label.contains("settings."),
                "{} is untranslated: {label:?}",
                slot.key
            );
        }
    }

    #[test]
    fn the_two_management_rows_never_share_a_tab_index() {
        // A row takes `CatalogActions::TAB_SPAN` consecutive indices from the
        // base it was built with, whether or not it is currently asking
        // anything, and has to stay clear of the control that follows it.
        let last = |base: isize| base + CatalogActions::TAB_SPAN - 1;
        assert!(last(tab::UI_THEME_ACTIONS) < tab::TITLEBAR);
        assert!(last(tab::SCHEME_ACTIONS) < tab::FONT_FAMILY);
        // Each row follows the picker it belongs to.
        const { assert!(tab::UI_THEME < tab::UI_THEME_ACTIONS) };
        const { assert!(tab::SCHEME < tab::SCHEME_ACTIONS) };
    }

    #[test]
    fn a_blank_default_username_is_an_absence_rather_than_an_empty_name() {
        // The one form helper that stayed behind is `optional_text`, and this
        // is why: `default_username` is an `Option<String>`, so a blank field
        // has to come back as `None` and agree with the default.
        assert_eq!(AppSettings::default().connection.default_username, None);
    }
}
