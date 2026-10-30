//! Profiles.

use super::*;

/// Shared ordering for the dialog and start screen's saved connections.
pub(crate) enum ProfileListItem<'a> {
    Folder(Option<&'a str>),
    Profile(&'a SessionProfile),
}

/// Keep folders in first-seen order and preserve stored order within each one.
/// A wholly unfiled list keeps its original appearance, without a heading.
pub(crate) fn profile_list_items(profiles: &[SessionProfile]) -> Vec<ProfileListItem<'_>> {
    let mut groups: Vec<(Option<&str>, Vec<&SessionProfile>)> = Vec::new();
    for profile in profiles {
        let folder = profile
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if let Some((_, members)) = groups.iter_mut().find(|(name, _)| *name == folder) {
            members.push(profile);
        } else {
            groups.push((folder, vec![profile]));
        }
    }
    let show_headings = groups.iter().any(|(folder, _)| folder.is_some());
    let mut items = Vec::with_capacity(profiles.len() + groups.len());
    for (folder, members) in groups {
        if show_headings {
            items.push(ProfileListItem::Folder(folder));
        }
        items.extend(members.into_iter().map(ProfileListItem::Profile));
    }
    items
}

/// The names of the dashboards that follow a file over the profile `id`, in
/// saved order and each named once.
///
/// What the delete question adds to its sentence: a dashboard keeps a deleted
/// connection's rows on purpose and skips them on open, so forgetting the
/// profile quietly empties panes the user may be counting on. Free so the rule
/// is checked without a store on disk.
pub(crate) fn dashboards_using(dashboards: &[Dashboard], id: Uuid) -> Vec<String> {
    dashboards
        .iter()
        .filter(|dashboard| dashboard.panes.iter().any(|pane| pane.profile == id))
        .map(|dashboard| dashboard.name.clone())
        .collect()
}

/// The lines of the question a delete puts up before it forgets the profile
/// `name`: the same sentence the sibling applications ask, and a second one
/// naming `dashboards` when any of them follow a file over it.
pub(crate) fn delete_question(name: &str, dashboards: &[String]) -> Vec<SharedString> {
    let mut lines = vec![ts!("connection.delete_confirm", name = name.to_owned())];
    if !dashboards.is_empty() {
        lines.push(ts!(
            "connection.delete_confirm_dashboards",
            names = dashboards.join(", ")
        ));
    }
    lines
}

pub(crate) fn folder_heading(folder: Option<&str>, cx: &App) -> gpui::Div {
    div()
        .px(px(8.))
        .pt(px(6.))
        .pb(px(2.))
        .text_size(px(11.))
        .text_color(theme(cx).text_muted)
        .truncate()
        .child(
            folder
                .map(|name| SharedString::from(name.to_owned()))
                .unwrap_or_else(|| ts!("connection.no_folder")),
        )
}

impl ConnectionDialog {
    pub(super) fn toggle_folder(&mut self, folder: &str, cx: &mut Context<Self>) {
        if !self.collapsed_folders.remove(folder) {
            self.collapsed_folders.insert(folder.to_owned());
        }
        cx.notify();
    }

    /// Show the dialog pre-filled from the saved profile `id`.
    ///
    /// An unknown `id` opens the empty form rather than failing.
    pub fn open_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let load_error = self.reload_store();
        self.refresh_placeholders(cx);
        self.reset_form(cx);
        self.open = true;

        match self.store.get(id).cloned() {
            Some(profile) => {
                let has_secret = profile.save_secret;
                let agent = matches!(profile.auth, AuthMethod::Agent);
                self.fill_form(&profile, cx);
                self.pending_focus = Some(if agent {
                    FocusTarget::Host
                } else {
                    FocusTarget::Secret
                });
                if has_secret && !agent {
                    self.set_status(StatusLevel::Info, ts!("connection.saved_secret"));
                }
            }
            None => {
                log::warn!("connection dialog asked to open unknown profile {id}");
                self.pending_focus = Some(FocusTarget::Host);
            }
        }
        // Over the saved-secret note: that one only says what will happen,
        // this one that something cannot.
        if let Some(error) = load_error {
            self.set_status(
                StatusLevel::Error,
                ts!("connection.load_failed", error = error),
            );
        }

