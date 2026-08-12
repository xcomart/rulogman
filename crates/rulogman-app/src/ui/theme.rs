//! Color palette used by every widget in [`crate::ui`].
//!
//! The theme is stored as a gpui [`Global`], so any widget that has access to an
//! [`App`] reference can read it without threading it through its constructor.
//!
//! # Themes and their ids
//!
//! Six themes ship with rulogman, one per built-in terminal color scheme and
//! carrying the same id, so that picking "Dracula" for the chrome and for the
//! terminal is one word in both places. A theme can also come from a file:
//! [`ThemeFile`] is the on-disk form, [`crate::theme_store`] reads the files,
//! and [`ThemeRegistry`] is where the two kinds are listed and resolved
//! together.
//!
//! # Stored colors and derived ones
//!
//! Not every slot of a [`Theme`] is written down. A palette spells out the
//! colors an author chooses — that is [`Palette`], the one gate every theme is
//! built through — and the slots that have to *hold* for any palette whatever
//! are worked out from those. [`Theme::icon`] is the first of them: it is the
//! muted text bent until it clears [`MIN_ICON_CONTRAST`] on both backgrounds,
//! so that a theme nobody checked still draws icons that can be seen.

use gpui::{App, Global, Hsla, Rgba, hsla};
use serde::{Deserialize, Serialize};

/// A flat set of semantic colors.
///
/// Widgets never hardcode colors; they always resolve them through a `Theme` so
/// that swapping [`Theme::dark`] for [`Theme::light`] restyles the whole app.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Whether this is a dark palette.
    ///
    /// Nothing in the widget layer branches on it — every widget reads the
    /// colors below instead — but the platforms that draw their own window
    /// caption need to be told which side of light/dark the app is on, and the
    /// palette is the only thing that knows.
    pub dark: bool,
    /// Window / app background.
    pub background: Hsla,
    /// Background of raised chrome such as panels, toolbars and the tab bar.
    pub surface: Hsla,
    /// Surface color while the pointer hovers an interactive element.
    pub surface_hover: Hsla,
    /// Surface color while an interactive element is pressed or selected.
    pub surface_active: Hsla,
    /// Hairline separators and control outlines.
    pub border: Hsla,
    /// Primary foreground color.
    pub text: Hsla,
    /// Secondary foreground color for hints, placeholders and inactive labels.
    pub text_muted: Hsla,
    /// Resting foreground color of an *icon*, as opposed to muted text.
    ///
    /// Icons used to be painted in [`Theme::text_muted`], which is the right
    /// hierarchy for a hint or an inactive label but the wrong one for a mark:
    /// a glyph is a solid run of pixels, while an icon is a hairline — the
    /// caption buttons draw a 1.1 px stroke — and a stroke that thin never
    /// reaches full coverage once it has been antialiased, so the same color
    /// arrives on screen weaker than the text beside it. WCAG asks 3:1 of a
    /// graphical control for that reason, and several of the built-in dark
    /// palettes did not even reach that with their muted text.
    ///
    /// So this slot is *derived* rather than stored: [`Palette`] runs
    /// [`readable_icon`] over the theme's own muted text, keeping its hue and
    /// saturation and moving only its lightness away from the surfaces until it
    /// clears [`MIN_ICON_CONTRAST`] against both [`Theme::background`] and
    /// [`Theme::surface`]. Deriving it is what lets a theme a user wrote by
    /// hand — [`ThemeFile`] carries no `icon` key, and gains none — come out
    /// legible without the user having thought about contrast at all.
    pub icon: Hsla,
    /// Brand color used for the active tab, focus rings and primary buttons.
    pub accent: Hsla,
    /// Destructive actions and error states.
    pub danger: Hsla,
    /// Successful / connected states.
    pub success: Hsla,
    /// Translucent backdrop painted behind modal dialogs (includes alpha).
    pub overlay: Hsla,
}

impl Theme {
    /// The default dark theme, in the spirit of One Dark.
    pub fn dark() -> Self {
        Palette {
            dark: true,
            background: hsla(220. / 360., 0.13, 0.18, 1.0),
            surface: hsla(220. / 360., 0.13, 0.14, 1.0),
            surface_hover: hsla(220. / 360., 0.13, 0.23, 1.0),
            surface_active: hsla(220. / 360., 0.13, 0.28, 1.0),
            border: hsla(220. / 360., 0.13, 0.31, 1.0),
            text: hsla(219. / 360., 0.14, 0.78, 1.0),
            text_muted: hsla(220. / 360., 0.09, 0.55, 1.0),
            accent: hsla(207. / 360., 0.82, 0.66, 1.0),
            danger: hsla(355. / 360., 0.65, 0.65, 1.0),
            success: hsla(95. / 360., 0.38, 0.62, 1.0),
            overlay: hsla(220. / 360., 0.13, 0.06, 0.62),
        }
        .into()
    }

