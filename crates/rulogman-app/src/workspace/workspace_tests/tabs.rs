//! Reordering the tab strip by drag, and the drop bar naming the landing.
//!
//! A reorder moves a whole tab — sessions, panes and all — so what is
//! asserted is the strip order and where the focus sits in it, never what any
//! tab shows. The tabs themselves are told apart by [`SessionTab::id`], which
//! is what the strip element is built with: titles repeat across tabs on the
//! same profile, and an index would name a different tab after the move.

use super::*;

/// The strip's tabs, front to back, by the identity the strip draws them with.
fn order(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<u64> {
    workspace.read_with(cx, |workspace, _| {
        workspace.tabs.iter().map(|tab| tab.id).collect()
    })
}

/// Which tab the focus is on.
fn active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| workspace.active)
}

/// The drop bar, if one is up.
fn bar(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<(u64, usize)> {
    workspace.read_with(cx, |workspace, _| workspace.tab_drop.current())
}

/// A drag of the session tab that stood at `index` when it began.
fn session_drag(index: usize) -> DraggedTab {
    DraggedTab {
        source: SESSION_STRIP_TAG,
        index,
    }
}

/// Three tabs, the first one active.
fn three_tabs(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext, Vec<u64>) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    open_remote(&workspace, cx, true);
    open_remote(&workspace, cx, true);
    select(&workspace, cx, 0);
    let before = order(&workspace, cx);
    (workspace, cx, before)
}

#[gpui::test]
fn moving_the_active_tab_carries_the_focus_with_it(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    workspace.update(cx, |workspace, cx| workspace.move_tab(0, 3, cx));
    assert_eq!(order(&workspace, cx), vec![before[1], before[2], before[0]]);
    assert_eq!(active(&workspace, cx), 2);
}

#[gpui::test]
fn moving_a_background_tab_leaves_the_focus_on_its_tab(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    select(&workspace, cx, 2);
    workspace.update(cx, |workspace, cx| workspace.move_tab(0, 3, cx));
    assert_eq!(order(&workspace, cx), vec![before[1], before[2], before[0]]);
    // The tab that was third is second now, and still the one in focus.
    assert_eq!(active(&workspace, cx), 1);
    assert_eq!(order(&workspace, cx)[1], before[2]);
}

#[gpui::test]
fn moving_a_tab_back_counts_down_from_where_it_stood(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    workspace.update(cx, |workspace, cx| workspace.move_tab(2, 0, cx));
    assert_eq!(order(&workspace, cx), vec![before[2], before[0], before[1]]);
    // The focused tab rode one slot down with everything behind the landing.
    assert_eq!(active(&workspace, cx), 1);
    assert_eq!(order(&workspace, cx)[1], before[0]);
}

#[gpui::test]
fn a_move_that_changes_nothing_is_refused_silently(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    select(&workspace, cx, 1);
    workspace.update(cx, |workspace, cx| {
        // Onto itself, and into the gap beside itself: the strip as drawn.
        workspace.move_tab(1, 1, cx);
        workspace.move_tab(1, 2, cx);
        workspace.move_tab(0, 1, cx);
        // Past either end of the strip.
        workspace.move_tab(0, 4, cx);
        workspace.move_tab(4, 0, cx);
    });
    assert_eq!(order(&workspace, cx), before);
    assert_eq!(active(&workspace, cx), 1);
}

#[gpui::test]
fn tabs_keep_their_strip_identity_across_a_reorder(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    assert!(
        before[0] != before[1] && before[1] != before[2] && before[0] != before[2],
        "two tabs share a strip identity: {before:?}"
    );
    workspace.update(cx, |workspace, cx| workspace.move_tab(0, 3, cx));
    // The identities rode along with their tabs rather than staying in place.
    assert_eq!(order(&workspace, cx), vec![before[1], before[2], before[0]]);
}

#[gpui::test]
fn the_drop_bar_tracks_a_session_drag_over_its_own_strip(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    let dragged = DraggedTab {
        source: SESSION_STRIP_TAG,
        index: 0,
    };
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, dragged, Some(2), cx);
    });
    assert_eq!(bar(&workspace, cx), Some((SESSION_STRIP_TAG, 2)));

    // The pointer leaving the strip takes the bar down with it.
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, dragged, None, cx);
    });
    assert_eq!(bar(&workspace, cx), None);
}

