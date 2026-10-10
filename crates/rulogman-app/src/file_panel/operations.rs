//! Operations.

use super::*;

/// Walks `paths` and works out everything an upload into `directory` will do.
///
/// Runs on a background thread, because a dropped folder can hold tens of
/// thousands of entries and every one of them costs a `stat`.
///
/// Two rules decide what is in the plan:
///
/// * **Symlinked directories are left out entirely**, not followed. A tree can
///   link back into itself, and a walk that followed such a link would recurse
///   until it ran out of memory. A symlinked *file* is sent as its target,
///   which is what dragging a link into a terminal usually means.
/// * **Anything that cannot be stat'ed is left out and logged** — a broken
///   link, or a file removed between the listing and the walk. Failing the
///   whole batch over one of them would be worse than copying the rest.
///
/// The walk is breadth-first so that [`UploadPlan::directories`] comes out with
/// parents before children: SFTP has no `mkdir -p`, so the list is created in
/// exactly that order.
pub(super) fn plan_upload(paths: Vec<PathBuf>, directory: String) -> UploadPlan {
    let mut plan = UploadPlan::default();
    let mut queue: VecDeque<(PathBuf, String)> = paths
        .into_iter()
        .map(|path| (path, directory.clone()))
        .collect();

    while let Some((local, parent)) = queue.pop_front() {
        // `symlink_metadata` describes the link itself, so this is the real
        // directory test; a link to one falls through to the follow below.
        let Ok(link) = std::fs::symlink_metadata(&local) else {
            log::debug!("not uploading {}: it could not be read", local.display());
            continue;
        };
        let Some(name) = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            log::debug!("not uploading {}: it has no file name", local.display());
            continue;
        };

        if link.is_dir() {
            let remote = join(&parent, &name);
            match std::fs::read_dir(&local) {
                Ok(entries) => {
                    plan.directories.push(remote.clone());
                    for entry in entries.flatten() {
                        queue.push_back((entry.path(), remote.clone()));
                    }
                }
                Err(error) => {
                    log::debug!("not uploading {}: {error}", local.display());
                }
            }
            continue;
        }

        let Ok(target) = std::fs::metadata(&local) else {
            log::debug!(
                "not uploading {}: its target could not be read",
                local.display()
            );
            continue;
        };
        if target.is_dir() {
            log::debug!("not uploading {}: it links to a directory", local.display());
            continue;
        }
        plan.total = plan.total.saturating_add(target.len());
        plan.files.push(PlannedUpload {
            local,
            directory: parent,
            size: target.len(),
        });
    }
    plan
}

/// Walks the remote directory `remote` and works out what a download into
/// `local` will do.
///
/// Mirrors [`plan_upload`], with the same cycle rule in the other direction: an
/// entry that is a symlink *and* a directory is not descended into, because a
/// remote tree can link back into itself and there is no cheap way to prove it
/// does not. Sizes come from the listing, which is what gives the progress bar
/// a total without a `stat` per file.
pub(super) async fn plan_download(
    source: &Arc<dyn FileSource>,
    remote: String,
    local: PathBuf,
) -> Result<DownloadPlan, FileError> {
    let mut plan = DownloadPlan {
        directories: vec![local.clone()],
        ..DownloadPlan::default()
    };
    let mut queue = VecDeque::from([(remote, local)]);

    while let Some((remote, local)) = queue.pop_front() {
        for entry in source.read_dir(&remote).await? {
            if !is_plain_name(&entry.name) {
                log::debug!("not downloading {}/{}: odd name", remote, entry.name);
                continue;
            }
            let child_remote = join(&remote, &entry.name);
            let child_local = local.join(&entry.name);

            if entry.is_dir {
                if entry.is_symlink {
                    log::debug!("not downloading {child_remote}: it links to a directory");
                    continue;
                }
                plan.directories.push(child_local.clone());
                queue.push_back((child_remote, child_local));
            } else {
                plan.total = plan.total.saturating_add(entry.size);
                plan.files.push(PlannedDownload {
                    remote: child_remote,
                    local: child_local,
                    size: entry.size,
                });
            }
        }
    }
    Ok(plan)
}

