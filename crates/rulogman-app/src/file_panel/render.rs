//! Render.

use super::*;

impl Render for FilePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        self.apply_pending_focus(window, cx);
        self.watch_list_scroll(cx);
        let state = self
            .session
            .as_ref()
            .and_then(|session| self.states.get(&session.entity_id()));

        let header = self.render_header(state, cx);
        let list = self.render_list(state, cx);
        let notice = self.render_notice(state, cx);
        let context = self.render_context(state, cx);
        let accent = theme.accent;
        // Asked of the focus tree here rather than remembered from a focus
        // listener: listeners run at the tail of a draw, so a remembered flag
        // would light the frame one input event late, while `window.focus`
        // itself schedules the repaint this read then sees. `contains_focused`
        // rather than `is_focused` so that typing in the rename field — a
        // handle nested under this one — still counts as the panel having the
        // keyboard.
        let focused = self.focus_handle.contains_focused(window, cx);

        // The same band the terminal splits are dragged by, so the panel's edge
        // answers a pointer the way every other seam in the window does: an
        // invisible grab area that takes the press, and an accent bar inside it
        // that fades in while the pointer is on it or holding it.
        //
        // `at_end` keeps the whole band inside the panel, which is what lets it
        // be added last and win the hit test against the rows it covers.
        // Straddling the border would put half the grab area over the pane next
        // door, which is drawn after the panel and would take those pixels
        // back; it would also leave the bar floating a few pixels in from the
        // hairline instead of landing on it.
        let handle = ResizeHandle::new("file-panel-edge", gpui::Axis::Horizontal, DraggedPanelEdge)
            .at_end()
            .thickness(px(PANEL_HANDLE));

        div()
            .id("file-panel")
            // Makes the panel a focus target, so a click anywhere in it moves
            // the keyboard off the terminal instead of leaving focus behind
            // where it was.
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(self.width))
            .h_full()
            .min_h_0()
            // The panel is the only thing covering these pixels, so this is
            // where the window opacity lands on them. Exactly one such fill per
            // pixel — see `app_settings::window_tint`.
            .bg(app_settings::window_tint(theme.background, cx))
            // A full hairline rather than just the divider on the right, so the
            // drop highlight below can recolour a frame the user can see
            // without adding a second tinted fill over the panel. It doubles as
            // the focus indication, the same accent frame the panes carry — the
            // workspace drops the active pane's accent while this one is lit,
            // so only ever one frame claims the keyboard.
            .border_1()
            .border_color(if focused { accent } else { theme.border })
            // Refined over the base style, so a drag hovering the panel keeps
            // the accent whether or not the panel also holds focus.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(accent))
            .can_drop(|dragged, _window, _cx| {
                <dyn Any>::downcast_ref::<ExternalPaths>(dragged).is_some()
            })
            .on_drop(cx.listener(|panel, paths: &ExternalPaths, _window, cx| {
                panel.upload(paths.paths().to_vec(), cx);
            }))
            // Listening on the panel, not on the handle: the handle slides out
            // from under the pointer as the drag goes on, while the panel's left
            // edge — the one the new width is measured from — stays put.
            .on_drag_move::<DraggedPanelEdge>(
                cx.listener(|panel, event, _window, cx| panel.drag_edge(event, cx)),
            )
            // Same reasoning one step over: the listing's thumb slides out from
            // under the pointer, and the panel is what stays mounted for the
            // whole gesture.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |panel, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    panel.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|panel, _: &MouseUpEvent, _window, cx| panel.release_scrollbar(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|panel, _: &MouseUpEvent, _window, cx| panel.release_scrollbar(cx)),
            )
            .child(header)
            .child(list)
            .children(notice)
            .child(handle)
            .children(context)
    }
}

/// The frame of the strip along the bottom of the panel.
///
/// Shared by the status line and the question below it so that the two never
/// draw two borders, two paddings or two hairlines between them: whatever is
/// showing, there is exactly one strip.
pub(super) fn notice_strip(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(4.))
        .w_full()
        .min_w_0()
        .px(px(8.))
        .py(px(4.))
        .border_t_1()
        .border_color(theme.border)
}