#[gpui::test]
fn a_foreign_tab_never_bars_a_session_drag(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    let dragged = DraggedTab {
        source: 7,
        index: 0,
    };
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, dragged, Some(1), cx);
    });
    assert_eq!(bar(&workspace, cx), None);
}

#[gpui::test]
fn leaving_another_strip_leaves_this_strips_bar_alone(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    let dragged = DraggedTab {
        source: SESSION_STRIP_TAG,
        index: 0,
    };
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, dragged, Some(2), cx);
        // The pointer left some other strip, not this one.
        workspace.drag_over_tab(7, dragged, None, cx);
    });
    assert_eq!(bar(&workspace, cx), Some((SESSION_STRIP_TAG, 2)));
}

#[gpui::test]
fn a_drop_lands_in_the_gap_it_names_and_ends_the_drag(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, session_drag(0), Some(3), cx);
    });
    assert_eq!(bar(&workspace, cx), Some((SESSION_STRIP_TAG, 3)));
    workspace.update(cx, |workspace, cx| {
        workspace.drop_tab(session_drag(0), 3, cx);
    });
    assert_eq!(order(&workspace, cx), vec![before[1], before[2], before[0]]);
    // A drop ends the gesture: neither the bar nor the held tab outlives it.
    assert_eq!(bar(&workspace, cx), None);
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.held_tab, None));
}

#[gpui::test]
fn the_dragged_tab_moves_even_when_the_strip_changed_under_the_drag(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    // The drag picks up the third tab...
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, session_drag(2), Some(1), cx);
    });
    // ...and the first one closes while it is in the air — its session ended —
    // so index 2, the one the payload still names, is past the end now.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_tab_now(0, window, cx);
    });
    workspace.update(cx, |workspace, cx| {
        workspace.drop_tab(session_drag(2), 0, cx);
    });
    assert_eq!(order(&workspace, cx), vec![before[2], before[1]]);
}

#[gpui::test]
fn a_tab_that_closed_mid_drag_is_not_replaced_by_its_neighbor(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, session_drag(0), Some(2), cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_tab_now(0, window, cx);
    });
    // Index 0 now names the tab that was second; dropping must not move it.
    workspace.update(cx, |workspace, cx| {
        workspace.drop_tab(session_drag(0), 2, cx);
    });
    assert_eq!(order(&workspace, cx), vec![before[1], before[2]]);
    assert_eq!(bar(&workspace, cx), None);
}

#[gpui::test]
fn a_reorder_with_no_drag_behind_it_takes_the_index_as_it_is(cx: &mut TestAppContext) {
    // The keyboard's Ctrl+Shift+PageDown arrives as a drop no hover preceded.
    let (workspace, cx, before) = three_tabs(cx);
    workspace.update(cx, |workspace, cx| {
        workspace.drop_tab(session_drag(0), 2, cx);
    });
    assert_eq!(order(&workspace, cx), vec![before[1], before[0], before[2]]);
}

#[gpui::test]
fn a_foreign_tab_dropped_here_moves_nothing_and_takes_the_bar_down(cx: &mut TestAppContext) {
    let (workspace, cx, before) = three_tabs(cx);
    let foreign = DraggedTab {
        source: 7,
        index: 0,
    };
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, session_drag(1), Some(2), cx);
        workspace.drop_tab(foreign, 1, cx);
    });
    assert_eq!(order(&workspace, cx), before);
    assert_eq!(bar(&workspace, cx), None);
}

#[gpui::test]
fn a_canceled_drag_lets_go_of_its_tab(cx: &mut TestAppContext) {
    let (workspace, cx, _before) = three_tabs(cx);
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, session_drag(1), Some(0), cx);
        // Released outside every drop target: only the root hears it.
        workspace.end_tab_drag(cx);
    });
    assert_eq!(bar(&workspace, cx), None);
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.held_tab, None));
}

#[gpui::test]
fn clearing_an_empty_bar_is_silent(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    workspace.update(cx, |workspace, cx| {
        workspace.end_tab_drag(cx);
    });
    assert_eq!(bar(&workspace, cx), None);
}
