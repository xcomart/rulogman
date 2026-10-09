use super::*;

use gpui::{TestAppContext, VisualTestContext};
use rulogman_core::{AppSettings, AuthMethod, TailRule};

/// A workspace in a window, on settings that say `local_panel` for the
/// shells that follow it.
///
/// The settings go in before the window opens for the same reason they do in
/// `main`: everything the workspace builds reads the global, and a workspace
/// built on one set of settings and asked about another would be answering a
/// question nobody put to it.
fn workspace(
    cx: &mut TestAppContext,
    local_panel: bool,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| set_local_panel(cx, local_panel));
    cx.add_window_view(|window, cx| Workspace::new(TitlebarStyle::System, window, cx))
}

/// Puts `local_panel` into the settings global, leaving the rest at their
/// defaults.
fn set_local_panel(cx: &mut App, open: bool) {
    let mut settings = AppSettings::default();
    settings.files.local_panel = open;
    app_settings::replace(settings, cx);
}

/// A profile that does or does not want the panel beside it.
fn profile_showing_files(show_files: bool) -> SessionProfile {
    let mut profile =
        SessionProfile::new("web-01", "example.com", 22, "alice", AuthMethod::Password);
    profile.show_files = show_files;
    profile
}

/// Gives the workspace a tab on a host whose profile says `show_files`.
///
/// [`Workspace::open_session`] with the connection taken out of it: the same
/// two lines that decide the panel and hand the session over, around a
/// session that never dials the host it names.
fn open_remote(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, show_files: bool) {
    workspace.update_in(cx, |workspace, window, cx| {
        let profile = profile_showing_files(show_files);
        let panel_open = Workspace::panel_opens_for(Some(&profile), cx);
        let session = cx.new(|cx| Session::dormant_remote(profile, cx));
        workspace.adopt_session(session, panel_open, window, cx);
    });
}

/// The same for a shell on this machine, which comes from no profile and is
/// judged by the setting instead.
fn open_local(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        let panel_open = Workspace::panel_opens_for(None, cx);
        let session = cx.new(Session::dormant);
        workspace.adopt_session(session, panel_open, window, cx);
    });
}

/// Gives the workspace a tab following `path`, the way
/// [`Workspace::open_tail_session`] ends.
///
/// The connection is taken out of it exactly as [`open_remote`] takes it
/// out of a shell tab, and for the same reason: what is under test is the
/// pane the workspace builds, not what is on the other end of it. The
/// panel flag is the call's own `false` rather than
/// [`Workspace::panel_opens_for`], since a followed file never asks.
fn open_tail(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, path: &str) {
    workspace.update_in(cx, |workspace, window, cx| {
        let profile = profile_showing_files(true);
        let caps = Workspace::pane_caps_source(cx);
        let session = cx.new(|cx| Session::dormant_tail(profile, path.to_owned(), cx));
        let terminal = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let view = cx.new(|cx| {
            TailView::new(
                terminal,
                session.clone(),
                path.to_owned(),
                SharedString::default(),
                cx,
            )
        });
        let leaf = workspace.new_tail_pane(view, session, window, cx);

        workspace
            .tabs
            .push(SessionTab::single(leaf).with_panel(false));
        workspace.active = workspace.tabs.len() - 1;
        workspace.focus_active(window, cx);
    });
}

/// Gives the workspace a tab for a profile that names `paths` to follow,
/// the way [`Workspace::open_session_with_tails`] ends: one tab, the
/// shell pane plus one tail pane per path, stacked in the order given.
///
/// [`Workspace::open_session_with_tails`] itself dials a real connection
/// for the shell and for every tail, so — as `open_remote` and `open_tail`
/// already do for their own calls — the sessions here are the dormant
/// stand-ins instead. What is under test is
/// [`Workspace::compose_tailed_tab`]'s arrangement of the panes, not what
/// is on the other end of any of them.
fn open_tailed(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, paths: &[&str]) {
    workspace.update_in(cx, |workspace, window, cx| {
        let mut profile = profile_showing_files(true);
        profile.tails = paths.iter().map(|path| TailRule::new(*path)).collect();
        let panel_open = Workspace::panel_opens_for(Some(&profile), cx);
        let caps = Workspace::pane_caps_source(cx);

        let session = cx.new(Session::dormant);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
        let shell_leaf = workspace.new_pane(view, session, window, cx);

        let tail_leaves = paths
            .iter()
            .map(|path| {
                let session =
                    cx.new(|cx| Session::dormant_tail(profile.clone(), (*path).to_owned(), cx));
                let terminal =
                    cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
                let view = cx.new(|cx| {
                    TailView::new(
                        terminal,
                        session.clone(),
                        (*path).to_owned(),
                        SharedString::default(),
                        cx,
                    )
                });
                workspace.new_tail_pane(view, session, window, cx)
            })
            .collect();

        let tab = Workspace::compose_tailed_tab(shell_leaf, tail_leaves, panel_open);
        workspace.tabs.push(tab);
        workspace.active = workspace.tabs.len() - 1;
        workspace.focus_active(window, cx);
    });
}

