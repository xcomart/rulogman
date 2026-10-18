//! Compiled highlight rules, and the recolouring of a snapshot by them.
//!
//! [`rulogman_core::highlight`] holds the *model* — the patterns as text, the
//! colours as strings, the resolution that decides which list a pane runs with
//! — and deliberately owns no matcher. This is the other half: the patterns
//! compiled, the colours parsed, and the one operation the renderer needs,
//! which is to take the [`TerminalSnapshot`] it was about to paint and hand it
//! back with the matching spans wearing the rules' colours.
//!
//! # Why the snapshot is rewritten rather than overlaid
//!
//! A snapshot is already the renderer's own vocabulary — runs of cells sharing
//! one style, with `INVERSE`, `DIM` and `HIDDEN` folded into `fg`/`bg` before
//! it is built — so rewriting a run's colours is final in a way an overlay
//! could not be: nothing downstream re-derives them. It is also cheap, because
//! the snapshot is rebuilt from the grid every frame anyway; the copy this
//! mutates is the frame's own and is dropped with it.
//!
//! # First match wins
//!
//! Rules are tried in list order and a cell already coloured by an earlier rule
//! keeps what it was given. That is what makes the list a *severity* order —
//! see [`rulogman_core::highlight_preset`], which is written that way on
//! purpose — and it is why a `Line`-scope rule that matches shuts the rest of
//! the list out of that line: it claims every cell of it.
//!
//! # The wash
//!
//! A `Line`-scope rule with a background cannot be expressed by rewriting runs
//! alone. Trailing blank cells are trimmed out of a [`TerminalLine`], so a row
//! with eight characters on an eighty column grid has eight columns of runs and
//! seventy-two columns of nothing — and a background that stopped at column
//! eight would read as a highlight of the *word* rather than of the line.
//! [`Highlighter::apply`] therefore returns, per visible row, the colour that
//! row's full width has to be filled with, and the element paints it as one
//! quad underneath everything else the row draws.

use std::ops::Range;

use regex::{Regex, RegexSet};
use rulogman_core::{HighlightColor, HighlightRule, HighlightScope};
use rulogman_term::{Rgb, RunFlags, StyledRun, TerminalLine, TerminalSnapshot, TerminalTheme};

/// One rule with its pattern compiled and its colours parsed.
struct CompiledRule {
    /// The compiled pattern, already carrying `(?i)` if the rule asked for it.
    regex: Regex,
    /// Text colour, or `None` to leave the log's own foreground alone.
    fg: Option<HighlightColor>,
    /// Background colour, or `None` to leave the cell's background alone.
    bg: Option<HighlightColor>,
    /// Whether the highlighted span is drawn bold.
    bold: bool,
    /// How much of the line a match recolours.
    scope: HighlightScope,
}

/// The colours one rule imposes, resolved against a session's scheme.
///
/// Both channels stay optional all the way down to the run: a rule that only
/// names a foreground must leave a cell's own background exactly as the log's
/// escape sequences set it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    /// Text colour to impose, or `None` to keep the run's own.
    fg: Option<Rgb>,
    /// Background colour to impose, or `None` to keep the run's own.
    bg: Option<Rgb>,
    /// Whether to add [`RunFlags::BOLD`]. Never removes it: a rule saying
    /// nothing about weight has nothing to say about text the log itself
    /// emphasised.
    bold: bool,
}

/// A rule list, compiled once and applied to every frame of one pane.
///
/// Built by [`Highlighter::compile`] and held behind an `Rc` by the view, so
/// that the per-frame cost is a pointer clone rather than a recompile.
pub struct Highlighter {
    /// The usable rules, in the order they are tried.
    rules: Vec<CompiledRule>,
    /// Prefilter answering "which of these could match this line at all" in one
    /// pass, so a list of a dozen rules costs one scan rather than a dozen.
    ///
    /// `None` when the set itself would not build — the combined program has a
    /// size limit the individual patterns do not — in which case every rule is
    /// simply tried. The set is an optimisation and never a filter with an
    /// opinion of its own, so losing it changes nothing but the cost.
    set: Option<RegexSet>,
}

