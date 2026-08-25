//! rulogman's terminal colour schemes, as a [`ThemeCatalog`].
//!
//! `rugpui-shell` manages a palette catalogue without knowing what kind of
//! palette it holds: [`CatalogActions`](rugpui_shell::CatalogActions) duplicates,
//! edits, deletes, imports and exports entries, and
//! [`ThemeEditor`](rugpui_shell::ThemeEditor) edits one colour by colour. Both are
//! written against [`ThemeCatalog`], and two implementations of it ship there —
//! one over `rugpui`'s chrome themes, one over its editor themes. A terminal
//! scheme is neither: it is Windows Terminal's format, so published palettes
//! work unchanged, and a widget kit has no terminal to draw one in.
//!
//! So this is the third implementation, and it lives here for the same reason
//! [`crate::theme_store`]'s scheme half does. The file travels through the
//! shell as [`CatalogFile::Other`], which the generic code moves around without
//! ever looking inside — every question it could ask of one is a method below.
//!
//! The twenty slots are the four terminal roles and then the sixteen ANSI
//! colours, and their order is load-bearing: [`SchemeCatalog::values_of`] and
//! [`SchemeCatalog::file_from`] read and write the fields by position.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{AnyElement, App, Hsla, IntoElement, SharedString, div, prelude::*, px};
use rugpui::ThemeFile;
use rugpui_shell::catalog::{CatalogEntry, CatalogFile, ImportError, Slot, ThemeCatalog, slot};
use rulogman_core::AppSettings;
use rulogman_term::{Rgb, SchemeFile, TerminalTheme};

use crate::theme_store;

/// The twenty scheme slots: the four terminal roles, then the ANSI palette.
///
/// Normal and bright are laid out one after the other so that the editor's two
/// columns pair each colour with its own bright variant. No slot takes an alpha
/// channel: a scheme file is Windows Terminal's, and nothing in it is drawn
/// translucent.
const SCHEME_SLOTS: [Slot; 20] = [
    slot("foreground", "settings.editor.term.foreground", false),
    slot("background", "settings.editor.term.background", false),
    slot("cursor", "settings.editor.term.cursor", false),
    slot("selection", "settings.editor.term.selection", false),
    slot("black", "settings.editor.term.black", false),
    slot("red", "settings.editor.term.red", false),
    slot("green", "settings.editor.term.green", false),
    slot("yellow", "settings.editor.term.yellow", false),
    slot("blue", "settings.editor.term.blue", false),
    slot("magenta", "settings.editor.term.magenta", false),
    slot("cyan", "settings.editor.term.cyan", false),
    slot("white", "settings.editor.term.white", false),
    slot("bright-black", "settings.editor.term.bright_black", false),
    slot("bright-red", "settings.editor.term.bright_red", false),
    slot("bright-green", "settings.editor.term.bright_green", false),
    slot("bright-yellow", "settings.editor.term.bright_yellow", false),
    slot("bright-blue", "settings.editor.term.bright_blue", false),
    slot(
        "bright-magenta",
        "settings.editor.term.bright_magenta",
        false,
    ),
    slot("bright-cyan", "settings.editor.term.bright_cyan", false),
    slot("bright-white", "settings.editor.term.bright_white", false),
];

/// The terminal colour schemes of [`TerminalTheme`], as a catalogue.
pub struct SchemeCatalog;

impl SchemeCatalog {
    /// The file, when it is one of ours.
    ///
    /// A catalogue only ever sees files it produced or read itself, so the
    /// `None` arm is unreachable in practice; it is what keeps a downcast out
    /// of the callers.
    fn file(file: &CatalogFile) -> Option<&SchemeFile> {
        match file {
            CatalogFile::Other(any) => any.downcast_ref::<SchemeFile>(),
            _ => None,
        }
    }

    /// The same file, wrapped the way [`CatalogFile::Other`] carries one.
    ///
    /// An [`Arc`] rather than a [`Box`] because the shell clones a
    /// [`CatalogFile`] — a subscription only ever borrows the event that
    /// carries one — and a `Box<dyn Any>` cannot be cloned without knowing what
    /// is inside it.
    fn wrap(file: SchemeFile) -> CatalogFile {
        CatalogFile::Other(Arc::new(file))
    }
}

