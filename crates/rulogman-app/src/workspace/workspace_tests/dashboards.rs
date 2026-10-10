use super::*;

#[gpui::test]
fn a_followed_file_is_a_session_tab_that_wants_no_panel(cx: &mut TestAppContext) {
    // The setting says "open the panel", and so does the profile the tail
    // is opened from: a followed file has to refuse it whatever either of
    // them says, there being no shell on the other end to browse a
    // filesystem beside — and [`Session::files`] answering nothing for such
    // a session, so a panel here would sit empty for good.
    let (workspace, cx) = workspace(cx, true);
    open_tail(&workspace, cx, "/var/log/nginx/access.log");

    assert!(
        !showing(&workspace, cx),
        "a followed file opened the file panel"
    );

    // It is a session like any other, which is the answer every rule about
    // a pane is written against: the tab strip's label and status dot, the
    // status bar, the disconnect that retires the pane, the reconnect.
    let session = workspace
        .read_with(cx, |workspace, cx| {
            workspace.tabs[workspace.active].active_session(cx)
        })
        .expect("a tail pane did not answer as a session");

    // And it is named after the file, not after the connection: two logs on
    // one host would otherwise wear the same label.
    assert_eq!(
        session.read_with(cx, |session, _| session.title()),
        SharedString::from("access.log - web-01")
    );
}

#[gpui::test]
fn a_profile_with_tails_gets_one_tab_with_the_shell_on_top(cx: &mut TestAppContext) {
    // Two rules, so the arrangement has to be told apart from "a tail pane
    // happened to land somewhere" — three leaves in all, and nowhere else
    // for the other two to have gone but this one tab.
    let (workspace, cx) = workspace(cx, true);
    open_tailed(
        &workspace,
        cx,
        &["/var/log/nginx/access.log", "/var/log/nginx/error.log"],
    );

    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.tabs.len()),
        1,
        "the tail rules opened tabs of their own instead of joining the shell's"
    );

    let leaf_count = workspace.read_with(cx, |workspace, _| {
        workspace.tabs[workspace.active].panes.leaf_count()
    });
    assert_eq!(
        leaf_count, 3,
        "expected the shell pane plus one pane per tail rule"
    );

    // The shell, not either tail, is what the tab hands the keyboard to on
    // arrival: a rule nobody has looked at yet has nothing to answer a
    // keypress with.
    let active_is_shell = workspace.read_with(cx, |workspace, _| {
        matches!(
            workspace.tabs[workspace.active].active_view(),
            PaneView::Terminal(_)
        )
    });
    assert!(
        active_is_shell,
        "a tail pane held the active pane instead of the shell"
    );
}

#[gpui::test]
fn a_dashboard_of_two_files_puts_them_side_by_side(cx: &mut TestAppContext) {
    // Two panes are one row of two columns, not a column of two: a log is
    // read across, and halving the width of a terminal costs less than
    // halving the number of lines of it that are on screen.
    let (workspace, cx) = workspace(cx, true);
    open_dashboard(&workspace, cx, "Deploy watch", 2);

    assert_eq!(
        grid(&workspace, cx),
        (2, 0, 1),
        "two files did not open as one row of two"
    );

    // The setting says "open the panel" and so does the profile behind
    // every pane; a dashboard refuses it for the reason a single followed
    // file refuses it, there being no shell here to browse a filesystem
    // beside.
    assert!(
        !showing(&workspace, cx),
        "a dashboard opened the file panel"
    );
    assert!(
        first_pane_is_active(&workspace, cx),
        "a dashboard handed the keyboard to something other than its first pane"
    );
}

