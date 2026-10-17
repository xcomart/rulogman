//! Form.

use super::*;

impl ConnectionDialog {
    /// Copy `profile` into the form and remember that it is being edited.
    ///
    /// Secrets are never copied back into the form: an empty password field
    /// means "reuse whatever the keychain holds".
    pub(super) fn fill_form(&mut self, profile: &SessionProfile, cx: &mut Context<Self>) {
        self.name_input
            .update(cx, |input, cx| input.set_content(profile.name.clone(), cx));
        self.folder_input.update(cx, |input, cx| {
            input.set_content(profile.folder.clone().unwrap_or_default(), cx)
        });
        self.host_input
            .update(cx, |input, cx| input.set_content(profile.host.clone(), cx));
        self.port_input.update(cx, |input, cx| {
            input.set_content(profile.port.to_string(), cx)
        });
        self.username_input.update(cx, |input, cx| {
            input.set_content(profile.username.clone(), cx)
        });
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.passphrase_input
            .update(cx, |input, cx| input.clear(cx));

        match &profile.auth {
            AuthMethod::Password => {
                self.auth_kind = AuthKind::Password;
                self.key_path_input.update(cx, |input, cx| input.clear(cx));
            }
            AuthMethod::PublicKey { key_path } => {
                self.auth_kind = AuthKind::PrivateKey;
                let path = key_path.display().to_string();
                self.key_path_input
                    .update(cx, |input, cx| input.set_content(path, cx));
            }
            AuthMethod::Agent => {
                self.auth_kind = AuthKind::Agent;
                self.key_path_input.update(cx, |input, cx| input.clear(cx));
            }
        }

        // Restore the per-session overrides, and reveal the section when the
        // profile actually has any — otherwise they would be invisible.
        let overrides = &profile.overrides;
        self.overrides_open = !overrides.is_empty();
        self.override_scheme = overrides
            .scheme
            .as_deref()
            .filter(|scheme| !scheme.trim().is_empty())
            .map(|scheme| SharedString::from(scheme.to_owned()));
        let font_size = overrides.font_size.map(format_number).unwrap_or_default();
        let scrollback = overrides
            .scrollback_lines
            .map(|lines| lines.to_string())
            .unwrap_or_default();
        let term = overrides.term.clone().unwrap_or_default();
        self.override_charset = overrides
            .charset
            .as_deref()
            .filter(|charset| !charset.trim().is_empty())
            .map(str::to_owned);
        self.override_font_size_input
            .update(cx, |input, cx| input.set_content(font_size, cx));
        self.override_scrollback_input
            .update(cx, |input, cx| input.set_content(scrollback, cx));
        self.override_term_input
            .update(cx, |input, cx| input.set_content(term, cx));

        // Same treatment for the three list sections: a profile that jumps
        // through nothing, forwards nothing and follows nothing keeps them
        // shut, and either way the rows of whatever profile was selected
        // before are gone.
        self.hops_open = !profile.hops.is_empty();
        self.set_hop_rows(&profile.hops, cx);
        self.tunnels_open = !profile.tunnels.is_empty();
        self.set_tunnel_rows(&profile.tunnels, cx);
        self.tails_open = !profile.tails.is_empty();
        self.set_tail_rows(&profile.tails, cx);

        self.save_secret = profile.save_secret;
        self.show_files = profile.show_files;
        self.editing = Some(profile.id);
        // The single funnel through which a profile becomes the selection, so
        // the single place the pinned local row has to be deselected.
        self.clear_local_selection();
    }

    /// The per-session overrides described by the form.
    ///
    /// A blank field means "inherit", so it maps to `None` rather than to an
    /// empty string — that is what keeps `overrides` out of `profiles.json`
    /// entirely for a profile that overrides nothing.
    pub(super) fn collect_overrides(&self, cx: &App) -> SessionOverrides {
        SessionOverrides {
            scheme: self
                .override_scheme
                .as_ref()
                .map(|scheme| scheme.to_string()),
            font_size: Self::text(&self.override_font_size_input, cx)
                .parse::<f32>()
                .ok(),
            scrollback_lines: Self::text(&self.override_scrollback_input, cx)
                .parse::<usize>()
                .ok(),
            term: {
                let term = Self::text(&self.override_term_input, cx);
                (!term.is_empty()).then_some(term)
            },
            charset: self.override_charset.clone(),
        }
    }

