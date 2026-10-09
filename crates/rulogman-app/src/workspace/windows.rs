//! Windows.

use super::*;

/// Opens a window on a workspace of its own, and hands back its handle.
///
/// Every window comes through here — the one the launch opens and every one
/// *New window* opens after it — so a second window is a first window in every
/// respect. The settings are read afresh on each call rather than captured
/// once, which is what lets a window opened after a visit to the settings
/// dialog arrive already wearing the title bar and the translucency the user
/// chose, instead of the ones the process started on.
pub(super) fn open_workspace_window(cx: &mut App) -> anyhow::Result<WindowHandle<Workspace>> {
    let bounds = new_window_bounds(cx);
    open_workspace_window_at(bounds, cx)
}

/// [`open_workspace_window`] with the placement already decided.
///
/// The split exists for the one caller that knows where its window goes and
/// cannot use [`new_window_bounds`] to find out: moving a tab out is dispatched
/// from inside the window it steps off, which gpui lifts off its own map for the
/// length of a dispatch, so that window cannot be asked for its bounds through a
/// handle — but it is right there as an argument. See
/// [`Workspace::move_tab_to_new_window`].
pub(super) fn open_workspace_window_at(
    bounds: Bounds<Pixels>,
    cx: &mut App,
) -> anyhow::Result<WindowHandle<Workspace>> {
    let settings = app_settings::current(cx);
    // Read once, here: this is only the state the window opens in. Changing the
    // setting later reaches the open window rather than waiting for the next
    // launch — [`Workspace::apply_settings`] hands it to
    // `set_titlebar_transparent` on Windows and macOS, and to
    // `request_decorations` on the Linux backends, which is why nothing here
    // tells the user to restart.
    let titlebar = settings.window.titlebar;
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("rulogman".into()),
                appears_transparent: titlebar == TitlebarStyle::Custom,
                // Ignored unless the caption is transparent; it moves the
                // traffic lights AppKit keeps drawing into the toolbar
                // band the app puts in the caption's place.
                traffic_light_position: (titlebar == TitlebarStyle::Custom)
                    .then_some(TRAFFIC_LIGHT_ORIGIN),
            }),
            // Only the Linux backends read this. `appears_transparent`
            // above means nothing to X11 and Wayland: the caption stays
            // the compositor's until the window asks for client-side
            // decorations outright. gpui falls back to server decorations
            // on its own when no compositor is present, and
            // [`draws_own_titlebar`] follows what the window actually got.
            window_decorations: (titlebar == TitlebarStyle::Custom)
                .then_some(gpui::WindowDecorations::Client),
            // Wayland compositors and X11 docks match this against
            // com.aihouse.rulogman.desktop to pick up the application icon.
            app_id: Some("com.aihouse.rulogman".into()),
            // A translucent or blurred window needs the platform surface to
            // permit alpha; the terminal view then tints its background.
            window_background: chrome::window_appearance(
                settings.window.background_blur,
                settings.window.background_opacity,
            ),
            ..Default::default()
        },
        |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(titlebar, window, cx));
            let handle = workspace.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            apply_caption_theme(window, &theme(cx), cx);
            workspace
        },
    )
}

/// Where the next window goes.
///
/// Stepped off the window the command came from when there is one, and centred
/// on the display when there is not — which is the launch, and also a *New
/// window* arriving while the platform says nothing is focused.
pub(super) fn new_window_bounds(cx: &mut App) -> Bounds<Pixels> {
    let front = active_workspace_window(cx);
    let bounds = front.and_then(|handle| handle.update(cx, |_, window, _| window.bounds()).ok());
    match bounds {
        Some(bounds) => cascaded(bounds),
        None => Bounds::centered(None, size(px(1100.), px(700.)), cx),
    }
}

/// `bounds` stepped down and across by [`WINDOW_CASCADE`], keeping its size.
///
/// A free function, and the whole of the placement rule, so that where a second
/// window lands can be checked without opening one.
pub(super) fn cascaded(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: bounds.origin + point(px(WINDOW_CASCADE), px(WINDOW_CASCADE)),
        size: bounds.size,
    }
}

/// Every open window whose root view is a [`Workspace`].
///
/// The application's own windows and nothing else: `cx.windows()` answers for
/// the process, and a dialog the platform put up on its own has no workspace in
/// it to speak to.
pub(super) fn workspace_windows(cx: &App) -> Vec<WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Workspace>())
        .collect()
}