#[gpui::test]
fn a_dashboard_of_three_files_fills_the_top_row_first(cx: &mut TestAppContext) {
    // Three into two columns: a full row and a short one. Which of the two
    // rows is the short one is the whole of what "row-major" means here, so
    // the tree itself is read rather than only the divider counts — those
    // would say the same thing about a dashboard that had filled the bottom
    // row and left a gap at the top.
    let (workspace, cx) = workspace(cx, true);
    open_dashboard(&workspace, cx, "Deploy watch", 3);

    assert_eq!(
        grid(&workspace, cx),
        (3, 1, 1),
        "three files did not open as two rows of at most two"
    );

    let short_row_last = workspace.read_with(cx, |workspace, _| {
        match workspace.tabs[workspace.active].panes.root() {
            PaneNode::Split {
                axis: Axis::Vertical,
                first,
                second,
                ..
            } => {
                matches!(
                    **first,
                    PaneNode::Split {
                        axis: Axis::Horizontal,
                        ..
                    }
                ) && matches!(**second, PaneNode::Leaf { .. })
            }
            _ => false,
        }
    });
    assert!(
        short_row_last,
        "the row with room to spare was not the bottom one"
    );
}

#[gpui::test]
fn a_dashboard_of_four_files_opens_two_by_two(cx: &mut TestAppContext) {
    // The case the whole shape exists for: four panes are a square, not a
    // stack of four and not a row of four.
    let (workspace, cx) = workspace(cx, true);
    open_dashboard(&workspace, cx, "Deploy watch", 4);

    assert_eq!(
        grid(&workspace, cx),
        (4, 1, 2),
        "four files did not open two by two"
    );
    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.tabs.len()),
        1,
        "the dashboard's files opened tabs of their own instead of one tab"
    );
}

#[gpui::test]
fn a_dashboard_tab_is_named_after_the_dashboard(cx: &mut TestAppContext) {
    // Not after whichever pane holds the keyboard, which is what every
    // other tab is named after: a dashboard is a named arrangement, and a
    // strip that renamed it as the focus moved would be reporting on the
    // wrong thing.
    let (workspace, cx) = workspace(cx, true);
    open_dashboard(&workspace, cx, "Deploy watch", 4);

    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.tabs[workspace.active]
            .label
            .clone()),
        Some(SharedString::from("Deploy watch"))
    );

    // And the name it is *not* wearing is a real one: the active pane has a
    // session with a title of its own, which is what the strip would have
    // used had the tab carried no name.
    let title = workspace
        .read_with(cx, |workspace, cx| {
            workspace.tabs[workspace.active].active_session(cx)
        })
        .expect("a dashboard pane did not answer as a session")
        .read_with(cx, |session, _| session.title());
    assert_eq!(title, SharedString::from("app-0.log - web-01"));
}

#[test]
fn the_marked_dashboards_open_at_startup_in_saved_order() {
    let store = dashboard_store(&["morning", "deploy", "night"], &["night", "morning"]);

    assert_eq!(
        startup_dashboards(&store, &[]),
        vec![
            dashboard_id(&store, "morning"),
            dashboard_id(&store, "night")
        ]
    );
}

#[test]
fn a_dashboard_named_on_the_command_line_opens_after_the_marked_ones() {
    let store = dashboard_store(&["morning", "deploy", "night"], &["morning"]);

    assert_eq!(
        startup_dashboards(&store, &["night".to_owned(), "deploy".to_owned()]),
        vec![
            dashboard_id(&store, "morning"),
            dashboard_id(&store, "night"),
            dashboard_id(&store, "deploy"),
        ]
    );
}

#[test]
fn a_dashboard_both_marked_and_named_opens_once() {
    let store = dashboard_store(&["morning", "deploy"], &["morning"]);

    assert_eq!(
        startup_dashboards(&store, &["morning".to_owned(), "morning".to_owned()]),
        vec![dashboard_id(&store, "morning")]
    );
}

#[test]
fn a_name_no_dashboard_answers_to_is_skipped() {
    let store = dashboard_store(&["morning"], &[]);

    assert!(startup_dashboards(&store, &["Morning".to_owned()]).is_empty());
    assert!(startup_dashboards(&store, &["morning ".to_owned()]).is_empty());
    assert_eq!(
        startup_dashboards(&store, &["gone".to_owned(), "morning".to_owned()]),
        vec![dashboard_id(&store, "morning")]
    );
}

