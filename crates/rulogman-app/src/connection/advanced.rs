//! Advanced.

use super::*;

impl ConnectionDialog {
    /// Replace the jump-host rows with one per hop of a profile.
    ///
    /// Each row takes the stored hop's id, so that editing the rest of the row
    /// keeps addressing the keychain entry the hop already has. The secret
    /// itself is never copied back into the form, for the reason
    /// [`Self::fill_form`] never copies the profile's own: an empty field means
    /// "keep whatever is stored".
    pub(super) fn set_hop_rows(&mut self, hops: &[HopRule], cx: &mut Context<Self>) {
        let mut rows = Vec::with_capacity(hops.len());
        for (position, hop) in hops.iter().enumerate() {
            let mut row = Self::hop_row(cx, position);
            row.id = hop.id;
            row.save_secret = hop.save_secret;
            row.host
                .update(cx, |input, cx| input.set_content(hop.host.clone(), cx));
            row.port
                .update(cx, |input, cx| input.set_content(hop.port.to_string(), cx));
            row.username
                .update(cx, |input, cx| input.set_content(hop.username.clone(), cx));
            match &hop.auth {
                AuthMethod::PublicKey { key_path } => {
                    row.auth_kind = AuthKind::PrivateKey;
                    let path = key_path.display().to_string();
                    row.key_path
                        .update(cx, |input, cx| input.set_content(path, cx));
                }
                AuthMethod::Password => {
                    row.auth_kind = AuthKind::Password;
                }
                AuthMethod::Agent => row.auth_kind = AuthKind::Agent,
            }
            Self::set_hop_secret_placeholder(&row, cx);
            rows.push(row);
        }
        self.hop_rows = rows;
    }

    /// Hint the secret field with the word for what the row is now asking for.
    ///
    /// A password and a passphrase are not the same thing to the person typing
    /// one, and the field is masked, so the placeholder is the only thing on
    /// screen that says which is wanted.
    pub(super) fn set_hop_secret_placeholder(row: &HopRow, cx: &mut Context<Self>) {
        let placeholder = match row.auth_kind {
            AuthKind::PrivateKey => ts!("connection.passphrase_placeholder"),
            _ => ts!("connection.password_placeholder"),
        };
        row.secret
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
    }

    /// Switch one hop's authentication method.
    ///
    /// The secret typed for the previous method is discarded, for the reason
    /// [`Self::set_auth_kind`] discards the form's: a passphrase must not be
    /// offered to a host as a password.
    pub(super) fn set_hop_auth_kind(
        &mut self,
        index: usize,
        kind: AuthKind,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.hop_rows.get(index) else {
            return;
        };
        if row.auth_kind == kind {
            return;
        }
        row.secret.update(cx, |input, cx| input.clear(cx));
        self.hop_rows[index].auth_kind = kind;
        Self::set_hop_secret_placeholder(&self.hop_rows[index], cx);
        cx.notify();
    }

    /// Append an empty jump-host row.
    pub(super) fn add_hop_row(&mut self, cx: &mut Context<Self>) {
        let row = Self::hop_row(cx, self.hop_rows.len());
        self.hop_rows.push(row);
        cx.notify();
    }