/// [`workspace_windows`] without the window the caller is in.
///
/// The exclusion is a requirement rather than a courtesy. A caller reached
/// through one of its own window's callbacks holds that window off gpui's
/// stack and its workspace out of the entity map for the length of the call, so
/// reading or updating it a second time from here would fail or panic on the
/// double lease. Every caller answers for its own window itself.
pub(super) fn other_workspace_windows(except: &Window, cx: &App) -> Vec<WindowHandle<Workspace>> {
    let except = except.window_handle().window_id();
    workspace_windows(cx)
        .into_iter()
        .filter(|window| window.window_id() != except)
        .collect()
}

/// Re-applies the current settings to every window but `except`.
///
/// The settings are one answer for the application: the language, the theme and
/// the window chrome are chosen once and every window has to come back wearing
/// them. The window the dialog was opened in is left to the workspace that owns
/// it — see [`other_workspace_windows`].
pub(super) fn apply_settings_elsewhere(except: &Window, cx: &mut App) {
    for handle in other_workspace_windows(except, cx) {
        let applied = handle.update(cx, |workspace, window, cx| {
            workspace.apply_settings(window, cx);
        });
        if let Err(error) = applied {
            log::warn!("could not apply the settings to another window: {error}");
        }
    }
}

/// Every session held by a window other than `except`.
///
/// The other half of the answer to a question that reads as if it were about one
/// window and is really about the machine: which ports are bound right now. See
/// [`Workspace::tunnels_held_elsewhere`], which asks it, and
/// [`other_workspace_windows`] for why the asking window is left out.
pub(super) fn sessions_in_other_windows(except: &Window, cx: &App) -> Vec<Entity<Session>> {
    other_workspace_windows(except, cx)
        .into_iter()
        .filter_map(|handle| handle.read(cx).ok())
        .flat_map(|workspace| workspace.sessions(cx))
        .collect()
}

/// Whether a window other than `except` is installing an update.
///
/// See [`Workspace::update_installing`], which is what asks.
pub(super) fn installing_elsewhere(except: &Window, cx: &App) -> bool {
    other_workspace_windows(except, cx)
        .into_iter()
        .filter_map(|handle| handle.read(cx).ok())
        .any(|workspace| workspace.update.read(cx).is_busy())
}

/// Whether the caller is the first to ask, which every later caller is not.
///
/// The silent start-up update check belongs to the launch and not to a window:
/// a second window opened from the menu is not a second launch, and asking
/// GitHub again would risk a dialog announcing a release the user has already
/// been shown — or dismissed. The answer is kept in a global because the
/// question is about the process, and every window that could ask has an `App`
/// in front of it.
pub(super) fn claim_startup_check(cx: &mut App) -> bool {
    if cx.has_global::<StartupCheckDone>() {
        return false;
    }
    cx.set_global(StartupCheckDone);
    true
}

/// The marker [`claim_startup_check`] sets once and never clears.
pub(super) struct StartupCheckDone;

impl Global for StartupCheckDone {}

/// The window a request arriving from outside the application should act on.
///
/// Whichever window is in front, because that is the one the user was looking at
/// when they asked; failing that the first one open, since the platform reports
/// no active window while the application is in the background — which is
/// exactly the case a Finder *Open with* arrives in.
pub(super) fn active_workspace_window(cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.active_window()
        .and_then(|window| window.downcast::<Workspace>())
        .or_else(|| workspace_windows(cx).into_iter().next())
}

/// Opens a tab per directory the launch named, and brings the window forward
/// if it opened any.
///
/// Both launch paths end here — the argv read before the app started and the
/// URLs macOS delivers while it runs — because from the workspace's point of
/// view they are the same request arriving twice over. The tabs go to the
/// window that is in front at the moment the paths arrive rather than to a
/// window fixed at start-up: by the time a second *Open with* lands there may
/// be several, and the first one opened is not necessarily the one the user is
/// working in. On the launch itself there is only the window just opened, so
/// the same rule covers both.
pub(super) fn open_start_dirs(dirs: Vec<PathBuf>, cx: &mut App) {
    if dirs.is_empty() {
        return;
    }
    let Some(window) = active_workspace_window(cx) else {
        log::warn!("no window is open to show the paths given in");
        return;
    };
    let opened = window.update(cx, |workspace, window, cx| {
        for dir in dirs {
            workspace.open_local_directory(dir, window, cx);
        }
        // For the second launch rather than the first: the user asked for this
        // window by opening something with it, and on macOS the app it woke is
        // otherwise left in the background.
        window.activate_window();
    });
    if let Err(error) = opened {
        log::warn!("could not open a shell for the paths given: {error}");
    }
}