    /// Expands or collapses the "Session overrides" section.
    pub(super) fn set_overrides_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.overrides_open = open;
        // Both dropdowns live inside the section, so collapsing it takes their
        // triggers away; a flag left standing would reopen the list the next
        // time the section is expanded, with nothing having asked for it.
        self.open_list = None;
        if self.overrides_open {
            // Index of the section within the scrolled body; see `render`.
            self.body_scroll.scroll_to_item(1);
        }
        cx.notify();
    }

    /// Build an empty jump-host row numbered for `position` in the list.
    ///
    /// The row is given its identifier here, not when it is saved: the id is
    /// what a secret typed into the row would be stored under, so it has to
    /// exist for as long as the row does.
    pub(super) fn hop_row(cx: &mut Context<Self>, position: usize) -> HopRow {
        // Clamped exactly like a tunnel row's numbering, so that a list longer
        // than the numbering allows cannot push a row past the "Add jump host"
        // button and out of the tab ring's order.
        let base = (tab::HOP_ROWS + position as isize * tab::HOP_ROW_STRIDE)
            .min(tab::HOP_ADD - tab::HOP_ROW_STRIDE);
        // Sample values, like the host and port hints of the form above: they
        // read the same in every language and are never translated.
        let host = Self::field(cx, "bastion.example.com".into(), false, base);
        let port = Self::field(cx, DEFAULT_PORT.to_string().into(), false, base + 1);
        let username = Self::field(cx, "alice".into(), false, base + 2);
        // Index `base + 3` belongs to the method picker, which is not a field.
        let key_path = Self::field(cx, "~/.ssh/id_ed25519".into(), false, base + 4);
        let secret = Self::field(cx, ts!("connection.password_placeholder"), true, base + 5);
        digits_only(cx, &port, false, MAX_PORT_DIGITS);
        HopRow {
            tab_base: base,
            id: Uuid::new_v4(),
            host,
            port,
            username,
            auth_kind: AuthKind::Password,
            key_path,
            secret,
            save_secret: false,
        }
    }

    /// Set the private key path, e.g. from the platform file picker.
    pub(super) fn set_key_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let text = path.display().to_string();
        self.key_path_input
            .update(cx, |input, cx| input.set_content(text, cx));
        cx.notify();
    }

    /// Trimmed content of `input`.
    pub(super) fn text(input: &Entity<TextInput>, cx: &App) -> String {
        input.read(cx).content().trim().to_owned()
    }

    /// The port typed into the form, or `None` when it is out of range.
    ///
    /// An empty field means [`DEFAULT_PORT`].
    pub(super) fn port(&self, cx: &App) -> Option<u16> {
        let raw = Self::text(&self.port_input, cx);
        if raw.is_empty() {
            return Some(DEFAULT_PORT);
        }
        raw.parse::<u16>().ok().filter(|port| *port != 0)
    }

    /// Whether the form holds enough information to open a session.
    pub(super) fn can_connect(&self, cx: &App) -> bool {
        // A pinned local row is always ready: there is no host to reach, no
        // credential to check and no form to complete.
        if self.is_local_selected() {
            return true;
        }
        if Self::text(&self.host_input, cx).is_empty()
            || Self::text(&self.username_input, cx).is_empty()
        {
            return false;
        }
        if self.auth_kind == AuthKind::PrivateKey && Self::text(&self.key_path_input, cx).is_empty()
        {
            return false;
        }
        if self.port(cx).is_none() {
            return false;
        }
        // A jump host or a forwarding the user started and did not finish
        // blocks the session rather than being dropped from it: see
        // `collect_hop_rules` and `collect_tunnel_rules`. A followed file's
        // path cannot be half-written, but a highlight rule of its own can —
        // and a rule that does not compile would follow the file silently
        // doing nothing, so it blocks the session too.
        self.hop_rules(cx).is_some()
            && self.tunnel_rules(cx).is_some()
            && self.tail_rules(cx).is_some()
    }

    /// `Enter` in any field: connect when the form is complete, explain why not
    /// otherwise.
    pub(super) fn submit(&mut self, cx: &mut Context<Self>) {
        if self.can_connect(cx) {
            self.connect(cx);
        } else {
            self.explain_incomplete(cx);
        }
    }

    /// Fill the message strip with the reason [`Self::can_connect`] said no.
    pub(super) fn explain_incomplete(&mut self, cx: &mut Context<Self>) {
        let reason = if Self::text(&self.host_input, cx).is_empty() {
            ts!("connection.need_host")
        } else if Self::text(&self.username_input, cx).is_empty() {
            ts!("connection.need_username")
        } else if self.auth_kind == AuthKind::PrivateKey
            && Self::text(&self.key_path_input, cx).is_empty()
        {
            ts!("connection.need_key")
        } else if self.port(cx).is_none() {
            ts!("connection.need_port")
        } else if self.hop_rules(cx).is_none() {
            ts!("connection.hops.incomplete")
        } else if self.tunnel_rules(cx).is_none() {
            ts!("connection.tunnels.incomplete")
        } else {
            ts!("connection.tails.incomplete")
        };
        self.set_status(StatusLevel::Error, reason);
        cx.notify();
    }

    /// Persist the form, resolve the credentials and emit
    /// [`ConnectionDialogEvent::Connect`].
    ///
    /// Storage problems never block the connection: they are reported in the
    /// message strip and the dialog stays open so the user can read them, while
    /// the session opens behind it. A clean run closes the dialog.
    pub(super) fn connect(&mut self, cx: &mut Context<Self>) {
        // A local session is not a profile: nothing is written to disk, no
        // keychain entry is touched, and there are no credentials to resolve.
        // Returning before any of that is what keeps the pinned row from
        // disturbing the saved profiles or the form's own state.
        #[cfg(unix)]
        if self.local_selected {
            cx.emit(ConnectionDialogEvent::ConnectLocal);
            self.close(cx);
            return;
        }
        // Cloned out of the list first: closing the dialog needs the whole of
        // `self`, and the event has to carry the choice past that.
        #[cfg(windows)]
        if let Some(shell) = self.selected_local_shell().cloned() {
            cx.emit(ConnectionDialogEvent::ConnectLocalShell(shell));
            self.close(cx);
            return;
        }

        // One connect at a time: a second click while the keychain is still
        // answering the first would store the same profile twice and open two
        // sessions for it.
        if self.keychain_task.is_some() {
            return;
        }
        if !self.can_connect(cx) {
            self.explain_incomplete(cx);
            return;
        }

        let auth_kind = self.auth_kind;
        let host = Self::text(&self.host_input, cx);
        let username = Self::text(&self.username_input, cx);
        let key_path = PathBuf::from(Self::text(&self.key_path_input, cx));
        let Some(port) = self.port(cx) else {
            self.explain_incomplete(cx);
            return;
        };
        let Some(tunnels) = self.tunnel_rules(cx) else {
            self.explain_incomplete(cx);
            return;
        };
        let Some(mut hops) = self.hop_rules(cx) else {
            self.explain_incomplete(cx);
            return;
        };
        let Some(tails) = self.tail_rules(cx) else {
            self.explain_incomplete(cx);
            return;
        };
        // Read once, before anything is written: the fields are the only place
        // a hop's new secret exists, and every decision below turns on which of
        // them hold something.
        let hop_secrets = self.hop_secrets(cx);
        // A hop the user typed a secret for is a hop with a keychain entry,
        // whether or not the stored rule already said so. There is no checkbox
        // to say it with: for the target host that choice is worth a control of
        // its own, but a jump host is only ever reached on the way somewhere
        // else, and a bastion password nobody remembered would be asked for on
        // every single connection through it.
        for hop in &mut hops {
            if hop_secrets.iter().any(|(id, _)| *id == hop.id) {
                hop.save_secret = true;
            }
        }

        let name = {
            let typed = Self::text(&self.name_input, cx);
            if typed.is_empty() {
                host.clone()
            } else {
                typed
            }
        };

        let auth_method = match auth_kind {
            AuthKind::Password => AuthMethod::Password,
            AuthKind::PrivateKey => AuthMethod::PublicKey {
                key_path: key_path.clone(),
            },
            AuthKind::Agent => AuthMethod::Agent,
        };

        // The list as it is on disk now, which the profile is written into: the
        // one on screen may be empty or stale (see `read_store`). When the file
        // cannot be read the session still opens, from the profile as the
        // dialog has it, but nothing about it is stored.
        let fresh = self.read_store();
        let stored = self.editing.and_then(|id| {
            fresh
                .as_ref()
                .ok()
                .and_then(|store| store.get(id))
                .or_else(|| self.store.get(id))
                .cloned()
        });
        let previous_save_secret = stored.as_ref().is_some_and(|profile| profile.save_secret);
        let mut profile = match stored {
            Some(mut existing) => {
                existing.name = name;
                existing.host = host;
                existing.port = port;
                existing.username = username;
                existing.auth = auth_method;
                existing
            }
            None => SessionProfile::new(name, host, port, username, auth_method),
        };
        // The hops as they were stored, so the ones the user took off the
        // profile can have their keychain entries removed below. Read before
        // the list is replaced, which is the last moment they exist.
        let previous_hops: Vec<(Uuid, String, bool)> = profile
            .hops
            .iter()
            .map(|hop| (hop.id, hop.host.clone(), hop.save_secret))
            .collect();

        let folder = Self::text(&self.folder_input, cx);
        profile.folder = (!folder.is_empty()).then_some(folder);
        profile.save_secret = self.save_secret && auth_kind != AuthKind::Agent;
        profile.show_files = self.show_files;
        profile.overrides = self.collect_overrides(cx);
        // The form is the whole truth about the forwardings: a rule the user
        // removed from an existing profile has to disappear from it too. The
        // same goes for the hops and the followed files beside them.
        profile.tunnels = tunnels;
        profile.hops = hops;
        profile.tails = tails;

        // Each entry is a whole sentence, shown on a line of its own under the
        // heading: a list of problems cannot be joined into one sentence in a
        // way that survives translation. The error details inside them come
        // from the storage layer and stay in English.
        let mut problems: Vec<SharedString> = Vec::new();

        // The secret typed into the form wins; an empty field falls back to the
        // keychain, which is how a saved profile connects without retyping.
        let typed = match auth_kind {
            AuthKind::Password => self.password_input.read(cx).content().to_owned(),
            AuthKind::PrivateKey => self.passphrase_input.read(cx).content().to_owned(),
            AuthKind::Agent => String::new(),
        };
        let mut keychain = KeychainWork {
            profile: profile.id,
            read_stored: typed.is_empty() && self.editing.is_some() && auth_kind != AuthKind::Agent,
            typed,
            entry: None,
            hop_saves: Vec::new(),
            hop_deletes: Vec::new(),
        };

        match fresh {
            Ok(mut store) => {
                store.upsert(profile.clone());
                if let Err(err) = self.write_store(&store) {
                    problems.push(ts!(
                        "connection.problem_profile_save",
                        error = format!("{err:#}")
                    ));
                }
                self.store = store;
                keychain.plan_writes(
                    &profile,
                    auth_kind,
                    previous_save_secret,
                    previous_hops,
                    hop_secrets,
                );
            }
            // The keychain is left alone too: its entries belong to the profile
            // as it is stored, and a hop this form dropped is still in there.
            Err(err) => {
                log::error!("the profile list could not be read, so it was not changed: {err:#}");
                problems.push(ts!(
                    "connection.problem_profile_unreadable",
                    error = format!("{err:#}")
                ));
            }
        }
        self.editing = Some(profile.id);

        let finish = FinishConnect {
            profile,
            auth_kind,
            key_path,
            problems,
        };
        // Most connects ask nothing of the keychain — an agent profile, a
        // profile that keeps no secret — and finish on the click itself.
        if keychain.is_empty() {
            let outcome = keychain.run();
            self.finish_connect(finish, outcome, cx);
            return;
        }
        // The rest wait for it off the UI thread: reading or writing an entry
        // is a synchronous platform call that the Secret Service and the macOS
        // keychain both put behind an unlock prompt, and the UI thread waiting
        // on that prompt would freeze every window — the prompt's own parent
        // included. The dialog stays up, as it is, until the answer is back.
        self.keychain_task = Some(cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { keychain.run() })
                .await;
            let _ = this.update(cx, |dialog, cx| {
                dialog.keychain_task = None;
                dialog.finish_connect(finish, outcome, cx);
            });
        }));
    }

    /// The last half of [`ConnectionDialog::connect`], once the keychain has
    /// answered: resolves the credentials, emits
    /// [`ConnectionDialogEvent::Connect`], and closes the dialog or says what
    /// could not be stored.
    ///
    /// A secret that could not be read comes first among the problems, ahead
    /// of the profile list's own — it is the one that may leave the session
    /// with nothing to authenticate with — and the keychain's write failures
    /// last, as they were always listed.
    fn finish_connect(
        &mut self,
        finish: FinishConnect,
        outcome: KeychainOutcome,
        cx: &mut Context<Self>,
    ) {
        let FinishConnect {
            profile,
            auth_kind,
            key_path,
            problems: saved,
        } = finish;
        let KeychainOutcome {
            secret,
            read_problem,
            write_problems,
        } = outcome;
        let problems: Vec<SharedString> = read_problem
            .iter()
            .map(KeychainProblem::message)
            .chain(saved)
            .chain(write_problems.iter().map(KeychainProblem::message))
            .collect();

        let auth = match auth_kind {
            AuthKind::Password => SshAuth::Password(secret),
            AuthKind::PrivateKey => SshAuth::PrivateKeyFile {
                path: key_path,
                passphrase: (!secret.is_empty()).then_some(secret),
            },
            AuthKind::Agent => SshAuth::Agent,
        };

        cx.emit(ConnectionDialogEvent::Connect { profile, auth });

        if problems.is_empty() {
            self.close(cx);
        } else {
            let mut lines = Vec::with_capacity(problems.len() + 1);
            lines.push(ts!("connection.connect_problems"));
            lines.extend(problems);
            self.set_status_lines(StatusLevel::Warning, lines);
            cx.notify();
        }
    }

    /// Close the dialog and report that nothing was connected.
    ///
    /// This is the single dismissal path: `Escape`, the backdrop and the Cancel
    /// button all route through here, so [`ConnectionDialogEvent::Dismissed`] is
    /// emitted exactly once however the user backs out.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(ConnectionDialogEvent::Dismissed);
        self.close(cx);
    }

    /// The collapsible "Session overrides" section.
    ///
    /// Collapsed by default. Nothing inside a collapsed section is painted, so
    /// its controls drop out of the tab ring on their own.
    pub(super) fn render_overrides(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let open = self.overrides_open;
        let defaults = crate::app_settings::current(cx).terminal;

        let overrides = self.collect_overrides(cx);
        let set = [
            overrides.scheme.is_some(),
            overrides.font_size.is_some(),
            overrides.scrollback_lines.is_some(),
            overrides.term.is_some(),
            overrides.charset.is_some(),
        ]
        .iter()
        .filter(|value| **value)
        .count();
        // Two keys rather than a plural rule: only "one" and "more than one"
        // are ever needed here.
        let summary = match set {
            0 => ts!("connection.overrides.none"),
            1 => ts!("connection.overrides.one"),
            many => ts!("connection.overrides.many", count = many),
        };

        // The id stays empty — it is what "inherit" is stored as — while the
        // row itself is labelled in the user's language.
        let mut swatches = vec![
            SchemeSwatch::new(
                INHERIT_SCHEME_ID,
                ts!("connection.overrides.scheme_default"),
            )
            .placeholder_label(ts!("common.inherits")),
        ];
        swatches.extend(crate::settings_dialog::scheme_swatches());

        let picker = SchemeSelect::new("connection-override-scheme")
            .chevron_icon(icons::CHEVRON_DOWN)
            .options(swatches)
            .selected(Some(
                self.override_scheme
                    .clone()
                    .unwrap_or_else(|| SharedString::from(INHERIT_SCHEME_ID)),
            ))
            .open(self.open_list == Some(OpenList::Scheme))
            .tab_index(tab::OVERRIDE_SCHEME)
            .scroll_handle(self.scheme_scroll.clone())
            .on_select({
                let this = this.clone();
                move |id, _window, cx| {
                    let id = id.to_owned();
                    this.update(cx, |dialog, cx| dialog.set_override_scheme(&id, cx));
                }
            })
            .on_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_list_open(OpenList::Scheme, open, cx);
                    });
                }
            });

        // Ten entries are far too many for the segmented control the rest of
        // the form picks enumerations with, so the character set gets a
        // dropdown. Its "Default" row doubles as the placeholder, which is what
        // makes that row the highlighted one while nothing is overridden.
        let charset = Select::new("connection-override-charset")
            .chevron_icon(icons::CHEVRON_DOWN)
            .options(charset_options())
            .selected(
                self.override_charset
                    .as_deref()
                    // Resolved rather than shown as stored, so that a label
                    // written by hand highlights the row of the encoding it
                    // names instead of matching none of them.
                    .map(|label| SharedString::from(Charset::from_label_or_utf8(label).name())),
            )
            .placeholder(ts!("connection.overrides.charset_default"))
            .open(self.open_list == Some(OpenList::Charset))
            .tab_index(tab::OVERRIDE_CHARSET)
            .scroll_handle(self.charset_scroll.clone())
            .on_select({
                let this = this.clone();
                // By index, not by the picked text: row 0 is the "inherit" row
                // and is the one string in the list that is translated.
                move |index, _label, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.override_charset = charset_at(index);
                        cx.notify();
                    });
                }
            })
            .on_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_list_open(OpenList::Charset, open, cx);
                    });
                }
            });

        // Each field says which global value it would inherit, so a blank field
        // is self-explanatory.
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(form_row(ts!("connection.overrides.scheme"), picker))
            .child(form_row(
                ts!("connection.overrides.font_size"),
                inherit_hint(
                    self.override_font_size_input.clone(),
                    ts!(
                        "connection.overrides.inherits_value",
                        value = format_number(defaults.font_size)
                    ),
                    cx,
                ),
            ))
            .child(form_row(
                ts!("connection.overrides.scrollback"),
                inherit_hint(
                    self.override_scrollback_input.clone(),
                    ts!(
                        "connection.overrides.inherits_lines",
                        value = defaults.scrollback_lines
                    ),
                    cx,
                ),
            ))
            .child(form_row(
                ts!("connection.overrides.term"),
                inherit_hint(
                    self.override_term_input.clone(),
                    ts!("connection.overrides.inherits_value", value = defaults.term),
                    cx,
                ),
            ))
            .child(form_row(
                ts!("connection.overrides.charset"),
                inherit_hint(
                    charset,
                    // Not a global setting like the three above it: there is no
                    // charset in `TerminalSettings`, so inheriting means UTF-8
                    // and the constant is what says so.
                    ts!(
                        "connection.overrides.inherits_value",
                        value = rulogman_core::DEFAULT_CHARSET
                    ),
                    cx,
                ),
            ));

        section(
            theme.border,
            Collapsible::new("connection-overrides", ts!("connection.overrides.title"))
                .open(open)
                .arrow_icons(icons::CHEVRON_RIGHT, icons::CHEVRON_DOWN)
                .tab_index(tab::OVERRIDES)
                // The rows inside are the dialog's own `form_row`s, and they
                // line up with the ones above the section; a body stepped in by
                // the arrow box would break that column.
                .indent(false)
                .trailing(summary_note(summary, theme.text_muted))
                .on_toggle({
                    let this = this.clone();
                    move |open, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.set_overrides_open(open, cx));
                    }
                })
                .child(body),
        )
    }
}

