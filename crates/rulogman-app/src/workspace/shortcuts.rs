//! Shortcuts.
//!
//! The application menu's *Keyboard shortcuts* sheet, and its *User guide*
//! row. The sheet is written from the same constants the bindings and the menu
//! hints are, so the three cannot drift apart, and its descriptions are the
//! menu's own words wherever the menu has a row for the command.

use super::*;
use crate::terminal_view::{COPY_SHORTCUT, PASTE_SHORTCUT, SEARCH_SHORTCUT};
use rugpui::TextSize;

/// Where *User guide* goes: the manual, as the repository publishes it.
const USER_GUIDE_URL: &str = "https://github.com/xcomart/rulogman/blob/main/docs/user-guide.md";

/// Width of the sheet.
const SHEET_WIDTH: f32 = 560.;

/// Height at which the sheet's list starts scrolling.
const SHEET_MAX_HEIGHT: f32 = 460.;

/// Width of the key column.
const KEY_COLUMN: f32 = 200.;

/// The sheet's groups, each a heading and its rows of key and command, in the
/// order the guide's own table lists them.
pub(super) fn shortcut_groups() -> Vec<(SharedString, Vec<(SharedString, SharedString)>)> {
    let key = |text: &str| SharedString::from(text.to_owned());
    let pane = PANE_SHORTCUT_MODIFIER;
    let modifier = SHORTCUT_MODIFIER;
    vec![
        (
            ts!("shortcuts.group_window"),
            vec![
                (key(NEW_SESSION_SHORTCUT_LABEL), ts!("menu.new_session")),
                (key(WINDOW_SHORTCUT_LABEL), ts!("menu.new_window")),
                (
                    key(CLOSE_SESSION_SHORTCUT_LABEL),
                    ts!("terminal.menu_close_pane"),
                ),
                (key(NEXT_TAB_SHORTCUT_LABEL), ts!("menu.next_tab")),
                (key(PREV_TAB_SHORTCUT_LABEL), ts!("menu.prev_tab")),
                (
                    key(&format!("{modifier}+1\u{2026}9")),
                    ts!("shortcuts.select_tab"),
                ),
                (
                    key(&format!("{modifier}+Alt+1\u{2026}9")),
                    ts!("shortcuts.open_dashboard"),
                ),
                (key(PANEL_SHORTCUT_LABEL), ts!("files.toggle")),
                (key(&format!("{modifier}+,")), ts!("menu.settings")),
                (key("Esc"), ts!("shortcuts.dismiss")),
                (key(QUIT_SHORTCUT_LABEL), ts!("menu.quit")),
            ],
        ),
        (
            ts!("shortcuts.group_panes"),
            vec![
                (key(&format!("{pane}+]")), ts!("menu.focus_next_pane")),
                (key(&format!("{pane}+[")), ts!("menu.focus_prev_pane")),
                (key("Ctrl+Tab"), ts!("menu.focus_next_pane")),
                (key("Ctrl+1\u{2026}9"), ts!("shortcuts.focus_pane")),
                (key(&format!("{pane}+Shift+D")), ts!("menu.duplicate_right")),
                (key(&format!("{pane}+Shift+S")), ts!("menu.duplicate_below")),
                (key(&format!("{pane}+Shift+B")), ts!("menu.break_out_pane")),
                (key(&format!("{pane}+Shift+N")), ts!("menu.tab_to_window")),
                (key(&format!("{modifier}+Shift+L")), ts!("tab.save_layout")),
            ],
        ),
        (
            ts!("shortcuts.group_terminal"),
            vec![
                (key(COPY_SHORTCUT), ts!("terminal.menu_copy")),
                (key(PASTE_SHORTCUT), ts!("terminal.menu_paste")),
                (key(SEARCH_SHORTCUT), ts!("terminal.menu_find")),
                (key("Shift+PgUp / PgDn"), ts!("shortcuts.scroll_page")),
                (key("Shift+Home / End"), ts!("shortcuts.scroll_ends")),
            ],
        ),
        (
            ts!("shortcuts.group_editor"),
            vec![
                (key(&format!("{modifier}+S")), ts!("common.save")),
                (key(&format!("{modifier}+F")), ts!("editor.find")),
                (key(&format!("{modifier}+H")), ts!("editor.replace")),
            ],
        ),
    ]
}

impl Workspace {
    /// Shows the shortcut sheet.
    pub(super) fn show_shortcuts(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.shortcuts_open = true;
        cx.notify();
    }

    /// Puts the shortcut sheet away.
    pub(super) fn close_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.shortcuts_open) {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Opens the user guide in the browser.
    pub(super) fn open_user_guide(&mut self, cx: &mut Context<Self>) {
        cx.open_url(USER_GUIDE_URL);
    }

    /// Renders the shortcut sheet, while it is open.
    pub(super) fn render_shortcuts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.shortcuts_open {
            return None;
        }
        let theme = theme(cx);
        let this = cx.entity();
        let groups = shortcut_groups()
            .into_iter()
            .enumerate()
            .map(|(index, (heading, rows))| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .when(index > 0, |group| group.pt(px(10.)))
                    .child(
                        div()
                            .pb(px(2.))
                            .text_size(TextSize::Xs.size(cx))
                            .text_color(theme.text_muted)
                            .child(heading),
                    )
                    .children(rows.into_iter().map(|(key, action)| {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.))
                            .text_size(TextSize::Sm.size(cx))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(KEY_COLUMN))
                                    .text_color(theme.accent)
                                    .child(key),
                            )
                            .child(div().flex_1().min_w_0().child(action))
                    }))
            });
        let body = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .id("shortcut-sheet")
                    .flex()
                    .flex_col()
                    .max_h(px(SHEET_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .children(groups),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("shortcut-sheet-guide", ts!("menu.user_guide"))
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, _window, cx| {
                                workspace.open_user_guide(cx);
                            })),
                    )
                    .child(
                        Button::new("shortcut-sheet-close", ts!("common.close"))
                            .variant(ButtonVariant::Primary)
                            .focus_handle(&self.question_focus)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.close_shortcuts(window, cx);
                            })),
                    ),
            );
        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    modal(
                        "shortcut-sheet-modal",
                        ts!("menu.shortcuts"),
                        px(SHEET_WIDTH),
                        body,
                        move |window, cx| {
                            this.update(cx, |workspace, cx| workspace.close_shortcuts(window, cx));
                        },
                    )
                    .initial_focus(&self.question_focus)
                    .escape_dismisses(true),
                )
                .into_any_element(),
        )
    }
}
