//! The colours the text surface is drawn in, worked out from the *terminal*
//! colour scheme.
//!
//! [`ruui_editor`] draws a buffer in a [`ruui::EditorTheme`] — twenty-one slots
//! a lexer's token classes are looked up in — and every other application built
//! on it takes that palette from the widget layer's own global, picked
//! independently of the chrome. rulogman does not, and this module is the whole
//! of why: an editor pane sits beside a terminal pane showing the same host, and
//! the two surfaces reading as one material is what stops the split looking like
//! two applications glued together. A user who picked Solarized for their shell
//! gets Solarized for the file they open out of it, without having chosen twice.
//!
//! The syntax slots follow the same rule and are the reason it matters most: a
//! scheme's ANSI sixteen are what its author chose to have `ls` and `git diff`
//! and a shell prompt drawn in, so taking a string's green from there is taking
//! it from the person who already decided what green means on this screen.
//! Inventing a syntax palette instead would put two colour systems side by side
//! in one window.
//!
//! This stayed here rather than moving into the widget with the editor because
//! it is the one half of the old `src/editor/` that is *about a terminal*. The
//! widget has never heard of a session, a scheme or an ANSI sixteen, and the
//! database tools that share it have no terminal to match.

use rulogman_term::{Rgb, TerminalTheme};
use ruui::EditorTheme;

use crate::terminal_view::to_hsla;

/// How far the caret's line is lifted off the background, as a mix of the
/// foreground into it.
///
/// Small enough that a wash across the full width of the pane does not read as
/// a selection, which is the one thing it must never be confused with.
const LINE_HIGHLIGHT_MIX: f32 = 0.08;

/// How far a line number is lifted off the background.
///
/// Halfway is what makes the gutter legible without competing with the text: a
/// scheme's own `foreground` is what the *content* is drawn in, and numbers as
/// loud as the content would be read as content.
const GUTTER_MIX: f32 = 0.5;

/// Index of normal red in the sixteen-slot ANSI palette.
const ANSI_RED: usize = 1;
/// Index of normal green in the sixteen-slot ANSI palette.
const ANSI_GREEN: usize = 2;
/// Index of normal yellow in the sixteen-slot ANSI palette.
const ANSI_YELLOW: usize = 3;
/// Index of normal blue in the sixteen-slot ANSI palette.
const ANSI_BLUE: usize = 4;
/// Index of normal magenta in the sixteen-slot ANSI palette.
const ANSI_MAGENTA: usize = 5;
/// Index of normal cyan in the sixteen-slot ANSI palette.
const ANSI_CYAN: usize = 6;
/// Index of bright black in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_BLACK: usize = 8;
/// Index of bright yellow in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_YELLOW: usize = 11;
/// Index of bright blue in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_BLUE: usize = 12;
/// Index of bright magenta in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_MAGENTA: usize = 13;
/// Index of bright cyan in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_CYAN: usize = 14;

/// The least contrast ratio a syntax colour may stand at against the page it is
/// drawn on.
///
/// Well under the 4.5 that WCAG asks of body text, and deliberately: an ANSI
/// sixteen is chosen to be *distinguishable*, not to be legible as prose, and
/// holding a scheme's own blue to 4.5 against its own background would reject
/// half the schemes people actually use. What this number is really for is the
/// pathological case — a scheme whose normal blue is nearly its background —
/// where the token would otherwise vanish entirely.
const MIN_CONTRAST: f32 = 2.2;

/// The lower bar a comment is held to. A comment is *meant* to recede, so it is
/// only lifted when it has all but disappeared.
const MIN_COMMENT_CONTRAST: f32 = 1.6;

/// How many steps [`legible`] takes from a colour towards the foreground.
const LEGIBILITY_STEPS: u8 = 4;

/// `a` with `t` of `b` mixed into it, channel by channel.
///
/// Mixing rather than compositing with an alpha, because both slots this feeds
/// are painted as an *opaque* fill under the text. The editor background is
/// itself opaque, so a translucent wash would be composited against it anyway,
/// and doing the arithmetic here means the fills stack predictably instead of
/// each one darkening whatever it happens to land on top of.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0., 1.);
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    Rgb::new(channel(a.r, b.r), channel(a.g, b.g), channel(a.b, b.b))
}

