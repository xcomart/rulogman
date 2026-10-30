use super::*;

use std::time::Duration;

#[gpui::test]
fn a_local_shell_opens_with_the_panel_the_setting_asked_for(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, true);
    open_local(&workspace, cx);
    assert!(
        showing(&workspace, cx),
        "a local shell ignored a setting that asked for the panel"
    );

    // The setting is read when the tab opens, so a change to it reaches the
    // next shell and leaves the one already open alone.
    cx.update(|_window, cx| set_local_panel(cx, false));
    open_local(&workspace, cx);
    assert!(
        !showing(&workspace, cx),
        "a local shell ignored a setting that refused the panel"
    );
    assert!(
        flag(&workspace, cx, 0),
        "changing the setting shut the panel on a shell already open"
    );
}

#[gpui::test]
fn the_toggle_moves_the_active_tab_and_no_other(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    open_remote(&workspace, cx, true);

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_file_panel(window, cx)
    });
    assert!(
        !showing(&workspace, cx),
        "the toggle did not shut the panel"
    );
    assert!(
        flag(&workspace, cx, 0),
        "shutting the panel on one tab shut it on the tab beside it"
    );

    // Each tab goes on showing its own answer as the strip is walked, which
    // is the whole of what the flag being per tab buys.
    select(&workspace, cx, 0);
    assert!(showing(&workspace, cx));
    select(&workspace, cx, 1);
    assert!(!showing(&workspace, cx));

    // And the toggle is a toggle: the same tab comes back.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_file_panel(window, cx)
    });
    assert!(showing(&workspace, cx));
}

#[gpui::test]
fn the_welcome_screen_has_no_panel_and_nothing_to_toggle(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, true);
    assert!(
        !showing(&workspace, cx),
        "a window with nothing open drew the panel beside the welcome screen"
    );

    // The guard the grayed-out menu row and the missing toolbar button agree
    // with: there is no tab to write the answer on, so the shortcut does
    // nothing rather than panicking or arming a panel nothing can draw.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_file_panel(window, cx)
    });
    assert!(!showing(&workspace, cx));
}

#[gpui::test]
fn the_panel_edge_is_grabbed_on_the_seam_beside_it(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    cx.run_until_parked();
    let before = cx.debug_bounds("file-panel").expect("the panel is drawn");

    // The panel's outer edge, which is the middle of the two hairlines that
    // meet there — the panel's own and the pane's. A band tucked inside the
    // panel's border never reached this far, and the press went to the pane.
    let seam = point(before.right(), before.center().y);
    let to = point(seam.x + px(40.), seam.y);
    cx.simulate_mouse_move(seam, None, gpui::Modifiers::none());
    cx.simulate_mouse_down(seam, MouseButton::Left, gpui::Modifiers::none());
    // Past the two pixels gpui tells a drag from a click by, then on.
    cx.simulate_mouse_move(
        point(seam.x + px(3.), seam.y),
        Some(MouseButton::Left),
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();

    // Exactly as far as the pointer went: grabbed on the seam, the edge does
    // not first jump to meet it.
    let after = cx.debug_bounds("file-panel").expect("the panel is drawn");
    assert_eq!(after.left(), before.left());
    assert_eq!(after.right(), to.x, "the edge did not follow the pointer");
}

#[gpui::test]
fn a_pane_broken_out_takes_the_panel_its_tab_was_showing(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);

    // A tab whose profile refused the panel: the tab it splits off must not
    // gain one on the way out.
    open_remote(&workspace, cx, false);
    split_active(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.break_out_active_pane(window, cx);
    });
    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.tabs.len()),
        2
    );
    assert!(
        !showing(&workspace, cx),
        "a pane broken out of a tab with no panel opened one"
    );

    // And the other way: the flag followed is the source tab's, whatever it
    // says. Toggled rather than opened from a profile, because it is what
    // the tab shows *now* that the new one continues — the profile has had
    // its say and the user may have moved on from it.
    select(&workspace, cx, 0);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_file_panel(window, cx)
    });
    split_active(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.break_out_active_pane(window, cx);
    });
    assert!(
        showing(&workspace, cx),
        "a pane broken out of a tab showing the panel lost it"
    );
}

