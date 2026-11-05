//! Importing hosts from OpenSSH's own `~/.ssh/config` into the profile list.
//!
//! The reading is `rulogman_core::ssh_config`'s; what lives here is the
//! checklist that stands over the dialog while the user picks which hosts to
//! keep, and the write that turns the ticked ones into saved profiles.

use super::*;
use rugpui::{Checkbox, ScrollbarExt as _, TextSize};
use rulogman_core::ssh_config::{
    IMPORT_FOLDER, ImportContext, ImportNote, ImportedHost, default_config_path, read_config,
};

/// Width of the checklist panel.
const IMPORT_WIDTH: f32 = 600.;

/// Tallest the host list grows before it scrolls.
const IMPORT_LIST_MAX_HEIGHT: f32 = 360.;

/// First tab index of the checklist, past everything the dialog numbers.
const TAB_IMPORT: isize = 20_000;

/// The checklist, while it is up.
pub(super) struct SshImport {
    /// The file that was read, or would have been.
    path: PathBuf,
    /// One row per host the file names, in its order.
    rows: Vec<ImportRow>,
    /// How many `Match` blocks were skipped.
    match_blocks: usize,
    /// Included files that could not be read.
    unreadable: Vec<PathBuf>,
    /// Why nothing could be read at all, when nothing could.
    error: Option<SharedString>,
    /// The button the keyboard starts on: Import, or Close when there is
    /// nothing to import.
    focus: FocusHandle,
    /// The host list's scroll position.
    scroll: ScrollHandle,
}

/// One host of the checklist.
struct ImportRow {
    /// What the host would become.
    host: ImportedHost,
    /// Whether it is ticked to be imported.
    checked: bool,
    /// The name of a saved profile that already reaches the same place, if
    /// one does.
    existing: Option<String>,
}

/// The name of a saved profile reaching the same user, host and port as
/// `profile`, if there is one.
///
/// The host compared ignoring case, as host names are. That is what makes a
/// second import of the same file offer nothing new, and an import over
/// profiles typed in by hand leave them alone.
pub(super) fn existing_profile(store: &ProfileStore, profile: &SessionProfile) -> Option<String> {
    store
        .profiles()
        .iter()
        .find(|saved| {
            saved.host.eq_ignore_ascii_case(&profile.host)
                && saved.port == profile.port
                && saved.username == profile.username
        })
        .map(|saved| saved.name.clone())
}

/// What this machine tells the import: the home directory, the login name
/// and whether an agent answers.
fn import_context() -> Option<ImportContext> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    let local_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    Some(ImportContext {
        home,
        local_user,
        agent_available: agent_available(),
    })
}

/// Whether an SSH agent looks reachable, the way the transport looks for one.
#[cfg(unix)]
fn agent_available() -> bool {
    std::env::var_os("SSH_AUTH_SOCK").is_some_and(|socket| Path::new(&socket).exists())
}

/// Whether an SSH agent looks reachable, the way the transport looks for one:
/// the Windows OpenSSH service's pipe. Pageant is not looked for, so a host
/// it would serve imports with a password instead — the profile can be
/// switched to the agent in the form.
#[cfg(windows)]
fn agent_available() -> bool {
    std::fs::metadata(r"\\.\pipe\openssh-ssh-agent").is_ok()
}

/// One line saying what could not be carried over.
fn note_text(note: &ImportNote) -> SharedString {
    match note {
        ImportNote::ProxyCommand => ts!("connection.ssh_import.note_proxy_command"),
        ImportNote::UnexpandedToken(file) => {
            ts!("connection.ssh_import.note_token", file = file.as_str())
        }
        ImportNote::MissingKey(path) => ts!(
            "connection.ssh_import.note_missing_key",
            file = path.display().to_string()
        ),
        ImportNote::BadPort(port) => {
            ts!("connection.ssh_import.note_bad_port", port = port.as_str())
        }
        ImportNote::UnsupportedForward(line) => {
            ts!("connection.ssh_import.note_forward", line = line.as_str())
        }
        ImportNote::JumpLoop => ts!("connection.ssh_import.note_jump_loop"),
    }
}