    /// Drop the jump-host row at `index`.
    ///
    /// The keychain is not touched here. The row is only gone from the *form*
    /// until the profile is saved, and a dialog the user then cancels has to
    /// leave the stored hop — secret and all — exactly as it was;
    /// [`Self::connect`] is where a hop that actually left the profile has its
    /// entry removed.
    pub(super) fn remove_hop_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.hop_rows.len() {
            return;
        }
        self.hop_rows.remove(index);
        cx.notify();
    }

    /// Expands or collapses the "Jump hosts" section.
    pub(super) fn set_hops_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.hops_open = open;
        if self.hops_open {
            // Opening an empty section on nothing but a button says less than
            // opening it on the row the user came to fill in.
            if self.hop_rows.is_empty() {
                let row = Self::hop_row(cx, 0);
                self.hop_rows.push(row);
            }
            // Index of the section within the scrolled body; see `render`.
            self.body_scroll.scroll_to_item(2);
        }
        cx.notify();
    }

    /// The text of every jump-host row, in order.
    pub(super) fn hop_fields(&self, cx: &App) -> Vec<HopFields> {
        self.hop_rows
            .iter()
            .map(|row| HopFields {
                id: row.id,
                host: Self::text(&row.host, cx),
                port: Self::text(&row.port, cx),
                username: Self::text(&row.username, cx),
                auth: row.auth_kind,
                key_path: Self::text(&row.key_path, cx),
                save_secret: row.save_secret,
            })
            .collect()
    }

    /// The jump hosts described by the form, or `None` while a row is
    /// half-written.
    pub(super) fn hop_rules(&self, cx: &App) -> Option<Vec<HopRule>> {
        collect_hop_rules(&self.hop_fields(cx))
    }

    /// What the user has typed into each hop's secret field, by hop id.
    ///
    /// Only the fields that hold something: an empty one means "keep whatever
    /// the keychain has", which is a decision about what *not* to write and so
    /// has nothing to carry. Never logged, and never put in a status line.
    pub(super) fn hop_secrets(&self, cx: &App) -> Vec<(Uuid, String)> {
        self.hop_rows
            .iter()
            .filter(|row| row.auth_kind != AuthKind::Agent)
            .filter_map(|row| {
                let secret = row.secret.read(cx).content().to_owned();
                (!secret.is_empty()).then_some((row.id, secret))
            })
            .collect()
    }

    /// Build an empty tunnel row numbered for `position` in the list.
    pub(super) fn tunnel_row(cx: &mut Context<Self>, position: usize) -> TunnelRow {
        // Clamped so that a list longer than the numbering allows for cannot
        // push a row past the "Add tunnel" button and out of the tab ring's
        // order; rows that far down share an index and tab in paint order.
        let base = (tab::TUNNEL_ROWS + position as isize * tab::TUNNEL_ROW_STRIDE)
            .min(tab::TUNNEL_ADD - tab::TUNNEL_ROW_STRIDE);
        // Sample values, like the host and port hints of the form above: they
        // read the same in every language and are never translated.
        let local_port = Self::field(cx, "8080".into(), false, base);
        let remote_host = Self::field(cx, "db.internal".into(), false, base + 1);
        let remote_port = Self::field(cx, "5432".into(), false, base + 2);
        digits_only(cx, &local_port, false, MAX_PORT_DIGITS);
        digits_only(cx, &remote_port, false, MAX_PORT_DIGITS);
        TunnelRow {
            local_port,
            remote_host,
            remote_port,
            bind_address: DEFAULT_BIND_ADDRESS.to_owned(),
        }
    }

    /// Replace the tunnel rows with one per rule of a profile.
    ///
    /// The rows are rebuilt from scratch on every profile, which is what stops
    /// the forwardings of the previously selected one from following the user
    /// to the next.
    pub(super) fn set_tunnel_rows(&mut self, rules: &[TunnelRule], cx: &mut Context<Self>) {
        let mut rows = Vec::with_capacity(rules.len());
        for (position, rule) in rules.iter().enumerate() {
            let mut row = Self::tunnel_row(cx, position);
            // The one field the form does not show, carried through the edit
            // so that saving a rule cannot quietly move its listener.
            row.bind_address = rule.bind_address.clone();
            row.local_port.update(cx, |input, cx| {
                input.set_content(rule.local_port.to_string(), cx)
            });
            row.remote_host.update(cx, |input, cx| {
                input.set_content(rule.remote_host.clone(), cx)
            });
            row.remote_port.update(cx, |input, cx| {
                input.set_content(rule.remote_port.to_string(), cx)
            });
            rows.push(row);
        }
        self.tunnel_rows = rows;
    }

    /// Append an empty tunnel row.
    pub(super) fn add_tunnel_row(&mut self, cx: &mut Context<Self>) {
        let row = Self::tunnel_row(cx, self.tunnel_rows.len());
        self.tunnel_rows.push(row);
        cx.notify();
    }

    /// Drop the tunnel row at `index`.
    pub(super) fn remove_tunnel_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tunnel_rows.len() {
            return;
        }
        self.tunnel_rows.remove(index);
        cx.notify();
    }

    /// Expands or collapses the "SSH tunnels" section.
    pub(super) fn set_tunnels_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.tunnels_open = open;
        if self.tunnels_open {
            // Opening an empty section on nothing but a button says less than
            // opening it on the row the user came to fill in.
            if self.tunnel_rows.is_empty() {
                let row = Self::tunnel_row(cx, 0);
                self.tunnel_rows.push(row);
            }
            // Index of the section within the scrolled body; see `render`.
            self.body_scroll.scroll_to_item(3);
        }
        cx.notify();
    }

    /// The text of every tunnel row, in order.
    pub(super) fn tunnel_fields(&self, cx: &App) -> Vec<TunnelFields> {
        self.tunnel_rows
            .iter()
            .map(|row| TunnelFields {
                local_port: Self::text(&row.local_port, cx),
                remote_host: Self::text(&row.remote_host, cx),
                remote_port: Self::text(&row.remote_port, cx),
                bind_address: row.bind_address.clone(),
            })
            .collect()
    }

    /// The forwardings described by the form, or `None` while a row is
    /// half-written.
    pub(super) fn tunnel_rules(&self, cx: &App) -> Option<Vec<TunnelRule>> {
        collect_tunnel_rules(&self.tunnel_fields(cx))
    }

    /// Build an empty followed-file row numbered for `position` in the list.
    pub(super) fn tail_row(cx: &mut Context<Self>, position: usize) -> TailRow {
        // Clamped like the two sections above, and for the same reason.
        let base = (tab::TAIL_ROWS + position as isize * tab::TAIL_ROW_STRIDE)
            .min(tab::TAIL_ADD - tab::TAIL_ROW_STRIDE);
        // A sample path, which reads the same in every language.
        let path = Self::field(cx, "/var/log/nginx/access.log".into(), false, base);
        // Built with the row rather than when the tick is set: the list owns
        // text fields, and a list created mid-edit would have nowhere to put
        // what the row already knows.
        let highlights = cx.new(|cx| HighlightRuleList::new(cx, base + tab::TAIL_HIGHLIGHTS));
        TailRow {
            path,
            custom_highlights: false,
            highlights,
            seeded: false,
            tab_base: base,
        }
    }

    /// Replace the followed-file rows with one per rule of a profile.
    ///
    /// A stored rule that carries its own highlights comes back with the tick
    /// set and the list filled in — and marked as seeded, so that unticking and
    /// re-ticking it restores what was stored rather than the global rules.
    pub(super) fn set_tail_rows(&mut self, rules: &[TailRule], cx: &mut Context<Self>) {
        let mut rows = Vec::with_capacity(rules.len());
        for (position, rule) in rules.iter().enumerate() {
            let mut row = Self::tail_row(cx, position);
            row.path
                .update(cx, |input, cx| input.set_content(rule.path.clone(), cx));
            if let Some(highlights) = &rule.highlights {
                row.custom_highlights = true;
                row.seeded = true;
                row.highlights
                    .update(cx, |list, cx| list.set_rules(highlights, cx));
            }
            rows.push(row);
        }
        self.tail_rows = rows;
    }

    /// Set or clear the "Custom highlighting" tick on the row at `index`.
    ///
    /// Setting it on a row that has never carried rules copies in the rules
    /// that apply to the file now — the global list, or the built-in preset
    /// when there is none — so that the user starts from what they were already
    /// looking at. Clearing it keeps the rows: the tick is the whole of the
    /// override, and a user who unticks to compare against the global colours
    /// must not lose the list to do it.
    pub(super) fn set_tail_custom_highlights(
        &mut self,
        index: usize,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.tail_rows.get(index) else {
            return;
        };
        if row.custom_highlights == on {
            return;
        }
        let seed = on && !row.seeded;
        if seed {
            let settings = crate::app_settings::current(cx);
            let rules = effective_highlights(&settings.highlights, None).into_owned();
            row.highlights
                .clone()
                .update(cx, |list, cx| list.set_rules(&rules, cx));
        }
        let row = &mut self.tail_rows[index];
        row.custom_highlights = on;
        row.seeded |= seed;
        cx.notify();
    }

    /// Append an empty followed-file row.
    pub(super) fn add_tail_row(&mut self, cx: &mut Context<Self>) {
        let row = Self::tail_row(cx, self.tail_rows.len());
        self.tail_rows.push(row);
        cx.notify();
    }

    /// Drop the followed-file row at `index`.
    pub(super) fn remove_tail_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tail_rows.len() {
            return;
        }
        self.tail_rows.remove(index);
        cx.notify();
    }

    /// Expands or collapses the "Tail files" section.
    pub(super) fn set_tails_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.tails_open = open;
        if self.tails_open {
            if self.tail_rows.is_empty() {
                let row = Self::tail_row(cx, 0);
                self.tail_rows.push(row);
            }
            // Index of the section within the scrolled body; see `render`.
            self.body_scroll.scroll_to_item(4);
        }
        cx.notify();
    }

    /// The content of every followed-file row, in order.
    pub(super) fn tail_fields(&self, cx: &App) -> Vec<TailFields> {
        self.tail_rows
            .iter()
            .map(|row| TailFields {
                path: Self::text(&row.path, cx),
                // Only read while the tick is set: an unticked row's list is
                // kept so the tick can be put back, but what it holds is not
                // an answer the profile is entitled to.
                highlights: row
                    .custom_highlights
                    .then(|| row.highlights.read(cx).fields(cx)),
            })
            .collect()
    }

    /// The files the form says to follow, or `None` while a rule is unusable.
    pub(super) fn tail_rules(&self, cx: &App) -> Option<Vec<TailRule>> {
        collect_tail_rules(&self.tail_fields(cx))
    }

    /// Pick the per-session scheme, or clear it back to "inherit".
    pub(super) fn set_override_scheme(&mut self, id: &str, cx: &mut Context<Self>) {
        self.override_scheme = (id != INHERIT_SCHEME_ID).then(|| SharedString::from(id.to_owned()));
        cx.notify();
    }

    /// The collapsible "Jump hosts" section.
    ///
    /// Two lines per hop rather than one row of six fields: a hop is a whole
    /// login — where, as whom, and with what — and squeezing that onto one line
    /// would leave every field too narrow to read the value in. The first line
    /// is the table the column headings name; the second is how that host
    /// authenticates, hinted by its placeholders instead of labelled, since a
    /// second row of headings would say what the controls already say.
    ///
    /// Above the tunnels on purpose: a hop is part of how the connection is
    /// made, while a forwarding is something the finished connection carries.
    pub(super) fn render_hops(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let open = self.hops_open;

        // Counts what the user has begun, not what could be connected through:
        // a row still being filled in is exactly the one worth mentioning while
        // the section is collapsed over it.
        let started = self
            .hop_fields(cx)
            .iter()
            .filter(|fields| !fields.is_blank())
            .count();
        // Two keys rather than a plural rule, as in the sections around it.
        let summary = match started {
            0 => ts!("connection.hops.none"),
            1 => ts!("connection.hops.one"),
            many => ts!("connection.hops.many", count = many),
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(div().flex_1().min_w_0().child(ts!("connection.hops.host")))
            .child(
                div()
                    .flex_none()
                    .w(px(HOP_PORT_WIDTH))
                    .child(ts!("connection.hops.port")),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(HOP_USERNAME_WIDTH))
                    .child(ts!("connection.hops.username")),
            )
            // Holds the column of the per-row remove action open, so the
            // headings stay over the fields they name.
            .child(div().flex_none().w(px(HOP_ACTION_WIDTH)));

        let rows = self
            .hop_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let auth_kind = row.auth_kind;
                let picker = Segmented::new(("connection-hop-auth", index))
                    .options(auth_options())
                    .selected(auth_kind.index())
                    .tab_index(
                        (tab::HOP_ROWS + index as isize * tab::HOP_ROW_STRIDE + 3)
                            .min(tab::HOP_ADD - 1),
                    )
                    .on_select({
                        let this = this.clone();
                        move |picked, _window, cx| {
                            this.update(cx, |dialog, cx| {
                                dialog.set_hop_auth_kind(index, AuthKind::from_index(picked), cx);
                            });
                        }
                    });

                let first = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().min_w_0().child(row.host.clone()))
                    .child(
                        div()
                            .flex_none()
                            .w(px(HOP_PORT_WIDTH))
                            .child(row.port.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(HOP_USERNAME_WIDTH))
                            .child(row.username.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .w(px(HOP_ACTION_WIDTH))
                            .justify_end()
                            .child(row_action(
                                ElementId::from(("connection-hop-remove", index)),
                                ts!("connection.hops.remove"),
                                theme.danger,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.remove_hop_row(index, cx);
                                        });
                                    }
                                },
                            )),
                    );

                let second = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_none().w(px(HOP_AUTH_WIDTH)).child(picker))
                    // Agent authentication has no key file or secret to enter.
                    .when(auth_kind == AuthKind::PrivateKey, |line| {
                        line.child(div().flex_1().min_w_0().child(row.key_path.clone()))
                    })
                    .when(auth_kind != AuthKind::Agent, |line| {
                        line.child(div().flex_1().min_w_0().child(row.secret.clone()))
                    })
                    .when(auth_kind == AuthKind::Agent, |line| {
                        line.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .text_color(theme.text_muted)
                                .child(ts!("connection.agent_hint")),
                        )
                    })
                    // Keeps the second line clear of the remove action's
                    // column, so the two lines of a hop end on the same edge.
                    .child(div().flex_none().w(px(HOP_ACTION_WIDTH)));

                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(first)
                    .child(second)
            })
            .collect::<Vec<_>>();

        let add = Button::new("connection-hop-add", ts!("connection.hops.add"))
            .variant(ButtonVariant::Secondary)
            .tab_index(tab::HOP_ADD)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| {
                    this.update(cx, |dialog, cx| dialog.add_hop_row(cx));
                }
            });

        let body = div()
            .flex()
            .flex_col()
            // Wider than the tunnel table's gap: each entry here is two lines
            // of its own, so the space between hops has to read as larger than
            // the space inside one.
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(ts!("connection.hops.hint")),
            )
            .when(!rows.is_empty(), |this| this.child(header))
            .children(rows)
            .child(div().flex().flex_row().pt(px(2.)).child(add));

        section(
            theme.border,
            Collapsible::new("connection-hops", ts!("connection.hops.title"))
                .open(open)
                .arrow_icons(icons::CHEVRON_RIGHT, icons::CHEVRON_DOWN)
                .tab_index(tab::HOPS)
                // A table, which draws its own columns from the left edge of
                // the section.
                .indent(false)
                .trailing(summary_note(summary, theme.text_muted))
                .on_toggle({
                    let this = this.clone();
                    move |open, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.set_hops_open(open, cx));
                    }
                })
                .child(body),
        )
    }

    /// The collapsible "SSH tunnels" section.
    ///
    /// Laid out as a table rather than as a stack of [`form_row`]s: a rule is
    /// three values read together, and one label per input would put nine of
    /// them on screen for three rules.
    pub(super) fn render_tunnels(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let open = self.tunnels_open;

        // Counts what the user has begun, not what would be forwarded: a row
        // that is still being filled in is exactly the one worth mentioning
        // while the section is collapsed over it.
        let started = self
            .tunnel_fields(cx)
            .iter()
            .filter(|fields| !fields.is_blank())
            .count();
        // Two keys rather than a plural rule, as in the overrides section.
        let summary = match started {
            0 => ts!("connection.tunnels.none"),
            1 => ts!("connection.tunnels.one"),
            many => ts!("connection.tunnels.many", count = many),
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex_none()
                    .w(px(TUNNEL_PORT_WIDTH))
                    .child(ts!("connection.tunnels.local_port")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ts!("connection.tunnels.remote_host")),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(TUNNEL_PORT_WIDTH))
                    .child(ts!("connection.tunnels.remote_port")),
            )
            // Holds the column of the per-row remove action open, so the
            // headings stay over the fields they name.
            .child(div().flex_none().w(px(TUNNEL_ACTION_WIDTH)));

        let rows = self
            .tunnel_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(TUNNEL_PORT_WIDTH))
                            .child(row.local_port.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(row.remote_host.clone()))
                    .child(
                        div()
                            .flex_none()
                            .w(px(TUNNEL_PORT_WIDTH))
                            .child(row.remote_port.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .w(px(TUNNEL_ACTION_WIDTH))
                            .justify_end()
                            .child(row_action(
                                ElementId::from(("connection-tunnel-remove", index)),
                                ts!("connection.tunnels.remove"),
                                theme.danger,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.remove_tunnel_row(index, cx);
                                        });
                                    }
                                },
                            )),
                    )
            })
            .collect::<Vec<_>>();

        let add = Button::new("connection-tunnel-add", ts!("connection.tunnels.add"))
            .variant(ButtonVariant::Secondary)
            .tab_index(tab::TUNNEL_ADD)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| {
                    this.update(cx, |dialog, cx| dialog.add_tunnel_row(cx));
                }
            });

        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(ts!("connection.tunnels.hint")),
            )
            .when(!rows.is_empty(), |this| this.child(header))
            .children(rows)
            .child(div().flex().flex_row().pt(px(2.)).child(add));

        section(
            theme.border,
            Collapsible::new("connection-tunnels", ts!("connection.tunnels.title"))
                .open(open)
                .arrow_icons(icons::CHEVRON_RIGHT, icons::CHEVRON_DOWN)
                .tab_index(tab::TUNNELS)
                // A table, which draws its own columns from the left edge of
                // the section.
                .indent(false)
                .trailing(summary_note(summary, theme.text_muted))
                .on_toggle({
                    let this = this.clone();
                    move |open, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.set_tunnels_open(open, cx));
                    }
                })
                .child(body),
        )
    }

    /// The collapsible "Tail files" section.
    ///
    /// One column, so no headings: the hint above the rows names what a row is,
    /// and a single heading over a single column would only repeat it.
    pub(super) fn render_tails(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let open = self.tails_open;

        // Counts the paths that are actually there. Unlike a tunnel or a hop, a
        // row here cannot be half-written, so what has been started and what
        // would be followed are the same number.
        let named = self
            .tail_fields(cx)
            .iter()
            .filter(|fields| !fields.path.is_empty())
            .count();
        // Two keys rather than a plural rule, as in the sections above it.
        let summary = match named {
            0 => ts!("connection.tails.none"),
            1 => ts!("connection.tails.one"),
            many => ts!("connection.tails.many", count = many),
        };

        let rows = self
            .tail_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let first = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().min_w_0().child(row.path.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .w(px(TAIL_ACTION_WIDTH))
                            .justify_end()
                            .child(row_action(
                                ElementId::from(("connection-tail-remove", index)),
                                ts!("connection.tails.remove"),
                                theme.danger,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.remove_tail_row(index, cx);
                                        });
                                    }
                                },
                            )),
                    );

                // Under the path rather than beside it, so the tick reads as a
                // fact about the file above it and the rules it reveals hang
                // off the same left edge as everything else in the section.
                let custom = Checkbox::new(
                    ElementId::from(("connection-tail-custom", index)),
                    ts!("connection.tails.custom_highlights"),
                )
                .checked(row.custom_highlights)
                .tab_index(row.tab_base + tab::TAIL_CUSTOM)
                .on_toggle({
                    let this = this.clone();
                    move |checked, _window, cx| {
                        this.update(cx, |dialog, cx| {
                            dialog.set_tail_custom_highlights(index, checked, cx);
                        });
                    }
                });

                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(first)
                    .child(custom)
                    .when(row.custom_highlights, |this| {
                        this.child(row.highlights.clone())
                    })
            })
            .collect::<Vec<_>>();

        let add = Button::new("connection-tail-add", ts!("connection.tails.add"))
            .variant(ButtonVariant::Secondary)
            .tab_index(tab::TAIL_ADD)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| {
                    this.update(cx, |dialog, cx| dialog.add_tail_row(cx));
                }
            });

        let body = div()
            .flex()
            .flex_col()
            // Wider than it was while a row was one field: each entry is at
            // least two lines of its own now, so the space between files has to
            // read as larger than the space inside one — exactly the reason the
            // jump-host table above uses the same gap.
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(ts!("connection.tails.hint")),
            )
            .children(rows)
            .child(div().flex().flex_row().pt(px(2.)).child(add));

        section(
            theme.border,
            Collapsible::new("connection-tails", ts!("connection.tails.title"))
                .open(open)
                .arrow_icons(icons::CHEVRON_RIGHT, icons::CHEVRON_DOWN)
                .tab_index(tab::TAILS)
                // A list of full-width fields, which start at the left edge of
                // the section like the tables above them.
                .indent(false)
                .trailing(summary_note(summary, theme.text_muted))
                .on_toggle({
                    let this = this.clone();
                    move |open, _window, cx| {
                        this.update(cx, |dialog, cx| dialog.set_tails_open(open, cx));
                    }
                })
                .child(body),
        )
    }
}