/// The relative luminance of `colour`, as sRGB defines it.
///
/// The gamma-corrected form rather than a plain average, because the whole
/// point of the number is to predict what the eye will do with the colour, and
/// a plain average calls a saturated blue as bright as a saturated green.
fn luminance(colour: Rgb) -> f32 {
    let channel = |value: u8| {
        let value = f32::from(value) / 255.;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b)
}

/// The WCAG contrast ratio between two colours, from 1 to 21.
fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

/// `colour`, lifted towards `foreground` until it stands `least` off
/// `background`.
///
/// The defence against a scheme whose ANSI colour for some token happens to sit
/// on top of its own background — which is not hypothetical: several popular
/// dark schemes put bright black within a few percent of the page. Walking
/// towards the *foreground* rather than towards white or black is what keeps
/// the result in the scheme's own family: a washed-out Solarized blue is still
/// recognisably Solarized, and the last step is the foreground itself, which
/// the scheme has already guaranteed is readable.
fn legible(colour: Rgb, background: Rgb, foreground: Rgb, least: f32) -> Rgb {
    (0..=LEGIBILITY_STEPS)
        .map(|step| {
            mix(
                colour,
                foreground,
                f32::from(step) / f32::from(LEGIBILITY_STEPS),
            )
        })
        .find(|candidate| contrast(*candidate, background) >= least)
        .unwrap_or(foreground)
}

/// The editor palette for a terminal colour `scheme`.
///
/// The four slots a scheme names outright — background, foreground, cursor and
/// selection — are taken verbatim, so a caret in the editor and a caret in the
/// terminal beside it are the same mark, and a selection in either is the same
/// block of colour. The three it does not name are mixed out of the ones it
/// does, which is what keeps a scheme's contrast intact whether it is a dark
/// one or a light one: nothing here assumes the background is the darker of the
/// two. `dark` is the answer to which of the two *is* darker, asked of the
/// luminances rather than guessed from a threshold.
///
/// # The token classes
///
/// Each is an ANSI colour of the scheme, put through [`legible`] so that a
/// scheme which happens to place one on top of its own background does not lose
/// the token entirely:
///
/// | slot          | ANSI              | why                                                                     |
/// |---------------|-------------------|-------------------------------------------------------------------------|
/// | comment       | 8 bright black    | the one slot in the sixteen whose job already *is* to be quiet          |
/// | string        | 2 green           | what every terminal scheme, and every editor theme after them, uses     |
/// | number        | 5 magenta         | a constant; the literals `true` and `null` are constants and share it   |
/// | keyword       | 4 blue            | the structural colour, and the one a prompt uses for a directory        |
/// | key           | 6 cyan            | beside blue in hue and clearly apart from it, which is what a `key: value` line needs |
/// | variable      | 3 yellow          | *something is substituted here*                                         |
/// | function      | 12 bright blue    | a call is a keyword-shaped thing, one step louder                       |
/// | type          | 11 bright yellow  | the hue every editor theme after Atom spends on a type name             |
/// | operator      | 14 bright cyan    | punctuation that means something, beside the `key:` it shares a hue with |
/// | bracket_match | 13 bright magenta | a mark rather than a class: it has to be found, not read                 |
/// | error         | 1 red             | the one thing red is for on a terminal                                   |
/// | warning       | 3 yellow          | see below                                                                |
///
/// `number` and the `true`/`false`/`null` literals share one slot rather than
/// having two: no format here treats a boolean as a different kind of thing
/// from a number, and a palette that split them would be asking every future
/// reader to tell two magentas apart for nothing.
///
/// `warning` takes the same yellow as `variable`, which looks like a collision
/// and is not one. Nothing rulogman opens a file with ever emits a *warning
/// token* — that class belongs to a language with a parser behind it — but the
/// editor paints its find matches as `warning` at a fifth and a half of its
/// alpha, and yellow is the one hue a palette reserves for *look here* rather
/// than for an error or for success. One is a glyph colour and the other a
/// block painted behind glyphs, so the two can never be read as each other.
///
/// `identifier` and `punctuation` are the foreground itself. They are what a
/// buffer is mostly made of, and a palette that tints them tints the whole
/// file; leaving them alone is also the claim the design rests on, that a file
/// with nothing to highlight looks exactly as it did before there were lexers.
pub fn palette_for(scheme: &TerminalTheme) -> EditorTheme {
    let background = scheme.background;
    let foreground = scheme.foreground;
    let syntax = |index: usize| {
        to_hsla(legible(
            scheme.ansi[index],
            background,
            foreground,
            MIN_CONTRAST,
        ))
    };
    EditorTheme {
        dark: luminance(background) < luminance(foreground),
        background: to_hsla(background),
        foreground: to_hsla(foreground),
        cursor: to_hsla(scheme.cursor),
        selection: to_hsla(scheme.selection),
        line_highlight: to_hsla(mix(background, foreground, LINE_HIGHLIGHT_MIX)),
        gutter: to_hsla(mix(background, foreground, GUTTER_MIX)),
        gutter_active: to_hsla(foreground),
        comment: to_hsla(legible(
            scheme.ansi[ANSI_BRIGHT_BLACK],
            background,
            foreground,
            MIN_COMMENT_CONTRAST,
        )),
        string: syntax(ANSI_GREEN),
        number: syntax(ANSI_MAGENTA),
        keyword: syntax(ANSI_BLUE),
        key: syntax(ANSI_CYAN),
        variable: syntax(ANSI_YELLOW),
        function: syntax(ANSI_BRIGHT_BLUE),
        r#type: syntax(ANSI_BRIGHT_YELLOW),
        operator: syntax(ANSI_BRIGHT_CYAN),
        bracket_match: syntax(ANSI_BRIGHT_MAGENTA),
        error: syntax(ANSI_RED),
        warning: syntax(ANSI_YELLOW),
        identifier: to_hsla(foreground),
        punctuation: to_hsla(foreground),
    }
}

