//! The rules the workspace can be held to without a window, and the one thing
//! that needs one.
//!
//! Everything the tab strip decides — what a tab of an open file is called,
//! whether closing it has to ask, where the focus lands as tabs are taken out —
//! is a rule about names and indices, and each is written as a free function
//! precisely so that it can be checked here without a session, a pane or a
//! window. What is left is [`centered_scroll`], which is entirely a question of
//! layout: it is put under test through what its scroll handle reports, since
//! the handle is where gpui writes down the answer — the box it measured, and
//! how far past it the column ran.

use std::ops::Deref;

use super::*;
use gpui::{TestAppContext, VisualTestContext};

/// Height of the stand-in column.
///
/// Nothing about the real welcome screen's contents matters here — only that
/// there is a definite height to hold the window against — so the test hands
/// the box one plain child rather than rebuilding the screen.
const COLUMN: f32 = 400.;

/// A window tall enough for the column and both its margins, several times
/// over.
const ROOMY: f32 = 900.;

/// A window shorter than the column, which is the whole point of the box.
const CRAMPED: f32 = 300.;

/// Wide enough that nothing wraps; the box only scrolls one way.
const WIDTH: f32 = 600.;

/// How far apart two measurements may be and still count as the same, in a
/// layout whose lengths are rounded to hundredths of a pixel.
const SLACK: f32 = 0.5;

/// A window holding nothing but the box under test.
struct Harness {
    scroll: ScrollHandle,
    bar: ScrollbarState,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::dark();
        let bar = Scrollbar::for_handle(SCROLLBARS[0].0, Surface::Empty.axis(), &self.scroll)
            .fade(self.bar.fade());

        div().flex().flex_col().size_full().child(centered_scroll(
            EMPTY_STATE,
            &self.scroll,
            bar,
            &theme,
            div().flex_none().w(px(320.)).h(px(COLUMN)),
        ))
    }
}

/// Opens the harness in a window `height` tall and hands back its handle.
///
/// Drawn twice: a bar is built from the box as the previous frame measured
/// it, so the opening frame has nothing to build one out of.
fn open(cx: &mut TestAppContext, height: f32) -> ScrollHandle {
    let scroll = ScrollHandle::new();
    let window = cx.add_window({
        let scroll = scroll.clone();
        move |_, _| Harness {
            scroll,
            bar: ScrollbarState::new(),
        }
    });

    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    cx.simulate_resize(size(px(WIDTH), px(height)));
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    scroll
}

/// The bar the workspace would draw over the box as it now stands.
fn scrollbar(scroll: &ScrollHandle) -> Scrollbar {
    Scrollbar::for_handle(SCROLLBARS[0].0, Surface::Empty.axis(), scroll)
}

/// With room to spare the column sits in the middle, exactly where
/// `justify_center` used to put it, and there is nothing to scroll — so no
/// bar is drawn either.
#[gpui::test]
fn a_column_that_fits_stays_in_the_middle(cx: &mut TestAppContext) {
    let scroll = open(cx, ROOMY);
    let box_ = scroll.bounds();
    let column = scroll
        .bounds_for_item(0)
        .expect("the box never measured its column");

    let above = f32::from(column.top() - box_.top());
    let below = f32::from(box_.bottom() - column.bottom());
    assert!(
        (above - below).abs() < SLACK,
        "the column was not centered: {above} above, {below} below"
    );
    assert_eq!(
        scroll.max_offset().y,
        px(0.),
        "a column that fits left something to scroll"
    );
    assert!(
        scrollbar(&scroll).thumb().is_none(),
        "a box with nothing to scroll drew a bar anyway"
    );
}

/// The regression: with less room than the column needs, the head of it used
/// to be pushed off the top edge and left there. It now starts at the top of
/// the box, and everything past the bottom is reachable by scrolling.
#[gpui::test]
fn a_column_that_does_not_fit_starts_at_the_top(cx: &mut TestAppContext) {
    let scroll = open(cx, CRAMPED);
    let box_ = scroll.bounds();
    let column = scroll
        .bounds_for_item(0)
        .expect("the box never measured its column");

    assert!(
        f32::from(column.top() - box_.top()).abs() < SLACK,
        "the column did not start at the top of the box: {:?} in {:?}",
        column,
        box_
    );
    assert!(
        (f32::from(scroll.max_offset().y) - f32::from(column.size.height - box_.size.height)).abs()
            < SLACK,
        "the scrollable range did not cover the whole of the column"
    );
    assert!(
        scrollbar(&scroll).thumb().is_some(),
        "a box with something to scroll drew no bar"
    );
}

