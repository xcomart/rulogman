//! The update check, and the self-update it now leads to.
//!
//! Two halves that share one HTTPS client and one notion of what a release is.
//!
//! The **check** is one request against the project's `releases/latest`
//! endpoint. It runs once per launch from the background executor, and it runs
//! again — with a different filter — whenever the user picks "Check for
//! updates" from the menu. Its whole visible outcome is the
//! [`UpdateDialog`][crate::update_dialog::UpdateDialog] appearing.
//!
//! The **install** is what "Update" now does: fetch the release asset built for
//! this exact target triple, verify it against what the API said it should be,
//! unpack it beside the installed copy, and move the new one into the old one's
//! place. The application then restarts itself into the binary it just wrote.
//!
//! # Why the start-up check fails silently
//!
//! A terminal is opened to get work done, and an update check is the least
//! important thing happening at start-up. Every way it can go wrong — no
//! network, a captive portal answering HTML, GitHub rate-limiting the address,
//! a tag someone pushed by hand in a shape the parser does not recognise — has
//! the same correct response: say nothing and carry on. So [`check`] ends every
//! failure path in a `log::debug!` and a `None`.
//!
//! A *manual* check is the opposite: the user asked a question and is owed an
//! answer, including "I could not reach GitHub". That is why [`check_now`]
//! answers with a three-way [`Check`] instead, and why the manual path also
//! ignores the "never mention this version again" tag — the user has just
//! overruled it by asking.
//!
//! # Why `ureq` and not gpui's HTTP client
//!
//! `cx.http_client()` is a `NullHttpClient` unless the application installs a
//! real one, and this binary does not: nothing else it does speaks HTTP. A
//! small blocking client called from a background task is a far smaller thing
//! to carry than an async HTTP stack installed for two kinds of request.
//!
//! # Why a successful update also writes one registry value
//!
//! Windows ships twice. The zip is what this module downloads; beside it goes
//! an Inno Setup installer, which exists so the Windows Package Manager has
//! something it can install and account for. What the installer adds is not
//! files — it lays down the same single executable — but an entry under *Apps &
//! features*, and winget reads that entry's `DisplayVersion` to decide which
//! version is present and whether an upgrade is available. The updater replaces
//! the executable and would otherwise know nothing about it, so an installed
//! copy that updated itself would leave winget convinced the old release was
//! still there: `winget list` reporting a version that has not been on disk for
//! months, and `winget upgrade` offering — and then pointlessly reinstalling —
//! a release already applied. One value, written once per update, is the whole
//! fix.
//!
//! [`sync_arp_version`] is written to be a no-op everywhere it is not wanted,
//! and the two things it refuses to do are the interesting ones.
//!
//! **It never creates the key.** A copy unpacked from the portable archive has
//! no entry, is not an installed program, and inventing one would put rulogman
//! in a list whose only offered action — uninstall — would run an uninstaller
//! that is not there.
//!
//! **It writes only to an entry that describes *this* copy.** The two
//! distributions can sit on one machine at once: an installed copy under
//! `%LOCALAPPDATA%\Programs\rulogman` and a zip unpacked wherever the user
//! keeps it. If the portable copy updated itself and bumped the installed
//! copy's recorded version, the installed copy would drop out of winget's
//! upgrade list while its executable stayed at the old release — a worse
//! failure than the one being fixed, because nothing afterwards corrects it. So
//! the entry's `InstallLocation` is compared against the directory this
//! executable is actually running from, and a mismatch means the entry belongs
//! to someone else and is left alone.
//!
//! # What the install deliberately does not do
//!
//! No package manager is consulted, no installer is run, nothing is elevated,
//! and the only thing written outside the directory rulogman is already
//! installed in is the single `DisplayVersion` value above — a correction to a
//! record of that same directory, not a claim on anything else. A copy the user
//! cannot overwrite — a system package, a read-only mount, a `.app` opened from
//! a disk image — fails the rename and lands in the dialog's error state, whose
//! one action is the browser fallback this module used to be limited to. That
//! is the honest outcome: an updater that starts asking for administrator
//! rights is a different program.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use gpui::App;
use rulogman_core::AppSettings;

use crate::app_settings;

/// Version of the running binary, taken from its `Cargo.toml`.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub API endpoint answering with the most recent non-draft,
/// non-prerelease release of the project.
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/xcomart/rulogman/releases/latest";

/// Where "Update" goes when the API answered without an `html_url`.
///
/// The releases index rather than the project page: whatever the user came here
/// for, it is a download.
const RELEASES_PAGE: &str = "https://github.com/xcomart/rulogman/releases";

/// How long the whole *check* may take, connection included.
///
/// Short on purpose. Nothing waits on this — the window is already up — but a
/// background task blocked for minutes on a black-holed connection is a thread
/// of the executor pool held hostage for no possible benefit, and an answer
/// that arrives long after start-up would open a dialog over whatever the user
/// had started doing in the meantime.
///
/// Emphatically *not* reused for the download: see [`CONNECT_TIMEOUT`].
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long the *download* may take to reach the server.
///
/// A global timeout is wrong for a download — a release archive on a slow line
/// legitimately takes minutes, and killing it at any fixed deadline would make
/// the updater useless exactly where it is most wanted. What can still be
/// bounded is the handshake, so an unreachable host fails quickly instead of
/// leaving the dialog spinning at 0%.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on what the download will write to disk.
///
/// The size the API reported is checked afterwards, but only a reader that
/// stops can do the checking; without a limit a server answering an endless
/// body would fill the volume first. Two orders of magnitude above any release
/// this project has published, so it can only ever catch a fault.
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

/// Copy buffer for the download.
const DOWNLOAD_BUFFER: usize = 64 * 1024;

/// How many bytes must land before the download reports progress again.
///
/// The read loop turns over hundreds of times a second; a report per turn would
/// wake the UI thread for a bar that has not moved a pixel.
const PROGRESS_STEP: u64 = 256 * 1024;

/// Name of the scratch directory the download and the unpacking happen in.
///
/// Created *beside the installed copy* rather than in the system temp
/// directory, and that placement is load-bearing: the last step of an install
/// is a `fs::rename` of the unpacked payload onto the installed one, and a
/// rename cannot cross a volume. Staging in `%TEMP%` or `/tmp` would work on
/// most machines and fail with `EXDEV` on exactly the ones where the
/// application lives on another disk.
const STAGING_DIR: &str = ".update";

/// Where the unpacked archive goes inside [`STAGING_DIR`].
const UNPACKED_DIR: &str = "unpacked";

/// Suffix the replaced copy is renamed to.
///
/// Windows will not let a running executable be deleted, but it will let it be
/// renamed, which is what makes an in-place swap possible at all. The leftover
/// is removed by [`clean_leftovers`] on the next launch — one code path for all
/// three platforms, rather than an immediate unlink on unix and a deferred one
/// on Windows.
const OLD_SUFFIX: &str = ".old";

