//! Finding text in a terminal's scrollback.
//!
//! The grid keeps no text of its own, only rows of styled runs, so a search
//! reads the rows back out of the model and looks through them one at a time.
//! What comes back is where each match sits — a scrollback row and the columns
//! it covers — which is the same addressing a selection uses, so the view shows
//! a match by selecting it and copies it the way it copies anything else.
//!
//! A row is searched on its own. A match that a soft wrap has folded onto two
//! rows is not found, which is the price of keeping the columns exact: joining
//! the rows would put a column of one row and a column of the next at the same
//! offset of one string.

use rulogman_term::TerminalLine;

/// One occurrence of the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchMatch {
    /// Scrollback row, `0` being the oldest line held.
    pub line: usize,
    /// First column the match covers.
    pub start: u16,
    /// Last column the match covers, inclusive.
    pub end: u16,
}

/// The characters of `line`, lower-cased, each with the first column it sits
/// on and the number of columns it spans.
///
/// A run is either plain characters one column each, or a single cluster of
/// [`StyledRun::cells`](rulogman_term::StyledRun::cells) columns — a wide
/// character with whatever marks combine onto it — so a run of several
/// characters that is not one column per character is that one cluster, and
/// every character of it is placed on the cluster's columns.
fn cells_of(line: &TerminalLine) -> Vec<(char, u16, u16)> {
    let mut cells = Vec::new();
    for run in &line.runs {
        let chars = run.text.chars().count() as u16;
        if chars == run.cells {
            for (offset, ch) in run.text.chars().enumerate() {
                cells.push((lower(ch), run.start_col + offset as u16, 1));
            }
        } else {
            for ch in run.text.chars() {
                cells.push((lower(ch), run.start_col, run.cells.max(1)));
            }
        }
    }
    cells
}

/// `ch` the way a case-blind comparison sees it: the first character of its
/// lower case, which is the whole of it for every script a log is written in.
fn lower(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// Every occurrence of `query` in `lines`, case-blind, in reading order; the
/// first of `lines` is scrollback row `first_row`.
///
/// Occurrences never overlap: the search resumes after the end of each one, the
/// way an editor's *find next* steps. An empty query matches nothing.
pub(crate) fn find_matches(
    lines: &[TerminalLine],
    first_row: usize,
    query: &str,
) -> Vec<SearchMatch> {
    let needle: Vec<char> = query.chars().map(lower).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let cells = cells_of(line);
        let mut at = 0;
        while at + needle.len() <= cells.len() {
            let found = cells[at..at + needle.len()]
                .iter()
                .zip(&needle)
                .all(|((ch, _, _), wanted)| ch == wanted);
            if found {
                let (_, start, _) = cells[at];
                let (_, last, width) = cells[at + needle.len() - 1];
                matches.push(SearchMatch {
                    line: first_row + index,
                    start,
                    end: last + width - 1,
                });
                at += needle.len();
            } else {
                at += 1;
            }
        }
    }
    matches
}

/// The match to move to from `current`, forward or back, wrapping at either
/// end; `None` when there are none.
///
/// With no current match either step lands on the last match — the newest
/// output, which is what a log is usually searched for.
pub(crate) fn step(current: Option<usize>, count: usize, forward: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, _) => count - 1,
        (Some(index), true) => (index + 1) % count,
        (Some(index), false) => (index + count - 1) % count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rulogman_term::{Rgb, RunFlags, StyledRun};

    fn line(runs: &[(&str, u16, u16)]) -> TerminalLine {
        TerminalLine {
            runs: runs
                .iter()
                .map(|(text, start_col, cells)| StyledRun {
                    text: (*text).to_owned(),
                    start_col: *start_col,
                    cells: *cells,
                    fg: Rgb { r: 0, g: 0, b: 0 },
                    bg: Rgb { r: 0, g: 0, b: 0 },
                    flags: RunFlags::empty(),
                })
                .collect(),
            wrapped: false,
        }
    }

    #[test]
    fn matches_are_found_case_blind_with_their_columns() {
        let lines = [
            line(&[("ok", 0, 2)]),
            line(&[("[", 0, 1), ("ERROR", 1, 5), ("] disk error", 6, 12)]),
        ];
        let found = find_matches(&lines, 40, "error");
        assert_eq!(
            found,
            [
                SearchMatch {
                    line: 41,
                    start: 1,
                    end: 5
                },
                SearchMatch {
                    line: 41,
                    start: 13,
                    end: 17
                },
            ]
        );
        assert!(find_matches(&lines, 0, "").is_empty());
    }

    #[test]
    fn a_wide_character_covers_both_of_its_columns() {
        // "로그 a": two wide clusters, then a space and a letter.
        let lines = [line(&[("로", 0, 2), ("그", 2, 2), (" a", 4, 2)])];
        assert_eq!(
            find_matches(&lines, 0, "그 a"),
            [SearchMatch {
                line: 0,
                start: 2,
                end: 5
            }]
        );
    }

    #[test]
    fn stepping_wraps_and_starts_from_the_newest() {
        assert_eq!(step(None, 3, true), Some(2));
        assert_eq!(step(Some(2), 3, true), Some(0));
        assert_eq!(step(Some(0), 3, false), Some(2));
        assert_eq!(step(None, 0, true), None);
    }
}