/// The second line of a host's row: where it goes, how it logs in, and what
/// else it brings along.
fn detail_text(profile: &SessionProfile) -> String {
    let mut parts = vec![profile.label()];
    parts.push(match &profile.auth {
        AuthMethod::PublicKey { key_path } => ts!(
            "connection.ssh_import.auth_key",
            file = key_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| key_path.display().to_string())
        )
        .to_string(),
        AuthMethod::Agent => ts!("connection.auth.agent").to_string(),
        AuthMethod::Password => ts!("connection.auth.password").to_string(),
    });
    if !profile.hops.is_empty() {
        let hosts = profile
            .hops
            .iter()
            .map(|hop| hop.host.as_str())
            .collect::<Vec<_>>()
            .join(" → ");
        parts.push(ts!("connection.ssh_import.via", hosts = hosts).to_string());
    }
    if !profile.tunnels.is_empty() {
        parts.push(
            ts!(
                "connection.ssh_import.forwards",
                count = profile.tunnels.len()
            )
            .to_string(),
        );
    }
    parts.join(" · ")
}

impl ConnectionDialog {
    /// Read `~/.ssh/config` and put the checklist up over the dialog.
    ///
    /// Every host the file names starts ticked but for those a saved profile
    /// already reaches, which start unticked and say so.
    pub(super) fn open_ssh_import(&mut self, cx: &mut Context<Self>) {
        let path = default_config_path().unwrap_or_else(|| PathBuf::from("~/.ssh/config"));
        self.open_ssh_import_from(path, import_context(), cx);
    }

    /// [`Self::open_ssh_import`] for an explicit file and machine.
    pub(super) fn open_ssh_import_from(
        &mut self,
        path: PathBuf,
        context: Option<ImportContext>,
        cx: &mut Context<Self>,
    ) {
        let mut import = SshImport {
            path: path.clone(),
            rows: Vec::new(),
            match_blocks: 0,
            unreadable: Vec::new(),
            error: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        };
        let shown = path.display().to_string();
        let read = context
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
            .and_then(|context| read_config(&path, &context));
        match read {
            Ok(found) => {
                import.match_blocks = found.match_blocks;
                import.unreadable = found.unreadable;
                import.rows = found
                    .hosts
                    .into_iter()
                    .map(|host| {
                        let existing = existing_profile(&self.store, &host.profile);
                        ImportRow {
                            checked: existing.is_none(),
                            existing,
                            host,
                        }
                    })
                    .collect();
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                import.error = Some(ts!("connection.ssh_import.missing", path = shown));
            }
            Err(err) => {
                import.error = Some(ts!(
                    "connection.ssh_import.unreadable_file",
                    path = shown,
                    error = err.to_string()
                ));
            }
        }
        self.close_context(cx);
        self.ssh_import = Some(import);
        cx.notify();
    }