    /// A light counterpart to [`Theme::dark`].
    pub fn light() -> Self {
        Palette {
            dark: false,
            background: hsla(0., 0.0, 1.0, 1.0),
            surface: hsla(220. / 360., 0.16, 0.96, 1.0),
            surface_hover: hsla(220. / 360., 0.16, 0.91, 1.0),
            surface_active: hsla(220. / 360., 0.16, 0.86, 1.0),
            border: hsla(220. / 360., 0.13, 0.80, 1.0),
            text: hsla(220. / 360., 0.16, 0.20, 1.0),
            text_muted: hsla(220. / 360., 0.10, 0.45, 1.0),
            accent: hsla(212. / 360., 0.76, 0.46, 1.0),
            danger: hsla(355. / 360., 0.66, 0.46, 1.0),
            success: hsla(120. / 360., 0.45, 0.33, 1.0),
            overlay: hsla(220. / 360., 0.13, 0.35, 0.40),
        }
        .into()
    }

    /// Chrome for Solarized Dark.
    ///
    /// The surfaces walk down Ethan Schoonover's base ramp from `base03`
    /// (`#002b36`, the terminal background) and the text back up it to `base0`
    /// and `base01`; the three status colors are the palette's own blue, red
    /// and green.
    pub fn solarized_dark() -> Self {
        Palette {
            dark: true,
            background: hsla(192. / 360., 1.00, 0.11, 1.0),
            surface: hsla(192. / 360., 1.00, 0.085, 1.0),
            surface_hover: hsla(192. / 360., 0.81, 0.16, 1.0),
            surface_active: hsla(192. / 360., 0.62, 0.21, 1.0),
            border: hsla(194. / 360., 0.25, 0.28, 1.0),
            text: hsla(186. / 360., 0.08, 0.55, 1.0),
            text_muted: hsla(194. / 360., 0.14, 0.40, 1.0),
            accent: hsla(205. / 360., 0.69, 0.49, 1.0),
            danger: hsla(1. / 360., 0.71, 0.52, 1.0),
            success: hsla(68. / 360., 1.00, 0.30, 1.0),
            overlay: hsla(192. / 360., 1.00, 0.04, 0.62),
        }
        .into()
    }

    /// Chrome for Solarized Light.
    ///
    /// The same palette as [`Theme::solarized_dark`] read from the other end:
    /// the surfaces run down from `base3` (`#fdf6e3`) towards `base2`, and the
    /// text is `base01` over `base0`, which is the contrast pairing Solarized
    /// itself prescribes for a light background.
    pub fn solarized_light() -> Self {
        Palette {
            dark: false,
            background: hsla(44. / 360., 0.87, 0.94, 1.0),
            surface: hsla(46. / 360., 0.42, 0.88, 1.0),
            surface_hover: hsla(46. / 360., 0.35, 0.84, 1.0),
            surface_active: hsla(46. / 360., 0.28, 0.79, 1.0),
            border: hsla(46. / 360., 0.20, 0.72, 1.0),
            text: hsla(194. / 360., 0.14, 0.34, 1.0),
            text_muted: hsla(194. / 360., 0.11, 0.48, 1.0),
            accent: hsla(205. / 360., 0.69, 0.42, 1.0),
            danger: hsla(1. / 360., 0.71, 0.45, 1.0),
            success: hsla(68. / 360., 1.00, 0.26, 1.0),
            overlay: hsla(44. / 360., 0.30, 0.35, 0.40),
        }
        .into()
    }

    /// Chrome for Gruvbox Dark.
    ///
    /// The surfaces are morhetz's `dark0` … `dark3` ramp, warm and barely
    /// saturated; the text is `light1` over `gray`, and the accents are the
    /// bright blue, red and green of the ANSI palette.
    pub fn gruvbox_dark() -> Self {
        Palette {
            dark: true,
            background: hsla(20. / 360., 0.03, 0.157, 1.0),
            surface: hsla(20. / 360., 0.03, 0.12, 1.0),
            surface_hover: hsla(20. / 360., 0.05, 0.224, 1.0),
            surface_active: hsla(22. / 360., 0.07, 0.29, 1.0),
            border: hsla(27. / 360., 0.10, 0.365, 1.0),
            text: hsla(43. / 360., 0.59, 0.81, 1.0),
            text_muted: hsla(30. / 360., 0.12, 0.514, 1.0),
            accent: hsla(157. / 360., 0.16, 0.58, 1.0),
            danger: hsla(6. / 360., 0.96, 0.59, 1.0),
            success: hsla(61. / 360., 0.66, 0.44, 1.0),
            overlay: hsla(20. / 360., 0.05, 0.06, 0.62),
        }
        .into()
    }

    /// Chrome for Dracula.
    ///
    /// Background and hover surface are the scheme's own `Background` and
    /// `Current Line`, the muted text is `Comment`, and the accent is the
    /// `Purple` that Dracula puts in the ANSI blue slot.
    pub fn dracula() -> Self {
        Palette {
            dark: true,
            background: hsla(231. / 360., 0.15, 0.184, 1.0),
            surface: hsla(231. / 360., 0.15, 0.14, 1.0),
            surface_hover: hsla(232. / 360., 0.14, 0.31, 1.0),
            surface_active: hsla(232. / 360., 0.15, 0.37, 1.0),
            border: hsla(226. / 360., 0.20, 0.42, 1.0),
            text: hsla(60. / 360., 0.30, 0.96, 1.0),
            text_muted: hsla(225. / 360., 0.27, 0.51, 1.0),
            accent: hsla(265. / 360., 0.89, 0.78, 1.0),
            danger: hsla(0., 1.00, 0.667, 1.0),
            success: hsla(135. / 360., 0.94, 0.65, 1.0),
            overlay: hsla(231. / 360., 0.15, 0.07, 0.62),
        }
        .into()
    }
}

