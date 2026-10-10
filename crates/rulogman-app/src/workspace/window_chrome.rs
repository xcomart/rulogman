//! Window chrome.

use super::*;

impl Workspace {
    /// Renders the toolbar: the application menu button and the tab strip.
    ///
    /// The button is left out on macOS, where [`app_menus`] puts the same
    /// commands in the system menu bar.
    ///
    /// In the custom title bar style this row *is* the title bar. It then marks
    /// itself as the window's drag area, takes over writing the application's
    /// name at its left end, and — off macOS, which keeps its native traffic
    /// lights — grows a set of caption buttons at its right end. Every
    /// *control* inside it occludes, so the drag area only ever answers for the
    /// gaps between them; see [`rugpui::window_controls`]. The name is not a
    /// control and deliberately does not.
    pub(super) fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let custom = chrome::draws_own_titlebar(chrome_style(self.titlebar), window);
        let titlebar_active = custom && window.is_window_active();
        let titlebar_text = if titlebar_active {
            theme.text
        } else {
            theme.text_muted
        };
        let menu = (!cfg!(target_os = "macos")).then(|| self.render_app_menu(window, cx));
        // Nothing to browse without a session, so the toggle goes with the panel
        // it would open. A session of either kind has a filesystem behind it —
        // the server's, or this computer's — so nothing finer is asked here.
        let toggle = self.tabs.get(self.active).is_some().then(|| {
            let open = self.panel_showing();
            let hover = theme.surface_hover;
            // The open state is already carried by the accent colour, so only
            // the closed button brightens on hover. The icon is tinted by its
            // own `text_color` rather than the button's, so the hover shade has
            // to reach it through the group.
            let hover_text = if open { theme.accent } else { theme.text };
            div()
                .id("toggle-file-panel")
                // The row behind it may be a window drag area; see
                // [`rugpui::window_controls`].
                .occlude()
                .group(PANEL_TOGGLE_GROUP)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(28.))
                .rounded_md()
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(|workspace, _, _window, cx| {
                    workspace.toggle_file_panel(cx);
                }))
                // The shortcut rides along, the way the dropdown row for the
                // same command carries it: this button is the only place the
                // binding is discoverable on macOS, where there is no in-app
                // menu to read it off.
                .tooltip(tooltip_label(ts!(
                    "files.tip_toggle",
                    shortcut = PANEL_SHORTCUT_LABEL
                )))
                .child(
                    icons::icon(
                        icons::PANEL,
                        px(16.),
                        if open { theme.accent } else { theme.icon },
                    )
                    .group_hover(PANEL_TOGGLE_GROUP, move |style| {
                        style.text_color(hover_text)
                    }),
                )
        });

        // One cell for the leading controls, so the menu button and the panel
        // toggle share the toolbar's fill and bottom hairline with the strip.
        let leading = (menu.is_some() || toggle.is_some()).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(2.))
                .h(px(TOOLBAR_HEIGHT))
                .px(px(4.))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .children(menu)
                .children(toggle)
        });

        // Room for the traffic lights AppKit still draws over the transparent
        // title bar. Painted like the leading cell rather than left empty, so
        // the band reads as one strip. Fullscreen hides the buttons, and the
        // gap goes with them.
        let traffic_lights =
            (custom && cfg!(target_os = "macos") && !window.is_fullscreen()).then(|| {
                div()
                    .flex_none()
                    .w(px(TRAFFIC_LIGHT_GAP))
                    .h(px(TOOLBAR_HEIGHT))
                    .bg(theme.surface)
                    .border_b_1()
                    .border_color(theme.border)
            });

        // The application's own name, which only the custom style has to write:
        // a system title bar already carries it, and drawing it twice would put
        // it in two places at once.
        //
        // Windows and the GTK/KDE captions set an application icon beside the
        // title and macOS does not, so the mark follows that split.
        //
        // Nothing here is interactive, and — unlike every control in this row —
        // nothing here occludes either. The name and the mark are part of the
        // *empty* title bar as far as the window is concerned, so a press on
        // them has to reach the drag area underneath and move the window.
        let title = custom.then(|| {
            // The shipped icon in its own colours, not a theme-tinted sprite:
            // the current icon's embossed ring keeps its tile legible on dark
            // chrome, which is what used to force the tinted stand-in. See
            // [`icons::APP_ICON`].
            let icon = (!cfg!(target_os = "macos")).then(|| {
                let icon = img(icons::APP_ICON).w(px(16.)).h(px(16.)).flex_none();
                if custom && !titlebar_active {
                    div()
                        .size(px(16.))
                        .flex_none()
                        .opacity(0.55)
                        .child(icon)
                        .into_any_element()
                } else {
                    icon.into_any_element()
                }
            });
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .h(px(TOOLBAR_HEIGHT))
                .px(px(10.))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                // A shade quieter than a tab title, which is the one label in
                // this row that has to be read.
                .text_size(px(12.))
                .text_color(titlebar_text)
                .children(icon)
                .child("rulogman")
        });

        // The caption buttons the other two platforms have to draw themselves,
        // as the two ends a Linux desktop may ask for: putting them on the left
        // is a setting people actually use, and the shell's own glyphs are what
        // they are drawn with.
        let (leading_controls, trailing_controls) = chrome::window_control_strips(
            &rugpui_shell::window_control_icons(),
            custom,
            window,
            cx,
        );

        div()
            .id("toolbar")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .w_full()
            .h(px(TOOLBAR_HEIGHT))
            .when(custom, |this| {
                // Occluding is load-bearing, not just hygiene: the workspace
                // root tracks focus, and gpui's focus transfer marks every
                // mouse down over it `default_prevented` — which the Windows
                // backend reads as "the app took this press", swallowing the
                // `HTCAPTION` down that would have started the system drag.
                // Cutting the root's hitbox out from under the strip keeps the
                // press unclaimed, and spares the terminal a focus loss for a
                // click that was aimed at the window, not the app.
                chrome::titlebar_gestures(
                    this.occlude().window_control_area(WindowControlArea::Drag),
                )
            })
            // Ahead of the wordmark, which is where a desktop that asks for
            // left-hand caption buttons expects them: the buttons are the
            // window's, the name is the application's.
            .children(leading_controls)
            .children(traffic_lights)
            .children(title)
            .children(leading)
            .child(div().flex_1().min_w_0().child(self.render_tab_bar(cx)))
            .children(trailing_controls)
            .into_any_element()
    }

    /// Builds the dropdown menu shown on the platforms without a native one.
    ///
    /// Every row dispatches the action its keyboard shortcut dispatches, so the
    /// menu adds a way in rather than a second implementation.
    ///
    /// Splitting with a second connection is here, and so is breaking a pane
    /// out; merging a tab in is not, and cannot be: a merge needs a *source*
    /// tab, which a menu of static commands has no way to name. That one half of
    /// splitting lives in the tab context menu alone — see
    /// [`Workspace::render_tab_context`] — and the same asymmetry shapes
    /// [`app_menus`].
    ///
    /// The list is the same one on every frame, so a command that cannot run
    /// now is greyed rather than dropped — see [`MenuEntry::enabled`]. That is
    /// most of the menu on the welcome screen, where there is no pane to split,
    /// break out, or hang a file panel beside; only opening a session, the
    /// settings, the update check, the about box and quitting mean anything
    /// without one.
    pub(super) fn render_app_menu(&self, window: &Window, cx: &mut Context<Self>) -> MenuButton {
        let this = cx.entity();
        let caps = self.pane_caps(cx);
        // A tab is what the file panel is drawn beside: the welcome screen
        // replaces the body the panel lives in, and there is no filesystem to
        // browse until a session opens one.
        let has_tab = self.tabs.get(self.active).is_some();
        // The same guard `check_updates` applies: an install already running
        // owns the dialog, which cannot be closed and so must not be reopened.
        let updating = self.update_installing(window, cx);
        let entries = vec![
            MenuEntry::new(ts!("menu.new_session"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+T"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(NewSession), cx)),
            // Next to the new session, because the two are the same command at
            // two sizes: one opens a tab here, the other a window of its own.
            MenuEntry::new(ts!("menu.new_window"))
                .shortcut(WINDOW_SHORTCUT_LABEL)
                .on_activate(|window, cx| window.dispatch_action(Box::new(NewWindow), cx)),
            MenuEntry::new(ts!("menu.duplicate_right"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+D"))
                .disabled(!caps.split_right)
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitRight), cx)
                }),
            MenuEntry::new(ts!("menu.duplicate_below"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+S"))
                .disabled(!caps.split_below)
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitBelow), cx)
                }),
            // Under the two splits, because they are what make the command
            // worth having: a third pane comes out half the width of the first,
            // and this is how it stops being.
            MenuEntry::new(ts!("menu.equalize_widths"))
                .disabled(!caps.equalize_widths)
                .on_activate(|window, cx| window.dispatch_action(Box::new(EqualizeWidths), cx)),
            MenuEntry::new(ts!("menu.equalize_heights"))
                .disabled(!caps.equalize_heights)
                .on_activate(|window, cx| window.dispatch_action(Box::new(EqualizeHeights), cx)),
            MenuEntry::new(ts!("menu.break_out_pane"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+B"))
                .disabled(!caps.break_out)
                .on_activate(|window, cx| window.dispatch_action(Box::new(BreakOutPane), cx)),
            // After the break-out, because it is the same command a size up: one
            // moves a pane out of its tab, the other a tab out of its window.
            MenuEntry::new(ts!("menu.tab_to_window"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+N"))
                .disabled(!tab_can_move_out(self.tabs.len()))
                .on_activate(|window, cx| window.dispatch_action(Box::new(MoveTabToNewWindow), cx)),
            MenuEntry::new(ts!("files.toggle"))
                .shortcut(PANEL_SHORTCUT_LABEL)
                .disabled(!has_tab)
                .on_activate(|window, cx| window.dispatch_action(Box::new(ToggleFilePanel), cx)),
            MenuEntry::new(ts!("menu.settings"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+,"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
            MenuEntry::separator(),
            // Next to About, where a Help menu would put it and where users of
            // every other desktop application look for it.
            MenuEntry::new(ts!("menu.check_updates"))
                .disabled(updating)
                .on_activate(|window, cx| window.dispatch_action(Box::new(CheckUpdates), cx)),
            MenuEntry::new(ts!("menu.about"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(ShowAbout), cx)),
            MenuEntry::separator(),
            MenuEntry::new(ts!("menu.quit"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+Q"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(Quit), cx)),
        ];

        MenuButton::new("app-menu")
            .tooltip(ts!("menu.tip_menu"))
            .open(self.menu_open)
            .entries(entries)
            .on_open_change(move |open, _window, cx| {
                this.update(cx, |workspace, cx| workspace.set_menu_open(open, cx));
            })
    }

    /// Renders the tab strip.
    pub(super) fn render_tab_bar(&self, cx: &mut Context<Self>) -> TabBar {
        let this = cx.entity();
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                // A split tab is labelled after its active pane, so the strip
                // says what the user is looking at rather than what the tab
                // happened to be opened as. A tab holding nothing but open files
                // — which is what "Edit" opens — is named after the active one
                // of them and wears no status dot: the tab is not a connection,
                // so there is nothing for a dot to report on. See
                // [`editor_tab_label`] for what such a tab is called.
                // A tab that carries a name of its own — a dashboard — keeps
                // it in both arms: the name is the whole point of that tab, and
                // the status dot is still the active session's to report. See
                // [`SessionTab::label`].
                let named = tab.label.clone();
                match tab.active_session(cx) {
                    Some(session) => {
                        let session = session.read(cx);
                        let title = named.unwrap_or_else(|| session.title());
                        let item = TabItem::new(("session-tab", index), title)
                            .status(session.tab_status());
                        // Only the session that won the bind reports any, so
                        // the mark appears on exactly one tab per rule: the tab
                        // whose shell the forwarded traffic is actually riding
                        // on. Opening the same profile again leaves the second
                        // tab unmarked, which is the answer to the question the
                        // mark exists for.
                        match tunnel_tooltip(session.open_tunnels()) {
                            Some(tooltip) => item.mark(icons::TUNNEL, tooltip),
                            None => item,
                        }
                    }
                    None => TabItem::new(
                        ("session-tab", index),
                        named.unwrap_or_else(|| tab.active_view().label(cx)),
                    ),
                }
            })
            .collect();

        TabBar::new("session-tabs")
            .tabs(tabs)
            .active(self.active)
            .scroll_handle(&self.tab_scroll)
            .scrollbar(self.hovering_scrollbar(SCROLLBARS[0].0, Surface::Tabs, cx))
            .menu_icon(icons::TAB_LIST)
            .new_icon(icons::NEW_TAB)
            // The close button reuses the tab menu's own row: it is the same
            // command, worded the same way, and neither takes an ellipsis.
            .tooltips(
                ts!("tab.tip_list"),
                ts!("tab.tip_new", shortcut = format!("{SHORTCUT_MODIFIER}+T")),
                ts!("tab.close"),
            )
            .menu_open(self.tab_menu_open)
            .on_menu_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |workspace, cx| workspace.set_tab_menu_open(open, cx));
                }
            })
            .on_select({
                let this = this.clone();
                move |index, window, cx| {
                    this.update(cx, |workspace, cx| workspace.select_tab(index, window, cx));
                }
            })
            .on_close({
                let this = this.clone();
                move |index, window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_tab(index, window, cx));
                }
            })
            .on_context_menu({
                let this = this.clone();
                move |index, at, _window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.open_tab_context(index, at, cx)
                    });
                }
            })
            .on_new(move |_window, cx| {
                this.update(cx, |workspace, cx| workspace.open_dialog(cx));
            })
    }

    /// Renders the file-type picker, if it is open.
    ///
    /// Anchored by its **bottom** left corner, which is the whole reason
    /// [`ContextMenu::anchor`] exists: the trigger sits in the last two dozen
    /// pixels of the window, so a list hanging down from it would be snapped
    /// back over the point it was opened from and cover the answer it is asking
    /// about. Standing it on the pointer opens it into the window instead.
    ///
    /// Narrower than the application's own menus as well. These rows are one
    /// word each — `JSON`, `Rust` — and the width that fits "Split right of
    /// current tab" reads as a dialog that lost its contents.
    ///
    /// The list is the language registry every time it is built rather than
    /// once: building it on the press is what keeps this from being a second
    /// copy of that table.
    pub(super) fn render_language_menu(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.language_menu?;
        // The picker acts on the active pane, so a pane that stopped being a
        // file while the menu stood leaves nothing for the rows to pick for.
        self.active_editor()?;
        let this = cx.entity();

        // Every row is live, the one already in force included. A greyed row
        // runs nothing *and* leaves the menu standing — which is right for a
        // command that cannot be run and wrong for a list of answers, where the
        // obvious way to say "never mind" is to pick what is already picked. The
        // current language needs no mark of its own either: it is written on the
        // button this list is standing on, a row's height below it.
        let entries = languages::registry(cx)
            .all()
            .iter()
            .map(|entry| {
                let this = this.clone();
                let id = entry.id.clone();
                MenuEntry::new(language_label(entry)).on_activate(move |_window, cx| {
                    let id = id.clone();
                    this.update(cx, |workspace, cx| {
                        workspace.set_active_language(&id, cx);
                    });
                })
            })
            .collect();

        Some(
            ContextMenu::new("language-menu")
                .position(position)
                .anchor(Anchor::BottomLeft)
                .width(px(LANGUAGE_MENU_WIDTH))
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_language_menu(cx));
                }),
        )
    }

    /// Builds the status bar's character-encoding picker, if it is open.
    ///
    /// The file-type picker's twin in every mechanical respect — see
    /// [`Workspace::render_language_menu`] for why it stands on the pointer and
    /// grows upward, why every row is live and why none of them is marked. The
    /// rows are [`Charset::SUPPORTED`] and are not translated: `EUC-KR` and
    /// `windows-1252` are the names of the encodings themselves, and the same
    /// width fits them as fits `Dockerfile`.
    pub(super) fn render_charset_menu(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.charset_menu?;
        self.active_editor()?;
        let this = cx.entity();

        let entries = Charset::SUPPORTED
            .into_iter()
            .map(|charset| {
                let this = this.clone();
                MenuEntry::new(SharedString::new_static(charset.name())).on_activate(
                    move |_window, cx| {
                        this.update(cx, |workspace, cx| {
                            workspace.set_active_charset(charset, cx);
                        });
                    },
                )
            })
            .collect();

        Some(
            ContextMenu::new("charset-menu")
                .position(position)
                .anchor(Anchor::BottomLeft)
                .width(px(LANGUAGE_MENU_WIDTH))
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_charset_menu(cx));
                }),
        )
    }

    /// Renders the right end of the status bar while the keyboard is in a file:
    /// what it is being coloured as, what it is being decoded as, and where the
    /// caret is in it.
    ///
    /// The first two are controls and the position is not, which is why only
    /// they take a hover and a pointer cursor. The chevron points *up* because
    /// that is where the list opens, and it is what says the name is a button at
    /// all — a status bar is otherwise a place where nothing can be clicked.
    ///
    /// The order is the order they were added in: the file type keeps the place
    /// it has always had, the encoding takes the one next to it, and the caret
    /// stays at the far right where a number belongs. The two pickers are
    /// neighbours because they answer the same kind of question — how this file
    /// is being read — and neither is worth hunting for at the other end of the
    /// bar.
    pub(super) fn render_editor_status(
        &self,
        editor: &Entity<EditorPane>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let pane = editor.read(cx);
        let registry = languages::registry(cx);
        // An id the registry has never heard of cannot happen — the pane took
        // its own out of this table — but a button with nothing written on it
        // would be the worst possible way to find that out, so the id stands in.
        let language = registry.get(pane.language()).map_or_else(
            || SharedString::from(pane.language().to_string()),
            language_label,
        );
        let charset = SharedString::new_static(pane.charset().name());
        let (line, lines, column) = pane.caret_summary(cx);
        let this = cx.entity();

        // One recipe for both triggers, so they cannot drift apart on the bar:
        // the press handler is all that differs, and it is chained on after.
        let trigger = |id: &'static str, label: SharedString, tip: SharedString| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .h(px(18.))
                .px(px(6.))
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
                .tooltip(tooltip_label(tip))
                .child(div().whitespace_nowrap().child(label))
                .child(div().flex_none().text_size(px(8.)).child(CHEVRON_UP))
        };

        let open_language = this.clone();
        vec![
            trigger("status-language", language, ts!("editor.language_tip"))
                // On the press rather than on the click, so the list is up by
                // the time the button comes back up — the same moment every
                // other menu in the window opens at, and the reason a second
                // press lands on the backdrop and closes it again.
                .on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, _window, cx| {
                        let at = event.position;
                        open_language
                            .update(cx, |workspace, cx| workspace.open_language_menu(at, cx));
                    },
                )
                .into_any_element(),
            trigger("status-charset", charset, ts!("editor.charset_tip"))
                .on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, _window, cx| {
                        let at = event.position;
                        this.update(cx, |workspace, cx| workspace.open_charset_menu(at, cx));
                    },
                )
                .into_any_element(),
            div()
                .flex_none()
                .whitespace_nowrap()
                .child(caret_summary(line, lines, column))
                .into_any_element(),
        ]
    }

    /// Renders the bottom status bar.
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let (target, status, grid): (SharedString, SharedString, SharedString) =
            match self.tabs.get(self.active) {
                // The active pane, not the tab: on a split tab the bar reports
                // the session the keyboard is aimed at. A tab holding nothing
                // but open files reports as no session at all rather than
                // inventing a state for one that has gone.
                Some(tab) => match tab.active_session(cx) {
                    Some(session) => {
                        let session = session.read(cx);
                        let (cols, rows) = session.terminal().size();
                        (
                            session.label(),
                            session.status().summary(),
                            format!("{cols}x{rows}").into(),
                        )
                    }
                    None => (
                        ts!("statusbar.no_session"),
                        ts!("statusbar.idle"),
                        SharedString::new_static("-"),
                    ),
                },
                None => (
                    ts!("statusbar.no_session"),
                    ts!("statusbar.idle"),
                    // A dash standing in for the grid size: punctuation, not a
                    // word, so it is the same in every language.
                    SharedString::new_static("-"),
                ),
            };

        // The left of the bar speaks for the tab's session and the right for
        // the *pane*: a terminal reports the grid it is drawing, and a file
        // reports what it is being coloured as and where the caret is. The two
        // are never both worth showing, since only one surface has the keyboard.
        let trailing = match self.active_editor().cloned() {
            Some(editor) => self.render_editor_status(&editor, &theme, cx),
            None => vec![
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .child(grid)
                    .into_any_element(),
            ],
        };

        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(14.))
            .h(px(24.))
            .px(px(10.))
            // The bar is inert, so a press on it must not move the keyboard.
            // Without this the workspace root's `track_focus` would claim the
            // click, and the accent frame would jump to the active pane even
            // though no pane received focus.
            .on_any_mouse_down(|_, window, _cx| window.prevent_default())
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(div().flex_none().whitespace_nowrap().child(target))
            // The status summary carries the failure reason, which can be far
            // wider than the window; letting it shrink and ellipsize keeps the
            // right end of the bar pinned to the right edge instead of pushing
            // it out.
            .child(div().flex_1().min_w_0().truncate().child(status))
            .children(trailing)
            .into_any_element()
    }
}