#[cfg(test)]
mod tests {
    use gpui::Hsla;

    use super::*;

    /// The twenty-one colour slots, so a property can be asserted of all of
    /// them at once. `dark` is the one field that is not a colour and is left
    /// out.
    fn slots(palette: &EditorTheme) -> [Hsla; 21] {
        [
            palette.background,
            palette.foreground,
            palette.cursor,
            palette.selection,
            palette.line_highlight,
            palette.gutter,
            palette.gutter_active,
            palette.comment,
            palette.string,
            palette.number,
            palette.keyword,
            palette.key,
            palette.variable,
            palette.function,
            palette.r#type,
            palette.operator,
            palette.bracket_match,
            palette.error,
            palette.warning,
            palette.identifier,
            palette.punctuation,
        ]
    }

    /// Every slot derived from an ANSI colour, paired with the index and the
    /// least contrast it was derived under.
    ///
    /// `warning` is left out: it is the same derivation as `variable` and would
    /// only assert the mapping twice.
    fn syntax_slots(palette: &EditorTheme) -> [(Hsla, usize, f32); 11] {
        [
            (palette.comment, ANSI_BRIGHT_BLACK, MIN_COMMENT_CONTRAST),
            (palette.string, ANSI_GREEN, MIN_CONTRAST),
            (palette.number, ANSI_MAGENTA, MIN_CONTRAST),
            (palette.keyword, ANSI_BLUE, MIN_CONTRAST),
            (palette.key, ANSI_CYAN, MIN_CONTRAST),
            (palette.variable, ANSI_YELLOW, MIN_CONTRAST),
            (palette.function, ANSI_BRIGHT_BLUE, MIN_CONTRAST),
            (palette.r#type, ANSI_BRIGHT_YELLOW, MIN_CONTRAST),
            (palette.operator, ANSI_BRIGHT_CYAN, MIN_CONTRAST),
            (palette.bracket_match, ANSI_BRIGHT_MAGENTA, MIN_CONTRAST),
            (palette.error, ANSI_RED, MIN_CONTRAST),
        ]
    }

    /// The classes one line of one language can actually show at once, which is
    /// the only set "no two of these may collide" is a real claim about.
    ///
    /// Two groups, because no language here has both: a configuration format
    /// has keys and expansions and no types, and a language with a compiler has
    /// types and calls and no `key:` at the head of a line.
    fn co_occurring(palette: &EditorTheme) -> [Vec<Hsla>; 2] {
        [
            vec![
                palette.comment,
                palette.string,
                palette.number,
                palette.keyword,
                palette.key,
                palette.variable,
            ],
            vec![
                palette.comment,
                palette.string,
                palette.number,
                palette.keyword,
                palette.function,
                palette.r#type,
                palette.operator,
            ],
        ]
    }

    /// Every built-in scheme with its name, which is the set every palette
    /// property is asserted over.
    ///
    /// Read off the registry rather than listed here, so that a scheme added
    /// later is held to the same properties without anybody remembering to add
    /// it.
    fn schemes() -> Vec<(&'static str, TerminalTheme)> {
        TerminalTheme::builtin()
            .iter()
            .map(|info| (info.name, TerminalTheme::by_name_or_default(info.id)))
            .collect()
    }

    #[test]
    fn mixing_walks_from_one_colour_to_the_other_and_clamps() {
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(255, 255, 255);
        assert_eq!(mix(black, white, 0.), black);
        assert_eq!(mix(black, white, 1.), white);
        assert_eq!(mix(black, white, 0.5), Rgb::new(128, 128, 128));
        // Out of range is clamped rather than extrapolated: a share past either
        // end would leave the visible range and wrap the channel.
        assert_eq!(mix(black, white, -1.), black);
        assert_eq!(mix(black, white, 2.), white);
    }

    #[test]
    fn the_four_colours_a_scheme_names_are_taken_verbatim() {
        // The whole point of deriving from the scheme: a caret in the editor
        // and a caret in the terminal beside it have to be the same mark.
        let scheme = TerminalTheme::solarized_dark();
        let palette = palette_for(&scheme);
        assert_eq!(palette.background, to_hsla(scheme.background));
        assert_eq!(palette.foreground, to_hsla(scheme.foreground));
        assert_eq!(palette.cursor, to_hsla(scheme.cursor));
        assert_eq!(palette.selection, to_hsla(scheme.selection));
        // And the one slot that is a rename rather than a mix.
        assert_eq!(palette.gutter_active, to_hsla(scheme.foreground));
    }

    #[test]
    fn every_slot_is_opaque_in_every_built_in_scheme() {
        // The frame slots are painted as fills under the text, over a
        // background that is itself opaque, and the editor derives the find
        // fills from `warning` by taking an alpha of it. An alpha already in a
        // slot would make one highlight darken whatever it landed on top of.
        for (name, scheme) in schemes() {
            for colour in slots(&palette_for(&scheme)) {
                assert_eq!(colour.a, 1., "a slot of {name} is translucent");
            }
        }
    }

    #[test]
    fn contrast_is_the_ratio_the_specification_defines() {
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(255, 255, 255);
        // The two ends of the scale, and the symmetry the definition has.
        assert!((contrast(black, white) - 21.).abs() < 0.01);
        assert!((contrast(white, black) - 21.).abs() < 0.01);
        assert!((contrast(white, white) - 1.).abs() < 0.001);
    }

    #[test]
    fn a_colour_that_would_vanish_is_lifted_and_one_that_would_not_is_left_alone() {
        let background = Rgb::new(0, 0, 0);
        let foreground = Rgb::new(255, 255, 255);
        // Already legible: taken verbatim, because a scheme's own colour is the
        // whole point and the guard is not a filter.
        let green = Rgb::new(0, 200, 0);
        assert_eq!(legible(green, background, foreground, MIN_CONTRAST), green);
        // All but invisible: walked towards the foreground until it is not.
        let lost = Rgb::new(10, 10, 10);
        let lifted = legible(lost, background, foreground, MIN_CONTRAST);
        assert_ne!(lifted, lost);
        assert!(contrast(lifted, background) >= MIN_CONTRAST);
    }

    #[test]
    fn each_syntax_slot_is_the_ansi_colour_the_scheme_named() {
        // The mapping the documentation on `palette_for` sets out, asserted
        // rather than described: a scheme's green is the string colour and
        // nothing else decides it.
        for (name, scheme) in schemes() {
            let palette = palette_for(&scheme);
            for (slot, index, least) in syntax_slots(&palette) {
                let expected = legible(
                    scheme.ansi[index],
                    scheme.background,
                    scheme.foreground,
                    least,
                );
                assert_eq!(slot, to_hsla(expected), "{name} at ANSI {index}");
            }
            // And the two classes that are the foreground rather than a hue.
            assert_eq!(palette.identifier, to_hsla(scheme.foreground), "{name}");
            assert_eq!(palette.punctuation, to_hsla(scheme.foreground), "{name}");
            // The find fill is `warning`, and `warning` is the yellow the
            // find fill has always been drawn from.
            assert_eq!(palette.warning, palette.variable, "{name}");
        }
    }

    #[test]
    fn every_syntax_colour_stands_off_the_page_in_every_built_in_scheme() {
        // Not a tautology: `legible` gives up after four steps and falls back
        // to the foreground, so this is the claim that no built-in scheme needs
        // the fallback to fail. Solarized Dark is the one this was written for
        // — its bright black is three percent off its background.
        for (name, scheme) in schemes() {
            let palette = palette_for(&scheme);
            for (_, index, least) in syntax_slots(&palette) {
                let colour = legible(
                    scheme.ansi[index],
                    scheme.background,
                    scheme.foreground,
                    least,
                );
                assert!(
                    contrast(colour, scheme.background) >= least,
                    "ANSI {index} of {name} would have been lost"
                );
            }
        }
    }

    #[test]
    fn the_syntax_slots_are_told_apart_from_one_another() {
        for (name, scheme) in schemes() {
            let palette = palette_for(&scheme);
            for group in co_occurring(&palette) {
                for (index, colour) in group.iter().enumerate() {
                    assert_ne!(
                        *colour, palette.background,
                        "a token drawn on itself in {name}"
                    );
                    for other in &group[index + 1..] {
                        assert_ne!(colour, other, "two syntax slots collided in {name}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_plain_token_is_the_scheme_s_own_foreground() {
        // The claim the whole design rests on: a file with nothing to highlight
        // looks exactly as it did before there were lexers.
        let scheme = TerminalTheme::gruvbox_dark();
        let palette = palette_for(&scheme);
        assert_eq!(palette.foreground, to_hsla(scheme.foreground));
        assert_eq!(palette.identifier, to_hsla(scheme.foreground));
        assert_eq!(palette.punctuation, to_hsla(scheme.foreground));
    }

    #[test]
    fn the_marks_that_can_overlap_are_told_apart() {
        for scheme in [TerminalTheme::dark(), TerminalTheme::light()] {
            let palette = palette_for(&scheme);
            // The find fill is an alpha of `warning`, so it is only ever seen
            // where that colour is not the page it is painted on. The same goes
            // for the wash across the caret's line.
            assert_ne!(palette.warning, palette.background);
            assert_ne!(palette.line_highlight, palette.background);
            // A selection drawn *in* the highlight band would vanish on the one
            // line it is most likely to be made on.
            assert_ne!(palette.selection, palette.line_highlight);
            // The gutter is quieter than the line number the caret is on.
            assert_ne!(palette.gutter, palette.gutter_active);
        }
    }

    #[test]
    fn a_different_scheme_is_a_different_palette() {
        // What makes switching the scheme — from the settings, or per session —
        // actually reach the text surface. The light one is also the one that
        // says `dark` is read off the scheme and not assumed.
        let dark = palette_for(&TerminalTheme::dark());
        let light = palette_for(&TerminalTheme::light());
        assert_ne!(slots(&dark), slots(&light));
        assert!(dark.dark);
        assert!(!light.dark);
    }
}
