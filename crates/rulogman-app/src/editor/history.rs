//! Undo and redo, and the grouping rule that decides how much of it one press
//! takes back.
//!
//! An [`Edit`] is a byte range and the two texts — what was there and what
//! replaced it — which is enough to apply it in either direction. A
//! [`Transaction`] is a run of them plus the selection before and after, so
//! that undoing puts the caret back where the typing started rather than where
//! it ended.
//!
//! # Grouping
//!
//! One press of `ctrl-z` should take back a word, not a letter, and should not
//! take back a word *and* the paste before it. The rule here, which is the one
//! every editor converges on:
//!
//! * A single-character insertion at the caret extends the transaction the
//!   previous one opened, as long as the previous one was also typing and ended
//!   exactly where this one starts.
//! * A backspace extends a run of backspaces the same way, backwards.
//! * A newline ends the group after itself: undo stops at line boundaries,
//!   which is what makes it usable in a file that is read a line at a time.
//! * Everything else — a paste, a caret move, an indent, a comment toggle, a
//!   find-and-replace, the commit of an IME composition — is its own
//!   transaction, and [`History::break_group`] is what the editor calls to say
//!   so.
//!
//! # Compositions
//!
//! An IME composition is not recorded while it is running. Typing `ㅎ`, `하`,
//! `한` is three replacements of the same range, and three transactions for one
//! syllable would be wrong in both directions — the intermediate states are not
//! text anyone typed. The editor opens the composition with
//! [`History::begin_composition`], which remembers where it started and what it
//! displaced, and closes it with [`History::end_composition`], which records
//! the one edit that is the difference. See [`crate::editor::view`] for the
//! call sites.

use std::ops::Range;

/// Where the caret and selection were.
///
/// Carried by a transaction so undo restores it; `reversed` is which end of a
/// non-empty selection the caret is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionState {
    /// The selected byte range, `start <= end`.
    pub range: Range<usize>,
    /// Whether the caret sits at `range.start` rather than at `range.end`.
    pub reversed: bool,
}

impl SelectionState {
    /// A collapsed caret at `offset`.
    pub const fn at(offset: usize) -> Self {
        Self {
            range: offset..offset,
            reversed: false,
        }
    }
}

/// One replacement, in a form that can be applied either way round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Byte offset the replacement starts at.
    pub start: usize,
    /// What was there before.
    pub removed: String,
    /// What is there now.
    pub inserted: String,
}

impl Edit {
    /// The range this edit occupies before it is applied.
    pub fn old_range(&self) -> Range<usize> {
        self.start..self.start + self.removed.len()
    }

    /// The range this edit occupies after it is applied.
    pub fn new_range(&self) -> Range<usize> {
        self.start..self.start + self.inserted.len()
    }
}

/// What kind of change an edit was, for the grouping rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// A character the user typed, which may extend a run of them.
    Typing,
    /// A backspace, which may extend a run of them.
    DeleteBack,
    /// A forward delete, which may extend a run of them.
    DeleteForward,
    /// Anything else. Always its own transaction.
    Other,
}

/// One undo step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// The edits, in the order they were applied.
    pub edits: Vec<Edit>,
    /// The selection before the first of them.
    pub before: SelectionState,
    /// The selection after the last of them.
    pub after: SelectionState,
    /// What the last edit in it was, for grouping.
    kind: EditKind,
    /// Whether another edit is still allowed to join this transaction.
    open: bool,
}

/// The undo and redo stacks.
#[derive(Debug, Default)]
pub struct History {
    /// Transactions that can be undone, oldest first.
    undo: Vec<Transaction>,
    /// Transactions that can be redone, oldest first.
    redo: Vec<Transaction>,
    /// Where a running IME composition started, and what it displaced.
    composition: Option<(usize, String, SelectionState)>,
}

/// How large a group is allowed to grow before it is closed anyway.
///
/// A guard rather than a rule: without it, holding a key down would build one
/// transaction the length of the run, and taking a thousand characters back in
/// one press is not what the press means.
const MAX_GROUP: usize = 128;

