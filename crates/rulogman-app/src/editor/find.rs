//! Find and replace: the matching, and the state the bar keeps.
//!
//! Plain substring search, not a regular expression. What people look for in a
//! log is a request id, a host name or the word `error`, and a regex engine
//! here would be more than a megabyte of DFA machinery for a feature nothing
//! else in the application wants. If a pattern language is ever needed it goes
//! in as an option beside this, not instead of it.
//!
//! Matches are **non-overlapping** and found left to right: searching `aa` in
//! `aaaa` finds two, not three. That is what makes "replace all" a single
//! left-to-right pass with a running offset correction, and what makes
//! `find next` terminate.
//!
//! Case-insensitive matching compares `char::to_lowercase` a character at a
//! time rather than lowercasing the haystack, because lowercasing changes byte
//! lengths — `İ` is two bytes and lowercases to three — and every offset this
//! module hands back has to be a byte offset into the buffer as it stands.

use std::ops::Range;

/// Every non-overlapping occurrence of `needle` in `haystack`.
///
/// An empty needle matches nothing, which is what keeps an empty find bar from
/// highlighting the whole buffer.
pub fn find_all(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        // The common case, and `str::match_indices` is already
        // non-overlapping.
        return haystack
            .match_indices(needle)
            .map(|(at, text)| at..at + text.len())
            .collect();
    }

    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let mut matches = Vec::new();
    let mut at = 0;
    while at < haystack.len() {
        if let Some(end) = match_at(haystack, at, &needle) {
            matches.push(at..end);
            at = end;
        } else {
            // Step by one character, never by one byte: the next candidate
            // start is the next character boundary.
            at += haystack[at..]
                .chars()
                .next()
                .map_or(1, |first| first.len_utf8());
        }
    }
    matches
}

/// Where a case-insensitive match starting at `at` ends, if there is one.
fn match_at(haystack: &str, at: usize, needle_lower: &[char]) -> Option<usize> {
    let mut wanted = needle_lower.iter().copied();
    let mut end = at;
    for ch in haystack[at..].chars() {
        for lowered in ch.to_lowercase() {
            if wanted.next()? != lowered {
                return None;
            }
        }
        end += ch.len_utf8();
        if wanted.len() == 0 {
            return Some(end);
        }
    }
    // The haystack ran out first.
    (wanted.len() == 0).then_some(end)
}

/// What the find bar is showing and what it has found.
#[derive(Debug, Default)]
pub struct FindState {
    /// Whether the bar is on screen at all.
    pub open: bool,
    /// Whether the replace row is showing beneath the query row.
    pub replacing: bool,
    /// Whether the search distinguishes case.
    pub case_sensitive: bool,
    /// The query the matches below were found with, so that a query that has
    /// not changed does not cost a re-scan.
    pub query: String,
    /// Every match, in order.
    pub matches: Vec<Range<usize>>,
    /// Which match is the current one, an index into [`Self::matches`].
    pub current: usize,
}

impl FindState {
    /// Recomputes [`Self::matches`] for `query` over `text`.
    ///
    /// Keeps the current match pointing at the nearest match at or after
    /// `caret`, which is what makes reopening the bar resume where the caret
    /// is rather than at the top of the buffer.
    pub fn search(&mut self, text: &str, query: &str, caret: usize) {
        self.query = query.to_owned();
        self.matches = find_all(text, query, self.case_sensitive);
        self.current = self
            .matches
            .iter()
            .position(|found| found.start >= caret)
            .unwrap_or(0);
    }

    /// The current match, if there is one.
    pub fn current(&self) -> Option<Range<usize>> {
        self.matches.get(self.current).cloned()
    }

    /// Steps to the next match, wrapping. Answers with it.
    ///
    /// Not `next`: this is not an iterator, and it wraps.
    pub fn advance(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = (self.current + 1) % self.matches.len();
        self.current()
    }

    /// Steps to the previous match, wrapping. Answers with it.
    pub fn retreat(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = if self.current == 0 {
            self.matches.len() - 1
        } else {
            self.current - 1
        };
        self.current()
    }

    /// Corrects the matches after `range` was replaced by `new_len` bytes.
    ///
    /// The match that was replaced is dropped and every one after it shifts;
    /// nothing is re-scanned, so a replacement that *creates* a new match does
    /// not find it until the next search. That is the behaviour every editor
    /// has, and the alternative — rescanning the whole file per replacement —
    /// is not one.
    pub fn shift_after_replace(&mut self, range: &Range<usize>, new_len: usize) {
        let old_len = range.end - range.start;
        self.matches
            .retain(|found| found.end <= range.start || found.start >= range.end);
        for found in &mut self.matches {
            if found.start >= range.end {
                found.start = found.start + new_len - old_len;
                found.end = found.end + new_len - old_len;
            }
        }
        // The replaced match is gone, so the one that took its index is the
        // next one; clamp for the case where it was the last.
        if self.current >= self.matches.len() {
            self.current = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_case_sensitive_when_asked() {
        let text = "Error error ERROR";
        assert_eq!(find_all(text, "error", true), vec![6..11]);
        assert_eq!(find_all(text, "error", false), vec![0..5, 6..11, 12..17]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(find_all("aaaa", "aa", true), vec![0..2, 2..4]);
        assert_eq!(find_all("aaaa", "aa", false), vec![0..2, 2..4]);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(find_all("error", "", false).is_empty());
        assert!(find_all("error", "", true).is_empty());
    }

    #[test]
    fn matching_lands_on_character_boundaries() {
        let text = "오류 ERROR 오류";
        let found = find_all(text, "오류", false);
        assert_eq!(found.len(), 2);
        for range in found {
            assert_eq!(&text[range], "오류");
        }
    }

    #[test]
    fn a_case_insensitive_query_finds_a_non_ascii_match() {
        let text = "Ölçek ölçek";
        let found = find_all(text, "ÖLÇEK", false);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn replacing_one_shifts_the_matches_after_it() {
        let text = "aa bb aa bb aa";
        let mut state = FindState::default();
        state.search(text, "aa", 0);
        assert_eq!(state.matches, vec![0..2, 6..8, 12..14]);

        // Replace the first with something longer; the two after it move.
        state.shift_after_replace(&(0..2), 5);
        assert_eq!(state.matches, vec![9..11, 15..17]);
        assert_eq!(state.current, 0);
    }

    #[test]
    fn the_current_match_starts_at_the_caret() {
        let text = "aa bb aa bb aa";
        let mut state = FindState::default();
        state.search(text, "aa", 7);
        assert_eq!(state.current(), Some(12..14));
        assert_eq!(state.advance(), Some(0..2), "and wraps");
        assert_eq!(state.retreat(), Some(12..14));
    }
}
