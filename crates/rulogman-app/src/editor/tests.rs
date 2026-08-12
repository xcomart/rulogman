//! Headless tests for the editor as a whole: the entity, its input handler and
//! its element, driven through a real gpui window.
//!
//! The unit tests of the pieces live beside them — the rope in
//! [`crate::editor::buffer`], the grouping rule in [`crate::editor::history`],
//! the matcher in [`crate::editor::find`]. What is here is everything that only
//! exists once there is a focused element and a platform input handler attached
//! to it, which is the whole of the IME story and the whole of the
//! virtualisation story.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use gpui::{
    Context, Entity, EntityInputHandler, Focusable, Hsla, IntoElement, Pixels, Render,
    TestAppContext, VisualTestContext, Window, div, font, prelude::*, px,
};
use rulogman_term::{Rgb, TerminalTheme};

use crate::editor::syntax::{Language, TokenKind};
use crate::editor::view::{EditorEvent, EditorView};
use crate::editor::{
    ANSI_BLUE, ANSI_BRIGHT_BLACK, ANSI_CYAN, ANSI_GREEN, ANSI_MAGENTA, ANSI_YELLOW, EditorPalette,
    MIN_COMMENT_CONTRAST, MIN_CONTRAST, contrast, legible, mix, palette_for,
};
use crate::terminal_view::to_hsla;
use crate::ui::scrollbar::ScrollbarAxis;

/// A view that does nothing but hold the editor, as a pane would.
struct Harness {
    editor: Entity<EditorView>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

/// Forces a redraw and waits for it.
///
/// The test platform draws on the effect cycle, so a frame is a refresh plus a
/// turn of the executor; the tests that count shaping work need one to have
/// happened.
fn draw(cx: &mut VisualTestContext) {
    cx.refresh().expect("the window is open");
    cx.run_until_parked();
}

/// The editor under test and what it announced.
struct Handles {
    editor: Entity<EditorView>,
    events: Rc<RefCell<Vec<EditorEvent>>>,
}

impl Handles {
    /// Reads something off the editor.
    fn read<R>(&self, cx: &mut VisualTestContext, f: impl FnOnce(&EditorView) -> R) -> R {
        cx.update(|_, cx| f(self.editor.read(cx)))
    }

    /// Mutates the editor.
    fn update<R>(
        &self,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&mut EditorView, &mut Context<EditorView>) -> R,
    ) -> R {
        cx.update(|_, cx| self.editor.update(cx, f))
    }

    /// Mutates the editor with a window in hand, which the input handler needs.
    fn with_window<R>(
        &self,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&mut EditorView, &mut Window, &mut Context<EditorView>) -> R,
    ) -> R {
        cx.update(|window, cx| self.editor.update(cx, |editor, cx| f(editor, window, cx)))
    }

    /// The buffer's text.
    fn text(&self, cx: &mut VisualTestContext) -> String {
        self.read(cx, EditorView::text)
    }

    /// The caret's byte offset.
    fn caret(&self, cx: &mut VisualTestContext) -> usize {
        self.read(cx, EditorView::caret)
    }

    /// The events emitted so far, draining them.
    fn drain_events(&self) -> Vec<EditorEvent> {
        self.events.borrow_mut().drain(..).collect()
    }
}

/// Opens a window holding an editor over `text`, focused.
fn open(text: &str, cx: &mut TestAppContext) -> (Handles, VisualTestContext) {
    cx.update(crate::ui::init);
    cx.update(crate::editor::init);

    let events: Rc<RefCell<Vec<EditorEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let text = text.to_owned();
    let window = cx.add_window({
        let events = events.clone();
        move |_, cx| {
            let editor = cx.new(|cx| {
                let mut editor = EditorView::new(cx);
                editor.set_text(&text, cx);
                editor.mark_clean(cx);
                editor
            });
            cx.subscribe(
                &editor,
                move |_: &mut Harness, _, event: &EditorEvent, _| {
                    events.borrow_mut().push(event.clone());
                },
            )
            .detach();
            Harness { editor }
        }
    });
    let editor = window
        .update(cx, |harness, _, _| harness.editor.clone())
        .expect("the window is open");

    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    cx.update(|window, cx| {
        let handle = editor.read(cx).focus_handle(cx);
        handle.focus(window);
    });
    cx.run_until_parked();

    (Handles { editor, events }, cx)
}

// --- editing -----------------------------------------------------------------

#[gpui::test]
fn typing_inserts_at_the_caret(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("", cx);
    cx.simulate_input("listen 22");
    assert_eq!(editor.text(&mut cx), "listen 22");
    assert_eq!(editor.caret(&mut cx), 9);
    assert!(editor.read(&mut cx, |editor| editor.is_dirty()));
}

#[gpui::test]
fn enter_carries_the_indent_of_the_line_above(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("    listen 22", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(13, cx));
    cx.simulate_keystrokes("enter");
    cx.simulate_input("user b");
    assert_eq!(editor.text(&mut cx), "    listen 22\n    user b");
}

#[gpui::test]
fn backspace_and_delete_step_by_grapheme(cx: &mut TestAppContext) {
    // A Hangul syllable is three bytes, a joined emoji eleven; one press takes
    // one of each, not one byte of either.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let (editor, mut cx) = open(&format!("한{family}글"), cx);
    editor.update(&mut cx, |editor, cx| {
        let end = editor.text().len();
        editor.move_to(end, cx);
    });

    cx.simulate_keystrokes("backspace");
    assert_eq!(editor.text(&mut cx), format!("한{family}"));
    cx.simulate_keystrokes("backspace");
    assert_eq!(editor.text(&mut cx), "한");

    editor.update(&mut cx, |editor, cx| editor.move_to(0, cx));
    cx.simulate_keystrokes("delete");
    assert_eq!(editor.text(&mut cx), "");
}

