use super::*;

/// A listing entry, for the ordering test.
fn entry(name: &str, is_dir: bool) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        is_dir,
        is_symlink: false,
        size: 0,
    }
}

#[test]
fn directories_sort_before_files_and_case_is_ignored() {
    let mut entries = vec![
        entry("notes.txt", false),
        entry("Zebra", true),
        entry("apple", true),
        entry("Beta.log", false),
    ];
    sort_entries(&mut entries);

    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["apple", "Zebra", "Beta.log", "notes.txt"]);
}

#[test]
fn joining_adds_exactly_one_separator() {
    assert_eq!(join("/home/alice", "notes.txt"), "/home/alice/notes.txt");
    assert_eq!(join("/", ".."), "/..");
    assert_eq!(join("/srv/", "app"), "/srv/app");
}

/// The labels of a breadcrumb row, in the order the header draws them.
fn labels(crumbs: &[Crumb]) -> Vec<&str> {
    crumbs.iter().map(|crumb| crumb.label.as_ref()).collect()
}

/// The directory a piece would list to fill its dropdown, or `None` for the
/// ellipsis, which already knows.
///
/// A root answers with itself: its dropdown is the source's other roots
/// when there are any, and its own subdirectories when there are not.
fn sibling_of(crumb: &Crumb) -> Option<&str> {
    match &crumb.menu {
        CrumbMenu::Root(root) => Some(root.as_str()),
        CrumbMenu::Siblings(directory) => Some(directory.as_str()),
        CrumbMenu::Folded(_) => None,
    }
}

/// Whether a piece is the leading one, which is the only kind whose
/// dropdown may turn out to be a list of roots.
fn is_root_crumb(crumb: &Crumb) -> bool {
    matches!(crumb.menu, CrumbMenu::Root(_))
}

/// The budget the panel gets at the width it opens at, which is the one
/// every fold test below is written against.
fn budget() -> usize {
    fold_budget(DEFAULT_PANEL_WIDTH)
}

#[test]
fn the_root_is_a_single_crumb_listing_itself() {
    let crumbs = crumbs("/", budget());
    assert_eq!(labels(&crumbs), ["/"]);
    // Marked as the root rather than inferred from its label further down:
    // it is the one piece whose dropdown may be a list of roots instead.
    assert!(is_root_crumb(&crumbs[0]));
    // No parent to take siblings from, so the root offers what is inside
    // it: the alternative is a piece that cannot be pressed at all.
    assert_eq!(sibling_of(&crumbs[0]), Some("/"));
    assert_eq!(crumbs[0].path().as_deref(), Some("/"));
}

/// What the root breadcrumb does with the answer, which is the whole of the
/// drive-switching feature: several roots become the menu, and anything
/// less leaves the piece behaving as it did before roots existed.
#[test]
fn only_a_source_with_several_roots_offers_them() {
    let drives =
        root_targets(vec!["C:/".to_owned(), "D:/".to_owned()]).expect("two drives must be a menu");
    let labels: Vec<&str> = drives.iter().map(|target| target.label.as_ref()).collect();
    let paths: Vec<&str> = drives.iter().map(|target| target.path.as_str()).collect();
    // A drive names itself, and the row navigates to the drive's own top —
    // which is only a place at all if the separator survived.
    assert_eq!(labels, ["C:/", "D:/"]);
    assert_eq!(paths, ["C:/", "D:/"]);

    // The order is the source's, and the drive the panel is already on is a
    // row like any other: it is a real move from anywhere below it.
    let many = root_targets(vec!["A:/".to_owned(), "C:/".to_owned(), "Z:/".to_owned()])
        .expect("three drives must be a menu");
    assert_eq!(many.len(), 3);

    // A POSIX source — SFTP, WSL, and unix's own local source — reports the
    // single root, which must leave the piece listing subdirectories.
    assert!(root_targets(vec!["/".to_owned()]).is_none());
    // And a source that could not answer says no less than that one does.
    assert!(root_targets(Vec::new()).is_none());
}

#[test]
fn a_short_path_keeps_every_crumb_and_names_its_parent() {
    let crumbs = crumbs("/srv/app/logs", budget());
    assert_eq!(labels(&crumbs), ["/", "srv", "app", "logs"]);

    let parents: Vec<Option<&str>> = crumbs.iter().map(sibling_of).collect();
    assert_eq!(
        parents,
        [Some("/"), Some("/"), Some("/srv"), Some("/srv/app")]
    );
    // Pressing a row of the leaf's menu must land beside the leaf, not
    // inside it.
    assert_eq!(crumbs[3].path().as_deref(), Some("/srv/app/logs"));
}

