//! Editing one UI theme or one terminal color scheme, colour by colour.
//!
//! # Where the editor is drawn
//!
//! Not as a modal of its own. [`crate::settings_dialog::SettingsDialog`] is
//! already a modal, and stacking a second one on top of it would leave the
//! form underneath rendered — which is to say still in the window's tab ring,
//! so `Tab` would walk out of the editor and into controls nobody can see. The
//! settings dialog therefore swaps its *body* for this view while an editor is
//! open: one modal, one set of tab stops, and `Escape` has a single obvious
//! meaning at every moment. The view returned by [`ThemeEditor`]'s `Render` is
//! consequently a plain panel, not a dialog; the frame around it belongs to the
//! settings dialog.
//!
//! # What it edits
//!
//! One component for both catalogues, because they differ only in which slots
//! they carry: a UI theme is a name, a dark/light flag and the eleven slots of
//! [`ThemeColors`], and a scheme is a name, four terminal roles and the sixteen
//! ANSI colours. Everything else — the hex fields, the live preview, the
//! refusal of a malformed colour, saving under an id that never changes — is
//! the same work, and [`Catalog`] is the one place the two part ways.

use std::path::{Path, PathBuf};

use anyhow::Result;
use gpui::{
    App, Context, Div, DragMoveEvent, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    IntoElement, MouseButton, MouseUpEvent, Render, ScrollHandle, SharedString, Window, div,
    prelude::*, px,
};
use rulogman_core::AppSettings;
use rulogman_term::{Rgb, SchemeFile, TerminalTheme};
use serde::Serialize;

use crate::i18n::{input_menu_labels, ts};
use crate::theme_store;
use ruui::{
    Button, ButtonVariant, Checkbox, DraggedThumb, Scrollbar, ScrollbarAxis, ScrollbarState,
    TextInput, ThemeColors, ThemeFile, ThemeRegistry, form_row, hide_later, hide_now, parse_hex,
    scroll_to, scrolled, theme,
};

/// Element id of the editor's overlay scroll indicator.
const SCROLLBAR_ID: &str = "theme-editor-scrollbar";

/// Height at which the editor's field list starts scrolling.
///
/// The same cap the settings form uses, so the modal keeps its size as the
/// dialog swaps one body for the other.
const BODY_MAX_HEIGHT: f32 = 520.;

/// Colour fields per row.
///
/// Two, for both catalogues: at the dialog's width a row of two leaves each
/// label enough room to be read in every language, and it turns the sixteen
/// ANSI slots into eight tidy rows of normal-then-bright pairs.
const FIELD_COLUMNS: usize = 2;

/// Width of a colour field's label, in pixels.
const LABEL_WIDTH: f32 = 118.;

/// Side of the swatch drawn beside a colour field.
const SWATCH_SIZE: f32 = 26.;

/// Tab order inside the editor, spaced so slots can be inserted later.
///
/// A ring of its own rather than a continuation of the settings form's: while
/// the editor is open the form is not rendered at all, so there is nothing for
/// these indices to collide with.
mod tab {
    /// The name field.
    pub const NAME: isize = 10;
    /// The dark/light checkbox, on a UI theme.
    pub const DARK: isize = 20;
    /// The first colour field; the rest follow one after another.
    pub const FIRST_COLOR: isize = 30;
    /// Cancel. Far enough past the colours that no catalogue can reach it.
    pub const CANCEL: isize = 900;
    /// Save.
    pub const SAVE: isize = 910;
}

/// Which of the two colour catalogues is meant.
///
/// The catalogues are parallel in every way that matters to the settings
/// dialog — each is a list of built-in entries plus a directory of files, each
/// is picked from a grid of cards — so every management action is written once
/// against this enum instead of twice against the two registries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    /// The chrome themes of [`ThemeRegistry`].
    UiTheme,
    /// The terminal colour schemes of [`TerminalTheme`].
    Scheme,
}

/// One entry of a catalogue, as the management row needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Stable id, which is also the stem of the file a custom entry lives in.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether the entry ships with rulogman rather than coming from a file.
    pub builtin: bool,
}

/// One theme or scheme file, whichever the catalogue holds.
///
/// Untagged on the way out, so the JSON written is exactly the JSON the
/// corresponding loader reads — a theme file, or the Windows Terminal scheme
/// file [`SchemeFile`] mirrors — with no wrapper of rulogman's own.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CatalogFile {
    /// A UI theme.
    UiTheme(Box<ThemeFile>),
    /// A terminal colour scheme.
    Scheme(Box<SchemeFile>),
}

impl CatalogFile {
    /// The name the file carries.
    pub fn name(&self) -> &str {
        match self {
            Self::UiTheme(file) => &file.name,
            Self::Scheme(file) => &file.name,
        }
    }