/// What [`ConnectionDialog::finish_connect`] needs that was decided before the
/// keychain was asked anything.
struct FinishConnect {
    /// The profile as it was stored, and as the session is opened from.
    profile: SessionProfile,
    /// How the profile authenticates, which says what the secret is for.
    auth_kind: AuthKind,
    /// The key file of a key profile.
    key_path: PathBuf,
    /// The profile list's own problems, as whole sentences.
    problems: Vec<SharedString>,
}

/// Everything one connect asks of the keychain, decided on the UI thread and
/// carried out off it by [`KeychainWork::run`].
///
/// Decided first and carried out afterwards so that the decisions — which
/// entry is read, written or removed — stay with the form that knows why,
/// while the platform calls, which can block behind an unlock prompt, are the
/// only thing that moves to the background executor.
struct KeychainWork {
    /// The profile whose own entry is meant.
    profile: Uuid,
    /// The secret typed into the form; empty when nothing was.
    typed: String,
    /// Whether an empty `typed` falls back to the stored secret: a saved
    /// profile connects without retyping.
    read_stored: bool,
    /// What becomes of the profile's own entry; `None` leaves it alone.
    entry: Option<EntryChange>,
    /// Hop secrets the user typed, as `(id, host, secret)`, to be stored.
    hop_saves: Vec<(Uuid, String, String)>,
    /// Hops that lost their secret, as `(id, host)`, whose entries go.
    hop_deletes: Vec<(Uuid, String)>,
}

