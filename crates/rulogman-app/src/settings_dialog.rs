//! The application settings dialog.
//!
//! Edits [`AppSettings`] and the dashboards, and nothing else: it reads the
//! current settings snapshot from [`crate::app_settings`] when it opens, writes
//! the edited copy to disk when the user saves, and replaces the global so the
//! rest of the app picks the change up. Range checking is deliberately *not*
//! duplicated here — the form collects whatever the user typed and
//! [`AppSettings::sanitize`] clamps it once on the way out, which keeps one
//! definition of "valid" in `rulogman-core`.
//!
//! The dashboards are the one thing here that is not a setting. They live in
//! this dialog because they are the same *kind* of thing to edit — a list the
//! user keeps, edited nowhere near a live session, applied by pressing Save —
//! and because the alternative was a second modal that would have said the same
//! Cancel and Save. They are persisted alongside the settings on Save and
//! re-read from disk on every opening, exactly as the settings are.

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Once},
};

use gpui::{
    App, Context, DragMoveEvent, ElementId, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseUpEvent, Render, ScrollHandle,
    SharedString, Subscription, Window, actions, div, prelude::*, px, rgb,
};
use std::collections::HashMap;

use rulogman_core::{
    AppSettings, Dashboard, DashboardPane, DashboardStore, HIGHLIGHT_COLOR_NAMES, LayoutNode,
    ProfileStore, TitlebarStyle, effective_highlights, highlight_preset, is_highlight_preset,
};
use rulogman_term::TerminalTheme;
use uuid::Uuid;

use crate::app_settings;
use crate::highlight_rules::{HighlightProblem, HighlightRuleList, collect_highlight_rules};
use crate::i18n::{self, input_menu_labels, ts};
use crate::icons;
use crate::questions::{self, Answer};
use crate::scheme_catalog::SchemeCatalog;
use crate::tail_view::{HEADER_COLORS, header_fill, header_hex, header_preset, header_swatches};
use crate::theme_store;
use rugpui::SmoothScrollExt;
use rugpui::{
    Button, ButtonVariant, Checkbox, Collapsible, ColorPicker, DraggedThumb, Hsv, SchemePreview,
    SchemeSelect, SchemeSwatch, Scrollbar, ScrollbarAxis, ScrollbarState, Segmented, Select,
    SelectOption, TextInput, Theme, ThemeRegistry, form_row, hide_later, hide_now, modal,
    scroll_to, scrolled, theme,
};
use rugpui_shell::form::{
    format_number, hint, installed_fonts, parse_number, restrict_to_number, section, set_text,
    suffixed, text,
};
use rugpui_shell::{
    CatalogActionEvent, CatalogActions, ThemeCatalog, ThemeEditor, ThemeEditorEvent, UiThemeCatalog,
};

/// Element id of the dialog body's overlay scroll indicator.
///
/// The drag listener on the dialog root tells this bar's drags from any other
/// by it. The dropdowns' lists run bars of their own.
const BODY_SCROLLBAR: &str = "settings-body-scrollbar";

/// Width of the dialog panel.
const DIALOG_WIDTH: f32 = 760.;

/// Height at which the form body starts scrolling.
const BODY_MAX_HEIGHT: f32 = 520.;

/// Width of the connection column in a dashboard's pane table.
///
/// Wide enough for a connection name of ordinary length; the path takes
/// whatever is left, since it is the value of a pane with no length limit.
const DASHBOARD_PROFILE_WIDTH: f32 = 200.;

/// Width of the header color column in a dashboard's pane table.
///
/// Enough for a swatch and the longest color name the translations give.
const DASHBOARD_COLOR_WIDTH: f32 = 170.;

/// Width of the action column at the end of a pane row.
const DASHBOARD_ACTION_WIDTH: f32 = 72.;

/// Element ids reserved for one dashboard's pane rows.
///
/// A pane row's controls are identified by `dashboard * this + pane`, which is
/// what keeps two dashboards' first rows from claiming the same id. Nothing
/// enforces the ceiling: a dashboard with a thousand panes would collide with
/// the next one's ids, and would have run out of tab indices long before.
const PANE_IDS_PER_DASHBOARD: usize = 1000;

/// The name a dashboard is saved under when the user left the field blank.
///
/// Deliberately not translated. It is written into `dashboards.json` and read
/// back as data, so a dashboard named in one language would keep that name
/// after the interface was switched to another — a stored name is the user's,
/// not the interface's, and only looks like a word by coincidence.
const DEFAULT_DASHBOARD_NAME: &str = "Dashboard";

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
    /// Interface size, in points.
    pub const UI_FONT_SIZE: isize = 25;
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
    /// Multi-line paste warning toggle.
    pub const WARN_MULTILINE_PASTE: isize = 102;
    /// Ask-before-closing-with-sessions toggle.
    pub const CONFIRM_CLOSE: isize = 103;
    /// File panel toggle for shells on this machine.
    pub const LOCAL_FILE_PANEL: isize = 105;
    /// Word wrap toggle for the editor a file opens in.
    pub const EDITOR_WORD_WRAP: isize = 107;
    /// Default SSH port for new connections.
    pub const DEFAULT_PORT: isize = 110;
    /// Default login name for new connections.
    pub const DEFAULT_USERNAME: isize = 120;
    /// Keepalive interval.
    pub const KEEPALIVE: isize = 130;
    /// Connect timeout.
    pub const TIMEOUT: isize = 140;
    /// Disclosure of the first dashboard.
    ///
    /// Every dashboard takes [`DASHBOARD_STRIDE`] indices — a block wide enough
    /// for its disclosure, its name, its remove button and every pane row it is
    /// ever likely to hold — numbered from its position in the list, so the
    /// dashboards tab in the order they are drawn.
    pub const DASHBOARDS: isize = 200;
    /// Indices one dashboard occupies, disclosure to "Add file" inclusive.
    pub const DASHBOARD_STRIDE: isize = 100;
    /// Offset of a dashboard's name field within its block.
    pub const DASHBOARD_NAME: isize = 1;
    /// Offset of a dashboard's "Remove dashboard" button within its block.
    pub const DASHBOARD_REMOVE: isize = 2;
    /// Offset of a dashboard's "Open at startup" toggle within its block.
    ///
    /// Drawn directly under the name, but numbered after the remove button
    /// because that button already holds the index between them and renumbering
    /// it would move a control the user has learned the position of. The block
    /// leaves room to spare before its pane rows at [`DASHBOARD_PANES`], which
    /// is where the space was reserved for exactly this.
    pub const DASHBOARD_OPEN_AT_STARTUP: isize = 3;
    /// Offset of the first pane row within a dashboard's block.
    pub const DASHBOARD_PANES: isize = 10;
    /// Indices one pane row occupies: the connection picker, the path, the
    /// header color list, the color picker under the row, its hex field and
    /// the button that folds it away, and the button that removes the row.
    pub const DASHBOARD_PANE_STRIDE: isize = 7;
    /// Offset of a dashboard's "Add file" button, past every pane row the
    /// numbering inside the block can reach.
    pub const DASHBOARD_PANE_ADD: isize = 90;
    /// The "Add dashboard" button, past every block the numbering can reach.
    pub const DASHBOARD_ADD: isize = 800;
    /// First index of the global highlight rule list.
    ///
    /// The list numbers its own rows inside
    /// [`TAB_SPAN`](crate::highlight_rules::TAB_SPAN) indices from here, which
    /// is why the two footer buttons sit so far above: a band
    /// wide enough for a rule list is wider than everything above it put
    /// together, and moving Cancel and Save was cheaper than interleaving them.
    pub const HIGHLIGHTS: isize = 1000;
    /// The "Reset to preset" button, just past the list it undoes.
    pub const HIGHLIGHTS_RESET: isize = HIGHLIGHTS + crate::highlight_rules::TAB_SPAN;
    /// Cancel.
    pub const CANCEL: isize = 1900;
    /// Save.
    pub const SAVE: isize = 1910;
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

/// Which of the two color catalogs is meant.
///
/// The catalogs are parallel in every way that matters to this dialog — each
/// is a list of built-in entries plus a directory of files, each is picked from
/// a grid of cards, each has a management row under it — so every action is
/// written once against this enum instead of twice against the two registries.
/// What each of them *is* lives behind
/// [`ThemeCatalog`](rugpui_shell::ThemeCatalog): `rugpui`'s own
/// [`UiThemeCatalog`](rugpui_shell::UiThemeCatalog) for the chrome themes, and
/// [`SchemeCatalog`] for the terminal palettes, which is rulogman's because a
/// widget kit has no terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Catalog {
    /// The chrome themes of [`ThemeRegistry`].
    UiTheme,
    /// The terminal color schemes of [`TerminalTheme`].
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

/// A few of the color names a highlight rule accepts, for the section's hint.
///
/// Read out of the published list rather than spelled out a second time, so the
/// hint cannot come to advertise a name [`rulogman_core::HighlightColor::parse`]
/// has stopped taking. Four of the sixteen slots: the hint is a sentence rather
/// than a reference table, and the rule rows' own placeholders show the rest of
/// the vocabulary — a bright slot name and a hex value.
fn highlight_color_examples() -> String {
    HIGHLIGHT_COLOR_NAMES[1..5].join(", ")
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
    /// The connection picker of one dashboard's pane row.
    Pane {
        /// Position of the dashboard in the list.
        dashboard: usize,
        /// Position of the pane within that dashboard.
        pane: usize,
    },
    /// The header color picker of one dashboard's pane row.
    PaneColor {
        /// Position of the dashboard in the list.
        dashboard: usize,
        /// Position of the pane within that dashboard.
        pane: usize,
    },
}

/// One editable pane row of a dashboard.
///
/// The connection is held as the id it refers to rather than as a position in
/// the profile list: the list is re-read every time the dialog opens, and a
/// position would silently come to mean a different host the moment a profile
/// above it was removed. An id that no longer resolves is kept as it is and
/// drawn as broken — see [`DashboardPane::profile`].
struct PaneRow {
    /// The connection this file is followed over, once one has been picked.
    profile: Option<Uuid>,
    /// Absolute path of the file on that host.
    path: Entity<TextInput>,
    /// The palette color the pane's header is painted in, or `None` for the
    /// theme's own strip — picked here or on the pane's own header. Ignored
    /// while [`Self::custom_chosen`] is set.
    ///
    /// Carried with the row, so a row that is moved or removed takes its color
    /// with it.
    color: Option<String>,
    /// Whether the header wears a color of the user's own, picked in the
    /// color picker under the row rather than from the palette.
    custom_chosen: bool,
    /// The color of the user's own, as the picker under the row holds it.
    custom_color: Hsv,
    /// Whether that picker is unfolded under the row.
    ///
    /// Unfolded by choosing *Custom…* in the color list — again, to change a
    /// color already picked — and folded by its *Done*, so a finished color
    /// stops taking the height of a picker. A row loaded with a color of the
    /// user's own starts folded: the list's swatch already shows the color.
    picker_open: bool,
    /// Tab index of the row's connection picker, fixed when the row was built.
    ///
    /// Held rather than derived from the row's position, because the path field
    /// beside it took its own index at construction and cannot be renumbered:
    /// deriving one of the pair and baking the other would put the two out of
    /// order the moment a row above them was removed.
    tab_index: isize,
}

/// One editable dashboard: a name, and the panes under it.
struct DashboardRow {
    /// Identifier of the dashboard this row edits, so that saving renames the
    /// stored entry instead of replacing it with a new one.
    id: Uuid,
    /// The name shown on the section header and stored in `dashboards.json`.
    name: Entity<TextInput>,
    /// Whether launching rulogman opens this dashboard — the state of the
    /// toggle, which is [`Dashboard::open_at_startup`] as the form has it.
    ///
    /// A plain `bool` rather than an [`Entity`], because a checkbox has no
    /// buffer of its own to hold: the dialog owns the value and hands it back
    /// to the control on every paint.
    open_at_startup: bool,
    /// The files this dashboard shows, in the order they are drawn.
    panes: Vec<PaneRow>,
    /// Whether the section is expanded. Nothing to do with
    /// [`open_at_startup`](Self::open_at_startup): this one is where the
    /// disclosure triangle is pointing and is never saved.
    open: bool,
    /// First tab index of this dashboard's block, fixed at construction for the
    /// reason [`PaneRow::tab_index`] is.
    tab_base: isize,
}