/// A local Windows session hands the panel `C:/Users/ada`, whose root is the
/// drive. Reading `C:` as an ordinary piece would hang it off `/` and send
/// every press on it to `/C:`, which names nothing.
#[test]
fn a_drive_path_roots_itself_at_the_drive() {
    let crumbs = crumbs("C:/Users/ada", budget());
    assert_eq!(labels(&crumbs), ["C:/", "Users", "ada"]);

    let parents: Vec<Option<&str>> = crumbs.iter().map(sibling_of).collect();
    assert_eq!(parents, [Some("C:/"), Some("C:/"), Some("C:/Users")]);

    // The drive is the piece that can offer the other drives; `Users`
    // carries the same directory but is an ordinary piece hanging off it.
    assert!(is_root_crumb(&crumbs[0]));
    assert!(!is_root_crumb(&crumbs[1]));

    // Every piece has to be a path the source can be asked for, which the
    // drive root is only if it kept its separator.
    assert_eq!(crumbs[0].path().as_deref(), Some("C:/"));
    assert_eq!(crumbs[1].path().as_deref(), Some("C:/Users"));
    assert_eq!(crumbs[2].path().as_deref(), Some("C:/Users/ada"));

    // The drive alone is a whole row, and the one with no parent above it.
    let alone = super::crumbs("C:/", budget());
    assert_eq!(labels(&alone), ["C:/"]);
    assert_eq!(alone[0].path().as_deref(), Some("C:/"));
    assert!(is_root("C:/") && is_root("/"));
    assert!(!is_root("C:/Users") && !is_root("/srv"));
}

/// A drive root is two characters wider than `/`, so the fold has to spend
/// them: a row folded as if the root cost one would come back over budget.
#[test]
fn a_long_drive_path_folds_around_the_drive() {
    let path = "C:/Users/ada/AppData/Local/Programs/rulogman/releases/today";
    let crumbs = crumbs(path, budget());

    assert_eq!(labels(&crumbs).first(), Some(&"C:/"));
    assert_eq!(labels(&crumbs).get(1), Some(&"\u{2026}"));
    assert_eq!(labels(&crumbs).last(), Some(&"today"));
    assert!(
        crumb_width(&crumbs) <= budget(),
        "the folded row is still {} characters wide",
        crumb_width(&crumbs)
    );

    // The folded pieces stay reachable, and by paths that start at the
    // drive rather than at a root the machine does not have.
    let CrumbMenu::Folded(folded) = &crumbs[1].menu else {
        panic!("the second piece must carry the folded ancestors");
    };
    assert_eq!(
        folded.first().map(|target| target.path.as_str()),
        Some("C:/Users")
    );
    assert!(
        folded.iter().all(|target| target.path.starts_with("C:/")),
        "a folded piece left the drive"
    );
}

/// The drive test is a *shape* test, not a platform one, so the shapes that
/// only look like drives have to stay ordinary pieces — a POSIX path can
/// hold a `:` anywhere, including in its first name.
#[test]
fn only_an_absolute_drive_is_read_as_one() {
    assert_eq!(drive_prefix("C:/Users"), Some("C:"));
    assert_eq!(drive_prefix("c:"), Some("c:"));
    // Relative to that drive's own current directory, which the panel never
    // holds — and would be a path this header could not walk.
    assert_eq!(drive_prefix("C:logs"), None);
    // POSIX paths, one of which begins with a name containing a colon.
    assert_eq!(drive_prefix("/srv/app"), None);
    assert_eq!(drive_prefix("/C:/app"), None);
    assert_eq!(drive_prefix("1:/app"), None);
    assert_eq!(drive_prefix(""), None);
}

/// The budget follows the panel's edge: dragging it wider must never fold
/// *more* of the path away, and the width the panel opens at must still
/// hold the 38 characters the header showed before it could be resized.
#[test]
fn the_budget_grows_with_the_panel_and_never_falls_below_its_floor() {
    let narrow = fold_budget(MIN_PANEL_WIDTH);
    let default = fold_budget(DEFAULT_PANEL_WIDTH);
    let wide = fold_budget(MAX_PANEL_WIDTH);

    assert!(narrow < default, "{narrow} is not narrower than {default}");
    assert!(default < wide, "{default} is not narrower than {wide}");
    assert_eq!(default, 38);

    // The floor holds whatever arrives: a width smaller than the padding
    // itself, and the degenerate values a drag outside the window could
    // otherwise arrive with.
    assert!(narrow >= MIN_PATH_CHARS);
    assert_eq!(fold_budget(0.), MIN_PATH_CHARS);
    assert_eq!(fold_budget(-100.), MIN_PATH_CHARS);
    assert_eq!(fold_budget(f32::NAN), MIN_PATH_CHARS);

    // Even at the floor there is room for the root, the ellipsis and a leaf
    // of a useful length.
    let crumbs = crumbs("/srv/application/logs/today", MIN_PATH_CHARS);
    assert_eq!(labels(&crumbs).first(), Some(&"/"));
    assert_eq!(labels(&crumbs).last(), Some(&"today"));
}