/// The colors a theme *spells out*, before the derived ones are worked out.
///
/// Every `Theme` in the application is built from one of these — the six
/// built-in palettes above and [`ThemeFile::to_theme`] all end in
/// `Palette { … }.into()` — which is the point of the type: a slot that has to
/// hold for *any* palette, [`Theme::icon`] so far, can then be derived in the
/// single [`From`] impl below instead of being spelled out once per theme and
/// forgotten by the seventh. It is deliberately not public: a palette written
/// outside this module could not be held to that promise.
struct Palette {
    /// See [`Theme::dark`](Theme#structfield.dark).
    dark: bool,
    /// See [`Theme::background`](Theme#structfield.background).
    background: Hsla,
    /// See [`Theme::surface`](Theme#structfield.surface).
    surface: Hsla,
    /// See [`Theme::surface_hover`](Theme#structfield.surface_hover).
    surface_hover: Hsla,
    /// See [`Theme::surface_active`](Theme#structfield.surface_active).
    surface_active: Hsla,
    /// See [`Theme::border`](Theme#structfield.border).
    border: Hsla,
    /// See [`Theme::text`](Theme#structfield.text).
    text: Hsla,
    /// See [`Theme::text_muted`](Theme#structfield.text_muted).
    text_muted: Hsla,
    /// See [`Theme::accent`](Theme#structfield.accent).
    accent: Hsla,
    /// See [`Theme::danger`](Theme#structfield.danger).
    danger: Hsla,
    /// See [`Theme::success`](Theme#structfield.success).
    success: Hsla,
    /// See [`Theme::overlay`](Theme#structfield.overlay).
    overlay: Hsla,
}

impl From<Palette> for Theme {
    fn from(palette: Palette) -> Self {
        Self {
            dark: palette.dark,
            background: palette.background,
            surface: palette.surface,
            surface_hover: palette.surface_hover,
            surface_active: palette.surface_active,
            border: palette.border,
            text: palette.text,
            text_muted: palette.text_muted,
            // The one slot no palette writes; see [`Theme::icon`].
            icon: readable_icon(palette.text_muted, palette.background, palette.surface),
            accent: palette.accent,
            danger: palette.danger,
            success: palette.success,
            overlay: palette.overlay,
        }
    }
}

/// Contrast an icon has to reach against the surfaces it is painted on.
///
/// WCAG 2.1 asks 3:1 of a graphical control and 4.5:1 of body text; icons are
/// held to the text figure here because they are drawn *thinner* than text —
/// a 12 px caption glyph is a stroke a little over a pixel wide, and an
/// antialiased stroke that narrow never reaches the full coverage the ratio
/// assumes. Aiming at 4.5 buys back roughly what the antialiasing gives away.
const MIN_ICON_CONTRAST: f32 = 4.5;

/// How many times [`readable_icon`] halves the interval it searches.
///
/// Lightness is an `f32` in `[0, 1]`, so twenty-four halvings put the answer
/// far below the 1/255 that survives being written to a framebuffer: the
/// search stops well before precision does.
const ICON_SEARCH_STEPS: u32 = 24;

/// The relative luminance of `color`, as WCAG 2.1 defines it.
///
/// Alpha plays no part: every foreground slot of a theme is opaque, and a
/// translucent one would have to be composited against a specific background
/// before its luminance meant anything at all.
fn relative_luminance(color: Hsla) -> f32 {
    let rgba = Rgba::from(color);
    // sRGB's transfer function, undone: the stored channel is gamma-encoded,
    // and luminance is a sum of *linear* light.
    let linear = |channel: f32| {
        let channel = channel.clamp(0.0, 1.0);
        if channel <= 0.03928 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgba.r) + 0.7152 * linear(rgba.g) + 0.0722 * linear(rgba.b)
}