    /// Replaces the name the file carries.
    pub fn set_name(&mut self, name: impl Into<String>) {
        match self {
            Self::UiTheme(file) => file.name = name.into(),
            Self::Scheme(file) => file.name = name.into(),
        }
    }

    /// Which catalogue the file belongs to.
    pub fn catalog(&self) -> Catalog {
        match self {
            Self::UiTheme(_) => Catalog::UiTheme,
            Self::Scheme(_) => Catalog::Scheme,
        }
    }

    /// Writes the file into the configuration directory under `id`.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`theme_store::save_ui_theme`] does: an unusable
    /// id, one belonging to a built-in entry, or a write that does not go
    /// through.
    pub fn save(&self, id: &str) -> Result<PathBuf> {
        match self {
            Self::UiTheme(file) => theme_store::save_ui_theme(id, file),
            Self::Scheme(file) => theme_store::save_scheme(id, file),
        }
    }
}

impl Catalog {
    /// Every entry, the built-in ones first and then the user's own.
    pub fn entries(self, cx: &App) -> Vec<CatalogEntry> {
        match self {
            Self::UiTheme => ThemeRegistry::all(cx)
                .into_iter()
                .map(|entry| CatalogEntry {
                    id: entry.id,
                    name: entry.name,
                    builtin: entry.builtin,
                })
                .collect(),
            Self::Scheme => TerminalTheme::all_schemes()
                .into_iter()
                .map(|entry| CatalogEntry {
                    id: entry.id,
                    name: entry.name,
                    builtin: entry.builtin,
                })
                .collect(),
        }
    }

    /// The entry `id` names, or `None` when nothing answers to it.
    pub fn entry(self, id: &str, cx: &App) -> Option<CatalogEntry> {
        self.entries(cx)
            .into_iter()
            .find(|entry| entry.id.eq_ignore_ascii_case(id))
    }

    /// Every id already spoken for, which is what a new one has to dodge.
    pub fn taken_ids(self, cx: &App) -> Vec<String> {
        self.entries(cx).into_iter().map(|entry| entry.id).collect()
    }

    /// Prefix of the ids made up for an entry whose name yields no slug.
    pub fn generated_id_prefix(self) -> &'static str {
        match self {
            Self::UiTheme => theme_store::GENERATED_THEME_ID,
            Self::Scheme => theme_store::GENERATED_SCHEME_ID,
        }
    }

    /// The directory this catalogue's files live in.
    ///
    /// # Errors
    ///
    /// Fails when no home directory can be determined for the current user.
    pub fn directory(self) -> Result<PathBuf> {
        match self {
            Self::UiTheme => rulogman_core::ui_themes_dir(),
            Self::Scheme => rulogman_core::schemes_dir(),
        }
    }

    /// The id selected when the one in hand has just been deleted.
    pub fn default_id(self) -> String {
        let defaults = AppSettings::default();
        match self {
            Self::UiTheme => defaults.ui_theme,
            Self::Scheme => defaults.terminal.scheme,
        }
    }

    /// Removes the file `id` lives in.
    ///
    /// # Errors
    ///
    /// Fails when `id` has no usable slug or the file cannot be removed.
    pub fn delete(self, id: &str) -> Result<()> {
        match self {
            Self::UiTheme => theme_store::delete_ui_theme(id),
            Self::Scheme => theme_store::delete_scheme(id),
        }
    }

    /// Reads one file of this catalogue from anywhere on the disk.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or is not a file of this kind.
    pub fn read(self, path: &Path) -> Result<CatalogFile> {
        Ok(match self {
            Self::UiTheme => CatalogFile::UiTheme(Box::new(theme_store::read_file(path)?)),
            Self::Scheme => CatalogFile::Scheme(Box::new(theme_store::read_file(path)?)),
        })
    }

    /// The file that would reproduce the entry `id` names.
    ///
    /// Resolved through the registries rather than read back off the disk, so a
    /// built-in entry — which has no file — exports and duplicates exactly like
    /// one of the user's own.
    pub fn file_for(self, id: &str, cx: &App) -> Option<CatalogFile> {
        let entry = self.entry(id, cx)?;
        Some(match self {
            Self::UiTheme => CatalogFile::UiTheme(Box::new(ThemeFile::from_theme(
                entry.name,
                &ThemeRegistry::resolve(id, cx),
            ))),
            Self::Scheme => CatalogFile::Scheme(Box::new(SchemeFile::from_theme(
                entry.name,
                &TerminalTheme::by_name_or_default(id),
            ))),
        })
    }

    /// Prefix of the element ids of this catalogue's management row.
    ///
    /// Static, and never translated: gpui element ids only have to be unique
    /// among their siblings, and the two rows are siblings within one form.
    pub fn element_prefix(self) -> &'static str {
        match self {
            Self::UiTheme => "settings-ui-theme-action",
            Self::Scheme => "settings-scheme-action",
        }
    }

    /// Heading shown over the editor while one of this catalogue's entries is
    /// being edited.
    pub fn editor_title(self) -> SharedString {
        match self {
            Self::UiTheme => ts!("settings.editor.theme_title"),
            Self::Scheme => ts!("settings.editor.scheme_title"),
        }
    }
}