#[gpui::test]
fn the_arrows_move_by_grapheme_not_by_byte(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("한글", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(0, cx));
    cx.simulate_keystrokes("right");
    assert_eq!(editor.caret(&mut cx), 3);
    cx.simulate_keystrokes("right");
    assert_eq!(editor.caret(&mut cx), 6);
    cx.simulate_keystrokes("left");
    assert_eq!(editor.caret(&mut cx), 3);
}

#[gpui::test]
fn up_and_down_keep_the_goal_column(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("aaaaaaaa\nbb\ncccccccc", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(6, cx));

    cx.simulate_keystrokes("down");
    assert_eq!(editor.caret(&mut cx), 11, "clamped to the short line");
    cx.simulate_keystrokes("down");
    assert_eq!(editor.caret(&mut cx), 18, "and back out to column six");
    cx.simulate_keystrokes("up up");
    assert_eq!(editor.caret(&mut cx), 6);
}

#[gpui::test]
fn shift_arrows_extend_the_selection(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen 22", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(0, cx));
    cx.simulate_keystrokes("shift-right shift-right shift-right");
    assert_eq!(editor.read(&mut cx, EditorView::selection), 0..3);

    // Typing over a selection replaces it.
    cx.simulate_input("X");
    assert_eq!(editor.text(&mut cx), "Xten 22");
}

#[gpui::test]
fn select_all_then_typing_replaces_the_buffer(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("host a\nhost b", cx);
    cx.simulate_keystrokes("cmd-a ctrl-a");
    cx.simulate_input("x");
    assert_eq!(editor.text(&mut cx), "x");
}

#[gpui::test]
fn cut_copy_and_paste_go_through_the_clipboard(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("host a\nhost b", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..6, cx));
    cx.simulate_keystrokes("cmd-x ctrl-x");
    assert_eq!(editor.text(&mut cx), "\nhost b");

    editor.update(&mut cx, |editor, cx| {
        let end = editor.text().len();
        editor.move_to(end, cx);
    });
    cx.simulate_keystrokes("cmd-v ctrl-v");
    assert_eq!(editor.text(&mut cx), "\nhost bhost a");
}

#[gpui::test]
fn a_paste_keeps_its_line_breaks(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("a\nb", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..3, cx));
    cx.simulate_keystrokes("cmd-c ctrl-c");
    editor.update(&mut cx, |editor, cx| editor.move_to(3, cx));
    cx.simulate_keystrokes("cmd-v ctrl-v");
    assert_eq!(editor.text(&mut cx), "a\nba\nb");
}

#[gpui::test]
fn tab_indents_a_block_and_shift_tab_takes_it_back(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("host a\nport 22\nuser b", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..20, cx));

    cx.simulate_keystrokes("tab");
    assert_eq!(editor.text(&mut cx), "    host a\n    port 22\n    user b");
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(editor.text(&mut cx), "host a\nport 22\nuser b");
}

#[gpui::test]
fn tab_on_a_caret_is_an_indent_not_a_command(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("ab", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(1, cx));
    cx.simulate_keystrokes("tab");
    assert_eq!(editor.text(&mut cx), "a    b");
}

#[gpui::test]
fn the_comment_toggle_takes_the_whole_selection(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("host a\n  port 22\n\nuser b", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..15, cx));

    cx.simulate_keystrokes("cmd-/ ctrl-/");
    assert_eq!(editor.text(&mut cx), "# host a\n#   port 22\n\nuser b");

    editor.update(&mut cx, |editor, cx| editor.select_range(0..20, cx));
    cx.simulate_keystrokes("cmd-/ ctrl-/");
    assert_eq!(editor.text(&mut cx), "host a\n  port 22\n\nuser b");
}

#[gpui::test]
fn the_comment_toggle_follows_the_language(cx: &mut TestAppContext) {
    // A plain buffer keeps the `#` it always had -- a file the detector did not
    // place is a config file far more often than it is prose.
    let (editor, mut cx) = open("{\"a\": 1}", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..8, cx));
    cx.simulate_keystrokes("cmd-/ ctrl-/");
    assert_eq!(editor.text(&mut cx), "# {\"a\": 1}");

    // JSON has no comment syntax, so the press writes nothing rather than
    // writing something the file's own reader would reject.
    editor.update(&mut cx, |editor, cx| {
        editor.set_text("{\"a\": 1}", cx);
        editor.set_language(Language::Json, cx);
        editor.select_range(0..8, cx);
    });
    cx.simulate_keystrokes("cmd-/ ctrl-/");
    assert_eq!(editor.text(&mut cx), "{\"a\": 1}");
}

#[gpui::test]
fn a_read_only_editor_refuses_every_change(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen 22", cx);
    editor.update(&mut cx, |editor, cx| editor.set_read_only(true, cx));

    cx.simulate_input("x");
    cx.simulate_keystrokes("backspace enter tab");
    assert_eq!(editor.text(&mut cx), "listen 22");
    assert!(!editor.read(&mut cx, |editor| editor.is_dirty()));

    // Moving about still works, which is what makes it readable.
    cx.simulate_keystrokes("right right");
    assert_eq!(editor.caret(&mut cx), 2);
}

// --- where the caret is ------------------------------------------------------