/// Creates the local directories of `plan` and fetches its files in order.
///
/// Shared by the one-entry and the many-entry download so that both measure
/// against the same bar in the same way; only the question asked beforehand and
/// the sentence said afterwards differ between them.
pub(super) async fn run_download(
    panel: &WeakEntity<FilePanel>,
    cx: &mut AsyncApp,
    session: EntityId,
    source: &Arc<dyn FileSource>,
    plan: DownloadPlan,
) -> Ran {
    if panel
        .update(cx, |panel, cx| panel.size_transfer(session, plan.total, cx))
        .is_err()
    {
        return Ran::Abandoned;
    }

    // One hop to a background thread for every directory at once: the creations
    // are microseconds each but there can be thousands, and this is a UI thread
    // that also has to keep drawing the bar.
    if let Err(error) = create_all(cx, plan.directories).await {
        return Ran::Finished(Some(error));
    }

    let mut moved = 0u64;
    for file in plan.files {
        let label = file_name(&file.local);
        if panel
            .update(cx, |panel, cx| {
                panel.transfer_file(session, label, moved, cx);
            })
            .is_err()
        {
            return Ran::Abandoned;
        }

        let (sender, receiver) = mpsc::unbounded();
        let transfer = source.copy_out(&file.remote, file.local, Some(sender));
        match follow(panel, cx, session, moved, receiver, transfer).await {
            Ok(()) => moved = moved.saturating_add(file.size),
            Err(error) => return Ran::Finished(Some(error)),
        }
    }
    Ran::Finished(None)
}

/// Works out every remote call a delete of `targets` in `directory` will make.
///
/// Two rules, and both of them are about not deleting more than was asked:
///
/// * **A symbolic link is removed as a link**, never descended into, even when
///   it points at a directory. Walking one would delete the target's contents —
///   somewhere else entirely on the server — and leave the link behind.
/// * **A real directory is emptied from the leaves upwards.** SFTP refuses to
///   remove a directory that still holds anything, so the order is not a
///   preference but the only order that works: the walk collects directories
///   breadth-first and the plan hands them back reversed.
pub(super) async fn plan_delete(
    source: &Arc<dyn FileSource>,
    directory: &str,
    targets: Vec<FileEntry>,
) -> Result<Vec<Removal>, FileError> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut queue = VecDeque::new();

    for entry in targets {
        let path = join(directory, &entry.name);
        if needs_walking(&entry) {
            directories.push(removal(path.clone(), &entry.name, true));
            queue.push_back(path);
        } else {
            files.push(removal(path, &entry.name, false));
        }
    }

    while let Some(parent) = queue.pop_front() {
        for entry in source.read_dir(&parent).await? {
            if !is_plain_name(&entry.name) {
                log::debug!("not deleting {}/{}: odd name", parent, entry.name);
                continue;
            }
            let path = join(&parent, &entry.name);
            if entry.is_dir && !entry.is_symlink {
                directories.push(removal(path.clone(), &entry.name, true));
                queue.push_back(path);
            } else {
                files.push(removal(path, &entry.name, false));
            }
        }
    }

    // Files first — they can go in any order — then the directories from the
    // deepest outwards, which is what the reversal of a breadth-first walk is.
    directories.reverse();
    files.extend(directories);
    Ok(files)
}

/// Whether a delete has to walk into `entry` before it can remove it.
///
/// True only for a *real* directory. A symbolic link is removed as itself no
/// matter what it points at: the listing reports a link to a directory with
/// `is_dir` set so that it can be navigated into, and treating that as a
/// directory here would walk somewhere else on the server and delete the
/// target's contents while leaving the link behind.
pub(super) fn needs_walking(entry: &FileEntry) -> bool {
    entry.is_dir && !entry.is_symlink
}

/// Builds one entry of a delete plan.
pub(super) fn removal(path: String, name: &str, directory: bool) -> Removal {
    Removal {
        path,
        name: SharedString::from(name.to_owned()),
        directory,
    }
}