/// Closing the pane you are working in should put you back where you came
/// from, which on a tab split more than once is not the pane beside it.
///
/// Three panes in a row, the keyboard in the middle one having come from the
/// leftmost: layout order would answer the pane on the right, which is one
/// the user has not looked at since the split that made it.
#[gpui::test]
fn closing_a_pane_hands_the_keyboard_back_to_the_one_it_came_from(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    split_active(&workspace, cx);
    split_active(&workspace, cx);

    let panes = pane_ids(&workspace, cx);
    assert_eq!(panes.len(), 3);

    focus_pane(&workspace, cx, panes[0]);
    focus_pane(&workspace, cx, panes[1]);
    assert_eq!(active_pane(&workspace, cx), panes[1]);

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_active_pane(window, cx);
    });

    assert_eq!(
        active_pane(&workspace, cx),
        panes[0],
        "closing the middle pane followed layout order instead of the focus history"
    );
}

/// A pane closing in the background takes its entry with it, so the pane the
/// keyboard came from is still the one it goes back to afterwards.
#[gpui::test]
fn a_pane_that_closed_unwatched_is_not_offered_as_a_successor(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    split_active(&workspace, cx);
    split_active(&workspace, cx);

    let panes = pane_ids(&workspace, cx);
    // The keyboard's whole history: the right pane, then the left, then the
    // middle. The right one is the oldest and is about to go.
    focus_pane(&workspace, cx, panes[2]);
    focus_pane(&workspace, cx, panes[0]);
    focus_pane(&workspace, cx, panes[1]);

    // Closed without ever being focused again — a session that hung up on
    // its own, in the codebase this stands in for.
    workspace.update_in(cx, |workspace, window, cx| {
        let active = workspace.active;
        workspace.remove_pane(active, panes[0], window, cx);
    });
    // The active pane was not the one removed, so it stands.
    assert_eq!(active_pane(&workspace, cx), panes[1]);

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_active_pane(window, cx);
    });
    assert_eq!(
        active_pane(&workspace, cx),
        panes[2],
        "a pane that had already closed was picked as the successor"
    );
}

/// Splitting the same pane twice leaves the first half twice the width of
/// the other two, and nothing but this command squares them up.
#[gpui::test]
fn evening_the_columns_out_gives_every_pane_the_same_width(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    split_active(&workspace, cx);
    split_active(&workspace, cx);

    // Both dividers sit at the middle of what they divide, so the leftmost
    // pane has half the window and the other two a quarter each.
    assert_eq!(ratios(&workspace, cx), vec![0.5, 0.5]);
    assert!(workspace.read_with(cx, |workspace, _| workspace.can_equalize(Axis::Horizontal)));
    // Nothing is stacked, so there is no row to even out.
    assert!(!workspace.read_with(cx, |workspace, _| workspace.can_equalize(Axis::Vertical)));

    workspace.update(cx, |workspace, cx| {
        workspace.equalize_panes(Axis::Horizontal, cx);
    });

    // A third to the left of the outer divider, and the inner one halving
    // what is left: three equal columns.
    assert_eq!(ratios(&workspace, cx), vec![1. / 3., 0.5]);
}

#[gpui::test]
fn a_tab_that_leaves_its_window_keeps_its_sessions_running(cx: &mut TestAppContext) {
    let (source, cx) = workspace(cx, false);
    open_local(&source, cx);
    open_local(&source, cx);

    let session = source.read_with(cx, |workspace, cx| {
        workspace.tabs[0].sessions(cx)[0].clone()
    });

    let tab = source
        .update_in(cx, |workspace, window, cx| {
            workspace.detach_tab(0, window, cx)
        })
        .expect("a window with two tabs may send one of them off");

    assert_eq!(
        source.read_with(cx, |workspace, _| workspace.tabs.len()),
        1,
        "the tab that left is still in the strip it left"
    );
    assert_eq!(
        source.read_with(cx, |workspace, _| workspace.active),
        0,
        "the active tab was not brought back into range behind the hole"
    );
    assert!(
        !session.read_with(cx, |session, _| matches!(
            session.status(),
            SessionStatus::Disconnected { .. }
        )),
        "moving a tab hung its session up, which is what closing one does"
    );

    let target = second_window(cx);
    target
        .update(cx, |workspace, window, cx| {
            workspace.adopt_tab(tab, window, cx);
        })
        .expect("the second window is open");

    let (tabs, active, sessions) = target
        .update(cx, |workspace, _window, cx| {
            (
                workspace.tabs.len(),
                workspace.active,
                workspace.sessions(cx),
            )
        })
        .expect("the second window is open");
    assert_eq!(
        tabs, 1,
        "the tab did not arrive in the window it was sent to"
    );
    assert_eq!(
        active, 0,
        "the tab arrived without being brought to the front"
    );
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].entity_id(),
        session.entity_id(),
        "the tab arrived on a different session from the one it left with"
    );
}