impl Highlighter {
    /// Compiles `rules`, dropping the ones that cannot be used.
    ///
    /// A rule is dropped when it is disabled or when its pattern does not
    /// compile, and a bad pattern is logged by name. Dropping rather than
    /// failing is the whole contract with `rulogman-core`: the pattern is
    /// stored as the user typed it, a half-finished regex in the settings
    /// dialog is a rule that never matches, and one such rule must not take the
    /// rest of the list down with it.
    ///
    /// A colour that does not parse is treated as "no colour" — the same as an
    /// absent one — and logged here rather than per frame, which is the only
    /// place it can be said once.
    pub fn compile(rules: &[HighlightRule]) -> Self {
        let mut compiled = Vec::new();
        let mut patterns = Vec::new();
        for rule in rules.iter().filter(|rule| rule.enabled) {
            // `(?i)` rather than `RegexBuilder::case_insensitive`, so that the
            // one source string is what both the rule and the prefilter set are
            // built from and the two can never disagree about a match.
            let pattern = if rule.ignore_case {
                format!("(?i){}", rule.pattern)
            } else {
                rule.pattern.clone()
            };
            let regex = match Regex::new(&pattern) {
                Ok(regex) => regex,
                Err(error) => {
                    log::warn!(
                        "ignoring the highlight rule {:?}: {error}",
                        rule.pattern.as_str()
                    );
                    continue;
                }
            };
            compiled.push(CompiledRule {
                regex,
                fg: parse_colour(rule.foreground.as_deref(), &rule.pattern),
                bg: parse_colour(rule.background.as_deref(), &rule.pattern),
                bold: rule.bold,
                scope: rule.scope,
            });
            patterns.push(pattern);
        }

        let set = match RegexSet::new(&patterns) {
            Ok(set) => Some(set),
            Err(error) => {
                log::warn!("highlight rules are matched one by one: {error}");
                None
            }
        };

        Self {
            rules: compiled,
            set,
        }
    }

    /// Whether nothing would ever be recoloured.
    ///
    /// The view keeps `None` rather than an empty highlighter, so this is what
    /// answers "is there anything to keep".
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Recolours `snapshot` in place and reports the rows needing a full-width
    /// fill.
    ///
    /// The return has exactly one entry per visible row: `Some(colour)` when a
    /// `Line`-scope rule with a background claimed the logical line that row
    /// belongs to, and `None` otherwise. See the module header for why that
    /// cannot be a run.
    ///
    /// Matching is done against *logical* lines — a soft-wrapped line is
    /// rejoined across the rows it occupies, so a word straddling the fold is
    /// still found and a `Line` rule washes every row of it. The viewport is
    /// the limit of that: a line whose head has scrolled off the top is matched
    /// on the part still visible, because the rows above are no longer in the
    /// snapshot to be read. The joint itself is the concatenation of the rows'
    /// text with nothing between them, which is what the wrap means; trailing
    /// blanks are already trimmed out of a [`TerminalLine`], so a wrapped row
    /// padded out with spaces rejoins one space short. Neither is worth a
    /// second buffer to fix: both change which characters a pattern sees at a
    /// boundary, and never which line gets coloured.
    pub fn apply(
        &self,
        snapshot: &mut TerminalSnapshot,
        theme: &TerminalTheme,
    ) -> Vec<Option<Rgb>> {
        let mut washes = vec![None; snapshot.lines.len()];
        if self.rules.is_empty() {
            return washes;
        }

        // Collected per row and applied at the end, because imposing a style on
        // part of a run means splitting that run, and a split invalidates every
        // index taken before it.
        let mut overrides: Vec<Vec<(Range<usize>, Style)>> = vec![Vec::new(); snapshot.lines.len()];

        let mut start = 0;
        while start < snapshot.lines.len() {
            let end = logical_line_end(&snapshot.lines, start);
            self.apply_to_group(
                &snapshot.lines[start..=end],
                start,
                theme,
                &mut overrides,
                &mut washes,
            );
            start = end + 1;
        }

        for (line, mut spans) in snapshot.lines.iter_mut().zip(overrides) {
            if spans.is_empty() {
                continue;
            }
            spans.sort_unstable_by_key(|(range, _)| range.start);
            recolour(line, &spans);
        }

        washes
    }