/// And the far end of that scroll reaches the foot of the column, margin and
/// all, rather than stopping short of the last button.
#[gpui::test]
fn scrolling_to_the_end_reaches_the_foot_of_the_column(cx: &mut TestAppContext) {
    let scroll = open(cx, CRAMPED);
    scroll.set_offset(point(px(0.), -scroll.max_offset().y));
    let box_ = scroll.bounds();
    let column = scroll
        .bounds_for_item(0)
        .expect("the box never measured its column");

    let foot = column.bottom() + scroll.offset().y;
    assert!(
        f32::from(foot - box_.bottom()).abs() < SLACK,
        "the end of the scroll left {:?} of the column below the box",
        foot - box_.bottom()
    );
    assert!(
        f32::from(column.size.height) > COLUMN + SCROLL_MARGIN,
        "the column was scrolled to its last button rather than past it"
    );
}

#[test]
fn a_tab_of_one_file_is_named_after_the_file_and_the_connection() {
    assert_eq!(
        editor_tab_label("nginx.conf", "web-01").as_ref(),
        "nginx.conf - web-01"
    );
}

#[test]
fn a_connection_with_no_name_to_give_leaves_the_file_name_alone() {
    // "nginx.conf - " reads as a label that was cut off, which is worse
    // than one that simply says less.
    assert_eq!(editor_tab_label("nginx.conf", "").as_ref(), "nginx.conf");
    assert_eq!(editor_tab_label("nginx.conf", "  ").as_ref(), "nginx.conf");
}

#[test]
fn a_session_holding_no_forwarding_has_nothing_to_mark() {
    // `None` is what leaves the tab unmarked, so this is the whole of the
    // rule that a tab which lost the bind — or never asked for a tunnel —
    // looks exactly as it did before.
    assert!(tunnel_tooltip(&[]).is_none());
}

#[test]
fn a_forwarding_is_named_from_the_local_end_to_the_remote_one() {
    let tooltip = tunnel_tooltip(&["8080:db:5432".into()]).expect("a rule to name");
    assert!(tooltip.contains("8080 \u{2192} db:5432"), "{tooltip}");

    // Every rule is named, and on one line: the mark says how many
    // forwardings ride on this tab, so a tooltip that stopped at the first
    // would be answering a different question.
    let both = tunnel_tooltip(&["8080:db:5432".into(), "6379:cache:6379".into()])
        .expect("two rules to name");
    assert!(both.contains("8080 \u{2192} db:5432"), "{both}");
    assert!(both.contains("6379 \u{2192} cache:6379"), "{both}");
    assert!(!both.contains('\n'), "{both}");
}

#[test]
fn a_label_that_is_not_a_rule_is_shown_as_it_arrived() {
    // Nothing emits one today. If anything ever does, it has to reach the
    // user as itself rather than being dropped for not parsing.
    let tooltip = tunnel_tooltip(&["something else".into()]).expect("a label to name");
    assert!(tooltip.contains("something else"), "{tooltip}");
}

/// A profile that does or does not want the panel beside it.
fn profile_showing_files(show_files: bool) -> SessionProfile {
    let mut profile = SessionProfile::new(
        "web-01",
        "example.com",
        22,
        "alice",
        rulogman_core::AuthMethod::Password,
    );
    profile.show_files = show_files;
    profile
}

/// The setting a local shell is judged by, as the settings dialog writes it.
fn local_panel(open: bool) -> FilesSettings {
    FilesSettings { local_panel: open }
}

#[test]
fn a_remote_session_opens_the_panel_its_profile_asked_for() {
    // Both directions, and both against a setting that says the opposite:
    // the setting is for the sessions no profile speaks for, and must not
    // get a vote on the ones that have one.
    assert!(panel_opens_with(
        Some(&profile_showing_files(true)),
        &local_panel(false)
    ));
    assert!(!panel_opens_with(
        Some(&profile_showing_files(false)),
        &local_panel(true)
    ));
}