/// What a connect does to a profile's own keychain entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryChange {
    /// Write the resolved secret to it.
    Save,
    /// Remove it.
    Delete,
}

/// What [`KeychainWork::run`] found and did.
#[derive(Debug, Default)]
struct KeychainOutcome {
    /// The secret to connect with: the typed one, or the stored one it fell
    /// back to; empty when there is none.
    secret: String,
    /// Why the stored secret could not be read, if it could not.
    read_problem: Option<KeychainProblem>,
    /// Every write or removal that failed, in the order they were tried.
    write_problems: Vec<KeychainProblem>,
}

/// One thing the keychain refused, kept as data until it is put into words.
///
/// The sentences are built on the UI thread, by [`KeychainProblem::message`],
/// rather than where the failure happened: the failure happens on the
/// background executor, and what a problem *says* is the dialog's business.
/// The `error`s come from the storage layer and stay in English.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeychainProblem {
    /// The stored secret could not be read.
    Read(String),
    /// The profile keeps its secret, but there was none to keep.
    NoSecret,
    /// The profile's secret could not be written.
    Save(String),
    /// A previously stored secret of the profile could not be removed.
    Delete(String),
    /// A hop's typed secret could not be written.
    HopSave { host: String, error: String },
    /// A removed hop's secret could not be removed.
    HopDelete { host: String, error: String },
}