    /// Runs every rule over one logical line, recording what it claims.
    ///
    /// `first` is the index of the group's first row in the snapshot, which is
    /// what turns an offset inside the rejoined text back into a row.
    fn apply_to_group(
        &self,
        group: &[TerminalLine],
        first: usize,
        theme: &TerminalTheme,
        overrides: &mut [Vec<(Range<usize>, Style)>],
        washes: &mut [Option<Rgb>],
    ) {
        // The rejoined text, plus where each row's own text sits inside it.
        let mut text = String::new();
        let mut rows: Vec<(usize, Range<usize>)> = Vec::with_capacity(group.len());
        for (offset, line) in group.iter().enumerate() {
            let start = text.len();
            for run in &line.runs {
                text.push_str(&run.text);
            }
            rows.push((first + offset, start..text.len()));
        }

        // One byte per byte of `text`, marking what an earlier rule already
        // took. Bytes rather than columns because that is the unit a match is
        // reported in, and every boundary set here is a match boundary and so a
        // character boundary too.
        let mut claimed = vec![false; text.len()];
        // Set by the first `Line`-scope rule to match. Every later rule is then
        // inert for this line — it would find every cell claimed anyway, and
        // this is also what stops a second such rule from washing a row whose
        // text it was not allowed to colour.
        let mut line_claimed = false;

        for index in self.candidates(&text) {
            if line_claimed {
                return;
            }
            let rule = &self.rules[index];
            let style = Style {
                fg: rule.fg.map(|colour| resolve(colour, theme)),
                bg: rule.bg.map(|colour| resolve(colour, theme)),
                bold: rule.bold,
            };
            match rule.scope {
                HighlightScope::Match => {
                    // Collected first: `find_iter` borrows `text`, and claiming
                    // is a write.
                    let spans: Vec<Range<usize>> = rule
                        .regex
                        .find_iter(&text)
                        .map(|found| found.range())
                        .collect();
                    for span in spans {
                        claim(span, style, &rows, &mut claimed, overrides);
                    }
                }
                HighlightScope::Line => {
                    if !rule.regex.is_match(&text) {
                        continue;
                    }
                    claim(0..text.len(), style, &rows, &mut claimed, overrides);
                    line_claimed = true;
                    if let Some(bg) = style.bg {
                        for (row, _) in &rows {
                            washes[*row] = Some(bg);
                        }
                    }
                }
            }
        }
    }

    /// The indices of the rules that could match `text`, in list order.
    ///
    /// The prefilter's whole job: a rule that the set says cannot match is not
    /// asked to scan the line a second time.
    fn candidates(&self, text: &str) -> Vec<usize> {
        match &self.set {
            Some(set) => set.matches(text).into_iter().collect(),
            None => (0..self.rules.len()).collect(),
        }
    }
}

/// Reads one stored colour, complaining once about a spelling nothing knows.
fn parse_colour(colour: Option<&str>, pattern: &str) -> Option<HighlightColor> {
    let colour = colour?;
    let parsed = HighlightColor::parse(colour);
    if parsed.is_none() {
        log::warn!("the highlight rule {pattern:?} names an unknown colour {colour:?}");
    }
    parsed
}