#[gpui::test]
fn a_split_tab_arrives_with_its_shape(cx: &mut TestAppContext) {
    let (source, cx) = workspace(cx, false);
    open_local(&source, cx);
    open_local(&source, cx);
    split_active(&source, cx);

    let (leaves, active_pane) = source.read_with(cx, |workspace, _| {
        let tab = &workspace.tabs[1];
        (tab.panes.leaf_count(), tab.active_pane())
    });
    assert_eq!(leaves, 2, "the split did not take");

    let tab = source
        .update_in(cx, |workspace, window, cx| {
            workspace.detach_tab(1, window, cx)
        })
        .expect("a window with two tabs may send one of them off");
    let target = second_window(cx);
    target
        .update(cx, |workspace, window, cx| {
            workspace.adopt_tab(tab, window, cx);
        })
        .expect("the second window is open");

    let (moved_leaves, moved_active, sessions) = target
        .update(cx, |workspace, _window, cx| {
            let tab = &workspace.tabs[0];
            (
                tab.panes.leaf_count(),
                tab.active_pane(),
                workspace.sessions(cx).len(),
            )
        })
        .expect("the second window is open");
    assert_eq!(moved_leaves, 2, "the split collapsed on the way across");
    assert_eq!(
        moved_active, active_pane,
        "the tab arrived with the keyboard in a different pane from the one it left in"
    );
    assert_eq!(sessions, 2, "a pane of the split tab lost its session");
}

#[gpui::test]
fn a_moved_pane_asks_its_new_window_which_commands_to_offer(cx: &mut TestAppContext) {
    let (source, cx) = workspace(cx, false);
    open_local(&source, cx);
    open_local(&source, cx);
    // The *second* tab is the split one, and it is the one left behind. The
    // pane that travels is therefore in an unsplit tab both before and
    // after, so the answer below turns on nothing but which workspace gives
    // it.
    split_active(&source, cx);

    let view = terminal_of(&source, cx, 0);
    assert!(
        cx.update(|_window, cx| view.read(cx).caps_at(80, 24, cx).break_out),
        "a workspace with a split tab in front did not offer the break-out"
    );

    let tab = source
        .update_in(cx, |workspace, window, cx| {
            workspace.detach_tab(0, window, cx)
        })
        .expect("a window with two tabs may send one of them off");
    let target = second_window(cx);
    target
        .update(cx, |workspace, window, cx| {
            workspace.adopt_tab(tab, window, cx);
        })
        .expect("the second window is open");

    assert!(
        !cx.update(|_window, cx| view.read(cx).caps_at(80, 24, cx).break_out),
        "the moved pane is still asking the workspace it left which commands it has"
    );
}

#[gpui::test]
fn the_only_tab_of_a_window_cannot_be_sent_off(cx: &mut TestAppContext) {
    assert!(
        !tab_can_move_out(1),
        "a window offered to move the one tab it has, leaving itself empty"
    );
    assert!(tab_can_move_out(2), "a window with a tab to spare refused");

    let (source, cx) = workspace(cx, false);
    open_local(&source, cx);
    assert!(
        source
            .update_in(cx, |workspace, window, cx| {
                workspace.detach_tab(0, window, cx)
            })
            .is_none(),
        "the command took the only tab out anyway"
    );
    assert_eq!(
        source.read_with(cx, |workspace, _| workspace.tabs.len()),
        1,
        "the refused move emptied the strip"
    );
}