    /// Take the checklist down, and say whether there was one.
    pub(super) fn close_ssh_import(&mut self, cx: &mut Context<Self>) -> bool {
        if self.ssh_import.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Tick or untick the host at `index`.
    pub(super) fn set_import_checked(
        &mut self,
        index: usize,
        checked: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self
            .ssh_import
            .as_mut()
            .and_then(|import| import.rows.get_mut(index))
        {
            row.checked = checked;
            cx.notify();
        }
    }

    /// Tick or untick every host.
    pub(super) fn set_all_import_checked(&mut self, checked: bool, cx: &mut Context<Self>) {
        if let Some(import) = self.ssh_import.as_mut() {
            for row in &mut import.rows {
                row.checked = checked;
            }
            cx.notify();
        }
    }

    /// How many hosts are ticked.
    pub(super) fn import_checked_count(&self) -> usize {
        self.ssh_import.as_ref().map_or(0, |import| {
            import.rows.iter().filter(|row| row.checked).count()
        })
    }

    /// Save every ticked host as a profile, take the checklist down and say
    /// how it went.
    ///
    /// The list is read from disk and written straight back, the way a
    /// duplicate is: nothing else would save these, and a file that cannot be
    /// read cannot be written over. The form is left alone — it may be
    /// holding edits of its own.
    pub(super) fn confirm_ssh_import(&mut self, cx: &mut Context<Self>) {
        let Some(import) = self.ssh_import.take() else {
            return;
        };
        let profiles: Vec<SessionProfile> = import
            .rows
            .into_iter()
            .filter(|row| row.checked)
            .map(|row| row.host.profile)
            .collect();
        if profiles.is_empty() {
            cx.notify();
            return;
        }
        let mut store = match self.read_store() {
            Ok(store) => store,
            Err(err) => {
                log::error!("the profile list could not be read, so nothing was imported: {err:#}");
                self.set_status(
                    StatusLevel::Error,
                    ts!("connection.load_failed", error = format!("{err:#}")),
                );
                cx.notify();
                return;
            }
        };
        let count = profiles.len();
        for profile in profiles {
            store.upsert(profile);
        }
        let written = self.write_store(&store);
        self.store = store;
        self.collapsed_folders.remove(IMPORT_FOLDER);
        match written {
            // Two keys rather than a plural rule, as the dialog's counts are.
            Ok(()) => self.set_status(
                StatusLevel::Info,
                if count == 1 {
                    ts!("connection.ssh_import.done_one", folder = IMPORT_FOLDER)
                } else {
                    ts!(
                        "connection.ssh_import.done_many",
                        count = count,
                        folder = IMPORT_FOLDER
                    )
                },
            ),
            Err(err) => {
                log::error!("could not write the profile list: {err:#}");
                self.set_status(
                    StatusLevel::Error,
                    ts!("connection.ssh_import.failed", error = format!("{err:#}")),
                );
            }
        }
        cx.notify();
    }

    /// The small "Import from SSH config…" action beside the list heading.
    pub(super) fn render_ssh_import_link(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let this = cx.entity();
        row_action(
            "connection-ssh-import".into(),
            ts!("connection.ssh_import.open"),
            tab::LIST - 1,
            theme.accent,
            theme.surface_hover,
            move |cx| this.update(cx, |dialog, cx| dialog.open_ssh_import(cx)),
            cx,
        )
        .into_any_element()
    }

    /// The checklist, while it is up.
    pub(super) fn render_ssh_import(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let import = self.ssh_import.as_ref()?;
        let theme = theme(cx);
        let this = cx.entity();
        let muted = |text: SharedString| {
            div()
                .text_size(TextSize::Xs.size(cx))
                .text_color(theme.text_muted)
                .child(text)
        };

        let mut body = div()
            .flex()
            .flex_col()
            .min_h_0()
            .gap(px(10.))
            .child(muted(ts!(
                "connection.ssh_import.source",
                path = import.path.display().to_string()
            )));

        let checked = self.import_checked_count();
        let close = Button::new("connection-ssh-import-cancel", ts!("common.cancel"))
            .variant(ButtonVariant::Secondary)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| {
                    this.update(cx, |dialog, cx| dialog.close_ssh_import(cx));
                }
            });