/// The WCAG contrast ratio between two colors, from `1.0` to `21.0`.
///
/// Symmetric in its arguments, so a caller need not know which of the two is
/// the foreground.
pub fn contrast_ratio(left: Hsla, right: Hsla) -> f32 {
    let (left, right) = (relative_luminance(left), relative_luminance(right));
    let (lighter, darker) = if left >= right {
        (left, right)
    } else {
        (right, left)
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// The icon tint a theme whose muted text is `muted` should use.
///
/// Icons sit on both of a theme's two backgrounds — the app background and the
/// raised chrome of toolbars, panels and the tab strip — so the color is judged
/// against the *worse* of the two and has to clear [`MIN_ICON_CONTRAST`] there.
/// A palette whose muted text already does is left exactly as it is, which is
/// what keeps a well-judged theme looking like itself; only the ones that fall
/// short are moved, and then only in lightness, so that the hue and saturation
/// the theme's author chose survive.
///
/// The direction is whichever end of the lightness axis is further from the two
/// backgrounds — away from them, in other words, which for a dark theme means
/// brighter and for a light one darker — and the amount is the smallest that
/// reaches the target, found by bisection. Relative luminance rises
/// monotonically with HSL lightness at a fixed hue and saturation, so beyond
/// the backgrounds the contrast does too and the bisection is well-founded.
///
/// One background always leaves an end of the axis that clears 4.5:1, so the
/// only palettes the search cannot satisfy are the ones whose two backgrounds
/// sit far apart on the ramp — black chrome on a white page, which no theme
/// here or on disk is — and those are given the better end anyway rather than
/// left where they were.
fn readable_icon(muted: Hsla, background: Hsla, surface: Hsla) -> Hsla {
    let at = |lightness: f32| Hsla {
        l: lightness,
        ..muted
    };
    let worst = |color: Hsla| contrast_ratio(color, background).min(contrast_ratio(color, surface));

    if worst(muted) >= MIN_ICON_CONTRAST {
        return muted;
    }

    let end = if worst(at(1.0)) >= worst(at(0.0)) {
        1.0
    } else {
        0.0
    };
    if worst(at(end)) < MIN_ICON_CONTRAST {
        return at(end);
    }

    // The invariant carried through the halvings: `short` fails the target and
    // `enough` meets it, so the answer is always the endpoint that meets it.
    let (mut short, mut enough) = (muted.l, end);
    for _ in 0..ICON_SEARCH_STEPS {
        let middle = (short + enough) / 2.0;
        if worst(at(middle)) >= MIN_ICON_CONTRAST {
            enough = middle;
        } else {
            short = middle;
        }
    }
    at(enough)
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}

/// Id of the default theme; the chrome counterpart of the `one-dark` scheme.
const ID_ONE_DARK: &str = "one-dark";
/// Id of the light counterpart of [`ID_ONE_DARK`].
const ID_ONE_LIGHT: &str = "one-light";
/// Id of the dark Solarized theme.
const ID_SOLARIZED_DARK: &str = "solarized-dark";
/// Id of the light Solarized theme.
const ID_SOLARIZED_LIGHT: &str = "solarized-light";
/// Id of the dark Gruvbox theme.
const ID_GRUVBOX_DARK: &str = "gruvbox-dark";
/// Id of the Dracula theme.
const ID_DRACULA: &str = "dracula";

/// What [`ID_ONE_DARK`] was called before the themes had ids.
const LEGACY_ID_DARK: &str = "dark";
/// What [`ID_ONE_LIGHT`] was called before the themes had ids.
const LEGACY_ID_LIGHT: &str = "light";

/// One entry of the built-in theme table.
struct BuiltinTheme {
    /// Stable id stored in settings.
    id: &'static str,
    /// Human-readable name, matching the terminal scheme of the same id.
    name: &'static str,
    /// Whether the palette is a dark one.
    dark: bool,
    /// Builds the palette. A function rather than a value because [`Hsla`] is
    /// not constructible in a `const`.
    build: fn() -> Theme,
}

/// Every built-in theme, in the order the terminal schemes are presented in.
const BUILTIN_THEMES: [BuiltinTheme; 6] = [
    BuiltinTheme {
        id: ID_ONE_DARK,
        name: "One Dark",
        dark: true,
        build: Theme::dark,
    },
    BuiltinTheme {
        id: ID_ONE_LIGHT,
        name: "One Light",
        dark: false,
        build: Theme::light,
    },
    BuiltinTheme {
        id: ID_SOLARIZED_DARK,
        name: "Solarized Dark",
        dark: true,
        build: Theme::solarized_dark,
    },
    BuiltinTheme {
        id: ID_SOLARIZED_LIGHT,
        name: "Solarized Light",
        dark: false,
        build: Theme::solarized_light,
    },
    BuiltinTheme {
        id: ID_GRUVBOX_DARK,
        name: "Gruvbox Dark",
        dark: true,
        build: Theme::gruvbox_dark,
    },
    BuiltinTheme {
        id: ID_DRACULA,
        name: "Dracula",
        dark: true,
        build: Theme::dracula,
    },
];

/// A theme loaded from a file rather than compiled in.
#[derive(Debug, Clone)]
pub struct CustomUiTheme {
    /// Stable id stored in settings, taken from the file name.
    pub id: String,
    /// Human-readable name, taken from the file's `name` key.
    pub name: String,
    /// The palette itself.
    pub theme: Theme,
}

/// One entry of the combined built-in + custom theme listing.
///
/// What a picker needs to draw a row, and nothing more; the colors are fetched
/// with [`ThemeRegistry::resolve`] only for the entries that end up on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEntry {
    /// Stable id stored in settings.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether the palette is a dark one.
    pub dark: bool,
    /// Whether the theme ships with rulogman rather than coming from a file.
    pub builtin: bool,
}

/// The themes read from the user's `themes` directory.
///
/// A gpui [`Global`] rather than a process-wide static, because — unlike the
/// terminal schemes, which `rulogman-term` has to resolve without an [`App`] in
/// hand — every reader of a UI theme already holds one.
#[derive(Debug, Default)]
pub struct ThemeRegistry {
    /// The custom themes, in the order the loader found them.
    custom: Vec<CustomUiTheme>,
}

impl Global for ThemeRegistry {}

impl ThemeRegistry {
    /// Installs an empty registry, if none has been installed yet.
    ///
    /// Called from [`crate::ui::init`], so that resolving an id before the
    /// theme files have been read answers the built-in themes rather than
    /// panicking on a missing global.
    pub fn init(cx: &mut App) {
        if cx.try_global::<Self>().is_none() {
            cx.set_global(Self::default());
        }
    }

    /// Replaces the themes loaded from the user's `themes` directory.
    ///
    /// The whole list is swapped at once, so re-scanning the directory cannot
    /// leave behind a theme its file no longer defines.
    pub fn set_custom(themes: Vec<CustomUiTheme>, cx: &mut App) {
        cx.set_global(Self { custom: themes });
    }

    /// The themes currently loaded from the user's `themes` directory.
    pub fn custom(cx: &App) -> Vec<CustomUiTheme> {
        cx.try_global::<Self>()
            .map(|registry| registry.custom.clone())
            .unwrap_or_default()
    }

    /// Whether `id` names a theme that ships with rulogman.
    pub fn is_builtin(id: &str) -> bool {
        BUILTIN_THEMES
            .iter()
            .any(|theme| theme.id.eq_ignore_ascii_case(id))
    }

    /// Every selectable theme: the built-in ones in presentation order, then
    /// the custom ones sorted by name.
    ///
    /// A custom theme whose id shadows a built-in one is left out, since
    /// [`ThemeRegistry::resolve`] would never hand it back anyway.
    pub fn all(cx: &App) -> Vec<ThemeEntry> {
        let mut entries: Vec<ThemeEntry> = BUILTIN_THEMES
            .iter()
            .map(|theme| ThemeEntry {
                id: theme.id.to_string(),
                name: theme.name.to_string(),
                dark: theme.dark,
                builtin: true,
            })
            .collect();

        let mut custom: Vec<ThemeEntry> = Self::custom(cx)
            .into_iter()
            .filter(|theme| !Self::is_builtin(&theme.id))
            .map(|theme| ThemeEntry {
                dark: theme.theme.dark,
                id: theme.id,
                name: theme.name,
                builtin: false,
            })
            .collect();
        custom.sort_by(|a, b| a.name.cmp(&b.name));

        entries.append(&mut custom);
        entries
    }

    /// The palette `id` names, falling back to [`Theme::dark`].
    ///
    /// Ids are case-insensitive, built-in themes win over custom ones, and the
    /// two names the default themes went by before they had ids — `dark` and
    /// `light` — still resolve. An id nothing answers to falls back rather than
    /// failing: a settings file naming a theme whose file has been deleted has
    /// to keep opening the app.
    pub fn resolve(id: &str, cx: &App) -> Theme {
        if id.eq_ignore_ascii_case(LEGACY_ID_DARK) {
            return Theme::dark();
        }
        if id.eq_ignore_ascii_case(LEGACY_ID_LIGHT) {
            return Theme::light();
        }
        if let Some(builtin) = BUILTIN_THEMES
            .iter()
            .find(|theme| theme.id.eq_ignore_ascii_case(id))
        {
            return (builtin.build)();
        }
        Self::custom(cx)
            .into_iter()
            .find(|theme| theme.id.eq_ignore_ascii_case(id))
            .map(|theme| theme.theme)
            .unwrap_or_else(Theme::dark)
    }
}

/// Schema version written into a [`ThemeFile`] by this build.
const THEME_FILE_VERSION: u32 = 1;

/// Version assumed for a file that does not carry one.
fn default_theme_file_version() -> u32 {
    THEME_FILE_VERSION
}

/// One UI theme as it is written to disk.
///
/// Hand-editable by design, and read the same way `settings.json` is: keys
/// rulogman does not know are ignored, and a color it cannot parse falls back to
/// the corresponding slot of [`Theme::dark`] instead of failing the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeFile {
    /// Schema version of the file; informational until a migration is needed.
    #[serde(default = "default_theme_file_version")]
    pub version: u32,
    /// Human-readable name, shown in the picker.
    pub name: String,
    /// Whether the palette is a dark one; drives the native window caption.
    #[serde(default)]
    pub dark: bool,
    /// The palette itself.
    pub colors: ThemeColors,
}