#[test]
fn a_local_shell_follows_the_setting() {
    assert!(panel_opens_with(None, &local_panel(true)));
    assert!(!panel_opens_with(None, &local_panel(false)));
}

#[test]
fn a_profile_saved_before_the_choice_existed_still_opens_the_panel() {
    // `SessionProfile::new` is what the connection dialog builds a new
    // profile from, and what a `profiles.json` with no key in it loads as.
    // Either way the panel has to go on appearing, which is what every
    // session did before today.
    let profile = SessionProfile::new(
        "web-01",
        "example.com",
        22,
        "alice",
        rulogman_core::AuthMethod::Password,
    );
    assert!(panel_opens_with(Some(&profile), &local_panel(false)));
}

/// The profile the sessions below were opened from.
fn a_profile() -> Uuid {
    Uuid::from_u128(1)
}

/// One session as [`tunnels_held_for`] is given them: which profile it came
/// from, which session it is, and whether it is holding forwardings.
fn session(profile: Option<Uuid>, id: u64, holding: bool) -> (Option<Uuid>, EntityId, bool) {
    (profile, EntityId::from(id), holding)
}

#[test]
fn a_sibling_holding_the_ports_keeps_the_next_session_off_them() {
    // The case the whole rule exists for: a second tab on a profile whose
    // forwardings the first tab is running. Asking again could only fail,
    // once per rule and in yellow across a terminal just opened.
    assert!(tunnels_held_for(
        a_profile(),
        None,
        [session(Some(a_profile()), 1, true)],
    ));
}

#[test]
fn a_sibling_that_is_holding_nothing_leaves_the_ports_free() {
    // Either it never bound them or its transport has gone; both leave the
    // list empty, and both mean the next session may take them.
    assert!(!tunnels_held_for(
        a_profile(),
        None,
        [session(Some(a_profile()), 1, false)],
    ));
}

#[test]
fn another_profiles_forwardings_are_not_this_profiles_business() {
    // Local ports do collide across profiles, but that is a conflict with
    // something outside this profile's tabs, and the transport reporting it
    // is how the user hears about it. A local session — no profile at all —
    // is nobody's sibling either.
    assert!(!tunnels_held_for(
        a_profile(),
        None,
        [
            session(Some(Uuid::from_u128(2)), 1, true),
            session(None, 2, true),
        ],
    ));
}

#[test]
fn a_session_reconnecting_is_not_its_own_rival() {
    // What it is holding this instant it is about to drop, so its own
    // forwardings must not be the reason it comes back without them.
    let reconnecting = session(Some(a_profile()), 1, true);
    assert!(!tunnels_held_for(
        a_profile(),
        Some(EntityId::from(1)),
        [reconnecting],
    ));
    // A second tab holding them is still a reason, exception or no.
    assert!(tunnels_held_for(
        a_profile(),
        Some(EntityId::from(1)),
        [reconnecting, session(Some(a_profile()), 2, true)],
    ));
}

#[test]
fn only_a_tab_that_is_one_unsaved_file_asks_before_it_closes() {
    assert!(tab_close_asks(1, true));
    // Nothing is at stake, so the close button is not a question.
    assert!(!tab_close_asks(1, false));
    // A split tab: the question closes one pane, and this close was aimed
    // at the whole tab.
    assert!(!tab_close_asks(2, true));
}

#[test]
fn closing_a_tab_behind_the_active_one_leaves_the_focus_where_it_is() {
    assert_eq!(active_after_close(1, 3, 0), 1);
}

#[test]
fn closing_a_tab_in_front_of_the_active_one_moves_it_down_a_slot() {
    assert_eq!(active_after_close(3, 1, 0), 2);
}

#[test]
fn a_split_needs_half_a_grid_on_the_axis_it_divides() {
    // Exactly twice the minimum is the last size that still splits, since
    // both halves come out at the minimum itself.
    assert!(split_fits(
        Axis::Horizontal,
        MIN_PANE_COLS * 2,
        MIN_PANE_ROWS
    ));
    assert!(!split_fits(
        Axis::Horizontal,
        MIN_PANE_COLS * 2 - 1,
        MIN_PANE_ROWS
    ));
    assert!(split_fits(Axis::Vertical, MIN_PANE_COLS, MIN_PANE_ROWS * 2));
    assert!(!split_fits(
        Axis::Vertical,
        MIN_PANE_COLS,
        MIN_PANE_ROWS * 2 - 1
    ));
}