impl KeychainProblem {
    /// The problem as the whole sentence the message strip shows.
    fn message(&self) -> SharedString {
        match self {
            Self::Read(error) => ts!("connection.problem_secret_read", error = error),
            Self::NoSecret => ts!("connection.problem_no_secret"),
            Self::Save(error) => ts!("connection.problem_secret_save", error = error),
            Self::Delete(error) => ts!("connection.problem_secret_delete", error = error),
            Self::HopSave { host, error } => ts!(
                "connection.hops.secret_save_failed",
                host = host,
                error = error
            ),
            Self::HopDelete { host, error } => ts!(
                "connection.hops.secret_delete_failed",
                host = host,
                error = error
            ),
        }
    }
}

impl KeychainWork {
    /// Decides what bringing the keychain in line with `profile` — which has
    /// just been written to the profile list — takes: its own secret, and
    /// those of its hops.
    ///
    /// `previous_hops` are the hops as they were stored, as `(id, host,
    /// had_secret)`, so the ones the user took off the profile lose their
    /// entries; `hop_secrets` are the secrets typed into the hop rows.
    fn plan_writes(
        &mut self,
        profile: &SessionProfile,
        auth_kind: AuthKind,
        previous_save_secret: bool,
        previous_hops: Vec<(Uuid, String, bool)>,
        hop_secrets: Vec<(Uuid, String)>,
    ) {
        self.entry = if profile.save_secret {
            Some(EntryChange::Save)
        } else if auth_kind != AuthKind::Agent || previous_save_secret {
            Some(EntryChange::Delete)
        } else {
            None
        };

        // Each hop's own credential, under the hop's id. Only what the user
        // actually typed is written: an empty field on a hop that already has
        // an entry leaves that entry alone, which is what lets a saved profile
        // be edited without retyping every bastion password on the way.
        self.hop_saves = hop_secrets
            .into_iter()
            .filter_map(|(id, secret)| {
                // A row that was blank apart from its secret never became a
                // rule; there is nothing to store the secret against.
                let hop = profile.hops.iter().find(|hop| hop.id == id)?;
                Some((id, hop.host.clone(), secret))
            })
            .collect();

        // A hop removed or switched to agent authentication takes its secret
        // with it, for the reason deleting a profile takes its own: nothing
        // refers to the entry any more, and orphaned secrets are hard to audit.
        self.hop_deletes = previous_hops
            .into_iter()
            .filter(|(id, _, had_secret)| {
                !profile.hops.iter().any(|hop| {
                    hop.id == *id && (!matches!(hop.auth, AuthMethod::Agent) || !had_secret)
                })
            })
            .map(|(id, host, _)| (id, host))
            .collect();
    }