/// The color slots of a [`ThemeFile`].
///
/// Each value is `#RRGGBB`, or `#RRGGBBAA` where the slot carries alpha —
/// which, of the eleven, only `overlay` meaningfully does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeColors {
    /// Window / app background.
    pub background: String,
    /// Background of raised chrome.
    pub surface: String,
    /// Surface color under the pointer.
    pub surface_hover: String,
    /// Surface color while pressed or selected.
    pub surface_active: String,
    /// Hairline separators and control outlines.
    pub border: String,
    /// Primary foreground color.
    pub text: String,
    /// Secondary foreground color.
    pub text_muted: String,
    /// Brand color.
    pub accent: String,
    /// Destructive actions and error states.
    pub danger: String,
    /// Successful / connected states.
    pub success: String,
    /// Translucent modal backdrop.
    pub overlay: String,
}

impl ThemeFile {
    /// The file for a name, a darkness and a set of already-written colors.
    ///
    /// The counterpart of [`ThemeFile::from_theme`] for the theme editor, which
    /// holds each slot as the string the user typed rather than as a resolved
    /// color and has to write those strings back untouched — a `#ABCDEF` the
    /// user prefers in capitals stays in capitals.
    pub fn new(name: impl Into<String>, dark: bool, colors: ThemeColors) -> Self {
        Self {
            version: THEME_FILE_VERSION,
            name: name.into(),
            dark,
            colors,
        }
    }

