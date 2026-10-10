//! Navigation.

use super::*;

impl ConnectionDialog {
    /// The handle and bar state of one scrolling surface.
    pub(super) fn surface(&mut self, surface: Surface) -> (&ScrollHandle, &mut ScrollbarState) {
        match surface {
            Surface::Body => (&self.body_scroll, &mut self.body_scrollbar),
            Surface::List => (&self.list_scroll, &mut self.list_scrollbar),
        }
    }

    /// The same pair, for the renders that only read them.
    pub(super) fn surface_ref(&self, surface: Surface) -> (&ScrollHandle, &ScrollbarState) {
        match surface {
            Surface::Body => (&self.body_scroll, &self.body_scrollbar),
            Surface::List => (&self.list_scroll, &self.list_scrollbar),
        }
    }

    /// The overlay scroll indicator of one surface, as it stands.
    pub(super) fn scrollbar(&self, id: &'static str, surface: Surface) -> Scrollbar {
        let (handle, state) = self.surface_ref(surface);
        Scrollbar::for_handle(id, ScrollbarAxis::Vertical, handle).fade(state.fade())
    }

    /// The same bar, listening for the pointer reaching the edge it rides.
    ///
    /// Only the bars that are drawn need it: the one the drag path builds is
    /// there to be measured, and never reaches an element tree.
    pub(super) fn hovering_scrollbar(
        &self,
        id: &'static str,
        surface: Surface,
        cx: &mut Context<Self>,
    ) -> Scrollbar {
        self.scrollbar(id, surface).on_hover(cx.listener(
            move |dialog, hovered: &bool, _window, cx| {
                dialog.hover_scrollbar(surface, *hovered, cx);
            },
        ))
    }

