//! Keyboard.
//!
//! The panel is driven the way a file manager's list is: the arrows move the
//! selection, `Shift` stretches it, `Enter` opens what is selected, `Backspace`
//! climbs a level, `Delete` asks before removing, `F2` renames, the select-all
//! chord takes the whole listing, and typing a name jumps to it.
//!
//! The keys are read off the panel's own `on_key_down` rather than bound as
//! actions, and only while the panel itself holds the keyboard — not one of the
//! fields of its questions, which bubble their keys up through it. Bindings
//! would have to beat the workspace's to the same keys, and a list this small
//! has no chord worth rebinding.

use std::time::{Duration, Instant};

use gpui::KeyDownEvent;

use super::*;

/// How long typed characters keep adding to one name rather than starting a
/// new one, the way every file manager's type-ahead behaves.
const TYPE_AHEAD: Duration = Duration::from_millis(900);

/// The row a step of `delta` from `current` lands on in a list of `len` rows,
/// clamped at both ends; with nothing selected the first step down lands on
/// the first row and the first step up on the last.
pub(super) fn stepped_row(current: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len as isize - 1;
    Some(match current {
        None if delta < 0 => last as usize,
        None => 0,
        Some(index) => (index as isize + delta).clamp(0, last) as usize,
    })
}

/// The first entry from `from` on — wrapping round — whose name starts with
/// `prefix`, ignoring case.
pub(super) fn find_prefix(entries: &[FileEntry], prefix: &str, from: usize) -> Option<usize> {
    let prefix = prefix.to_lowercase();
    let len = entries.len();
    (0..len)
        .map(|offset| (from + offset) % len.max(1))
        .find(|index| entries[*index].name.to_lowercase().starts_with(&prefix))
}

