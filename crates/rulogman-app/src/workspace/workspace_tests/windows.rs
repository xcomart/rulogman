//! The rules a second window brings with it.
//!
//! Three questions, and none of them needs a workspace on screen. Which windows
//! belong to the application is a filter over what gpui holds; where the next
//! one lands is arithmetic on a rectangle; and whether the start-up update check
//! has already run is a flag on the process. Opening a window for real is left
//! out on purpose: [`open_workspace_window`] paints a caption from the widget
//! layer's theme, which a headless test has no reason to install.

use super::*;

use gpui::TestAppContext;
use rulogman_core::AppSettings;

/// A window root that is not a [`Workspace`], so the sweep has something it
/// has to leave out.
struct Bystander;

impl Render for Bystander {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A window on a workspace, on the settings a fresh install starts with.
///
/// The settings go in first for the same reason they do in `main`:
/// everything the workspace builds reads the global.
fn window(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    cx.update(|cx| app_settings::replace(AppSettings::default(), cx));
    cx.add_window(|window, cx| Workspace::new(TitlebarStyle::System, window, cx))
}

#[gpui::test]
fn every_window_of_the_application_is_found_and_nothing_else_is(cx: &mut TestAppContext) {
    let first = window(cx);
    let second = window(cx);
    cx.add_window(|_window, _cx| Bystander);

    let found = cx.update(|cx| workspace_windows(cx));
    assert_eq!(
        found.len(),
        2,
        "the sweep did not answer with exactly the two workspace windows"
    );
    assert!(
        found.contains(&first) && found.contains(&second),
        "the sweep missed one of the two windows it was asked for"
    );
    assert!(
        found
            .iter()
            .all(|handle| handle.window_id() != cx.windows()[2].window_id()),
        "a window whose root is not a workspace came back from the sweep"
    );
}

#[gpui::test]
fn the_start_up_update_check_is_claimed_once(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assert!(
            claim_startup_check(cx),
            "the first window did not take the start-up check"
        );
        assert!(
            !claim_startup_check(cx),
            "a second window asked GitHub over again"
        );
    });
}

#[gpui::test]
fn a_second_window_steps_off_the_one_it_came_from(_cx: &mut TestAppContext) {
    let from = Bounds {
        origin: point(px(100.), px(200.)),
        size: size(px(1100.), px(700.)),
    };
    let next = cascaded(from);
    assert_eq!(
        next.origin,
        point(px(100. + WINDOW_CASCADE), px(200. + WINDOW_CASCADE)),
        "the new window did not step clear of the one it came from"
    );
    assert_eq!(
        next.size, from.size,
        "stepping across the desktop resized the window"
    );
}
