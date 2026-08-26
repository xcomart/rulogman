//! rulogman's own vector icons, embedded in the binary.
//!
//! gpui's [`svg`](gpui::svg) element resolves its `path` through the
//! [`AssetSource`](gpui::AssetSource) the application was built with — [`ICONS`]
//! here — and paints the result as a *monochrome* sprite: resvg rasterises the
//! file, only the alpha channel survives, and the element's `text_color`
//! supplies the colour. Two things follow, and both are why these files look
//! the way they do:
//!
//! * the colours written in an icon never reach the screen, only its coverage
//!   does, so a `fill-opacity` below `1` reads as a lighter shade of the tint —
//!   which is how the folder and panel icons get their fill from one path;
//! * the tint is whatever the *element* asks for, and unlike text it is not
//!   inherited from a parent, so a hover that recolours a button has to reach
//!   the icon through [`group_hover`](gpui::InteractiveElement::group_hover).
//!
//! The bytes come from [`include_bytes!`], not from files read at run time: a
//! release then carries its icons wherever it is unpacked, and packaging has
//! nothing extra to ship. Cargo tracks the embedded files itself, so an edited
//! icon rebuilds the crate without help from `build.rs`.
//!
//! Only the marks that are *rulogman's* are here. The four caption glyphs a
//! self-drawn title bar needs are the same four files in every application that
//! draws one, so they come from
//! [`rugpui_shell::WINDOW_CONTROL_ICONS`](rugpui_shell::WINDOW_CONTROL_ICONS);
//! the two disclosure carets the widget kit draws by default — on a
//! collapsible's header, a tree row's twisty, a select's trigger — come from
//! [`rugpui::ICONS`](rugpui::ICONS), which the kit does not install itself
//! because the application owns the asset source. [`ICONS`] concatenates the
//! three tables. [`icon`] is the shell's too, and is re-exported here so that a
//! call site names one module rather than two.

use rugpui_shell::IconSet;
pub use rugpui_shell::icon;

/// A directory row in the file panel, and its parent row.
pub const FOLDER: &str = "icons/folder.svg";

/// A file row in the file panel.
pub const FILE: &str = "icons/file.svg";

/// Drawn after the name of a symbolic link, where `ls -l` writes an arrow.
pub const SYMLINK: &str = "icons/symlink.svg";

/// The file panel button that lists the current directory again.
pub const REFRESH: &str = "icons/refresh.svg";

/// The file panel button that uploads local files into the current directory.
pub const UPLOAD: &str = "icons/upload.svg";

/// The file panel button that uploads a whole local folder.
///
/// A second button rather than a second mode of the first: the platform file
/// pickers cannot offer files and folders at once everywhere, so the choice has
/// to be made before the dialog opens.
pub const UPLOAD_FOLDER: &str = "icons/upload-folder.svg";

/// The file panel button that saves the selected remote file locally.
pub const DOWNLOAD: &str = "icons/download.svg";

/// The file panel button that creates a directory in the listed one.
///
/// The same folder outline [`UPLOAD_FOLDER`] draws, carrying a plus where that
/// one carries an arrow: the two sit side by side in the toolbar, and reading
/// them as a pair is what says one adds a folder while the other sends one.
pub const NEW_FOLDER: &str = "icons/new-folder.svg";

/// The file panel button that renames the one selected entry.
pub const RENAME: &str = "icons/rename.svg";

/// The file panel button that deletes the selection.
pub const DELETE: &str = "icons/delete.svg";

/// The toolbar button that shows and hides the remote file panel.
pub const PANEL: &str = "icons/panel.svg";

/// The mark on anything that discloses downwards.
///
/// One chevron for three controls that all say the same thing: the button at
/// the end of the tab strip that lists every open tab, the trigger of every
/// dropdown, and the header of an expanded fold-away section. A window in which
/// a select, a section and a tab list each drew a mark of its own would be
/// telling the reader three times that there is more underneath, in three
/// different hands.
pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";

/// The same mark turned a quarter, on a fold-away section that is closed.
///
/// Only ever seen beside [`CHEVRON_DOWN`], never on its own: a disclosure
/// arrow is read as a *rotation* — the thing it points at is where the section
/// will open — so the two have to be one glyph in two positions rather than two
/// glyphs.
pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";

/// The button at the end of the tab strip that lists every open tab.
///
/// A plain chevron rather than a stack of lines: the strip's other end already
/// carries the application menu's `☰`, and two list-shaped glyphs facing each
/// other across one toolbar would read as the same control twice. A chevron
/// says "this opens downwards", which is the one thing the button does — and it
/// is [`CHEVRON_DOWN`] itself, under the name the strip knows it by, so that
/// the button and the dropdowns below it cannot drift apart.
pub const TAB_LIST: &str = CHEVRON_DOWN;

/// The button at the end of the tab strip that opens a new session.
///
/// Drawn with the stroke of [`TAB_LIST`] rather than the toolbar icons': the
/// two sit shoulder to shoulder in the strip, and it is that pairing the glyph
/// has to match.
pub const NEW_TAB: &str = "icons/new-tab.svg";

/// Drawn on the tab of a session that owns its profile's port forwardings.
///
/// An arrow inside a rounded conduit, and neither of the two obvious
/// alternatives: a chain link is the web's mark for a hyperlink and would read
/// as "this tab has a link in it", and a plug reads as hardware rather than as
/// traffic. What has to come across at the 13 px the strip draws it at is that
/// something is *flowing through* something, which is exactly what a forwarding
/// is, so the glyph is only those two shapes and the arrow is kept well clear
/// of the conduit's stroke — at that size a gap of a pixel is the difference
/// between two shapes and one blot.
pub const TUNNEL: &str = "icons/tunnel.svg";

