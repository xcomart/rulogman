//! Bootstrap.

use super::*;

/// Installs the widget theme the configured id names.
///
/// An id nothing answers to — a theme file the user has since deleted — falls
/// back to the default theme rather than failing; see
/// [`ThemeRegistry::resolve`].
pub(super) fn apply_ui_theme(id: &str, cx: &mut App) {
    let theme = ThemeRegistry::resolve(id, cx);
    set_theme(theme, cx);
}

/// The application menu bar, in macOS layout.
///
/// gpui only turns this into a real menu bar on macOS — the Windows and Linux
/// backends store it and draw nothing — so the other platforms get the same
/// commands from the in-app dropdown built by
/// [`Workspace::render_app_menu`]. Every item dispatches an action that is also
/// bound to a shortcut in [`bind_shortcuts`], which is what lets the macOS
/// backend label the items with their key equivalents; register the bindings
/// first so the keymap it reads is already populated.
///
/// About, Check for updates, Settings and Quit live in the application menu
/// because that is where macOS users look for them.
///
/// The item labels are translated, but the application menu's own name is the
/// "rulogman" wordmark and stays as it is. Rebuilt and re-installed whenever the
/// language changes, because gpui takes the menu bar by value.
pub(super) fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "rulogman".into(),
            items: vec![
                MenuItem::action(ts!("menu.about"), ShowAbout),
                MenuItem::action(ts!("menu.check_updates"), CheckUpdates),
                MenuItem::separator(),
                MenuItem::action(ts!("menu.settings"), OpenSettings),
                MenuItem::separator(),
                MenuItem::action(ts!("menu.mac.quit"), Quit),
            ],
            disabled: false,
        },
        Menu {
            name: ts!("menu.session"),
            items: vec![
                MenuItem::action(ts!("menu.mac.new_session"), NewSession),
                MenuItem::action(ts!("menu.mac.new_window"), NewWindow),
                MenuItem::action(ts!("menu.mac.close_session"), CloseSession),
                // Only half of splitting is here, for the reason given on
                // [`Workspace::render_app_menu`]: a merge has to name a source
                // tab, so it belongs to the tab context menu alone.
                MenuItem::action(ts!("menu.mac.duplicate_right"), DuplicateSplitRight),
                MenuItem::action(ts!("menu.mac.duplicate_below"), DuplicateSplitBelow),
                MenuItem::action(ts!("menu.mac.equalize_widths"), EqualizeWidths),
                MenuItem::action(ts!("menu.mac.equalize_heights"), EqualizeHeights),
                MenuItem::action(ts!("menu.mac.break_out_pane"), BreakOutPane),
                MenuItem::action(ts!("menu.mac.tab_to_window"), MoveTabToNewWindow),
                MenuItem::separator(),
                MenuItem::action(ts!("files.mac.toggle"), ToggleFilePanel),
            ],
            disabled: false,
        },
    ]
}