/// One editable colour: what it is called, and what has been typed into it.
struct ColorField {
    /// Label shown to the left of the field.
    label: SharedString,
    /// Element id fragment; never translated.
    key: &'static str,
    /// Whether this slot accepts an `#RRGGBBAA` value as well as `#RRGGBB`.
    alpha: bool,
    /// The field itself.
    input: Entity<TextInput>,
}

/// Whether `value` is a colour the file format accepts.
///
/// Stricter than [`parse_hex`] on purpose: that helper takes an alpha channel
/// wherever it finds one, while only the UI theme's `overlay` slot is drawn
/// with one — and a scheme file, being Windows Terminal's, has no slot that is
/// anything but six digits.
fn valid_hex(value: &str, alpha: bool) -> bool {
    let trimmed = value.trim();
    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let length_ok = digits.len() == 6 || (alpha && digits.len() == 8);
    length_ok && parse_hex(trimmed).is_some()
}

/// The eleven UI theme slots, in the order [`ThemeColors`] declares them.
///
/// The order is load-bearing: [`ui_colors`] reads the fields back by position.
fn ui_slots() -> Vec<(&'static str, SharedString, bool)> {
    vec![
        ("background", ts!("settings.editor.slot.background"), false),
        ("surface", ts!("settings.editor.slot.surface"), false),
        (
            "surface_hover",
            ts!("settings.editor.slot.surface_hover"),
            false,
        ),
        (
            "surface_active",
            ts!("settings.editor.slot.surface_active"),
            false,
        ),
        ("border", ts!("settings.editor.slot.border"), false),
        ("text", ts!("settings.editor.slot.text"), false),
        ("text_muted", ts!("settings.editor.slot.text_muted"), false),
        ("accent", ts!("settings.editor.slot.accent"), false),
        ("danger", ts!("settings.editor.slot.danger"), false),
        ("success", ts!("settings.editor.slot.success"), false),
        // The one slot that is drawn translucent, and so the one that may
        // carry an eighth and ninth hex digit.
        ("overlay", ts!("settings.editor.slot.overlay"), true),
    ]
}

/// The current value of every UI theme slot, in [`ui_slots`] order.
fn ui_values(colors: &ThemeColors) -> Vec<String> {
    vec![
        colors.background.clone(),
        colors.surface.clone(),
        colors.surface_hover.clone(),
        colors.surface_active.clone(),
        colors.border.clone(),
        colors.text.clone(),
        colors.text_muted.clone(),
        colors.accent.clone(),
        colors.danger.clone(),
        colors.success.clone(),
        colors.overlay.clone(),
    ]
}

/// The slots of a UI theme, read back out of the fields in [`ui_slots`] order.
///
/// `carried` is the file the editor was opened on, and it is what the slots
/// this editor does not offer are taken from. A `ruui` theme file also colours
/// the result grid of the sibling database tools — a header row, an alternating
/// body row, a selection fill and two foregrounds — which rulogman has nowhere
/// to draw and so does not put a field on screen for. The same file is read by
/// those tools, though, so an edit made here has to hand those five values back
/// unchanged rather than strip them out of a theme the user shares.
fn ui_colors(values: &[String], carried: Option<&ThemeColors>) -> ThemeColors {
    let at = |index: usize| values.get(index).cloned().unwrap_or_default();
    ThemeColors {
        background: at(0),
        surface: at(1),
        surface_hover: at(2),
        surface_active: at(3),
        border: at(4),
        text: at(5),
        text_muted: at(6),
        accent: at(7),
        danger: at(8),
        success: at(9),
        overlay: at(10),
        grid_header: carried.and_then(|colors| colors.grid_header.clone()),
        grid_row_alt: carried.and_then(|colors| colors.grid_row_alt.clone()),
        grid_selection: carried.and_then(|colors| colors.grid_selection.clone()),
        grid_null: carried.and_then(|colors| colors.grid_null.clone()),
        grid_pk: carried.and_then(|colors| colors.grid_pk.clone()),
    }
}