#[test]
fn a_split_ignores_the_axis_it_does_not_divide() {
    // A side-by-side split leaves the row count alone, so a grid one row
    // tall still splits horizontally — and a grid one column wide still
    // splits vertically. Each half keeps the whole of the other dimension.
    assert!(split_fits(Axis::Horizontal, MIN_PANE_COLS * 2, 1));
    assert!(split_fits(Axis::Vertical, 1, MIN_PANE_ROWS * 2));
}

#[test]
fn the_caret_is_printed_as_the_line_out_of_the_lines_and_then_the_column() {
    assert_eq!(caret_summary(12, 200, 5).as_ref(), "12/200 : 5");
    // A file of one line still reads as a fraction rather than as a bare
    // number: the second half is what says how much file there is.
    assert_eq!(caret_summary(1, 1, 1).as_ref(), "1/1 : 1");
}

#[test]
fn a_named_format_is_labeled_by_its_own_name() {
    // Straight out of the syntax module, untranslated, because `JSON` is
    // `JSON` in every locale. Plain text is the one row that is looked up,
    // and what it comes back as depends on which locale is loaded — which
    // is the i18n module's test to make, not this one's.
    let registry = rugpui_editor::LanguageRegistry::builtin();
    let label = |id: &str| language_label(registry.get(id).expect(id));
    assert_eq!(label("json").as_ref(), "JSON");
    assert_eq!(label("dockerfile").as_ref(), "Dockerfile");
    assert!(!label(languages::PLAIN).is_empty());
}

#[test]
fn closing_the_active_tab_hands_the_focus_to_the_survivor() {
    // A survivor in front of the hole does not move.
    assert_eq!(active_after_close(2, 2, 0), 0);
    // One behind it moves down with everything else: the tab that was
    // fourth is third once the second has gone.
    assert_eq!(active_after_close(1, 1, 3), 2);
}

/// Off macOS the bare control letters a shell lives on — word erase, transpose,
/// XON — must arrive at the terminal, and the application's chords for the same
/// commands are the shifted ones. Every binding the terminal and the workspace
/// register is in the keymap here, so an empty answer for a bare chord is the
/// keymap having nothing to take it away with.
#[cfg(not(target_os = "macos"))]
#[gpui::test]
fn bare_control_letters_are_left_to_the_shell(cx: &mut TestAppContext) {
    cx.update(|cx| {
        TerminalView::init(cx);
        bootstrap::bind_shortcuts(cx);
        for chord in ["ctrl-t", "ctrl-w", "ctrl-q"] {
            let keystroke = gpui::Keystroke::parse(chord).unwrap();
            assert!(
                cx.all_bindings_for_input(&[keystroke]).is_empty(),
                "{chord}"
            );
        }
        let bound = |chord: &str| {
            let keystroke = gpui::Keystroke::parse(chord).unwrap();
            cx.all_bindings_for_input(&[keystroke])
                .iter()
                .map(|binding| binding.action().name())
                .collect::<Vec<_>>()
        };
        use gpui::Action as _;
        assert_eq!(bound("ctrl-shift-t"), [NewSession.name()]);
        assert_eq!(bound("ctrl-shift-w"), [CloseSession.name()]);
        assert_eq!(bound("ctrl-shift-q"), [Quit.name()]);
    });
}

#[test]
fn stepping_through_the_tabs_wraps_at_either_end() {
    assert_eq!(stepped_tab(0, 3, true), Some(1));
    assert_eq!(stepped_tab(2, 3, true), Some(0));
    assert_eq!(stepped_tab(0, 3, false), Some(2));
    assert_eq!(stepped_tab(1, 3, false), Some(0));
    // One tab, or none, has nowhere to step to.
    assert_eq!(stepped_tab(0, 1, true), None);
    assert_eq!(stepped_tab(0, 0, false), None);
}