/// Opens a window of its own for the directories a service named, and brings it
/// forward.
///
/// The *New rulogman Window Here* half of the pair the bundle declares, and the
/// only place in the application where a request from outside makes a window
/// rather than a tab. [`open_start_dirs`] is the other half, and everything
/// after the window is chosen is the same in both.
///
/// A window with nothing in it is taken over rather than added to. A service
/// invoked while rulogman is not running starts it, and by the time the request
/// is drained the run closure has already opened the window every launch opens
/// — showing the start screen, since the launch itself named no paths. Opening
/// a second window on top of that leaves the first one standing empty behind it,
/// which is not what *in a new window* meant: what the user asked for is a
/// window showing their folder, and an empty one is a window that has yet to be
/// given anything. Any tabless workspace will do, not merely the one this
/// launch opened, because a window the user emptied by closing its last tab is
/// in exactly the same state and equally has nothing to lose.
///
/// Nothing at all happens for an empty list. A service whose folders have all
/// gone since the Finder drew the menu has asked for nothing, and answering it
/// with an empty window would be the one outcome worse than answering it with
/// nothing.
pub(super) fn open_start_dirs_in_new_window(dirs: Vec<PathBuf>, cx: &mut App) {
    if dirs.is_empty() {
        return;
    }
    let window = match workspace_windows(cx)
        .into_iter()
        .find(|window| window.read(cx).is_ok_and(Workspace::has_no_tabs))
    {
        Some(window) => window,
        None => match open_workspace_window(cx) {
            Ok(window) => window,
            Err(error) => {
                log::warn!("could not open a window for the paths given: {error:#}");
                return;
            }
        },
    };
    let opened = window.update(cx, |workspace, window, cx| {
        for dir in dirs {
            workspace.open_local_directory(dir, window, cx);
        }
        // The application is in the background whenever a service reaches it —
        // the user was in the Finder — so the window it just made would
        // otherwise open behind whatever they were looking at.
        window.activate_window();
    });
    if let Err(error) = opened {
        log::warn!("could not open a shell for the paths given: {error}");
    }
}

/// Opens the tab a `-e <command…>` asked for, and brings the window forward.
///
/// Shaped like [`open_start_dirs`] and for the same reason: the launch is
/// answered before the window is shown, so a launch that asked for `btop` never
/// flashes the start screen on its way to it. There is at most one such tab —
/// `-e` ends the parse, so a launch names one command or none.
///
/// The command runs in this process's working directory, which is where the
/// launcher put it: see [`launch::command_start_dir`] for why that is read
/// straight rather than through the home-directory filter a path-less launch
/// goes through.
#[cfg(unix)]
pub(super) fn open_launch_command(command: Option<Vec<String>>, cx: &mut App) {
    let Some(command) = command else {
        return;
    };
    let Some(window) = active_workspace_window(cx) else {
        log::warn!("no window is open to run the command given in");
        return;
    };
    let cwd = launch::command_start_dir();
    let opened = window.update(cx, |workspace, window, cx| {
        workspace.open_local_command_at(command, cwd, window, cx);
        window.activate_window();
    });
    if let Err(error) = opened {
        log::warn!("could not run the command given: {error}");
    }
}

/// The Windows answer to the same request, which is to say so in the log.
///
/// `-e` is a unix convention — it is what the freedesktop desktop entry spec
/// and every terminal on that platform mean by *run this* — and nothing on
/// Windows launches rulogman that way. Parsed there regardless, because the
/// parser has no business being two parsers, and turned down here rather than
/// silently swallowed so that a user who typed it learns why nothing happened.
#[cfg(not(unix))]
pub(super) fn open_launch_command(command: Option<Vec<String>>, _cx: &mut App) {
    if let Some(command) = command {
        log::warn!(
            "ignoring -e {}: running a command in place of the shell is a unix convention",
            command.join(" ")
        );
    }
}

/// The dashboards a launch should open, in the order they should open in.
///
/// Two ways of asking, answered as one list. A dashboard the user marked
/// *open at startup* asks every time, silently and from the store itself; a
/// `--dashboard <name>` on the command line asks once, for this run. The
/// marked ones come first because they are the standing arrangement — the
/// thing the user set up to be there whenever rulogman starts — and what the
/// command line named is what they asked for *today*, which is the tab they
/// want to be looking at when the window comes up.
///
/// Deduplicated by id, keeping the first appearance, so a dashboard that is
/// both marked and named opens one tab rather than two identical ones.
///
/// Only the launch asks this. What the names alone resolve to is
/// [`named_dashboards`], which is what a `rulogman://dashboard/<name>` URL
/// arriving later goes through.
pub(super) fn startup_dashboards(store: &DashboardStore, requested: &[String]) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = store
        .dashboards()
        .iter()
        .filter(|dashboard| dashboard.open_at_startup)
        .map(|dashboard| dashboard.id)
        .collect();

    for id in named_dashboards(store, requested) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    ids
}

