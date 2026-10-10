//! Navigation.

use super::*;

impl FilePanel {
    /// Selects every entry of the listing.
    ///
    /// The anchor lands on the first entry rather than staying where the last
    /// click left it, so that a <kbd>Shift</kbd>-click afterwards reads as
    /// "everything from the top down to here" — which is the only reading a
    /// selection that starts at the top can be narrowed by.
    pub(super) fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        if state.entries.is_empty() {
            return;
        }
        state.selected = state
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        state.anchor = state.entries.first().map(|entry| entry.name.clone());
        cx.notify();
    }

    /// Puts the selected entry's name on the clipboard.
    ///
    /// Nothing is said on the status line afterwards: the menu row named the
    /// act, the clipboard is where the result went, and a line reporting it
    /// would be the only one the panel writes for something that cannot fail.
    pub(super) fn copy_name(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self
            .showing()
            .and_then(SessionState::only_selected)
            .map(|entry| entry.name.clone())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(name));
    }

    /// Puts the selected entry's whole path on the clipboard.
    ///
    /// Built with [`join`], like every other path the panel hands to an
    /// operation, so what is copied is exactly the string a rename or a
    /// download would have aimed at — in the spelling the source itself uses
    /// rather than in this computer's.
    pub(super) fn copy_path(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.showing() else {
            return;
        };
        let (Some(directory), Some(entry)) = (state.path.as_deref(), state.only_selected()) else {
            return;
        };
        let path = join(directory, &entry.name);
        cx.write_to_clipboard(ClipboardItem::new_string(path));
    }

    /// Opens the context menu at `at`, over the row named `name`.
    ///
    /// A right-click on a row that is *not* selected selects it first, the way
    /// every file manager does: the menu that follows has to act on what the
    /// pointer is on. A right-click inside an existing selection leaves that
    /// selection alone, which is how a multi-entry command is asked for.
    ///
    /// `name` is `None` for the background and for the `..` row, and the menu
    /// then offers what can be done to the directory rather than to its
    /// contents.
    pub(super) fn open_context(
        &mut self,
        name: Option<&str>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        if state.path.is_none() {
            return;
        }

        let on_rows = match name {
            Some(name) if state.entries.iter().any(|entry| entry.name == name) => {
                if !state.selected.contains(name) {
                    state.select_only(name);
                }
                true
            }
            _ => false,
        };
        self.context = Some(PanelMenu {
            at,
            kind: MenuKind::Listing { on_rows },
        });
        cx.notify();
    }

    /// Puts the open menu away, and says whether there was one to put away.
    ///
    /// The answer is what lets the workspace layer <kbd>Escape</kbd>: the key
    /// belongs to this menu while it is up and to whatever is behind the panel
    /// once it is not, and only the panel knows which of those is the case.
    pub fn close_context(&mut self, cx: &mut Context<Self>) -> bool {
        if self.context.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Opens the dropdown of the breadcrumb piece pressed at `at`.
    ///
    /// A folded piece already knows what it offers — the ancestors the header
    /// had no room for — so its menu opens on the spot. Every other piece has
    /// to ask the source something first and opens once that answer lands: the
    /// directories beside it, or, for the root, which roots there are at all.
    pub(super) fn open_crumb(
        &mut self,
        menu: CrumbMenu,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Every kind navigates, and a session that cannot navigate — one whose
        // connection has since dropped — must not be offered a menu of places
        // its rows could not take it.
        if self.acting_on(cx).is_none() {
            return;
        }
        match menu {
            CrumbMenu::Folded(targets) => {
                self.context = Some(PanelMenu {
                    at,
                    kind: MenuKind::Crumb(targets),
                });
                cx.notify();
            }
            CrumbMenu::Root(root) => self.list_roots(root, at, cx),
            CrumbMenu::Siblings(directory) => self.list_siblings(directory, at, cx),
        }
    }

    /// Asks which roots the source has, to open the root piece's dropdown on.
    ///
    /// `root` is the one the panel is standing in, and is what the fallback
    /// lists if the answer turns out to hold nothing to choose between.
    pub(super) fn list_roots(&mut self, root: String, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((session, source, _)) = self.acting_on(cx) else {
            return;
        };
        if self.crumb_pending {
            return;
        }
        self.crumb_pending = true;

        cx.spawn(async move |panel, cx| {
            let result = source.roots().await;
            panel
                .update(cx, |panel, cx| {
                    panel.roots_arrived(session, root, at, result, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Opens the root piece's dropdown on the roots the source reported.
    ///
    /// Two or more and the menu is those roots, each row moving to the top of
    /// that tree — the current one included, since standing in `C:/Users` makes
    /// "go to `C:/`" as real a move as "go to `D:/`". Fewer than two and there
    /// is nothing a menu of roots could offer, so the piece falls back to the
    /// dropdown it has always had: the root's own subdirectories.
    ///
    /// A failure takes the same fallback rather than a notice. The only source
    /// that can fail here is the local one, whose failure means the drive
    /// letters could not be read — which leaves the panel knowing no less than
    /// a single-rooted source does, and the press still opens something.
    pub(super) fn roots_arrived(
        &mut self,
        session: EntityId,
        root: String,
        at: Point<Pixels>,
        result: Result<Vec<String>, FileError>,
        cx: &mut Context<Self>,
    ) {
        self.crumb_pending = false;
        // The answer describes a session that may no longer be the one on
        // screen; opening a menu of its roots over another session's listing
        // would navigate the wrong panel.
        if self.session.as_ref().map(Entity::entity_id) != Some(session) {
            return;
        }

        let roots = match result {
            Ok(roots) => roots,
            Err(error) => {
                log::debug!("could not read the roots of this filesystem: {error}");
                Vec::new()
            }
        };
        let Some(targets) = root_targets(roots) else {
            self.list_siblings(root, at, cx);
            return;
        };
        self.context = Some(PanelMenu {
            at,
            kind: MenuKind::Crumb(targets),
        });
        cx.notify();
    }

    /// Asks for the contents of `directory`, to open a breadcrumb dropdown on.
    pub(super) fn list_siblings(
        &mut self,
        directory: String,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some((session, source, _)) = self.acting_on(cx) else {
            return;
        };
        if self.crumb_pending {
            return;
        }
        self.crumb_pending = true;

        cx.spawn(async move |panel, cx| {
            let result = source.read_dir(&directory).await;
            panel
                .update(cx, |panel, cx| {
                    panel.siblings_arrived(session, directory, at, result, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Opens the breadcrumb dropdown the listing of `directory` was asked for.
    ///
    /// Only directories go on it — a breadcrumb piece can only ever stand for
    /// one — and only those whose names are safe to append to a path, since the
    /// rows are built by joining a server-sent name onto `directory`. A
    /// directory with nothing in it to offer opens no menu at all: an empty
    /// panel hanging off the header says less than the header already did.
    pub(super) fn siblings_arrived(
        &mut self,
        session: EntityId,
        directory: String,
        at: Point<Pixels>,
        result: Result<Vec<FileEntry>, FileError>,
        cx: &mut Context<Self>,
    ) {
        self.crumb_pending = false;
        // The answer describes the directory of a session that may no longer be
        // the one on screen; opening a menu of its paths over another session's
        // listing would navigate the wrong panel.
        if self.session.as_ref().map(Entity::entity_id) != Some(session) {
            return;
        }

        let mut entries = match result {
            Ok(entries) => entries,
            Err(error) => {
                let notice = Notice::from_error(&error, self.source_is_local(session));
                self.show_notice(session, notice, cx);
                return;
            }
        };
        // Sorted before the filter rather than after so the rows come out in
        // exactly the order the listing itself would show them in.
        sort_entries(&mut entries);
        let targets: Vec<CrumbTarget> = entries
            .into_iter()
            .filter(|entry| entry.is_dir && is_plain_name(&entry.name))
            .map(|entry| CrumbTarget {
                path: join(&directory, &entry.name),
                label: SharedString::from(entry.name),
            })
            .collect();

        if targets.is_empty() {
            cx.notify();
            return;
        }
        self.context = Some(PanelMenu {
            at,
            kind: MenuKind::Crumb(targets),
        });
        cx.notify();
    }

    /// Lists `path`, as a breadcrumb row asks.
    ///
    /// Allowed while a transfer is running, like the double-click that opens a
    /// directory: navigating changes what is listed, not what is moving.
    pub(super) fn open_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some((session, source, _)) = self.acting_on(cx) else {
            return;
        };
        self.go(session, source, Target::Exact(path), cx);
    }

    /// Opens the entry named `name`, if it is a directory.
    pub(super) fn activate(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some((session, source, path)) = self.acting_on(cx) else {
            return;
        };
        let is_dir = self
            .states
            .get(&session)
            .and_then(|state| state.entries.iter().find(|entry| entry.name == name))
            .is_some_and(|entry| entry.is_dir);
        if !is_dir {
            return;
        }
        self.go(session, source, Target::Exact(join(&path, name)), cx);
    }

    /// Opens the selected file in an editor pane, if it can be edited at all.
    ///
    /// The whole file is fetched here rather than in the pane, so that every
    /// refusal lands on the status line the user is already looking at instead
    /// of inside a pane that would have to be opened to say it could not be.
    /// The size is checked off the listing, before anything is transferred; the
    /// encoding cannot be, since only the bytes can answer it.
    pub(super) fn edit(&mut self, cx: &mut Context<Self>) {
        let Some((id, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(session) = self.session.clone() else {
            return;
        };
        // Read out of the borrow before anything below wants `self` mutably.
        let Some((name, size)) = self
            .states
            .get(&id)
            .and_then(SessionState::only_selected)
            .filter(|entry| !entry.is_dir)
            .map(|entry| (SharedString::from(entry.name.clone()), entry.size))
        else {
            return;
        };

        let is_local = self.source_is_local(id);
        if size > MAX_EDIT_BYTES {
            self.show_notice(id, edit_notice(&LoadError::TooLarge, is_local), cx);
            return;
        }

        // The session's own charset is the opening guess, because a file on a
        // host whose shell speaks EUC-KR is overwhelmingly likely to be written
        // in it — and it is the same answer the terminal is already decoding
        // that host with. The status bar's picker is where a file that turns out
        // to disagree gets corrected.
        let charset = Charset::from_label_or_utf8(&session.read(cx).effective(cx).charset);

        cx.spawn(async move |panel, cx| {
            let loaded = match read_file(&source, &directory, &name).await {
                Ok(bytes) => TextFile::decode(&bytes, charset).map(|file| (file, bytes)),
                Err(error) => Err(LoadError::Transport(error)),
            };
            // Asked here, beside the read, because this is the future that
            // already has the source and the path in hand and the only one that
            // can afford the round trip: the pane is built on the frame the
            // event lands on, and a probe there would hold the window up. Only
            // when there is going to be a pane at all — a file that could not be
            // read is refused above, and asking about it would buy nothing.
            let writable = match &loaded {
                Ok(_) => source.writable(&file_path(&directory, &name)).await,
                Err(_) => true,
            };
            // And only then the second question, which is what the *first* one
            // leads to: a file that can be written has no use for a way around
            // the account that can write it, and asking anyway would spend up
            // to three round trips on every file opened. A writable file
            // therefore carries `None`, and the pane reads the field only while
            // it is locked.
            let root_access = if writable {
                RootAccess::None
            } else {
                source.root_access().await
            };
            panel
                .update(cx, |panel, cx| match loaded {
                    Ok((file, original_bytes)) => {
                        cx.emit(FilePanelEvent::OpenEditor(Box::new(OpenEditor {
                            session,
                            source,
                            dir: directory,
                            name,
                            file,
                            original_bytes,
                            writable,
                            root_access,
                        })))
                    }
                    Err(error) => panel.show_notice(id, edit_notice(&error, is_local), cx),
                })
                .ok();
        })
        .detach();
    }

    /// Moves to the parent of the current directory.
    pub(super) fn open_parent(&mut self, cx: &mut Context<Self>) {
        let Some((session, source, path)) = self.acting_on(cx) else {
            return;
        };
        self.go(
            session,
            source,
            Target::Resolve(join(&path, PARENT_NAME)),
            cx,
        );
    }

    /// Asks the platform for files or a folder and copies them into the listed
    /// directory.
    ///
    /// `folders` picks which of the two pickers opens, because no single dialog
    /// offers both on every platform: macOS's `NSOpenPanel` can choose files and
    /// directories at once, but Windows' `IFileOpenDialog` turns into a folder
    /// browser once `FOS_PICKFOLDERS` is set, and the Linux portal's `directory`
    /// flag is just as exclusive. Two buttons behave the same everywhere; one
    /// button would behave differently on each.
    pub(super) fn pick_upload(&mut self, folders: bool, cx: &mut Context<Self>) {
        let Some((_, source, _)) = self.acting_on(cx) else {
            return;
        };
        let is_local = source.is_local();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: !folders,
            directories: folders,
            multiple: true,
            prompt: Some(if folders {
                ts!(key(
                    is_local,
                    "files.select_upload_folder",
                    "files.local.select_copy_in_folder"
                ))
            } else {
                ts!(key(
                    is_local,
                    "files.select_upload",
                    "files.local.select_copy_in"
                ))
            }),
        });

        cx.spawn(async move |panel, cx| {
            let chosen = match paths.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    log::warn!("the file picker could not be opened: {error:#}");
                    return;
                }
            };
            panel.update(cx, |panel, cx| panel.upload(chosen, cx)).ok();
        })
        .detach();
    }
}