/// The name of one ANSI colour and of its bright counterpart.
///
/// The bright half is interpolated rather than spelled out eight more times,
/// because the languages rulogman ships disagree about where the word goes —
/// "Bright red", "Rouge clair" — and an interpolated pattern is the only form
/// that survives that.
fn ansi_names() -> [(SharedString, SharedString); 8] {
    [
        ts!("settings.editor.term.black"),
        ts!("settings.editor.term.red"),
        ts!("settings.editor.term.green"),
        ts!("settings.editor.term.yellow"),
        ts!("settings.editor.term.blue"),
        ts!("settings.editor.term.magenta"),
        ts!("settings.editor.term.cyan"),
        ts!("settings.editor.term.white"),
    ]
    .map(|base| {
        let bright = ts!("settings.editor.term.bright", name = base.to_string());
        (base, bright)
    })
}

/// Element id fragments of the sixteen ANSI slots, in palette order.
const ANSI_KEYS: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

/// The twenty scheme slots: the four terminal roles, then the ANSI palette.
///
/// As with [`ui_slots`], the order is what [`scheme_file`] reads back.
fn scheme_slots() -> Vec<(&'static str, SharedString, bool)> {
    let mut slots = vec![
        ("foreground", ts!("settings.editor.term.foreground"), false),
        ("background", ts!("settings.editor.term.background"), false),
        ("cursor", ts!("settings.editor.term.cursor"), false),
        ("selection", ts!("settings.editor.term.selection"), false),
    ];

    let names = ansi_names();
    // Normal and bright are laid out one after the other so that the two
    // columns of the grid pair each colour with its own bright variant.
    for (index, key) in ANSI_KEYS.iter().enumerate() {
        let (normal, bright) = &names[index % 8];
        let label = if index < 8 { normal } else { bright };
        slots.push((*key, label.clone(), false));
    }
    slots
}

/// The current value of every scheme slot, in [`scheme_slots`] order.
fn scheme_values(file: &SchemeFile) -> Vec<String> {
    let theme = file.to_theme();
    let mut values = vec![
        file.foreground.clone(),
        file.background.clone(),
        file.cursor_color
            .clone()
            .unwrap_or_else(|| theme.cursor.to_hex()),
        file.selection_background
            .clone()
            .unwrap_or_else(|| theme.selection.to_hex()),
    ];
    values.extend([
        file.black.clone(),
        file.red.clone(),
        file.green.clone(),
        file.yellow.clone(),
        file.blue.clone(),
        file.purple.clone(),
        file.cyan.clone(),
        file.white.clone(),
        file.bright_black.clone(),
        file.bright_red.clone(),
        file.bright_green.clone(),
        file.bright_yellow.clone(),
        file.bright_blue.clone(),
        file.bright_purple.clone(),
        file.bright_cyan.clone(),
        file.bright_white.clone(),
    ]);
    values
}

/// A scheme file assembled from the fields, in [`scheme_slots`] order.
///
/// Both optional keys are written out, as [`SchemeFile::from_theme`] does: a
/// file rulogman saves says what it means rather than leaning on a derivation.
fn scheme_file(name: String, values: &[String]) -> SchemeFile {
    let at = |index: usize| values.get(index).cloned().unwrap_or_default();
    SchemeFile {
        name,
        foreground: at(0),
        background: at(1),
        cursor_color: Some(at(2)),
        selection_background: Some(at(3)),
        black: at(4),
        red: at(5),
        green: at(6),
        yellow: at(7),
        blue: at(8),
        purple: at(9),
        cyan: at(10),
        white: at(11),
        bright_black: at(12),
        bright_red: at(13),
        bright_green: at(14),
        bright_yellow: at(15),
        bright_blue: at(16),
        bright_purple: at(17),
        bright_cyan: at(18),
        bright_white: at(19),
    }
}

/// Emitted by [`ThemeEditor`] when the user is done with it.
pub enum ThemeEditorEvent {
    /// The entry has been written and both registries reloaded. The host has to
    /// repaint whatever was already wearing it.
    Saved,
    /// The user backed out; nothing was written.
    Cancelled,
}