/// The dashboards a list of names asks for, in the order they were asked for.
///
/// A name is matched exactly, and against the name alone: dashboard names are
/// not unique — identity in the store is the id — so two dashboards may answer
/// to one name and the first in store order takes it. That is a shape the user
/// can see, since the welcome screen lists the store in the same order; a
/// fuzzy or case-insensitive match would not be. A name nothing answers to is
/// warned about and skipped, the same stance a path that is not there gets:
/// the window still opens, with one tab fewer than asked for.
///
/// Deduplicated by id, so naming the same dashboard twice — which a repeated
/// `--dashboard` or a repeated URL may well do — opens one tab.
pub(super) fn named_dashboards(store: &DashboardStore, names: &[String]) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = Vec::new();

    for name in names {
        match store
            .dashboards()
            .iter()
            .find(|dashboard| dashboard.name == *name)
        {
            Some(dashboard) if !ids.contains(&dashboard.id) => ids.push(dashboard.id),
            Some(_) => {}
            None => log::warn!("ignoring the dashboard {name}: no dashboard is called that"),
        }
    }

    ids
}

/// Opens a tab per dashboard the launch asked for, marked or named.
///
/// The launch only, and once: a request that arrives while the application is
/// running goes through [`open_named_dashboards`] instead, which reads no
/// marks. Shaped like [`open_start_dirs`], and placed right after it, so that
/// the whole launch is answered before the window is shown. The store is the one
/// the window already loaded rather than a second read of the file: two copies
/// could disagree about what is on disk, and it is the window's copy the
/// welcome screen lists and the numbered shortcuts index.
///
/// Opening order is [`startup_dashboards`] order, and every dashboard lands in
/// a tab of its own after the ones already there, so the last one opened is the
/// one left active. That is the intended end state — the newest request is what
/// the user is looking at — and it is also simply what
/// [`Workspace::open_dashboard`] does with each tab it appends.
///
/// A dashboard whose connections have nothing saved is not opened at all: the
/// all-or-nothing credential gate in [`Workspace::open_dashboard`] puts the
/// connection form up instead and leaves the dashboard to be clicked. At
/// start-up that means a window that comes up on a pre-filled dialog rather
/// than on the arrangement, which is the right end of the trade — the
/// alternative is a start-up that queues one modal per unsaved host — and it is
/// the same answer clicking the dashboard would have given.
pub(super) fn open_startup_dashboards(names: Vec<String>, cx: &mut App) {
    let Some(window) = active_workspace_window(cx) else {
        // Only worth a word when something was actually asked for on the
        // command line; a marked dashboard cannot even be looked for without a
        // window to read the store from.
        if !names.is_empty() {
            log::warn!("no window is open to show the dashboards asked for");
        }
        return;
    };
    let opened = window.update(cx, |workspace, window, cx| {
        for id in startup_dashboards(&workspace.dashboards, &names) {
            workspace.open_dashboard(id, window, cx);
        }
    });
    if let Err(error) = opened {
        log::warn!("could not open the dashboards asked for: {error}");
    }
}

/// Opens a tab per dashboard a `rulogman://dashboard/<name>` URL named, in a
/// window that is already up, and brings that window forward.
///
/// The names and nothing else. [`open_startup_dashboards`] answers the launch,
/// and part of what it answers is the standing arrangement — every dashboard
/// marked *open at startup* — which was already opened when this process came
/// up. A URL is a fresh request made of a running application, so it is
/// answered with exactly what it asked for; reading the marks again would add
/// the morning's tabs to the window every time a link was opened.
///
/// The window is activated for the reason [`open_start_dirs`] activates it: on
/// macOS the URL woke an application that is otherwise left in the background,
/// and the user who opened the link is waiting to be shown the dashboard.
pub(super) fn open_named_dashboards(names: Vec<String>, cx: &mut App) {
    if names.is_empty() {
        return;
    }
    let Some(window) = active_workspace_window(cx) else {
        log::warn!("no window is open to show the dashboards asked for");
        return;
    };
    let opened = window.update(cx, |workspace, window, cx| {
        for id in named_dashboards(&workspace.dashboards, &names) {
            workspace.open_dashboard(id, window, cx);
        }
        window.activate_window();
    });
    if let Err(error) = opened {
        log::warn!("could not open the dashboards asked for: {error}");
    }
}