    /// Puts each surface's bar up whenever it has been scrolled, and starts the
    /// clock that takes it down again.
    pub(super) fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            let (handle, state) = self.surface(surface);
            let scrolled = scrolled(handle, ScrollbarAxis::Vertical);
            if let Some(epoch) = state.moved(scrolled) {
                hide_later(epoch, cx, move |dialog| Some(dialog.surface(surface).1));
            }
        }
    }

    /// Scrolls whichever surface's thumb has been dragged.
    pub(super) fn drag_scrollbar(
        &mut self,
        event: &DragMoveEvent<DraggedThumb>,
        cx: &mut Context<Self>,
    ) {
        for (id, surface) in SCROLLBARS {
            let Some(progress) = self.scrollbar(id, surface).dragged(event, cx) else {
                continue;
            };

            let (handle, state) = self.surface(surface);
            state.hold();
            scroll_to(handle, ScrollbarAxis::Vertical, progress);
            cx.notify();
            return;
        }
    }

    /// Lets go of whichever thumb was being held, and starts its clock again.
    pub(super) fn release_scrollbars(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            if let Some(epoch) = self.surface(surface).1.release() {
                hide_later(epoch, cx, move |dialog| Some(dialog.surface(surface).1));
                cx.notify();
            }
        }
    }

    /// Puts one surface's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    ///
    /// Told which surface rather than asked to work it out: the profile column
    /// scrolls inside the body, so a pointer on the column's edge is inside both
    /// surfaces at once, and only the strip it actually reached knows which of
    /// the two bars was being asked for.
    pub(super) fn hover_scrollbar(
        &mut self,
        surface: Surface,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let state = self.surface(surface).1;
        if hovered {
            if state.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = state.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, move |dialog| {
            Some(dialog.surface(surface).1)
        });
    }

    /// Re-translate the placeholders of the fields that have a worded one.
    ///
    /// The text fields are built once, when the dialog is created, so their
    /// hints would otherwise stay in whatever language was active at start-up
    /// after the user switches. Called from every `open_*`, which is the only
    /// moment a stale hint could become visible.
    pub(super) fn refresh_placeholders(&self, cx: &mut Context<Self>) {
        self.password_input.update(cx, |input, cx| {
            input.set_placeholder(ts!("connection.password_placeholder"), cx);
        });
        self.passphrase_input.update(cx, |input, cx| {
            input.set_placeholder(ts!("connection.passphrase_placeholder"), cx);
        });
        for input in [
            &self.override_font_size_input,
            &self.override_scrollback_input,
            &self.override_term_input,
        ] {
            input.update(cx, |input, cx| {
                input.set_placeholder(ts!("connection.inherit_placeholder"), cx);
            });
        }
    }

    /// Show the dialog with an empty form.
    pub fn open_new(&mut self, cx: &mut Context<Self>) {
        self.reload_store();
        self.refresh_placeholders(cx);
        self.reset_form(cx);
        self.open = true;
        self.pending_focus = Some(FocusTarget::Host);
        cx.notify();
    }

    /// Show or hide `list`, revealing the current row as it opens so that the
    /// list does not have to be scrolled to find it.
    ///
    /// Opening one list closes the other, since both are drawn deferred and two
    /// open at once would paint over each other.
    pub(super) fn set_list_open(&mut self, list: OpenList, open: bool, cx: &mut Context<Self>) {
        self.open_list = open.then_some(list);
        if open {
            let (scroll, row) = match list {
                // Asked of the catalogue rather than of the swatches the list
                // is drawn from: the two are built from the same entries in the
                // same order, offset by the one "inherit" row that leads them.
                OpenList::Scheme => {
                    let row = match self.override_scheme.as_ref() {
                        // Nothing overridden: the "inherit" row that leads the
                        // list is the one in force.
                        None => 0,
                        Some(scheme) => {
                            let selected: &str = scheme;
                            TerminalTheme::all_schemes()
                                .iter()
                                .position(|entry| entry.id == selected)
                                .map_or(0, |index| index + 1)
                        }
                    };
                    (&self.scheme_scroll, row)
                }
                OpenList::Charset => (
                    &self.charset_scroll,
                    charset_row(self.override_charset.as_deref()),
                ),
            };
            scroll.scroll_to_item(row);
        }
        cx.notify();
    }

    /// Put whichever list is showing away, and say whether there was one to put
    /// away.
    ///
    /// A list is drawn deferred, over the rest of the form, so anything that
    /// takes the user elsewhere — `Escape`, or `Tab` off the trigger — has to
    /// close it rather than leave it painted with nobody driving it.
    pub(super) fn close_lists(&mut self, cx: &mut Context<Self>) -> bool {
        if self.open_list.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Replace the message strip with a single sentence.
    pub(super) fn set_status(&mut self, level: StatusLevel, message: impl Into<SharedString>) {
        self.set_status_lines(level, vec![message.into()]);
    }

    /// Replace the message strip with one sentence per line.
    pub(super) fn set_status_lines(&mut self, level: StatusLevel, lines: Vec<SharedString>) {
        self.status = Some(DialogStatus { level, lines });
    }

    /// `Tab`: move focus to the next control.
    ///
    /// gpui's tab ring wraps on its own — [`Window::focus_next`] falls back to
    /// the first stop once it runs off the end — so the only thing to add is
    /// closing the dropdown the focus may be leaving.
    pub(super) fn focus_next(
        &mut self,
        _: &FocusNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_lists(cx);
        window.focus_next(cx);
    }

    /// `Shift+Tab`: move focus to the previous control, wrapping to the last.
    pub(super) fn focus_prev(
        &mut self,
        _: &FocusPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_lists(cx);
        window.focus_prev(cx);
    }

    /// `Escape` dismisses the dialog from anywhere inside it.
    ///
    /// A row menu or an open dropdown takes the key first and only undoes
    /// itself: backing out of one must not also throw away the form behind it,
    /// which is how the workspace layers its own menus over the dialogs.
    pub(super) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.open || event.keystroke.key != "escape" {
            return;
        }
        cx.stop_propagation();
        if self.close_context(cx) || self.close_lists(cx) {
            return;
        }
        self.dismiss(cx);
    }

    /// Open the context menu of the saved profile `id`, with its corner at `at`.
    ///
    /// The right-click deliberately does not also load the profile into the
    /// form, the way a left-click does: the menu's own Connect and Edit rows
    /// say so explicitly, and a menu that had already overwritten the form
    /// would leave a user who dismisses it worse off than before.
    pub(super) fn open_context(&mut self, id: Uuid, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.context = Some((id, at));
        cx.notify();
    }

    /// Put the row menu away, and say whether there was one to put away.
    pub(super) fn close_context(&mut self, cx: &mut Context<Self>) -> bool {
        if self.context.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }
}