/// The content of one pane row, read out of its controls.
///
/// Splitting the reading from the interpreting is what lets the rules of an
/// unfinished row be exercised without a window, the way the connection
/// dialog's `TunnelFields` does: [`collect_dashboards`] sees only these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PaneFields {
    /// The connection picked, if any.
    profile: Option<Uuid>,
    /// Path as typed.
    path: String,
    /// The header color the row carries, as it would be stored: a palette
    /// name or a `#rrggbb` — or, carried through untouched, a spelling read
    /// from the file that is neither.
    color: Option<String>,
}

/// The content of one dashboard, read out of its controls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DashboardFields {
    /// Identifier the finished dashboard keeps.
    id: Uuid,
    /// Name as typed; blank falls back to [`DEFAULT_DASHBOARD_NAME`].
    name: String,
    /// State of the "Open at startup" toggle.
    open_at_startup: bool,
    /// The pane rows, in order.
    panes: Vec<PaneFields>,
}

/// The name each row would be stored under, with the blank ones filled in.
///
/// A dashboard is picked out of a list by its name, so "" is not a name it can
/// have: a row the user has not named yet is given [`DEFAULT_DASHBOARD_NAME`],
/// numbered from two upwards when something already answers to it. The names
/// already typed are counted as taken, so filling one in cannot collide with a
/// dashboard the user did name — and the header of the section shows the same
/// string this returns, so what is on screen is what will be written.
fn dashboard_names(rows: &[DashboardFields]) -> Vec<String> {
    let mut taken: Vec<String> = rows
        .iter()
        .map(|row| row.name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();

    let mut names = Vec::with_capacity(rows.len());
    for row in rows {
        let typed = row.name.trim();
        if !typed.is_empty() {
            names.push(typed.to_owned());
            continue;
        }
        let filled = (1..)
            .map(|n| match n {
                1 => DEFAULT_DASHBOARD_NAME.to_owned(),
                n => format!("{DEFAULT_DASHBOARD_NAME} {n}"),
            })
            .find(|candidate| !taken.iter().any(|name| name == candidate))
            // The range is unbounded and the list is finite, so a name is
            // always found; `unwrap_or_else` only spares the caller an
            // `expect`.
            .unwrap_or_else(|| DEFAULT_DASHBOARD_NAME.to_owned());
        taken.push(filled.clone());
        names.push(filled);
    }
    names
}

/// Turn the dashboard rows of the form into dashboards, or refuse.
///
/// A pane row with no path is dropped rather than complained about: a section
/// always ends on the empty row "Add file" produced, and an empty form is not a
/// mistake. A row that *does* name a file has to say which connection reaches
/// it, and there the refusal stops: the row has to carry *a* connection, not a
/// connection that still exists.
///
/// The distinction is the whole policy. A pane whose profile was deleted since
/// the dashboard was written is kept verbatim — id and all — for the reason
/// [`rulogman_core::dashboard`] gives for keeping it on disk: the id is the
/// only record of which host the file was on, and the user is the only one who
/// can say what it should be now. The picker draws such a row as dead so it
/// cannot be missed, and the form still saves, because a dashboard broken by an
/// unrelated deletion must not hold the *font size* hostage. Dropping the pane
/// instead would lose the path, and rebinding it would silently follow a
/// different file than the one asked for.
///
/// What is refused is a row that names a file and no connection at all: nothing
/// was lost there, the user simply has not finished, and one more press on the
/// picker completes it.
///
/// A dashboard with no panes at all is kept. It is the state every dashboard
/// passes through between being created and being filled in, and refusing to
/// save the form over it would mean the user could not put the dialog down
/// halfway.
///
/// `None` is the refusal; the caller turns it into the message strip.
fn collect_dashboards(rows: &[DashboardFields]) -> Option<Vec<Dashboard>> {
    let names = dashboard_names(rows);
    let mut dashboards = Vec::with_capacity(rows.len());
    for (row, name) in rows.iter().zip(names) {
        let mut panes = Vec::with_capacity(row.panes.len());
        for pane in &row.panes {
            let path = pane.path.trim();
            if path.is_empty() {
                continue;
            }
            // Not checked against the profiles that exist; see above.
            let profile = pane.profile?;
            panes.push(DashboardPane {
                profile,
                path: path.to_owned(),
                color: pane.color.clone(),
            });
        }
        dashboards.push(Dashboard {
            id: row.id,
            name,
            open_at_startup: row.open_at_startup,
            panes,
            // The form carries no geometry; the caller restores each layout by
            // id from the store the dialog opened on. See [`SettingsDialog::save`].
            layout: None,
        });
    }
    Some(dashboards)
}

/// Which row of a pane's color picker stands for what `pane` holds: the
/// default row, a palette color's row, or the *Custom…* row at the end.
fn header_color_option(pane: &PaneRow) -> usize {
    if pane.custom_chosen {
        return HEADER_COLORS.len() + 1;
    }
    pane.color
        .as_deref()
        .and_then(header_preset)
        .map_or(0, |slot| slot + 1)
}

/// The rows of a pane's color picker: the default, every palette color led
/// by the color it paints, and *Custom…*, led by the color of the user's own
/// once `custom` holds one.
///
/// The default row leads with `strip`, the theme's own header, which is what
/// it gives back.
fn header_color_options(custom: Option<&str>, strip: Hsla) -> Vec<SelectOption> {
    let mut options = vec![SelectOption::new(ts!("tab.header_color_default")).swatch(strip)];
    for (name, _, label) in HEADER_COLORS {
        let option = SelectOption::new(ts!(label));
        options.push(match header_fill(name) {
            Some(fill) => option.swatch(fill),
            None => option,
        });
    }
    let own = SelectOption::new(ts!("tab.header_color_custom"));
    options.push(match custom.and_then(header_fill) {
        Some(fill) => own.swatch(fill),
        None => own,
    });
    options
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

type PaletteChoices = (Rc<[SchemeSwatch]>, Rc<[SchemeSwatch]>);
type FontChoices = (
    SharedString,
    Option<SharedString>,
    Rc<[rugpui::SelectOption]>,
);

/// Modal dialog editing [`rulogman_core::AppSettings`].
///
/// Create it once with [`SettingsDialog::new`], keep the handle, subscribe to
/// [`SettingsDialogEvent`], and render it as the last child of a `relative()`
/// root. It renders nothing while [`SettingsDialog::is_open`] is `false`, so it
/// is safe to render unconditionally.
pub struct SettingsDialog {
    // Invalidated when opening the dialog or changing a palette catalog.
    palette_choices: RefCell<Option<PaletteChoices>>,
    font_choices: RefCell<Option<FontChoices>>,
    /// Whether the dialog is currently visible.
    open: bool,
    /// The snapshot the form was populated from. Saving starts from this value
    /// so fields the dialog does not edit — the schema version, for one —
    /// survive a round trip.
    base: AppSettings,
    /// The form as the first frame after opening drew it, against which
    /// [`Self::has_unsaved_edits`] measures it; `None` until that frame.
    base_form: Option<FormSnapshot>,
    /// Whether `Escape` asked to throw away edits and is waiting for an answer.
    discard_confirm: bool,
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
    /// Whether a multi-line paste into a shell without bracketed paste asks
    /// first.
    warn_multiline_paste: bool,
    /// Whether closing or quitting with sessions open asks first.
    confirm_close_with_sessions: bool,
    /// Whether a shell on this machine opens with the file panel beside it.
    ///
    /// The local counterpart of the connection dialog's own checkbox: a remote
    /// session carries the answer on its profile, and a local shell has no
    /// profile to carry it, so this is where every local shell is answered for
    /// at once.
    local_file_panel: bool,
    /// Whether the editor a file opens in breaks lines too long for the pane.
    ///
    /// Beside the file panel toggle rather than in a section of its own: what
    /// this dialog offers for the editor is one flag, and the two rows are the
    /// same kind of answer about the same half of a session — the files beside
    /// the shell rather than the shell itself.
    editor_word_wrap: bool,
    /// The global highlight rules, as the form has them.
    ///
    /// An entity of its own because the same editor is what a followed-file row
    /// of the connection dialog puts under its "Custom highlighting" tick; see
    /// [`crate::highlight_rules`].
    highlights: Entity<HighlightRuleList>,
    /// The management row under the UI theme picker.
    ui_theme_actions: Entity<CatalogActions>,
    /// The management row under the color scheme picker.
    scheme_actions: Entity<CatalogActions>,
    /// Keeps the two rows' subscriptions alive.
    _catalog_events: [Subscription; 2],
    /// The color editor, while one is open. The dialog renders it *instead of*
    /// the form rather than over it; see [`rugpui_shell::theme_editor`] for why.
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
    /// Scroll position of whichever pane row's list is open. One handle for
    /// every row: at most one dropdown is open at a time anywhere in the
    /// dialog, so the rows share it the way they share the [`OpenList`] slot.
    pane_scroll: ScrollHandle,
    /// The dashboards being edited, one row each, rebuilt every time the dialog
    /// opens.
    dashboard_rows: Vec<DashboardRow>,
    /// The saved geometry of each dashboard as it stood when the dialog opened,
    /// kept by id so a Save can carry it forward.
    ///
    /// The form has no controls for a layout, so [`collect_dashboards`] rebuilds
    /// each dashboard without one; without this the mere act of opening the
    /// dialog and pressing Save would wipe every hand-tuned arrangement. A
    /// layout stale after a pane edit is harmless to keep — [`Dashboard::valid_layout`]
    /// re-checks it at open time and falls back to a grid — so it is threaded
    /// through untouched rather than validated here.
    dashboard_layouts: HashMap<Uuid, LayoutNode>,
    /// Why `dashboards.json` could not be read when the dialog opened, if it
    /// could not.
    ///
    /// The dashboard rows are then empty rather than the user's, and Save
    /// writes the whole list, so it would replace every dashboard in the file
    /// with what the form shows. Save refuses while this is set, and only the
    /// next opening reads the file again: the rows already on screen were built
    /// from nothing, so a file that has become readable since does not make
    /// them right.
    dashboards_error: Option<String>,
    /// Where the dashboards are read from and written to. `None` is the default
    /// configuration file; only a test names another, so that it never touches
    /// the `dashboards.json` of whoever runs it.
    dashboards_file: Option<PathBuf>,
    /// The saved connections, as `(id, name)`, read when the dialog opens.
    ///
    /// Read straight from a [`ProfileStore`] of this dialog's own rather than
    /// borrowed from the connection dialog, which owns the *writable* store: a
    /// dashboard only ever needs to name a profile, never to edit one, and a
    /// second writable handle on one file is exactly how two dialogs come to
    /// disagree about what is in it.
    profiles: Vec<(Uuid, SharedString)>,
    /// Window background opacity, in whole percent.
    opacity_input: Entity<TextInput>,
    /// The chrome's body text size, in points — "Interface size".
    ui_font_size_input: Entity<TextInput>,
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
        BIND_KEYS.call_once(|| {
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, Some(KEY_CONTEXT)),
                KeyBinding::new("shift-tab", FocusPrev, Some(KEY_CONTEXT)),
            ]);
        });

        // Every placeholder but one is a sample *value* — a number, or the
        // default `TERM` — and reads the same in every language. The username
        // hint is a word, so it is translated; it is also the only placeholder
        // `refresh_placeholders` has to revisit after a language switch.
        let opacity_input = Self::field(cx, "100".into(), tab::OPACITY);
        let ui_font_size_input = Self::field(
            cx,
            format_number(rulogman_core::DEFAULT_UI_FONT_SIZE).into(),
            tab::UI_FONT_SIZE,
        );
        let font_size_input = Self::field(cx, "14".into(), tab::FONT_SIZE);
        let scrollback_input = Self::field(cx, "5000".into(), tab::SCROLLBACK);
        let term_input = Self::field(cx, "xterm-256color".into(), tab::TERM);
        let port_input = Self::field(cx, "22".into(), tab::DEFAULT_PORT);
        let username_input = Self::field(
            cx,
            ts!("settings.username_placeholder"),
            tab::DEFAULT_USERNAME,
        );
        let keepalive_input = Self::field(cx, "30".into(), tab::KEEPALIVE);
        let timeout_input = Self::field(cx, "15".into(), tab::TIMEOUT);

        // Numeric fields have no input filter of their own, so each one is
        // sanitized after the fact by an observer.
        restrict_to_number(cx, &opacity_input, false, 3);
        restrict_to_number(cx, &ui_font_size_input, true, 5);
        restrict_to_number(cx, &font_size_input, true, 5);
        restrict_to_number(cx, &scrollback_input, false, 6);
        restrict_to_number(cx, &port_input, false, 5);
        restrict_to_number(cx, &keepalive_input, false, 5);
        restrict_to_number(cx, &timeout_input, false, 5);

        let base = AppSettings::default();

        // The two catalogs, with the ids to fall back on when the selected
        // entry is deleted. Built once: the directories are fixed for the run,
        // and a row that had to be rebuilt would drop the confirmation it was
        // in the middle of asking.
        let ui_catalog: Arc<dyn ThemeCatalog> = Arc::new(UiThemeCatalog::new(
            theme_store::theme_dirs_or_empty(),
            base.ui_theme.clone(),
        ));
        let scheme_catalog: Arc<dyn ThemeCatalog> = Arc::new(SchemeCatalog);
        let highlights = cx.new(|cx| HighlightRuleList::new(cx, tab::HIGHLIGHTS));
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
            warn_multiline_paste: base.terminal.warn_multiline_paste,
            confirm_close_with_sessions: base.window.confirm_close_with_sessions,
            local_file_panel: base.files.local_panel,
            editor_word_wrap: base.editor.word_wrap,
            font_family: base.terminal.font_family.clone().map(SharedString::from),
            base,
            base_form: None,
            discard_confirm: false,
            highlights,
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
            visible_section: 0,
            open_list: None,
            fonts: Vec::new(),
            palette_choices: RefCell::new(None),
            font_choices: RefCell::new(None),
            font_scroll: ScrollHandle::new(),
            language_scroll: ScrollHandle::new(),
            ui_theme_scroll: ScrollHandle::new(),
            scheme_scroll: ScrollHandle::new(),
            pane_scroll: ScrollHandle::new(),
            dashboard_rows: Vec::new(),
            dashboard_layouts: HashMap::new(),
            dashboards_error: None,
            dashboards_file: None,
            profiles: Vec::new(),
            opacity_input,
            ui_font_size_input,
            font_size_input,
            scrollback_input,
            term_input,
            port_input,
            username_input,
            keepalive_input,
            timeout_input,
        }
    }

    /// Build one of the dialog's text fields.
    ///
    /// `Enter` saves from any of them, matching the connection dialog. The
    /// deferred call is load-bearing: `on_submit` runs while gpui has the
    /// TextInput leased, and saving reads every field back.
    fn field(
        cx: &mut Context<Self>,
        placeholder: SharedString,
        tab_index: isize,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
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

    /// The body's overlay scroll indicator, as it stands.
    fn body_bar(&self) -> Scrollbar {
        Scrollbar::for_handle(BODY_SCROLLBAR, ScrollbarAxis::Vertical, &self.body_scroll)
            .fade(self.body_scrollbar.fade())
    }

    /// The same bar, listening for the pointer reaching the edge it rides.
    ///
    /// Only the bar that is drawn needs it: the one the drag path builds is
    /// there to be measured, and never reaches an element tree.
    fn hovering_body_bar(&self, cx: &mut Context<Self>) -> Scrollbar {
        self.body_bar()
            .on_hover(cx.listener(|dialog, hovered: &bool, _window, cx| {
                dialog.hover_scrollbar(*hovered, cx);
            }))
    }

    /// Puts the body's bar up whenever it has been scrolled, and starts the
    /// clock that takes it down again.
    fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        let scrolled = scrolled(&self.body_scroll, ScrollbarAxis::Vertical);
        if let Some(epoch) = self.body_scrollbar.moved(scrolled) {
            hide_later(epoch, cx, |dialog| Some(&mut dialog.body_scrollbar));
        }
    }

    /// Scrolls the body when its thumb is dragged.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        let Some(progress) = self.body_bar().dragged(event, cx) else {
            return;
        };
        self.body_scrollbar.hold();
        scroll_to(&self.body_scroll, ScrollbarAxis::Vertical, progress);
        cx.notify();
    }

    /// Lets go of the body's thumb, if it was held, and starts its clock again.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) {
        if let Some(epoch) = self.body_scrollbar.release() {
            hide_later(epoch, cx, |dialog| Some(&mut dialog.body_scrollbar));
            cx.notify();
        }
    }

    /// Puts the body's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    fn hover_scrollbar(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            if self.body_scrollbar.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = self.body_scrollbar.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, |dialog| Some(&mut dialog.body_scrollbar));
    }

    /// [`Self::open`], scrolled to `section` — `Manage dashboards…` and the
    /// welcome screen's *Create dashboard…* open on the dashboards, which sit
    /// below the first screenful of the body.
    pub fn open_at(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.open(cx);
        self.show_section(section as usize, cx);
    }

    /// Scrolls the body to the section at `index`, as the index above it does.
    fn show_section(&mut self, index: usize, cx: &mut Context<Self>) {
        self.visible_section = index;
        self.body_scroll.scroll_to_item(index);
        cx.notify();
    }

    /// The row of section names above the body, each one scrolling to its
    /// section: the body is taller than the dialog, and an overlay scrollbar
    /// does not say there is anything below the first screenful.
    fn render_section_index(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let names = [
            ts!("settings.section.appearance"),
            ts!("settings.section.terminal"),
            ts!("settings.section.connection"),
            ts!("settings.dashboards.title"),
            ts!("settings.highlights.title"),
        ];
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .flex_none()
            .gap(px(4.))
            .children(names.into_iter().enumerate().map(|(index, name)| {
                let current = index == self.visible_section;
                div()
                    .id(("settings-section-link", index))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(if current {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .when(current, |link| link.bg(theme.surface_active))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
                    .on_click(
                        cx.listener(move |dialog, _: &gpui::ClickEvent, _window, cx| {
                            dialog.show_section(index, cx);
                        }),
                    )
                    .child(name)
            }))
    }

    /// Show the dialog, re-reading the current settings into the form.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.palette_choices.get_mut().take();
        self.font_choices.get_mut().take();
        let settings = app_settings::current(cx);
        self.fonts = installed_fonts(cx);
        self.refresh_placeholders(cx);
        self.fill_form(&settings, cx);
        self.base = settings;
        self.base_form = None;
        self.discard_confirm = false;
        self.load_dashboards(cx);
        self.status = app_settings::load_error(cx)
            .map(|error| ts!("settings.load_failed", error = error))
            .or_else(|| self.dashboards_refusal());
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
        self.discard_confirm = false;
        self.open_list = None;
        self.pending_focus = false;
        self.status = None;
        self.editor = None;
        self.editor_events = None;
        cx.notify();
    }

    /// The management row under one catalog's picker.
    fn actions(&self, catalog: Catalog) -> &Entity<CatalogActions> {
        match catalog {
            Catalog::UiTheme => &self.ui_theme_actions,
            Catalog::Scheme => &self.scheme_actions,
        }
    }

    /// The catalog behind one of the two rows.
    fn catalog_of(&self, catalog: Catalog, cx: &App) -> Arc<dyn ThemeCatalog> {
        self.actions(catalog).read(cx).catalog().clone()
    }

    /// Highlights `id` in one catalog's picker.
    ///
    /// Only the form is touched; nothing is persisted until the dialog is
    /// saved, exactly as when the user clicks a card. The management row is
    /// told too, because everything it offers is about the selection and a row
    /// that had not been told would gray the wrong buttons out.
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
    /// editor is drawn instead of. See [`rugpui_shell::catalog_ui`] for why the
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
            CatalogActionEvent::Changed => {
                self.palette_choices.get_mut().take();
                cx.emit(SettingsDialogEvent::ThemesChanged);
            }
            // The file the row loaded travels in the event, so nothing is read
            // twice: a gpui subscription only ever borrows the event, but
            // `CatalogFile` is `Clone` and the clone of a catalog's own kind
            // is an `Arc`'s.
            CatalogActionEvent::Edit { id, file } => {
                let source = self.catalog_of(catalog, cx);
                let editor = cx.new(|cx| ThemeEditor::new(source, id.clone(), file, cx));
                self.open_editor(editor, cx);
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
    /// in use repaints under its new colors without the settings themselves
    /// having to be saved.
    fn close_editor(&mut self, saved: bool, cx: &mut Context<Self>) {
        self.editor = None;
        self.editor_events = None;
        self.pending_focus = true;
        if saved {
            self.palette_choices.get_mut().take();
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
        // not been told would gray the wrong buttons out.
        self.select(Catalog::UiTheme, settings.ui_theme.clone(), cx);
        self.select(Catalog::Scheme, settings.terminal.scheme.clone(), cx);
        self.titlebar = settings.window.titlebar;
        self.language = settings.language.clone();
        self.background_blur = settings.window.background_blur;
        self.copy_on_select = settings.terminal.copy_on_select;
        self.warn_multiline_paste = settings.terminal.warn_multiline_paste;
        self.local_file_panel = settings.files.local_panel;
        self.editor_word_wrap = settings.editor.word_wrap;
        self.font_family = settings
            .terminal
            .font_family
            .clone()
            .map(SharedString::from);

        // The rules that *apply*, not the ones stored: a `None` list is the
        // built-in preset, and showing the user an empty editor for it would
        // read as "highlighting is off" when it is the opposite. Resolved
        // against `None` for the file, because this dialog edits the level
        // every file falls back to.
        let rules = effective_highlights(&settings.highlights, None).into_owned();
        self.highlights
            .clone()
            .update(cx, |list, cx| list.set_rules(&rules, cx));

        let percent = (settings.window.background_opacity * 100.0).round() as i32;
        set_text(&self.opacity_input, percent.to_string(), cx);
        set_text(
            &self.ui_font_size_input,
            format_number(settings.window.ui_font_size),
            cx,
        );
        self.confirm_close_with_sessions = settings.window.confirm_close_with_sessions;
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
        if let Some(size) = parse_number::<f32>(&self.ui_font_size_input, cx) {
            settings.window.ui_font_size = size;
        }
        settings.window.confirm_close_with_sessions = self.confirm_close_with_sessions;
        if let Some(percent) = parse_number::<f32>(&self.opacity_input, cx) {
            settings.window.background_opacity = percent / 100.0;
        }

        settings.terminal.scheme = self.scheme.to_string();
        settings.terminal.font_family = self.font_family.as_ref().map(ToString::to_string);
        settings.terminal.copy_on_select = self.copy_on_select;
        settings.terminal.warn_multiline_paste = self.warn_multiline_paste;
        settings.files.local_panel = self.local_file_panel;
        settings.editor.word_wrap = self.editor_word_wrap;
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
    ///
    /// Refuses before anything is written while `settings.json` could not be
    /// read at start-up: the form was filled from defaults then, and applying
    /// it would replace the file with them (see [`app_settings::load_error`]).
    /// The same goes for `dashboards.json`, whose rows are empty when it could
    /// not be read as the dialog opened (see [`Self::dashboards_error`]).
    fn save(&mut self, cx: &mut Context<Self>) {
        if let Some(error) = app_settings::load_error(cx) {
            self.status = Some(ts!("settings.load_failed", error = error));
            cx.notify();
            return;
        }
        if let Some(refusal) = self.dashboards_refusal() {
            self.status = Some(refusal);
            cx.notify();
            return;
        }
        // Refused before anything is written, the way the connection dialog
        // refuses a half-written tunnel: a pane naming a file and no connection
        // at all is a row the user has not finished, and saving over it would
        // quietly drop the path they typed. A pane whose connection has since
        // been *deleted* is not that, and goes through — see
        // [`collect_dashboards`].
        let Some(dashboards) = self.dashboards(cx) else {
            self.status = Some(ts!("settings.dashboards.incomplete"));
            cx.notify();
            return;
        };

        // Refused for the same reason, one step further on: a pattern that
        // does not compile and a color nothing can parse are both stored
        // happily by `rulogman-core` and both then do nothing at all, which
        // from the far end of a `tail -f` is indistinguishable from a rule
        // that is simply wrong about the log.
        let rules = match collect_highlight_rules(&self.highlights.read(cx).fields(cx)) {
            Ok(rules) => rules,
            Err(problem) => {
                self.status = Some(match problem {
                    HighlightProblem::BadPattern(text) => {
                        ts!("settings.highlights.bad_pattern", pattern = text)
                    }
                    HighlightProblem::BadColor(text) => {
                        ts!("settings.highlights.bad_color", color = text)
                    }
                });
                cx.notify();
                return;
            }
        };

        let mut settings = self.collect(cx);
        // `collect` starts from the snapshot the dialog opened on; the window
        // may have been resized since, and that is not the dialog's to undo.
        app_settings::carry_window_geometry(&mut settings, cx);
        // An untouched preset is written back as "nothing configured" rather
        // than as a frozen copy of today's preset, so that a later build's
        // improvements to the built-in list keep reaching this user. Every
        // other list is stored verbatim — including the empty one, which is how
        // "turn highlighting off" is said and is not the same as saying nothing.
        settings.highlights.rules = (!is_highlight_preset(&rules)).then_some(rules);
        settings.sanitize();

        // Reported through the settings' own message, because from the user's
        // side this is one Save: the dialog could not write what it was asked
        // to write, and the recovery — the configuration directory — is the
        // same whichever of the two files refused.
        let mut store = match &self.dashboards_file {
            Some(path) => DashboardStore::at(path),
            None => DashboardStore::default(),
        };
        for mut dashboard in dashboards {
            // Carry the saved geometry the form cannot edit forward by id, so a
            // Save that only changed a font size does not wipe every dashboard's
            // arrangement. A layout outdated by a pane edit is left in place —
            // `Dashboard::valid_layout` catches the drift at open time.
            dashboard.layout = self.dashboard_layouts.get(&dashboard.id).cloned();
            store.upsert(dashboard);
        }
        if let Err(err) = store.save() {
            log::error!("could not write dashboards.json: {err:#}");
            self.status = Some(ts!("settings.save_failed", error = format!("{err:#}")));
            cx.notify();
            return;
        }

        if let Err(err) = app_settings::save(&settings, cx) {
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

    /// Everything Save would write, as one value two moments compare by.
    fn form_snapshot(&self, cx: &App) -> FormSnapshot {
        FormSnapshot {
            settings: self.collect(cx),
            dashboards: self.dashboard_fields(cx),
            highlights: self.highlights.read(cx).fields(cx),
        }
    }

    /// Whether the form differs from what it held when the dialog opened.
    pub(crate) fn has_unsaved_edits(&self, cx: &App) -> bool {
        self.base_form
            .as_ref()
            .is_some_and(|base| *base != self.form_snapshot(cx))
    }

    /// `Escape` and the workspace's own dismissal: closes the dialog, unless
    /// the form holds edits nobody saved — then it asks first.
    pub fn request_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.has_unsaved_edits(cx) {
            self.discard_confirm = true;
            cx.notify();
        } else {
            self.dismiss(cx);
        }
    }

    /// The discard question's *Keep editing*, and `Escape` over it.
    fn keep_editing(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.discard_confirm) {
            return false;
        }
        cx.notify();
        true
    }

    /// The question `Escape` asks over a form holding unsaved edits.
    fn render_discard_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.discard_confirm {
            return None;
        }
        let (keep, discard) = (cx.entity(), cx.entity());
        let keep = Answer::new(
            ts!("connection.keep_editing"),
            ButtonVariant::Primary,
            move |_window, cx| {
                keep.update(cx, |dialog, cx| {
                    dialog.keep_editing(cx);
                });
            },
        );
        let safe = keep.on_click.clone();
        Some(questions::question(
            "settings-discard-question",
            ts!("connection.discard_title"),
            vec![ts!("settings.discard_confirm")],
            Answer::new(
                ts!("connection.discard"),
                ButtonVariant::Danger,
                move |_window, cx| discard.update(cx, |dialog, cx| dialog.dismiss(cx)),
            ),
            keep,
            safe,
            cx,
        ))
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
            index if index <= tab::EDITOR_WORD_WRAP => 1,
            index if index <= tab::TIMEOUT => 2,
            index if index <= tab::DASHBOARD_ADD => 3,
            // The footer's two buttons land here as well, which is what they
            // did before the dashboards section existed: the last section is
            // the one above them either way.
            _ => 4,
        };
        if section != self.visible_section {
            self.visible_section = section;
            self.body_scroll.scroll_to_item(section);
            cx.notify();
        }
    }

    /// `Escape` dismisses the dialog from anywhere inside it — asking first
    /// when the form holds edits nobody saved.
    ///
    /// Anything layered on top of the form takes the key first and only undoes
    /// itself, so that backing out of a list, a question or the color editor
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
        for catalog in [Catalog::UiTheme, Catalog::Scheme] {
            let actions = self.actions(catalog).clone();
            if actions.read(cx).is_confirming() {
                actions.update(cx, |row, cx| row.cancel_confirm(cx));
                return;
            }
        }
        if self.keep_editing(cx) {
            return;
        }
        self.request_dismiss(cx);
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

    fn cached_palettes(&self, cx: &App) -> PaletteChoices {
        self.palette_choices
            .borrow_mut()
            .get_or_insert_with(|| (ui_theme_swatches(cx).into(), scheme_swatches().into()))
            .clone()
    }

    fn cached_fonts(&self) -> Rc<[rugpui::SelectOption]> {
        let label = system_default();
        let mut cache = self.font_choices.borrow_mut();
        if cache.as_ref().is_none_or(|(cached_label, family, _)| {
            cached_label != &label || family != &self.font_family
        }) {
            let options = self
                .font_options()
                .into_iter()
                .map(rugpui::SelectOption::from)
                .collect();
            *cache = Some((label, self.font_family.clone(), options));
        }
        cache.as_ref().unwrap().2.clone()
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
                // Asked of the catalogs rather than of the swatches the list
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
                OpenList::PaneColor { dashboard, pane } => {
                    let current = self
                        .dashboard_rows
                        .get(dashboard)
                        .and_then(|row| row.panes.get(pane))
                        .map(header_color_option);
                    (&self.pane_scroll, current)
                }
                OpenList::Pane { dashboard, pane } => {
                    let picked = self
                        .dashboard_rows
                        .get(dashboard)
                        .and_then(|row| row.panes.get(pane))
                        .and_then(|pane| pane.profile);
                    // A resolvable connection is at its own position in the
                    // list. An unresolvable one has no row but the dead entry
                    // a dangling row is given, which is drawn in front of the
                    // profiles and is therefore row zero.
                    let current = picked.map(|id| {
                        self.profiles
                            .iter()
                            .position(|(known, _)| *known == id)
                            .unwrap_or(0)
                    });
                    (&self.pane_scroll, current)
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

    /// First tab index of the dashboard at `position` in the list.
    ///
    /// Clamped so that a list longer than the numbering allows for cannot push
    /// a dashboard past the "Add dashboard" button and out of the tab ring's
    /// order; dashboards that far down share a block and tab in paint order,
    /// exactly as the connection dialog's repeatable rows do.
    fn dashboard_tab_base(position: usize) -> isize {
        (tab::DASHBOARDS + position as isize * tab::DASHBOARD_STRIDE)
            .min(tab::DASHBOARD_ADD - tab::DASHBOARD_STRIDE)
    }

    /// Tab index of the connection picker on pane `position` of the block at
    /// `base`. Clamped within the block for the reason above.
    fn pane_tab_index(base: isize, position: usize) -> isize {
        (base + tab::DASHBOARD_PANES + position as isize * tab::DASHBOARD_PANE_STRIDE)
            .min(base + tab::DASHBOARD_PANE_ADD - tab::DASHBOARD_PANE_STRIDE)
    }

    /// Build an empty pane row for `position` within the block at `base`.
    fn pane_row(cx: &mut Context<Self>, base: isize, position: usize) -> PaneRow {
        let tab_index = Self::pane_tab_index(base, position);
        // A sample path, like the numeric hints of the form above: it reads the
        // same in every language and is never translated.
        let path = Self::field(cx, "/var/log/syslog".into(), tab_index + 1);
        PaneRow {
            profile: None,
            path,
            color: None,
            custom_chosen: false,
            custom_color: Hsv::default(),
            picker_open: false,
            tab_index,
        }
    }

    /// Build an empty dashboard row numbered for `position` in the list.
    fn dashboard_row(cx: &mut Context<Self>, position: usize) -> DashboardRow {
        let tab_base = Self::dashboard_tab_base(position);
        // The placeholder is the name the dashboard would be saved under if it
        // were left blank, so the hint and the outcome agree.
        let name = Self::field(
            cx,
            DEFAULT_DASHBOARD_NAME.into(),
            tab_base + tab::DASHBOARD_NAME,
        );
        DashboardRow {
            id: Uuid::new_v4(),
            name,
            open_at_startup: false,
            panes: Vec::new(),
            open: false,
            tab_base,
        }
    }

    /// Replace the dashboard rows with one per stored dashboard.
    ///
    /// Rebuilt from scratch rather than reconciled, which is what stops an
    /// edit the user abandoned by pressing Cancel from following them into the
    /// next opening of the dialog.
    fn set_dashboard_rows(&mut self, dashboards: &[Dashboard], cx: &mut Context<Self>) {
        let mut rows = Vec::with_capacity(dashboards.len());
        for (position, dashboard) in dashboards.iter().enumerate() {
            let mut row = Self::dashboard_row(cx, position);
            row.id = dashboard.id;
            set_text(&row.name, dashboard.name.clone(), cx);
            row.open_at_startup = dashboard.open_at_startup;
            for (index, pane) in dashboard.panes.iter().enumerate() {
                let mut slot = Self::pane_row(cx, row.tab_base, index);
                slot.profile = Some(pane.profile);
                // A palette name goes to the list and a `#rrggbb` to the picker.
                // A spelling that is neither — hand-written into the file — is
                // drawn as no color, so the list says *Default*, and it is
                // carried through a Save untouched until the user picks another.
                match pane
                    .color
                    .as_deref()
                    .filter(|color| header_preset(color).is_none())
                    .and_then(Hsv::from_hex)
                {
                    Some(color) => {
                        slot.custom_chosen = true;
                        slot.custom_color = color;
                    }
                    None => slot.color = pane.color.clone(),
                }
                set_text(&slot.path, pane.path.clone(), cx);
                row.panes.push(slot);
            }
            rows.push(row);
        }
        self.dashboard_rows = rows;
    }

    /// Re-read the dashboards and the connections they may point at.
    ///
    /// Both are read afresh on every opening, so a dashboard edited by hand or
    /// a connection added since the dialog was last up is picked up. Neither
    /// failure is fatal to the dialog: an unreadable file leaves the section
    /// empty and is logged. An unreadable `dashboards.json` is also kept in
    /// [`Self::dashboards_error`], because Save would otherwise write that
    /// empty section over it.
    fn load_dashboards(&mut self, cx: &mut Context<Self>) {
        self.profiles = match ProfileStore::load() {
            Ok(store) => store
                .profiles()
                .iter()
                .map(|profile| (profile.id, SharedString::from(profile.name.clone())))
                .collect(),
            Err(err) => {
                log::warn!("no connections to offer a dashboard: {err:#}");
                Vec::new()
            }
        };
        let loaded = match &self.dashboards_file {
            Some(path) => DashboardStore::load_from(path),
            None => DashboardStore::load(),
        };
        let store = match loaded {
            Ok(store) => {
                self.dashboards_error = None;
                store
            }
            Err(err) => {
                log::warn!("dashboards.json could not be read, so they cannot be saved: {err:#}");
                self.dashboards_error = Some(format!("{err:#}"));
                DashboardStore::default()
            }
        };
        // Remembered so a Save can carry each layout forward: the form edits
        // names and panes, never geometry, so this is the only record of it.
        self.dashboard_layouts = store
            .dashboards()
            .iter()
            .filter_map(|dashboard| {
                dashboard
                    .layout
                    .clone()
                    .map(|layout| (dashboard.id, layout))
            })
            .collect();
        self.set_dashboard_rows(store.dashboards(), cx);
    }

    /// Why Save is refused while `dashboards.json` could not be read, if it
    /// could not.
    fn dashboards_refusal(&self) -> Option<SharedString> {
        self.dashboards_error
            .as_ref()
            .map(|error| ts!("settings.dashboards.load_failed", error = error.clone()))
    }

    /// Append an empty dashboard, expanded on the field the user came to fill.
    fn add_dashboard_row(&mut self, cx: &mut Context<Self>) {
        let mut row = Self::dashboard_row(cx, self.dashboard_rows.len());
        row.open = true;
        let pane = Self::pane_row(cx, row.tab_base, 0);
        row.panes.push(pane);
        self.dashboard_rows.push(row);
        cx.notify();
    }

    /// Drop the dashboard at `index`.
    fn remove_dashboard_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.dashboard_rows.len() {
            return;
        }
        self.dashboard_rows.remove(index);
        // The open list, if there was one, belonged to a row that has just
        // moved or gone; leaving it open would hang it off the wrong pane.
        self.close_lists(cx);
        cx.notify();
    }

    /// Expand or collapse the dashboard at `index`.
    fn set_dashboard_open(&mut self, index: usize, open: bool, cx: &mut Context<Self>) {
        let Some(row) = self.dashboard_rows.get_mut(index) else {
            return;
        };
        row.open = open;
        if open && row.panes.is_empty() {
            // Opening an empty dashboard on nothing but a button says less
            // than opening it on the row the user came to fill in.
            let base = row.tab_base;
            let pane = Self::pane_row(cx, base, 0);
            self.dashboard_rows[index].panes.push(pane);
        }
        cx.notify();
    }

    /// Mark or unmark the dashboard at `index` as one that opens at launch.
    ///
    /// Any number of them may be marked — each opens its own tab — so this sets
    /// one row and leaves the others alone rather than moving a single choice
    /// from one dashboard to another.
    fn set_dashboard_open_at_startup(
        &mut self,
        index: usize,
        open_at_startup: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.dashboard_rows.get_mut(index) else {
            return;
        };
        row.open_at_startup = open_at_startup;
        cx.notify();
    }

    /// Append an empty pane row to the dashboard at `index`.
    fn add_pane_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.dashboard_rows.get(index) else {
            return;
        };
        let pane = Self::pane_row(cx, row.tab_base, row.panes.len());
        self.dashboard_rows[index].panes.push(pane);
        cx.notify();
    }

    /// Drop pane `pane` of the dashboard at `index`.
    fn remove_pane_row(&mut self, index: usize, pane: usize, cx: &mut Context<Self>) {
        let Some(row) = self.dashboard_rows.get_mut(index) else {
            return;
        };
        if pane >= row.panes.len() {
            return;
        }
        row.panes.remove(pane);
        self.close_lists(cx);
        cx.notify();
    }

    /// Point pane `pane` of the dashboard at `index` at profile `picked`.
    ///
    /// `None` is the dead row a dangling pane is given: picking it again is not
    /// a choice of connection, so the id the pane already carries is kept and
    /// the user can still see which one it was.
    fn set_pane_profile(
        &mut self,
        index: usize,
        pane: usize,
        picked: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = picked
            .and_then(|slot| self.profiles.get(slot))
            .map(|(id, _)| *id)
        else {
            return;
        };
        let Some(slot) = self
            .dashboard_rows
            .get_mut(index)
            .and_then(|row| row.panes.get_mut(pane))
        else {
            return;
        };
        slot.profile = Some(id);
        cx.notify();
    }

    /// Apply row `option` of the color picker of pane `pane` of dashboard
    /// `index`: the default, a palette color, or the user's own.
    ///
    /// Choosing a color of one's own starts the picker from the palette
    /// color the header wears now, so the custom color begins as a variation
    /// on the one already picked.
    fn set_pane_color(&mut self, index: usize, pane: usize, option: usize, cx: &mut Context<Self>) {
        let Some(slot) = self
            .dashboard_rows
            .get_mut(index)
            .and_then(|row| row.panes.get_mut(pane))
        else {
            return;
        };
        match option {
            0 => {
                slot.color = None;
                slot.custom_chosen = false;
            }
            n if n <= HEADER_COLORS.len() => {
                slot.color = Some(HEADER_COLORS[n - 1].0.to_owned());
                slot.custom_chosen = false;
            }
            _ => {
                if !slot.custom_chosen
                    && let Some(color) = slot
                        .color
                        .as_deref()
                        .and_then(header_hex)
                        .and_then(|hex| Hsv::from_hex(&hex))
                {
                    slot.custom_color = color;
                }
                slot.custom_chosen = true;
                slot.picker_open = true;
            }
        }
        if !slot.custom_chosen {
            slot.picker_open = false;
        }
        cx.notify();
    }

    /// Fold the color picker of pane `pane` of dashboard `index` away, the
    /// color it holds kept.
    fn close_pane_picker(&mut self, index: usize, pane: usize, cx: &mut Context<Self>) {
        let Some(slot) = self
            .dashboard_rows
            .get_mut(index)
            .and_then(|row| row.panes.get_mut(pane))
        else {
            return;
        };
        slot.picker_open = false;
        cx.notify();
    }

    /// Move the custom color of pane `pane` of dashboard `index` to `color`,
    /// as the picker under the row asks.
    fn set_custom_color(&mut self, index: usize, pane: usize, color: Hsv, cx: &mut Context<Self>) {
        let Some(slot) = self
            .dashboard_rows
            .get_mut(index)
            .and_then(|row| row.panes.get_mut(pane))
        else {
            return;
        };
        slot.custom_color = color;
        cx.notify();
    }

    /// The content of every dashboard row, in order.
    fn dashboard_fields(&self, cx: &App) -> Vec<DashboardFields> {
        self.dashboard_rows
            .iter()
            .map(|row| DashboardFields {
                id: row.id,
                name: text(&row.name, cx),
                open_at_startup: row.open_at_startup,
                panes: row
                    .panes
                    .iter()
                    .map(|pane| PaneFields {
                        profile: pane.profile,
                        path: text(&pane.path, cx),
                        color: if pane.custom_chosen {
                            Some(pane.custom_color.to_hex())
                        } else {
                            pane.color.clone()
                        },
                    })
                    .collect(),
            })
            .collect()
    }

    /// The dashboards the form describes, or `None` while a pane is unfinished.
    fn dashboards(&self, cx: &App) -> Option<Vec<Dashboard>> {
        collect_dashboards(&self.dashboard_fields(cx))
    }

    /// The "Appearance" section.
    fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();
        // Built before the section is assembled, because `section` borrows the
        // context to read the theme and this borrows it mutably to listen.
        // A view of its own, drawn under the picker it manages; the tab
        // indices it takes start at `tab::UI_THEME_ACTIONS` and were fixed at
        // construction.
        let theme_actions = self.ui_theme_actions.clone();

        let theme_picker = SchemeSelect::new("settings-ui-theme")
            .chevron_icon(icons::CHEVRON_DOWN)
            .options_shared(self.cached_palettes(cx).0)
            .selected(Some(self.ui_theme.clone()))
            .open(self.open_list == Some(OpenList::UiTheme))
            .tab_index(tab::UI_THEME)
            .scroll_handle(self.ui_theme_scroll.clone())
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
            .chevron_icon(icons::CHEVRON_DOWN)
            .options(Self::language_options())
            .selected(self.language.as_deref().and_then(i18n::display_name))
            .placeholder(system_default())
            .open(self.open_list == Some(OpenList::Language))
            .tab_index(tab::LANGUAGE)
            .scroll_handle(self.language_scroll.clone())
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
                    ts!("settings.ui_font_size"),
                    suffixed(
                        self.ui_font_size_input.clone(),
                        ts!("settings.font_size_hint"),
                        cx,
                    ),
                ))
                .child(form_row(
                    ts!("settings.opacity"),
                    suffixed(self.opacity_input.clone(), ts!("settings.opacity_hint"), cx),
                ))
                .child(form_row("", blur)),
        )
    }

    /// The "Terminal" section.
    fn render_terminal(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();
        // Hoisted for the same reason the appearance section's row is.
        // As above, from `tab::SCHEME_ACTIONS`.
        let scheme_actions = self.scheme_actions.clone();

        let picker = SchemeSelect::new("settings-scheme")
            .chevron_icon(icons::CHEVRON_DOWN)
            .options_shared(self.cached_palettes(cx).1)
            .selected(Some(self.scheme.clone()))
            .open(self.open_list == Some(OpenList::Scheme))
            .tab_index(tab::SCHEME)
            .scroll_handle(self.scheme_scroll.clone())
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
            .chevron_icon(icons::CHEVRON_DOWN)
            .options_shared(self.cached_fonts())
            .selected(self.font_family.clone())
            .placeholder(system_default())
            .open(self.open_list == Some(OpenList::Font))
            .tab_index(tab::FONT_FAMILY)
            .scroll_handle(self.font_scroll.clone())
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

        let warn_multiline_paste = Checkbox::new(
            "settings-warn-multiline-paste",
            ts!("settings.warn_multiline_paste"),
        )
        .checked(self.warn_multiline_paste)
        .tab_index(tab::WARN_MULTILINE_PASTE)
        .on_toggle({
            let this = this.clone();
            move |checked, _window, cx| {
                this.update(cx, |dialog, cx| {
                    dialog.warn_multiline_paste = checked;
                    cx.notify();
                });
            }
        });

        let confirm_close = Checkbox::new(
            "settings-confirm-close",
            ts!("settings.confirm_close_with_sessions"),
        )
        .checked(self.confirm_close_with_sessions)
        .tab_index(tab::CONFIRM_CLOSE)
        .on_toggle({
            let this = this.clone();
            move |checked, _window, cx| {
                this.update(cx, |dialog, cx| {
                    dialog.confirm_close_with_sessions = checked;
                    cx.notify();
                });
            }
        });

        let local_file_panel = Checkbox::new(
            "settings-local-file-panel",
            ts!("settings.local_file_panel"),
        )
        .checked(self.local_file_panel)
        .tab_index(tab::LOCAL_FILE_PANEL)
        .on_toggle({
            let this = this.clone();
            move |checked, _window, cx| {
                this.update(cx, |dialog, cx| {
                    dialog.local_file_panel = checked;
                    cx.notify();
                });
            }
        });

        let editor_word_wrap = Checkbox::new(
            "settings-editor-word-wrap",
            ts!("settings.editor_word_wrap"),
        )
        .checked(self.editor_word_wrap)
        .tab_index(tab::EDITOR_WORD_WRAP)
        .on_toggle({
            let this = this.clone();
            move |checked, _window, cx| {
                this.update(cx, |dialog, cx| {
                    dialog.editor_word_wrap = checked;
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
                .child(form_row("", copy_on_select))
                .child(form_row("", warn_multiline_paste))
                .child(form_row("", confirm_close))
                .child(form_row("", local_file_panel))
                .child(form_row("", editor_word_wrap)),
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

    /// The "Dashboards" section.
    ///
    /// One collapsible per dashboard, each a small table of "this file, on that
    /// connection". The table is the same shape as the connection dialog's
    /// tunnel and followed-file sections, because it is the same gesture: a
    /// list the user grows a row at a time and empties again from the end.
    fn render_dashboards(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        // Read once for the whole section: the header of each dashboard shows
        // the name it would be saved under, which is a question about all of
        // them at once — a blank one is numbered around the names already
        // taken.
        let fields = self.dashboard_fields(cx);
        let titles = dashboard_names(&fields);
        let missing = ts!("settings.dashboards.missing_profile");

        let mut cards = Vec::with_capacity(self.dashboard_rows.len());
        for (index, row) in self.dashboard_rows.iter().enumerate() {
            let base = row.tab_base;
            // Counts the files that are actually named. A pane row cannot be
            // half-written the way a tunnel rule can, so what has been started
            // and what would be shown are the same number.
            let named = fields[index]
                .panes
                .iter()
                .filter(|pane| !pane.path.trim().is_empty())
                .count();
            // Two keys rather than a plural rule, as everywhere else a count is
            // put on a section header.
            let summary = match named {
                0 => ts!("settings.dashboards.none"),
                1 => ts!("settings.dashboards.one"),
                many => ts!("settings.dashboards.many", count = many),
            };

            let body = row.open.then(|| {
                let header = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(
                        div()
                            .flex_none()
                            .w(px(DASHBOARD_PROFILE_WIDTH))
                            .child(ts!("settings.dashboards.profile")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(ts!("settings.dashboards.path")),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(DASHBOARD_COLOR_WIDTH))
                            .child(ts!("settings.dashboards.color")),
                    )
                    // Holds the column of the per-row remove action open, so the
                    // headings stay over the fields they name.
                    .child(div().flex_none().w(px(DASHBOARD_ACTION_WIDTH)));

                let mut panes = Vec::with_capacity(row.panes.len());
                for (position, pane) in row.panes.iter().enumerate() {
                    let open = self.open_list
                        == Some(OpenList::Pane {
                            dashboard: index,
                            pane: position,
                        });
                    // A pane whose connection has been deleted says so and keeps
                    // the id: rebinding it to whichever profile now sits at that
                    // position would open a different file than the one asked for.
                    let dangling = pane
                        .profile
                        .is_some_and(|id| !self.profiles.iter().any(|(known, _)| *known == id));

                    let mut options = Vec::with_capacity(self.profiles.len() + 1);
                    if dangling {
                        options.push(missing.clone());
                    }
                    options.extend(self.profiles.iter().map(|(_, name)| name.clone()));
                    let selected = if dangling {
                        Some(missing.clone())
                    } else {
                        pane.profile.and_then(|id| {
                            self.profiles
                                .iter()
                                .find(|(known, _)| *known == id)
                                .map(|(_, name)| name.clone())
                        })
                    };

                    let id = index * PANE_IDS_PER_DASHBOARD + position;
                    let picker = Select::new(ElementId::from(("settings-dashboard-profile", id)))
                        .chevron_icon(icons::CHEVRON_DOWN)
                        .options(options)
                        .selected(selected)
                        .placeholder(ts!("settings.dashboards.profile"))
                        .open(open)
                        .tab_index(pane.tab_index)
                        .scroll_handle(self.pane_scroll.clone())
                        .on_select({
                            let this = this.clone();
                            // By index, not by label: two connections may share a
                            // name, and the dead row of a dangling pane is not a
                            // connection at all.
                            move |option, _label, _window, cx| {
                                let picked = if dangling {
                                    option.checked_sub(1)
                                } else {
                                    Some(option)
                                };
                                this.update(cx, |dialog, cx| {
                                    dialog.set_pane_profile(index, position, picked, cx);
                                });
                            }
                        })
                        .on_open_change({
                            let this = this.clone();
                            move |open, _window, cx| {
                                this.update(cx, |dialog, cx| {
                                    let list = OpenList::Pane {
                                        dashboard: index,
                                        pane: position,
                                    };
                                    dialog.set_list_open(list, open, cx);
                                });
                            }
                        });

                    let color_open = self.open_list
                        == Some(OpenList::PaneColor {
                            dashboard: index,
                            pane: position,
                        });
                    let custom = pane.custom_chosen.then(|| pane.custom_color.to_hex());
                    let colors = header_color_options(custom.as_deref(), theme.surface);
                    let selected = colors[header_color_option(pane)].label.clone();
                    let color_picker =
                        Select::new(ElementId::from(("settings-dashboard-color", id)))
                            .chevron_icon(icons::CHEVRON_DOWN)
                            .options(colors)
                            .selected(Some(selected))
                            .open(color_open)
                            .tab_index(pane.tab_index + 2)
                            .scroll_handle(self.pane_scroll.clone())
                            .on_select({
                                let this = this.clone();
                                move |option, _label, _window, cx| {
                                    this.update(cx, |dialog, cx| {
                                        dialog.set_pane_color(index, position, option, cx);
                                    });
                                }
                            })
                            .on_open_change({
                                let this = this.clone();
                                move |open, _window, cx| {
                                    this.update(cx, |dialog, cx| {
                                        let list = OpenList::PaneColor {
                                            dashboard: index,
                                            pane: position,
                                        };
                                        dialog.set_list_open(list, open, cx);
                                    });
                                }
                            });

                    let remove = Button::new(
                        ElementId::from(("settings-dashboard-pane-remove", id)),
                        ts!("settings.dashboards.remove_pane"),
                    )
                    .variant(ButtonVariant::Ghost)
                    .compact()
                    .tab_index(pane.tab_index + 6)
                    .on_click({
                        let this = this.clone();
                        move |_, _window, cx| {
                            this.update(cx, |dialog, cx| {
                                dialog.remove_pane_row(index, position, cx)
                            });
                        }
                    });

                    // Under the color and action columns, while the row wears
                    // a color of its own and the user has not said *Done*.
                    let custom_picker = (pane.custom_chosen && pane.picker_open).then(|| {
                        let this = this.clone();
                        let done = Button::new(
                            ElementId::from(("settings-dashboard-picker-done", id)),
                            ts!("settings.dashboards.color_done"),
                        )
                        .variant(ButtonVariant::Secondary)
                        .compact()
                        .tab_index(pane.tab_index + 5)
                        .on_click({
                            let this = this.clone();
                            move |_, _window, cx| {
                                this.update(cx, |dialog, cx| {
                                    dialog.close_pane_picker(index, position, cx);
                                });
                            }
                        });
                        div().flex().flex_row().child(div().flex_1()).child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.))
                                .flex_none()
                                .w(px(DASHBOARD_COLOR_WIDTH + 6. + DASHBOARD_ACTION_WIDTH))
                                .child(
                                    ColorPicker::new(ElementId::from((
                                        "settings-dashboard-picker",
                                        id,
                                    )))
                                    .value(pane.custom_color)
                                    .field_height(px(110.))
                                    .presets(header_swatches())
                                    .tab_index(pane.tab_index + 3)
                                    .on_change(
                                        move |color, _window, cx| {
                                            this.update(cx, |dialog, cx| {
                                                dialog.set_custom_color(index, position, color, cx);
                                            });
                                        },
                                    ),
                                )
                                .child(div().flex().flex_row().justify_end().child(done)),
                        )
                    });

                    panes.push(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(DASHBOARD_PROFILE_WIDTH))
                                    .child(picker),
                            )
                            .child(div().flex_1().min_w_0().child(pane.path.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(DASHBOARD_COLOR_WIDTH))
                                    .child(color_picker),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .w(px(DASHBOARD_ACTION_WIDTH))
                                    .justify_end()
                                    .child(remove),
                            ),
                    );
                    panes.extend(custom_picker);
                }

                let add_pane = Button::new(
                    ElementId::from(("settings-dashboard-add-pane", index)),
                    ts!("settings.dashboards.add_pane"),
                )
                .variant(ButtonVariant::Secondary)
                .tab_index(base + tab::DASHBOARD_PANE_ADD)
                .on_click({
                    let this = this.clone();
                    move |_, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.add_pane_row(index, cx));
                    }
                });

                let remove = Button::new(
                    ElementId::from(("settings-dashboard-remove", index)),
                    ts!("settings.dashboards.remove"),
                )
                .variant(ButtonVariant::Ghost)
                .tab_index(base + tab::DASHBOARD_REMOVE)
                .on_click({
                    let this = this.clone();
                    move |_, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.remove_dashboard_row(index, cx));
                    }
                });

                // Sits under the name because it is the other thing the dashboard
                // *is*, rather than one of the files it holds; the table below is
                // the list, and this would read as a column heading inside it.
                let open_at_startup = Checkbox::new(
                    ElementId::from(("settings-dashboard-startup", index)),
                    ts!("settings.dashboards.open_at_startup"),
                )
                .checked(row.open_at_startup)
                .tab_index(base + tab::DASHBOARD_OPEN_AT_STARTUP)
                .on_toggle({
                    let this = this.clone();
                    move |checked, _window, cx| {
                        this.update(cx, |dialog, cx| {
                            dialog.set_dashboard_open_at_startup(index, checked, cx);
                        });
                    }
                });

                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(form_row(ts!("settings.dashboards.name"), row.name.clone()))
                    .child(form_row("", open_at_startup))
                    .when(!panes.is_empty(), |this| this.child(header))
                    .children(panes)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .pt(px(2.))
                            .child(add_pane)
                            .child(remove),
                    )
            });

            cards.push(
                Collapsible::new(
                    ElementId::from(("settings-dashboard", index)),
                    SharedString::from(titles[index].clone()),
                )
                .open(row.open)
                .arrow_icons(icons::CHEVRON_RIGHT, icons::CHEVRON_DOWN)
                .tab_index(base)
                // A table, which draws its own columns from the left edge of
                // the card.
                .indent(false)
                .trailing(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(summary),
                )
                .on_toggle({
                    let this = this.clone();
                    move |open, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.set_dashboard_open(index, open, cx));
                    }
                })
                .children(body),
            );
        }

        let add = Button::new("settings-dashboard-add", ts!("settings.dashboards.add"))
            .variant(ButtonVariant::Secondary)
            .tab_index(tab::DASHBOARD_ADD)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| {
                    this.update(cx, |dialog, cx| dialog.add_dashboard_row(cx));
                }
            });

        section(
            ts!("settings.dashboards.title"),
            cx,
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(hint(ts!("settings.dashboards.hint"), cx))
                // A dashboard is files over saved connections, and without
                // one the connection picker of every pane would be empty.
                .when(self.profiles.is_empty(), |this| {
                    this.child(hint(ts!("settings.dashboards.no_connections"), cx))
                })
                .children(cards)
                .child(div().flex().flex_row().child(add)),
        )
    }

    /// The "Highlighting" section.
    ///
    /// Last of the five, under the dashboards, because it is the only one that
    /// is about what a followed file *looks* like rather than about what the
    /// application does — and because the list under it is the tallest thing in
    /// the dialog, which is better at the bottom of a scrolling body than in
    /// the middle of it.
    fn render_highlighting(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();

        let reset = Button::new(
            "settings-highlights-reset",
            ts!("settings.highlights.reset"),
        )
        .variant(ButtonVariant::Secondary)
        .tab_index(tab::HIGHLIGHTS_RESET)
        .on_click({
            let this = this.clone();
            move |_, _window, cx| {
                this.update(cx, |dialog, cx| {
                    dialog
                        .highlights
                        .clone()
                        .update(cx, |list, cx| list.set_rules(&highlight_preset(), cx));
                });
            }
        });

        section(
            ts!("settings.highlights.title"),
            cx,
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(hint(
                    ts!(
                        "settings.highlights.hint",
                        colors = highlight_color_examples()
                    ),
                    cx,
                ))
                .child(self.highlights.clone())
                .child(div().flex().flex_row().child(reset)),
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
            .child(self.render_section_index(cx))
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
                            .smooth_scroll(&self.body_scroll)
                            .restrict_scroll_to_axis()
                            .child(self.render_appearance(cx))
                            .child(self.render_terminal(cx))
                            .child(self.render_connection(cx))
                            .child(self.render_dashboards(cx))
                            .child(self.render_highlighting(cx)),
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
        // The first frame after opening is the state edits are measured from;
        // see `has_unsaved_edits`.
        if self.base_form.is_none() {
            self.base_form = Some(self.form_snapshot(cx));
        }
        let theme = theme(cx);
        let body_bar = self.hovering_body_bar(cx);

        // While a color is being edited the form steps aside entirely rather
        // than being covered up, so that the window's tab ring holds only the
        // controls that are actually on screen; see [`rugpui_shell::theme_editor`].
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
            // The body's bar is answered from here: gpui hands a drag move to
            // every listener of that type wherever it sits, and the body scrolls
            // away under its own bar. The dropdowns answer for their lists' bars.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |dialog, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    dialog.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbar(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbar(cx);
                }),
            )
            .child(modal(
                "settings-modal",
                title,
                px(DIALOG_WIDTH),
                body,
                on_dismiss,
            ))
            .children(self.render_discard_confirm(cx))
    }
}