/// A centred message standing in for a listing.
pub(super) fn placeholder(message: SharedString, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .px(px(12.))
        .text_size(px(11.))
        .text_color(theme.text_muted)
        .child(message)
        .into_any_element()
}

/// A compact icon-only toolbar button, in the style of the tab strip's own.
pub(super) fn icon_button(
    id: impl Into<ElementId>,
    path: &'static str,
    tip: SharedString,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = theme.surface_hover;
    let text = theme.text;
    // The icon tint rather than the muted text of the labels around the panel:
    // these buttons are all mark and no word. A disabled one is faded from the
    // same colour, so the two states stay one family.
    let color = if enabled {
        theme.icon
    } else {
        theme.icon.opacity(0.4)
    };

    div()
        .id(id.into())
        .group(BUTTON_GROUP)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded_sm()
        // Outside the `enabled` gate on purpose. These buttons carry no text, so
        // a dimmed one is a glyph with no explanation at all — and "what would
        // this have done?" is exactly the question a user has when a button will
        // not take a click. gpui builds the tooltip's hitbox from the tooltip
        // alone, so this keeps working with every listener below removed.
        .tooltip(tooltip_label(tip))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(move |event, window, cx| on_click(event, window, cx))
        })
        .child(
            icons::icon(path, px(TOOLBAR_ICON), color).when(enabled, |icon| {
                icon.group_hover(BUTTON_GROUP, move |style| style.text_color(text))
            }),
        )
}