#[gpui::test]
fn the_caret_reports_its_place_the_way_a_reader_counts(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("one\ntwo\nthree\n", cx);

    // The very start of the file is line one, column one — not the zero the
    // buffer counts in, which is the whole reason these accessors exist.
    assert_eq!(editor.read(&mut cx, EditorView::caret_position), (1, 1));
    // Four lines: a file ending in a newline has an empty last one, and the
    // caret can be put on it.
    assert_eq!(editor.read(&mut cx, EditorView::line_count), 4);

    // Onto the third line, two graphemes in.
    editor.update(&mut cx, |editor, cx| editor.move_to(10, cx));
    assert_eq!(editor.read(&mut cx, EditorView::caret_position), (3, 3));

    // And the end of the buffer, which is the empty line after the last break.
    editor.update(&mut cx, |editor, cx| {
        let end = editor.text().len();
        editor.move_to(end, cx);
    });
    assert_eq!(editor.read(&mut cx, EditorView::caret_position), (4, 1));
}

#[gpui::test]
fn the_column_counts_graphemes_and_not_bytes(cx: &mut TestAppContext) {
    // Three Hangul syllables of three bytes each, and a family emoji written as
    // three four-byte people joined by two zero-width joiners.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let (editor, mut cx) = open(&format!("한국어{family}x"), cx);
    editor.update(&mut cx, |editor, cx| {
        let end = editor.text().len();
        editor.move_to(end, cx);
    });

    // Twenty-eight bytes in, and five things a reader would count: three
    // syllables, one family, one `x`. A byte column would say 29.
    assert_eq!(editor.caret(&mut cx), 28);
    assert_eq!(editor.read(&mut cx, EditorView::caret_position), (1, 6));
}

#[gpui::test]
fn an_empty_buffer_is_one_line_with_the_caret_at_its_head(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("", cx);
    assert_eq!(editor.read(&mut cx, EditorView::line_count), 1);
    assert_eq!(editor.read(&mut cx, EditorView::caret_position), (1, 1));
}

#[gpui::test]
fn a_caret_move_is_announced_so_a_host_can_follow_it(cx: &mut TestAppContext) {
    // What the status bar's line number rides on: the editor draws the caret
    // itself, so nothing else would repaint if the move were kept quiet.
    let (editor, mut cx) = open("one\ntwo\n", cx);
    editor.drain_events();

    cx.simulate_keystrokes("down");
    assert_eq!(editor.read(&mut cx, EditorView::caret_position), (2, 1));
    assert!(
        editor
            .drain_events()
            .contains(&EditorEvent::SelectionChanged)
    );
}

// --- undo and redo -----------------------------------------------------------

#[gpui::test]
fn a_run_of_typing_undoes_in_one_press(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("", cx);
    cx.simulate_input("listen");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "");
    cx.simulate_keystrokes("cmd-shift-z ctrl-shift-z");
    assert_eq!(editor.text(&mut cx), "listen");
}

#[gpui::test]
fn a_caret_move_is_an_undo_boundary(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("", cx);
    cx.simulate_input("lis");
    cx.simulate_keystrokes("left right");
    cx.simulate_input("ten");
    assert_eq!(editor.text(&mut cx), "listen");

    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "lis");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "");
}

#[gpui::test]
fn a_paste_is_its_own_undo_step(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("pasted", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..6, cx));
    cx.simulate_keystrokes("cmd-c ctrl-c");
    editor.update(&mut cx, |editor, cx| editor.move_to(6, cx));

    cx.simulate_input("ab");
    cx.simulate_keystrokes("cmd-v ctrl-v");
    cx.simulate_input("cd");
    assert_eq!(editor.text(&mut cx), "pastedabpastedcd");

    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "pastedabpasted");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "pastedab");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "pasted");
}

#[gpui::test]
fn undoing_a_block_indent_takes_every_line_back(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("a\nb\nc", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..5, cx));
    cx.simulate_keystrokes("tab");
    assert_eq!(editor.text(&mut cx), "    a\n    b\n    c");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "a\nb\nc");
    cx.simulate_keystrokes("cmd-shift-z ctrl-shift-z");
    assert_eq!(editor.text(&mut cx), "    a\n    b\n    c");
}

#[gpui::test]
fn undo_puts_the_caret_back_where_the_typing_started(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen  on 22", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(7, cx));
    cx.simulate_input("x");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.caret(&mut cx), 7);
}

// --- the IME -----------------------------------------------------------------

/// Runs one composition step, the way a platform IME does.
fn compose(editor: &Handles, cx: &mut VisualTestContext, preview: &str) {
    editor.with_window(cx, |editor, window, cx| {
        editor.replace_and_mark_text_in_range(None, preview, None, window, cx);
    });
}

/// Commits a composition.
fn commit(editor: &Handles, cx: &mut VisualTestContext, text: &str) {
    editor.with_window(cx, |editor, window, cx| {
        editor.replace_text_in_range(None, text, window, cx);
    });
}

/// The marked range, in UTF-16 code units.
fn marked(editor: &Handles, cx: &mut VisualTestContext) -> Option<std::ops::Range<usize>> {
    editor.with_window(cx, |editor, window, cx| {
        editor.marked_text_range(window, cx)
    })
}