/// Whether the pane numbers are up.
fn hints_shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| workspace.pane_hints.shown)
}

/// `Ctrl` alone, held down.
fn ctrl() -> gpui::Modifiers {
    gpui::Modifiers {
        control: true,
        ..Default::default()
    }
}

#[gpui::test]
fn ctrl_and_a_digit_focus_the_pane_of_that_number(cx: &mut TestAppContext) {
    cx.update(bootstrap::bind_shortcuts);
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    split_active(&workspace, cx);
    split_active(&workspace, cx);
    let panes = pane_ids(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_active(window, cx)
    });

    cx.simulate_keystrokes("ctrl-1");
    assert_eq!(active_pane(&workspace, cx), panes[0]);
    cx.simulate_keystrokes("ctrl-3");
    assert_eq!(active_pane(&workspace, cx), panes[2]);

    // Past the last pane nothing moves — not the pane, and not the tab.
    cx.simulate_keystrokes("ctrl-4");
    assert_eq!(active_pane(&workspace, cx), panes[2]);

    // `Tab` steps through them in the same order, wrapping at the end.
    cx.simulate_keystrokes("ctrl-tab");
    assert_eq!(active_pane(&workspace, cx), panes[0]);
    cx.simulate_keystrokes("ctrl-shift-tab");
    assert_eq!(active_pane(&workspace, cx), panes[2]);
}

/// Off macOS the digits are the tab chord as well, and a tab with nothing to
/// number leaves them to it.
#[cfg(not(target_os = "macos"))]
#[gpui::test]
fn ctrl_and_a_digit_still_pick_a_tab_when_it_is_not_split(cx: &mut TestAppContext) {
    cx.update(bootstrap::bind_shortcuts);
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    open_local(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_active(window, cx)
    });

    cx.simulate_keystrokes("ctrl-1");
    assert_eq!(workspace.read_with(cx, |workspace, _| workspace.active), 0);
}

#[gpui::test]
fn holding_ctrl_over_a_split_tab_numbers_its_panes(cx: &mut TestAppContext) {
    cx.update(bootstrap::bind_shortcuts);
    let (workspace, cx) = workspace(cx, false);
    // The numbers are drawn only in the window that has the keyboard.
    cx.update(|window, _| window.activate_window());
    open_local(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_active(window, cx)
    });

    // Nothing to number on a single pane.
    cx.simulate_modifiers_change(ctrl());
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(!hints_shown(&workspace, cx));
    cx.simulate_modifiers_change(gpui::Modifiers::default());

    split_active(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_active(window, cx)
    });
    cx.simulate_modifiers_change(ctrl());
    assert!(
        !hints_shown(&workspace, cx),
        "the numbers skipped the delay"
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(hints_shown(&workspace, cx), "holding Ctrl drew no numbers");
    let order = workspace.update_in(cx, |workspace, window, cx| {
        workspace.pane_hint_order(window, cx)
    });
    assert_eq!(
        order,
        Some(pane_ids(&workspace, cx)),
        "the numbers were not drawn"
    );

    // Letting go takes them down.
    cx.simulate_modifiers_change(gpui::Modifiers::default());
    assert!(!hints_shown(&workspace, cx));
}

#[gpui::test]
fn a_ctrl_chord_meant_for_the_shell_keeps_the_numbers_down(cx: &mut TestAppContext) {
    cx.update(bootstrap::bind_shortcuts);
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    split_active(&workspace, cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_active(window, cx)
    });

    cx.simulate_modifiers_change(ctrl());
    cx.simulate_keystrokes("ctrl-c");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(
        !hints_shown(&workspace, cx),
        "Ctrl+C flashed the pane numbers"
    );

    // A pane shortcut, on the other hand, puts them up at once.
    cx.simulate_modifiers_change(gpui::Modifiers::default());
    cx.simulate_modifiers_change(ctrl());
    cx.simulate_keystrokes("ctrl-tab");
    assert!(hints_shown(&workspace, cx));
}