/// Registers every shortcut the workspace listens for.
///
/// A binding here beats the terminal: gpui matches key bindings along the whole
/// dispatch path before it delivers the key event itself, so every chord bound
/// in this function is taken away from the remote shell. That is what decides
/// the pane modifier below.
pub(super) fn bind_shortcuts(cx: &mut App) {
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };

    // Pane navigation follows iTerm2 on macOS, where `cmd` never reaches the
    // shell. Elsewhere the same chords would swallow `Ctrl+[` — which every
    // remote shell reads as ESC — and `Ctrl+]`, so those platforms use `alt`
    // instead, the modifier Windows Terminal also keeps for pane navigation.
    // The bracket keys stay unshifted on purpose: both macOS and Windows report
    // a shifted bracket as `}` with the shift flag already consumed, so a
    // `shift-]` binding would never match. Hence a letter for the break-out.
    let pane_modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "alt"
    };

    let mut bindings = vec![
        KeyBinding::new(&format!("{modifier}-q"), Quit, None),
        KeyBinding::new(&format!("{modifier}-t"), NewSession, Some(KEY_CONTEXT)),
        KeyBinding::new(WINDOW_SHORTCUT, NewWindow, Some(KEY_CONTEXT)),
        KeyBinding::new(&format!("{modifier}-w"), CloseSession, Some(KEY_CONTEXT)),
        KeyBinding::new(&format!("{modifier}-,"), OpenSettings, Some(KEY_CONTEXT)),
        KeyBinding::new("escape", DismissDialog, Some(KEY_CONTEXT)),
        KeyBinding::new(
            &format!("{pane_modifier}-]"),
            FocusNextPane,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{pane_modifier}-["),
            FocusPrevPane,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{pane_modifier}-shift-b"),
            BreakOutPane,
            Some(KEY_CONTEXT),
        ),
        // Shifted for the reason the break-out is, and by the same arithmetic:
        // off macOS the pane modifier is `alt`, and bare `Alt+N` is readline's
        // *non-incremental-forward-search-history*. The shifted chord costs the
        // remote shell nothing, because a terminal cannot encode `Alt+Shift+N`
        // distinctly from `Alt+N` in the first place — see the split bindings
        // below. `N` rather than a letter of its own so that it reads with the
        // window commands: `Ctrl+Shift+N` opens an empty window, and this fills
        // one.
        KeyBinding::new(
            &format!("{pane_modifier}-shift-n"),
            MoveTabToNewWindow,
            Some(KEY_CONTEXT),
        ),
        // Shifted for the same reason the break-out is: off macOS the pane
        // modifier is `alt`, and bare `Alt+D` is readline's *kill-word*, which
        // a user typing in the pane being split would miss immediately. The
        // shifted chord is free in a way the bare one is not — a terminal
        // cannot encode `Alt+Shift+D` distinctly from `Alt+D` — so taking it
        // costs the remote shell nothing. `Alt+S` is shifted to match, since
        // the two split directions have to read as one pair of commands.
        KeyBinding::new(
            &format!("{pane_modifier}-shift-d"),
            DuplicateSplitRight,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{pane_modifier}-shift-s"),
            DuplicateSplitBelow,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(PANEL_SHORTCUT, ToggleFilePanel, Some(KEY_CONTEXT)),
        // Shifted to stay clear of the shell for the reason the window chord is:
        // bare `Ctrl+L` is readline's *clear-screen*, and a terminal cannot
        // encode `Ctrl+Shift+L` distinctly from it, so the shifted chord is free
        // to take. `L` for layout; a no-op on any tab that is not a dashboard.
        KeyBinding::new(
            &format!("{modifier}-shift-l"),
            SaveDashboardLayout,
            Some(KEY_CONTEXT),
        ),
    ];
    for index in 0..QUICK_SELECT_TABS {
        bindings.push(KeyBinding::new(
            &format!("{modifier}-{}", index + 1),
            SelectTab(index),
            Some(KEY_CONTEXT),
        ));
    }
    // The digits again with `Alt` added, which reads as what it is: the tab
    // chord one level up — `Ctrl+1` picks the first tab, `Ctrl+Alt+1` opens the
    // first dashboard. `Alt` is what is left to add: every other chord this
    // function registers is `{modifier}`, `{pane_modifier}` or one of those
    // shifted, and none of them is the pair, so nothing in the application is
    // being taken away from.
    //
    // Nor is anything being taken from the remote shell, on either half of the
    // split. On macOS `cmd` never reaches it at all. Elsewhere the chord is
    // `Ctrl+Alt+digit`, which a terminal cannot encode distinctly in the first
    // place — there is no control code for a digit — so what a shell would have
    // received for it is at most the `ESC digit` of a bare `Alt+digit`, and
    // that chord is untouched: the two arrive here as different modifier sets
    // and only the one with `Ctrl` is bound.
    //
    // The index is into the saved order of the dashboard store, which is the
    // order the welcome screen lists them in — so the number to press is the
    // number of the row the user is already looking at.
    for index in 0..QUICK_OPEN_DASHBOARDS {
        bindings.push(KeyBinding::new(
            &format!("{modifier}-alt-{}", index + 1),
            OpenDashboard(index),
            Some(KEY_CONTEXT),
        ));
    }

    cx.bind_keys(bindings);
}

/// Something the desktop handed a running rulogman, on its way to the UI
/// thread.
///
/// Both arrive on a platform callback that has no `App` to work with, and both
/// have to be answered on the thread that owns the windows, so both take the
/// same channel — and taking the same channel is what keeps them in the order
/// they were made. What the two have in common is a list of folders; what tells
/// them apart is that a service also says *where* the folders should be opened,
/// and that is the whole of why this is an enum rather than a `Vec<String>`.
pub(super) enum Arrival {
    /// URLs from `application:openURLs:`: a `file://` per folder a Finder *Open
    /// with* named, or a `rulogman://dashboard/<name>`. See
    /// [`launch::split_open_urls`].
    Urls(Vec<String>),
    /// One of the services the bundle declares, with the folders it was invoked
    /// on and the `NSUserData` saying which entry the user picked. See
    /// [`launch::service_target`].
    Service(ServiceRequest),
}

pub(super) fn run() {
    env_logger::init();

    // Before anything reads a configuration file — and before the app runs at
    // all, since this is pure filesystem work that needs no window. A user
    // updating from a release published under the old name still has their
    // profiles, settings and themes in the directory that name derived; this
    // copies them across once. Failing is survivable: the app then starts with
    // an empty configuration, exactly as it would have without the attempt.
    if let Err(error) = rulogman_core::migrate_from_logman() {
        log::warn!("could not migrate the configuration of the previous release: {error:#}");
    }

    // Read before anything else touches the launch, because everything about
    // it is filesystem work that wants no window: what is left is a list of
    // directories, and a directory that was named but is not there has already
    // been dropped with a warning by the time the app starts.
    //
    // The argv is split first, because not everything in it is a path:
    // `--dashboard <name>` asks for a saved arrangement rather than a folder,
    // and `-e <command…>` asks for a program to be run in place of the shell,
    // and all three are answered in different places. See
    // [`launch::split_launch_args`].
    let launch::LaunchArgs {
        paths: path_args,
        dashboards: dashboard_names,
        command: launch_command,
    } = launch::split_launch_args(std::env::args_os().skip(1));
    let start_dirs = launch::start_dirs(path_args);
    // KDE's *Open Terminal Here* — and any launcher that treats rulogman as
    // the desktop's default terminal — never puts the folder in argv at all:
    // `KTerminalLauncherJob` only knows how to pass `--workdir` to konsole, so
    // for every other terminal it runs the desktop entry's `Exec=` line
    // unchanged and communicates the folder solely by setting the child's
    // working directory. Without this, that arrives here as zero paths and
    // opens the welcome screen instead of a shell in the folder Dolphin meant.
    //
    // A launch that named a dashboard is excluded, and has to be: the working
    // directory is only a signal *because* the launch said nothing else, and
    // `rulogman --dashboard morning` typed in a project folder has said
    // something else. Reading the folder as a request too would open a shell
    // beside the dashboard that nobody asked for. A launch that named a command
    // is excluded for exactly that reason — `rulogman -e btop` is the same
    // desktop asking for something specific — and doubly so, since the command
    // is itself started in that very directory.
    #[cfg(all(unix, not(target_os = "macos")))]
    let start_dirs =
        if start_dirs.is_empty() && dashboard_names.is_empty() && launch_command.is_none() {
            launch::implicit_start_dir().into_iter().collect()
        } else {
            start_dirs
        };

    // The other half of the same question, and the only half macOS asks. A
    // Finder *Open with* — or `open -a rulogman /var/log`, or an `open
    // "rulogman://dashboard/Morning"` — reaches the app as
    // `application:openURLs:` rather than as an argv, and it does so whether
    // the app was already running or is starting because of it. It is the only
    // thing a second launch can say at all: `open -a rulogman` hands a running
    // application no argv, so a URL is how everything after the first launch
    // asks for anything.
    //
    // The two services the bundle declares come in through the same door and
    // are the same request but for one word. *New rulogman Window Here* and
    // *New rulogman Tab Here* — the entries in the Finder's right-click
    // *Services* submenu — hand over a folder selected in some other
    // application, in the same `file://` spelling an *Open with* uses, plus the
    // `NSUserData` of the entry the user picked, which is the only way macOS
    // says which one it was: the menu title they actually read is localised and
    // never reaches the application. See [`launch::service_target`].
    //
    // The callbacks
    // have no `App` to work with, so they do the one thing they can: hand what
    // arrived to a channel the run closure below drains on the UI thread. One
    // channel rather than two, because what is at the far end is one queue of
    // requests and answering them out of the order they were made would open
    // the second folder in the window the first one was still about to make. On
    // Linux and Windows nothing ever sends on it, since both platforms put the
    // paths — and the `rulogman://` URL a browser or `xdg-open` hands over —
    // in the argv read above; registering them regardless costs two callbacks
    // that are never called.
    let (arrivals, mut arrivals_rx) = mpsc::unbounded();
    let opened_urls = arrivals.clone();
    // `LastWindowClosed` rather than the default, which is this only away from
    // macOS: there an app whose last window closes stays in the Dock with its
    // menu bar, and *New Window* would still be reachable from it — but there is
    // nothing behind an empty screen worth keeping alive. Every session belongs
    // to a window and goes when the window does, so once the last one is closed
    // the process has no work left. One rule on every platform is what the app
    // has always done.
    let app = gpui_platform::application()
        .with_assets(icons::ICONS)
        .with_quit_mode(QuitMode::LastWindowClosed);
    app.on_open_urls(move |urls| {
        // Failing means the receiver is gone, which means the app is on its way
        // out and there is no window left to open a tab in.
        let _ = opened_urls.unbounded_send(Arrival::Urls(urls));
    });
    app.on_service_request(move |request| {
        let _ = arrivals.unbounded_send(Arrival::Service(request));
    });

    // The icon set has to be installed before the app runs: `svg()` resolves
    // every path through this source, and the default one answers `None`.
    app.run(move |cx: &mut App| {
        // Everything `rugpui-shell` is not allowed to guess at, handed over
        // before anything that could read it runs. `set_strings` goes through
        // `ts!`, so the shell follows a language change without being told
        // again; `set_update_policy` is the two-line window onto the
        // `ignored_update` field of `settings.json`. `init` first, and before
        // `clean_leftovers` below: it is what fills the process-wide identity
        // slot the update paths read, and what records — while the running
        // image is still where it was launched from — the path
        // `rugpui_shell::restart_path` hands back after a swap has moved it.
        rugpui_shell::init(IDENTITY, cx);
        rugpui_shell::set_strings(Box::new(AppStrings), cx);
        rugpui_shell::set_update_policy(Box::new(IgnoredUpdate), cx);

        if let Err(error) = rulogman_core::init_secrets() {
            log::warn!("the OS keychain is unavailable: {error}");
        }

        // A self-update renames the copy it replaces aside instead of deleting
        // it — Windows cannot delete a running image, and one code path for
        // three platforms is worth more than an immediate unlink on the two
        // that could. This is the other half: the leftover is swept up on the
        // next launch. On the background executor because removing a `.app`
        // bundle is a recursive delete and nothing on screen depends on it.
        cx.background_executor()
            .spawn(async { shell_update::clean_leftovers() })
            .detach();

        // Load settings before the widget layer installs its default theme, then
        // override that theme to match what the user configured.
        app_settings::init(cx);
        let settings = app_settings::current(cx);
        // Ahead of everything that renders a string — the menu bar included —
        // so nothing is ever built in the wrong language and then corrected.
        i18n::apply(settings.language.as_deref());

        rugpui::init(cx);
        // After `rugpui::init`, which installs a fully opaque default of its own:
        // the widgets that have to agree with a translucent window read the
        // opacity from a global of the widget layer's.
        app_settings::set_tint(&settings, cx);
        // After `rugpui::init`, because the find bar is built out of the widget
        // layer's text field and binds keys in a context nested inside it.
        rugpui_editor::init(cx);
        // After `rugpui_editor::init`, because the pane's own context wraps the
        // editor's and binds the one command the widget cannot have: saving.
        editor_pane::init(cx);
        TerminalView::init(cx);
        bind_shortcuts(cx);
        cx.set_menus(app_menus());

        // Before the theme is applied: the id in the settings may well name one
        // of the user's own themes, and the same goes for the scheme every
        // session is about to be opened with.
        theme_store::reload(cx);
        // The languages the editor widget lexes, the definitions rulogman
        // ships and whatever the user has put beside them — here for the same
        // reason the palettes are: an editor opened later has to find the
        // registry already installed. Read once and never again, so a
        // definition added while rulogman is running arrives on the next launch.
        languages::init(cx);
        apply_ui_theme(&settings.ui_theme, cx);

        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        // Global rather than a listener on the workspace, the way quitting is:
        // opening a window is a command about the application, and nothing in
        // the workspace it was invoked from has to be consulted to carry it
        // out. Where the new window lands is the one thing that window has a
        // say in, and that is read from its bounds below.
        cx.on_action(|_: &NewWindow, cx: &mut App| {
            // Deferred, and only here: the action is dispatched from inside the
            // window it was invoked in, and gpui lifts a window off its own map
            // for the length of a dispatch — so the bounds the new window is
            // about to step off cannot be read until the dispatch is over. The
            // call below runs the moment it is, with every window back in
            // place. `main` calls the same function directly, because there is
            // no window to step off there.
            cx.defer(|cx| {
                if let Err(error) = open_workspace_window(cx) {
                    log::warn!("could not open another window: {error:#}");
                }
            });
        });

        open_workspace_window(cx).expect("failed to open the rulogman window");

        // A tab per path the launch named, before the window is shown: the
        // start screen is what a launch with no paths opens on, and a launch
        // with them should never flash it.
        open_start_dirs(start_dirs, cx);
        // And a tab per dashboard, in the same breath and for the same reason:
        // a launch that opens a dashboard must not flash the welcome screen
        // either. After the paths, so that a launch naming both puts the
        // dashboards where the eye ends up — see [`open_startup_dashboards`].
        open_startup_dashboards(dashboard_names, cx);
        // And, last so that it is the tab left in front, whatever `-e` named:
        // a launch that asked for a program to be run is a launch whose whole
        // point is that program.
        open_launch_command(launch_command, cx);
        // And a tab per path — or per dashboard — every *later* launch names,
        // for as long as this process lives. On macOS a second *Open with*
        // does not start a second rulogman — it wakes this one — so what it
        // asks for has to land in a window that is already open rather than in
        // a new one.
        cx.spawn(async move |cx| {
            // The loop ends on its own when the application does: the senders
            // live in the callbacks the platform owns, so the stream closes as
            // the platform is torn down and this task never reaches an `App`
            // that is no longer there.
            while let Some(arrival) = arrivals_rx.next().await {
                match arrival {
                    Arrival::Urls(batch) => {
                        // The two kinds of request the scheme makes reachable,
                        // told apart before either is answered: a `file://`
                        // names a folder, a `rulogman://dashboard/<name>` names
                        // a dashboard.
                        let (paths, names) = launch::split_open_urls(batch);
                        let dirs = launch::start_dirs(paths);
                        cx.update(|cx| {
                            open_start_dirs(dirs, cx);
                            // Names only — never [`open_startup_dashboards`].
                            // The dashboards marked *open at startup* were
                            // opened when this process came up; a URL arriving
                            // an hour later asks for the one dashboard it names
                            // and nothing else, and reading the marks again
                            // would pile the morning's tabs on top of it every
                            // time somebody opened a link.
                            open_named_dashboards(names, cx);
                        });
                    }
                    Arrival::Service(request) => {
                        // Folders and nothing else: the bundle declares
                        // `NSSendFileTypes` = `public.folder` for both entries,
                        // so a service never carries a `rulogman://` URL and
                        // there is nothing here to split apart. `start_dirs`
                        // still stands between the pasteboard and the
                        // workspace, since a folder can have gone between the
                        // Finder drawing the menu and the user reading it.
                        let dirs = launch::start_dirs(request.urls);
                        // An entry this build does not know is answered as a
                        // tab rather than dropped: the folders are the request
                        // and the target is only where to put it, so the worse
                        // of the two answers is still the right one. See
                        // [`launch::service_target`], which logs what it saw.
                        let target = launch::service_target(&request.user_data)
                            .unwrap_or(launch::ServiceTarget::Tab);
                        cx.update(|cx| match target {
                            launch::ServiceTarget::Window => {
                                open_start_dirs_in_new_window(dirs, cx)
                            }
                            launch::ServiceTarget::Tab => open_start_dirs(dirs, cx),
                        });
                    }
                }
            }
        })
        .detach();

        cx.activate(true);
    });
}