#[gpui::test]
fn a_hangul_syllable_composes_in_place(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("", cx);

    // ㅎ -> 하 -> 한: three previews of the same syllable, each replacing the
    // last, then the commit.
    compose(&editor, &mut cx, "ㅎ");
    assert_eq!(editor.text(&mut cx), "ㅎ");
    assert_eq!(marked(&editor, &mut cx), Some(0..1));
    assert_eq!(editor.caret(&mut cx), 3, "in bytes, past the syllable");

    compose(&editor, &mut cx, "하");
    assert_eq!(editor.text(&mut cx), "하", "the preview replaced itself");
    assert_eq!(marked(&editor, &mut cx), Some(0..1));

    compose(&editor, &mut cx, "한");
    assert_eq!(editor.text(&mut cx), "한");
    assert_eq!(marked(&editor, &mut cx), Some(0..1));

    commit(&editor, &mut cx, "한");
    assert_eq!(editor.text(&mut cx), "한");
    assert_eq!(marked(&editor, &mut cx), None);
    assert_eq!(editor.caret(&mut cx), 3);
}

#[gpui::test]
fn a_composition_after_text_starts_where_the_caret_is(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen ", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(7, cx));

    compose(&editor, &mut cx, "ㅎ");
    // Seven ASCII bytes are seven UTF-16 units; the syllable is one.
    assert_eq!(marked(&editor, &mut cx), Some(7..8));
    compose(&editor, &mut cx, "한");
    commit(&editor, &mut cx, "한");
    assert_eq!(editor.text(&mut cx), "listen 한");
    assert_eq!(editor.caret(&mut cx), 10);
}

#[gpui::test]
fn a_composition_past_the_basic_plane_counts_surrogates(cx: &mut TestAppContext) {
    // Four bytes, two UTF-16 units: an offset conversion that counted
    // characters would put the mark in the wrong place from here on.
    let (editor, mut cx) = open("\u{1f600}", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(4, cx));

    compose(&editor, &mut cx, "한");
    assert_eq!(marked(&editor, &mut cx), Some(2..3));
    commit(&editor, &mut cx, "한");
    assert_eq!(editor.text(&mut cx), "\u{1f600}한");
}

#[gpui::test]
fn a_whole_composition_is_one_undo_step(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("", cx);
    compose(&editor, &mut cx, "ㅎ");
    compose(&editor, &mut cx, "하");
    compose(&editor, &mut cx, "한");
    commit(&editor, &mut cx, "한");
    compose(&editor, &mut cx, "ㄱ");
    compose(&editor, &mut cx, "글");
    commit(&editor, &mut cx, "글");
    assert_eq!(editor.text(&mut cx), "한글");

    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "한", "one syllable, not one jamo");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "");
}

#[gpui::test]
fn a_composition_over_a_selection_replaces_it(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen xx", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(7..9, cx));

    compose(&editor, &mut cx, "ㅎ");
    assert_eq!(editor.text(&mut cx), "listen ㅎ");
    commit(&editor, &mut cx, "한");
    assert_eq!(editor.text(&mut cx), "listen 한");

    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "listen xx");
}

#[gpui::test]
fn an_empty_preview_cancels_the_composition(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("ab", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(2, cx));

    compose(&editor, &mut cx, "ㅎ");
    assert_eq!(editor.text(&mut cx), "abㅎ");
    compose(&editor, &mut cx, "");
    assert_eq!(editor.text(&mut cx), "ab");
    assert_eq!(marked(&editor, &mut cx), None);
    assert_eq!(editor.caret(&mut cx), 2);
}

#[gpui::test]
fn a_caret_inside_a_preview_is_a_caret_and_not_a_selection(cx: &mut TestAppContext) {
    // This is the case gpui's own example gets wrong: a preview replacing a
    // preview, with a caret position inside it, as Windows sends.
    let (editor, mut cx) = open("listen ", cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(7, cx));

    compose(&editor, &mut cx, "ㅎ");
    editor.with_window(&mut cx, |editor, window, cx| {
        editor.replace_and_mark_text_in_range(None, "한", Some(1..1), window, cx);
    });
    assert_eq!(
        editor.read(&mut cx, EditorView::selection),
        10..10,
        "past the syllable, not across it"
    );
}

#[gpui::test]
fn the_selection_is_reported_in_utf16(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("한글 log", cx);
    editor.update(&mut cx, |editor, cx| editor.select_range(0..7, cx));

    let reported = editor.with_window(&mut cx, |editor, window, cx| {
        editor.selected_text_range(false, window, cx)
    });
    // Two syllables and a space: three units, not seven bytes.
    assert_eq!(reported.expect("a selection").range, 0..3);
}

#[gpui::test]
fn text_for_range_answers_in_utf16_too(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("한글", cx);
    let mut actual = None;
    let text = editor.with_window(&mut cx, |editor, window, cx| {
        editor.text_for_range(1..2, &mut actual, window, cx)
    });
    assert_eq!(text.as_deref(), Some("글"));
    assert_eq!(actual, Some(1..2));
}

// --- find --------------------------------------------------------------------

#[gpui::test]
fn find_is_case_insensitive_until_it_is_not(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("Error error ERROR", cx);
    editor.update(&mut cx, |editor, cx| editor.set_find_query("error", cx));
    assert_eq!(editor.read(&mut cx, |editor| editor.matches().len()), 3);

    editor.update(&mut cx, |editor, cx| {
        editor.set_find_case_sensitive(true, cx);
    });
    assert_eq!(
        editor.read(&mut cx, |editor| editor.matches().to_vec()),
        vec![6..11]
    );
}