/// The fold: what does not fit goes behind one ellipsis, and stays
/// reachable through the menu that ellipsis carries.
#[test]
fn a_long_path_folds_its_middle_and_keeps_the_leaf() {
    let path = "/srv/application/releases/2026-07-30T12-00/logs/today";
    let crumbs = crumbs(path, budget());

    assert_eq!(labels(&crumbs).first(), Some(&"/"));
    assert_eq!(labels(&crumbs).get(1), Some(&"\u{2026}"));
    assert_eq!(labels(&crumbs).last(), Some(&"today"));
    assert!(
        crumb_width(&crumbs) <= budget(),
        "the folded row is still {} characters wide",
        crumb_width(&crumbs)
    );

    // Every dropped piece is on the ellipsis's menu, in path order, with
    // the absolute path that moves there.
    let CrumbMenu::Folded(folded) = &crumbs[1].menu else {
        panic!("the second piece must carry the folded ancestors");
    };
    let names: Vec<&str> = folded.iter().map(|target| target.label.as_ref()).collect();
    let paths: Vec<&str> = folded.iter().map(|target| target.path.as_str()).collect();
    assert_eq!(names, ["srv", "application", "releases"]);
    assert_eq!(
        paths,
        ["/srv", "/srv/application", "/srv/application/releases"]
    );
}

/// The budget counts characters, not bytes: a Korean directory name is
/// three bytes a letter, and folding on bytes would hide a row that fits on
/// screen with room to spare.
#[test]
fn the_fold_budget_counts_characters_rather_than_bytes() {
    let path = "/사용자문서/보고서모음/분기별매출자료";
    assert!(path.len() > budget(), "the byte length must exceed it");
    assert!(path.chars().count() <= budget(), "but the length must not");

    let crumbs = crumbs(path, budget());
    assert_eq!(
        labels(&crumbs),
        ["/", "사용자문서", "보고서모음", "분기별매출자료"]
    );
    assert_eq!(crumb_width(&crumbs), path.chars().count());
}

/// The narrowest the panel goes still has to say where you are, even when a
/// single name is longer than the whole budget.
#[test]
fn a_leaf_wider_than_the_budget_is_kept_without_an_ellipsis() {
    let deep = crumbs("/srv/a-directory-with-a-very-long-name-indeed", 10);
    assert_eq!(
        labels(&deep),
        ["/", "\u{2026}", "a-directory-with-a-very-long-name-indeed"]
    );

    // Nothing but the root is left to fold when the leaf alone overflows.
    let only = crumbs("/a-directory-with-a-very-long-name-indeed", 10);
    assert_eq!(
        labels(&only),
        ["/", "a-directory-with-a-very-long-name-indeed"]
    );
}

#[test]
fn every_refusal_to_edit_reaches_the_status_line_as_a_failure() {
    // All three are refusals, so none of them may expire on its own the way
    // an `Info` does: the file the user asked for is not open, and only a
    // later success can honestly take that sentence down.
    assert!(matches!(
        edit_notice(&LoadError::TooLarge, false),
        Notice::Error(_)
    ));
    assert!(matches!(
        edit_notice(&LoadError::NotUtf8, false),
        Notice::Error(_)
    ));
    // A transport failure is folded through the same sentence every other
    // panel command uses, so the wording of the failure is preserved whole.
    let transport = LoadError::Transport(FileError::Backend("denied".to_owned()));
    let Notice::Error(said) = edit_notice(&transport, false) else {
        panic!("a failed transfer must not be reported as an aside");
    };
    assert!(said.contains("denied"), "the reason was dropped: {said}");
}

#[test]
fn the_size_cap_is_stated_in_whole_megabytes() {
    // The sentence spells the unit and interpolates a number, so the number
    // has to be one: `10485760 MB` would be a nonsense limit.
    assert_eq!(MAX_EDIT_BYTES % (1024 * 1024), 0);
    assert_eq!(MAX_EDIT_BYTES / 1024 / 1024, 10);
}