/// The value of every slot of `file`, in [`SCHEME_SLOTS`] order.
///
/// Both optional keys are filled in from the derivation the loader would apply,
/// because the editor has a field for each of them and an empty field would
/// read as a colour nobody chose rather than as one the file leaves out.
fn scheme_values(file: &SchemeFile) -> Vec<String> {
    let theme = file.to_theme();
    vec![
        file.foreground.clone(),
        file.background.clone(),
        file.cursor_color
            .clone()
            .unwrap_or_else(|| theme.cursor.to_hex()),
        file.selection_background
            .clone()
            .unwrap_or_else(|| theme.selection.to_hex()),
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
    ]
}

/// A scheme file assembled from the fields, in [`SCHEME_SLOTS`] order.
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

impl ThemeCatalog for SchemeCatalog {
    fn kind_label_key(&self) -> &'static str {
        "settings.editor.scheme_title"
    }

    fn element_prefix(&self) -> &'static str {
        "settings-scheme-action"
    }

    fn delete_confirm_key(&self) -> &'static str {
        "settings.manage.delete_scheme_confirm"
    }

    fn entries(&self, _cx: &App) -> Vec<CatalogEntry> {
        TerminalTheme::all_schemes()
            .into_iter()
            .map(|entry| CatalogEntry {
                id: entry.id,
                name: entry.name,
                builtin: entry.builtin,
            })
            .collect()
    }

    fn slots(&self) -> &'static [Slot] {
        &SCHEME_SLOTS
    }

    fn load(&self, id: &str, cx: &App) -> Option<CatalogFile> {
        let entry = self.entry(id, cx)?;
        // Through the registry rather than off the disk, so a built-in scheme —
        // which has no file — duplicates and exports exactly like one of the
        // user's own.
        Some(Self::wrap(SchemeFile::from_theme(
            entry.name,
            &TerminalTheme::by_name_or_default(id),
        )))
    }

    /// No dark/light flag at all: a terminal palette *is* its background, and
    /// there is nothing for a second answer to change. The editor takes its
    /// checkbox away rather than drawing an inert one, and hands `file_from`
    /// back exactly the flag `values_of` reported.
    fn has_dark_flag(&self) -> bool {
        false
    }

    /// One heading, over the sixteen ANSI colours.
    ///
    /// Slot four is `black`, the first of them, so the four terminal roles
    /// above it stay under the editor's own opening rows and the palette below
    /// reads as the one list it is.
    fn group_headings(&self) -> Vec<(usize, &'static str)> {
        vec![(4, "settings.editor.term.ansi")]
    }

    fn values_of(&self, file: &CatalogFile) -> (Vec<String>, bool) {
        // The flag is `false` because a scheme has none; `has_dark_flag` is
        // what keeps the editor from offering to change it, and what brings it
        // back to `file_from` untouched.
        match Self::file(file) {
            Some(file) => (scheme_values(file), false),
            None => (Vec::new(), false),
        }
    }

    fn file_from(&self, name: String, values: &[String], _dark: bool) -> CatalogFile {
        Self::wrap(scheme_file(name, values))
    }

    fn dir(&self) -> Result<PathBuf> {
        rulogman_core::schemes_dir()
    }

    fn default_id(&self) -> String {
        AppSettings::default().terminal.scheme
    }

    fn generated_id_prefix(&self) -> &'static str {
        theme_store::GENERATED_SCHEME_ID
    }

    fn save(&self, id: &str, file: &CatalogFile) -> Result<PathBuf> {
        let file = Self::file(file).ok_or_else(|| anyhow::anyhow!("not a colour scheme"))?;
        theme_store::save_scheme(id, file)
    }

    fn write(&self, file: &CatalogFile, path: &Path) -> Result<()> {
        let file = Self::file(file).ok_or_else(|| anyhow::anyhow!("not a colour scheme"))?;
        theme_store::write_file(path, file)
    }

    fn delete(&self, id: &str) -> Result<()> {
        theme_store::delete_scheme(id)
    }

    fn read(&self, path: &Path) -> std::result::Result<CatalogFile, ImportError> {
        let file = match theme_store::read_file::<SchemeFile>(path) {
            Ok(file) => Self::wrap(file),
            // The two formats can always be told apart because neither one's
            // required keys are a subset of the other's: a chrome palette has to
            // carry `surface`, a scheme `black`. One more read of a file already
            // in the page cache is the difference between "this file is broken"
            // and "this file belongs under the other picker".
            Err(_) if theme_store::read_file::<ThemeFile>(path).is_ok() => {
                return Err(ImportError::WrongKind(
                    "settings.manage.import_not_a_scheme",
                ));
            }
            Err(error) => return Err(ImportError::Unreadable(error)),
        };
        self.validate(&file)?;
        Ok(file)
    }

    fn reload(&self, cx: &mut App) {
        theme_store::reload(cx);
    }

    fn name_of(&self, file: &CatalogFile) -> String {
        Self::file(file)
            .map(|file| file.name.clone())
            .unwrap_or_default()
    }

    /// Renamed into a new [`Arc`] rather than in place: the one the file
    /// travels in may be shared with the row that emitted it, and a scheme is
    /// twenty short strings, so a copy costs nothing worth counting.
    fn set_name(&self, file: &mut CatalogFile, name: String) {
        if let Some(scheme) = Self::file(file) {
            let mut renamed = scheme.clone();
            renamed.name = name;
            *file = Self::wrap(renamed);
        }
    }

    /// The terminal surface the edited scheme would draw.
    ///
    /// Everything at once, because a scheme is judged as a whole: sample text
    /// over the background, a caret in the cursor colour, a run of selected
    /// text, and both halves of the ANSI palette as chips.
    fn render_preview(
        &self,
        _id: &str,
        _name: SharedString,
        values: &[String],
        _dark: bool,
        _cx: &mut App,
    ) -> AnyElement {
        let palette = scheme_file(String::new(), values).to_theme();
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
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scheme_slots_round_trip_through_the_fields() {
        let file = SchemeFile::from_theme("Mine", &TerminalTheme::dracula());
        let values = scheme_values(&file);
        assert_eq!(values.len(), SCHEME_SLOTS.len());
        assert_eq!(scheme_file("Mine".to_string(), &values), file);
    }

    #[test]
    fn a_scheme_without_its_optional_keys_still_fills_every_field() {
        // A palette in circulation may leave `cursorColor` and
        // `selectionBackground` out; the editor has a field for each, and it
        // shows what the loader would have derived rather than nothing at all.
        let mut file = SchemeFile::from_theme("Sparse", &TerminalTheme::gruvbox_dark());
        file.cursor_color = None;
        file.selection_background = None;

        let values = scheme_values(&file);
        assert!(!values[2].is_empty(), "the cursor colour was derived");
        assert!(!values[3].is_empty(), "the selection colour was derived");
        // And what is saved back spells both of them out.
        let saved = scheme_file("Sparse".to_string(), &values);
        assert_eq!(saved.cursor_color.as_deref(), Some(values[2].as_str()));
        assert_eq!(
            saved.selection_background.as_deref(),
            Some(values[3].as_str())
        );
    }

    #[test]
    fn the_ansi_heading_stands_in_front_of_the_first_ansi_slot() {
        // The four terminal roles come first and the sixteen ANSI colours
        // after them, so the heading belongs on slot four — and the field it
        // names has to be the one the editor would draw there.
        let headings = SchemeCatalog.group_headings();
        assert_eq!(headings.len(), 1);
        let (index, key) = headings[0];
        assert_eq!(key, "settings.editor.term.ansi");
        assert_eq!(SCHEME_SLOTS[index].key, "black");
    }

    #[test]
    fn a_scheme_has_no_dark_flag_for_the_editor_to_offer() {
        // A terminal palette *is* its background. The editor draws no checkbox,
        // and what `values_of` reported comes back to `file_from` untouched.
        assert!(!SchemeCatalog.has_dark_flag());
    }

    #[test]
    fn every_slot_names_a_key_under_the_editors_own_namespace() {
        // The labels are looked up by key as the editor draws, so a key that is
        // not there shows on screen as itself. The locale check in `i18n` is
        // what asserts the keys exist; this is what asserts they are asked for.
        for slot in SCHEME_SLOTS {
            assert!(
                slot.label_key.starts_with("settings.editor.term."),
                "{} is not a terminal slot label",
                slot.key
            );
            assert!(!slot.alpha, "{} is not drawn translucent", slot.key);
            assert!(!slot.optional, "{} is never derived", slot.key);
        }
    }
}
