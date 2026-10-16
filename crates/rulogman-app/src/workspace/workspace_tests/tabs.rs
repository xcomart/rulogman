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
    workspace.read_with(cx, |workspace, _| workspace.tab_drop)
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
fn the_drop_lands_where_the_bar_named_or_where_it_fell(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    let dragged = DraggedTab {
        source: SESSION_STRIP_TAG,
        index: 0,
    };
    workspace.update(cx, |workspace, cx| {
        workspace.drag_over_tab(SESSION_STRIP_TAG, dragged, Some(2), cx);
    });
    // The static position loses to the bar the pointer last named.
    assert_eq!(bar(&workspace, cx), Some((SESSION_STRIP_TAG, 2)));
    let landed = workspace.update(cx, |workspace, _| {
        workspace.resolve_tab_drop(SESSION_STRIP_TAG, 0)
    });
    assert_eq!(landed, 2);
    // A drop ends the gesture: the bar is gone either way.
    assert_eq!(bar(&workspace, cx), None);
    // With no bar up, the drop falls where it fell.
    let fell = workspace.update(cx, |workspace, _| {
        workspace.resolve_tab_drop(SESSION_STRIP_TAG, 1)
    });
    assert_eq!(fell, 1);
}

#[gpui::test]
fn a_bar_for_another_strip_is_not_ours_to_land_on(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    workspace.update(cx, |workspace, _| {
        workspace.tab_drop = Some((7, 2));
        // The fallback wins, and the foreign bar is still taken: a drop ends
        // the gesture, wherever it lands.
        assert_eq!(workspace.resolve_tab_drop(SESSION_STRIP_TAG, 1), 1);
        assert_eq!(workspace.tab_drop, None);
    });
}

#[gpui::test]
fn clearing_an_empty_bar_is_silent(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace(cx, false);
    open_remote(&workspace, cx, true);
    workspace.update(cx, |workspace, cx| {
        workspace.clear_tab_drop(cx);
    });
    assert_eq!(bar(&workspace, cx), None);
}