/// Splits the active tab in two, the way [`Workspace::duplicate_split`] ends.
///
/// Not that call itself: it splits by *duplicating*, and a duplicate starts
/// a second transport — a pty on the machine running the tests, or a TCP
/// connection to a host that does not exist. The half it would have made is
/// put there directly instead, on a session of its own that connects to
/// nothing, because what is under test here is what the tab carries rather
/// than what is on the other end of either pane.
fn split_active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        let session = cx.new(Session::dormant);
        let caps = Workspace::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let leaf = workspace.new_pane(view, session, window, cx);

        let active = workspace.active;
        let tab = &mut workspace.tabs[active];
        let target = tab.active_pane();
        let pane = tab
            .panes
            .split(target, Axis::Horizontal, leaf)
            .expect("the pane to split came out of this tab");
        tab.focus(pane);
    });
}

/// Whether the panel is showing beside the active tab, as both render paths
/// ask it.
fn showing(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| workspace.panel_showing())
}

/// The flag on the tab at `index`, whether or not it is the active one.
fn flag(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, index: usize) -> bool {
    workspace.read_with(cx, |workspace, _| workspace.tabs[index].panel_open)
}

/// Gives the workspace a dashboard tab of `count` followed files under the
/// name `name`, the way [`Workspace::open_dashboard`] ends.
///
/// The lookups and the keychain are taken out of it exactly as
/// [`open_tailed`] takes out the connection, and for the same reason: what
/// is under test is [`Workspace::compose_dashboard_tab`]'s arrangement and
/// the name the tab wears, neither of which is a question about what is on
/// the other end of a pane. One profile for all of them, since the grid is
/// the same grid however many hosts the panes came from.
fn open_dashboard(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    name: &str,
    count: usize,
) {
    workspace.update_in(cx, |workspace, window, cx| {
        let profile = profile_showing_files(true);
        let caps = Workspace::pane_caps_source(cx);
        let leaves = (0..count)
            .map(|index| {
                let path = format!("/var/log/app-{index}.log");
                let session = cx.new(|cx| Session::dormant_tail(profile.clone(), path.clone(), cx));
                let terminal =
                    cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
                let view = cx.new(|cx| {
                    TailView::new(
                        terminal,
                        session.clone(),
                        path,
                        SharedString::from(profile.name.clone()),
                        cx,
                    )
                });
                workspace.new_tail_pane(view, session, window, cx)
            })
            .collect();

        let tab = Workspace::compose_dashboard_tab(leaves, false).with_label(name.to_owned());
        workspace.tabs.push(tab);
        workspace.active = workspace.tabs.len() - 1;
        workspace.focus_active(window, cx);
    });
}

/// The active tab's shape: how many panes it holds, and how many of its
/// dividers run each way.
///
/// Rows and columns are not stored anywhere — the tree is binary — so the
/// grid is asserted through the two counts, which pin it down between them:
/// `r` rows of `c` columns is `r - 1` splits along [`Axis::Vertical`] and
/// one per cell past the first of every row along [`Axis::Horizontal`].
fn grid(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (usize, usize, usize) {
    workspace.read_with(cx, |workspace, _| {
        let panes = &workspace.tabs[workspace.active].panes;
        (
            panes.leaf_count(),
            panes.splits_along(Axis::Vertical),
            panes.splits_along(Axis::Horizontal),
        )
    })
}

/// Whether the active tab hands the keyboard to its top-left pane.
fn first_pane_is_active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        let tab = &workspace.tabs[workspace.active];
        tab.active_pane() == tab.panes.first_leaf().0
    })
}

/// Brings the tab at `index` to the front.
fn select(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, index: usize) {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_tab(index, window, cx);
    });
}

/// A [`LayoutNode::Split`], spelled out so the layout tests read as trees.
fn layout_split(axis: LayoutAxis, ratio: f32, first: LayoutNode, second: LayoutNode) -> LayoutNode {
    LayoutNode::Split {
        axis,
        ratio,
        first: Box::new(first),
        second: Box::new(second),
    }
}

/// A dashboard of `count` followed files named `/var/log/app-N.log`, each on
/// a profile of its own, optionally carrying a saved `layout`.
fn dashboard_of(name: &str, count: usize, layout: Option<LayoutNode>) -> Dashboard {
    let mut dashboard = Dashboard::new(name);
    for index in 0..count {
        dashboard.panes.push(DashboardPane {
            profile: Uuid::new_v4(),
            path: format!("/var/log/app-{index}.log"),
        });
    }
    dashboard.layout = layout;
    dashboard
}