    /// Turn the file into a palette the widgets can use.
    ///
    /// A color that is not a `#RRGGBB` or `#RRGGBBAA` value keeps the default
    /// theme's color for that slot, which is the same forgiveness the settings
    /// loader shows a hand-edited value.
    pub fn to_theme(&self) -> Theme {
        let fallback = Theme::dark();
        let color = |value: &str, fallback: Hsla| parse_hex(value).unwrap_or(fallback);

        Palette {
            dark: self.dark,
            background: color(&self.colors.background, fallback.background),
            surface: color(&self.colors.surface, fallback.surface),
            surface_hover: color(&self.colors.surface_hover, fallback.surface_hover),
            surface_active: color(&self.colors.surface_active, fallback.surface_active),
            border: color(&self.colors.border, fallback.border),
            text: color(&self.colors.text, fallback.text),
            text_muted: color(&self.colors.text_muted, fallback.text_muted),
            accent: color(&self.colors.accent, fallback.accent),
            danger: color(&self.colors.danger, fallback.danger),
            success: color(&self.colors.success, fallback.success),
            overlay: color(&self.colors.overlay, fallback.overlay),
        }
        // Which also settles [`Theme::icon`], for a file that never mentions
        // it: a hand-written theme is legible whether or not its author
        // thought about the icons.
        .into()
    }

    /// The file that would reproduce `theme` under the name `name`.
    pub fn from_theme(name: impl Into<String>, theme: &Theme) -> Self {
        Self {
            version: THEME_FILE_VERSION,
            name: name.into(),
            dark: theme.dark,
            colors: ThemeColors {
                background: to_hex(theme.background),
                surface: to_hex(theme.surface),
                surface_hover: to_hex(theme.surface_hover),
                surface_active: to_hex(theme.surface_active),
                border: to_hex(theme.border),
                text: to_hex(theme.text),
                text_muted: to_hex(theme.text_muted),
                accent: to_hex(theme.accent),
                danger: to_hex(theme.danger),
                success: to_hex(theme.success),
                overlay: to_hex(theme.overlay),
            },
        }
    }
}

/// Parse a `#RRGGBB` or `#RRGGBBAA` string into a color.
///
/// The leading `#` is optional and the digits are case-insensitive; anything
/// else — a short `#rgb`, a color name — answers `None`.
pub fn parse_hex(value: &str) -> Option<Hsla> {
    let value = value.trim();
    let digits = value.strip_prefix('#').unwrap_or(value);
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |index: usize| {
        u8::from_str_radix(digits.get(index..index + 2)?, 16)
            .ok()
            .map(|value| value as f32 / 255.0)
    };
    let alpha = match digits.len() {
        6 => 1.0,
        8 => channel(6)?,
        _ => return None,
    };
    Some(
        Rgba {
            r: channel(0)?,
            g: channel(2)?,
            b: channel(4)?,
            a: alpha,
        }
        .into(),
    )
}

/// Format a color as the `#rrggbb` a theme file expects.
///
/// The alpha channel is only written when the color has one to write, so the
/// ten opaque slots of a theme file stay readable six-digit values.
pub fn to_hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (r, g, b) = (channel(rgba.r), channel(rgba.g), channel(rgba.b));
    if rgba.a >= 1.0 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{:02x}", channel(rgba.a))
    }
}

/// Returns the active theme, falling back to [`Theme::dark`] when the app has
/// not installed one yet.
///
/// A clone is returned rather than a borrow so that callers can keep using the
/// [`App`] mutably while styling their elements.
pub fn theme(cx: &App) -> Theme {
    cx.try_global::<Theme>().cloned().unwrap_or_default()
}

/// Installs `theme` as the active [`Theme`] global.
pub fn set_theme(theme: Theme, cx: &mut App) {
    cx.set_global(theme);
}

