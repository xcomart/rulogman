//! The vector icon set, embedded in the binary.
//!
//! gpui's [`svg`](gpui::svg) element resolves its `path` through the
//! [`AssetSource`] the application was built with — [`Icons`] here — and paints
//! the result as a *monochrome* sprite: resvg rasterises the file, only the
//! alpha channel survives, and the element's `text_color` supplies the colour.
//! Two things follow, and both are why these files look the way they do:
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

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Pixels, Result, SharedString, Styled, Svg, svg};

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

/// The button at the end of the tab strip that lists every open tab.
///
/// A plain chevron rather than a stack of lines: the strip's other end already
/// carries the application menu's `☰`, and two list-shaped glyphs facing each
/// other across one toolbar would read as the same control twice. A chevron
/// says "this opens downwards", which is the one thing the button does.
pub const TAB_LIST: &str = "icons/tab-list.svg";

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

/// The custom title bar's minimise button.
///
/// The four window-control glyphs are drawn edge to edge of the 24×24 box
/// rather than inset like the rest of the set: they are painted at half the
/// size of a toolbar icon, and a glyph that kept the usual margin would come
/// out thinner and smaller than the caption buttons of the platform they stand
/// in for.
///
/// They carry a heavier stroke than the rest of the set for the same reason —
/// `2.2` against the usual `1.8`. The caption strip renders them at 12 px
/// (`GLYPH_SIZE` in [`crate::ui::window_controls`]), which is half the viewBox,
/// so the stroke that reaches the screen is half what the file asks for: `1.8`
/// arrived as 0.9 px, a hairline no row of pixels could hold at full coverage
/// once it had been antialiased, and `2.2` arrives as 1.1 px instead. All four
/// share the value, including both rectangles of [`WINDOW_RESTORE`], so that
/// the strip reads as one set.
pub const WINDOW_MINIMIZE: &str = "icons/window-minimize.svg";

/// The custom title bar's maximise button, while the window is not maximised.
pub const WINDOW_MAXIMIZE: &str = "icons/window-maximize.svg";

/// The custom title bar's maximise button, while the window *is* maximised.
///
/// Two offset squares, the shape every desktop uses for "put it back": the
/// button keeps its place and only the glyph says which way it will go.
pub const WINDOW_RESTORE: &str = "icons/window-restore.svg";

/// The custom title bar's close button.
pub const WINDOW_CLOSE: &str = "icons/window-close.svg";

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

/// Every icon, paired with the bytes [`Icons`] hands back for it.
const ICONS: [(&str, &[u8]); 19] = [
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
    (TAB_LIST, include_bytes!("../assets/icons/tab-list.svg")),
    (NEW_TAB, include_bytes!("../assets/icons/new-tab.svg")),
    (TUNNEL, include_bytes!("../assets/icons/tunnel.svg")),
    (
        WINDOW_MINIMIZE,
        include_bytes!("../assets/icons/window-minimize.svg"),
    ),
    (
        WINDOW_MAXIMIZE,
        include_bytes!("../assets/icons/window-maximize.svg"),
    ),
    (
        WINDOW_RESTORE,
        include_bytes!("../assets/icons/window-restore.svg"),
    ),
    (
        WINDOW_CLOSE,
        include_bytes!("../assets/icons/window-close.svg"),
    ),
];

/// The asset source backing every [`svg`](gpui::svg) element in the app.
///
/// Install it with [`Application::with_assets`](gpui::Application::with_assets);
/// without it gpui's default source answers every path with `None` and the
/// icons paint as nothing at all.
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

/// A square icon, sized and tinted.
///
/// The result is still an [`Svg`], so a caller can go on styling it — which is
/// what the hover states do.
pub fn icon(path: &'static str, size: Pixels, color: Hsla) -> Svg {
    svg().size(size).flex_none().path(path).text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads_and_matches_its_extension() {
        for (name, _) in ICONS {
            let bytes = Icons
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
            Icons
                .load("icons/nothing.svg")
                .expect("a missing asset is not a failure")
                .is_none()
        );
    }

    #[test]
    fn listing_returns_the_whole_set() {
        assert_eq!(Icons.list("icons/").unwrap().len(), ICONS.len());
    }
}