#[gpui::test]
fn f3_walks_the_matches_and_wraps(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("aa bb aa bb aa", cx);
    editor.update(&mut cx, |editor, cx| {
        editor.move_to(0, cx);
        editor.set_find_query("aa", cx);
    });

    cx.dispatch_action(crate::editor::view::FindNext);
    assert_eq!(editor.read(&mut cx, EditorView::selection), 6..8);
    cx.dispatch_action(crate::editor::view::FindNext);
    assert_eq!(editor.read(&mut cx, EditorView::selection), 12..14);
    cx.dispatch_action(crate::editor::view::FindNext);
    assert_eq!(editor.read(&mut cx, EditorView::selection), 0..2);
    cx.dispatch_action(crate::editor::view::FindPrev);
    assert_eq!(editor.read(&mut cx, EditorView::selection), 12..14);
}

#[gpui::test]
fn replacing_one_corrects_the_offsets_of_the_rest(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("aa bb aa bb aa", cx);
    editor.update(&mut cx, |editor, cx| {
        editor.move_to(0, cx);
        editor.set_find_query("aa", cx);
        editor.set_find_replacement("xxxxx", cx);
    });

    cx.dispatch_action(crate::editor::view::ReplaceNext);
    assert_eq!(editor.text(&mut cx), "xxxxx bb aa bb aa");
    // The two matches left have to have moved by three bytes each.
    assert_eq!(
        editor.read(&mut cx, |editor| editor.matches().to_vec()),
        vec![9..11, 15..17]
    );
}

#[gpui::test]
fn replace_all_rewrites_every_match_in_one_undo_step(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("aa bb aa bb aa", cx);
    editor.update(&mut cx, |editor, cx| {
        editor.set_find_query("aa", cx);
        editor.set_find_replacement("z", cx);
    });

    cx.dispatch_action(crate::editor::view::ReplaceAll);
    assert_eq!(editor.text(&mut cx), "z bb z bb z");
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "aa bb aa bb aa");
}

// --- virtualisation and cost -------------------------------------------------

/// A log of `lines` rows, one per line.
fn long_file(lines: usize) -> String {
    let mut text = String::with_capacity(lines * 24);
    for line in 0..lines {
        text.push_str("2026-08-08 12:00:00 row ");
        text.push_str(&line.to_string());
        text.push('\n');
    }
    text
}

#[gpui::test]
fn drawing_a_hundred_thousand_lines_shapes_only_the_visible_ones(cx: &mut TestAppContext) {
    let (editor, mut cx) = open(&long_file(100_000), cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(0, cx));
    draw(&mut cx);

    // The element shapes the rows the viewport covers and a row of overscan at
    // each edge, and nothing else. A frame that touched the buffer would shape
    // a hundred thousand.
    let shaped = editor.read(&mut cx, EditorView::shaped_lines);
    assert!(
        shaped > 0 && shaped < 200,
        "one frame shaped {shaped} lines of a hundred thousand"
    );
}

/// Regression: the bar was handed the scrolled *fraction* where its geometry
/// expects the scrolled *distance* in the same unit as the range — an upstream
/// slip the port inherited — so the thumb sat pinned to the top of the track
/// however far the surface had scrolled.
#[gpui::test]
fn the_thumb_follows_the_scroll(cx: &mut TestAppContext) {
    let (editor, mut cx) = open(&long_file(1_000), cx);
    editor.update(&mut cx, |editor, cx| editor.move_to(0, cx));
    draw(&mut cx);

    let thumb = |editor: &EditorView| {
        editor
            .scrollbar(ScrollbarAxis::Vertical)
            .and_then(|bar| bar.thumb())
    };
    let at_top = editor
        .read(&mut cx, thumb)
        .expect("a thousand lines outgrow the viewport");
    assert_eq!(at_top.start, px(0.), "an unscrolled thumb sat off the top");

    // Landing the caret on the last line scrolls the surface to its end, and
    // the thumb has to arrive there with it: at the far end of its track, not
    // a fraction-of-a-pixel below the top.
    editor.update(&mut cx, |editor, cx| {
        let end = editor.buffer().len();
        editor.move_to(end, cx);
    });
    draw(&mut cx);

    let at_bottom = editor
        .read(&mut cx, thumb)
        .expect("the surface still outgrows the viewport");
    assert!(
        at_bottom.start > at_top.start + px(10.),
        "the thumb barely moved for a scroll to the end: {:?}",
        at_bottom.start
    );
}

/// A shell script of `lines` commands, for the tests that need a language with
/// something to lex.
fn long_script(lines: usize) -> String {
    let mut text = String::with_capacity(lines * 16);
    for line in 0..lines {
        text.push_str("echo \"row ");
        text.push_str(&line.to_string());
        text.push_str("\"\n");
    }
    text
}

#[gpui::test]
fn one_keystroke_in_a_hundred_thousand_lines_relexes_a_constant_number(cx: &mut TestAppContext) {
    let (editor, mut cx) = open(&long_script(100_000), cx);
    editor.update(&mut cx, |editor, cx| {
        editor.set_language(Language::Shell, cx);
        let at = editor.buffer().line_start(50_000) + 7;
        editor.move_to(at, cx);
    });
    draw(&mut cx);

    // Two runs of different lengths with the same surroundings, so that the
    // frame the harness draws around them cancels out and what is left is the
    // marginal cost of one keystroke.
    let mut count = |presses: usize| {
        let before = editor.read(&mut cx, |editor| editor.highlighter().lex_calls());
        editor.with_window(&mut cx, |editor, window, cx| {
            for _ in 0..presses {
                editor.replace_text_in_range(None, "x", window, cx);
            }
        });
        editor.read(&mut cx, |editor| editor.highlighter().lex_calls()) - before
    };
    let short = count(100);
    let long = count(1_000);
    let per_keystroke = (long - short) / 900;

    assert!(
        per_keystroke <= 3,
        "one keystroke re-lexed {per_keystroke} lines of a hundred thousand"
    );
}