#[test]
fn a_launch_that_asks_for_nothing_opens_no_dashboard() {
    assert!(startup_dashboards(&dashboard_store(&["morning"], &[]), &[]).is_empty());
    assert!(startup_dashboards(&DashboardStore::default(), &["morning".to_owned()]).is_empty());
}

#[test]
fn two_dashboards_of_one_name_are_reached_by_the_first() {
    // Names are not unique — identity in the store is the id — so the
    // command line can only ever name the first of them, which is the one
    // the welcome screen lists first too.
    let store = dashboard_store(&["morning", "morning"], &[]);

    assert_eq!(
        startup_dashboards(&store, &["morning".to_owned()]),
        vec![store.dashboards()[0].id]
    );
}

#[gpui::test]
fn a_dashboard_restores_its_saved_layout(cx: &mut TestAppContext) {
    // A three-pane arrangement with real shape: pane 0 fills the top, and
    // the two below it share a row. The tree, the axis at each split and the
    // ratio the divider sits at all have to come back exactly, or the saved
    // geometry was not honoured.
    let (workspace, cx) = workspace(cx, true);
    let layout = layout_split(
        LayoutAxis::Vertical,
        0.3,
        LayoutNode::Leaf { pane: 0 },
        layout_split(
            LayoutAxis::Horizontal,
            0.6,
            LayoutNode::Leaf { pane: 1 },
            LayoutNode::Leaf { pane: 2 },
        ),
    );
    let dashboard = dashboard_of("Tuned", 3, Some(layout));
    open_dashboard_tab(&workspace, cx, &dashboard);

    workspace.read_with(cx, |workspace, _| {
        let PaneNode::Split {
            axis: Axis::Vertical,
            ratio,
            first,
            second,
            ..
        } = workspace.tabs[workspace.active].panes.root()
        else {
            panic!("the root was not the saved vertical split");
        };
        assert!((*ratio - 0.3).abs() < 1e-6, "the top divider moved");
        assert!(
            matches!(**first, PaneNode::Leaf { .. }),
            "the top of the split was not a single pane"
        );
        let PaneNode::Split {
            axis: Axis::Horizontal,
            ratio,
            first,
            second,
            ..
        } = &**second
        else {
            panic!("the bottom of the split was not a horizontal split");
        };
        assert!((*ratio - 0.6).abs() < 1e-6, "the lower divider moved");
        assert!(
            matches!(**first, PaneNode::Leaf { .. }) && matches!(**second, PaneNode::Leaf { .. }),
            "the lower split did not hold two panes"
        );
    });

    // And the panes landed in the order the leaves named them: 0 on top,
    // then 1 and 2 across the row below.
    assert_eq!(
        leaf_paths(&workspace, cx),
        vec![
            "/var/log/app-0.log".to_owned(),
            "/var/log/app-1.log".to_owned(),
            "/var/log/app-2.log".to_owned(),
        ],
        "the panes did not restore in their saved positions"
    );
    assert!(
        first_pane_is_active(&workspace, cx),
        "a restored dashboard handed the keyboard to something other than its first pane"
    );
}