impl History {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there is anything to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether there is anything to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Forgets everything, for [`crate::editor::EditorView::set_text`].
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.composition = None;
    }

    /// Closes the open group, so the next edit starts a transaction of its own.
    ///
    /// Called on every caret move and on every command that is not typing.
    pub fn break_group(&mut self) {
        if let Some(last) = self.undo.last_mut() {
            last.open = false;
        }
    }

    /// Records `edit`, extending the open transaction when the rule allows it.
    pub fn push(
        &mut self,
        edit: Edit,
        kind: EditKind,
        before: SelectionState,
        after: SelectionState,
    ) {
        // Any new edit invalidates the redo stack: the future it led to is no
        // longer reachable.
        self.redo.clear();

        if self.extend(&edit, kind, &after) {
            return;
        }
        // A line break closes its own group, so that undo stops at line ends
        // rather than swallowing the line above.
        let crosses_a_line = edit.inserted.contains('\n') || edit.removed.contains('\n');
        let open = kind != EditKind::Other && !crosses_a_line;
        self.undo.push(Transaction {
            edits: vec![edit],
            before,
            after,
            kind,
            open,
        });
    }

    /// Records several edits as one undo step, never folded into a group.
    ///
    /// The order matters and it is the order they were applied in: undo
    /// reverses them back to front and redo replays them front to back, which
    /// is what makes each one's offsets valid at the moment it is touched
    /// however the edits are spread over the buffer.
    pub fn push_transaction(
        &mut self,
        edits: Vec<Edit>,
        before: SelectionState,
        after: SelectionState,
    ) {
        if edits.is_empty() {
            return;
        }
        self.redo.clear();
        self.undo.push(Transaction {
            edits,
            before,
            after,
            kind: EditKind::Other,
            open: false,
        });
    }

    /// Starts an IME composition at `start`, displacing `removed`.
    ///
    /// Nothing is recorded until [`Self::end_composition`]; see the module
    /// documentation for why.
    pub fn begin_composition(&mut self, start: usize, removed: String, before: SelectionState) {
        self.break_group();
        self.composition = Some((start, removed, before));
    }

    /// Whether a composition is being recorded.
    pub fn in_composition(&self) -> bool {
        self.composition.is_some()
    }

    /// Ends a composition, recording it as one edit that inserted `inserted`.
    ///
    /// A composition that inserted exactly what it displaced records nothing,
    /// which is what makes a cancelled composition invisible to undo.
    pub fn end_composition(&mut self, inserted: String, after: SelectionState) {
        let Some((start, removed, before)) = self.composition.take() else {
            return;
        };
        if removed == inserted {
            return;
        }
        self.redo.clear();
        self.undo.push(Transaction {
            edits: vec![Edit {
                start,
                removed,
                inserted,
            }],
            before,
            after,
            kind: EditKind::Other,
            open: false,
        });
    }

    /// Abandons a running composition without recording anything.
    pub fn cancel_composition(&mut self) {
        self.composition = None;
    }

    /// Takes the newest transaction off the undo stack, ready to be reversed.
    ///
    /// The caller applies the reversal and calls [`Self::finish_undo`] with the
    /// same transaction.
    pub fn pop_undo(&mut self) -> Option<Transaction> {
        self.composition = None;
        self.undo.pop()
    }

    /// Puts a reversed transaction onto the redo stack.
    pub fn finish_undo(&mut self, transaction: Transaction) {
        self.redo.push(transaction);
    }

    /// Takes the newest transaction off the redo stack, ready to be replayed.
    pub fn pop_redo(&mut self) -> Option<Transaction> {
        self.composition = None;
        self.redo.pop()
    }

    /// Puts a replayed transaction back onto the undo stack, closed.
    pub fn finish_redo(&mut self, mut transaction: Transaction) {
        transaction.open = false;
        self.undo.push(transaction);
    }

    /// Tries to fold `edit` into the open transaction. Answers whether it did.
    fn extend(&mut self, edit: &Edit, kind: EditKind, after: &SelectionState) -> bool {
        if kind == EditKind::Other {
            return false;
        }
        let Some(last) = self.undo.last_mut() else {
            return false;
        };
        if !last.open || last.kind != kind {
            return false;
        }
        let Some(previous) = last.edits.last_mut() else {
            return false;
        };
        if previous.inserted.len() + previous.removed.len() >= MAX_GROUP {
            return false;
        }

        match kind {
            EditKind::Typing => {
                // Only a straight run: the new text has to start exactly where
                // the last one ended, and neither may be a line break.
                if previous.start + previous.inserted.len() != edit.start
                    || !previous.removed.is_empty()
                    || !edit.removed.is_empty()
                    || edit.inserted.contains('\n')
                    || previous.inserted.contains('\n')
                {
                    return false;
                }
                previous.inserted.push_str(&edit.inserted);
            }
            EditKind::DeleteBack => {
                // Backspace walks left, so each edit ends where the previous
                // one began.
                if edit.start + edit.removed.len() != previous.start
                    || !edit.inserted.is_empty()
                    || !previous.inserted.is_empty()
                {
                    return false;
                }
                previous.start = edit.start;
                let mut removed = edit.removed.clone();
                removed.push_str(&previous.removed);
                previous.removed = removed;
            }
            EditKind::DeleteForward => {
                // Delete stays put and eats forwards.
                if edit.start != previous.start
                    || !edit.inserted.is_empty()
                    || !previous.inserted.is_empty()
                {
                    return false;
                }
                previous.removed.push_str(&edit.removed);
            }
            EditKind::Other => unreachable!("returned above"),
        }
        last.after = after.clone();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An insertion of `text` at `at`.
    fn typed(at: usize, text: &str) -> Edit {
        Edit {
            start: at,
            removed: String::new(),
            inserted: text.to_owned(),
        }
    }

    /// A removal of `text` at `at`.
    fn removed(at: usize, text: &str) -> Edit {
        Edit {
            start: at,
            removed: text.to_owned(),
            inserted: String::new(),
        }
    }

    /// Types `text` one character at a time, as the input handler would.
    fn type_run(history: &mut History, from: usize, text: &str) {
        let mut at = from;
        for ch in text.chars() {
            let next = at + ch.len_utf8();
            history.push(
                typed(at, &ch.to_string()),
                EditKind::Typing,
                SelectionState::at(at),
                SelectionState::at(next),
            );
            at = next;
        }
    }

    #[test]
    fn a_run_of_typing_is_one_undo_step() {
        let mut history = History::new();
        type_run(&mut history, 0, "config");
        let transaction = history.pop_undo().expect("one group");
        assert_eq!(transaction.edits, vec![typed(0, "config")]);
        assert_eq!(transaction.before, SelectionState::at(0));
        assert_eq!(transaction.after, SelectionState::at(6));
        assert!(!history.can_undo());
    }

    #[test]
    fn a_caret_move_ends_the_group() {
        let mut history = History::new();
        type_run(&mut history, 0, "con");
        history.break_group();
        type_run(&mut history, 3, "fig");
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn typing_somewhere_else_ends_the_group_on_its_own() {
        let mut history = History::new();
        type_run(&mut history, 0, "abc");
        type_run(&mut history, 40, "xyz");
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn a_newline_closes_its_group() {
        let mut history = History::new();
        type_run(&mut history, 0, "abc");
        history.push(
            typed(3, "\n"),
            EditKind::Typing,
            SelectionState::at(3),
            SelectionState::at(4),
        );
        type_run(&mut history, 4, "def");
        assert_eq!(history.undo.len(), 3, "before, the newline, and after");
    }

    #[test]
    fn a_paste_is_never_folded_in() {
        let mut history = History::new();
        type_run(&mut history, 0, "abc");
        history.push(
            typed(3, "pasted"),
            EditKind::Other,
            SelectionState::at(3),
            SelectionState::at(9),
        );
        type_run(&mut history, 9, "def");
        assert_eq!(history.undo.len(), 3);
    }

    #[test]
    fn a_run_of_backspaces_is_one_undo_step() {
        let mut history = History::new();
        for (at, ch) in [(5, "g"), (4, "i"), (3, "f")] {
            history.push(
                removed(at, ch),
                EditKind::DeleteBack,
                SelectionState::at(at + 1),
                SelectionState::at(at),
            );
        }
        let transaction = history.pop_undo().expect("one group");
        assert_eq!(transaction.edits, vec![removed(3, "fig")]);
    }

    #[test]
    fn deleting_forwards_groups_the_other_way_round() {
        let mut history = History::new();
        for ch in ["c", "o", "n"] {
            history.push(
                removed(2, ch),
                EditKind::DeleteForward,
                SelectionState::at(2),
                SelectionState::at(2),
            );
        }
        let transaction = history.pop_undo().expect("one group");
        assert_eq!(transaction.edits, vec![removed(2, "con")]);
    }

    #[test]
    fn a_long_run_is_broken_up() {
        let mut history = History::new();
        type_run(&mut history, 0, &"x".repeat(MAX_GROUP * 2 + 1));
        assert!(history.undo.len() > 1);
    }

    #[test]
    fn a_composition_records_one_edit_for_the_whole_syllable() {
        let mut history = History::new();
        history.begin_composition(0, String::new(), SelectionState::at(0));
        assert!(history.in_composition());
        history.end_composition("한".to_owned(), SelectionState::at(3));

        let transaction = history.pop_undo().expect("one group");
        assert_eq!(transaction.edits, vec![typed(0, "한")]);
    }

    #[test]
    fn a_composition_that_changed_nothing_records_nothing() {
        let mut history = History::new();
        history.begin_composition(0, "x".to_owned(), SelectionState::at(0));
        history.end_composition("x".to_owned(), SelectionState::at(1));
        assert!(!history.can_undo());
    }

    #[test]
    fn a_new_edit_drops_the_redo_stack() {
        let mut history = History::new();
        type_run(&mut history, 0, "abc");
        let transaction = history.pop_undo().expect("one group");
        history.finish_undo(transaction);
        assert!(history.can_redo());

        type_run(&mut history, 0, "z");
        assert!(!history.can_redo());
    }
}