#[gpui::test]
fn typing_in_a_hundred_thousand_lines_stays_quick(cx: &mut TestAppContext) {
    let (editor, mut cx) = open(&long_file(100_000), cx);
    editor.update(&mut cx, |editor, cx| {
        let at = editor.buffer().line_start(50_000) + 7;
        editor.move_to(at, cx);
    });

    let started = std::time::Instant::now();
    editor.with_window(&mut cx, |editor, window, cx| {
        for _ in 0..500 {
            editor.replace_text_in_range(None, "x", window, cx);
        }
    });
    let each = started.elapsed() / 500;

    // Generous by two orders of magnitude against what it measures, because
    // this runs on whatever machine CI has; what it is really holding down is
    // that nothing on the edit path is linear in the buffer.
    assert!(
        each < std::time::Duration::from_millis(1),
        "a keystroke took {each:?}"
    );
}

#[gpui::test]
fn setting_the_text_clears_the_history_and_the_dirty_flag(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen 22", cx);
    cx.simulate_input("x");
    assert!(editor.read(&mut cx, |editor| editor.is_dirty()));

    editor.update(&mut cx, |editor, cx| editor.set_text("listen 23", cx));
    assert!(!editor.read(&mut cx, |editor| editor.is_dirty()));
    cx.simulate_keystrokes("cmd-z ctrl-z");
    assert_eq!(editor.text(&mut cx), "listen 23", "undo does not cross it");
}

// --- the mouse ---------------------------------------------------------------

#[gpui::test]
fn a_double_click_selects_a_word_and_a_triple_click_a_line(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Point, px};

    let (editor, mut cx) = open("listen count on 22\nsecond line", cx);
    draw(&mut cx);

    let line_height = editor.read(&mut cx, |editor| editor.layout.line_height);
    let gutter = editor.read(&mut cx, |editor| editor.layout.gutter);
    let position = Point {
        // Somewhere inside `count`, which starts at column seven.
        x: gutter + px(70.),
        y: line_height / 2.,
    };
    let click = |cx: &mut VisualTestContext, count: usize| {
        cx.simulate_event(MouseDownEvent {
            position,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: count,
        });
        cx.run_until_parked();
    };

    click(&mut cx, 2);
    let word = editor.read(&mut cx, EditorView::selection);
    assert!(!word.is_empty(), "a double click selects something");
    assert!(
        word.start >= 7 && word.end <= 18,
        "and it is on the first line: {word:?}"
    );

    click(&mut cx, 3);
    assert_eq!(
        editor.read(&mut cx, EditorView::selection),
        0..19,
        "a triple click takes the whole line"
    );
}

/// A right click asks the host for a menu and leaves the selection alone —
/// which is the whole point, since the menu is usually raised over a selection
/// in order to copy it.
#[gpui::test]
fn a_right_click_asks_for_a_menu_without_moving_the_caret(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Point, px};

    let (editor, mut cx) = open("listen count on 22\nsecond line", cx);
    draw(&mut cx);

    editor.update(&mut cx, |editor, cx| editor.select_range(7..12, cx));
    assert!(editor.read(&mut cx, EditorView::has_selection));
    editor.drain_events();

    // On the second line, well outside the selection.
    let line_height = editor.read(&mut cx, |editor| editor.layout.line_height);
    let gutter = editor.read(&mut cx, |editor| editor.layout.gutter);
    let position = Point {
        x: gutter + px(30.),
        y: line_height * 1.5,
    };
    cx.simulate_event(MouseDownEvent {
        position,
        modifiers: Modifiers::none(),
        button: MouseButton::Right,
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position,
        modifiers: Modifiers::none(),
        button: MouseButton::Right,
        click_count: 1,
    });
    cx.run_until_parked();

    assert_eq!(
        editor.drain_events(),
        vec![EditorEvent::ContextMenu { position }],
        "the press was not reported in window coordinates"
    );
    assert_eq!(
        editor.read(&mut cx, EditorView::selection),
        7..12,
        "a right click moved the selection"
    );
    assert!(
        editor.with_window(&mut cx, |editor, window, _| editor.is_focused(window)),
        "a right click did not take the focus"
    );
}

// --- the font ----------------------------------------------------------------

/// The width line zero was last shaped at, and the row pitch it was drawn at.
fn shaped_geometry(editor: &Handles, cx: &mut VisualTestContext) -> (Pixels, Pixels) {
    editor.read(cx, |editor| {
        let width = editor
            .layout
            .lines
            .iter()
            .find_map(|(line, shaped)| (*line == 0).then_some(shaped.width))
            .expect("the first line was drawn");
        (width, editor.layout.line_height)
    })
}

/// The font is the host's to supply — see [`crate::editor_pane`], which takes it
/// from the session's terminal settings — so what has to hold is that pushing
/// one in actually reaches the measuring.
#[gpui::test]
fn the_injected_font_size_reaches_the_shaping_and_the_row_pitch(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen 22", cx);

    editor.update(&mut cx, |editor, cx| {
        editor.set_font(font("Consolas"), px(10.), cx);
    });
    draw(&mut cx);
    let (narrow, short) = shaped_geometry(&editor, &mut cx);

    editor.update(&mut cx, |editor, cx| {
        editor.set_font(font("Consolas"), px(20.), cx);
    });
    draw(&mut cx);
    let (wide, tall) = shaped_geometry(&editor, &mut cx);

    // Both, and by the same factor: the glyphs and the rows they sit on are
    // derived from the one size, so a caret placed against the shaped text
    // lands on the glyph it points at instead of beside it.
    assert!(
        wide > narrow * 1.9,
        "the text did not grow: {narrow:?} -> {wide:?}"
    );
    assert!(
        tall > short * 1.9,
        "the rows did not grow: {short:?} -> {tall:?}"
    );
}