/// The application icon, drawn at the left end of the custom title bar.
///
/// The shipped icon itself, in its own colours — a raster, unlike everything
/// else in this set, because those colours are the point: the title bar used
/// to draw a monochrome sprite of the mark instead, from the days when the
/// icon's tile was a flat dark plate that a dark theme's chrome swallowed
/// whole. The current icon wears an embossed ring that keeps the tile's edge
/// legible on dark and light chrome alike, so the bar can show the same face
/// the taskbar does. Rendered at 64 px — four times the 16 px it is drawn at,
/// so it stays sharp on any display scale — by
/// `assets/render.py assets/icon.svg --sizes 64 --outdir crates/rulogman-app/assets/icons`;
/// regenerate it whenever the master SVG changes.
pub const APP_ICON: &str = "icons/icon-64.png";

/// rulogman's own icons, paired with the bytes [`ICONS`] hands back for them.
const APP_ICONS: &[(&str, &[u8])] = &[
    (APP_ICON, include_bytes!("../assets/icons/icon-64.png")),
    (FOLDER, include_bytes!("../assets/icons/folder.svg")),
    (FILE, include_bytes!("../assets/icons/file.svg")),
    (SYMLINK, include_bytes!("../assets/icons/symlink.svg")),
    (REFRESH, include_bytes!("../assets/icons/refresh.svg")),
    (UPLOAD, include_bytes!("../assets/icons/upload.svg")),
    (
        UPLOAD_FOLDER,
        include_bytes!("../assets/icons/upload-folder.svg"),
    ),
    (DOWNLOAD, include_bytes!("../assets/icons/download.svg")),
    (NEW_FOLDER, include_bytes!("../assets/icons/new-folder.svg")),
    (RENAME, include_bytes!("../assets/icons/rename.svg")),
    (DELETE, include_bytes!("../assets/icons/delete.svg")),
    (PANEL, include_bytes!("../assets/icons/panel.svg")),
    (
        CHEVRON_DOWN,
        include_bytes!("../assets/icons/chevron-down.svg"),
    ),
    (
        CHEVRON_RIGHT,
        include_bytes!("../assets/icons/chevron-right.svg"),
    ),
    (NEW_TAB, include_bytes!("../assets/icons/new-tab.svg")),
    (TUNNEL, include_bytes!("../assets/icons/tunnel.svg")),
];

/// The asset source backing every [`svg`](gpui::svg) element in the app.
///
/// Install it with [`Application::with_assets`](gpui::Application::with_assets);
/// without it gpui's default source answers every path with `None` and the
/// icons paint as nothing at all.
///
/// Three tables and not one: the kit's disclosure carets stay a `const` slice
/// in `rugpui`, the caption glyphs stay one in `rugpui-shell`, and rulogman's
/// own stay one here. Dropping the kit's table would not fail a build — it
/// would leave every caret and dropdown chevron painting nothing, and
/// [`rugpui::init`] warning about it once at start-up.
pub const ICONS: IconSet =
    IconSet::new(&[rugpui::ICONS, rugpui_shell::WINDOW_CONTROL_ICONS, APP_ICONS]);

#[cfg(test)]
mod tests {
    use gpui::AssetSource;

    use super::*;

    #[test]
    fn every_icon_loads_and_matches_its_extension() {
        for (name, _) in ICONS.all() {
            let bytes = ICONS
                .load(name)
                .expect("loading an embedded icon cannot fail")
                .unwrap_or_else(|| panic!("{name} is missing from the asset source"));
            // The one raster in the set: the title bar's application icon,
            // which is shipped in its own colours rather than tinted.
            if name.ends_with(".png") {
                assert!(bytes.starts_with(b"\x89PNG"), "{name} is not a PNG");
                continue;
            }
            let text = std::str::from_utf8(&bytes).expect("an icon must be UTF-8");
            assert!(text.contains("<svg"), "{name} is not an SVG");
            assert!(
                text.contains("viewBox=\"0 0 24 24\""),
                "{name} is not 24x24"
            );
        }
    }

    #[test]
    fn an_unknown_path_is_not_an_error() {
        assert!(
            ICONS
                .load("icons/nothing.svg")
                .expect("a missing asset is not a failure")
                .is_none()
        );
    }

    #[test]
    fn listing_returns_the_whole_set() {
        assert_eq!(ICONS.list("icons/").unwrap().len(), ICONS.len());
        // rulogman's own sixteen, the shell's four caption glyphs and the
        // kit's two disclosure carets.
        assert_eq!(ICONS.len(), APP_ICONS.len() + 4 + 2);
    }

    /// The widget kit reaches for these two by path and never installs them,
    /// so a set that has not chained `rugpui::ICONS` draws no carets at all.
    #[test]
    fn the_kits_disclosure_carets_are_in_the_set() {
        for path in [rugpui::CARET_DOWN, rugpui::CARET_RIGHT] {
            assert!(
                ICONS.load(path).expect("loading cannot fail").is_some(),
                "{path} is not in the set"
            );
        }
    }

    #[test]
    fn the_caption_strip_is_handed_paths_this_set_answers_to() {
        let icons = rugpui_shell::window_control_icons();
        for path in [icons.minimize, icons.maximize, icons.restore, icons.close] {
            assert!(
                ICONS.load(&path).expect("loading cannot fail").is_some(),
                "{path} is not in the set"
            );
        }
    }
}