/// Editor for one UI theme or one terminal colour scheme.
///
/// Built with [`ThemeEditor::new`] from the file that is to be edited, rendered
/// as the body of the settings dialog, and finished by one of
/// [`ThemeEditorEvent`]'s two variants. The id it saves under is fixed at
/// construction and never follows the name: renaming a theme must not orphan
/// the settings entry — or the profile override — that selected it.
pub struct ThemeEditor {
    /// Which catalogue the entry belongs to.
    catalog: Catalog,
    /// The id it is saved under, from construction to save.
    id: String,
    /// The name, which is the only thing about it that is free text.
    name_input: Entity<TextInput>,
    /// Whether the palette is a dark one. Meaningless for a scheme, whose
    /// darkness [`TerminalTheme::is_dark`] works out from the background.
    dark: bool,
    /// One field per colour slot, in the catalogue's own order.
    fields: Vec<ColorField>,
    /// The UI theme this editor was opened on, kept for the slots that have no
    /// field of their own; see [`ui_colors`]. `None` for a colour scheme, whose
    /// file has no such slots.
    carried: Option<ThemeColors>,
    /// Why the last save did not go through, if it did not.
    status: Option<SharedString>,
    /// Focus of the editor root; the anchor the host's `Escape` handler sits on.
    focus_handle: FocusHandle,
    /// Whether focus should move into the name field on the next render.
    pending_focus: bool,
    /// Scroll position of the field list.
    scroll: ScrollHandle,
    /// Whether the field list's overlay scroll indicator is on screen.
    scrollbar: ScrollbarState,
}

impl ThemeEditor {
    /// Builds an editor over `file`, which will be saved back under `id`.
    pub fn new(id: impl Into<String>, file: &CatalogFile, cx: &mut Context<Self>) -> Self {
        let catalog = file.catalog();
        let carried = match file {
            CatalogFile::UiTheme(theme) => Some(theme.colors.clone()),
            CatalogFile::Scheme(_) => None,
        };
        let (slots, values, dark) = match file {
            CatalogFile::UiTheme(theme) => (ui_slots(), ui_values(&theme.colors), theme.dark),
            CatalogFile::Scheme(scheme) => (
                scheme_slots(),
                scheme_values(scheme),
                scheme.to_theme().is_dark(),
            ),
        };

        let name_input = cx.new(|cx| {
            let mut input = TextInput::new(cx)
                .context_menu(input_menu_labels)
                .tab_index(tab::NAME);
            input.set_content(file.name().to_owned(), cx);
            input
        });
        // The name is not validated, but it *is* previewed, so the editor has
        // to hear about it changing just as it hears about the colours.
        cx.observe(&name_input, |_editor, _input, cx| cx.notify())
            .detach();

        let mut fields = Vec::with_capacity(slots.len());
        for (index, (key, label, alpha)) in slots.into_iter().enumerate() {
            let value = values.get(index).cloned().unwrap_or_default();
            // Marked as it opens, not only once it is typed into: a file edited
            // by hand can arrive with a slot that is not a colour, and the
            // editor is exactly where that has to be visible.
            let valid = valid_hex(&value, alpha);
            let input = cx.new(|cx| {
                let mut input = TextInput::new(cx)
                    .context_menu(input_menu_labels)
                    .placeholder("#000000")
                    .tab_index(tab::FIRST_COLOR + index as isize);
                input.set_content(value, cx);
                input.set_invalid(!valid, cx);
                input
            });
            // gpui does not re-render a parent when a child entity notifies, so
            // without this the live preview would only follow the typing at the
            // next unrelated repaint — and the refusal of a malformed colour
            // would never appear at all.
            cx.observe(&input, |editor, _input, cx| editor.revalidate(cx))
                .detach();
            fields.push(ColorField {
                label,
                key,
                alpha,
                input,
            });
        }

        Self {
            catalog,
            id: id.into(),
            name_input,
            dark,
            fields,
            carried,
            status: None,
            focus_handle: cx.focus_handle(),
            pending_focus: true,
            scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
        }
    }

    /// Heading the host draws over the editor.
    pub fn title(&self) -> SharedString {
        self.catalog.editor_title()
    }