/// Whether a name is safe to append to a path, local or remote.
///
/// Names come from the server, and one answering `..` or `a/b` would make
/// [`Path::join`] write *outside* the directory the user picked — or, on the
/// remote side, aim a delete at something the user never saw. A listing has no
/// legitimate use for either, so a name carrying one is dropped rather than
/// sanitised: there is no correct guess at what it was supposed to be.
///
/// The same test guards the rename field, where the name comes from the user
/// instead. It is the same hazard from the other direction — `../notes.txt`
/// typed into it would move the entry out of the directory on screen — and the
/// answer is the same: refuse it rather than interpret it.
pub(super) fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

/// Creates every directory in `directories`, on a background thread.
///
/// One hop for the whole list rather than one per directory: each creation is
/// microseconds, but a deep tree has thousands of them and this is a thread
/// that also has to keep drawing the progress bar.
pub(super) async fn create_all(
    cx: &mut AsyncApp,
    directories: Vec<PathBuf>,
) -> Result<(), FileError> {
    if directories.is_empty() {
        return Ok(());
    }
    cx.background_executor()
        .spawn(async move {
            for directory in directories {
                std::fs::create_dir_all(&directory).map_err(|error| {
                    FileError::Local(format!("could not create {}: {error}", directory.display()))
                })?;
            }
            Ok(())
        })
        .await
}

/// Drives one file transfer while feeding its byte count into the status line.
///
/// The transfer future and its progress stream are polled *together*, which is
/// the whole reason the SFTP layer takes a channel: awaiting the transfer first
/// and reading the counts afterwards would leave a single large file showing no
/// movement at all until it landed. `base` is what the batch had already moved
/// before this file started, so the bar measures the batch and not the file.
///
/// The service drops its sender before answering, so the stream ends first and
/// the loop always leaves through the transfer arm.
pub(super) async fn follow<T>(
    panel: &WeakEntity<FilePanel>,
    cx: &mut AsyncApp,
    session: EntityId,
    base: u64,
    mut receiver: UnboundedReceiver<u64>,
    transfer: impl Future<Output = Result<T, FileError>>,
) -> Result<T, FileError> {
    let transfer = futures::FutureExt::fuse(transfer);
    futures::pin_mut!(transfer);

    loop {
        futures::select! {
            outcome = transfer => return outcome,
            moved = receiver.next() => {
                let Some(moved) = moved else { continue };
                if panel
                    .update(cx, |panel, cx| {
                        panel.advance_transfer(session, base.saturating_add(moved), cx);
                    })
                    .is_err()
                {
                    // The panel is gone; the transfer still has to be waited
                    // for, or dropping it here would abandon a half-written
                    // file with no one to report it.
                    return transfer.await;
                }
            }
        }
    }
}

impl FilePanel {
    /// Records how many bytes the batch that just started will move.
    pub(super) fn size_transfer(&mut self, session: EntityId, total: u64, cx: &mut Context<Self>) {
        let Some(transfer) = self
            .states
            .get_mut(&session)
            .and_then(|state| state.transfer.as_mut())
        else {
            return;
        };
        transfer.total = total;
        transfer.percent = TransferProgress::percent_of(transfer.done, total);
        cx.notify();
    }

    /// Moves the batch on to `name`, with `done` bytes already behind it.
    pub(super) fn transfer_file(
        &mut self,
        session: EntityId,
        name: SharedString,
        done: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(transfer) = self
            .states
            .get_mut(&session)
            .and_then(|state| state.transfer.as_mut())
        else {
            return;
        };
        transfer.name = name;
        transfer.done = done;
        transfer.percent = TransferProgress::percent_of(done, transfer.total);
        cx.notify();
    }

    /// Records `done` bytes moved, repainting only when that is visible.
    ///
    /// Called once per 64 KB chunk. Repainting each time would redraw the whole
    /// panel hundreds of times a second for a bar that has not moved a pixel,
    /// so the notify is spent only when the whole percent changes.
    pub(super) fn advance_transfer(
        &mut self,
        session: EntityId,
        done: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(transfer) = self
            .states
            .get_mut(&session)
            .and_then(|state| state.transfer.as_mut())
        else {
            return;
        };
        transfer.done = done;
        let percent = TransferProgress::percent_of(done, transfer.total);
        if percent == transfer.percent {
            return;
        }
        transfer.percent = percent;
        cx.notify();
    }