/// The tab chord reaches the workspace from inside a split tab — the case the
/// digits cannot serve, since there they pick a pane.
#[gpui::test]
fn the_next_tab_chord_leaves_a_split_tab(cx: &mut TestAppContext) {
    cx.update(|cx| {
        TerminalView::init(cx);
        bootstrap::bind_shortcuts(cx);
    });
    let (workspace, cx) = super::workspace(cx, false);
    super::open_local(&workspace, cx);
    super::open_tailed(&workspace, cx, &["/var/log/syslog"]);
    assert_eq!(workspace.read_with(cx, |workspace, _| workspace.active), 1);
    let chord = if cfg!(target_os = "macos") {
        "cmd-}"
    } else {
        "ctrl-pagedown"
    };
    cx.simulate_keystrokes(chord);
    assert_eq!(workspace.read_with(cx, |workspace, _| workspace.active), 0);
    let back = if cfg!(target_os = "macos") {
        "cmd-{"
    } else {
        "ctrl-pageup"
    };
    cx.simulate_keystrokes(back);
    assert_eq!(workspace.read_with(cx, |workspace, _| workspace.active), 1);
}

/// The shortcut sheet is written from the binding constants, and every one of
/// its descriptions is a looked-up sentence rather than a key.
#[test]
fn the_shortcut_sheet_names_the_chords_the_bindings_use() {
    let groups = shortcuts::shortcut_groups();
    assert_eq!(groups.len(), 4);
    let rows: Vec<_> = groups.iter().flat_map(|(_, rows)| rows).collect();
    assert!(
        rows.iter()
            .any(|(key, _)| key.as_ref() == NEW_SESSION_SHORTCUT_LABEL),
        "the new-session chord is missing"
    );
    for (key, action) in rows {
        assert!(!key.is_empty());
        assert!(
            !action.contains('.') || action.contains(' '),
            "untranslated {action}"
        );
    }
}

/// With a shell open, the sheet opens over it and `Escape` puts it away.
#[gpui::test]
fn the_shortcut_sheet_opens_and_escape_closes_it(cx: &mut TestAppContext) {
    let (workspace, cx) = super::workspace(cx, false);
    workspace.update(cx, |workspace, cx| workspace.show_shortcuts(cx));
    assert!(workspace.read_with(cx, |workspace, cx| workspace.dialog_open(cx)));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.dismiss_dialog_action(&DismissDialog, window, cx);
    });
    assert!(!workspace.read_with(cx, |workspace, _| workspace.shortcuts_open));
}

/// What closing the window lists: open sessions, by count, while the setting
/// asks for them — and nothing once it does not.
#[gpui::test]
fn the_close_guard_lists_open_sessions_only_while_asked_to(cx: &mut TestAppContext) {
    let (workspace, cx) = super::workspace(cx, false);
    super::open_remote(&workspace, cx, false);
    super::open_remote(&workspace, cx, false);
    let items = workspace.read_with(cx, |workspace, cx| workspace.unsaved_work(cx));
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(items[0].contains('2'), "{items:?}");

    cx.update(|_, cx| {
        let mut settings = app_settings::current(cx);
        settings.window.confirm_close_with_sessions = false;
        app_settings::replace(settings, cx);
    });
    let items = workspace.read_with(cx, |workspace, cx| workspace.unsaved_work(cx));
    assert!(items.is_empty(), "{items:?}");
}

/// `F10` opens the application menu from anywhere but a terminal pane, where
/// it belongs to the remote program — mc's Quit, among others.
#[cfg(not(target_os = "macos"))]
#[gpui::test]
fn f10_opens_the_menu_everywhere_but_in_a_terminal(cx: &mut TestAppContext) {
    cx.update(|cx| {
        rugpui::init(cx);
        TerminalView::init(cx);
        bootstrap::bind_shortcuts(cx);
    });
    let (workspace, cx) = super::workspace(cx, false);
    super::open_local(&workspace, cx);
    cx.simulate_keystrokes("f10");
    assert!(
        !workspace.read_with(cx, |workspace, _| workspace.menu_open),
        "F10 was taken from the terminal"
    );

    workspace.update_in(cx, |workspace, window, cx| {
        let handle = workspace.focus_handle.clone();
        window.focus(&handle, cx);
    });
    cx.simulate_keystrokes("f10");
    assert!(workspace.read_with(cx, |workspace, _| workspace.menu_open
        && workspace.menu_by_keyboard));
}