    /// Discards the edits and tells the host to put its own body back.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(ThemeEditorEvent::Cancelled);
    }

    /// Re-marks every field that does not hold a colour, and repaints.
    fn revalidate(&mut self, cx: &mut Context<Self>) {
        for field in &self.fields {
            let valid = valid_hex(field.input.read(cx).content(), field.alpha);
            field
                .input
                .update(cx, |input, cx| input.set_invalid(!valid, cx));
        }
        cx.notify();
    }

    /// Whether every field holds a colour the format accepts.
    fn is_valid(&self, cx: &App) -> bool {
        self.fields
            .iter()
            .all(|field| valid_hex(field.input.read(cx).content(), field.alpha))
    }

    /// What has been typed into every field, in the catalogue's own order.
    fn values(&self, cx: &App) -> Vec<String> {
        self.fields
            .iter()
            .map(|field| field.input.read(cx).content().trim().to_owned())
            .collect()
    }

    /// The file the fields currently describe.
    fn collect(&self, cx: &App) -> CatalogFile {
        let name = self.name_input.read(cx).content().trim().to_owned();
        let values = self.values(cx);
        match self.catalog {
            Catalog::UiTheme => CatalogFile::UiTheme(Box::new(ThemeFile::new(
                name,
                self.dark,
                ui_colors(&values, self.carried.as_ref()),
            ))),
            Catalog::Scheme => CatalogFile::Scheme(Box::new(scheme_file(name, &values))),
        }
    }

    /// Writes the edits and reloads both registries.
    ///
    /// A failed write leaves the editor open with the reason showing, so the
    /// user never believes a colour took effect when it did not.
    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.is_valid(cx) {
            self.status = Some(ts!("settings.editor.invalid"));
            cx.notify();
            return;
        }

        let file = self.collect(cx);
        if let Err(err) = file.save(&self.id) {
            log::error!("could not write the {} file: {err:#}", self.id);
            self.status = Some(ts!(
                "settings.manage.write_failed",
                error = format!("{err:#}")
            ));
            cx.notify();
            return;
        }

        theme_store::reload(cx);
        cx.emit(ThemeEditorEvent::Saved);
    }

    /// Moves focus into the name field the first time the editor is drawn.
    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pending_focus {
            return;
        }
        self.pending_focus = false;
        let handle = self.name_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// The overlay scroll indicator of the field list, as it stands.
    fn scrollbar(&self) -> Scrollbar {
        Scrollbar::for_handle(SCROLLBAR_ID, ScrollbarAxis::Vertical, &self.scroll)
            .fade(self.scrollbar.fade())
    }

    /// Puts the bar up whenever the list has been scrolled, and starts the
    /// clock that takes it down again.
    fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        let scrolled = scrolled(&self.scroll, ScrollbarAxis::Vertical);
        if let Some(epoch) = self.scrollbar.moved(scrolled) {
            hide_later(epoch, cx, |editor| Some(&mut editor.scrollbar));
        }
    }

    /// Scrolls the list while its thumb is dragged.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        let Some(progress) = self.scrollbar().dragged(event, cx) else {
            return;
        };
        self.scrollbar.hold();
        scroll_to(&self.scroll, ScrollbarAxis::Vertical, progress);
        cx.notify();
    }

    /// Lets go of the thumb, and starts its clock again.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) {
        if let Some(epoch) = self.scrollbar.release() {
            hide_later(epoch, cx, |editor| Some(&mut editor.scrollbar));
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
            hide_now(self, epoch, cx, |editor| Some(&mut editor.scrollbar));
        }
    }

    /// The colour a field currently describes, or `None` while it does not.
    fn color_of(&self, field: &ColorField, cx: &App) -> Option<Hsla> {
        let value = field.input.read(cx).content();
        valid_hex(value, field.alpha)
            .then(|| parse_hex(value))
            .flatten()
    }

    /// One labelled colour field: the slot's name, the hex value, the swatch.
    ///
    /// The swatch is what turns a hex value back into something a person can
    /// judge, and it doubles as the refusal: a field holding anything but a
    /// colour has nothing to draw, so the swatch shows an outline instead —
    /// next to the field, which is itself already outlined in the danger
    /// colour.
    fn render_field(&self, field: &ColorField, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let color = self.color_of(field, cx);

        let swatch = div()
            .flex_none()
            .size(px(SWATCH_SIZE))
            .rounded_md()
            .border_1()
            .border_color(match color {
                Some(_) => theme.border,
                None => theme.danger,
            })
            .when_some(color, |this, color| this.bg(color));

        div()
            // Named after the slot rather than numbered, so that the element
            // keeps its identity as the two catalogues swap field lists.
            .id(field.key)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .w(px(LABEL_WIDTH))
                    .truncate()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(field.label.clone()),
            )
            .child(div().flex_1().min_w_0().child(field.input.clone()))
            .child(swatch)
    }

    /// The colour fields, laid out [`FIELD_COLUMNS`] to a row.
    fn render_fields(&self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<Div> {
        let fields = &self.fields[range];
        fields
            .chunks(FIELD_COLUMNS)
            .map(|row| {
                let cells: Vec<_> = row
                    .iter()
                    .map(|field| self.render_field(field, cx).into_any_element())
                    .collect();
                // Pad a short last row so its fields keep the width they have
                // in every other row rather than stretching to fill it.
                let padding = (FIELD_COLUMNS - row.len()) % FIELD_COLUMNS;
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .gap(px(12.))
                    .children(cells)
                    .children((0..padding).map(|_| div().flex_1().min_w_0().into_any_element()))
            })
            .collect()
    }

    /// A miniature of the chrome the edited UI theme would draw.
    ///
    /// The colours a theme is actually judged by: a window background with a
    /// raised surface on it, primary and muted text, and a chip each for the
    /// accent and the two status colours.
    fn render_theme_preview(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let values = self.values(cx);
        let palette =
            ThemeFile::new("", self.dark, ui_colors(&values, self.carried.as_ref())).to_theme();
        let name = SharedString::from(self.name_input.read(cx).content().to_owned());

        let chip = |color: Hsla| div().flex_none().size(px(12.)).rounded_full().bg(color);

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .bg(palette.background)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded_md()
                    .bg(palette.surface)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(palette.text)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .size(px(14.))
                            .rounded_sm()
                            .bg(palette.surface_active),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(chip(palette.accent))
                    .child(chip(palette.success))
                    .child(chip(palette.danger))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(palette.text_muted)
                            .child("Aa Bb Cc 0123"),
                    ),
            )
    }

    /// The terminal surface the edited scheme would draw.
    ///
    /// Everything at once, because a scheme is judged as a whole: sample text
    /// over the background, a caret in the cursor colour, a run of selected
    /// text, and both halves of the ANSI palette as chips.
    fn render_scheme_preview(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let values = self.values(cx);
        let palette = scheme_file(String::new(), &values).to_theme();
        let color = |rgb: Rgb| -> Hsla { gpui::rgb(rgb.to_u32()).into() };

        let chips = |half: usize| {
            div()
                .flex()
                .flex_row()
                .gap(px(4.))
                .children((0..8).map(|index| {
                    div()
                        .flex_none()
                        .size(px(12.))
                        .rounded_full()
                        .bg(color(palette.ansi[half * 8 + index]))
                }))
        };

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(10.))
            .rounded_md()
            .bg(color(palette.background))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(color(palette.foreground))
                    .child("Aa Bb Cc 0123")
                    .child(
                        div()
                            .px(px(4.))
                            .rounded_sm()
                            .bg(color(palette.selection))
                            .child("selected"),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(8.))
                            .h(px(14.))
                            .rounded_sm()
                            .bg(color(palette.cursor)),
                    ),
            )
            .child(chips(0))
            .child(chips(1))
    }

    /// The message strip and the two buttons that end the editor.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let valid = self.is_valid(cx);

        // A refused colour explains itself the moment it is typed rather than
        // waiting for a Save that is already held back — otherwise the only
        // sign would be a greyed-out button with no reason attached.
        let status = self
            .status
            .clone()
            .or_else(|| (!valid).then(|| ts!("settings.editor.invalid")))
            .map(|message| {
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
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
                        Button::new("theme-editor-cancel", ts!("common.cancel"))
                            .variant(ButtonVariant::Secondary)
                            .tab_index(tab::CANCEL)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |editor, cx| editor.cancel(cx));
                                }
                            }),
                    )
                    .child(
                        Button::new("theme-editor-save", ts!("common.save"))
                            .variant(ButtonVariant::Primary)
                            .disabled(!valid)
                            .tab_index(tab::SAVE)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |editor, cx| editor.save(cx));
                                }
                            }),
                    ),
            )
    }
}