    /// Releases the transfer slot and reports the outcome.
    pub(super) fn finish_transfer(
        &mut self,
        session: EntityId,
        notice: Notice,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        state.transfer = None;
        // Every caller that refreshes the listing does so straight after this,
        // and the answer must not take the verdict off the screen with it. What
        // does take it off is the expiry `show_notice` arms below, so a success
        // survives its own refresh and still does not stay up for good.
        state.keep_notice = true;
        self.show_notice(session, notice, cx);
    }

    /// Lists the current directory again.
    ///
    /// Also the way out of a failed first listing, which is not retried on its
    /// own.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let id = session.entity_id();
        let Some(source) = session.read(cx).files(cx) else {
            return;
        };
        let target = match self.states.get(&id).and_then(|state| state.path.clone()) {
            Some(path) => Target::Exact(path),
            None => Target::Home,
        };
        self.go(id, source, target, cx);
    }

    /// Copies `paths` into the current directory, recursing into folders.
    ///
    /// The whole tree is resolved before anything moves — see [`plan_upload`] —
    /// so the progress bar has a total to measure against from the first chunk,
    /// and so the walk itself happens off the UI thread. The directories are
    /// then created in order and the files sent one after another; a failure
    /// stops the batch, leaving what already landed in place, which the refresh
    /// at the end makes visible.
    pub(super) fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        if !self.begin_transfer(session, Activity::Upload, cx) {
            return;
        }

        // Read here, where the source is still in hand: the sentences below are
        // said long after this returns, and by then the panel may be showing
        // another tab entirely.
        let is_local = source.is_local();
        let listing = directory.clone();
        // A dropped folder can hold tens of thousands of entries and every one
        // of them costs a `stat`; on the UI thread that is dropped frames.
        let scan = cx
            .background_executor()
            .spawn(async move { plan_upload(paths, directory) });

        cx.spawn(async move |panel, cx| {
            let plan = scan.await;
            let folders = plan.directories.len();
            if panel
                .update(cx, |panel, cx| panel.size_transfer(session, plan.total, cx))
                .is_err()
            {
                return;
            }

            let mut failure = None;
            // Parents come first out of the plan, which is the whole ordering
            // requirement: SFTP has no `mkdir -p`.
            for directory in plan.directories {
                if let Err(error) = source.mkdir(&directory).await {
                    failure = Some(error);
                    break;
                }
            }

            let mut moved = 0u64;
            let mut sent = 0usize;
            let mut last = SharedString::default();
            if failure.is_none() {
                for file in plan.files {
                    let name = file_name(&file.local);
                    if panel
                        .update(cx, |panel, cx| {
                            panel.transfer_file(session, name.clone(), moved, cx);
                        })
                        .is_err()
                    {
                        return;
                    }

                    let (sender, receiver) = mpsc::unbounded();
                    let transfer = source.copy_in(file.local, &file.directory, Some(sender));
                    match follow(&panel, cx, session, moved, receiver, transfer).await {
                        Ok(_) => {
                            sent += 1;
                            last = name;
                            moved = moved.saturating_add(file.size);
                        }
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
            }

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None if folders > 0 => Notice::Info(ts!(
                    key(is_local, "files.uploaded_tree", "files.local.copied_tree"),
                    files = sent,
                    folders = folders
                )),
                None if sent == 1 => Notice::Info(ts!(
                    key(is_local, "files.uploaded", "files.local.copied"),
                    name = last
                )),
                // Nothing at all could be read — every path was a broken link,
                // or vanished between the drop and the walk. Saying "uploaded
                // 0 files" would read as success, which it is not.
                None if sent == 0 => Notice::Error(ts!(key(
                    is_local,
                    "files.nothing_to_upload",
                    "files.local.nothing_to_copy"
                ))),
                None => Notice::Info(ts!(
                    key(is_local, "files.uploaded_many", "files.local.copied_many"),
                    count = sent
                )),
            };

            panel
                .update(cx, |panel, cx| {
                    panel.finish_transfer(session, notice, cx);
                    if sent > 0 || folders > 0 {
                        panel.go(session, source, Target::Exact(listing), cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Saves the selection locally, asking where to put it first.
    ///
    /// The question differs with the size of the selection, because the answer
    /// has to: one entry can be renamed on the way down, so it gets a save
    /// dialog with its name in it, while several have to keep the names they
    /// have and so only need a folder to land in.
    pub(super) fn download(&mut self, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let chosen: Vec<FileEntry> = state.selection().cloned().collect();

        match chosen.as_slice() {
            [] => (),
            [only] => self.download_one(session, source, &directory, only, cx),
            _ => self.download_many(session, source, &directory, chosen, cx),
        }
    }

    /// Saves one entry, asking for the path to write it to.
    ///
    /// A directory is copied whole: the remote tree is walked with `read_dir`,
    /// the local directories are created, and the files come down one after
    /// another against the same progress bar an upload uses.
    pub(super) fn download_one(
        &mut self,
        session: EntityId,
        source: Arc<dyn FileSource>,
        directory: &str,
        entry: &FileEntry,
        cx: &mut Context<Self>,
    ) {
        let name = entry.name.clone();
        let is_dir = entry.is_dir;
        let size = entry.size;
        let remote = join(directory, &name);
        // Read while the source is in hand, as in `upload`: only the failure
        // sentence needs it here, since "Saved to <path>." is true of a copy
        // out of either kind of filesystem and is shared between them.
        let is_local = source.is_local();
        let prompt = cx.prompt_for_new_path(&suggested_directory(), Some(&name));

        cx.spawn(async move |panel, cx| {
            let local = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    log::warn!("the save dialog could not be opened: {error:#}");
                    return;
                }
            };

            // Claimed only now, not before the dialog: a save dialog can stand
            // open for minutes, and a transfer that was running when it opened
            // has very likely finished by the time a path comes back.
            let claimed = panel.update(cx, |panel, cx| {
                panel.begin_transfer(session, Activity::Download, cx)
            });
            if !matches!(claimed, Ok(true)) {
                return;
            }
            let shown = local.display().to_string();

            let plan = if is_dir {
                match plan_download(&source, remote, local).await {
                    Ok(plan) => plan,
                    Err(error) => {
                        panel
                            .update(cx, |panel, cx| {
                                let notice = Notice::from_error(&error, is_local);
                                panel.finish_transfer(session, notice, cx);
                            })
                            .ok();
                        return;
                    }
                }
            } else {
                DownloadPlan {
                    directories: Vec::new(),
                    total: size,
                    files: vec![PlannedDownload {
                        remote,
                        local,
                        size,
                    }],
                }
            };

            let count = plan.files.len();
            let failure = match run_download(&panel, cx, session, &source, plan).await {
                Ran::Finished(failure) => failure,
                Ran::Abandoned => return,
            };

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None if is_dir => {
                    Notice::Info(ts!("files.downloaded_tree", count = count, path = shown))
                }
                None => Notice::Info(ts!("files.downloaded", path = shown)),
            };
            panel
                .update(cx, |panel, cx| panel.finish_transfer(session, notice, cx))
                .ok();
        })
        .detach();
    }

    /// Saves several entries into one folder, keeping their names.
    ///
    /// The whole selection moves as a single batch against a single progress
    /// bar: separate transfers would each want the session's one progress slot
    /// and all but the first would be refused. A local file of the same name is
    /// overwritten, exactly as a single download overwrites what the save
    /// dialog was pointed at.
    pub(super) fn download_many(
        &mut self,
        session: EntityId,
        source: Arc<dyn FileSource>,
        directory: &str,
        chosen: Vec<FileEntry>,
        cx: &mut Context<Self>,
    ) {
        let directory = directory.to_owned();
        let is_local = source.is_local();
        // "Save" names the answer this dialog is asking for — a destination —
        // and says nothing about where the entries are coming from, so it is
        // the one picker label both kinds of source share.
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(ts!("files.select_download_folder")),
        });

        cx.spawn(async move |panel, cx| {
            let destination = match prompt.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => path,
                    None => return,
                },
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    log::warn!("the folder picker could not be opened: {error:#}");
                    return;
                }
            };

            let claimed = panel.update(cx, |panel, cx| {
                panel.begin_transfer(session, Activity::Download, cx)
            });
            if !matches!(claimed, Ok(true)) {
                return;
            }
            let shown = destination.display().to_string();

            let mut plan = DownloadPlan::default();
            let mut failure = None;
            for entry in chosen {
                // Server-sent names reach `Path::join` here, so the same guard
                // the recursive walk uses has to stand at the top of it too.
                if !is_plain_name(&entry.name) {
                    log::debug!("not downloading {}/{}: odd name", directory, entry.name);
                    continue;
                }
                let remote = join(&directory, &entry.name);
                let local = destination.join(&entry.name);

                if entry.is_dir {
                    match plan_download(&source, remote, local).await {
                        Ok(part) => plan.absorb(part),
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                } else {
                    plan.total = plan.total.saturating_add(entry.size);
                    plan.files.push(PlannedDownload {
                        remote,
                        local,
                        size: entry.size,
                    });
                }
            }

            let count = plan.files.len();
            if failure.is_none() {
                failure = match run_download(&panel, cx, session, &source, plan).await {
                    Ran::Finished(failure) => failure,
                    Ran::Abandoned => return,
                };
            }

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None => Notice::Info(ts!("files.downloaded_tree", count = count, path = shown)),
            };
            panel
                .update(cx, |panel, cx| panel.finish_transfer(session, notice, cx))
                .ok();
        })
        .detach();
    }

    /// Asks whether the selection should really be deleted.
    ///
    /// Nothing is sent until the question is answered — this only records what
    /// was asked about. Deleting is the one thing the panel does that cannot be
    /// undone by doing it again, and it is a *single* right-click away, so it
    /// gets the one confirmation step in the panel.
    pub(super) fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        let names: Vec<String> = state
            .selection()
            .map(|entry| entry.name.clone())
            .filter(|name| is_plain_name(name))
            .collect();
        if names.is_empty() {
            return;
        }
        state.prompt = Some(Prompt::Delete(names));
        cx.notify();
    }

    /// Drops whatever question is open without acting on it.
    pub(super) fn cancel_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        if state.prompt.take().is_some() {
            self.focus_prompt = false;
            cx.notify();
        }
    }

    /// Deletes the entries the open confirmation names.
    ///
    /// Each is removed the way its own type requires: a file — or a symbolic
    /// link of any kind — with one call, a real directory by walking it and
    /// removing the contents from the leaves upwards, since SFTP has no
    /// recursive delete. The walk itself is remote round trips, so it runs
    /// under the progress slot rather than before it.
    pub(super) fn delete(&mut self, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        // Read before it is cleared, so that a call arriving with some *other*
        // question open leaves that question standing instead of eating it.
        let names = match state.prompt.as_ref() {
            Some(Prompt::Delete(names)) => names.clone(),
            _ => return,
        };
        state.prompt = None;
        self.focus_prompt = false;

        // Resolved against the listing now rather than inside the walk: this is
        // where "is it a link?" is still answerable without another round trip,
        // and getting that wrong would delete a link's target instead of the
        // link.
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let targets: Vec<FileEntry> = names
            .iter()
            .filter_map(|name| state.entries.iter().find(|entry| &entry.name == name))
            .cloned()
            .collect();
        let count = targets.len();
        let last = targets
            .first()
            .map(|entry| SharedString::from(entry.name.clone()))
            .unwrap_or_default();
        if targets.is_empty() || !self.begin_transfer(session, Activity::Delete, cx) {
            return;
        }
        // Only the failure sentence needs it: a delete removes the same thing
        // and reports it the same way whichever filesystem it runs on.
        let is_local = source.is_local();

        cx.spawn(async move |panel, cx| {
            let removals = match plan_delete(&source, &directory, targets).await {
                Ok(removals) => removals,
                Err(error) => {
                    panel
                        .update(cx, |panel, cx| {
                            let notice = Notice::from_error(&error, is_local);
                            panel.finish_transfer(session, notice, cx);
                            panel.go(session, source, Target::Exact(directory), cx);
                        })
                        .ok();
                    return;
                }
            };

            if panel
                .update(cx, |panel, cx| {
                    panel.size_transfer(session, removals.len() as u64, cx);
                })
                .is_err()
            {
                return;
            }

            let mut failure = None;
            let mut done = 0u64;
            for removal in removals {
                if panel
                    .update(cx, |panel, cx| {
                        panel.transfer_file(session, removal.name.clone(), done, cx);
                    })
                    .is_err()
                {
                    return;
                }
                let outcome = if removal.directory {
                    source.remove_dir(&removal.path).await
                } else {
                    source.remove_file(&removal.path).await
                };
                if let Err(error) = outcome {
                    failure = Some(error);
                    break;
                }
                done = done.saturating_add(1);
            }

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None if count == 1 => Notice::Info(ts!("files.deleted", name = last)),
                None => Notice::Info(ts!("files.deleted_many", count = count)),
            };
            // Listed again either way: a batch stopped half-way has removed
            // real entries, and leaving them on screen would be worse than the
            // failure itself.
            panel
                .update(cx, |panel, cx| {
                    panel.finish_transfer(session, notice, cx);
                    if let Some(state) = panel.states.get_mut(&session) {
                        state.reset_selection();
                    }
                    panel.go(session, source, Target::Exact(directory), cx);
                })
                .ok();
        })
        .detach();
    }

    /// Opens the rename field over the one selected entry.
    pub(super) fn begin_rename(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let from = {
            let mut selection = state.selection();
            match (selection.next(), selection.next()) {
                (Some(entry), None) => entry.name.clone(),
                _ => return,
            }
        };

        let panel = cx.entity().downgrade();
        let input = cx.new(|cx| {
            // The typed text arrives as an argument rather than being read back
            // off the field: this runs inside the field's own update, and
            // reading an entity that is currently leased panics.
            let mut input = TextInput::new(cx)
                .context_menu(input_menu_labels)
                .on_submit(move |typed, _window, cx| {
                    let typed = typed.to_owned();
                    panel
                        .update(cx, |panel, cx| panel.commit_rename(&typed, cx))
                        .ok();
                });
            input.set_content(from.clone(), cx);
            input
        });

        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        state.prompt = Some(Prompt::Rename { from, input });
        self.focus_prompt = true;
        cx.notify();
    }

    /// Applies `typed` as the new name of the entry the rename field is over.
    ///
    /// The name comes in as an argument rather than being read off the field,
    /// because one of the two callers is the field's own `Enter` handler and
    /// the field is leased while that runs.
    ///
    /// The new name is checked before it is sent, not after: the only thing a
    /// server can say about `../etc` is that it worked, and by then something
    /// outside the directory the user was looking at has been renamed.
    pub(super) fn commit_rename(&mut self, typed: &str, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let Some(Prompt::Rename { from, .. }) = state.prompt.as_ref() else {
            return;
        };
        let from = from.clone();
        // Trimmed because a trailing space is legal on every server and almost
        // never meant: it produces a name that looks identical to one already
        // there and cannot be typed again by hand.
        let to = typed.trim().to_owned();

        if to == from {
            self.cancel_prompt(cx);
            return;
        }
        if !is_plain_name(&to) {
            self.show_notice(session, Notice::Error(ts!("files.invalid_name")), cx);
            return;
        }
        // Exclusive with a transfer for the same reason two transfers are:
        // both end in a listing, and the one that lands second would describe a
        // directory the other has already changed.
        if self
            .states
            .get(&session)
            .is_some_and(|state| state.transfer.is_some())
        {
            self.show_notice(session, Notice::Error(ts!("files.transfer_busy")), cx);
            return;
        }

        let old = join(&directory, &from);
        let new = join(&directory, &to);
        if let Some(state) = self.states.get_mut(&session) {
            state.prompt = None;
        }
        self.focus_prompt = false;
        cx.notify();

        cx.spawn(async move |panel, cx| {
            let outcome = source.rename(&old, &new).await;
            panel
                .update(cx, |panel, cx| match outcome {
                    Ok(()) => {
                        if let Some(state) = panel.states.get_mut(&session) {
                            state.keep_notice = true;
                            // Carried across the refresh: the listing keeps its
                            // selection when the path has not changed, so this
                            // leaves the renamed row highlighted where the old
                            // one was.
                            state.select_only(&to);
                        }
                        let said = ts!("files.renamed", name = to.clone());
                        panel.show_notice(session, Notice::Info(said), cx);
                        panel.go(session, source, Target::Exact(directory), cx);
                    }
                    Err(error) => {
                        let notice = Notice::from_error(&error, panel.source_is_local(session));
                        panel.show_notice(session, notice, cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Opens the field that names a directory to create here.
    pub(super) fn begin_new_folder(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        // Nothing to create *in* until the first listing has landed, and the
        // path is what the name would be joined onto.
        if !self
            .states
            .get(&session)
            .is_some_and(|state| state.path.is_some())
        {
            return;
        }

        let panel = cx.entity().downgrade();
        let input = cx.new(|cx| {
            // As with rename: the typed text arrives as an argument because
            // this runs inside the field's own update, and reading an entity
            // that is currently leased panics.
            TextInput::new(cx)
                .context_menu(input_menu_labels)
                .placeholder(ts!("files.new_folder_placeholder"))
                .on_submit(move |typed, _window, cx| {
                    let typed = typed.to_owned();
                    panel
                        .update(cx, |panel, cx| panel.commit_new_folder(&typed, cx))
                        .ok();
                })
        });

        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        state.prompt = Some(Prompt::NewFolder { input });
        self.focus_prompt = true;
        cx.notify();
    }

    /// Creates the directory `typed` names, inside the one on screen.
    ///
    /// The mirror of [`FilePanel::commit_rename`], down to why the name comes in
    /// as an argument and why it is checked here rather than left to the server:
    /// `../backup` would create a directory the user cannot see, in a directory
    /// they were not looking at.
    ///
    /// **An existing directory of that name is not an error.** [`FileSource::mkdir`]
    /// is idempotent — the recursive upload depends on that — so asking for a
    /// name already taken by a directory simply selects the one already there.
    /// Nothing is overwritten and nothing inside it is touched, so there is no
    /// harm to report; a name taken by a *file* is a real collision and does
    /// surface as the server's own refusal.
    pub(super) fn commit_new_folder(&mut self, typed: &str, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        if !self
            .states
            .get(&session)
            .is_some_and(|state| matches!(state.prompt, Some(Prompt::NewFolder { .. })))
        {
            return;
        }
        // Trimmed for the reason the rename field trims: a name with a trailing
        // space is legal, indistinguishable on screen from one without, and
        // essentially never what was meant.
        let name = typed.trim().to_owned();

        // Covers the empty field too — `is_plain_name` rejects it — so an
        // `Enter` on an untouched field says why instead of doing nothing.
        if !is_plain_name(&name) {
            self.show_notice(session, Notice::Error(ts!("files.invalid_name")), cx);
            return;
        }
        // Exclusive with a transfer exactly as a rename is: both end in a
        // listing, and the one that lands second would describe a directory the
        // other has already changed.
        if self
            .states
            .get(&session)
            .is_some_and(|state| state.transfer.is_some())
        {
            self.show_notice(session, Notice::Error(ts!("files.transfer_busy")), cx);
            return;
        }

        let path = join(&directory, &name);
        if let Some(state) = self.states.get_mut(&session) {
            state.prompt = None;
        }
        self.focus_prompt = false;
        cx.notify();

        cx.spawn(async move |panel, cx| {
            let outcome = source.mkdir(&path).await;
            panel
                .update(cx, |panel, cx| match outcome {
                    Ok(()) => {
                        if let Some(state) = panel.states.get_mut(&session) {
                            state.keep_notice = true;
                            // Selected before the listing that will contain it:
                            // the selection is held as names and filtered
                            // through the entries, so the new row arrives
                            // already highlighted.
                            state.select_only(&name);
                        }
                        let said = ts!("files.created", name = name.clone());
                        panel.show_notice(session, Notice::Info(said), cx);
                        panel.go(session, source, Target::Exact(directory), cx);
                    }
                    Err(error) => {
                        let notice = Notice::from_error(&error, panel.source_is_local(session));
                        panel.show_notice(session, notice, cx);
                    }
                })
                .ok();
        })
        .detach();
    }
}