/// Hit testing reads the shaped lines of the last frame, so a click has to
/// follow the injected size without anything else being told about it.
#[gpui::test]
fn a_click_lands_on_the_column_the_injected_size_puts_under_it(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Point};

    let (editor, mut cx) = open("listen 22", cx);
    editor.update(&mut cx, |editor, cx| {
        editor.set_font(font("Consolas"), px(20.), cx);
    });
    draw(&mut cx);

    // The headless text system advances every character by six tenths of the
    // font size, which is what makes the column arithmetic here exact.
    let advance = px(12.);
    let gutter = editor.read(&mut cx, |editor| editor.layout.gutter);
    let line_height = editor.read(&mut cx, |editor| editor.layout.line_height);
    let position = Point {
        // Just past the middle of the fourth character, so the nearest boundary
        // is unambiguous.
        x: gutter + advance * 3.2,
        y: line_height / 2.,
    };
    cx.simulate_event(MouseDownEvent {
        position,
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position,
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 1,
    });
    cx.run_until_parked();

    assert_eq!(editor.caret(&mut cx), 3, "the click missed its column");
}

/// The host pushes the font every frame, exactly as it pushes the palette, so
/// an unchanged pair has to cost nothing.
#[gpui::test]
fn pushing_the_same_font_again_repaints_nothing(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("listen 22", cx);
    editor.update(&mut cx, |editor, cx| {
        editor.set_font(font("Consolas"), px(20.), cx);
    });
    draw(&mut cx);
    let before = shaped_geometry(&editor, &mut cx);

    let notified = Rc::new(RefCell::new(0_usize));
    let observation = cx.update(|_, cx| {
        let seen = notified.clone();
        cx.observe(&editor.editor, move |_, _| *seen.borrow_mut() += 1)
    });
    editor.update(&mut cx, |editor, cx| {
        editor.set_font(font("Consolas"), px(20.), cx);
    });
    cx.run_until_parked();

    assert_eq!(*notified.borrow(), 0, "an unchanged font asked for a frame");
    assert_eq!(
        shaped_geometry(&editor, &mut cx),
        before,
        "an unchanged font reshaped the text"
    );

    // And the other half of the same claim, so that the silence above is the
    // guard doing its work rather than the observation never firing at all.
    editor.update(&mut cx, |editor, cx| {
        editor.set_font(font("Consolas"), px(21.), cx);
    });
    cx.run_until_parked();
    drop(observation);
    assert!(*notified.borrow() > 0, "a changed font asked for no frame");
}