impl FilePanel {
    /// Renders the header: the current path and the action buttons.
    pub(super) fn render_header(
        &self,
        state: Option<&SessionState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let is_local = self.showing_local(cx);
        let path = state.and_then(|state| state.path.as_deref());
        let ready = state.is_some_and(|state| state.path.is_some());
        let selected = state.map_or(0, SessionState::selected_count);
        // Directories included: a selected folder is copied whole.
        let downloadable = selected > 0;
        // The same rules the context menu applies, so that a command is offered
        // in exactly one shape whichever way it is reached: renaming needs one
        // target and only one, deleting takes as many as are selected.
        let renameable = selected == 1;
        let deletable = selected > 0;

        let title = match path {
            Some(path) => self.render_crumbs(path, &theme, cx),
            // Nothing is listed yet, so there is no path to break up and the
            // header carries the panel's own name instead.
            None => {
                // Mirrors the status bar: `truncate` needs a row flexing the
                // text child, not a bare `w_full`, to resolve its width.
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(ts!(key(is_local, "files.title", "files.local.title"))),
                    )
                    .into_any_element()
            }
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(4.))
            .px(px(HEADER_PADDING))
            .py(px(6.))
            .border_b_1()
            .border_color(theme.border)
            .child(title)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.))
                    .child(icon_button(
                        "file-panel-refresh",
                        icons::REFRESH,
                        ts!("files.tip_refresh"),
                        self.session.is_some(),
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.refresh(cx)),
                    ))
                    .child(icon_button(
                        "file-panel-new-folder",
                        icons::NEW_FOLDER,
                        ts!("files.tip_new_folder"),
                        ready,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.begin_new_folder(cx);
                        }),
                    ))
                    .child(icon_button(
                        "file-panel-upload",
                        icons::UPLOAD,
                        ts!(key(is_local, "files.tip_upload", "files.local.tip_copy_in")),
                        ready,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.pick_upload(false, cx);
                        }),
                    ))
                    .child(icon_button(
                        "file-panel-upload-folder",
                        icons::UPLOAD_FOLDER,
                        ts!(key(
                            is_local,
                            "files.tip_upload_folder",
                            "files.local.tip_copy_in_folder"
                        )),
                        ready,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.pick_upload(true, cx);
                        }),
                    ))
                    .child(icon_button(
                        "file-panel-download",
                        icons::DOWNLOAD,
                        ts!(key(
                            is_local,
                            "files.tip_download",
                            "files.local.tip_copy_out"
                        )),
                        ready && downloadable,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.download(cx)),
                    ))
                    .child(icon_button(
                        "file-panel-rename",
                        icons::RENAME,
                        ts!("files.tip_rename"),
                        ready && renameable,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.begin_rename(cx)),
                    ))
                    // Last, and deliberately: the row starts with the button
                    // pressed most often and ends with the one that cannot be
                    // undone, so a click that lands a button early hits a
                    // refresh rather than a delete.
                    .child(icon_button(
                        "file-panel-delete",
                        icons::DELETE,
                        ts!("files.tip_delete"),
                        ready && deletable,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.confirm_delete(cx)),
                    )),
            )
            .into_any_element()
    }

    /// Renders the current path as a row of pressable pieces.
    ///
    /// Each piece opens a menu of the directories beside it, which is what
    /// makes the header a way of *moving* rather than a label: the way out of
    /// `/srv/app/releases/2026-07-30` into last week's release is one press on
    /// the last piece, not four double-clicks through `..`.
    ///
    /// The row wraps rather than truncating. [`crumbs`] has already folded away
    /// what the panel's own width could not hold, but [`fold_budget`] is an
    /// estimate over a proportional font; wrapping costs a line of header, while
    /// truncating would cost the leaf directory — the one piece the user needs
    /// to see. A drag of the panel's edge repaints, so the fold follows the
    /// width as it moves.
    pub(super) fn render_crumbs(
        &self,
        path: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let crumbs = crumbs(path, fold_budget(self.width));
        // Taken before the pieces are consumed: a separator belongs in front of
        // every piece except the first and those following the root, whose own
        // label is already the slash that would go there.
        let separators: Vec<bool> = std::iter::once(false)
            .chain(
                crumbs
                    .windows(2)
                    .map(|pair| needs_separator(&pair[0].label)),
            )
            .collect();
        let hover = theme.surface_hover;
        let text = theme.text;
        // Fainter than the pieces on either side of it: a separator is
        // punctuation, and the header's job is to read as a path with parts
        // rather than as a row of equally loud buttons.
        let separator = theme.text_muted.opacity(0.6);

        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .w_full()
            .min_w_0()
            .text_size(px(11.))
            .text_color(theme.text_muted);

        for (index, crumb) in crumbs.into_iter().enumerate() {
            if separators.get(index).copied().unwrap_or_default() {
                row = row.child(
                    div()
                        .flex_none()
                        .text_color(separator)
                        .child(CRUMB_SEPARATOR),
                );
            }
            let menu = crumb.menu;
            row = row.child(
                div()
                    .id(ElementId::from(("file-crumb", index)))
                    .flex_none()
                    .px(px(2.))
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(text))
                    // A press rather than a click, for the position: a menu has
                    // to hang where the pointer is, and the press is where the
                    // right-click menus below take theirs from too.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, event: &MouseDownEvent, _window, cx| {
                            panel.open_crumb(menu.clone(), event.position, cx);
                        }),
                    )
                    .child(crumb.label),
            );
        }

        row.into_any_element()
    }

    /// Renders the directory listing, or the placeholder standing in for it.
    pub(super) fn render_list(
        &self,
        state: Option<&SessionState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let connected = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).files(cx).is_some());

        let Some(state) = state.filter(|state| state.path.is_some()) else {
            let message = if self.session.is_none() {
                ts!("files.no_session")
            } else if !connected {
                // The one wording chosen before a source exists to ask: a
                // session that has not started has no `FileSource` yet, and it
                // is its transport that decides which sentence is true.
                ts!(key(
                    self.showing_local(cx),
                    "files.not_connected",
                    "files.local.not_connected"
                ))
            } else {
                ts!("files.loading")
            };
            return placeholder(message, &theme);
        };

        // Safe by the filter above; kept as a match so a future change cannot
        // turn this into a panic.
        let path = state.path.as_deref().unwrap_or_default();
        let mut rows: Vec<AnyElement> = Vec::with_capacity(state.entries.len() + 1);

        if !is_root(path) && !path.is_empty() {
            rows.push(self.render_row(
                ElementId::from("file-row-parent"),
                PARENT_NAME,
                true,
                false,
                None,
                false,
                &theme,
                cx,
            ));
        }
        for (index, entry) in state.entries.iter().enumerate() {
            let size = (!entry.is_dir).then(|| SharedString::from(format_size(entry.size)));
            rows.push(self.render_row(
                ElementId::from(("file-row", index)),
                &entry.name,
                entry.is_dir,
                entry.is_symlink,
                size,
                state.selected.contains(&entry.name),
                &theme,
                cx,
            ));
        }

        // The placeholder goes *inside* the scroll box rather than replacing
        // it, so that an empty directory still has a background to right-click
        // — which is the only way to upload into one without the toolbar.
        if rows.is_empty() {
            rows.push(placeholder(ts!("files.empty"), &theme));
        }

        // The wrapper is what the overlay bar is placed against, and exists only
        // for that: the scrolling box cannot hold its own bar, because its
        // children are what scroll away.
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("file-panel-list")
                    .flex()
                    .flex_col()
                    .size_full()
                    .py(px(2.))
                    .overflow_y_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&state.scroll)
                    // Reached by the `..` row and by empty space alike: neither
                    // has an entry behind it, and both mean "do something to
                    // this directory".
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|panel, event: &MouseDownEvent, _window, cx| {
                            panel.open_context(None, event.position, cx);
                        }),
                    )
                    .children(rows),
            )
            .children(
                Self::scrollbar(state)
                    .on_hover(cx.listener(|panel, hovered: &bool, _window, cx| {
                        panel.hover_scrollbar(*hovered, cx);
                    }))
                    .render(&theme),
            )
            .into_any_element()
    }

    /// Renders one row of the listing.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_row(
        &self,
        id: ElementId,
        name: &str,
        is_dir: bool,
        is_symlink: bool,
        size: Option<SharedString>,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = SharedString::from(name.to_owned());
        let parent = name == PARENT_NAME;
        let owned = label.clone();
        let clicked = label.clone();
        // Recomputed every frame rather than cached, exactly as the breadcrumb's
        // budget is: the answer depends on the panel's width, and the width
        // changes continuously while the edge is being dragged.
        let clipped = name_is_clipped(
            self.width,
            name,
            is_symlink,
            size.as_ref().map(SharedString::as_ref),
        );

        div()
            .id(id)
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .w_full()
            .px(px(8.))
            .py(px(3.))
            .text_size(px(12.))
            .text_color(theme.text)
            .cursor_pointer()
            .when(selected, |row| row.bg(theme.surface_active))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme.surface_hover))
            })
            // On the row rather than on the label: the label is a bare `div`
            // with no id, and giving it one to carry a tooltip would add a
            // second hitbox over every row for no gain — gpui places the
            // tooltip at the pointer either way, and the pointer is over the
            // name whenever the name is what the user is reading.
            .when(clipped, |row| row.tooltip(tooltip_label(label.clone())))
            .on_click(cx.listener(move |panel, event: &ClickEvent, _window, cx| {
                // A double click arrives as two events, so the first one has
                // already selected the row by the time this opens it.
                if event.click_count() >= 2 {
                    if parent {
                        panel.open_parent(cx);
                    } else {
                        panel.activate(&owned, cx);
                    }
                } else if !parent {
                    panel.select(&owned, event.modifiers(), cx);
                }
            }))
            // The `..` row deliberately has no handler of its own: letting the
            // press bubble to the list is what makes right-clicking it mean the
            // same as right-clicking empty space.
            .when(!parent, |row| {
                row.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |panel, event: &MouseDownEvent, _window, cx| {
                        // The press belongs to this row, not to the list under
                        // it, which would take it as a click on the background.
                        cx.stop_propagation();
                        panel.open_context(Some(&clicked), event.position, cx);
                    }),
                )
            })
            // The accent on directories is what makes a listing scannable at a
            // glance: it separates the folders from the files ahead of the
            // sort order, in both themes.
            .child(if is_dir {
                icons::icon(icons::FOLDER, px(ROW_ICON), theme.accent)
            } else {
                icons::icon(icons::FILE, px(ROW_ICON), theme.icon)
            })
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when(is_symlink, |row| {
                row.child(icons::icon(icons::SYMLINK, px(BADGE_ICON), theme.icon))
            })
            .children(size.map(|size| {
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(size)
            }))
            .into_any_element()
    }

    /// Moves the keyboard into a question's text field the frame after it
    /// appears.
    ///
    /// The field is created inside a menu callback, where it has not been laid
    /// out yet; focusing it there would put the caret in a box that does not
    /// exist. Doing it from the render that first draws it is the same trick
    /// the connection dialog uses to focus its host field.
    pub(super) fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut App) {
        if !self.focus_prompt {
            return;
        }
        self.focus_prompt = false;
        let Some(state) = self
            .session
            .as_ref()
            .and_then(|session| self.states.get(&session.entity_id()))
        else {
            return;
        };
        if let Some(input) = state.prompt.as_ref().and_then(Prompt::field) {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Renders the open menu, if there is one.
    ///
    /// Three menus, picked by where the press landed: the two the listing
    /// offers, and the directories a breadcrumb piece can be swapped for. A row
    /// whose command would be refused is left out rather than shown greyed:
    /// "Rename…" appears only over a selection of exactly one, because renaming
    /// several things to one name is not a thing to offer and then decline.
    ///
    /// The listing menus come in three groups, separated in that order: what
    /// the press was on, what can be taken off it as text, and what acts on the
    /// listing as a whole. A group left empty contributes no rule of its own.
    pub(super) fn render_context(
        &self,
        state: Option<&SessionState>,
        cx: &mut Context<Self>,
    ) -> Option<ContextMenu> {
        let menu = self.context.as_ref()?;
        let selected = state.map_or(0, SessionState::selected_count);
        let is_local = self.showing_local(cx);
        let this = cx.entity();

        let on_rows = match &menu.kind {
            MenuKind::Listing { on_rows } => *on_rows,
            // Nothing but destinations: a breadcrumb dropdown is navigation,
            // and the commands the listing menus carry act on a selection this
            // menu was never about.
            MenuKind::Crumb(targets) => {
                let entries = targets
                    .iter()
                    .map(|target| {
                        let this = this.clone();
                        let path = target.path.clone();
                        MenuEntry::new(target.label.clone()).on_activate(move |_window, cx| {
                            let path = path.clone();
                            this.update(cx, |panel, cx| panel.open_path(path, cx));
                        })
                    })
                    .collect();
                return Some(
                    ContextMenu::new("file-panel-context")
                        .position(menu.at)
                        .entries(entries)
                        .on_dismiss(move |_window, cx| {
                            this.update(cx, |panel, cx| panel.close_context(cx));
                        }),
                );
            }
        };

        // The one selected entry, when there is exactly one. Every row phrased
        // in the singular hangs off it, and the kind it carries is what tells
        // an openable directory from a file.
        let only = state.and_then(SessionState::only_selected);

        let mut primary = Vec::new();
        let mut clipboard = Vec::new();
        if on_rows && selected > 0 {
            // What a double-click on the row would have done, said out loud —
            // and offered only where it would do anything, which is over a
            // single directory.
            if let Some(name) = only.filter(|entry| entry.is_dir).map(|entry| &entry.name) {
                let name = name.clone();
                primary.push(MenuEntry::new(ts!("files.menu_open")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        let name = name.clone();
                        this.update(cx, |panel, cx| panel.activate(&name, cx));
                    }
                }));
            }
            // The file counterpart of `menu_open`, and deliberately in the same
            // slot: over a directory the obvious thing to do is go into it,
            // over a file it is to look inside it. Only over one file, because
            // the row opens one pane, and never over a directory, which has no
            // contents a text buffer could hold.
            if only.is_some_and(|entry| !entry.is_dir) {
                primary.push(MenuEntry::new(ts!("files.menu_edit")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.edit(cx));
                    }
                }));
            }
            let copy_out = key(is_local, "files.menu_download", "files.local.menu_copy_out");
            primary.push(MenuEntry::new(ts!(copy_out)).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.download(cx));
                }
            }));
            if only.is_some() {
                primary.push(MenuEntry::new(ts!("files.menu_rename")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.begin_rename(cx));
                    }
                }));
            }
            primary.push(MenuEntry::new(ts!("files.menu_delete")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.confirm_delete(cx));
                }
            }));

            // A group of their own: these two move nothing and ask nothing,
            // they only hand the entry's name — or the whole path an operation
            // would have used — to whatever the user pastes into next.
            if only.is_some() {
                clipboard.push(MenuEntry::new(ts!("files.menu_copy_name")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.copy_name(cx));
                    }
                }));
                clipboard.push(MenuEntry::new(ts!("files.menu_copy_path")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.copy_path(cx));
                    }
                }));
            }
        } else {
            // First, the way every file manager orders this menu: creating is
            // the one command here that acts on the directory itself rather
            // than moving something into it.
            primary.push(MenuEntry::new(ts!("files.menu_new_folder")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.begin_new_folder(cx));
                }
            }));
            for (label, folders) in [
                (
                    ts!(key(
                        is_local,
                        "files.menu_upload",
                        "files.local.menu_copy_in"
                    )),
                    false,
                ),
                (
                    ts!(key(
                        is_local,
                        "files.menu_upload_folder",
                        "files.local.menu_copy_in_folder"
                    )),
                    true,
                ),
            ] {
                let this = this.clone();
                primary.push(MenuEntry::new(label).on_activate(move |_window, cx| {
                    this.update(cx, |panel, cx| panel.pick_upload(folders, cx));
                }));
            }
        }

        // Both of these speak for the listing rather than for whatever the
        // press landed on, so they are offered from either menu. Selecting
        // everything is left out once everything already is — there is nothing
        // for the row to change — and over an empty directory, which has
        // nothing to select.
        let listed = state.map_or(0, |state| state.entries.len());
        let mut whole = Vec::new();
        if listed > 0 && selected < listed {
            whole.push(MenuEntry::new(ts!("files.menu_select_all")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.select_all(cx));
                }
            }));
        }
        whole.push(MenuEntry::new(ts!("files.menu_refresh")).on_activate({
            let this = this.clone();
            move |_window, cx| {
                this.update(cx, |panel, cx| panel.refresh(cx));
            }
        }));

        let mut entries = Vec::new();
        for group in [primary, clipboard, whole] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::separator());
            }
            entries.extend(group);
        }

        Some(
            ContextMenu::new("file-panel-context")
                .position(menu.at)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |panel, cx| panel.close_context(cx));
                }),
        )
    }

    /// Renders the open question — a delete confirmation, or a field naming
    /// something — if there is one.
    ///
    /// All of them sit under the listing rather than over the window, so the
    /// rows they are about stay readable while the question is being answered.
    pub(super) fn render_prompt(
        &self,
        state: Option<&SessionState>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (question, confirm, body) = match state?.prompt.as_ref()? {
            Prompt::Delete(names) => {
                let question = match names.as_slice() {
                    [only] => ts!("files.delete_confirm_one", name = only.clone()),
                    names => ts!("files.delete_confirm", count = names.len()),
                };
                (
                    question,
                    Button::new("file-panel-delete", ts!("files.delete"))
                        .variant(ButtonVariant::Danger)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.delete(cx);
                        })),
                    None,
                )
            }
            Prompt::Rename { from, input } => {
                let field = input.clone();
                (
                    ts!("files.rename_prompt", name = from.clone()),
                    Button::new("file-panel-rename", ts!("files.rename")).on_click(cx.listener(
                        move |panel, _: &ClickEvent, _window, cx| {
                            // Safe to read here, unlike in the field's own
                            // `Enter` handler: this runs from the panel's
                            // update, so the field itself is not leased.
                            let typed = field.read(cx).content().to_owned();
                            panel.commit_rename(&typed, cx);
                        },
                    )),
                    Some(input.clone()),
                )
            }
            Prompt::NewFolder { input } => {
                let field = input.clone();
                (
                    ts!("files.new_folder_prompt"),
                    Button::new("file-panel-create", ts!("files.create")).on_click(cx.listener(
                        move |panel, _: &ClickEvent, _window, cx| {
                            let typed = field.read(cx).content().to_owned();
                            panel.commit_new_folder(&typed, cx);
                        },
                    )),
                    Some(input.clone()),
                )
            }
        };

        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(px(6.))
                .w_full()
                .min_w_0()
                .child(
                    // The header's recipe, for the header's reason: `truncate`
                    // resolves its width from a row flexing the text child, and
                    // a bare `w_full` leaves it with nothing to measure against
                    // — the whole line then collapses to an ellipsis.
                    div().flex().flex_row().w_full().child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text)
                            .child(question),
                    ),
                )
                .children(body)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        // Wraps rather than overflowing: the panel can be
                        // dragged down to 180px, and a locale that spells
                        // "Cancel" and "Rename" long enough would otherwise
                        // push a button out past the panel's own border.
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap(px(6.))
                        .child(
                            Button::new("file-panel-prompt-cancel", ts!("common.cancel"))
                                .variant(ButtonVariant::Secondary)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                                    panel.cancel_prompt(cx);
                                })),
                        )
                        .child(confirm),
                )
                .into_any_element(),
        )
    }

    /// Renders the status line, when there is anything to say.
    ///
    /// A running transfer outranks everything else here: it is the only state
    /// the user can neither see in the listing nor guess at, and while it lasts
    /// the line carries a progress bar under it. An open question is drawn
    /// *under* whatever the line says, so a refused name explains itself right
    /// above the field it was typed into.
    pub(super) fn render_notice(
        &self,
        state: Option<&SessionState>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = theme(cx);
        let prompt = self.render_prompt(state, &theme, cx);
        let state = state?;
        let transfer = state.transfer.as_ref();
        let (text, color) = match (transfer, &state.notice, state.busy) {
            (Some(transfer), _, _) => (transfer.line(state.is_local), theme.text_muted),
            (None, Some(Notice::Error(text)), _) => (text.clone(), theme.danger),
            (None, Some(Notice::Info(text)), _) => (text.clone(), theme.text_muted),
            (None, None, true) => (ts!("files.loading"), theme.text_muted),
            (None, None, false) => {
                // Nothing to report, but a question may still be waiting; it
                // owns the whole strip in that case.
                return prompt.map(|prompt| {
                    notice_strip(&theme)
                        .text_size(px(11.))
                        .child(prompt)
                        .into_any_element()
                });
            }
        };

        // The track is the same hairline colour as the panel's own borders, so
        // an idle-looking bar reads as part of the frame rather than as a
        // control; only the accent fill claims attention.
        let bar = transfer.map(|transfer| {
            div()
                .flex_none()
                .w_full()
                .h(px(PROGRESS_BAR))
                .rounded_sm()
                .bg(theme.border)
                .child(
                    div()
                        .h_full()
                        .w(relative(transfer.fraction()))
                        .rounded_sm()
                        .bg(theme.accent),
                )
        });

        Some(
            notice_strip(&theme)
                .text_size(px(11.))
                .text_color(color)
                // Same recipe as the header: `truncate` needs a row flexing the
                // text child to resolve its width against, and a bare `w_full`
                // gives it none, so the whole line collapses to an ellipsis.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .w_full()
                        .child(div().flex_1().min_w_0().truncate().child(text)),
                )
                .children(bar)
                .children(prompt)
                .into_any_element(),
        )
    }
}