/// Turns a rule's colour into the one the session's scheme means by it.
///
/// A slot rather than a literal is the normal spelling precisely so that this
/// resolution happens per session: the same rule drawn over two schemes gives
/// two colours, both of them legible against their own background.
fn resolve(colour: HighlightColor, theme: &TerminalTheme) -> Rgb {
    match colour {
        HighlightColor::Rgb { r, g, b } => Rgb::new(r, g, b),
        // Clamped rather than indexed blind: the slot is `0..16` by
        // construction, and a panic in a paint path is not the way to find out
        // that it stopped being.
        HighlightColor::Slot(slot) => theme.ansi[usize::from(slot).min(15)],
        HighlightColor::Foreground => theme.foreground,
        HighlightColor::Background => theme.background,
    }
}

/// The last row of the logical line starting at `start`.
///
/// [`TerminalLine::wrapped`] says the row *continues* onto the next one, so the
/// group ends at the first row that does not carry it — or at the bottom of the
/// viewport, whichever comes first.
fn logical_line_end(lines: &[TerminalLine], start: usize) -> usize {
    let mut end = start;
    while end + 1 < lines.len() && lines[end].wrapped {
        end += 1;
    }
    end
}

/// Records `style` over the still-unclaimed parts of `span`, and claims them.
///
/// The split into unclaimed pieces is what implements first-match-wins at the
/// granularity of a cell: a later rule overlapping an earlier one colours the
/// half that was left over rather than all or nothing.
fn claim(
    span: Range<usize>,
    style: Style,
    rows: &[(usize, Range<usize>)],
    claimed: &mut [bool],
    overrides: &mut [Vec<(Range<usize>, Style)>],
) {
    let mut at = span.start;
    while at < span.end {
        if claimed[at] {
            at += 1;
            continue;
        }
        let start = at;
        while at < span.end && !claimed[at] {
            claimed[at] = true;
            at += 1;
        }
        for (row, extent) in rows {
            let from = start.max(extent.start);
            let to = at.min(extent.end);
            if from < to {
                overrides[*row].push((from - extent.start..to - extent.start, style));
            }
        }
    }
}

/// Rewrites one row's runs so that `spans` — byte ranges into the row's own
/// text, sorted and disjoint — carry their styles.
///
/// A run is split where a span starts or ends inside it. Which is safe to do
/// only because of what a run is: an ASCII stretch, where one byte is one
/// character is one column and a slice of the text is a slice of the grid; or a
/// single non-ASCII cluster, which has no interior to address and is therefore
/// taken whole by any span touching it at all.
fn recolour(line: &mut TerminalLine, spans: &[(Range<usize>, Style)]) {
    let mut out: Vec<StyledRun> = Vec::with_capacity(line.runs.len());
    let mut offset = 0;
    for run in line.runs.drain(..) {
        let extent = offset..offset + run.text.len();
        offset = extent.end;

        if !run.text.is_ascii() {
            // One cluster, one style: a span that reaches into it covers it.
            let style = spans
                .iter()
                .find(|(span, _)| span.start < extent.end && extent.start < span.end)
                .map(|(_, style)| *style);
            out.push(styled(run, style));
            continue;
        }

        // Every boundary either span falls on, inside this run, in order.
        let mut at = 0;
        while at < run.text.len() {
            let here = extent.start + at;
            let covering = spans
                .iter()
                .find(|(span, _)| span.start <= here && here < span.end);
            let (until, style) = match covering {
                Some((span, style)) => (span.end.min(extent.end), Some(*style)),
                None => (
                    spans
                        .iter()
                        .find(|(span, _)| span.start > here)
                        .map_or(extent.end, |(span, _)| span.start.min(extent.end)),
                    None,
                ),
            };
            let until = until - extent.start;
            let piece = StyledRun {
                text: run.text[at..until].to_string(),
                // Safe on both counts because the run is ASCII: the offset is a
                // column count, and it cannot leave the grid the run is on.
                start_col: run.start_col + at as u16,
                cells: (until - at) as u16,
                ..run.clone()
            };
            out.push(styled(piece, style));
            at = until;
        }
    }
    line.runs = out;
}