/// The language is pushed in by the host on the same terms as the font and the
/// palette, so an unchanged one has to cost nothing.
#[gpui::test]
fn pushing_the_same_language_again_repaints_nothing(cx: &mut TestAppContext) {
    let (editor, mut cx) = open("echo hi", cx);
    editor.update(&mut cx, |editor, cx| {
        editor.set_language(Language::Shell, cx);
    });
    draw(&mut cx);

    let notified = Rc::new(RefCell::new(0_usize));
    let observation = cx.update(|_, cx| {
        let seen = notified.clone();
        cx.observe(&editor.editor, move |_, _| *seen.borrow_mut() += 1)
    });
    editor.update(&mut cx, |editor, cx| {
        editor.set_language(Language::Shell, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        *notified.borrow(),
        0,
        "an unchanged language asked for a frame"
    );

    editor.update(&mut cx, |editor, cx| {
        editor.set_language(Language::Yaml, cx);
    });
    cx.run_until_parked();
    drop(observation);
    assert!(
        *notified.borrow() > 0,
        "a changed language asked for no frame"
    );
    assert_eq!(
        editor.read(&mut cx, EditorView::language),
        Language::Yaml,
        "the language did not stick"
    );
}

/// A file whose colours cross line boundaries, drawn: the runs the lexer
/// produces have to add up to each line exactly, and a shaping call given runs
/// that do not is what this would catch.
#[gpui::test]
fn a_highlighted_file_shapes_without_complaint(cx: &mut TestAppContext) {
    let text = "#!/bin/sh\n\
                # 주석입니다\n\
                NAME=\"한글 값\"\n\
                cat <<'EOF'\n\
                anything at all: 🙂\n\
                EOF\n\
                echo \"${NAME}\" # 끝\n";
    let (editor, mut cx) = open(text, cx);
    editor.update(&mut cx, |editor, cx| {
        editor.set_language(Language::Shell, cx);
        editor.move_to(0, cx);
    });
    draw(&mut cx);

    assert!(editor.read(&mut cx, EditorView::shaped_lines) >= 7);
    // The heredoc reached the line under it, which is the whole point of the
    // cache: line four is a string only because line three opened one.
    let inside = editor.read(&mut cx, |editor| {
        editor.highlighter().tokens(editor.buffer(), 4)
    });
    assert_eq!(inside[0].kind, TokenKind::String);
}

// The palette. Pure arithmetic over a colour scheme, so unlike everything above
// these need no window: what they hold on to is that the editor's surface is
// the terminal's surface, and that every fill painted under the text is opaque.

/// The fifteen slots, so a property can be asserted of all of them at once.
fn slots(palette: &EditorPalette) -> [Hsla; 15] {
    [
        palette.background,
        palette.foreground,
        palette.cursor,
        palette.selection,
        palette.line_highlight,
        palette.gutter,
        palette.gutter_active,
        palette.find_match,
        palette.find_current,
        palette.comment,
        palette.string,
        palette.number,
        palette.keyword,
        palette.key,
        palette.variable,
    ]
}

/// The six syntax slots on their own.
fn syntax_slots(palette: &EditorPalette) -> [Hsla; 6] {
    [
        palette.comment,
        palette.string,
        palette.number,
        palette.keyword,
        palette.key,
        palette.variable,
    ]
}

/// Every built-in scheme with its name, which is the set every palette
/// property is asserted over.
///
/// Read off the registry rather than listed here, so that a scheme added later
/// is held to the same properties without anybody remembering to add it.
fn schemes() -> Vec<(&'static str, TerminalTheme)> {
    TerminalTheme::builtin()
        .iter()
        .map(|info| (info.name, TerminalTheme::by_name_or_default(info.id)))
        .collect()
}

/// The ANSI index and the least contrast each syntax slot is built from, in the
/// order [`syntax_slots`] returns them.
const SYNTAX_SOURCES: [(usize, f32); 6] = [
    (ANSI_BRIGHT_BLACK, MIN_COMMENT_CONTRAST),
    (ANSI_GREEN, MIN_CONTRAST),
    (ANSI_MAGENTA, MIN_CONTRAST),
    (ANSI_BLUE, MIN_CONTRAST),
    (ANSI_CYAN, MIN_CONTRAST),
    (ANSI_YELLOW, MIN_CONTRAST),
];

#[test]
fn mixing_walks_from_one_colour_to_the_other_and_clamps() {
    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);
    assert_eq!(mix(black, white, 0.), black);
    assert_eq!(mix(black, white, 1.), white);
    assert_eq!(mix(black, white, 0.5), Rgb::new(128, 128, 128));
    // Out of range is clamped rather than extrapolated: a share past either end
    // would leave the visible range and wrap the channel.
    assert_eq!(mix(black, white, -1.), black);
    assert_eq!(mix(black, white, 2.), white);
}

#[test]
fn the_four_colours_a_scheme_names_are_taken_verbatim() {
    // The whole point of deriving from the scheme: a caret in the editor and a
    // caret in the terminal beside it have to be the same mark.
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
    // Five of the fifteen are painted as fills under the text, over a
    // background that is itself opaque. An alpha anywhere here would make one
    // highlight darken whatever it happened to land on top of.
    for (name, scheme) in schemes() {
        for colour in slots(&palette_for(&scheme)) {
            assert_eq!(colour.a, 1., "a slot of {name} is translucent");
        }
    }
}

// The syntax slots. What is asserted of them is not which hue they landed on —
// that is the scheme's business, and the point of deriving from it — but that
// each one can be seen and that no two of them are the same mark.

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
    // The mapping the documentation on `palette_for` sets out, asserted rather
    // than described: a scheme's green is the string colour and nothing else
    // decides it.
    for (name, scheme) in schemes() {
        let palette = palette_for(&scheme);
        for (slot, (index, least)) in syntax_slots(&palette).into_iter().zip(SYNTAX_SOURCES) {
            let expected = legible(
                scheme.ansi[index],
                scheme.background,
                scheme.foreground,
                least,
            );
            assert_eq!(slot, to_hsla(expected), "{name} at ANSI {index}");
        }
    }
}

#[test]
fn every_syntax_colour_stands_off_the_page_in_every_built_in_scheme() {
    // Not a tautology: `legible` gives up after four steps and falls back to
    // the foreground, so this is the claim that no built-in scheme needs the
    // fallback to fail. Solarized Dark is the one this was written for — its
    // bright black is three percent off its background.
    for (name, scheme) in schemes() {
        for (index, least) in SYNTAX_SOURCES {
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
        let slots = syntax_slots(&palette);
        for (index, colour) in slots.iter().enumerate() {
            assert_ne!(
                *colour, palette.background,
                "a token drawn on itself in {name}"
            );
            for other in &slots[index + 1..] {
                assert_ne!(colour, other, "two syntax slots collided in {name}");
            }
        }
    }
}

#[test]
fn a_plain_token_is_the_scheme_s_own_foreground() {
    // The claim the whole design rests on: a file with nothing to highlight
    // looks exactly as it did before there were lexers.
    let scheme = TerminalTheme::gruvbox_dark();
    assert_eq!(palette_for(&scheme).foreground, to_hsla(scheme.foreground));
}

#[test]
fn the_marks_that_can_overlap_are_told_apart() {
    for scheme in [TerminalTheme::dark(), TerminalTheme::light()] {
        let palette = palette_for(&scheme);
        // A match and the match the find bar is on sit side by side on screen.
        assert_ne!(palette.find_match, palette.find_current);
        // Both have to be visible against the page they are drawn on, as does
        // the wash across the caret's line.
        assert_ne!(palette.find_match, palette.background);
        assert_ne!(palette.find_current, palette.background);
        assert_ne!(palette.line_highlight, palette.background);
        // The gutter is quieter than the line number the caret is on.
        assert_ne!(palette.gutter, palette.gutter_active);
    }
}

#[test]
fn a_different_scheme_is_a_different_palette() {
    // What makes switching the scheme — from the settings, or per session —
    // actually reach the text surface.
    assert_ne!(
        slots(&palette_for(&TerminalTheme::dark())),
        slots(&palette_for(&TerminalTheme::light()))
    );
}