#[test]
fn a_percentage_covers_both_ends_and_the_empty_batch() {
    assert_eq!(TransferProgress::percent_of(0, 1000), 0);
    assert_eq!(TransferProgress::percent_of(500, 1000), 50);
    assert_eq!(TransferProgress::percent_of(1000, 1000), 100);
    // Nothing to move is as finished as it will ever be, and must not
    // divide by zero on the way to saying so.
    assert_eq!(TransferProgress::percent_of(0, 0), 100);
    // A file that grew under us must not overflow the bar.
    assert_eq!(TransferProgress::percent_of(2000, 1000), 100);
}

/// All three activities carry two placeholders, on both sides of the
/// wording split, and a translation that dropped one — or a stray `%` the
/// interpolator choked on — would show the raw key text to the user instead
/// of failing anywhere a test could see. The local twins are the reason
/// this loops over both: a missing `files.local.copying` would otherwise
/// only surface on a machine with a local session open.
#[test]
fn the_progress_line_carries_the_name_and_the_percentage() {
    for activity in [Activity::Upload, Activity::Download, Activity::Delete] {
        for is_local in [false, true] {
            let mut progress = TransferProgress::new(activity);
            progress.name = "notes.txt".into();
            progress.percent = 42;

            let line = progress.line(is_local);
            assert!(line.contains("notes.txt"), "saw {line}");
            assert!(line.contains("42"), "saw {line}");
            assert!(!line.contains("%{"), "unreplaced placeholder in {line}");
        }
    }
}

/// The guard that stops an expiring message from taking a later one with
/// it: every message is said at its own epoch, and the timer left behind by
/// the first one no longer matches once the second has been said.
#[test]
fn each_message_is_said_at_its_own_epoch() {
    let mut state = SessionState::new(false);

    let uploaded = state.say(Notice::Info("Uploaded notes.txt.".into()));
    assert_eq!(state.notice_epoch, uploaded);

    let failed = state.say(Notice::Error("could not list /etc".into()));
    assert_ne!(
        uploaded, failed,
        "the timer armed for the first message must not match the second"
    );
    assert_eq!(state.notice_epoch, failed);

    // The failure is what is on screen, so an expiry belonging to the
    // message before it has to be refused rather than clear the line.
    assert!(matches!(state.notice, Some(Notice::Error(_))));
}

/// A name the row has room for must not be explained, or every listing
/// would sprout tooltips nobody asked for.
#[test]
fn a_short_name_needs_no_tooltip() {
    assert!(!name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        "notes.txt",
        false,
        None
    ));
    assert!(!name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        "notes.txt",
        true,
        Some("1.5 MB")
    ));
    // The narrowest the panel goes still holds an ordinary name.
    assert!(!name_is_clipped(MIN_PANEL_WIDTH, "app.log", false, None));
}

/// The case the tooltip exists for: a name the row has to cut.
#[test]
fn a_name_too_long_for_the_row_is_explained() {
    let long = "2026-07-30T12-00-00-application-server.log";
    assert!(name_is_clipped(DEFAULT_PANEL_WIDTH, long, false, None));
    assert!(name_is_clipped(MIN_PANEL_WIDTH, long, false, None));
    // Even at its widest the panel is a sidebar, not a window.
    assert!(name_is_clipped(
        MAX_PANEL_WIDTH,
        &"x".repeat(200),
        false,
        None
    ));
}

/// The budget either side of the cut. Named columns rather than characters
/// because that is what the estimate counts.
#[test]
fn the_tooltip_appears_one_column_past_the_budget() {
    // 260 − 2 border − 16 padding − 14 icon − 6 gap = 222px ÷ 7 = 31.
    let budget = 31;
    assert!(!name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        &"a".repeat(budget),
        false,
        None
    ));
    assert!(name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        &"a".repeat(budget + 1),
        false,
        None
    ));
}

/// A Hangul name takes two columns per character, so counting `chars` would
/// let it run to twice the width before anyone called it long — which is
/// exactly the name most likely to be cut off.
#[test]
fn a_wide_name_is_measured_in_columns_not_characters() {
    let hangul = "가".repeat(16);
    let latin = "a".repeat(16);
    assert_eq!(hangul.chars().count(), latin.chars().count());

    assert!(name_is_clipped(DEFAULT_PANEL_WIDTH, &hangul, false, None));
    assert!(!name_is_clipped(DEFAULT_PANEL_WIDTH, &latin, false, None));
}

