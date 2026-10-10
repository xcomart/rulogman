//! Paths.

use super::*;

/// Whether a listing row's name is too long to be shown whole, and so wants a
/// tooltip carrying it in full.
///
/// An estimate, like [`fold_budget`], and chosen over measuring for a reason
/// specific to this list: **the listing is not virtualised**. Every entry of the
/// directory is built on every repaint, so a directory with ten thousand files
/// builds ten thousand rows a frame. Shaping each name through
/// [`Window::text_system`](gpui::Window::text_system) to learn its exact width
/// would put a text layout — the expensive half of drawing text — on that path,
/// multiplied by the size of the directory, to decide something no one sees
/// until they hover. The arithmetic below costs a few multiplications.
///
/// Being wrong is not symmetric here, so the estimate leans one way: a name cut
/// off with no way to read it is a real loss, while a tooltip on a name that
/// happened to fit is a moment of redundancy. Every constant therefore rounds
/// *up* — [`ROW_CHAR`], [`SIZE_CHAR`] — and every subtraction below is taken at
/// its most pessimistic, so the budget errs small and the tooltip errs present.
///
/// Widths count columns rather than characters: a Hangul or Han name occupies
/// two columns per character at the same font size, and counting `chars` would
/// let such a name run to twice the width before anyone thought it was long.
pub(super) fn name_is_clipped(width: f32, name: &str, badge: bool, size: Option<&str>) -> bool {
    // Everything the row spends before the name gets what is left: the panel's
    // own hairline border on both sides, the row's padding, the leading icon
    // and the gap after it, then the symlink badge and the size column when
    // they are there — each with the gap that precedes it.
    let mut spent = 2. + 2. * ROW_PADDING + ROW_ICON + ROW_GAP;
    if badge {
        spent += ROW_GAP + BADGE_ICON;
    }
    if let Some(size) = size {
        spent += ROW_GAP + columns(size) as f32 * SIZE_CHAR;
    }

    let usable = width - spent;
    if !usable.is_finite() || usable <= 0. {
        // No room to draw a name at all, so anything at all is clipped.
        return !name.is_empty();
    }
    let budget = (usable / ROW_CHAR).floor();
    let budget = if budget >= 0. { budget as usize } else { 0 };
    columns(name) > budget
}

