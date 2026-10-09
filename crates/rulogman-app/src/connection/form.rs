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
        if self.auth_kind == AuthKind::Agent {
            return false;
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
        let reason = if self.auth_kind == AuthKind::Agent {
            ts!("connection.agent_unsupported")
        } else if Self::text(&self.host_input, cx).is_empty() {
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
            // `can_connect` already rejected the agent method.
            AuthKind::Agent => return,
        };

        let mut profile = match self.editing.and_then(|id| self.store.get(id).cloned()) {
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
        let previous_hops: Vec<(Uuid, String)> = profile
            .hops
            .iter()
            .map(|hop| (hop.id, hop.host.clone()))
            .collect();

        profile.save_secret = self.save_secret;
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
        let secret = if !typed.is_empty() {
            typed
        } else if self.editing.is_some() {
            match SecretStore::get(profile.id) {
                Ok(stored) => stored.unwrap_or_default(),
                Err(err) => {
                    problems.push(ts!(
                        "connection.problem_secret_read",
                        error = format!("{err:#}")
                    ));
                    String::new()
                }
            }
        } else {
            String::new()
        };

        self.store.upsert(profile.clone());
        if let Err(err) = self.store.save() {
            problems.push(ts!(
                "connection.problem_profile_save",
                error = format!("{err:#}")
            ));
        }

        if profile.save_secret {
            if secret.is_empty() {
                problems.push(ts!("connection.problem_no_secret"));
            } else if let Err(err) = SecretStore::set(profile.id, &secret) {
                problems.push(ts!(
                    "connection.problem_secret_save",
                    error = format!("{err:#}")
                ));
            }
        } else if let Err(err) = SecretStore::delete(profile.id) {
            problems.push(ts!(
                "connection.problem_secret_delete",
                error = format!("{err:#}")
            ));
        }

        // Each hop's own credential, under the hop's id. Only what the user
        // actually typed is written: an empty field on a hop that already has
        // an entry leaves that entry alone, which is what lets a saved profile
        // be edited without retyping every bastion password on the way.
        for (id, secret) in &hop_secrets {
            let Some(hop) = profile.hops.iter().find(|hop| hop.id == *id) else {
                // The row was blank apart from its secret, so it never became a
                // rule; there is nothing to store the secret against.
                continue;
            };
            if let Err(err) = SecretStore::set(*id, secret) {
                problems.push(ts!(
                    "connection.hops.secret_save_failed",
                    host = hop.host.clone(),
                    error = format!("{err:#}")
                ));
            }
        }

        // A hop the user removed takes its secret with it, for the reason
        // deleting a profile takes its own: nothing refers to the entry any
        // more, and a keychain full of orphans is one nobody can audit.
        for (id, host) in previous_hops {
            if profile.hops.iter().any(|hop| hop.id == id) {
                continue;
            }
            if let Err(err) = SecretStore::delete(id) {
                problems.push(ts!(
                    "connection.hops.secret_delete_failed",
                    host = host,
                    error = format!("{err:#}")
                ));
            }
        }

        let auth = match auth_kind {
            AuthKind::Password => SshAuth::Password(secret),
            AuthKind::PrivateKey => SshAuth::PrivateKeyFile {
                path: key_path,
                passphrase: (!secret.is_empty()).then_some(secret),
            },
            AuthKind::Agent => return,
        };

        self.editing = Some(profile.id);
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