/// Opens `dashboard` into a tab the way [`Workspace::open_dashboard`] ends,
/// making the same layout-or-grid decision the real opener makes and taking
/// the lookups and the keychain out for the reason [`open_dashboard`] does.
///
/// Each pane's session carries the very profile id and path the dashboard
/// names, so what [`Workspace::capture_tab_layout`] reads back off the tab is
/// exactly what went in. The dashboard is also placed in the store, so the
/// opened tab's [`SessionTab::dashboard`] resolves to a real entry.
fn open_dashboard_tab(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    dashboard: &Dashboard,
) {
    workspace.update_in(cx, |workspace, window, cx| {
        let caps = Workspace::pane_caps_source(cx);
        let leaves: Vec<PaneLeaf> = dashboard
            .panes
            .iter()
            .map(|pane| {
                let mut profile = profile_showing_files(true);
                profile.id = pane.profile;
                let path = pane.path.clone();
                let session = cx.new(|cx| Session::dormant_tail(profile.clone(), path.clone(), cx));
                let terminal =
                    cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
                let view = cx.new(|cx| {
                    TailView::new(
                        terminal,
                        session.clone(),
                        path,
                        SharedString::from(profile.name.clone()),
                        cx,
                    )
                });
                workspace.new_tail_pane(view, session, window, cx)
            })
            .collect();

        let tab = match dashboard.valid_layout() {
            Some(layout) if leaves.len() == dashboard.panes.len() => {
                Workspace::compose_dashboard_layout(leaves, layout, false)
            }
            _ => Workspace::compose_dashboard_tab(leaves, false),
        }
        .with_label(dashboard.name.clone())
        .with_dashboard(dashboard.id);

        workspace.dashboards.upsert(dashboard.clone());
        workspace.tabs.push(tab);
        workspace.active = workspace.tabs.len() - 1;
        workspace.focus_active(window, cx);
    });
}

/// The followed paths of the active tab's panes, in depth-first layout
/// order, for asserting where each leaf landed.
fn leaf_paths(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.tabs[workspace.active]
            .panes
            .leaves()
            .into_iter()
            .map(|(_, leaf)| {
                leaf.view
                    .session(cx)
                    .and_then(|session| session.read(cx).tail_path().map(|path| path.to_owned()))
                    .expect("a dashboard pane is a followed file")
            })
            .collect()
    })
}

/// A store of dashboards named `names`, in that order, with the ones whose
/// name appears in `marked` flagged to open at start-up.
///
/// Ids are the store's own, so the assertions below have to go back through
/// the store to name what they expect — which is the point: what
/// [`startup_dashboards`] answers with is ids, and a name is only ever the
/// way in.
fn dashboard_store(names: &[&str], marked: &[&str]) -> DashboardStore {
    let mut store = DashboardStore::default();
    for name in names {
        let mut dashboard = Dashboard::new(*name);
        dashboard.open_at_startup = marked.contains(name);
        store.upsert(dashboard);
    }
    store
}

/// The id of the first dashboard called `name`, which is the one a request
/// for that name resolves to.
fn dashboard_id(store: &DashboardStore, name: &str) -> Uuid {
    store
        .dashboards()
        .iter()
        .find(|dashboard| dashboard.name == name)
        .expect("the fixture has no such dashboard")
        .id
}

/// A second window on a workspace of its own, for the tab moves below.
///
/// [`Workspace::move_tab_to_new_window`] itself is left untested for the
/// reason [`window_tests`] gives: opening a window for real paints a caption
/// from the widget layer's theme, which a headless test has no reason to
/// install. Its two halves are the whole of what it does, and they are what
/// is asserted on here.
fn second_window(cx: &mut VisualTestContext) -> WindowHandle<Workspace> {
    cx.add_window(|window, cx| Workspace::new(TitlebarStyle::System, window, cx))
}

/// The terminal view of the first pane of the tab at `index`.
fn terminal_of(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    index: usize,
) -> Entity<TerminalView> {
    workspace.read_with(cx, |workspace, _| {
        match &workspace.tabs[index].panes.first_leaf().1.view {
            PaneView::Terminal(view) => view.clone(),
            _ => unreachable!("the tab was opened as a shell"),
        }
    })
}

/// The panes of the active tab, in layout order.
fn pane_ids(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<PaneId> {
    workspace.read_with(cx, |workspace, _| {
        workspace.tabs[workspace.active].panes.leaf_ids()
    })
}

/// Which pane of the active tab holds the keyboard.
fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> PaneId {
    workspace.read_with(cx, |workspace, _| {
        workspace.tabs[workspace.active].active_pane()
    })
}

/// Moves the marker onto `pane` and records the visit, the way a click in
/// that pane does.
///
/// The focus tree itself is left out of it: what is under test is the order
/// the tab writes down, and `on_pane_focused` — which is what a real click
/// arrives through — does exactly this and nothing else that matters here.
fn focus_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, pane: PaneId) {
    workspace.update(cx, |workspace, _| {
        let active = workspace.active;
        workspace.tabs[active].focus(pane);
    });
}

/// Every divider of the active tab, outermost first, first child before
/// second.
fn ratios(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<f32> {
    fn walk(node: &PaneNode<PaneLeaf>, out: &mut Vec<f32>) {
        if let PaneNode::Split {
            ratio,
            first,
            second,
            ..
        } = node
        {
            out.push(*ratio);
            walk(first, out);
            walk(second, out);
        }
    }

    workspace.read_with(cx, |workspace, _| {
        let mut out = Vec::new();
        walk(workspace.tabs[workspace.active].panes.root(), &mut out);
        out
    })
}

mod dashboards;
mod panes;