/// How many columns `text` occupies, counting East Asian wide characters twice.
///
/// [`UnicodeWidthStr`] answers the question the estimate actually asks — how
/// much room this will take — for the one distinction that matters at this
/// resolution. It is not a substitute for measuring a proportional font; it is
/// what keeps a CJK name from being treated as half its real width.
pub(super) fn columns(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// How much path the header can hold at a panel `width` pixels wide, in
/// characters.
///
/// An estimate, and deliberately so: the header's font is proportional, so the
/// only exact answer would be to lay the row out and measure it, and a header
/// that reflowed after layout would need a second pass every repaint. The
/// estimate is allowed to be wrong because being wrong is cheap — the row wraps
/// rather than truncating, so a budget that came out too generous costs a line
/// of header and never the leaf directory the user is standing in.
///
/// Tied to the width rather than fixed because the panel is dragged between
/// [`MIN_PANEL_WIDTH`] and [`MAX_PANEL_WIDTH`]: one number for both ends would
/// fold a path that had room to spare at 560px, or overflow at 180px.
pub(super) fn fold_budget(width: f32) -> usize {
    let usable = width - 2. * HEADER_PADDING;
    if !usable.is_finite() || usable <= 0. {
        return MIN_PATH_CHARS;
    }
    let chars = (usable / CRUMB_CHAR).floor();
    // Saturating rather than wrapping: `as` on a float out of range would give
    // a budget the row could never spend.
    let chars = if chars >= 0. { chars as usize } else { 0 };
    chars.max(MIN_PATH_CHARS)
}

/// The `C:` at the head of `path`, when it has one.
///
/// A session running a shell on Windows browses paths whose root is a drive
/// rather than a bare slash, and the header has to know: splitting `C:/Users/ada`
/// on `/` alone would make `C:` an ordinary piece hanging off `/`, and pressing
/// it would navigate to `/C:`, which names nothing on any filesystem.
///
/// Decided from the shape of the path rather than from `cfg!(windows)`, because
/// the panel holds paths of both kinds at once — the tab beside a local one may
/// be an SSH session, and *those* paths are POSIX and absolute whatever the
/// server runs on. That is also why the two can never be confused: a POSIX
/// absolute path begins with `/`, which is not a letter, so nothing an SFTP
/// source produces can be read as a drive.
pub(super) fn drive_prefix(path: &str) -> Option<&str> {
    let mut chars = path.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    // `C:` or `C:/…` and nothing else: `C:logs` is relative to that drive's own
    // current directory, which is not a shape the panel is ever handed.
    match chars.next() {
        None | Some('/') => Some(&path[..2]),
        Some(_) => None,
    }
}

/// Whether `path` is a filesystem root, and so has no parent to walk up to.
///
/// Two spellings of one idea, for the same reason [`drive_prefix`] exists: `/`
/// on a POSIX source, `C:/` on a drive of this machine.
pub(super) fn is_root(path: &str) -> bool {
    match drive_prefix(path) {
        Some(drive) => &path[drive.len()..] == ROOT_CRUMB,
        None => path == ROOT_CRUMB,
    }
}

/// Breaks `path` into the pieces the header draws.
///
/// `/srv/app/logs` becomes `/`, `srv`, `app`, `logs`, each carrying the
/// directory whose subdirectories could take its place. What does not fit in
/// `budget` is folded away by [`fold`].
///
/// Paths are absolute and separated by `/` — remote ones because SFTP is POSIX
/// on the wire whatever the server runs on, local ones because the local source
/// spells them that way on the way out — so this splits on `/` and nothing else;
/// anything relative, which the panel never produces, is read as if it hung off
/// the root.
///
/// The root the pieces hang off is not always `/`, which is the one thing this
/// does not take on faith: see [`drive_prefix`].
pub(super) fn crumbs(path: &str, budget: usize) -> Vec<Crumb> {
    let (root, rest) = match drive_prefix(path) {
        Some(drive) => (format!("{drive}/"), &path[drive.len()..]),
        None => (ROOT_CRUMB.to_owned(), path),
    };

    let mut crumbs = vec![Crumb {
        label: SharedString::from(root.clone()),
        menu: CrumbMenu::Root(root.clone()),
    }];

    let mut directory = root;
    for name in rest.split('/').filter(|name| !name.is_empty()) {
        crumbs.push(Crumb {
            label: SharedString::from(name.to_owned()),
            menu: CrumbMenu::Siblings(directory.clone()),
        });
        directory = join(&directory, name);
    }

    fold(crumbs, budget)
}

/// The rows the root breadcrumb offers for `roots`, or `None` for no menu.
///
/// `None` means "there is nothing to choose between" — one root, or none at
/// all — and is what keeps every POSIX source behaving exactly as it did before
/// roots existed: the caller falls back to listing the root's subdirectories.
/// The threshold is two rather than one *other* root because the row for the
/// tree the panel is already in is a real destination, not a no-op: `C:/` from
/// `C:/Users/ada` moves.
///
/// Each root labels itself. A root is already a short, complete path — `/`,
/// `C:/` — so there is no shorter name to give it, and the label a user knows a
/// drive by is precisely its letter.
pub(super) fn root_targets(roots: Vec<String>) -> Option<Vec<CrumbTarget>> {
    if roots.len() < 2 {
        return None;
    }
    Some(
        roots
            .into_iter()
            .map(|root| CrumbTarget {
                label: SharedString::from(root.clone()),
                path: root,
            })
            .collect(),
    )
}

/// Replaces the pieces `budget` characters cannot hold with a single ellipsis.
///
/// The tail is what a header is for: `/srv/app/releases/2026-07-30/logs` says
/// where you are and `/srv/app/releases/2026-07…` does not, so the pieces are
/// kept from the *back* — the leaf always, then as many of its ancestors as
/// fit. The root survives whatever the budget, at the one or three characters it
/// spells itself with; it is the one destination reachable from nowhere else in
/// the row.
///
/// The folded pieces are not lost: they become the rows of the ellipsis's own
/// dropdown, which is the only reason a piece may be dropped at all.
pub(super) fn fold(crumbs: Vec<Crumb>, budget: usize) -> Vec<Crumb> {
    if crumb_width(&crumbs) <= budget {
        return crumbs;
    }

    // The root — whose own width is `/` or `C:/`, so it is read off the row
    // rather than assumed — and the ellipsis, then every kept piece at its own
    // text plus the separator drawn in front of it: the same arithmetic
    // `crumb_width` does, over the row this is about to build. Neither of the
    // first two is preceded by a separator, because a root label ends in one.
    let root_width = crumbs.first().map_or(0, |root| root.label.chars().count());
    let mut spent = root_width + FOLD_CRUMB.chars().count();
    let mut kept = 0;
    for crumb in crumbs.iter().skip(1).rev() {
        let cost = crumb.label.chars().count() + CRUMB_SEPARATOR.chars().count();
        // The leaf is kept whatever it costs: a row that folded away the
        // directory you are standing in would say nothing at all.
        if kept > 0 && spent + cost > budget {
            break;
        }
        spent += cost;
        kept += 1;
    }

    let mut pieces = crumbs.into_iter();
    let Some(root) = pieces.next() else {
        return Vec::new();
    };
    let mut rest: Vec<Crumb> = pieces.collect();
    let tail = rest.split_off(rest.len().saturating_sub(kept));

    let folded: Vec<CrumbTarget> = rest
        .into_iter()
        .filter_map(|crumb| {
            let path = crumb.path()?;
            Some(CrumbTarget {
                label: crumb.label,
                path,
            })
        })
        .collect();
    // A single piece too long for the budget folds nothing away, and an
    // ellipsis with an empty menu behind it would be a dead end.
    if folded.is_empty() {
        return std::iter::once(root).chain(tail).collect();
    }

    let ellipsis = Crumb {
        label: SharedString::new_static(FOLD_CRUMB),
        menu: CrumbMenu::Folded(folded),
    };
    std::iter::once(root)
        .chain(std::iter::once(ellipsis))
        .chain(tail)
        .collect()
}

/// How many characters the header spends on a run of pieces.
///
/// Every piece's own text, plus the separator in front of it — which the piece
/// before it may already have supplied, as the root's `/` does.
pub(super) fn crumb_width(crumbs: &[Crumb]) -> usize {
    crumbs
        .iter()
        .enumerate()
        .map(|(index, crumb)| {
            let separated = index
                .checked_sub(1)
                .is_some_and(|previous| needs_separator(&crumbs[previous].label));
            crumb.label.chars().count()
                + if separated {
                    CRUMB_SEPARATOR.chars().count()
                } else {
                    0
                }
        })
        .sum()
}

/// Whether a piece drawn after `label` needs a separator of its own.
///
/// Only a root does not ask for one: its label already ends in a slash — `/`
/// itself, or `C:/` on a drive — and a second one after it would read as `//`.
pub(super) fn needs_separator(label: &str) -> bool {
    !label.ends_with('/')
}

/// Renders a byte count the way a file manager does.
///
/// The unit symbols are not translated: like the terminal grid size in the
/// status bar they are symbols rather than words, and every locale writes them
/// the same way.
pub(super) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    match UNITS.get(unit) {
        Some(_) if unit == 0 => format!("{bytes} B"),
        Some(symbol) => format!("{value:.1} {symbol}"),
        None => format!("{bytes} B"),
    }
}

/// The file name of `path`, for a status message.
pub(super) fn file_name(path: &Path) -> SharedString {
    path.file_name()
        .map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

/// Where the save dialog opens by default.
///
/// The platform picker remembers the last directory the user chose, so this
/// only has to be a sensible *first* answer; the home directory is one on every
/// platform, and an empty path leaves the choice to the picker.
pub(super) fn suggested_directory() -> PathBuf {
    directories::UserDirs::new().map_or_else(PathBuf::new, |dirs| dirs.home_dir().to_owned())
}