#[gpui::test]
fn a_tab_layout_round_trips_through_the_store(cx: &mut TestAppContext) {
    // Build a tab from a known arrangement, read it back the way
    // *Save layout* does, and confirm the pair a dashboard is stored as
    // comes out matching: the panes in depth-first order and the very tree
    // that built the tab.
    let (workspace, cx) = workspace(cx, true);
    let layout = layout_split(
        LayoutAxis::Vertical,
        0.3,
        LayoutNode::Leaf { pane: 0 },
        layout_split(
            LayoutAxis::Horizontal,
            0.6,
            LayoutNode::Leaf { pane: 1 },
            LayoutNode::Leaf { pane: 2 },
        ),
    );
    let dashboard = dashboard_of("Tuned", 3, Some(layout.clone()));
    open_dashboard_tab(&workspace, cx, &dashboard);

    let (panes, captured) = workspace
        .read_with(cx, |workspace, cx| {
            Workspace::capture_tab_layout(&workspace.tabs[workspace.active], cx)
        })
        .expect("every pane of a dashboard tab is a followed file");

    // Depth-first order: 0, 1, 2 — the same order the leaves were named in.
    assert_eq!(
        panes
            .iter()
            .map(|pane| pane.path.clone())
            .collect::<Vec<_>>(),
        vec![
            "/var/log/app-0.log".to_owned(),
            "/var/log/app-1.log".to_owned(),
            "/var/log/app-2.log".to_owned(),
        ]
    );
    // And each pane kept the profile the dashboard named it on.
    assert_eq!(
        panes.iter().map(|pane| pane.profile).collect::<Vec<_>>(),
        dashboard
            .panes
            .iter()
            .map(|pane| pane.profile)
            .collect::<Vec<_>>()
    );
    // The tree is the one that built the tab, ratios and all.
    assert_eq!(captured, layout);

    // What *Save layout* writes minus the disk: the captured pair upserted
    // over the stored dashboard and read straight back.
    let mut store = DashboardStore::default();
    let mut updated = dashboard.clone();
    updated.panes = panes;
    updated.layout = Some(captured);
    store.upsert(updated);
    let stored = store
        .get(dashboard.id)
        .expect("the dashboard is in the store");
    assert_eq!(stored.layout, Some(layout));
}

#[gpui::test]
fn saving_a_tab_layout_writes_the_new_arrangement_to_the_store_file(cx: &mut TestAppContext) {
    // The whole of *Save layout*, disk included: open a dashboard on a known
    // geometry, drag one divider, invoke the command, and read the file back
    // off the filesystem rather than out of the store that wrote it.
    //
    // The real configuration directory is never in play, and by construction
    // rather than by assertion: the workspace's store is replaced with one
    // built by `DashboardStore::at` over a temporary directory, so
    // `save_tab_layout`'s `self.dashboards.save()` has nowhere else it could
    // land. (`Workspace::new` already starts a test build on an empty store
    // rather than reading the config file, so nothing is read either.)
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dashboards.json");

    let (workspace, cx) = workspace(cx, true);
    let saved = layout_split(
        LayoutAxis::Vertical,
        0.3,
        LayoutNode::Leaf { pane: 0 },
        layout_split(
            LayoutAxis::Horizontal,
            0.6,
            LayoutNode::Leaf { pane: 1 },
            LayoutNode::Leaf { pane: 2 },
        ),
    );
    let dashboard = dashboard_of("Tuned", 3, Some(saved));

    // Before the tab is opened: `open_dashboard_tab` upserts the dashboard
    // into whatever store the workspace holds, and this is the store it must
    // land in.
    workspace.update(cx, |workspace, _| {
        workspace.dashboards = DashboardStore::at(&path);
    });
    open_dashboard_tab(&workspace, cx, &dashboard);

    // Drag the outer divider, the way `Splitter` reports one being dropped.
    let root_split = workspace.read_with(cx, |workspace, _| {
        match workspace.tabs[workspace.active].panes.root() {
            PaneNode::Split { id, .. } => *id,
            PaneNode::Leaf { .. } => panic!("a three-pane dashboard opened as a single pane"),
        }
    });
    let index = workspace.read_with(cx, |workspace, _| workspace.active);
    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.set_split_ratio(root_split, 0.75, cx);
        workspace.save_tab_layout(index, cx);
    });

    // Off the disk, through the same reader the application starts with.
    let stored = DashboardStore::load_from(&path).expect("the store file was written");
    assert_eq!(
        stored.len(),
        1,
        "the save did not write exactly one dashboard"
    );
    let stored = stored
        .get(dashboard.id)
        .expect("the saved layout landed on the dashboard it was opened from");
    assert_eq!(
        stored.name, "Tuned",
        "the dashboard was renamed by the save"
    );

    // The dragged divider is what came back, with the untouched inner one
    // still where the opener put it.
    assert_eq!(
        stored.layout,
        Some(layout_split(
            LayoutAxis::Vertical,
            0.75,
            LayoutNode::Leaf { pane: 0 },
            layout_split(
                LayoutAxis::Horizontal,
                0.6,
                LayoutNode::Leaf { pane: 1 },
                LayoutNode::Leaf { pane: 2 },
            ),
        )),
        "the file does not hold the arrangement that was on screen"
    );

    // And the panes are in depth-first layout order, which is the order the
    // leaf indices above are counted in.
    assert_eq!(
        stored
            .panes
            .iter()
            .map(|pane| pane.path.clone())
            .collect::<Vec<_>>(),
        vec![
            "/var/log/app-0.log".to_owned(),
            "/var/log/app-1.log".to_owned(),
            "/var/log/app-2.log".to_owned(),
        ]
    );
    assert_eq!(
        stored
            .panes
            .iter()
            .map(|pane| pane.profile)
            .collect::<Vec<_>>(),
        dashboard
            .panes
            .iter()
            .map(|pane| pane.profile)
            .collect::<Vec<_>>(),
        "a pane lost the connection the dashboard named it on"
    );
    // The written geometry is one the opener would honour again.
    assert!(
        stored.valid_layout().is_some(),
        "the saved layout does not match its own pane list"
    );
}