/// A followed file scrolls with the bare navigation keys — it takes no input —
/// and its scrollback is searched from the keyboard: the search chord opens the
/// bar, a query selects its newest match, and `Escape` closes the bar.
#[gpui::test]
fn a_followed_file_scrolls_and_searches_from_the_keyboard(cx: &mut TestAppContext) {
    cx.update(|cx| {
        TerminalView::init(cx);
        bootstrap::bind_shortcuts(cx);
    });
    let (workspace, cx) = workspace(cx, false);
    open_tail(&workspace, cx, "/var/log/syslog");
    let (tail, session) = workspace.read_with(cx, |workspace, cx| {
        let tab = &workspace.tabs[workspace.active];
        let PaneView::Tail(tail) = tab.active_view() else {
            panic!("the tab is not a followed file");
        };
        (tail.clone(), tab.sessions(cx)[0].clone())
    });
    session.update(cx, |session, _| {
        for line in 0..200 {
            let text = if line % 50 == 7 {
                format!("line {line} ERROR disk\r\n")
            } else {
                format!("line {line} ok\r\n")
            };
            session.terminal_mut().feed_str(&text);
        }
    });
    let offset = |cx: &mut VisualTestContext| {
        session.read_with(cx, |session, _| {
            session.terminal().scroll_position().display_offset
        })
    };

    cx.simulate_keystrokes("pageup");
    assert!(offset(cx) > 0, "PageUp did not scroll the followed file");
    cx.simulate_keystrokes("end");
    assert_eq!(offset(cx), 0, "End did not come back to the live screen");
    cx.simulate_keystrokes("shift-home");
    assert!(offset(cx) > 0, "Shift+Home did not reach the top");

    let chord = if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-shift-f"
    };
    cx.simulate_keystrokes(chord);
    let terminal = tail.read_with(cx, |tail, _| tail.terminal_for_test());
    let input = terminal
        .read_with(cx, |view, _| view.search_input_for_test())
        .expect("the search bar did not open");
    input.update(cx, |input, cx| input.set_content("error", cx));
    cx.run_until_parked();
    let (selection, matches) = terminal.read_with(cx, |view, _| view.search_for_test());
    assert_eq!(matches, Some(4), "four lines say ERROR");
    let ((line, start), (end_line, end)) = selection.expect("a match is selected");
    assert_eq!((line, end_line), (line, line));
    assert_eq!(end - start, 4, "the selection is the word");

    cx.simulate_keystrokes("escape");
    let (_, matches) = terminal.read_with(cx, |view, _| view.search_for_test());
    assert_eq!(matches, None, "Escape did not close the bar");
}

/// A paste of several lines into a shell that has not turned on bracketed
/// paste asks first, and `Escape` drops it; one line goes straight through.
#[gpui::test]
fn a_multi_line_paste_asks_before_it_runs(cx: &mut TestAppContext) {
    cx.update(|cx| {
        TerminalView::init(cx);
        bootstrap::bind_shortcuts(cx);
    });
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    let view = workspace.read_with(cx, |workspace, _| {
        match workspace.tabs[workspace.active].active_view() {
            PaneView::Terminal(view) => view.clone(),
            _ => panic!("the tab is not a shell"),
        }
    });

    let asked = view.update(cx, |view, cx| view.paste_for_test("echo one\n", cx));
    assert!(!asked, "a single line asked");
    let asked = view.update(cx, |view, cx| {
        view.paste_for_test("cd /tmp\nrm -rf build\n", cx)
    });
    assert!(asked, "two lines did not ask");
    cx.simulate_keystrokes("escape");
    assert!(
        !view.read_with(cx, |view, _| view.asking_to_paste_for_test()),
        "Escape did not drop the paste"
    );
}

/// The panel chord hands the keyboard to the panel it shows, and hiding the
/// panel hands it back to the pane rather than leaving it on nothing.
#[gpui::test]
fn the_files_panel_takes_the_keyboard_and_gives_it_back(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_local(&workspace, cx);
    let in_panel = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
        workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .panel
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        })
    };

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_file_panel(window, cx)
    });
    assert!(
        in_panel(&workspace, cx),
        "showing the panel left the keyboard behind"
    );
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_file_panel(window, cx)
    });
    assert!(
        !in_panel(&workspace, cx),
        "hiding the panel kept the keyboard in it"
    );
}