/// Fallback name for the downloaded archive.
///
/// Used when the asset name from the API is not a plain file name. It never is
/// in practice; the guard exists so a hostile response cannot steer the write
/// out of the staging directory.
const FALLBACK_ARCHIVE: &str = "rulogman-update";

/// The "Apps & features" entry the Windows installer leaves behind, relative to
/// `HKEY_CURRENT_USER` or `HKEY_LOCAL_MACHINE`.
///
/// The GUID in the middle of it is one corner of a triangle that has to agree,
/// and it is a published identifier rather than an implementation detail:
///
/// * `packaging/windows/rulogman.iss` sets it as Inno Setup's `AppId`, and Inno
///   derives this key's name from it by appending `_is1`;
/// * the manifests under `packaging/winget/*/` record the same string, braces
///   and suffix included, as the package's `ProductCode`;
/// * and this constant is how [`sync_arp_version`] finds the entry again.
///
/// Move any one corner without the other two and winget stops recognising an
/// installed rulogman: `winget list` finds nothing, `winget upgrade` offers a
/// fresh install to sit beside the existing one, and `winget uninstall` has
/// nothing to remove — all silently, because a key that is not there is
/// indistinguishable from a copy that was never installed. None of the three
/// ever changes; see the README in `packaging/winget/`.
#[cfg(any(windows, test))]
const ARP_KEY: &str = concat!(
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\",
    "{D6066CD8-5F5D-4B13-AB5B-DAD7965FF725}_is1"
);

/// The value inside [`ARP_KEY`] that winget reads as the installed version.
#[cfg(windows)]
const DISPLAY_VERSION: &str = "DisplayVersion";

/// The value inside [`ARP_KEY`] naming the directory the entry describes.
#[cfg(windows)]
const INSTALL_LOCATION: &str = "InstallLocation";

/// The release-asset target triple for the platform this binary was built for,
/// or `None` where the project publishes no build.
///
/// The three arms are exactly the three jobs of `.github/workflows/release.yml`.
/// An Intel Mac or an ARM Linux box runs a locally built rulogman, and there is
/// nothing to hand it: those fall through to `None`, which makes "Update" open
/// the release page the way it always did.
const TARGET: Option<&str> = if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    Some("x86_64-pc-windows-msvc")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else {
    None
};

/// What the archive holds that has to end up on disk.
///
/// The Windows and Linux archives carry the bare executable; the macOS one
/// carries the whole application bundle, which is also what gets replaced.
#[cfg(windows)]
const PAYLOAD: &str = "rulogman.exe";
/// See the Windows variant above.
#[cfg(target_os = "macos")]
const PAYLOAD: &str = "rulogman.app";
/// See the Windows variant above.
#[cfg(all(unix, not(target_os = "macos")))]
const PAYLOAD: &str = "rulogman";

/// A downloadable build of a release, matched to this target triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// File name as published, e.g. `rulogman-v0.4.0-x86_64-pc-windows-msvc.zip`.
    pub name: String,
    /// Direct download URL. Answers a redirect to storage, which `ureq`
    /// follows.
    pub url: String,
    /// Size in bytes, as the API reported it. Checked against what actually
    /// arrived, and used to drive the progress bar.
    pub size: u64,
    /// Lower-case hex SHA-256 of the asset, when the API supplied one.
    ///
    /// `digest` is a recent addition to the releases API, so an older GitHub
    /// Enterprise or a cached response may omit it; the size check still
    /// applies in that case.
    pub digest: Option<String>,
}

/// A release worth telling the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The git tag GitHub published it under, e.g. `"v0.4.0"`.
    ///
    /// Kept verbatim rather than normalised, because this is also what gets
    /// written to `settings.json` when the user ignores the version, and the two
    /// are compared as strings.
    pub tag: String,
    /// Human-readable version for display: [`Release::tag`] without its `v`.
    pub version: String,
    /// The release page to open in the browser.
    pub url: String,
    /// The build for this platform, when the release published one.
    ///
    /// `None` on a target the project does not ship — and on any release whose
    /// assets do not include the expected name — which is what decides whether
    /// "Update" installs or hands off to the browser.
    pub asset: Option<Asset>,
}

/// The answer to a check the user asked for.
///
/// Distinguishes the two outcomes the start-up check collapses into `None`:
/// "there is nothing newer" is a satisfying answer to a question, and "GitHub
/// could not be reached" is not the same thing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// A newer release exists.
    Newer(Release),
    /// The running build is the latest one published.
    UpToDate,
    /// The check itself did not complete. Carries a short technical detail —
    /// untranslated on purpose, see [`install`].
    Failed(String),
}

/// How far an [`install`] has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// `done` of `total` bytes have been written to the staging directory.
    Downloading {
        /// Bytes received so far.
        done: u64,
        /// Bytes the API said the asset has. Zero when it said nothing.
        total: u64,
    },
    /// The download is complete; the archive is being unpacked and swapped in.
    Installing,
}

/// Ask GitHub whether a newer release exists, blocking until it answers.
///
/// **Call this from the background executor.** It performs a network request
/// and will block the calling thread for up to [`TIMEOUT`].
///
/// This is the *manual* check: it reports every outcome, and it knows nothing
/// about the ignore list. [`check`] is the start-up wrapper around it.
pub fn check_now() -> Check {
    let body = match fetch_latest() {
        Ok(body) => body,
        Err(err) => {
            log::debug!("update check: {err}");
            return Check::Failed(err.to_string());
        }
    };

    let Some(release) = parse_release(&body) else {
        return Check::Failed("GitHub answered with no readable release".to_string());
    };

    if is_newer(&release.tag, CURRENT_VERSION) {
        Check::Newer(release)
    } else {
        log::debug!(
            "update check: {} is not newer than {CURRENT_VERSION}",
            release.tag
        );
        Check::UpToDate
    }
}

/// The start-up check: answers `Some` only when there is something to say.
///
/// Answers `Some` only when all of the following hold, and `None` — silently —
/// otherwise:
///
/// * the request succeeded and the body parsed;
/// * the tag names a version strictly newer than the running one;
/// * that tag is not the one stored in `ignored`.
///
/// `ignored` is passed in rather than read from the settings global because the
/// global is only reachable from the UI thread, and this function runs off it.
pub fn check(ignored: Option<&str>) -> Option<Release> {
    match check_now() {
        Check::Newer(release) if ignored == Some(release.tag.as_str()) => {
            log::debug!("update check: {} is available but ignored", release.tag);
            None
        }
        Check::Newer(release) => Some(release),
        Check::UpToDate | Check::Failed(_) => None,
    }
}

/// The release page of `release`, or the releases index when it named none.
pub fn release_url(release: &Release) -> &str {
    if release.url.is_empty() {
        RELEASES_PAGE
    } else {
        &release.url
    }
}