impl FilePanel {
    /// The panel's keys; see the module documentation.
    pub(super) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus_handle.is_focused(window) {
            return;
        }
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let plain = !modifiers.control && !modifiers.alt && !modifiers.platform;
        let handled = match keystroke.key.as_str() {
            // A question under the listing is answered from the keyboard too.
            "enter" if plain && self.prompt_is_delete() => {
                self.delete(cx);
                true
            }
            "escape" if self.prompt_is_open() => {
                self.cancel_prompt(cx);
                true
            }
            "up" if plain => self.step(-1, modifiers.shift, cx),
            "down" if plain => self.step(1, modifiers.shift, cx),
            "pageup" if plain => self.step(-PAGE_ROWS, modifiers.shift, cx),
            "pagedown" if plain => self.step(PAGE_ROWS, modifiers.shift, cx),
            "home" if plain => self.step(isize::MIN / 2, modifiers.shift, cx),
            "end" if plain => self.step(isize::MAX / 2, modifiers.shift, cx),
            "enter" if plain && !modifiers.shift => {
                self.open_selected(cx);
                true
            }
            "backspace" if plain && !modifiers.shift => {
                self.open_parent(cx);
                true
            }
            "delete" if plain && !modifiers.shift => {
                self.confirm_delete(cx);
                true
            }
            "f2" if plain && !modifiers.shift => {
                self.begin_rename(cx);
                true
            }
            "a" if modifiers.secondary() && !modifiers.shift && !modifiers.alt => {
                self.select_all(cx);
                true
            }
            _ => match keystroke.key_char.as_deref() {
                Some(text) if plain && !text.trim().is_empty() => self.type_ahead(text, cx),
                _ => false,
            },
        };
        if handled {
            cx.stop_propagation();
        }
    }

    /// `Escape` while the panel has the keyboard and a question is standing:
    /// takes the question back, and says whether there was one.
    pub fn cancel_focused_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.focus_handle.contains_focused(window, cx) || !self.prompt_is_open() {
            return false;
        }
        self.cancel_prompt(cx);
        // The field the question was typed into goes with it; the keyboard
        // stays in the panel rather than on nothing.
        window.focus(&self.focus_handle, cx);
        true
    }

    /// Whether a question is standing under the listing.
    fn prompt_is_open(&self) -> bool {
        self.showing().is_some_and(|state| state.prompt.is_some())
    }

    /// Whether the question standing is the delete confirmation, which `Enter`
    /// answers — the others have a field of their own that does.
    fn prompt_is_delete(&self) -> bool {
        self.showing()
            .is_some_and(|state| matches!(state.prompt, Some(Prompt::Delete(_))))
    }

    /// Moves the selection `delta` rows, or stretches it from the anchor with
    /// `extend`, and scrolls the row it lands on into view.
    fn step(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.active_state() else {
            return false;
        };
        let position = |name: &String| state.entries.iter().position(|entry| &entry.name == name);
        // The row the keyboard is on: where the last step or click left it.
        let current = state
            .cursor
            .as_ref()
            .and_then(position)
            .or_else(|| state.anchor.as_ref().and_then(position));
        let Some(target) = stepped_row(current, state.entries.len(), delta) else {
            return false;
        };
        self.put_cursor(target, extend, cx);
        true
    }

    /// Puts the keyboard on row `target`: the selection becomes that row, or
    /// — with `extend` — everything from the anchor to it, and the row is
    /// scrolled into view.
    fn put_cursor(&mut self, target: usize, extend: bool, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        let Some(name) = state.entries.get(target).map(|entry| entry.name.clone()) else {
            return;
        };
        if extend && state.anchor.is_some() {
            let shift = Modifiers {
                shift: true,
                ..Modifiers::default()
            };
            self.select(&name, shift, cx);
        } else {
            state.select_only(&name);
        }
        let Some(state) = self.active_state() else {
            return;
        };
        state.cursor = Some(name);
        // The `..` row, when there is one, is the list's first child.
        let parent_row = state
            .path
            .as_deref()
            .is_some_and(|path| !is_root(path) && !path.is_empty());
        state
            .scroll
            .scroll_to_item(target + usize::from(parent_row));
        cx.notify();
    }

    /// `Enter`: opens the one selected entry — a directory is listed, a file
    /// goes to the editor, the way a double-click does.
    pub(super) fn open_selected(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self
            .showing()
            .and_then(SessionState::only_selected)
            .map(|entry| entry.name.clone())
        else {
            return;
        };
        self.activate(&name, cx);
    }

    /// Typing a name: selects the first entry starting with what has been
    /// typed in the last moment, searching on from the selected one.
    fn type_ahead(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        if self
            .typed_at
            .is_none_or(|at| now.duration_since(at) > TYPE_AHEAD)
        {
            self.typed.clear();
        }
        self.typed.push_str(text);
        self.typed_at = Some(now);
        let typed = self.typed.clone();
        let Some(state) = self.active_state() else {
            return false;
        };
        let current = state
            .cursor
            .as_ref()
            .and_then(|name| state.entries.iter().position(|entry| &entry.name == name));
        // A fresh name starts after the keyboard's row, so pressing the same
        // letter again walks through every entry that starts with it.
        let from = match current {
            Some(index) if typed.chars().count() == 1 => index + 1,
            Some(index) => index,
            None => 0,
        };
        if let Some(found) = find_prefix(&state.entries, &typed, from) {
            self.put_cursor(found, false, cx);
        }
        true
    }
}

/// How many rows a page key moves: roughly a screenful of the panel at its
/// usual height, which the panel cannot measure before it is painted.
const PAGE_ROWS: isize = 12;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            is_dir: false,
            is_symlink: false,
            size: 0,
        }
    }

    #[test]
    fn a_step_is_clamped_and_starts_from_the_nearer_end() {
        assert_eq!(stepped_row(None, 3, 1), Some(0));
        assert_eq!(stepped_row(None, 3, -1), Some(2));
        assert_eq!(stepped_row(Some(1), 3, 1), Some(2));
        assert_eq!(stepped_row(Some(2), 3, 1), Some(2));
        assert_eq!(stepped_row(Some(1), 3, isize::MIN / 2), Some(0));
        assert_eq!(stepped_row(Some(0), 0, 1), None);
    }

    #[test]
    fn typing_finds_a_name_ignoring_case_and_wraps() {
        let entries = [
            entry("alpha"),
            entry("Beta"),
            entry("bravo"),
            entry("charlie"),
        ];
        assert_eq!(find_prefix(&entries, "b", 0), Some(1));
        assert_eq!(find_prefix(&entries, "b", 2), Some(2));
        assert_eq!(find_prefix(&entries, "b", 3), Some(1), "it wraps round");
        assert_eq!(find_prefix(&entries, "BR", 0), Some(2));
        assert_eq!(find_prefix(&entries, "z", 0), None);
    }
}