#[gpui::test]
fn a_dashboard_without_a_layout_opens_as_a_grid(cx: &mut TestAppContext) {
    // No saved geometry is the ordinary state, and it must lay out as the
    // fresh grid `compose_dashboard_tab` builds: three panes are two rows of
    // at most two, one split each way.
    let (workspace, cx) = workspace(cx, true);
    let dashboard = dashboard_of("Plain", 3, None);
    open_dashboard_tab(&workspace, cx, &dashboard);

    assert_eq!(
        grid(&workspace, cx),
        (3, 1, 1),
        "a layout-less dashboard did not open as the grid"
    );
}

#[gpui::test]
fn a_drifted_layout_falls_back_to_the_grid(cx: &mut TestAppContext) {
    // A layout that no longer matches its panes — here it names only two of
    // the three — is caught by `valid_layout` and the grid takes over, so a
    // pane edit that outdated the geometry costs nothing but the tuning.
    let (workspace, cx) = workspace(cx, true);
    let stale = layout_split(
        LayoutAxis::Horizontal,
        0.5,
        LayoutNode::Leaf { pane: 0 },
        LayoutNode::Leaf { pane: 1 },
    );
    let dashboard = dashboard_of("Drifted", 3, Some(stale));
    open_dashboard_tab(&workspace, cx, &dashboard);

    assert_eq!(
        grid(&workspace, cx),
        (3, 1, 1),
        "a drifted layout was honoured instead of falling back to the grid"
    );
}

#[gpui::test]
fn a_tab_opens_with_the_panel_its_own_profile_asked_for(cx: &mut TestAppContext) {
    // The setting says the opposite of both profiles throughout: it speaks
    // for the sessions no profile speaks for, and must not get a vote on the
    // ones that have one.
    let (workspace, cx) = workspace(cx, false);

    open_remote(&workspace, cx, true);
    assert!(
        showing(&workspace, cx),
        "a host whose profile asks for the panel opened without it"
    );

    open_remote(&workspace, cx, false);
    assert!(
        !showing(&workspace, cx),
        "a host whose profile refuses the panel opened with it anyway"
    );

    // And the first tab is untouched by the second having opened: the flag
    // is the tab's, so switching back shows what that tab was given.
    assert!(flag(&workspace, cx, 0));
    select(&workspace, cx, 0);
    assert!(showing(&workspace, cx));
}