/// Returns `color` with its lightness shifted by `delta`, clamped to `[0, 1]`.
///
/// Used by widgets to derive hover / pressed shades from a base color without
/// having to store one entry per state in [`Theme`].
pub fn shift_lightness(color: Hsla, delta: f32) -> Hsla {
    Hsla {
        l: (color.l + delta).clamp(0.0, 1.0),
        ..color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Largest difference two colors may show and still count as the same one
    /// after a round trip through eight bits per channel.
    const CHANNEL_EPSILON: f32 = 1.0 / 255.0;

    /// Asserts that two colors survive a round trip as the same color.
    fn assert_same(left: Hsla, right: Hsla) {
        let left = Rgba::from(left);
        let right = Rgba::from(right);
        for (a, b) in [
            (left.r, right.r),
            (left.g, right.g),
            (left.b, right.b),
            (left.a, right.a),
        ] {
            assert!((a - b).abs() <= CHANNEL_EPSILON, "{left:?} != {right:?}");
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_same(
            parse_hex("#ff0000").expect("red"),
            gpui::rgb(0xff0000).into(),
        );
        assert_eq!(parse_hex("AABBCC"), parse_hex("#aabbcc"));
        assert_eq!(to_hex(gpui::rgb(0x00ff7f).into()), "#00ff7f");

        for theme in [Theme::dark(), Theme::light(), Theme::dracula()] {
            assert_same(
                parse_hex(&to_hex(theme.accent)).expect("accent"),
                theme.accent,
            );
            assert_same(
                parse_hex(&to_hex(theme.overlay)).expect("overlay"),
                theme.overlay,
            );
        }
    }

    #[test]
    fn hex_writes_alpha_only_when_there_is_some() {
        assert_eq!(to_hex(hsla(0., 0., 0., 1.0)), "#000000");
        assert_eq!(to_hex(hsla(0., 0., 0., 0.5)), "#00000080");
        assert_eq!(parse_hex("#00000080").expect("alpha").a, 128.0 / 255.0);
    }

    #[test]
    fn hex_rejects_everything_else() {
        for value in ["", "#", "#abc", "#abcde", "#gghhii", "rebeccapurple"] {
            assert!(parse_hex(value).is_none(), "accepted {value:?}");
        }
    }

    #[test]
    fn theme_file_round_trips_through_json() {
        let theme = Theme::solarized_light();
        let file = ThemeFile::from_theme("Solarized Light", &theme);
        let json = serde_json::to_string(&file).expect("serialize");
        assert!(json.contains("\"surface_hover\""), "{json}");

        let parsed: ThemeFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, file);
        assert_eq!(parsed.version, 1);
        assert!(!parsed.dark);

        let restored = parsed.to_theme();
        assert_eq!(restored.dark, theme.dark);
        for (left, right) in [
            (restored.background, theme.background),
            (restored.surface, theme.surface),
            (restored.surface_hover, theme.surface_hover),
            (restored.surface_active, theme.surface_active),
            (restored.border, theme.border),
            (restored.text, theme.text),
            (restored.text_muted, theme.text_muted),
            (restored.accent, theme.accent),
            (restored.danger, theme.danger),
            (restored.success, theme.success),
            (restored.overlay, theme.overlay),
        ] {
            assert_same(left, right);
        }
    }

    #[test]
    fn a_theme_file_tolerates_missing_and_unknown_keys() {
        let json = r##"{
            "name": "Sparse",
            "future_key": {"anything": [1, 2, 3]},
            "colors": {
                "background": "#101010",
                "surface": "#151515",
                "surface_hover": "not a color",
                "surface_active": "#252525",
                "border": "#303030",
                "text": "#e0e0e0",
                "text_muted": "#909090",
                "accent": "#3080f0",
                "danger": "#f04040",
                "success": "#40c060",
                "overlay": "#0000009e"
            }
        }"##;

        let file: ThemeFile = serde_json::from_str(json).expect("parse");
        // Both defaults apply: the version this build writes, and a light theme
        // only if the file says so.
        assert_eq!(file.version, 1);
        assert!(!file.dark);

        let theme = file.to_theme();
        assert_same(theme.background, gpui::rgb(0x101010).into());
        // The unparseable slot keeps the default theme's color.
        assert_same(theme.surface_hover, Theme::dark().surface_hover);
        assert_same(theme.overlay, gpui::rgba(0x0000009e).into());
    }

    #[test]
    fn every_builtin_id_resolves_and_is_listed_once() {
        let mut ids: Vec<&str> = BUILTIN_THEMES.iter().map(|theme| theme.id).collect();
        assert_eq!(ids.len(), 6);
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), unique, "duplicate theme id");

        for theme in &BUILTIN_THEMES {
            assert!(ThemeRegistry::is_builtin(theme.id));
            assert_eq!((theme.build)().dark, theme.dark, "{}", theme.id);
        }
        assert!(!ThemeRegistry::is_builtin("nonsense"));
    }

    /// The worst contrast an icon shows on the two backgrounds it is painted
    /// on, which is the figure [`readable_icon`] is judged by.
    fn icon_contrast(theme: &Theme) -> f32 {
        contrast_ratio(theme.icon, theme.background).min(contrast_ratio(theme.icon, theme.surface))
    }

    /// A palette built around one background and one muted text, for the cases
    /// no shipped theme covers.
    fn palette(background: Hsla, surface: Hsla, text_muted: Hsla) -> Theme {
        Palette {
            background,
            surface,
            text_muted,
            ..dark_palette()
        }
        .into()
    }

    /// The default palette, as a starting point for [`palette`].
    fn dark_palette() -> Palette {
        let theme = Theme::dark();
        Palette {
            dark: theme.dark,
            background: theme.background,
            surface: theme.surface,
            surface_hover: theme.surface_hover,
            surface_active: theme.surface_active,
            border: theme.border,
            text: theme.text,
            text_muted: theme.text_muted,
            accent: theme.accent,
            danger: theme.danger,
            success: theme.success,
            overlay: theme.overlay,
        }
    }

    #[test]
    fn contrast_is_symmetric_and_spans_the_whole_range() {
        let black = hsla(0., 0., 0., 1.0);
        let white = hsla(0., 0., 1.0, 1.0);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.01);
        assert_eq!(contrast_ratio(black, white), contrast_ratio(white, black));
        assert_eq!(contrast_ratio(white, white), 1.0);
    }

    /// The reason [`Theme::icon`] exists: every theme that ships has to clear
    /// the bar on *both* of the backgrounds an icon is drawn on, which several
    /// of them did not when the icons were painted in the muted text — and the
    /// bar is read off the implementation rather than hardcoded here, so that
    /// raising it can never leave this test agreeing with the old figure.
    #[test]
    fn every_builtin_theme_gives_its_icons_enough_contrast() {
        for builtin in &BUILTIN_THEMES {
            let theme = (builtin.build)();
            assert!(
                icon_contrast(&theme) >= MIN_ICON_CONTRAST,
                "{}: icons at {:.2}:1",
                builtin.id,
                icon_contrast(&theme)
            );
        }
    }

    /// And a palette whose muted text is already legible keeps it: the derived
    /// slot is a floor under a theme, not a restyling of one.
    #[test]
    fn a_muted_text_that_is_already_legible_is_left_alone() {
        let theme = palette(
            hsla(0., 0., 0.05, 1.0),
            hsla(0., 0., 0.10, 1.0),
            hsla(210. / 360., 0.20, 0.80, 1.0),
        );
        assert_eq!(theme.icon, theme.text_muted);
    }

    /// A palette that has to be moved keeps its hue and saturation, and moves
    /// no further than it must.
    #[test]
    fn an_illegible_muted_text_is_moved_only_in_lightness() {
        let muted = hsla(225. / 360., 0.27, 0.51, 1.0);
        let theme = palette(hsla(231. / 360., 0.15, 0.184, 1.0), muted, muted);
        assert_ne!(theme.icon, theme.text_muted);
        assert_eq!(theme.icon.h, muted.h);
        assert_eq!(theme.icon.s, muted.s);
        assert_eq!(theme.icon.a, muted.a);
        assert!(icon_contrast(&theme) >= MIN_ICON_CONTRAST);
        // The smallest move that works: a hair darker would not have.
        let short = Hsla {
            l: theme.icon.l - 0.01,
            ..theme.icon
        };
        assert!(
            contrast_ratio(short, theme.background).min(contrast_ratio(short, theme.surface))
                < MIN_ICON_CONTRAST
        );
    }

    /// The extremes answer rather than panicking, and they answer with the most
    /// legible colour on offer: a mid-grey background can still be cleared, by
    /// going dark rather than light.
    #[test]
    fn an_extreme_palette_still_gets_an_answer() {
        let grey = hsla(0., 0., 0.5, 1.0);
        let theme = palette(grey, grey, grey);
        assert!(theme.icon.l < grey.l, "the icon went the wrong way");
        assert!(icon_contrast(&theme) >= MIN_ICON_CONTRAST);

        // And a palette that no colour can satisfy — one background at each end
        // of the ramp — is pushed to an end of the axis instead of being left
        // where it was.
        let split = palette(
            hsla(0., 0., 0.0, 1.0),
            hsla(0., 0., 1.0, 1.0),
            hsla(0., 0., 0.5, 1.0),
        );
        assert!(split.icon.l == 0.0 || split.icon.l == 1.0);
    }

    /// The guarantee reaches themes rulogman never saw: a file carries no `icon`
    /// key — adding one would break every theme already written — so the slot
    /// is derived on the way in, for a hand-written palette as much as for a
    /// built-in one.
    #[test]
    fn a_theme_from_a_file_gets_the_same_guarantee() {
        let json = r##"{
            "name": "Murky",
            "dark": true,
            "colors": {
                "background": "#202430",
                "surface": "#1a1e28",
                "surface_hover": "#2a2f3c",
                "surface_active": "#333949",
                "border": "#3d4455",
                "text": "#c8ccd8",
                "text_muted": "#4a5064",
                "accent": "#6ea8fe",
                "danger": "#e05561",
                "success": "#8cc265",
                "overlay": "#0a0c129e"
            }
        }"##;

        let file: ThemeFile = serde_json::from_str(json).expect("parse");
        let muted = parse_hex("#4a5064").expect("muted");
        let theme = file.to_theme();
        assert!(
            contrast_ratio(muted, theme.background).min(contrast_ratio(muted, theme.surface))
                < MIN_ICON_CONTRAST,
            "the fixture was not the illegible palette this test needs"
        );
        assert!(icon_contrast(&theme) >= MIN_ICON_CONTRAST);

        // The file it writes back is still the eleven stored slots: the derived
        // one is not a key, so a round trip cannot invent one.
        let written = ThemeFile::from_theme("Murky", &theme);
        assert_eq!(written.colors, file.colors);
        let json = serde_json::to_string(&written).expect("serialize");
        assert!(!json.contains("icon"), "{json}");
    }

    #[test]
    fn the_builtin_themes_mirror_the_terminal_schemes() {
        // Same ids, same names, same order: picking one word in the settings
        // has to mean the same thing for the chrome and for the terminal.
        let schemes = rulogman_term::TerminalTheme::builtin();
        assert_eq!(schemes.len(), BUILTIN_THEMES.len());
        for (theme, scheme) in BUILTIN_THEMES.iter().zip(schemes) {
            assert_eq!(theme.id, scheme.id);
            assert_eq!(theme.name, scheme.name);
            assert_eq!(theme.dark, scheme.dark);
        }
    }
}