        cx.notify();
    }

    /// Show the dialog with the saved profile `id` loaded for editing.
    ///
    /// The other half of [`Self::open_profile`]: that one is on its way to a
    /// session and puts the caret in the field a connection is still waiting
    /// on, while this one is only the form, so the caret stays where an empty
    /// form would have left it — on the first field. An unknown `id` leaves the
    /// empty form standing, which is [`Self::select_profile`]'s behavior and
    /// the same thing [`Self::open_profile`] does with one.
    pub fn edit_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.open_new(cx);
        self.select_profile(id, cx);
    }

    /// Whether the dialog is visible.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Hide the dialog without connecting.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        // A connect still waiting on the keychain goes with the dialog: the
        // user backed out of it, and nothing may open behind their back.
        self.keychain_task = None;
        self.pending_focus = None;
        // Nothing renders it while the dialog is down, so a menu left standing
        // would reappear the next time the dialog comes up.
        self.context = None;
        // Belt and braces: every `open_*` resets the form anyway, but a closed
        // dialog must not carry a selection that outlives the reason for it.
        self.clear_local_selection();
        // A question nobody answered goes with the dialog it was asked in.
        self.delete_confirm = None;
        // A closed dialog has nothing to report; leaving the last message behind
        // would let it reappear for a moment the next time the dialog opens.
        self.status = None;
        // Never keep a secret in memory longer than the dialog is on screen.
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        // The jump hosts hold secrets of their own, one per row, and the rows
        // outlive a close: `reset_form` drops them on the next opening, which
        // is later than this rule allows.
        for row in &self.hop_rows {
            row.secret.update(cx, |input, cx| input.clear(cx));
        }
        cx.notify();
    }

    /// Saved profiles, in stored order.
    pub fn profiles(&self) -> Vec<SessionProfile> {
        self.store.profiles().to_vec()
    }

    /// Re-read the profile store so external edits are picked up when the dialog
    /// opens. A failure leaves the previously loaded profiles in place and is
    /// returned, for the dialog to say that nothing can be saved meanwhile.
    pub(super) fn reload_store(&mut self) -> Option<String> {
        match self.read_store() {
            Ok(store) => {
                self.store = store;
                None
            }
            Err(err) => {
                log::warn!("keeping the previously loaded profiles: {err:#}");
                Some(format!("{err:#}"))
            }
        }
    }

    /// The profile list as it is on disk now.
    ///
    /// Every change goes through this rather than through [`Self::store`]: the
    /// list on screen is empty or stale when the file could not be read, and
    /// writing it back would replace every profile in the file with it. A
    /// change applied to this copy leaves the rest of the file as it is, and a
    /// file that cannot be read cannot be written over at all.
    pub(super) fn read_store(&self) -> anyhow::Result<ProfileStore> {
        match &self.profiles_file {
            Some(path) => ProfileStore::load_from(path),
            None => ProfileStore::load(),
        }
    }

    /// Write `store`, which [`Self::read_store`] returned and one change was
    /// applied to, back where it came from.
    pub(super) fn write_store(&self, store: &ProfileStore) -> anyhow::Result<()> {
        match &self.profiles_file {
            Some(path) => store.save_to(path),
            None => store.save(),
        }
    }

    /// Say that the change just asked for was not made, because the profile
    /// list could not be read.
    fn refuse_unreadable(&mut self, err: anyhow::Error, cx: &mut Context<Self>) {
        log::error!("the profile list could not be read, so it was not changed: {err:#}");
        self.set_status(
            StatusLevel::Error,
            ts!("connection.load_failed", error = format!("{err:#}")),
        );
        cx.notify();
    }

    /// Load the profile `id` into the form.
    pub(super) fn select_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(profile) = self.store.get(id).cloned() else {
            return;
        };
        let has_secret = profile.save_secret;
        let agent = matches!(profile.auth, AuthMethod::Agent);
        self.fill_form(&profile, cx);
        self.status = None;
        if has_secret && !agent {
            self.set_status(StatusLevel::Info, ts!("connection.saved_secret"));
        }
        cx.notify();
    }

    /// Copy the profile `id` under a name of its own, and select the copy.
    ///
    /// The list is written back straight away, the way a delete writes it back:
    /// nothing else will, since a copy nobody connects to would otherwise live
    /// only until the dialog is closed. No secret comes with it — see
    /// [`ProfileStore::duplicate`] — so the copy asks for one the first time it
    /// is used, which is also why the form is filled from it: the copy is a
    /// profile that still wants finishing.
    ///
    /// Selecting it is skipped while the dialog is closed, which is how the
    /// empty state calls this. There is no form on screen to fill, and the next
    /// opening resets it anyway.
    pub(crate) fn duplicate_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let mut store = match self.read_store() {
            Ok(store) => store,
            Err(err) => return self.refuse_unreadable(err, cx),
        };
        let copy = store.duplicate(id);
        // Reported after the selection rather than before it: selecting has a
        // message of its own to put up or take down, and would clear this one.
        let written = copy.as_ref().map(|_| self.write_store(&store));
        // The file as it now is, which is also how a profile removed from it
        // since the list was drawn drops out of the list.
        self.store = store;
        let (Some(copy), Some(written)) = (copy, written) else {
            cx.notify();
            return;
        };
        if self.open {
            self.select_profile(copy.id, cx);
        }
        if let Err(err) = written {
            log::error!("could not write the profile list: {err:#}");
            self.set_status(
                StatusLevel::Error,
                ts!("connection.duplicate_failed", error = format!("{err:#}")),
            );
        }
        cx.notify();
    }

    /// Hands the dialog the saved dashboards, so a delete can say which of them
    /// follow a file over the profile it is about to forget.
    ///
    /// The workspace owns the list — no dialog of a window does — and calls
    /// this whenever it re-reads it.
    pub(crate) fn set_dashboards(&mut self, dashboards: &[Dashboard]) {
        self.dashboards = dashboards.to_vec();
    }

    /// The question [`Self::ask_delete_profile`] puts up for the profile `id`,
    /// or `None` once the profile has gone.
    pub(crate) fn delete_question_for(&self, id: Uuid) -> Option<Vec<SharedString>> {
        let profile = self.store.get(id)?;
        Some(delete_question(
            &profile.name,
            &dashboards_using(&self.dashboards, id),
        ))
    }

    /// Asks before the profile `id` is forgotten.
    ///
    /// Every route to a delete inside the dialog — the row's button and the row
    /// menu — lands here rather than on [`Self::delete_profile`]: the keychain
    /// entries go with the profile and cannot be put back, so the one press
    /// that removes them is the second.
    pub(super) fn ask_delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.context = None;
        self.delete_confirm = Some(id);
        cx.notify();
    }

    /// Takes the delete question back, and says whether one was standing.
    pub(super) fn cancel_delete(&mut self, cx: &mut Context<Self>) -> bool {
        if self.delete_confirm.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// The delete question's *Delete*: forgets the profile it was asked about.
    pub(super) fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.delete_confirm.take() {
            self.delete_profile(id, cx);
        }
    }

    /// Forget the profile `id`, together with any secret stored for it.
    ///
    /// Deleting the secret alongside the profile is what keeps the keychain from
    /// accumulating entries nothing refers to any more.
    ///
    /// Only two things can go wrong here, so all three outcomes are spelled out
    /// as whole sentences rather than clauses joined with "and": the conjunction
    /// and the clause order of such a join are not translatable.
    pub(crate) fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let mut store = match self.read_store() {
            Ok(store) => store,
            Err(err) => return self.refuse_unreadable(err, cx),
        };
        let Some(profile) = store.remove(id) else {
            self.store = store;
            cx.notify();
            return;
        };

        let list_error = self.write_store(&store).err().map(|err| format!("{err:#}"));
        self.store = store;
        // Every hop of the profile kept a credential of its own, under an id
        // that is about to refer to nothing. All of them are removed even when
        // one refuses, so a single locked entry cannot strand the rest; the
        // first failure is the one reported, since the recovery — the keychain
        // itself — is the same whichever of them it was.
        let mut secret_error = SecretStore::delete(id).err().map(|err| format!("{err:#}"));
        for hop in &profile.hops {
            if let Err(err) = SecretStore::delete(hop.id) {
                secret_error.get_or_insert_with(|| format!("{err:#}"));
            }
        }

        if self.editing == Some(id) {
            self.reset_form(cx);
        }

        // The error detail comes from the storage layer and stays in English.
        self.status = match (list_error, secret_error) {
            (None, None) => None,
            (Some(list_error), None) => Some(DialogStatus {
                level: StatusLevel::Error,
                lines: vec![ts!("connection.delete_failed_list", error = list_error)],
            }),
            (None, Some(secret_error)) => Some(DialogStatus {
                level: StatusLevel::Error,
                lines: vec![ts!("connection.delete_failed_secret", error = secret_error)],
            }),
            (Some(list_error), Some(secret_error)) => Some(DialogStatus {
                level: StatusLevel::Error,
                lines: vec![ts!(
                    "connection.delete_failed_both",
                    list_error = list_error,
                    secret_error = secret_error
                )],
            }),
        };
        cx.notify();
    }

    /// Switch the authentication method, discarding the secret typed for the
    /// previous one so it cannot be sent to the wrong place.
    pub(super) fn set_auth_kind(&mut self, kind: AuthKind, cx: &mut Context<Self>) {
        if self.auth_kind == kind {
            return;
        }
        self.auth_kind = kind;
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        self.status = None;
        cx.notify();
    }
}