/// Persist `tag` as the version the user never wants to hear about again.
///
/// Mirrors what the settings dialog does on save: replace the global, then write
/// the file. A failed write is logged and otherwise ignored — the tag still
/// applies for the rest of this run, and the worst case is that the same dialog
/// appears once more on the next launch, which is not worth an error message
/// over.
pub fn remember_ignored(tag: &str, cx: &mut App) {
    let mut settings: AppSettings = app_settings::current(cx);
    settings.ignored_update = Some(tag.to_string());
    if let Err(err) = settings.save() {
        log::warn!("could not record the ignored update {tag}: {err:#}");
    }
    app_settings::replace(settings, cx);
}

/// Remove what a previous update left behind, if anything.
///
/// **Call this from the background executor**, early in the run: removing a
/// `.app` bundle is a recursive delete, and nothing on screen depends on it.
///
/// The swap cannot delete the copy it replaces — on Windows because the file is
/// the running process, on the others because there is no reason to make the
/// three platforms differ — so it renames it aside and leaves it for the next
/// launch. That is here. Every failure is a debug line: a leftover costs disk
/// space and nothing else, and the next update will try again.
pub fn clean_leftovers() {
    let Ok(target) = install_target() else {
        return;
    };
    let Some(retired) = old_path(&target) else {
        return;
    };
    if !retired.exists() {
        return;
    }
    match remove(&retired) {
        Ok(()) => log::debug!("removed the previous version at {}", retired.display()),
        Err(error) => log::debug!(
            "could not remove the previous version at {}: {error}",
            retired.display()
        ),
    }
}

/// Download `release`, unpack it, and put it where the running copy is.
///
/// **Call this from the background executor.** It downloads tens of megabytes,
/// spawns `tar`, and renames files; none of that belongs on the UI thread.
/// `report` is called as the work proceeds, from this thread.
///
/// Returns on success only once the new build is fully in place, so the caller
/// may restart into it immediately. The `Ok` value is the path to restart
/// *from* — the executable, or on macOS the bundle — and it has to be passed
/// to the restart explicitly rather than looked up again afterwards: on Linux
/// `current_exe()` reads `/proc/self/exe`, which follows the inode and not
/// the name, so once [`swap`] has renamed the running copy aside it answers
/// `rulogman.old`, and a restart from that path relaunches the version that
/// was just replaced. This path is captured before the rename. On failure the
/// staging directory is gone, the installed copy is untouched, and the `Err`
/// carries a sentence for the dialog to show under its translated "the update
/// failed" heading.
///
/// # Why the error text is not translated
///
/// It is a technical detail — a `tar` message, an OS error, a byte count that
/// did not match — produced on a thread that has no business reaching into the
/// locale state, and shown beneath a heading that *is* translated. Translating
/// the detail would mean a key per failure mode and a per-locale copy of every
/// `io::Error` string, which is not what any of them say anyway.
pub fn install(release: &Release, report: &mut dyn FnMut(Progress)) -> Result<PathBuf, String> {
    let Some(asset) = release.asset.as_ref() else {
        return Err(format!(
            "{} publishes no build for this platform",
            release.tag
        ));
    };

    let target = install_target()?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;

    let staging = parent.join(STAGING_DIR);
    // A staging directory left by an interrupted run would otherwise poison
    // this one with a half-written archive under the same name.
    let _ = remove(&staging);
    fs::create_dir_all(&staging)
        .map_err(|error| format!("could not write to {}: {error}", parent.display()))?;

    let outcome = stage(asset, &target, &staging, report);
    // Best-effort on purpose: the update either happened or it did not, and a
    // scratch directory that outlives it is not worth turning a success into a
    // failure over. The next install removes it anyway.
    let _ = remove(&staging);

    // The first point at which the new build is certainly on disk, and the last
    // one that still knows *which* release it is — the two facts that settle
    // where the value is written from. See the notes on `sync_arp_version`.
    #[cfg(windows)]
    if outcome.is_ok() {
        sync_arp_version(parent, &release.version);
    }

    outcome.map(|()| target)
}

/// The download-verify-unpack-swap sequence, with `staging` already prepared.
///
/// Split out from [`install`] purely so the staging directory has exactly one
/// removal site covering every way out of it.
fn stage(
    asset: &Asset,
    target: &Path,
    staging: &Path,
    report: &mut dyn FnMut(Progress),
) -> Result<(), String> {
    let archive = staging.join(archive_name(&asset.name));
    download(asset, &archive, report)?;

    report(Progress::Installing);

    let unpacked = staging.join(UNPACKED_DIR);
    fs::create_dir_all(&unpacked)
        .map_err(|error| format!("could not create {}: {error}", unpacked.display()))?;
    extract(&archive, &unpacked)?;

    let payload = find_payload(&unpacked, PAYLOAD)
        .ok_or_else(|| format!("{} does not contain {PAYLOAD}", asset.name))?;

    swap(target, &payload)?;

    // With the new bundle in place and the restart imminent, this is the last
    // moment to make sure Gatekeeper will let it open.
    #[cfg(target_os = "macos")]
    clear_quarantine(target);

    Ok(())
}