        if let Some(error) = &import.error {
            body = body
                .child(
                    div()
                        .text_size(TextSize::Body.size(cx))
                        .text_color(theme.text)
                        .child(error.clone()),
                )
                .child(
                    div().flex().flex_row().justify_end().child(
                        Button::new("connection-ssh-import-close", ts!("common.close"))
                            .variant(ButtonVariant::Primary)
                            .focus_handle(&import.focus)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.close_ssh_import(cx));
                                }
                            }),
                    ),
                );
        } else if import.rows.is_empty() {
            body = body
                .child(
                    div()
                        .text_size(TextSize::Body.size(cx))
                        .text_color(theme.text)
                        .child(ts!("connection.ssh_import.empty")),
                )
                .child(
                    div().flex().flex_row().justify_end().child(
                        Button::new("connection-ssh-import-close", ts!("common.close"))
                            .variant(ButtonVariant::Primary)
                            .focus_handle(&import.focus)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.close_ssh_import(cx));
                                }
                            }),
                    ),
                );
        } else {
            let rows = import.rows.iter().enumerate().map(|(index, row)| {
                let profile = &row.host.profile;
                let tick = Checkbox::new(
                    ElementId::from(("connection-ssh-import-row", index)),
                    SharedString::from(profile.name.clone()),
                )
                .checked(row.checked)
                .tab_index(TAB_IMPORT + index as isize)
                .on_toggle({
                    let this = this.clone();
                    move |checked, _window, cx| {
                        this.update(cx, |dialog, cx| {
                            dialog.set_import_checked(index, checked, cx);
                        });
                    }
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(8.))
                    .py(px(6.))
                    .child(tick)
                    .child(
                        div()
                            .pl(px(24.))
                            .text_size(TextSize::Xs.size(cx))
                            .text_color(theme.text_muted)
                            .child(detail_text(profile)),
                    )
                    .children(row.existing.as_ref().map(|name| {
                        div()
                            .pl(px(24.))
                            .text_size(TextSize::Xs.size(cx))
                            .text_color(theme.text_muted)
                            .child(ts!("connection.ssh_import.existing", name = name.as_str()))
                    }))
                    .children(row.host.notes.iter().map(|note| {
                        div()
                            .pl(px(24.))
                            .text_size(TextSize::Xs.size(cx))
                            .text_color(StatusLevel::Warning.color(&theme))
                            .child(note_text(note))
                    }))
            });

            let list = div()
                .relative()
                .flex()
                .flex_col()
                .min_h_0()
                .child(
                    div()
                        .id("connection-ssh-import-list")
                        .track_scroll(&import.scroll)
                        .flex()
                        .flex_col()
                        .min_h_0()
                        .max_h(rugpui::scaled(IMPORT_LIST_MAX_HEIGHT, cx))
                        .overflow_y_scroll()
                        .restrict_scroll_to_axis()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .children(rows),
                )
                .with_scrollbar("connection-ssh-import-scrollbar", &import.scroll);

            let general = (import.match_blocks > 0)
                .then(|| {
                    muted(ts!(
                        "connection.ssh_import.match_skipped",
                        count = import.match_blocks
                    ))
                })
                .into_iter()
                .chain(import.unreadable.iter().map(|path| {
                    muted(ts!(
                        "connection.ssh_import.include_unreadable",
                        path = path.display().to_string()
                    ))
                }));

            let select = |id: &'static str, label: SharedString, on: bool, offset: isize| {
                let this = this.clone();
                Button::new(id, label)
                    .variant(ButtonVariant::Ghost)
                    .compact()
                    .tab_index(TAB_IMPORT + 1000 + offset)
                    .on_click(move |_, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.set_all_import_checked(on, cx));
                    })
            };

            body = body
                .child(list)
                .children(general)
                .child(muted(ts!("connection.ssh_import.secrets")))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(select(
                            "connection-ssh-import-all",
                            ts!("connection.ssh_import.select_all"),
                            true,
                            0,
                        ))
                        .child(select(
                            "connection-ssh-import-none",
                            ts!("connection.ssh_import.select_none"),
                            false,
                            1,
                        ))
                        .child(div().flex_1())
                        .child(close.tab_index(TAB_IMPORT + 1002))
                        .child(
                            Button::new(
                                "connection-ssh-import-confirm",
                                ts!("connection.ssh_import.import", count = checked),
                            )
                            .variant(ButtonVariant::Primary)
                            .disabled(checked == 0)
                            .tab_index(TAB_IMPORT + 1003)
                            .focus_handle(&import.focus)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.confirm_ssh_import(cx));
                                }
                            }),
                        ),
                );
        }

        let dismiss = this.clone();
        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    modal(
                        "connection-ssh-import-modal",
                        ts!("connection.ssh_import.title"),
                        rugpui::scaled(IMPORT_WIDTH, cx),
                        body,
                        move |_window, cx| {
                            dismiss.update(cx, |dialog, cx| dialog.close_ssh_import(cx));
                        },
                    )
                    .initial_focus(&import.focus)
                    .escape_dismisses(true),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn ticked_hosts_are_saved_and_known_ones_start_unticked(cx: &mut gpui::TestAppContext) {
        cx.update(rugpui::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let profiles = dir.path().join("profiles.json");
        let mut store = ProfileStore::default();
        store.upsert(SessionProfile::new(
            "typed by hand",
            "db.lan",
            22,
            "me",
            AuthMethod::Password,
        ));
        store.save_to(&profiles).expect("profile list");
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).expect(".ssh");
        let config = ssh.join("config");
        std::fs::write(
            &config,
            "Host web\n  HostName web.lan\nHost db\n  HostName db.lan\nHost *\n  User me\n",
        )
        .expect("config");
        let context = ImportContext {
            home: dir.path().to_path_buf(),
            local_user: "me".to_owned(),
            agent_available: false,
        };

        let window = cx.add_window(|_, cx| {
            let mut dialog = ConnectionDialog::new(cx);
            dialog.open = true;
            dialog.use_profiles_for_test(profiles.clone());
            dialog.open_ssh_import_from(config.clone(), Some(context.clone()), cx);
            dialog
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        window
            .update(&mut cx, |dialog, _window, cx| {
                let import = dialog.ssh_import.as_ref().expect("the checklist is up");
                let ticks: Vec<(&str, bool)> = import
                    .rows
                    .iter()
                    .map(|row| (row.host.profile.name.as_str(), row.checked))
                    .collect();
                assert_eq!(ticks, [("web", true), ("db", false)]);
                assert_eq!(import.rows[1].existing.as_deref(), Some("typed by hand"));
                dialog.confirm_ssh_import(cx);
                assert!(dialog.ssh_import.is_none());
            })
            .unwrap();

        let saved = ProfileStore::load_from(&profiles).expect("reload");
        let names: Vec<&str> = saved.profiles().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["typed by hand", "web"]);
        let web = &saved.profiles()[1];
        assert_eq!(web.host, "web.lan");
        assert_eq!(web.folder.as_deref(), Some(IMPORT_FOLDER));
    }

    #[gpui::test]
    fn a_missing_file_says_so_and_escape_takes_the_checklist_down(cx: &mut gpui::TestAppContext) {
        cx.update(rugpui::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let context = ImportContext {
            home: dir.path().to_path_buf(),
            local_user: "me".to_owned(),
            agent_available: false,
        };
        let window = cx.add_window(|_, cx| {
            let mut dialog = ConnectionDialog::new(cx);
            dialog.open = true;
            dialog.open_ssh_import_from(dir.path().join("nothing"), Some(context.clone()), cx);
            dialog
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        window
            .update(&mut cx, |dialog, _window, cx| {
                let import = dialog.ssh_import.as_ref().expect("the checklist is up");
                assert!(import.error.is_some());
                dialog.escape(cx);
                assert!(
                    dialog.ssh_import.is_none(),
                    "Escape closes the checklist first"
                );
                assert!(dialog.open, "and leaves the dialog where it was");
            })
            .unwrap();
    }

    #[test]
    fn a_profile_reaching_the_same_place_is_found_whatever_the_case() {
        let mut store = ProfileStore::default();
        let saved = SessionProfile::new("prod", "Web.Example.com", 22, "deploy", AuthMethod::Agent);
        store.upsert(saved);

        let same =
            SessionProfile::new("web", "web.example.com", 22, "deploy", AuthMethod::Password);
        assert_eq!(existing_profile(&store, &same).as_deref(), Some("prod"));

        let other_user =
            SessionProfile::new("web", "web.example.com", 22, "root", AuthMethod::Agent);
        assert_eq!(existing_profile(&store, &other_user), None);
        let other_port =
            SessionProfile::new("web", "web.example.com", 2222, "deploy", AuthMethod::Agent);
        assert_eq!(existing_profile(&store, &other_port), None);
    }

    #[test]
    fn a_row_says_where_how_through_what_and_with_what() {
        let mut profile = SessionProfile::new(
            "db",
            "10.0.0.5",
            2222,
            "admin",
            AuthMethod::PublicKey {
                key_path: PathBuf::from("/home/me/.ssh/id_db"),
            },
        );
        profile.hops.push(HopRule {
            id: Uuid::new_v4(),
            host: "bastion".to_owned(),
            port: 22,
            username: "gate".to_owned(),
            auth: AuthMethod::Agent,
            save_secret: false,
        });
        profile.tunnels.push(TunnelRule::dynamic(1080));
        let detail = detail_text(&profile);
        assert!(detail.starts_with("admin@10.0.0.5:2222"), "{detail}");
        assert!(detail.contains("id_db"), "{detail}");
        assert!(detail.contains("bastion"), "{detail}");
    }
}