impl EventEmitter<ThemeEditorEvent> for ThemeEditor {}

impl Focusable for ThemeEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThemeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_pending_focus(window, cx);
        self.watch_scroll(cx);
        let theme = theme(cx);
        let bar = self
            .scrollbar()
            .on_hover(cx.listener(|editor, hovered: &bool, _window, cx| {
                editor.hover_scrollbar(*hovered, cx);
            }));

        let preview = match self.catalog {
            Catalog::UiTheme => self.render_theme_preview(cx).into_any_element(),
            Catalog::Scheme => self.render_scheme_preview(cx).into_any_element(),
        };

        let dark = (self.catalog == Catalog::UiTheme).then(|| {
            let this = cx.entity();
            Checkbox::new("theme-editor-dark", ts!("settings.editor.dark"))
                .checked(self.dark)
                .tab_index(tab::DARK)
                .on_toggle(move |checked, _window, cx| {
                    this.update(cx, |editor, cx| {
                        editor.dark = checked;
                        cx.notify();
                    });
                })
        });

        // A scheme's sixteen ANSI slots get a heading of their own: without it
        // the four terminal roles above them run straight into a wall of
        // colour names with nothing to say where one group ends.
        let (roles, ansi) = match self.catalog {
            Catalog::UiTheme => (0..self.fields.len(), None),
            Catalog::Scheme => (0..4, Some(4..self.fields.len())),
        };

        let list = div()
            .id("theme-editor-fields")
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .min_h_0()
            .gap(px(8.))
            .max_h(px(BODY_MAX_HEIGHT))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .child(preview)
            .child(form_row(
                ts!("settings.editor.name"),
                self.name_input.clone(),
            ))
            .children(dark.map(|dark| form_row("", dark)))
            .children(self.render_fields(roles, cx))
            .children(ansi.map(|ansi| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .pt(px(4.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(ts!("settings.editor.term.ansi")),
                    )
                    .children(self.render_fields(ansi, cx))
            }));

        div()
            .id("theme-editor")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .min_h_0()
            .gap(px(12.))
            .on_drag_move::<DraggedThumb>(cx.listener(
                |editor, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    editor.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|editor, _: &MouseUpEvent, _window, cx| {
                    editor.release_scrollbar(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|editor, _: &MouseUpEvent, _window, cx| {
                    editor.release_scrollbar(cx);
                }),
            )
            .child(
                // The middle box exists only to hold the overlay bar, as in the
                // settings form: a scrolling box cannot, because its children
                // are what scroll away underneath it.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(list)
                    .children(bar.render(&theme)),
            )
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ruui::Theme;

    #[test]
    fn every_label_the_editor_draws_has_a_translation() {
        // `t!` answers with the key path itself when no such key exists, so a
        // typo in one of the thirty-odd lookups above would reach the screen as
        // "settings.editor.slot.backgrund". Catching it here is cheaper than
        // opening the dialog in eight languages.
        let translated = |label: &SharedString| {
            assert!(!label.is_empty(), "empty label");
            assert!(!label.contains("settings."), "untranslated label {label:?}");
        };

        for (_, label, _) in ui_slots().into_iter().chain(scheme_slots()) {
            translated(&label);
        }
        for label in [
            ts!("settings.editor.name"),
            ts!("settings.editor.dark"),
            ts!("settings.editor.invalid"),
            ts!("settings.editor.term.ansi"),
            Catalog::UiTheme.editor_title(),
            Catalog::Scheme.editor_title(),
        ] {
            translated(&label);
        }

        // The bright half is interpolated, so it also has to have picked the
        // base colour's name up rather than left `%{name}` standing.
        for (base, bright) in ansi_names() {
            assert_ne!(base, bright);
            assert!(bright.contains(base.as_ref()), "{bright:?}");
        }
    }

    #[test]
    fn a_six_digit_colour_is_accepted_everywhere() {
        for value in ["#ff0000", "ff0000", "  #AABBCC  "] {
            assert!(valid_hex(value, false), "refused {value:?}");
            assert!(valid_hex(value, true), "refused {value:?}");
        }
    }

    #[test]
    fn only_a_slot_with_alpha_takes_eight_digits() {
        assert!(valid_hex("#0000009e", true));
        assert!(!valid_hex("#0000009e", false));
    }

    #[test]
    fn anything_that_is_not_a_colour_is_refused() {
        for value in ["", "#", "#abc", "#abcde", "#gghhii", "rebeccapurple"] {
            assert!(!valid_hex(value, false), "accepted {value:?}");
            assert!(!valid_hex(value, true), "accepted {value:?}");
        }
    }

    #[test]
    fn the_ui_slots_round_trip_through_the_fields() {
        let file = ThemeFile::from_theme("Mine", &Theme::solarized_light());
        let values = ui_values(&file.colors);
        assert_eq!(values.len(), ui_slots().len());
        // Including the grid slots the editor shows no field for: they come
        // back off the file the editor was opened on rather than out of a form.
        assert!(file.colors.grid_header.is_some());
        assert_eq!(ui_colors(&values, Some(&file.colors)), file.colors);
        // With no file behind them they are absent rather than invented, which
        // is what `ruui` reads as "derive one from the rest of the palette".
        assert!(ui_colors(&values, None).grid_header.is_none());
        // Every slot but `overlay` is opaque, and only `overlay` takes alpha.
        let alpha: Vec<&str> = ui_slots()
            .into_iter()
            .filter(|(_, _, alpha)| *alpha)
            .map(|(key, _, _)| key)
            .collect();
        assert_eq!(alpha, ["overlay"]);
    }

    #[test]
    fn the_scheme_slots_round_trip_through_the_fields() {
        let file = SchemeFile::from_theme("Mine", &TerminalTheme::gruvbox_dark());
        let values = scheme_values(&file);
        assert_eq!(values.len(), scheme_slots().len());
        assert_eq!(values.len(), 20);
        assert_eq!(scheme_file("Mine".to_string(), &values), file);
    }

    #[test]
    fn a_scheme_without_its_optional_keys_still_fills_every_field() {
        // The two derived slots are what a published Windows Terminal palette
        // most often leaves out, and the editor has to open with something in
        // them rather than with two empty fields.
        let mut file = SchemeFile::from_theme("Sparse", &TerminalTheme::dark());
        file.cursor_color = None;
        file.selection_background = None;

        let values = scheme_values(&file);
        assert!(values.iter().all(|value| valid_hex(value, false)));
        assert_eq!(values[2], file.to_theme().cursor.to_hex());
        assert_eq!(values[3], file.to_theme().selection.to_hex());
    }
}