/// Strip the quarantine flag from the bundle just swapped in, best-effort.
///
/// A file this process downloads and unpacks should carry no quarantine of its
/// own — rulogman is not quarantine-aware, and `tar` restores none from the
/// CI-built archive — but Gatekeeper's rules have tightened release by release,
/// and the one unacceptable outcome here is an update that leaves the user with
/// an app macOS refuses to reopen. So the flag is cleared unconditionally: this
/// is the same `xattr -r -d com.apple.quarantine` the README walks a first-time
/// installer through, recursive because the flag lands on every file inside a
/// quarantined bundle, and best-effort because the attribute is usually not
/// there at all — a failure costs a debug line, never the update.
#[cfg(target_os = "macos")]
fn clear_quarantine(bundle: &Path) {
    match Command::new("xattr")
        .args(["-r", "-d", "com.apple.quarantine"])
        .arg(bundle)
        .output()
    {
        Ok(output) if output.status.success() => {}
        // The usual answer on a clean bundle: "No such xattr". Worth a debug
        // line and nothing more.
        Ok(output) => log::debug!(
            "xattr -r -d com.apple.quarantine {} exited with {}: {}",
            bundle.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => log::debug!("xattr could not be run: {error}"),
    }
}

/// Tell the "Apps & features" entry for this installation that `version` is now
/// what is on disk.
///
/// `installed_at` is the directory the running executable lives in — the same
/// one the swap has just written into. Called once from [`install`], on its one
/// success; see the module docs for what the value is for and why an entry
/// describing some other directory is left alone.
///
/// Answers nothing, and fails at nothing. Every way this can go wrong — no
/// entry, an entry for a different copy, a machine-wide entry this unelevated
/// process may read but not write — ends in a `log::debug!` and a return,
/// because by the time it runs the update itself has already succeeded. An
/// updater that reported failure over a registry value winget reads would be
/// telling the user their update did not happen, which is both wrong and
/// unactionable.
///
/// `HKEY_CURRENT_USER` is tried first because that is where the installer's
/// `PrivilegesRequired=lowest` puts the entry, and `HKEY_LOCAL_MACHINE` after
/// it, for the copy someone installed by running the setup elevated. Both are
/// tried even when the first one exists: a machine can carry one of each, and
/// only one of them can be the copy running this code.
#[cfg(windows)]
fn sync_arp_version(installed_at: &Path, version: &str) {
    let roots = [
        ("HKCU", windows_registry::CURRENT_USER),
        ("HKLM", windows_registry::LOCAL_MACHINE),
    ];
    for (name, root) in roots {
        if write_display_version(root, name, ARP_KEY, installed_at, version) {
            return;
        }
    }
}

/// The body of [`sync_arp_version`], for one registry root and one key path.
///
/// Split out with the key path as an argument for one reason: the real key is a
/// live part of the machine's installed-program list, and a test that wrote to
/// it would be editing the user's *Apps & features*. Everything with a decision
/// in it is therefore here, reachable with a scratch key under
/// `HKCU\Software`, and [`sync_arp_version`] is the four lines that supply the
/// constants.
///
/// `name` is the root's short name, for the log lines and nothing else. Answers
/// whether the value was written, which is how the caller knows to stop.
#[cfg(windows)]
fn write_display_version(
    root: &windows_registry::Key,
    name: &str,
    key_path: &str,
    installed_at: &Path,
    version: &str,
) -> bool {
    // Opened for writing up front, so a machine-wide entry an unelevated
    // process cannot touch fails here rather than after the comparison. The
    // overwhelmingly common outcome is the key simply not being there, which is
    // what a copy unpacked from the zip looks like.
    let key = match root.options().read().write().open(key_path) {
        Ok(key) => key,
        Err(error) => {
            log::debug!("{name}\\{key_path} is not open for writing: {error}");
            return false;
        }
    };

    let recorded = match key.get_string(INSTALL_LOCATION) {
        Ok(recorded) => recorded,
        Err(error) => {
            // An entry with no `InstallLocation` describes no directory, so
            // there is nothing to match it against and no basis for claiming it.
            log::debug!("{name}\\{key_path} records no {INSTALL_LOCATION}: {error}");
            return false;
        }
    };

    if !same_directory(Path::new(recorded.trim()), installed_at) {
        log::debug!(
            "{name}\\{key_path} describes {recorded}, not {}; its version is not this copy's to \
             change",
            installed_at.display()
        );
        return false;
    }

    match key.set_string(DISPLAY_VERSION, version) {
        Ok(()) => {
            log::debug!("{name}\\{key_path} now reports {version} as the installed version");
            true
        }
        Err(error) => {
            log::debug!("could not write {DISPLAY_VERSION} to {name}\\{key_path}: {error}");
            false
        }
    }
}

/// Whether two paths name the same directory.
///
/// Both sides go through `canonicalize` rather than being compared as text,
/// because they are written by different programs and agree on nothing else:
/// Inno Setup stores `InstallLocation` with a trailing backslash, the two may
/// differ in case on a filesystem that does not care, and either may run
/// through a junction or a substituted drive. Canonicalising resolves all of
/// that to the one form the operating system itself would.
///
/// A path that does not exist cannot be canonicalised, and the answer there is
/// `false`: a recorded install location pointing at a directory that is gone
/// describes some other, broken installation, and is emphatically not this one.
#[cfg(any(windows, test))]
fn same_directory(one: &Path, other: &Path) -> bool {
    match (fs::canonicalize(one), fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        _ => false,
    }
}

/// Stream `asset` into `to`, checking it against what the API promised.
///
/// Uses an agent of its own rather than the check's: that one carries a global
/// five-second deadline, which would abort a release download on any connection
/// slower than a datacentre's. Here only the connect phase is bounded.
fn download(asset: &Asset, to: &Path, report: &mut dyn FnMut(Progress)) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .build()
        .into();

    let mut response = agent
        .get(&asset.url)
        .header("User-Agent", format!("rulogman/{CURRENT_VERSION}"))
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|error| format!("could not download {}: {error}", asset.name))?;

    let mut body = response
        .body_mut()
        .with_config()
        .limit(MAX_ASSET_BYTES)
        .reader();

    let mut file =
        File::create(to).map_err(|error| format!("could not create {}: {error}", to.display()))?;

    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; DOWNLOAD_BUFFER];
    let mut done = 0u64;
    let mut reported = 0u64;

    loop {
        let read = body
            .read(&mut buffer)
            .map_err(|error| format!("could not download {}: {error}", asset.name))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        digest.update(chunk);
        file.write_all(chunk)
            .map_err(|error| format!("could not write {}: {error}", to.display()))?;
        done = done.saturating_add(read as u64);
        if done - reported >= PROGRESS_STEP {
            reported = done;
            report(Progress::Downloading {
                done,
                total: asset.size,
            });
        }
    }

    file.flush()
        .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    drop(file);

    report(Progress::Downloading {
        done,
        total: asset.size,
    });

    if asset.size != 0 && done != asset.size {
        return Err(format!(
            "{} is {done} bytes, but the release says {}",
            asset.name, asset.size
        ));
    }

    if let Some(expected) = &asset.digest {
        let actual = hex(digest.finish().as_ref());
        if &actual != expected {
            return Err(format!(
                "{} does not match its published checksum",
                asset.name
            ));
        }
    }

    Ok(())
}

/// Unpack `archive` into `into` using the system `tar`.
///
/// One extractor for three archive formats and three platforms, and no new
/// dependency: `tar` on macOS and Linux is bsdtar or GNU tar, both of which
/// autodetect gzip, and Windows has shipped bsdtar as `System32\tar.exe` since
/// 1803 — which also reads the `.zip` the Windows release is published as,
/// because libarchive sniffs the container rather than trusting the extension.
///
/// `CREATE_NO_WINDOW` on Windows for the reason the WSL probe uses it: a GUI
/// process starting a console program flashes a black rectangle on screen
/// otherwise, and here it would flash over a progress dialog.
fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let mut command = Command::new("tar");
    command.arg("-xf").arg(archive).arg("-C").arg(into);
    #[cfg(windows)]
    command.creation_flags(crate::wsl::CREATE_NO_WINDOW);

    let output = command
        .output()
        .map_err(|error| format!("could not run tar: {error}"))?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("tar could not unpack {}", archive.display())
        } else {
            format!("tar could not unpack {}: {detail}", archive.display())
        });
    }

    Ok(())
}

/// Move `replacement` onto `target`, keeping the displaced copy aside.
///
/// Two renames, in the only order that leaves a working installation at every
/// intermediate point: the running copy is renamed out of the way first — the
/// step Windows permits for a running image but a delete would not — and the
/// new one takes the freed name. If the second rename fails the first is undone,
/// so a failure here restores exactly what was there.
fn swap(target: &Path, replacement: &Path) -> Result<(), String> {
    let retired =
        old_path(target).ok_or_else(|| format!("{} has no file name", target.display()))?;

    // A leftover from a previous update that start-up could not remove would
    // make the rename below fail on Windows, where renaming onto an existing
    // name is an error.
    let _ = remove(&retired);

    fs::rename(target, &retired)
        .map_err(|error| format!("could not move {} aside: {error}", target.display()))?;

    if let Err(error) = fs::rename(replacement, target) {
        let restored = fs::rename(&retired, target);
        return Err(match restored {
            Ok(()) => format!("could not install the new version: {error}"),
            // Both renames failed, which means the installed copy is sitting
            // under its `.old` name with nothing in its place. Say so: the
            // browser fallback is the only way out, and the user needs to know
            // the directory is in an odd state.
            Err(second) => format!(
                "could not install the new version: {error}; \
                 the previous one is now at {} ({second})",
                retired.display()
            ),
        });
    }

    Ok(())
}