/// The sections of the settings body, in the order they are drawn — which is
/// also their index among the body's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    /// The saved dashboards, fourth of the five.
    Dashboards = 3,
}

/// The form reduced to what [`SettingsDialog::has_unsaved_edits`] compares:
/// the settings Save would write, the dashboards and the highlighting rules.
#[derive(Debug, Clone, PartialEq)]
struct FormSnapshot {
    settings: AppSettings,
    dashboards: Vec<DashboardFields>,
    highlights: Vec<crate::highlight_rules::HighlightRuleFields>,
}

/// Trimmed content of `input`, or `None` when it is blank.
///
/// The one field helper that is not [`rugpui_shell::form`]'s: only rulogman has a
/// settings field whose empty state is a meaningful `None` — the default login
/// name, where blank means "do not offer one".
fn optional_text(input: &Entity<TextInput>, cx: &App) -> Option<String> {
    let value = text(input, cx);
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_settings_window(
        cx: &mut gpui::TestAppContext,
    ) -> gpui::WindowHandle<SettingsDialog> {
        cx.update(rugpui::init);
        cx.add_window(|_, cx| {
            let mut dialog = SettingsDialog::new(cx);
            dialog.fill_form(&AppSettings::default(), cx);
            dialog.open = true;
            dialog
        })
    }

    /// *Manage dashboards…* opens the dialog on the dashboards, and the
    /// section index goes wherever it is pointed.
    #[gpui::test]
    fn the_dialog_opens_on_the_section_it_was_asked_for(cx: &mut gpui::TestAppContext) {
        let window = default_settings_window(cx);
        window
            .update(cx, |dialog, _, cx| {
                let dir = tempfile::tempdir().expect("tempdir");
                dialog.dashboards_file = Some(dir.path().join("dashboards.json"));
                dialog.open_at(SettingsSection::Dashboards, cx);
                assert!(dialog.open);
                assert_eq!(dialog.visible_section, SettingsSection::Dashboards as usize);
                dialog.show_section(0, cx);
                assert_eq!(dialog.visible_section, 0);
            })
            .unwrap();
    }

    /// `Escape` over an untouched form closes it; over an edited one it asks,
    /// and *Keep editing* leaves the edit where it was.
    #[gpui::test]
    fn escape_asks_before_it_throws_away_edited_settings(cx: &mut gpui::TestAppContext) {
        let window = default_settings_window(cx);
        window
            .update(cx, |dialog, _, cx| {
                dialog.base_form = Some(dialog.form_snapshot(cx));
                assert!(!dialog.has_unsaved_edits(cx));

                dialog.copy_on_select = !dialog.copy_on_select;
                assert!(dialog.has_unsaved_edits(cx));
                dialog.request_dismiss(cx);
                assert!(dialog.open && dialog.discard_confirm, "it asks");
                assert!(dialog.keep_editing(cx));
                assert!(dialog.open && dialog.has_unsaved_edits(cx));

                dialog.copy_on_select = !dialog.copy_on_select;
                dialog.request_dismiss(cx);
                assert!(!dialog.open, "an undone edit is no edit");
            })
            .unwrap();
    }

    /// The form over settings that could not be read is a form over defaults,
    /// and applying it would write them over the user's file.
    #[gpui::test]
    fn settings_that_could_not_be_read_are_not_applied(cx: &mut gpui::TestAppContext) {
        let window = default_settings_window(cx);
        cx.update(|cx| {
            app_settings::install(Err(anyhow::anyhow!("expected value at line 1")), cx);
        });
        window
            .update(cx, |dialog, _, cx| {
                // Returns before `dashboards.json` or `settings.json` is touched.
                dialog.save(cx);
                let status = dialog.status.clone().expect("the reason is on screen");
                assert!(!status.contains("settings."), "untranslated {status:?}");
                assert!(dialog.open, "the dialog stays up");
            })
            .unwrap();
    }

    /// *Custom…* unfolds the picker under its row, *Done* folds it away with
    /// the color kept, and choosing *Custom…* again unfolds it to change that
    /// color; a palette color folds it for good.
    #[gpui::test]
    fn the_custom_color_picker_unfolds_and_folds_away(cx: &mut gpui::TestAppContext) {
        let window = default_settings_window(cx);
        window
            .update(cx, |dialog, _, cx| {
                dialog.add_dashboard_row(cx);
                let custom = HEADER_COLORS.len() + 1;
                let pane = |dialog: &SettingsDialog| {
                    let pane = &dialog.dashboard_rows[0].panes[0];
                    (pane.custom_chosen, pane.picker_open)
                };

                dialog.set_pane_color(0, 0, custom, cx);
                assert_eq!(pane(dialog), (true, true), "Custom… did not unfold");
                let picked = Hsv::from_hex("#a1b2c3").expect("a color");
                dialog.set_custom_color(0, 0, picked, cx);

                dialog.close_pane_picker(0, 0, cx);
                assert_eq!(pane(dialog), (true, false), "Done did not fold");
                assert_eq!(
                    dialog.dashboard_fields(cx)[0].panes[0].color.as_deref(),
                    Some("#a1b2c3"),
                    "Done lost the color"
                );

                dialog.set_pane_color(0, 0, custom, cx);
                assert_eq!(pane(dialog), (true, true), "Custom… again did not unfold");
                assert_eq!(dialog.dashboard_rows[0].panes[0].custom_color, picked);

                dialog.set_pane_color(0, 0, 1, cx);
                assert_eq!(pane(dialog), (false, false), "a palette color left it open");
            })
            .unwrap();
    }

    /// Dashboards that could not be read open as an empty section, and Save
    /// writes the whole section, so applying the form would empty the file.
    #[gpui::test]
    fn dashboards_that_could_not_be_read_are_not_saved_over(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");
        let corrupt = b"{\"dashboards\": [{\"name\": \"Morning\", ".to_vec();
        std::fs::write(&path, &corrupt).expect("write the corrupt file");

        let window = default_settings_window(cx);
        window
            .update(cx, |dialog, _, cx| {
                dialog.dashboards_file = Some(path.clone());
                dialog.open(cx);
                let status = dialog.status.clone().expect("the reason shows on opening");
                assert!(!status.contains("settings."), "untranslated {status:?}");
                assert!(dialog.dashboard_rows.is_empty());

                // Even with a dashboard added, nothing is written.
                dialog.add_dashboard_row(cx);
                dialog.status = None;
                dialog.save(cx);
                let status = dialog.status.clone().expect("the refusal is on screen");
                assert!(!status.contains("settings."), "untranslated {status:?}");
                assert!(dialog.open, "the dialog stays up");
            })
            .unwrap();

        let on_disk = std::fs::read(&path).expect("the file is still there");
        assert_eq!(
            on_disk, corrupt,
            "the unreadable file was left byte for byte"
        );
    }

    #[gpui::test]
    fn settings_body_shrinks_with_the_window_and_keeps_its_scroll_range(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = default_settings_window(cx);
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        for (width, height) in [(1200., 900.), (1000., 600.), (900., 400.)] {
            cx.simulate_resize(gpui::size(px(width), px(height)));
            cx.run_until_parked();
            let bounds = window
                .update(&mut cx, |dialog, _, cx| {
                    let bounds = dialog.body_scroll.bounds();
                    assert!(bounds.size.width > px(600.));
                    assert!(bounds.size.height > px(0.));
                    assert!(bounds.size.height <= px(BODY_MAX_HEIGHT));
                    assert!(bounds.top() > px(0.));
                    assert!(bounds.bottom() < px(height - 48.));
                    assert!(dialog.body_scroll.max_offset().y > px(0.));
                    dialog
                        .body_scroll
                        .set_offset(gpui::point(px(0.), -dialog.body_scroll.max_offset().y));
                    cx.notify();
                    bounds
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(&mut cx, |dialog, _, _| {
                    assert_eq!(dialog.body_scroll.bounds(), bounds);
                    assert_eq!(
                        dialog.body_scroll.offset().y,
                        -dialog.body_scroll.max_offset().y
                    );
                })
                .unwrap();
        }
    }

    // Manual CPU-frame benchmark: this includes the real settings tree, its
    // default highlight rules and layout, but excludes GPU presentation.
    #[gpui::test]
    #[ignore = "manual settings frame benchmark"]
    fn profile_settings_frames(cx: &mut gpui::TestAppContext) {
        let window = default_settings_window(cx);
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(gpui::size(px(1200.), px(900.)));
        cx.run_until_parked();
        let mut times = Vec::with_capacity(200);
        for _ in 0..200 {
            let start = std::time::Instant::now();
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            times.push(start.elapsed().as_micros());
        }
        times.sort_unstable();
        eprintln!(
            "Settings CPU frame (microseconds): p50={}, p95={}, max={}",
            times[100], times[190], times[199]
        );
    }

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
    fn every_slot_of_both_catalogs_has_a_label_to_draw() {
        // The editor looks a slot's label up by key as it draws, so a key that
        // is not in `locales/*.yml` reaches the screen as the key path. The
        // chrome catalog's slots are the shell's — including the five grid
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
        // The headings a catalog asks for between those slots are looked up
        // the same way, and one naming a slot that is not there would be drawn
        // over nothing.
        for (slots, headings) in [
            (ui.slots(), ui.group_headings()),
            (scheme.slots(), scheme.group_headings()),
        ] {
            for (index, key) in headings {
                let label = ts!(key);
                assert!(!label.is_empty(), "{key} has an empty label");
                assert!(
                    !label.contains("settings."),
                    "{key} is untranslated: {label:?}"
                );
                assert!(index < slots.len(), "{key} stands in front of no slot");
            }
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

    /// A dashboard row with `name` and the panes given, unmarked for startup.
    fn dash(name: &str, panes: Vec<PaneFields>) -> DashboardFields {
        DashboardFields {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            open_at_startup: false,
            panes,
        }
    }

    /// A pane row pointing at `profile` and naming `path`.
    fn pane(profile: Option<Uuid>, path: &str) -> PaneFields {
        PaneFields {
            profile,
            path: path.to_owned(),
            color: None,
        }
    }

    #[test]
    fn every_word_the_dashboard_section_asks_for_has_a_translation() {
        // The section looks its words up by key as it draws, so a key that is
        // not in `locales/*.yml` reaches the screen as the key path and nothing
        // else would notice.
        let translated = |label: SharedString| {
            assert!(!label.is_empty(), "empty label");
            assert!(!label.contains("settings."), "untranslated label {label:?}");
        };

        for key in [
            "settings.dashboards.title",
            "settings.dashboards.hint",
            "settings.dashboards.none",
            "settings.dashboards.one",
            "settings.dashboards.add",
            "settings.dashboards.remove",
            "settings.dashboards.name",
            "settings.dashboards.open_at_startup",
            "settings.dashboards.profile",
            "settings.dashboards.path",
            "settings.dashboards.add_pane",
            "settings.dashboards.remove_pane",
            "settings.dashboards.missing_profile",
            "settings.dashboards.incomplete",
        ] {
            translated(ts!(key));
        }
        // The welcome screen's heading is worded here rather than there, so it
        // is checked here too.
        let heading = ts!("empty.dashboards");
        assert!(!heading.is_empty());
        assert!(!heading.contains("empty."), "{heading:?}");

        let many = ts!("settings.dashboards.many", count = 3);
        translated(many.clone());
        assert!(many.contains('3'), "the count is dropped: {many:?}");
    }

    #[test]
    fn a_pane_with_no_path_is_dropped_rather_than_refused() {
        // The row every section ends on once "Add file" has been pressed. An
        // empty form is not a mistake, so it must not block Save.
        let profile = Uuid::new_v4();
        let rows = [dash(
            "deploy",
            vec![
                pane(Some(profile), "/var/log/syslog"),
                pane(Some(profile), "   "),
                pane(None, ""),
            ],
        )];

        let dashboards = collect_dashboards(&rows).expect("the blank rows are ignored");
        assert_eq!(dashboards.len(), 1);
        assert_eq!(dashboards[0].panes.len(), 1);
        assert_eq!(dashboards[0].panes[0].path, "/var/log/syslog");
        assert_eq!(dashboards[0].panes[0].profile, profile);
    }

    #[test]
    fn a_named_file_on_no_connection_refuses_the_form() {
        // Nothing has been lost here: the user named a file and has not yet
        // said where it lives. Saving would drop the path they typed.
        let rows = [dash("deploy", vec![pane(None, "/var/log/syslog")])];
        assert!(collect_dashboards(&rows).is_none());
    }

    #[test]
    fn a_named_file_on_a_deleted_connection_is_kept_exactly_as_it_was() {
        // The opposite case, and the opposite answer. The id is the only
        // record of which host the file was on, so it survives the save and
        // the picker goes on drawing the row as dead until the user repoints
        // it. A dashboard broken by an unrelated deletion must not hold the
        // rest of the dialog — the font size — hostage.
        let deleted = Uuid::new_v4();
        let rows = [dash("deploy", vec![pane(Some(deleted), "/var/log/syslog")])];

        let dashboards = collect_dashboards(&rows).expect("the dangling pane is kept");
        assert_eq!(dashboards[0].panes.len(), 1);
        assert_eq!(dashboards[0].panes[0].profile, deleted);
        assert_eq!(dashboards[0].panes[0].path, "/var/log/syslog");
    }

    #[test]
    fn the_color_picker_offers_the_palette_and_a_color_of_ones_own() {
        let strip = gpui::black();
        let options = header_color_options(None, strip);
        assert_eq!(options.len(), HEADER_COLORS.len() + 2);
        assert_eq!(options[0].swatch, Some(strip));
        for (slot, (name, ..)) in HEADER_COLORS.iter().enumerate() {
            assert_eq!(options[slot + 1].swatch, header_fill(name), "{name}");
        }
        // *Custom…* shows no color until one is typed, then that one.
        assert_eq!(options.last().and_then(|option| option.swatch), None);
        let typed = header_color_options(Some("#a1b2c3"), strip);
        assert_eq!(
            typed.last().and_then(|option| option.swatch),
            header_fill("#a1b2c3")
        );
    }

    #[test]
    fn a_header_color_the_form_cannot_edit_survives_the_save() {
        let mut tinted = pane(Some(Uuid::new_v4()), "/var/log/syslog");
        tinted.color = Some("green".to_owned());
        let rows = [dash("deploy", vec![tinted])];

        let dashboards = collect_dashboards(&rows).expect("kept");
        assert_eq!(dashboards[0].panes[0].color.as_deref(), Some("green"));
    }

    #[test]
    fn a_dangling_pane_saves_while_an_unchosen_one_beside_it_still_refuses() {
        // The two cases in one form, to pin down that the collector tells them
        // apart by whether an id is there at all, never by whether it resolves.
        let live = Uuid::new_v4();
        let deleted = Uuid::new_v4();

        let rows = [dash(
            "deploy",
            vec![
                pane(Some(live), "/var/log/nginx/error.log"),
                pane(Some(deleted), "/var/log/syslog"),
            ],
        )];
        let dashboards = collect_dashboards(&rows).expect("both panes are kept");
        let panes: Vec<(Uuid, &str)> = dashboards[0]
            .panes
            .iter()
            .map(|pane| (pane.profile, pane.path.as_str()))
            .collect();
        assert_eq!(
            panes,
            [
                (live, "/var/log/nginx/error.log"),
                (deleted, "/var/log/syslog"),
            ]
        );

        // One unfinished row anywhere in the form is still a refusal.
        let rows = [dash(
            "deploy",
            vec![
                pane(Some(deleted), "/var/log/syslog"),
                pane(None, "/var/log/auth.log"),
            ],
        )];
        assert!(collect_dashboards(&rows).is_none());
    }

    #[test]
    fn a_dashboard_with_no_panes_is_kept() {
        // Mid-edit is a legitimate state: refusing to save over it would mean
        // the user could not put the dialog down halfway.
        let dashboards = collect_dashboards(&[dash("empty", Vec::new())]).expect("kept");
        assert_eq!(dashboards.len(), 1);
        assert!(dashboards[0].panes.is_empty());
    }

    #[test]
    fn a_blank_name_is_filled_in_and_numbered_around_the_names_taken() {
        let rows = [
            dash("", Vec::new()),
            dash("  ", Vec::new()),
            dash("  prod  ", Vec::new()),
            dash("", Vec::new()),
        ];
        let names = dashboard_names(&rows);
        assert_eq!(names, ["Dashboard", "Dashboard 2", "prod", "Dashboard 3"]);

        // A name the user typed is never taken from them to fill a blank one.
        let rows = [dash("Dashboard", Vec::new()), dash("", Vec::new())];
        assert_eq!(dashboard_names(&rows), ["Dashboard", "Dashboard 2"]);

        // And what the header shows is what gets stored.
        let stored = collect_dashboards(&rows).expect("kept");
        let stored: Vec<&str> = stored.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(stored, ["Dashboard", "Dashboard 2"]);
    }

    #[test]
    fn collecting_keeps_the_identifier_a_row_was_built_with() {
        // Saving has to rename the stored dashboard rather than replace it, or
        // every edit would look like a delete and an insert to anything holding
        // the id — an open dashboard tab, for one.
        let row = dash("renamed", Vec::new());
        let id = row.id;
        let dashboards = collect_dashboards(&[row]).expect("kept");
        assert_eq!(dashboards[0].id, id);
    }

    #[test]
    fn the_startup_mark_is_carried_through_collecting_and_is_per_dashboard() {
        // The flag is the one thing on the form that is neither a name nor a
        // file, so nothing else would notice if collecting dropped it. Two rows
        // rather than one, because any number of dashboards may be marked: the
        // mark belongs to the row it was put on and to no other.
        let mut morning = dash("morning", Vec::new());
        morning.open_at_startup = true;
        let plain = dash("plain", Vec::new());

        let dashboards = collect_dashboards(&[morning, plain]).expect("kept");
        assert!(dashboards[0].open_at_startup);
        assert!(!dashboards[1].open_at_startup);
    }

    #[test]
    fn a_row_the_form_never_marked_saves_a_dashboard_that_does_not_open_at_startup() {
        // The default of the control and the default of the stored dashboard
        // have to agree, or merely opening the dialog and saving would mark or
        // unmark every dashboard the user has.
        let dashboards = collect_dashboards(&[dash("plain", Vec::new())]).expect("kept");
        assert_eq!(
            dashboards[0].open_at_startup,
            Dashboard::new("plain").open_at_startup
        );
        assert!(!dashboards[0].open_at_startup);
    }

    #[test]
    fn every_dashboard_block_stays_inside_the_indices_it_was_given() {
        // The section sits between the connection settings and the footer, and
        // a block runs from its disclosure to its own "Add file" button.
        const { assert!(tab::TIMEOUT < tab::DASHBOARDS) };
        const { assert!(tab::DASHBOARDS < tab::DASHBOARD_ADD) };
        const { assert!(tab::DASHBOARD_ADD < tab::CANCEL) };
        const { assert!(tab::DASHBOARD_NAME < tab::DASHBOARD_PANES) };
        const { assert!(tab::DASHBOARD_REMOVE < tab::DASHBOARD_PANES) };
        // The startup toggle sits between the dashboard's own controls and its
        // table, taking an index the block had left spare.
        const { assert!(tab::DASHBOARD_REMOVE < tab::DASHBOARD_OPEN_AT_STARTUP) };
        const { assert!(tab::DASHBOARD_OPEN_AT_STARTUP < tab::DASHBOARD_PANES) };
        const { assert!(tab::DASHBOARD_PANE_ADD < tab::DASHBOARD_STRIDE) };

        // However long either list grows, the numbering stays inside its
        // block: rows past what it can number share an index and tab in paint
        // order, which is the connection dialog's bargain too.
        for position in [0_usize, 1, 5, 50, 5_000] {
            let base = SettingsDialog::dashboard_tab_base(position);
            assert!(base >= tab::DASHBOARDS);
            assert!(base + tab::DASHBOARD_PANE_ADD < tab::DASHBOARD_ADD);
            for pane in [0_usize, 1, 25, 5_000] {
                let index = SettingsDialog::pane_tab_index(base, pane);
                assert!(index >= base + tab::DASHBOARD_PANES);
                // A pane row occupies the picker, the path, the color list, the
                // color picker, its hex field and its *Done*, and the button
                // that removes it, and all seven have to stay under "Add file".
                assert!(index + 6 < base + tab::DASHBOARD_PANE_ADD);
            }
        }
    }

    #[test]
    fn every_word_the_highlighting_section_asks_for_has_a_translation() {
        // The section's own three words; the rule rows' vocabulary is checked
        // beside the component that draws it.
        for key in ["settings.highlights.title", "settings.highlights.reset"] {
            let label = ts!(key);
            assert!(!label.is_empty(), "{key} is empty");
            assert!(
                !label.contains("settings."),
                "untranslated {key}: {label:?}"
            );
        }

        let hint = ts!(
            "settings.highlights.hint",
            colors = highlight_color_examples()
        );
        assert!(!hint.contains("settings."), "untranslated hint: {hint:?}");
        // The names really reach the sentence: an interpolation that silently
        // dropped them would leave a hint that teaches no vocabulary at all.
        assert!(
            hint.contains("bright_black") || hint.contains("green"),
            "{hint:?}"
        );

        for (key, argument) in [
            ("settings.highlights.bad_pattern", "(unclosed"),
            ("settings.highlights.bad_color", "reddish"),
        ] {
            let message = match key {
                "settings.highlights.bad_pattern" => ts!(key, pattern = argument),
                _ => ts!(key, color = argument),
            };
            assert!(!message.contains("settings."), "untranslated {key}");
            // The offending text is what the user has to go and find, so it
            // has to survive into the strip rather than being described.
            assert!(
                message.contains(argument),
                "{key} drops its argument: {message:?}"
            );
        }
    }

    #[test]
    fn the_highlight_list_never_reaches_the_footer_buttons() {
        // The list numbers its rows inside the span it was given; anything the
        // dialog draws after it has to start above that, or a long rule list
        // would tab into Cancel halfway down.
        const { assert!(tab::DASHBOARD_ADD < tab::HIGHLIGHTS) };
        const { assert!(tab::HIGHLIGHTS + crate::highlight_rules::TAB_SPAN <= tab::CANCEL) };
        const { assert!(tab::HIGHLIGHTS_RESET < tab::CANCEL) };
        const { assert!(tab::CANCEL < tab::SAVE) };
    }

    #[test]
    fn an_untouched_preset_is_stored_as_nothing_configured() {
        // What `save` writes back, in the two shapes that are easy to get
        // backwards: the preset is `None` so a later build's improved list
        // still arrives, and an emptied list is `Some([])` — highlighting off —
        // rather than `None`, which would silently hand the preset back.
        let stored = |rules: Vec<rulogman_core::HighlightRule>| {
            (!is_highlight_preset(&rules)).then_some(rules)
        };
        assert_eq!(stored(highlight_preset()), None);
        assert_eq!(stored(Vec::new()), Some(Vec::new()));

        let mut edited = highlight_preset();
        edited[0].enabled = false;
        assert_eq!(stored(edited.clone()), Some(edited));
    }

    #[test]
    fn a_blank_default_username_is_an_absence_rather_than_an_empty_name() {
        // The one form helper that stayed behind is `optional_text`, and this
        // is why: `default_username` is an `Option<String>`, so a blank field
        // has to come back as `None` and agree with the default.
        assert_eq!(AppSettings::default().connection.default_username, None);
    }

    #[gpui::test]
    fn unchanged_choices_are_reused_and_missing_fonts_remain_selectable(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(rugpui::init);
        let dialog = cx.new(SettingsDialog::new);
        dialog.update(cx, |dialog, cx| {
            dialog.fonts = (0..500)
                .map(|index| format!("Font {index}").into())
                .collect();
            dialog.font_family = None;
            let fonts = dialog.cached_fonts();
            assert_eq!(fonts.len(), 501);
            assert!(Rc::ptr_eq(&fonts, &dialog.cached_fonts()));
            dialog.font_family = Some("Uninstalled font".into());
            let changed = dialog.cached_fonts();
            assert_eq!(changed.len(), 502);
            assert!(!Rc::ptr_eq(&fonts, &changed));
            assert!(Rc::ptr_eq(&changed, &dialog.cached_fonts()));

            let palettes = dialog.cached_palettes(cx);
            let same = dialog.cached_palettes(cx);
            assert!(Rc::ptr_eq(&palettes.0, &same.0));
            assert!(Rc::ptr_eq(&palettes.1, &same.1));
            dialog.close_editor(true, cx);
            let refreshed = dialog.cached_palettes(cx);
            assert!(!Rc::ptr_eq(&palettes.0, &refreshed.0));
            assert!(!Rc::ptr_eq(&palettes.1, &refreshed.1));
        });
    }
}