/// Everything drawn beside the name takes room from it, so the same name
/// can fit on a directory row and not on a symlinked file's.
#[test]
fn the_badge_and_the_size_column_shrink_the_budget() {
    let name = "a".repeat(26);

    // A directory: no size column, no badge, so the name has the row.
    assert!(!name_is_clipped(DEFAULT_PANEL_WIDTH, &name, false, None));
    // The same name on a file, once the size column is beside it.
    assert!(name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        &name,
        false,
        Some("1.5 MB")
    ));

    // And the badge takes a little more still: a name that survives the
    // size column alone can lose to the two together.
    let borderline = "a".repeat(23);
    assert!(!name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        &borderline,
        false,
        Some("1.5 MB")
    ));
    assert!(name_is_clipped(
        DEFAULT_PANEL_WIDTH,
        &borderline,
        true,
        Some("1.5 MB")
    ));
}

/// A width that leaves no room at all must not divide its way to "it fits".
#[test]
fn a_row_with_no_room_clips_anything_but_an_empty_name() {
    assert!(name_is_clipped(0., "a", false, None));
    assert!(!name_is_clipped(0., "", false, None));
    assert!(name_is_clipped(f32::NAN, "a", false, None));
}

/// The rule that keeps a delete inside the directory it was asked about.
/// A link to a directory looks exactly like a directory in the listing —
/// that is deliberate, so it can be opened — and getting this wrong would
/// empty the target instead of removing the link.
#[test]
fn a_delete_walks_into_real_directories_only() {
    let mut link = entry("shortcut", true);
    link.is_symlink = true;
    assert!(!needs_walking(&link));

    let mut broken = entry("dangling", false);
    broken.is_symlink = true;
    assert!(!needs_walking(&broken));

    assert!(needs_walking(&entry("logs", true)));
    assert!(!needs_walking(&entry("notes.txt", false)));
}

#[test]
fn a_name_that_could_escape_the_destination_is_refused() {
    assert!(is_plain_name("notes.txt"));
    assert!(is_plain_name("a file with spaces"));
    assert!(!is_plain_name(""));
    assert!(!is_plain_name("."));
    assert!(!is_plain_name(".."));
    assert!(!is_plain_name("etc/passwd"));
    assert!(!is_plain_name("..\\windows"));
}

#[test]
fn an_upload_plan_lists_parents_before_children() {
    let root = tempfile::tempdir().expect("creating the local tree must succeed");
    std::fs::create_dir_all(root.path().join("logs/old")).expect("nested dirs must be created");
    std::fs::write(root.path().join("logs/app.log"), b"line\n").expect("a file must be written");
    std::fs::write(root.path().join("logs/old/app.log"), b"older\n")
        .expect("a nested file must be written");

    let plan = plan_upload(vec![root.path().join("logs")], "/srv".to_owned());

    let base = format!("/srv/{}", "logs");
    assert_eq!(plan.directories, [base.clone(), format!("{base}/old")]);
    assert_eq!(plan.files.len(), 2);
    assert_eq!(plan.total, b"line\n".len() as u64 + b"older\n".len() as u64);
    // Every file must land in a directory the plan also creates, or the
    // upload would run `create` inside a directory that is not there yet.
    for file in &plan.files {
        assert!(
            plan.directories.contains(&file.directory),
            "{} has no directory in the plan",
            file.local.display()
        );
    }
}

/// The cycle guard: a link back to an ancestor must not be walked, or the
/// plan would grow until the process ran out of memory.
#[cfg(unix)]
#[test]
fn an_upload_plan_does_not_follow_a_symlinked_directory() {
    let root = tempfile::tempdir().expect("creating the local tree must succeed");
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).expect("the directory must be created");
    std::fs::write(tree.join("note.txt"), b"hi\n").expect("a file must be written");
    std::os::unix::fs::symlink(&tree, tree.join("loop")).expect("the symlink must be created");
    std::os::unix::fs::symlink(tree.join("note.txt"), tree.join("alias"))
        .expect("the file symlink must be created");

    let plan = plan_upload(vec![tree], "/srv".to_owned());

    assert_eq!(plan.directories, ["/srv/tree"]);
    let mut names: Vec<String> = plan
        .files
        .iter()
        .map(|file| file_name(&file.local).to_string())
        .collect();
    names.sort();
    // The link to a directory is gone; the link to a file is sent as its
    // target, which is why its size counts twice in the total.
    assert_eq!(names, ["alias", "note.txt"]);
    assert_eq!(plan.total, 2 * b"hi\n".len() as u64);
}

#[test]
fn sizes_read_the_way_a_file_manager_writes_them() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1023), "1023 B");
    assert_eq!(format_size(1024), "1.0 KB");
    assert_eq!(format_size(1536), "1.5 KB");
    assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
}