/// What this run of rulogman would replace: the executable, or on macOS the
/// bundle containing it.
///
/// The macOS arm is the one that can refuse. A `cargo run` build, or a bare
/// binary someone copied out of a bundle, has no `.app` to swap and no sensible
/// thing to do with an archive that contains one, so it reports that rather than
/// scattering a bundle into whatever directory it happens to sit in.
fn install_target() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate the running program: {error}"))?;

    #[cfg(target_os = "macos")]
    {
        bundle_root(&exe)
            .ok_or_else(|| "rulogman is not running from an application bundle".to_string())
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(exe)
    }
}

/// The `.app` directory `exe` lives inside, if any.
///
/// `current_exe()` in a bundle is `<name>.app/Contents/MacOS/rulogman`, but the
/// depth is not worth relying on: the ancestor chain is walked until a component
/// wears the `app` extension.
#[cfg(any(target_os = "macos", test))]
fn bundle_root(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .map(Path::to_path_buf)
}

/// `path` with [`OLD_SUFFIX`] appended to its file name.
///
/// Appended to the whole name rather than swapped for the extension, so
/// `rulogman.exe` becomes `rulogman.exe.old` and not `rulogman.old`: the second would
/// collide with a directory listing's idea of a different program, and on
/// Windows it would stop being an executable.
fn old_path(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(OLD_SUFFIX);
    Some(path.with_file_name(name))
}

/// Where inside the unpacked archive the payload actually is.
///
/// Every published archive wraps its contents in one directory named after the
/// asset, so the payload is one level down — but an archive that ever stops
/// doing that should still install, hence the direct hit is tried first and the
/// immediate subdirectories after it. Nothing deeper: a match further down would
/// be a different file that happens to share the name.
fn find_payload(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(name);
    if direct.exists() {
        return Some(direct);
    }

    let mut found: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(name))
        .filter(|candidate| candidate.exists())
        .collect();
    // Sorted so a two-directory archive picks the same one on every filesystem,
    // rather than whatever order the directory happened to be read in.
    found.sort();
    found.into_iter().next()
}

/// A file name for the downloaded archive that cannot escape the staging
/// directory.
///
/// The published names are plain, so this returns them unchanged; a name
/// carrying a separator — which only a compromised or confused API could send —
/// is replaced wholesale rather than sanitised, because there is no correct
/// guess at what it was meant to be.
fn archive_name(asset: &str) -> &str {
    let plain = !asset.is_empty()
        && asset != "."
        && asset != ".."
        && !asset.contains('/')
        && !asset.contains('\\');
    if plain { asset } else { FALLBACK_ARCHIVE }
}

/// Delete `path`, whichever kind of thing it is.
fn remove(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

/// Lower-case hex, for comparing against the API's `sha256:` field.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Fetch the raw JSON body of the latest-release endpoint.
///
/// The `User-Agent` is not optional politeness: the GitHub API rejects requests
/// without one. `Accept` pins the response to the current API media type so a
/// future default cannot silently change the field names underneath the parser.
fn fetch_latest() -> Result<String, ureq::Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();

    agent
        .get(LATEST_RELEASE_API)
        .header("User-Agent", format!("rulogman/{CURRENT_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_to_string()
}

/// Pick the tag, the release page and this platform's asset out of a
/// latest-release response.
///
/// `None` when the body is not an object, or carries no usable `tag_name`; a
/// missing `html_url` is tolerated and leaves [`Release::url`] empty, because
/// [`release_url`] has a sensible destination for that case and a release with
/// no page is still worth announcing. A missing asset is tolerated for the same
/// reason, and means the same thing to the dialog: hand off to the browser.
fn parse_release(body: &str) -> Option<Release> {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(err) => {
            log::debug!("update check: unreadable response: {err}");
            return None;
        }
    };

    let tag = value.get("tag_name")?.as_str()?.trim();
    if tag.is_empty() {
        return None;
    }

    let url = value
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let asset = TARGET
        .map(|target| asset_name(tag, target))
        .and_then(|name| find_asset(&value, &name));

    Some(Release {
        tag: tag.to_string(),
        version: strip_v(tag).to_string(),
        url: url.to_string(),
        asset,
    })
}

/// The file name the release workflow publishes for `tag` on `target`.
///
/// Mirrors `.github/workflows/release.yml`; the two have to be changed together,
/// and a mismatch degrades to the browser fallback rather than to a wrong
/// download, because nothing in the response would match this name.
fn asset_name(tag: &str, target: &str) -> String {
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("rulogman-{tag}-{target}.{extension}")
}

/// The `assets` entry called `name`, read into an [`Asset`].
///
/// An entry without a download URL is no asset at all, so it answers `None` and
/// the release announces itself without one.
fn find_asset(value: &serde_json::Value, name: &str) -> Option<Asset> {
    let entry = value
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(name))?;

    let url = entry
        .get("browser_download_url")
        .and_then(serde_json::Value::as_str)?;
    if url.is_empty() {
        return None;
    }

    Some(Asset {
        name: name.to_string(),
        url: url.to_string(),
        size: entry
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        digest: entry
            .get("digest")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_digest),
    })
}

