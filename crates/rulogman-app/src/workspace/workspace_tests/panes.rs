use super::*;

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

    workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
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
    workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
    assert!(showing(&workspace, cx));
}

#[gpui::test]
fn the_welcome_screen_has_no_panel_and_nothing_to_toggle(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, true);
    assert!(
        !showing(&workspace, cx),
        "a window with nothing open drew the panel beside the welcome screen"
    );

    // The guard the greyed-out menu row and the missing toolbar button agree
    // with: there is no tab to write the answer on, so the shortcut does
    // nothing rather than panicking or arming a panel nothing can draw.
    workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
    assert!(!showing(&workspace, cx));
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
    workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
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