/// Imposes `style` on `run`, or hands it back untouched when there is none.
fn styled(mut run: StyledRun, style: Option<Style>) -> StyledRun {
    let Some(style) = style else {
        return run;
    };
    if let Some(fg) = style.fg {
        run.fg = fg;
    }
    if let Some(bg) = style.bg {
        run.bg = bg;
    }
    if style.bold {
        run.flags |= RunFlags::BOLD;
    }
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scheme whose sixteen slots are each their own index, so a test can say
    /// which slot a cell ended up in by looking at one channel.
    fn theme() -> TerminalTheme {
        TerminalTheme {
            foreground: Rgb::new(200, 200, 200),
            background: Rgb::new(10, 10, 10),
            cursor: Rgb::new(255, 255, 255),
            selection: Rgb::new(50, 50, 50),
            ansi: std::array::from_fn(|slot| Rgb::new(slot as u8, 0, 0)),
        }
    }

    /// The colour slot `n` resolves to in [`theme`].
    fn slot(n: u8) -> Rgb {
        Rgb::new(n, 0, 0)
    }

    /// A rule with the defaults every test but the one exercising them wants.
    fn rule(pattern: &str, scope: HighlightScope) -> HighlightRule {
        HighlightRule {
            pattern: pattern.to_string(),
            foreground: None,
            background: None,
            bold: false,
            scope,
            ignore_case: false,
            enabled: true,
        }
    }

    /// One plain run of ASCII text starting at column `start_col`.
    fn run(text: &str, start_col: u16) -> StyledRun {
        StyledRun {
            text: text.to_string(),
            start_col,
            cells: text.chars().count() as u16,
            fg: Rgb::new(200, 200, 200),
            bg: Rgb::new(10, 10, 10),
            flags: RunFlags::empty(),
        }
    }

    /// A snapshot of `rows`, each `(runs, wrapped)`.
    fn snapshot(rows: Vec<(Vec<StyledRun>, bool)>) -> TerminalSnapshot {
        let lines: Vec<TerminalLine> = rows
            .into_iter()
            .map(|(runs, wrapped)| TerminalLine { runs, wrapped })
            .collect();
        TerminalSnapshot {
            cols: 80,
            rows: lines.len() as u16,
            lines,
            cursor: rulogman_term::CursorPos { line: 0, col: 0 },
            cursor_visible: false,
            display_offset: 0,
            total_scrollback: 0,
        }
    }

    /// Every run of `row`, as `(text, start_col, cells, fg, bold)`.
    fn shape(snapshot: &TerminalSnapshot, row: usize) -> Vec<(String, u16, u16, Rgb, bool)> {
        snapshot.lines[row]
            .runs
            .iter()
            .map(|run| {
                (
                    run.text.clone(),
                    run.start_col,
                    run.cells,
                    run.fg,
                    run.flags.contains(RunFlags::BOLD),
                )
            })
            .collect()
    }

    #[test]
    fn a_match_scope_rule_splits_the_run_and_colours_only_the_span() {
        let mut rules = vec![rule("INFO", HighlightScope::Match)];
        rules[0].foreground = Some("green".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![(vec![run("ab INFO cd", 0)], false)]);
        let washes = highlighter.apply(&mut snap, &theme());

        assert_eq!(
            shape(&snap, 0),
            vec![
                ("ab ".to_string(), 0, 3, Rgb::new(200, 200, 200), false),
                ("INFO".to_string(), 3, 4, slot(2), false),
                (" cd".to_string(), 7, 3, Rgb::new(200, 200, 200), false),
            ]
        );
        // Nothing to fill: only a line-scope background asks for one.
        assert_eq!(washes, vec![None]);
    }

    #[test]
    fn a_line_scope_rule_colours_every_run_and_washes_every_row_it_wraps_over() {
        let mut rules = vec![rule("ERROR", HighlightScope::Line)];
        rules[0].foreground = Some("bright_red".into());
        rules[0].background = Some("red".into());
        rules[0].bold = true;
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![
            (vec![run("ERROR whi", 0)], true),
            (vec![run("le doing ", 0), run("things", 9)], false),
        ]);
        let washes = highlighter.apply(&mut snap, &theme());

        for row in 0..2 {
            for run in &snap.lines[row].runs {
                assert_eq!(run.fg, slot(9), "row {row}");
                assert_eq!(run.bg, slot(1), "row {row}");
                assert!(run.flags.contains(RunFlags::BOLD), "row {row}");
            }
        }
        // Nothing was split: the whole line took one style.
        assert_eq!(snap.lines[0].runs.len(), 1);
        assert_eq!(snap.lines[1].runs.len(), 2);
        assert_eq!(washes, vec![Some(slot(1)), Some(slot(1))]);
    }

    #[test]
    fn a_match_on_the_second_row_of_a_wrap_group_washes_both_rows() {
        // The word is only on the continuation row, but the *line* is what a
        // line-scope rule claims, and the line began a row earlier.
        let mut rules = vec![rule("ERROR", HighlightScope::Line)];
        rules[0].background = Some("red".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![
            (vec![run("a quiet start", 0)], true),
            (vec![run("then ERROR", 0)], false),
        ]);
        let washes = highlighter.apply(&mut snap, &theme());

        assert_eq!(washes, vec![Some(slot(1)), Some(slot(1))]);
        assert_eq!(snap.lines[0].runs[0].bg, slot(1));
    }

    #[test]
    fn a_word_straddling_the_fold_is_still_found() {
        // The whole reason the group is matched as one string: neither row on
        // its own contains `ERROR`.
        let mut rules = vec![rule("ERROR", HighlightScope::Line)];
        rules[0].foreground = Some("bright_red".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![
            (vec![run("ERR", 0)], true),
            (vec![run("OR: boom", 0)], false),
        ]);
        highlighter.apply(&mut snap, &theme());

        assert_eq!(snap.lines[0].runs[0].fg, slot(9));
        assert_eq!(snap.lines[1].runs[0].fg, slot(9));
    }

    #[test]
    fn the_first_matching_rule_keeps_the_cells_it_took() {
        // Two rules over one word. The first colours it; the second gets what
        // is left, which here is everything after it.
        let mut first = rule("FATAL", HighlightScope::Match);
        first.foreground = Some("bright_white".into());
        let mut second = rule("FATAL: gone", HighlightScope::Match);
        second.foreground = Some("yellow".into());
        let highlighter = Highlighter::compile(&[first, second]);

        let mut snap = snapshot(vec![(vec![run("FATAL: gone", 0)], false)]);
        highlighter.apply(&mut snap, &theme());

        assert_eq!(
            shape(&snap, 0),
            vec![
                ("FATAL".to_string(), 0, 5, slot(15), false),
                (": gone".to_string(), 5, 6, slot(3), false),
            ]
        );
    }

    #[test]
    fn a_line_rule_shuts_the_rest_of_the_list_out_of_that_line() {
        let mut first = rule("ERROR", HighlightScope::Line);
        first.foreground = Some("bright_red".into());
        let mut second = rule("boom", HighlightScope::Line);
        second.foreground = Some("cyan".into());
        second.background = Some("blue".into());
        let highlighter = Highlighter::compile(&[first, second]);

        let mut snap = snapshot(vec![(vec![run("ERROR boom", 0)], false)]);
        let washes = highlighter.apply(&mut snap, &theme());

        assert_eq!(snap.lines[0].runs.len(), 1);
        assert_eq!(snap.lines[0].runs[0].fg, slot(9));
        // The second rule named a background, but it never got to colour a
        // cell; washing the row would have been a fill with no text under it.
        assert_eq!(washes, vec![None]);
    }

    #[test]
    fn a_disabled_rule_is_inert() {
        let mut disabled = rule("ERROR", HighlightScope::Line);
        disabled.foreground = Some("bright_red".into());
        disabled.enabled = false;
        let highlighter = Highlighter::compile(&[disabled]);
        assert!(highlighter.is_empty());

        let mut snap = snapshot(vec![(vec![run("ERROR", 0)], false)]);
        let before = snap.clone();
        let washes = highlighter.apply(&mut snap, &theme());
        assert_eq!(snap.lines, before.lines);
        assert_eq!(washes, vec![None]);
    }

    #[test]
    fn an_invalid_pattern_is_skipped_and_the_others_still_apply() {
        let mut broken = rule("(unclosed", HighlightScope::Match);
        broken.foreground = Some("red".into());
        let mut good = rule("ok", HighlightScope::Match);
        good.foreground = Some("green".into());
        let highlighter = Highlighter::compile(&[broken, good]);
        assert!(!highlighter.is_empty());

        let mut snap = snapshot(vec![(vec![run("ok", 0)], false)]);
        highlighter.apply(&mut snap, &theme());
        assert_eq!(snap.lines[0].runs[0].fg, slot(2));
    }

    #[test]
    fn an_unparseable_colour_leaves_that_channel_alone() {
        let mut rules = vec![rule("ERROR", HighlightScope::Line)];
        rules[0].foreground = Some("chartreuse".into());
        rules[0].background = Some("red".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![(vec![run("ERROR", 0)], false)]);
        highlighter.apply(&mut snap, &theme());

        assert_eq!(snap.lines[0].runs[0].fg, Rgb::new(200, 200, 200));
        assert_eq!(snap.lines[0].runs[0].bg, slot(1));
    }

    #[test]
    fn a_cjk_cluster_is_taken_whole_or_left_alone() {
        // `한` is one cluster in a run of its own, two columns wide. The first
        // match reaches it, the second does not.
        let mut rules = vec![rule("b한", HighlightScope::Match)];
        rules[0].foreground = Some("green".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![(
            vec![
                run("ab", 0),
                StyledRun {
                    text: "한".to_string(),
                    start_col: 2,
                    cells: 2,
                    fg: Rgb::new(200, 200, 200),
                    bg: Rgb::new(10, 10, 10),
                    flags: RunFlags::empty(),
                },
                run("cd", 4),
            ],
            false,
        )]);
        highlighter.apply(&mut snap, &theme());

        assert_eq!(
            shape(&snap, 0),
            vec![
                ("a".to_string(), 0, 1, Rgb::new(200, 200, 200), false),
                ("b".to_string(), 1, 1, slot(2), false),
                // Whole, and still two columns wide at column two.
                ("한".to_string(), 2, 2, slot(2), false),
                ("cd".to_string(), 4, 2, Rgb::new(200, 200, 200), false),
            ]
        );
    }

    #[test]
    fn a_match_that_misses_a_cluster_leaves_it_untouched() {
        let mut rules = vec![rule("cd", HighlightScope::Match)];
        rules[0].foreground = Some("green".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![(
            vec![
                StyledRun {
                    text: "한".to_string(),
                    start_col: 0,
                    cells: 2,
                    fg: Rgb::new(200, 200, 200),
                    bg: Rgb::new(10, 10, 10),
                    flags: RunFlags::empty(),
                },
                run("cd", 2),
            ],
            false,
        )]);
        highlighter.apply(&mut snap, &theme());

        assert_eq!(
            shape(&snap, 0),
            vec![
                ("한".to_string(), 0, 2, Rgb::new(200, 200, 200), false),
                ("cd".to_string(), 2, 2, slot(2), false),
            ]
        );
    }

    #[test]
    fn ignore_case_is_the_rules_own_business() {
        let mut insensitive = rule("error", HighlightScope::Match);
        insensitive.foreground = Some("green".into());
        insensitive.ignore_case = true;
        let highlighter = Highlighter::compile(&[insensitive]);
        let mut snap = snapshot(vec![(vec![run("ERROR", 0)], false)]);
        highlighter.apply(&mut snap, &theme());
        assert_eq!(snap.lines[0].runs[0].fg, slot(2));

        let mut sensitive = rule("error", HighlightScope::Match);
        sensitive.foreground = Some("green".into());
        let highlighter = Highlighter::compile(&[sensitive]);
        let mut snap = snapshot(vec![(vec![run("ERROR", 0)], false)]);
        highlighter.apply(&mut snap, &theme());
        assert_eq!(snap.lines[0].runs[0].fg, Rgb::new(200, 200, 200));
    }

    #[test]
    fn a_hex_colour_is_taken_literally() {
        let mut rules = vec![rule("x", HighlightScope::Match)];
        rules[0].foreground = Some("#ff8000".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![(vec![run("x", 0)], false)]);
        highlighter.apply(&mut snap, &theme());
        assert_eq!(snap.lines[0].runs[0].fg, Rgb::new(0xff, 0x80, 0x00));
    }

    #[test]
    fn a_group_ends_at_the_first_unwrapped_row() {
        // Three rows, two logical lines: the rule matches the first and must
        // not reach the third.
        let mut rules = vec![rule("ERROR", HighlightScope::Line)];
        rules[0].background = Some("red".into());
        let highlighter = Highlighter::compile(&rules);

        let mut snap = snapshot(vec![
            (vec![run("ERROR", 0)], true),
            (vec![run(" continued", 0)], false),
            (vec![run("all quiet", 0)], false),
        ]);
        let washes = highlighter.apply(&mut snap, &theme());

        assert_eq!(washes, vec![Some(slot(1)), Some(slot(1)), None]);
        assert_eq!(snap.lines[2].runs[0].bg, Rgb::new(10, 10, 10));
    }

    #[test]
    fn a_blank_snapshot_and_an_empty_rule_list_both_come_back_untouched() {
        let highlighter = Highlighter::compile(&[]);
        assert!(highlighter.is_empty());
        let mut snap = snapshot(vec![(vec![], false), (vec![], false)]);
        assert_eq!(highlighter.apply(&mut snap, &theme()), vec![None, None]);

        let mut rules = vec![rule("ERROR", HighlightScope::Line)];
        rules[0].background = Some("red".into());
        let highlighter = Highlighter::compile(&rules);
        let mut snap = snapshot(vec![(vec![], false)]);
        assert_eq!(highlighter.apply(&mut snap, &theme()), vec![None]);
        assert!(snap.lines[0].runs.is_empty());
    }

    #[test]
    fn every_row_gets_exactly_one_wash_entry() {
        let highlighter = Highlighter::compile(&rulogman_core::highlight_preset());
        let mut snap = snapshot(vec![
            (vec![run("2024 INFO started", 0)], false),
            (vec![run("2024 WARN slow", 0)], false),
            (vec![run("2024 FATAL gone", 0)], false),
        ]);
        let washes = highlighter.apply(&mut snap, &theme());
        assert_eq!(washes.len(), snap.lines.len());
        // Only `fatal` names a background in the preset.
        assert_eq!(washes[0], None);
        assert_eq!(washes[1], None);
        assert!(washes[2].is_some());
    }

    #[test]
    fn the_preset_colours_the_word_for_info_and_the_line_for_a_severity() {
        let highlighter = Highlighter::compile(&rulogman_core::highlight_preset());

        let mut snap = snapshot(vec![(vec![run("12:00 info ready", 0)], false)]);
        highlighter.apply(&mut snap, &theme());
        assert_eq!(
            shape(&snap, 0)
                .into_iter()
                .map(|(text, _, _, fg, _)| (text, fg))
                .collect::<Vec<_>>(),
            vec![
                ("12:00 ".to_string(), Rgb::new(200, 200, 200)),
                ("info".to_string(), slot(2)),
                (" ready".to_string(), Rgb::new(200, 200, 200)),
            ]
        );

        let mut snap = snapshot(vec![(vec![run("12:00 warn slow", 0)], false)]);
        highlighter.apply(&mut snap, &theme());
        assert_eq!(snap.lines[0].runs.len(), 1);
        assert_eq!(snap.lines[0].runs[0].fg, slot(3));
    }
}