/// Read the API's `digest` field, which is `"<algorithm>:<hex>"`.
///
/// Only SHA-256 is accepted, and only as exactly 64 hex digits. Anything else —
/// a future algorithm, a truncated value, a field that changed shape — answers
/// `None` and leaves the size check as the only verification, which is the
/// behaviour on the many responses that carry no digest at all.
fn parse_digest(raw: &str) -> Option<String> {
    let (algorithm, hex) = raw.trim().split_once(':')?;
    if !algorithm.eq_ignore_ascii_case("sha256") {
        return None;
    }
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// Whether `latest` names a strictly newer version than `current`.
///
/// Both sides are read by [`parse_version`], and anything it cannot read
/// compares as *not* newer. That asymmetry is the point: the only consequence of
/// answering `false` is that a dialog does not appear, while answering `true` on
/// a tag nobody can interpret would nag the user about a release that may not
/// exist. A hand-pushed `nightly` tag, a release named after a branch, an API
/// answering something unexpected — all of them stay quiet.
fn is_newer(latest: &str, current: &str) -> bool {
    let (Some(latest), Some(current)) = (parse_version(latest), parse_version(current)) else {
        return false;
    };

    // Compared position by position rather than as vectors, so that a tag with
    // fewer components than the running version — `v1` against `0.3.2` — is read
    // as `1.0.0` and wins, instead of being cut short by the shorter length.
    let len = latest.len().max(current.len());
    for index in 0..len {
        let left = latest.get(index).copied().unwrap_or(0);
        let right = current.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// Split a version string into its numeric components.
///
/// Accepts the `v` prefix the project's tags carry, in either case, and nothing
/// else: every dot-separated component must be a plain non-negative integer.
/// Pre-release and build suffixes (`1.2.3-rc1`, `1.2.3+build`) are therefore
/// rejected rather than truncated — rulogman does not publish them, so a tag
/// wearing one is a surprise, and a surprise should not open a dialog.
fn parse_version(version: &str) -> Option<Vec<u64>> {
    let version = strip_v(version.trim());
    if version.is_empty() {
        return None;
    }
    version
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

/// Drop one leading `v` or `V`, if there is one.
fn strip_v(version: &str) -> &str {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_component_anywhere_is_newer() {
        assert!(is_newer("0.3.3", "0.3.2"));
        assert!(is_newer("0.4.0", "0.3.2"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.3.3", "0.3.2"));
        assert!(is_newer("V0.3.3", "0.3.2"));
    }

    #[test]
    fn the_same_or_an_older_version_is_not_newer() {
        assert!(!is_newer("0.3.2", "0.3.2"));
        assert!(!is_newer("v0.3.2", "0.3.2"));
        assert!(!is_newer("0.3.1", "0.3.2"));
        assert!(!is_newer("0.2.9", "0.3.2"));
        assert!(!is_newer("0.3.2", "1.0.0"));
    }

    #[test]
    fn components_compare_numerically_and_not_as_text() {
        // The whole reason not to compare the strings: "10" sorts before "9".
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(is_newer("0.3.10", "0.3.9"));
    }

    #[test]
    fn a_missing_component_counts_as_zero() {
        assert!(!is_newer("0.3", "0.3.2"));
        assert!(!is_newer("0.3.2", "0.3.2.0"));
        assert!(!is_newer("0.3.2.0", "0.3.2"));
        assert!(is_newer("0.4", "0.3.2"));
        assert!(is_newer("1", "0.3.2"));
        assert!(is_newer("0.3.2.1", "0.3.2"));
    }

    #[test]
    fn an_unreadable_version_on_either_side_is_never_newer() {
        for tag in [
            "",
            "   ",
            "v",
            "nightly",
            "1.2.3-rc1",
            "1.2.3+build",
            "1..2",
            "1.2.",
            ".1.2",
            "1.-2",
            "0x10",
            "٩.٩",
            "99999999999999999999999",
        ] {
            assert!(!is_newer(tag, "0.3.2"), "{tag:?} must not read as newer");
            assert!(!is_newer("9.9.9", tag), "{tag:?} must not be compared to");
        }
    }

    #[test]
    fn the_shipped_version_is_one_this_module_can_read() {
        // A workspace version this parser cannot read would silence the check
        // permanently, and silently — exactly the failure this test exists to
        // notice the moment the version scheme changes.
        assert!(
            parse_version(CURRENT_VERSION).is_some(),
            "{CURRENT_VERSION} is not a version `parse_version` understands"
        );
        assert!(is_newer("999.0.0", CURRENT_VERSION));
        assert!(!is_newer(CURRENT_VERSION, CURRENT_VERSION));
    }

    #[test]
    fn a_release_response_yields_its_tag_and_page() {
        // Trimmed to the fields that matter; the real payload carries dozens
        // more, which is why the parser reaches for keys by name.
        let body = r#"{
            "tag_name": "v0.4.0",
            "name": "rulogman 0.4.0",
            "draft": false,
            "html_url": "https://github.com/xcomart/rulogman/releases/tag/v0.4.0",
            "assets": []
        }"#;
        let release = parse_release(body).expect("a well-formed release");
        assert_eq!(release.tag, "v0.4.0");
        assert_eq!(release.version, "0.4.0");
        assert_eq!(
            release.url,
            "https://github.com/xcomart/rulogman/releases/tag/v0.4.0"
        );
        assert_eq!(
            release_url(&release),
            "https://github.com/xcomart/rulogman/releases/tag/v0.4.0"
        );
        // No assets at all is the browser-fallback case on every platform.
        assert!(release.asset.is_none());
    }

    #[test]
    fn a_release_without_a_page_falls_back_to_the_releases_index() {
        let release = parse_release(r#"{"tag_name":"0.4.0"}"#).expect("a tag is enough");
        assert_eq!(release.tag, "0.4.0");
        assert_eq!(release.version, "0.4.0");
        assert!(release.url.is_empty());
        assert!(release.asset.is_none());
        assert_eq!(release_url(&release), RELEASES_PAGE);
    }

    #[test]
    fn a_response_without_a_usable_tag_is_no_release() {
        for body in [
            "",
            "not json at all",
            "<html>captive portal</html>",
            "null",
            "[]",
            r#"{"message":"API rate limit exceeded"}"#,
            r#"{"tag_name":null}"#,
            r#"{"tag_name":42}"#,
            r#"{"tag_name":""}"#,
            r#"{"tag_name":"   "}"#,
        ] {
            assert!(parse_release(body).is_none(), "{body:?} must yield nothing");
        }
    }

    #[test]
    fn a_surrounding_whitespace_only_differs_by_trimming() {
        let release = parse_release(r#"{"tag_name":"  v1.2.3  "}"#).expect("a padded tag");
        assert_eq!(release.tag, "v1.2.3");
        assert_eq!(release.version, "1.2.3");
    }

    #[test]
    fn an_asset_name_follows_the_release_workflow() {
        assert_eq!(
            asset_name("v0.4.0", "x86_64-pc-windows-msvc"),
            "rulogman-v0.4.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            asset_name("v0.4.0", "aarch64-apple-darwin"),
            "rulogman-v0.4.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name("v0.4.0", "x86_64-unknown-linux-gnu"),
            "rulogman-v0.4.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    /// A response carrying all three published assets, as the API shapes them.
    fn three_assets(tag: &str) -> String {
        let entries: Vec<String> = [
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
        ]
        .iter()
        .map(|target| {
            let name = asset_name(tag, target);
            format!(
                r#"{{"name":"{name}",
                    "size":1234,
                    "digest":"sha256:{hex}",
                    "browser_download_url":"https://example.invalid/{name}"}}"#,
                hex = "ab".repeat(32)
            )
        })
        .collect();
        format!(r#"{{"tag_name":"{tag}","assets":[{}]}}"#, entries.join(","))
    }

    #[test]
    fn the_asset_for_this_target_is_the_one_picked() {
        let release = parse_release(&three_assets("v9.9.9")).expect("a well-formed release");
        match TARGET {
            // The three targets the project publishes: exactly one entry of the
            // response is the right one, and it is chosen by name.
            Some(target) => {
                let asset = release.asset.expect("a build for a published target");
                assert_eq!(asset.name, asset_name("v9.9.9", target));
                assert!(asset.url.ends_with(&asset.name));
                assert_eq!(asset.size, 1234);
                assert_eq!(asset.digest.as_deref(), Some("ab".repeat(32).as_str()));
            }
            // Everything else — an Intel Mac, an ARM Linux box — has no build
            // to install and must fall back to the browser.
            None => assert!(release.asset.is_none()),
        }
    }

    #[test]
    fn an_asset_for_another_tag_is_not_this_release() {
        // The name carries the tag, so a response whose assets were built for a
        // different one matches nothing and degrades to the browser fallback.
        let body =
            three_assets("v9.9.9").replace("\"tag_name\":\"v9.9.9\"", "\"tag_name\":\"v8.8.8\"");
        let release = parse_release(&body).expect("a well-formed release");
        assert_eq!(release.tag, "v8.8.8");
        assert!(release.asset.is_none());
    }

    #[test]
    fn an_asset_without_a_download_url_is_no_asset() {
        let Some(target) = TARGET else { return };
        let name = asset_name("v9.9.9", target);
        for entry in [
            format!(r#"{{"name":"{name}","size":1}}"#),
            format!(r#"{{"name":"{name}","browser_download_url":""}}"#),
            format!(r#"{{"name":"{name}","browser_download_url":42}}"#),
        ] {
            let body = format!(r#"{{"tag_name":"v9.9.9","assets":[{entry}]}}"#);
            let release = parse_release(&body).expect("a well-formed release");
            assert!(release.asset.is_none(), "{entry} must not be usable");
        }
    }

    #[test]
    fn an_asset_may_arrive_without_a_size_or_a_digest() {
        let Some(target) = TARGET else { return };
        let name = asset_name("v9.9.9", target);
        let body = format!(
            r#"{{"tag_name":"v9.9.9","assets":[
                {{"name":"{name}","browser_download_url":"https://example.invalid/a"}}]}}"#
        );
        let asset = parse_release(&body)
            .and_then(|release| release.asset)
            .expect("a usable asset");
        // A zero size disables the byte-count check rather than failing it.
        assert_eq!(asset.size, 0);
        assert_eq!(asset.digest, None);
    }

    #[test]
    fn only_a_well_formed_sha256_digest_is_kept() {
        let sha = "ab".repeat(32);
        assert_eq!(parse_digest(&format!("sha256:{sha}")), Some(sha.clone()));
        assert_eq!(
            parse_digest(&format!("SHA256:{}", sha.to_uppercase())),
            Some(sha)
        );
        for raw in [
            "",
            "sha256",
            "sha256:",
            "sha512:{}",
            &format!("sha512:{}", "ab".repeat(32)),
            &format!("sha256:{}", "ab".repeat(31)),
            &format!("sha256:{}", "zz".repeat(32)),
        ] {
            assert_eq!(parse_digest(raw), None, "{raw:?} must not be accepted");
        }
    }

    #[test]
    fn a_digest_is_compared_as_lower_case_hex() {
        // The empty input's SHA-256, so the encoder is checked against a value
        // that is not of this codebase's making.
        let digest = ring::digest::digest(&ring::digest::SHA256, b"");
        assert_eq!(
            hex(digest.as_ref()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_displaced_copy_keeps_its_whole_name() {
        for (path, expected) in [
            ("C:/Program Files/rulogman/rulogman.exe", "rulogman.exe.old"),
            ("/usr/local/bin/rulogman", "rulogman.old"),
            ("/Applications/rulogman.app", "rulogman.app.old"),
        ] {
            let retired = old_path(Path::new(path)).expect("a path with a file name");
            assert_eq!(
                retired.file_name().and_then(|name| name.to_str()),
                Some(expected)
            );
            assert_eq!(retired.parent(), Path::new(path).parent());
        }
        assert_eq!(old_path(Path::new("/")), None);
    }

    #[test]
    fn a_bundle_is_found_however_deep_the_binary_sits() {
        assert_eq!(
            bundle_root(Path::new(
                "/Applications/rulogman.app/Contents/MacOS/rulogman"
            )),
            Some(PathBuf::from("/Applications/rulogman.app"))
        );
        // The extension is what identifies it, not the depth or the name.
        assert_eq!(
            bundle_root(Path::new("/tmp/x/Some Name.APP/Contents/MacOS/rulogman")),
            Some(PathBuf::from("/tmp/x/Some Name.APP"))
        );
        // A development build, and a binary copied out of its bundle: nothing
        // to swap, which is what makes the macOS install refuse.
        assert_eq!(
            bundle_root(Path::new("/work/rulogman/target/debug/rulogman")),
            None
        );
        assert_eq!(bundle_root(Path::new("/usr/local/bin/rulogman")), None);
    }

    #[test]
    fn an_archive_name_can_never_leave_the_staging_directory() {
        assert_eq!(
            archive_name("rulogman-v0.4.0-x86_64-pc-windows-msvc.zip"),
            "rulogman-v0.4.0-x86_64-pc-windows-msvc.zip"
        );
        for hostile in ["", ".", "..", "../evil", "a/b", "a\\b", "/etc/passwd"] {
            assert_eq!(archive_name(hostile), FALLBACK_ARCHIVE, "{hostile:?}");
        }
    }

    #[test]
    fn the_payload_is_found_at_the_root_or_one_level_down() {
        let root = tempfile::tempdir().expect("a temp directory");
        let root = root.path();

        // Nothing there yet.
        assert_eq!(find_payload(root, "rulogman"), None);

        // The shape every published archive has: one wrapper directory.
        let wrapper = root.join("rulogman-v0.4.0-x86_64-unknown-linux-gnu");
        fs::create_dir_all(&wrapper).expect("a wrapper directory");
        fs::write(wrapper.join("rulogman"), b"binary").expect("a payload");
        assert_eq!(
            find_payload(root, "rulogman"),
            Some(wrapper.join("rulogman"))
        );

        // A flat archive works too, and wins, because it is unambiguous.
        fs::write(root.join("rulogman"), b"binary").expect("a payload");
        assert_eq!(find_payload(root, "rulogman"), Some(root.join("rulogman")));

        // A directory counts as a payload: that is what the macOS bundle is.
        let bundles = tempfile::tempdir().expect("a temp directory");
        fs::create_dir_all(bundles.path().join("wrapper/rulogman.app/Contents")).expect("a bundle");
        assert_eq!(
            find_payload(bundles.path(), "rulogman.app"),
            Some(bundles.path().join("wrapper/rulogman.app"))
        );
    }

    #[test]
    fn a_release_with_no_asset_cannot_be_installed() {
        // The one `install` failure reachable without touching the network or
        // the filesystem, and the one that must never be a panic: it is what
        // an unpublished target reaches if the dialog ever routes it here.
        let release = Release {
            tag: "v9.9.9".to_string(),
            version: "9.9.9".to_string(),
            url: String::new(),
            asset: None,
        };
        let mut seen = Vec::new();
        let error = install(&release, &mut |progress| seen.push(progress))
            .expect_err("no asset, no install");
        assert!(error.contains("v9.9.9"), "{error}");
        assert!(seen.is_empty(), "nothing should have been reported");
    }

    #[test]
    fn a_directory_is_the_same_as_itself_however_it_is_written() {
        let installed = tempfile::tempdir().expect("a temporary directory");
        let path = installed.path();
        assert!(same_directory(path, path));
        // The shape Inno Setup actually stores. Comparing the two as text would
        // fail here, which is the whole reason the comparison canonicalises.
        let trailing = format!("{}{}", path.display(), std::path::MAIN_SEPARATOR);
        assert!(same_directory(Path::new(&trailing), path));
        // And a leading or trailing space of the kind a hand-edited registry
        // value collects, which the caller trims before asking.
        assert!(same_directory(
            Path::new(format!("  {trailing} ").trim()),
            path
        ));
    }

    #[test]
    fn a_different_missing_or_merely_nested_directory_is_not_the_same() {
        let installed = tempfile::tempdir().expect("a temporary directory");
        let elsewhere = tempfile::tempdir().expect("a second temporary directory");
        assert!(!same_directory(installed.path(), elsewhere.path()));

        // A parent or a child is a near miss and still a miss: a portable copy
        // unpacked inside the installed copy's directory is not that install.
        let nested = installed.path().join("syntaxes");
        fs::create_dir(&nested).expect("a subdirectory");
        assert!(!same_directory(&nested, installed.path()));
        assert!(!same_directory(installed.path(), &nested));

        // Nothing there to canonicalise. The answer is "no", never a panic:
        // an entry pointing at a directory that is gone is someone else's
        // broken installation.
        assert!(!same_directory(
            &installed.path().join("gone"),
            installed.path()
        ));
    }

    #[test]
    fn the_uninstall_key_is_the_one_the_installer_writes() {
        // The triangle from `ARP_KEY`'s docs, one side of it checked
        // mechanically. Inno Setup names its uninstall key by appending `_is1`
        // to `AppId`, so if these two ever drift the updater goes on running and
        // silently stops correcting the version — the exact failure mode that is
        // hardest to notice. The third side, the `ProductCode` in the winget
        // manifests, is not checked here only because that directory is named
        // after a release and would have to be found rather than named.
        let script = include_str!("../../../packaging/windows/rulogman.iss");
        // The doubled brace is Inno's escape for a literal one, so what follows
        // it is the GUID with its own closing brace still attached.
        let app_id = script
            .lines()
            .find_map(|line| line.trim().strip_prefix("AppId={{"))
            .expect("an AppId in rulogman.iss");
        assert!(
            ARP_KEY.ends_with(&format!("{{{app_id}_is1")),
            "{ARP_KEY} is not the key Inno derives from AppId={{{app_id}"
        );
    }

    /// A registry key under `HKCU\Software` that removes itself when the test
    /// holding it ends.
    ///
    /// The real uninstall key is a live part of this machine's installed-program
    /// list, and the tests below write to a scratch key instead — which is what
    /// [`write_display_version`] takes its key path as an argument for. The name
    /// carries a UUID so that two tests running on the same machine, in the same
    /// process or not, cannot collide.
    #[cfg(windows)]
    struct ScratchKey(String);

    #[cfg(windows)]
    impl ScratchKey {
        fn new() -> Self {
            let path = format!("Software\\rulogman-test-{}", uuid::Uuid::new_v4());
            windows_registry::CURRENT_USER
                .create(&path)
                .expect("a scratch registry key under HKCU");
            Self(path)
        }

        /// The key itself, opened for reading and writing.
        fn open(&self) -> windows_registry::Key {
            windows_registry::CURRENT_USER
                .options()
                .read()
                .write()
                .open(&self.0)
                .expect("the scratch key this test just created")
        }
    }

    #[cfg(windows)]
    impl Drop for ScratchKey {
        fn drop(&mut self) {
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_entry_describing_this_copy_has_its_version_rewritten() {
        let installed = tempfile::tempdir().expect("a temporary directory");
        let scratch = ScratchKey::new();
        let key = scratch.open();
        // Written with the trailing backslash Inno leaves, so the comparison is
        // exercised against the real shape and not a tidied one.
        key.set_string(
            INSTALL_LOCATION,
            format!("{}\\", installed.path().display()),
        )
        .expect("an install location");
        key.set_string(DISPLAY_VERSION, "0.3.7")
            .expect("a starting version");

        assert!(write_display_version(
            windows_registry::CURRENT_USER,
            "HKCU",
            &scratch.0,
            installed.path(),
            "0.3.8",
        ));
        assert_eq!(key.get_string(DISPLAY_VERSION).expect("a version"), "0.3.8");
    }

    #[cfg(windows)]
    #[test]
    fn an_entry_describing_another_copy_is_left_alone() {
        // The case the guard exists for: an installed copy and a portable one on
        // the same machine, and the portable one updating itself. Marking the
        // installed copy up to date would take it out of `winget upgrade`
        // forever while its executable stayed where it was.
        let installed = tempfile::tempdir().expect("a temporary directory");
        let portable = tempfile::tempdir().expect("a second temporary directory");
        let scratch = ScratchKey::new();
        let key = scratch.open();
        key.set_string(INSTALL_LOCATION, installed.path().display().to_string())
            .expect("an install location");
        key.set_string(DISPLAY_VERSION, "0.3.7")
            .expect("a starting version");

        assert!(!write_display_version(
            windows_registry::CURRENT_USER,
            "HKCU",
            &scratch.0,
            portable.path(),
            "0.3.8",
        ));
        assert_eq!(
            key.get_string(DISPLAY_VERSION).expect("a version"),
            "0.3.7",
            "the other installation's recorded version must not move"
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_entry_that_describes_nothing_is_left_alone() {
        // An uninstall key with no `InstallLocation` cannot be matched against
        // anything, so it is not this copy's to edit either.
        let installed = tempfile::tempdir().expect("a temporary directory");
        let scratch = ScratchKey::new();
        let key = scratch.open();
        key.set_string(DISPLAY_VERSION, "0.3.7")
            .expect("a starting version");

        assert!(!write_display_version(
            windows_registry::CURRENT_USER,
            "HKCU",
            &scratch.0,
            installed.path(),
            "0.3.8",
        ));
        assert_eq!(key.get_string(DISPLAY_VERSION).expect("a version"), "0.3.7");
    }

    #[cfg(windows)]
    #[test]
    fn an_entry_that_is_not_there_is_not_created() {
        // What a copy unpacked from the zip looks like, and the one outcome that
        // would be actively harmful: an "Apps & features" entry whose uninstall
        // command points at an uninstaller that was never installed.
        let installed = tempfile::tempdir().expect("a temporary directory");
        let absent = format!("Software\\rulogman-test-{}", uuid::Uuid::new_v4());

        assert!(!write_display_version(
            windows_registry::CURRENT_USER,
            "HKCU",
            &absent,
            installed.path(),
            "0.3.8",
        ));
        assert!(
            windows_registry::CURRENT_USER.open(&absent).is_err(),
            "the updater must never bring an uninstall entry into existence"
        );
    }
}