    /// Whether there is nothing for the keychain to do — no read, no write —
    /// so the connect can finish without waiting on it.
    fn is_empty(&self) -> bool {
        !(self.read_stored && self.typed.is_empty())
            && self.entry.is_none()
            && self.hop_saves.is_empty()
            && self.hop_deletes.is_empty()
    }

    /// Carries the plan out. Blocks on the platform keychain, so it runs on
    /// the background executor whenever [`KeychainWork::is_empty`] says there
    /// is anything to do.
    fn run(self) -> KeychainOutcome {
        let mut outcome = KeychainOutcome::default();
        outcome.secret = if !self.typed.is_empty() || !self.read_stored {
            self.typed
        } else {
            match SecretStore::get(self.profile) {
                Ok(stored) => stored.unwrap_or_default(),
                Err(err) => {
                    outcome.read_problem = Some(KeychainProblem::Read(format!("{err:#}")));
                    String::new()
                }
            }
        };

        match self.entry {
            Some(EntryChange::Save) if outcome.secret.is_empty() => {
                outcome.write_problems.push(KeychainProblem::NoSecret);
            }
            Some(EntryChange::Save) => {
                if let Err(err) = SecretStore::set(self.profile, &outcome.secret) {
                    outcome
                        .write_problems
                        .push(KeychainProblem::Save(format!("{err:#}")));
                }
            }
            Some(EntryChange::Delete) => {
                if let Err(err) = SecretStore::delete(self.profile) {
                    outcome
                        .write_problems
                        .push(KeychainProblem::Delete(format!("{err:#}")));
                }
            }
            None => {}
        }
        for (id, host, secret) in self.hop_saves {
            if let Err(err) = SecretStore::set(id, &secret) {
                outcome.write_problems.push(KeychainProblem::HopSave {
                    host,
                    error: format!("{err:#}"),
                });
            }
        }
        for (id, host) in self.hop_deletes {
            if let Err(err) = SecretStore::delete(id) {
                outcome.write_problems.push(KeychainProblem::HopDelete {
                    host,
                    error: format!("{err:#}"),
                });
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing planned yet for a profile that keeps no secret.
    fn work(typed: &str, read_stored: bool) -> KeychainWork {
        KeychainWork {
            profile: Uuid::new_v4(),
            typed: typed.to_owned(),
            read_stored,
            entry: None,
            hop_saves: Vec::new(),
            hop_deletes: Vec::new(),
        }
    }

    /// A jump host with the given authentication.
    fn hop(host: &str, auth: AuthMethod) -> HopRule {
        HopRule {
            id: Uuid::new_v4(),
            host: host.to_owned(),
            port: 22,
            username: "alice".to_owned(),
            auth,
            save_secret: true,
        }
    }

    #[test]
    fn an_agent_profile_asks_the_keychain_nothing_and_finishes_on_the_click() {
        let profile = SessionProfile::new("agent", "example.com", 22, "alice", AuthMethod::Agent);
        let mut work = work("", false);
        work.plan_writes(&profile, AuthKind::Agent, false, Vec::new(), Vec::new());
        assert!(work.is_empty(), "{:?}", work.entry);
        // Which is what lets it run inline: there is no call in it to block on.
        let outcome = work.run();
        assert_eq!(outcome.secret, "");
        assert_eq!(outcome.read_problem, None);
        assert!(outcome.write_problems.is_empty());
    }

    #[test]
    fn a_typed_secret_is_never_replaced_by_the_stored_one() {
        let work = work("typed", true);
        assert!(work.is_empty(), "nothing is read when something was typed");
        assert_eq!(work.run().secret, "typed");
    }

    #[test]
    fn an_empty_field_on_a_saved_profile_waits_for_the_keychain() {
        assert!(!work("", true).is_empty());
    }

    #[test]
    fn a_profile_that_keeps_its_secret_writes_it_and_one_that_stops_removes_it() {
        let mut profile =
            SessionProfile::new("web", "example.com", 22, "alice", AuthMethod::Password);
        profile.save_secret = true;
        let mut keep = work("hunter2", false);
        keep.plan_writes(&profile, AuthKind::Password, false, Vec::new(), Vec::new());
        assert_eq!(keep.entry, Some(EntryChange::Save));
        assert!(!keep.is_empty());

        profile.save_secret = false;
        let mut stop = work("hunter2", false);
        stop.plan_writes(&profile, AuthKind::Password, true, Vec::new(), Vec::new());
        assert_eq!(stop.entry, Some(EntryChange::Delete));

        // An agent profile that never kept one has nothing to remove.
        profile.auth = AuthMethod::Agent;
        let mut agent = work("", false);
        agent.plan_writes(&profile, AuthKind::Agent, false, Vec::new(), Vec::new());
        assert_eq!(agent.entry, None);
    }

    #[test]
    fn hop_secrets_are_stored_for_kept_hops_and_removed_with_dropped_ones() {
        let kept = hop("kept.example", AuthMethod::Password);
        let to_agent = hop("agent.example", AuthMethod::Agent);
        let mut profile = SessionProfile::new("web", "example.com", 22, "alice", AuthMethod::Agent);
        profile.hops = vec![kept.clone(), to_agent.clone()];
        let dropped = Uuid::new_v4();
        let blank_row = Uuid::new_v4();

        let mut work = work("", false);
        work.plan_writes(
            &profile,
            AuthKind::Agent,
            false,
            vec![
                (kept.id, kept.host.clone(), true),
                (to_agent.id, to_agent.host.clone(), true),
                (dropped, "gone.example".to_owned(), true),
            ],
            vec![
                (kept.id, "pw".to_owned()),
                // A row with nothing but a secret never became a rule.
                (blank_row, "orphan".to_owned()),
            ],
        );

        assert_eq!(
            work.hop_saves,
            vec![(kept.id, "kept.example".to_owned(), "pw".to_owned())]
        );
        assert_eq!(
            work.hop_deletes,
            vec![
                (to_agent.id, "agent.example".to_owned()),
                (dropped, "gone.example".to_owned()),
            ]
        );
    }

    #[test]
    fn every_keychain_problem_reads_as_a_translated_sentence() {
        for problem in [
            KeychainProblem::Read("locked".to_owned()),
            KeychainProblem::NoSecret,
            KeychainProblem::Save("locked".to_owned()),
            KeychainProblem::Delete("locked".to_owned()),
            KeychainProblem::HopSave {
                host: "bastion".to_owned(),
                error: "locked".to_owned(),
            },
            KeychainProblem::HopDelete {
                host: "bastion".to_owned(),
                error: "locked".to_owned(),
            },
        ] {
            let message = problem.message();
            assert!(!message.contains("connection."), "untranslated {message}");
            assert!(!message.contains("%{"), "uninterpolated {message}");
        }
    }
}
