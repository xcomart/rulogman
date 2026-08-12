//! One rulogman session: a transport bound to a terminal emulator.
//!
//! A [`Session`] is a gpui entity. Creating one immediately starts connecting
//! and spawns a pump that drains transport events onto the UI thread, so the
//! whole type is single threaded and never blocks a render.
//!
//! Two transports can drive it: an SSH connection to a remote host, and a shell
//! on this machine — the login shell on unix, and whichever of PowerShell,
//! `cmd` or a WSL distribution the user picked on Windows. They are
//! deliberately one type rather than two: every tab, pane and view in the shell
//! is written against `Entity<Session>`, so a second session type would have to
//! be threaded through all of them. What differs between the two lives in the
//! private [`Target`] and [`Transport`] enums instead, and the public surface
//! answers for both.
//!
//! Credentials are kept for reconnection but are deliberately unreachable from
//! the outside: there is no accessor for them, and the hand written
//! [`Debug`](std::fmt::Debug) implementation omits them entirely.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, SharedString, Task};
use rulogman_core::{EffectiveTerminal, SessionOverrides, SessionProfile};
use rulogman_pty::{PtyConfig, PtyEvent, PtySession};
// The one part of the pty surface that is not cross-platform: Windows has no
// login shell to name, and picks its command from the welcome screen instead.
#[cfg(unix)]
use rulogman_pty::login_shell_name;
use rulogman_ssh::{SshAuth, SshConfig, SshEvent, SshSession, TunnelForward};
use rulogman_term::{Charset, TerminalModel, TerminalTheme};
use uuid::Uuid;

use crate::app_settings;
use crate::files::{FileSource, LocalSource, SftpSource};
// The one source with no counterpart on the other platform: a WSL distribution
// exists to be reached from Windows, and nowhere else.
#[cfg(windows)]
use crate::files::WslSource;
use crate::i18n::ts;
use crate::ui::TabStatus;
use crate::verifier::host_key_verifier;

/// Columns a session starts with, until the view reports its real size.
const INITIAL_COLS: u16 = 80;

/// Rows a session starts with, until the view reports its real size.
const INITIAL_ROWS: u16 = 24;

/// Why a local session ended, when the shell simply exited.
///
/// English, like every other `reason` reaching [`SessionStatus`]: those come
/// from the SSH layer verbatim, and the wording around them is what the locale
/// translates.
const LOCAL_EXIT_REASON: &str = "the local shell exited";

/// Classification put on a [`SessionStatus::Failed`] raised by the local pty.
///
/// The SSH kinds name the *stage* that failed because a remote connection has
/// several of them; starting a local shell has one, so this names the subsystem
/// and leaves what went wrong entirely to the transport's own message.
const LOCAL_FAILURE_KIND: &str = "local shell";

/// Where a [`Session`] currently is in its life cycle.
#[derive(Debug, Clone)]
pub enum SessionStatus {
    /// The transport is connecting, verifying the host key or authenticating.
    Connecting,
    /// The remote shell is live.
    Connected,
    /// The session ended without an error.
    Disconnected {
        /// Human-readable explanation of why the session ended.
        reason: String,
    },
    /// The session failed and cannot continue without a reconnect.
    Failed {
        /// Coarse classification of the failure.
        ///
        /// A [`SharedString`] rather than an
        /// [`SshErrorKind`](rulogman_ssh::SshErrorKind) because a local shell has
        /// no SSH failure to classify; the SSH path fills it in from its own
        /// kind's [`Display`](std::fmt::Display).
        kind: SharedString,
        /// Human-readable explanation, safe to show to the user.
        message: String,
    },
}

impl SessionStatus {
    /// A short label for the status bar and the connection overlay.
    ///
    /// Translated here rather than stored translated, because the status
    /// outlives the language it was reached in: the caller asks for the summary
    /// while rendering, so a language switch shows up on the very next frame.
    /// The `reason`, `kind` and `message` inside come from the transport and
    /// stay in English; only the wording around them follows the locale.
    pub fn summary(&self) -> SharedString {
        match self {
            Self::Connecting => ts!("session.connecting"),
            Self::Connected => ts!("session.connected"),
            Self::Disconnected { reason } => ts!("session.disconnected", reason = reason),
            Self::Failed { kind, message } => {
                ts!("session.failed", kind = kind, message = message)
            }
        }
    }

    /// Whether the session can currently accept input.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Connecting | Self::Connected)
    }

    /// How a session in this state should be rendered in the tab strip.
    ///
    /// Lives on the status rather than only on [`Session`] so that the mapping
    /// can be asserted without standing an entity — and a gpui app — up first.
    pub fn tab_status(&self) -> TabStatus {
        match self {
            Self::Connecting => TabStatus::Connecting,
            Self::Connected => TabStatus::Connected,
            Self::Disconnected { .. } => TabStatus::Disconnected,
            Self::Failed { .. } => TabStatus::Error,
        }
    }
}

/// What a [`Session`] is attached to, and everything needed to attach again.
///
/// Held for the whole life of the session, transport or no transport: a
/// reconnect rebuilds the transport out of exactly this.
enum Target {
    /// A remote host reached over SSH.
    Ssh {
        /// Profile the session was opened from.
        ///
        /// Boxed because a profile is by far the largest thing either variant
        /// carries, and every local session would otherwise pay for it.
        profile: Box<SessionProfile>,
        /// Credentials, retained so that [`Session::reconnect`] can reuse them.
        ///
        /// Never rendered, logged or otherwise exposed.
        auth: SshAuth,
    },
    /// A shell on this machine.
    Local {
        /// What to call that shell in the tab strip and the status bar.
        ///
        /// Resolved once when the session is created rather than looked up per
        /// frame: on unix it cannot change under a running session and the
        /// lookup reads the passwd database, and on Windows it is simply the
        /// name of the button the user pressed.
        shell: SharedString,
        /// Directory the shell is started in; `None` means the app's own.
        ///
        /// Normally the user's home directory — the application's own working
        /// directory is `/` when macOS launches it from the Finder, which is
        /// no place to open a shell. Set by [`Session::duplicate`] so that a
        /// split of a local pane opens where the original shell is, and kept
        /// so that a restart lands in the same place the session originally
        /// started in.
        cwd: Option<PathBuf>,
        /// The command line to run, or `None` for the platform's default.
        ///
        /// `None` is what unix always uses: the pty starts the user's login
        /// shell, which is the only local shell that platform offers. Windows
        /// has several — PowerShell, `cmd`, one per installed WSL distribution
        /// — so the welcome screen names the one it wants here, and a
        /// reconnect or a duplicate starts that same one again.
        command: Option<Vec<String>>,
        /// Which filesystem the shell on the other end is standing in.
        ///
        /// Read by [`Session::files`], which is the only thing it changes:
        /// everything else about a WSL tab is an ordinary local pty.
        filesystem: LocalFilesystem,
    },
}

/// The filesystem a local shell browses.
///
/// A shell started on this machine is not necessarily standing in this
/// machine's filesystem. A WSL one is a Linux shell in a Linux tree, and the
/// directory it reports over `OSC 7` — `/home/ada` — names nothing in the
/// Windows tree this process sees; the two are reached by different means and
/// spell their paths differently, so which of them a session is looking at is
/// carried rather than guessed at.
///
/// A variant rather than the `bool` this started as, because the WSL answer is
/// not merely "not this machine": browsing the distribution takes its *name*,
/// and the only place that name is known is the button the user pressed.
#[derive(Debug, Clone)]
pub enum LocalFilesystem {
    /// The filesystem this process itself sees.
    ///
    /// Every shell that runs directly here — the login shell on unix,
    /// PowerShell and `cmd` on Windows.
    ThisMachine,
    /// A WSL distribution's own Linux filesystem, browsed over its share.
    ///
    /// Windows-only for the same reason the distributions themselves are: on
    /// Linux there is no `wsl.exe` to start a shell with, and so no session
    /// that could carry this.
    #[cfg(windows)]
    Wsl {
        /// The distribution's name, as `wsl.exe -l -q` reports it — which is
        /// also the share name its filesystem is reached under.
        distro: String,
    },
}

/// A shell this machine can start, as offered to the user before it exists.
///
/// Windows-only, because it answers a question only Windows asks: *which*
/// local shell? Unix has one — the login shell — and needs neither a list nor
/// a description of an entry in it. Here there are at least two, and one more
/// per installed WSL distribution, so the choice is a value that two places
/// have to agree on: the welcome screen's buttons and the connection dialog's
/// pinned rows offer the same shells, and a shell described in only one of
/// them would be a shell the other silently lacks.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct LocalShell {
    /// Plain name of the shell — `PowerShell`, `cmd`, or the distribution's
    /// own name. Never translated: it is what the thing is called, and it is
    /// also what the tab is called until the shell titles itself.
    pub name: SharedString,
    /// Command line that starts it.
    pub command: Vec<String>,
    /// Filesystem the shell stands in, which is what the session's file panel
    /// ends up browsing.
    pub filesystem: LocalFilesystem,
}

#[cfg(windows)]
impl LocalShell {
    /// What kind of shell this is, for the line above [`Self::name`].
    ///
    /// Both callers pair the two: the welcome screen joins them with a
    /// separator, the dialog stacks them the way a profile row stacks its name
    /// over its `user@host`. `WSL` is a product name and so stays as it is in
    /// every language; the other two are terminals on this machine and are
    /// named by the phrase the rest of the interface already uses for one.
    pub fn kind_label(&self) -> SharedString {
        match self.filesystem {
            LocalFilesystem::ThisMachine => ts!("connection.local.name"),
            LocalFilesystem::Wsl { .. } => "WSL".into(),
        }
    }
}

/// Every local shell this machine can start, given the WSL distributions
/// `distros` found on it.
///
/// The two fixed shells come first and in a stable order, so that an index
/// into this list keeps meaning the same shell when a later discovery appends
/// the WSL ones.
#[cfg(windows)]
pub fn local_shells(distros: &[String]) -> Vec<LocalShell> {
    // `-NoLogo` because the copyright banner is two lines of noise above the
    // first prompt, and the user asked for a shell rather than for the version
    // of it.
    let mut shells = vec![
        LocalShell {
            name: "PowerShell".into(),
            command: vec!["powershell.exe".to_owned(), "-NoLogo".to_owned()],
            filesystem: LocalFilesystem::ThisMachine,
        },
        LocalShell {
            name: "cmd".into(),
            command: vec!["cmd.exe".to_owned()],
            filesystem: LocalFilesystem::ThisMachine,
        },
    ];

    shells.extend(distros.iter().map(|distro| LocalShell {
        name: SharedString::from(distro.clone()),
        // `--cd ~` starts the shell in the distribution's home directory.
        // Without it WSL inherits this process's working directory and
        // translates it, dropping the user somewhere under `/mnt/c` instead.
        command: vec![
            "wsl.exe".to_owned(),
            "-d".to_owned(),
            distro.clone(),
            "--cd".to_owned(),
            "~".to_owned(),
        ],
        filesystem: LocalFilesystem::Wsl {
            distro: distro.clone(),
        },
    }));

    shells
}

/// The shell's short name for a local title that is only its binary's path,
/// or `None` for a title actually worth showing.
///
/// ConPTY's first title report is the console's default title, which is the
/// full path of the executable that was started: without this, a fresh
/// PowerShell tab reads `C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe`
/// until the shell retitles the console — which `cmd` never does. Such a title
/// says nothing that the name of the button the user pressed does not, at ten
/// times the width, so it is folded back into that name.
///
/// The comparison is by file stem, so it survives any install location and the
/// presence or absence of `.exe`; conhost's `path - command` form keeps its
/// tail, with only the path folded. A title whose stem is not the shell's —
/// a directory a prompt reported, a name the user set — is left alone, and a
/// session with no explicit command (unix, where the login shell is implied)
/// never matches.
fn local_shell_title(
    title: &str,
    shell: &SharedString,
    command: Option<&[String]>,
) -> Option<SharedString> {
    let exe_stem = file_stem(command?.first()?);
    let (path, running) = match title.split_once(" - ") {
        Some((path, running)) => (path, Some(running)),
        None => (title, None),
    };
    if !file_stem(path.trim()).eq_ignore_ascii_case(exe_stem) {
        return None;
    }
    Some(match running {
        Some(running) => SharedString::from(format!("{shell} - {running}")),
        None => shell.clone(),
    })
}

/// Last path segment of `text`, without its extension.
///
/// By hand rather than through [`Path::file_stem`], because the paths in
/// question are the *console's*: a title reported by ConPTY spells its
/// separators the Windows way whatever platform this build is on, and
/// `Path` on unix would read the whole of `C:\...\powershell.exe` as one
/// hidden-file-like component. Splitting on both separators is what
/// `Language::detect` does with panel names, for the same reason.
fn file_stem(text: &str) -> &str {
    let name = text.rsplit(['/', '\\']).next().unwrap_or(text);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// A live transport handle.
///
/// Both variants are fire-and-forget channels into threads owned by the
/// transport crates, so every method here is non-blocking.
enum Transport {
    /// A connected — or still connecting — SSH session.
    Ssh(SshSession),
    /// A local shell on its own pty.
    Local(PtySession),
}

impl Transport {
    /// Sends already encoded bytes to the shell on the other end.
    fn send_input(&self, bytes: Vec<u8>) {
        match self {
            Self::Ssh(ssh) => ssh.send_input(bytes),
            Self::Local(pty) => pty.send_input(bytes),
        }
    }

    /// Tells the shell that the terminal has been resized.
    fn resize(&self, cols: u16, rows: u16) {
        match self {
            Self::Ssh(ssh) => ssh.resize(cols, rows),
            Self::Local(pty) => pty.resize(cols, rows),
        }
    }

    /// Ends the session behind this handle.
    ///
    /// Takes `self` because a closed transport must not be reachable
    /// afterwards; the caller always reaches this through an `Option::take`.
    fn close(self) {
        match self {
            Self::Ssh(ssh) => ssh.disconnect(),
            Self::Local(pty) => pty.shutdown(),
        }
    }
}

/// The port forwardings a session currently owns.
///
/// Owns, not "was configured with". Several tabs can be opened from one
/// profile, but only one of them holds the profile's local ports: the first to
/// bind them keeps them, and a session opened — or reconnected — while a
/// sibling is still holding them is told so before it starts and never asks
/// (see [`Session::tunnels_suppressed`]), so it neither competes for the ports
/// nor prints a failure notice about them. The holder's forwardings are
/// therefore the only ones in play, and this list is what answers "which tab is
/// the tunnel actually running in" — the question the mark in the tab strip
/// exists to answer.
///
/// A rule can still fail, and that path is unchanged: something outside rulogman
/// may hold the port, or a listener may give up after a run of failed accepts.
/// Both arrive as [`SshEvent::TunnelFailed`] and are folded in below.
///
/// A type of its own, folding the events in itself, so that the rule can be
/// asserted without standing a session — and a gpui app — up first, exactly as
/// [`SessionStatus::tab_status`] can.
#[derive(Debug, Default)]
struct OpenTunnels {
    /// One label per live forwarding, as `local_port:remote_host:remote_port`.
    labels: Vec<SharedString>,
}

impl OpenTunnels {
    /// Folds one transport event into the list.
    fn observe(&mut self, event: &SshEvent) {
        match event {
            SshEvent::TunnelOpened { rule } => self.labels.push(SharedString::from(rule.clone())),
            // Both terminal events take the transport with them, and the
            // listeners live on the runtime behind it: by the time either of
            // these arrives, the local ports are already closed.
            SshEvent::Disconnected { .. } | SshEvent::Error(..) => self.clear(),
            // A failure withdraws a rule if — and only if — it names one that
            // is on the list. A listener that gives up after a run of failed
            // accepts closes its port on the way out and reports the very
            // label it opened under, so the mark has to go with it. The other
            // two failures cannot match: a rule that never bound was never
            // recorded, and a refused forwarding names one *connection* of a
            // rule rather than the rule itself.
            SshEvent::TunnelFailed { rule, .. } => self.labels.retain(|label| label != rule),
            _ => {}
        }
    }

    /// Forgets every forwarding, because the transport carrying them is gone.
    fn clear(&mut self) {
        self.labels.clear();
    }
}

/// A single session — remote or local — together with the terminal it drives.
pub struct Session {
    /// What the session connects to, and what a reconnect rebuilds from.
    target: Target,
    /// Per-session settings overrides layered on top of the global ones.
    ///
    /// Copied out of the profile for an SSH session and left at the defaults
    /// for a local one, which is not saved anywhere and so has nothing to
    /// override from.
    overrides: SessionOverrides,
    /// Live transport handle; `None` once the session has ended.
    transport: Option<Transport>,
    /// Screen contents and scrollback.
    terminal: TerminalModel,
    /// Current life cycle state.
    status: SessionStatus,
    /// Port forwardings this session, and no other, is currently holding.
    ///
    /// Always empty for a local session: a pty forwards nothing, and nothing
    /// ever reports a forwarding to one.
    tunnels: OpenTunnels,
    /// Whether [`Session::start`] must leave this session's forwardings alone.
    ///
    /// Set when the session was opened — or reconnected — while another live
    /// session from the same profile was already holding that profile's local
    /// ports. Asking for them again could only fail, once per rule and in
    /// yellow across the fresh terminal, so the rules are simply not handed to
    /// the transport at all.
    ///
    /// Decided by the workspace and carried in rather than worked out here,
    /// because the question is about the *other* sessions: a session can see
    /// its own forwardings and nothing else's. It is re-decided before every
    /// reconnect, so a tab whose sibling has since gone picks the forwardings
    /// up the moment it connects again.
    tunnels_suppressed: bool,
    /// Task draining the transport's event stream; dropping it stops the pump.
    _pump: Option<Task<()>>,
}

impl fmt::Debug for Session {
    /// Written by hand so that the credentials in [`Target::Ssh`] can never
    /// reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("target", &self.label())
            .field("status", &self.status)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Builds a session for `profile` and starts connecting straight away.
    ///
    /// The terminal is created from the effective settings — the global defaults
    /// with the profile's overrides applied — so the scheme and scrollback depth
    /// are correct from the very first frame.
    ///
    /// `tunnels_suppressed` says whether another live session from this same
    /// profile is already holding its forwardings; when it is, this one does not
    /// ask for them. It is a parameter rather than something set afterwards
    /// because connecting starts here — by the time the caller got the entity
    /// back the request would already have gone out.
    pub fn new(
        profile: SessionProfile,
        auth: SshAuth,
        tunnels_suppressed: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let overrides = profile.overrides.clone();
        let mut session = Self::build(
            Target::Ssh {
                profile: Box::new(profile),
                auth,
            },
            overrides,
            cx,
        );
        session.tunnels_suppressed = tunnels_suppressed;
        session.start(cx);
        session
    }

    /// Builds a session running the user's login shell on this machine, and
    /// starts it straight away.
    ///
    /// There is nothing to configure: the shell is whatever `$SHELL` — or the
    /// passwd entry — says, and everything else comes from the global terminal
    /// settings, since a local session is not saved and so carries no overrides
    /// of its own.
    #[cfg(unix)]
    pub fn new_local(cx: &mut Context<Self>) -> Self {
        // The login shell runs on this machine's own filesystem; unix has no
        // local shell that does not.
        Self::new_local_in(
            SharedString::from(login_shell_name()),
            None,
            None,
            LocalFilesystem::ThisMachine,
            cx,
        )
    }

    /// Builds a session running `command` on this machine, and starts it
    /// straight away.
    ///
    /// The Windows counterpart of [`Session::new_local`], and the reason the
    /// two are not one call: there is no single local shell to start here, so
    /// the caller — the welcome screen — says which one it means. `label` is
    /// what the tab is called until the shell sets a title of its own, and is
    /// the plain name of the shell rather than the command line, which is an
    /// implementation detail the user never typed.
    ///
    /// `filesystem` separates the shells that run *here* from a WSL one, which
    /// runs on a Linux filesystem of its own; only the caller knows which of
    /// the two it just built a command line for, and — for a WSL one — which
    /// distribution's filesystem that is.
    #[cfg(windows)]
    pub fn new_local_command(
        label: SharedString,
        command: Vec<String>,
        filesystem: LocalFilesystem,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_local_in(label, None, Some(command), filesystem, cx)
    }

    /// The shared body of the local constructors, starting the shell in `cwd`.
    ///
    /// With no directory of its own the shell opens in the user's home, not in
    /// the application's working directory: a GUI app launched from the Finder
    /// runs in `/`, which no terminal drops its user into.
    fn new_local_in(
        shell: SharedString,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        filesystem: LocalFilesystem,
        cx: &mut Context<Self>,
    ) -> Self {
        let target = Target::Local {
            shell,
            cwd: cwd.or_else(home_dir),
            command,
            filesystem,
        };
        let mut session = Self::build(target, SessionOverrides::default(), cx);
        session.start(cx);
        session
    }

    /// The common part of both constructors: a session with a terminal built
    /// from the effective settings, but no transport yet.
    fn build(target: Target, overrides: SessionOverrides, cx: &mut Context<Self>) -> Self {
        let effective = app_settings::current(cx).effective_terminal(&overrides);
        Self {
            target,
            overrides,
            transport: None,
            terminal: TerminalModel::new(
                INITIAL_COLS,
                INITIAL_ROWS,
                effective.scrollback_lines,
                TerminalTheme::by_name_or_default(&effective.scheme),
            ),
            status: SessionStatus::Connecting,
            tunnels: OpenTunnels::default(),
            // The default for both constructors: a local session has no
            // forwardings to suppress, and the SSH one sets this from its
            // caller's answer before it starts.
            tunnels_suppressed: false,
            _pump: None,
        }
    }

    /// The effective terminal settings for this session: the global defaults
    /// with this session's overrides layered on top.
    ///
    /// Exposed so the view can honor per-session values such as the font size
    /// and the copy-on-select behaviour without re-reading the global settings.
    pub fn effective(&self, cx: &App) -> EffectiveTerminal {
        app_settings::current(cx).effective_terminal(&self.overrides)
    }

    /// Re-reads the settings and applies the ones that can change on a live
    /// session.
    ///
    /// Only the color scheme takes effect immediately. The scrollback depth is
    /// fixed when the terminal model is built — changing it would rebuild the
    /// grid and clear the screen — and the `TERM` value has already been
    /// negotiated with the pty, so both are picked up only on the next
    /// reconnect instead.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let effective = self.effective(cx);
        self.terminal
            .set_theme(TerminalTheme::by_name_or_default(&effective.scheme));
        cx.notify();
    }

    /// The current life cycle state.
    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    /// The port forwardings this session is holding open right now, as
    /// `local_port:remote_host:remote_port`.
    ///
    /// Empty unless the session actually bound the ports: a second tab opened
    /// from the same profile does not ask for them at all while the first is
    /// holding them, and answers with nothing here — which is what lets the tab
    /// strip point at the one tab the forwardings are really running in.
    ///
    /// Non-empty only while the transport carrying the listeners is live:
    /// [`OpenTunnels::observe`] clears the list on either terminal event, and
    /// [`Session::disconnect`] and [`Session::start`] clear it outright. A
    /// caller asking whether some session is holding a profile's ports right
    /// now therefore has its whole answer here, with no status to check
    /// alongside it.
    pub fn open_tunnels(&self) -> &[SharedString] {
        &self.tunnels.labels
    }

    /// The profile this session was opened from, or `None` for a local one.
    ///
    /// The identity two sessions share when they are two connections to the
    /// same saved profile, which is what makes them rivals for one set of local
    /// ports. A local session is nobody's rival: it forwards nothing and was
    /// never opened from a profile.
    pub fn profile_id(&self) -> Option<Uuid> {
        match &self.target {
            Target::Ssh { profile, .. } => Some(profile.id),
            Target::Local { .. } => None,
        }
    }

    /// Tells the session whether to ask for its profile's forwardings the next
    /// time it starts.
    ///
    /// The reconnect counterpart of the flag [`Session::new`] takes: the
    /// workspace re-decides it against the sessions that are live *now* and
    /// sets it just before calling [`Session::reconnect`]. Nothing drawn reads
    /// the flag — what the tab strip marks is the forwardings actually held —
    /// so there is no repaint to ask for here.
    pub fn set_tunnels_suppressed(&mut self, suppressed: bool) {
        self.tunnels_suppressed = suppressed;
    }

    /// What this session is attached to, in one line: `user@host` for an SSH
    /// session, the name of the shell for a local one.
    ///
    /// This is what the status bar and the connection overlay identify a
    /// session by, so that neither has to know which transport it is looking at.
    pub fn label(&self) -> SharedString {
        match &self.target {
            Target::Ssh { profile, .. } => SharedString::from(profile.label()),
            Target::Local { shell, .. } => shell.clone(),
        }
    }

    /// Whether this session runs a shell on this machine rather than a remote
    /// one.
    ///
    /// The views use it to word themselves for what the user is actually
    /// looking at — there is no host to connect to and nothing to reconnect to.
    pub fn is_local(&self) -> bool {
        match &self.target {
            Target::Ssh { .. } => false,
            Target::Local { .. } => true,
        }
    }

    /// The title to show in the tab: the `OSC 0` / `OSC 2` title when the shell
    /// set one, the profile name — or, locally, the shell's name — otherwise.
    ///
    /// With one correction: a local title that is merely the path of the
    /// shell's own binary is shown as the shell's name instead — see
    /// [`local_shell_title`] for why such a title arrives at all.
    pub fn title(&self) -> SharedString {
        match self.terminal.title() {
            Some(title) if !title.trim().is_empty() => match &self.target {
                Target::Local { shell, command, .. } => {
                    local_shell_title(title, shell, command.as_deref())
                        .unwrap_or_else(|| SharedString::from(title.to_owned()))
                }
                Target::Ssh { .. } => SharedString::from(title.to_owned()),
            },
            _ => match &self.target {
                Target::Ssh { profile, .. } => SharedString::from(profile.name.clone()),
                Target::Local { shell, .. } => shell.clone(),
            },
        }
    }

    /// The working directory of the shell, if it announced one.
    ///
    /// Fed by the `OSC 7` / `OSC 1337` sequences a configured prompt emits, so
    /// it stays `None` for shells that do not report their directory. The value
    /// survives a disconnect - the last known directory is still the one the
    /// session ended in - and is cleared by [`Session::reconnect`], because the
    /// new shell has not reported anything yet.
    pub fn cwd(&self) -> Option<&str> {
        self.terminal.cwd()
    }

    /// The filesystem this session can browse, or `None` while it is not
    /// carrying a live shell to browse one over.
    ///
    /// An SSH session browses the server over SFTP; a local one browses this
    /// computer, or — for a WSL tab — the distribution its shell is standing in.
    /// Which of the three the caller gets is the only thing that differs: all
    /// are [`FileSource`]s, and the file panel above is written against the
    /// trait and never asks.
    ///
    /// Both arms are gated on [`SessionStatus::Connected`], and for the same
    /// reason rather than for two: **the panel shows what the session is
    /// looking at, and until a shell is running there is nothing it is looking
    /// at.** Remotely that is also a practical matter — during `Connecting` the
    /// handle is already there and the SFTP service would queue requests behind
    /// the authentication, leaving the panel on a pending listing with nothing
    /// to show for it. Locally the filesystem would answer perfectly well a
    /// moment early, and it is still the wrong answer: a pane that has not
    /// started its shell yet is drawn as *starting*, and a file panel listing a
    /// directory beside it would say the session was further along than it is.
    /// Once a session ends, both sources are gone with the transport.
    ///
    /// Cheap on all three sides, and it has to be: this is called on every
    /// terminal notification, which is every time the shell produces output.
    /// The SFTP source only clones a request channel — the channel itself is
    /// opened lazily on the first request and then reused — and the two local
    /// ones only clone the executor handle they do their blocking work on, plus
    /// a distribution name. Nothing here may probe a filesystem or start a
    /// process; the sources do that inside the work they hand to the executor.
    pub fn files(&self, cx: &App) -> Option<Arc<dyn FileSource>> {
        match (&self.status, &self.transport) {
            (SessionStatus::Connected, Some(Transport::Ssh(ssh))) => {
                Some(Arc::new(SftpSource::new(ssh.sftp())))
            }
            // The pty handle itself is not needed in either local arm: the
            // shell's filesystem is reachable without going through it, and
            // what the session contributes is only *which* filesystem that is.
            (SessionStatus::Connected, Some(Transport::Local(_))) => match &self.target {
                Target::Local {
                    filesystem: LocalFilesystem::ThisMachine,
                    ..
                } => Some(Arc::new(LocalSource::new(cx.background_executor().clone()))
                    as Arc<dyn FileSource>),
                // A WSL tab's shell lives in a Linux filesystem, so its panel
                // does too: the source below answers in the same `/home/ada`
                // the shell prints and reaches the files behind those paths
                // through the distribution's `\\wsl.localhost` share.
                #[cfg(windows)]
                Target::Local {
                    filesystem: LocalFilesystem::Wsl { distro },
                    ..
                } => Some(Arc::new(WslSource::new(
                    cx.background_executor().clone(),
                    distro.clone(),
                )) as Arc<dyn FileSource>),
                _ => None,
            },
            _ => None,
        }
    }

    /// The terminal model, for rendering.
    pub fn terminal(&self) -> &TerminalModel {
        &self.terminal
    }

    /// The terminal model, for scrolling and other view driven mutations.
    pub fn terminal_mut(&mut self) -> &mut TerminalModel {
        &mut self.terminal
    }

    /// Sends already encoded key or paste bytes to the shell.
    ///
    /// Typing always snaps the viewport back to the bottom of the scrollback,
    /// which is what every other terminal does.
    pub fn send_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        self.terminal.scroll_to_bottom();
        if let Some(transport) = &self.transport {
            transport.send_input(bytes);
        }
        cx.notify();
    }

    /// Resizes the terminal and tells the pty about it.
    ///
    /// A resize to the current size is ignored, so callers may invoke this on
    /// every layout pass without flooding the transport with window change
    /// requests.
    pub fn resize(&mut self, cols: u16, rows: u16, cx: &mut Context<Self>) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.terminal.size() == (cols, rows) {
            return;
        }

        self.terminal.resize(cols, rows);
        if let Some(transport) = &self.transport {
            transport.resize(cols, rows);
        }
        cx.notify();
    }

    /// Ends the session. Safe to call on an already closed session.
    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(transport) = self.transport.take() {
            transport.close();
        }
        // Closing the transport closes the listeners with it, and no event
        // reports that: the pump is dropped on the next line, so nothing would
        // arrive to clear them.
        self.tunnels.clear();
        self._pump = None;
        if self.status.is_live() {
            self.status = SessionStatus::Disconnected {
                reason: "closed by the user".to_owned(),
            };
        }
        cx.notify();
    }

    /// Reopens the session: reconnects to the same host with the same
    /// credentials, or — locally — starts the login shell again.
    ///
    /// The terminal is reset first so that the new shell starts on a clean
    /// screen rather than below the output of the previous one.
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(transport) = self.transport.take() {
            transport.close();
        }
        self.terminal.reset();
        self.start(cx);
    }

    /// Opens a second, independent session onto the same target as this one.
    ///
    /// Same principle as [`Session::reconnect`]: the credentials already in
    /// memory are reused, so a duplicate of an SSH session never asks for a
    /// password again. It lives here rather than in the caller for the reason
    /// given at the top of this module — the credentials have no accessor, so
    /// the only place that can hand them to a new session is inside this type.
    ///
    /// A duplicate of a local session starts in the directory the original
    /// shell is in, when it reports one that still exists; splitting a terminal
    /// and landing in the same place is what every other terminal does.
    ///
    /// The returned session is its own entity with its own transport, terminal
    /// and life cycle; nothing about it stays tied to this one. The current
    /// status is irrelevant, exactly as it is for a reconnect: duplicating a
    /// failed or disconnected session is how the user retries in a second pane
    /// while keeping the first one's error on screen.
    ///
    /// `tunnels_suppressed` is passed straight to [`Session::new`] and means
    /// exactly what it means there. It is asked of every duplicate rather than
    /// only of a remote one so that the callers — the two commands that split a
    /// pane or open a second tab — have one call to make whichever kind of pane
    /// they were pointed at; the local arm has nothing to do with it, having no
    /// forwardings to hold or to stay off.
    pub fn duplicate(&self, tunnels_suppressed: bool, cx: &mut Context<Self>) -> Entity<Self> {
        match &self.target {
            Target::Ssh { profile, auth } => {
                let (profile, auth) = ((**profile).clone(), auth.clone());
                cx.new(|cx| Self::new(profile, auth, tunnels_suppressed, cx))
            }
            // The command comes along with the directory: a duplicate of a
            // WSL tab has to open that same distribution, not the default
            // shell of the platform.
            Target::Local {
                shell,
                command,
                filesystem,
                ..
            } => {
                let (shell, command) = (shell.clone(), command.clone());
                let filesystem = filesystem.clone();
                let cwd = self.local_start_dir();
                cx.new(|cx| Self::new_local_in(shell, cwd, command, filesystem, cx))
            }
        }
    }

    /// How the session should be rendered in the tab strip.
    pub fn tab_status(&self) -> TabStatus {
        self.status.tab_status()
    }

    /// The directory a duplicate of this local session should start in.
    ///
    /// Only a directory that exists right now is worth passing on: the shell's
    /// report comes from an `OSC 7` sequence, which can name a directory that
    /// has since been removed, or — if the prompt is misconfigured — something
    /// that is not a path at all. A pty that cannot enter its working directory
    /// fails to start, so anything doubtful falls back to `None` and lets the
    /// new shell open in the user's home directory.
    ///
    /// The same two tests also do the work no third one has to on Windows: a
    /// WSL shell reports a Linux path such as `/home/ada`, which `is_absolute`
    /// rejects there, so a duplicated WSL tab quietly starts where a fresh one
    /// would rather than in a directory the pty could never enter.
    fn local_start_dir(&self) -> Option<PathBuf> {
        let cwd = self.cwd()?;
        let path = Path::new(cwd);
        // Relative is a sign the report was not a path at all: `OSC 7` carries
        // an absolute one, and resolving it against *our* directory would send
        // the new shell somewhere the user never was.
        if !path.is_absolute() || !path.is_dir() {
            return None;
        }
        Some(path.to_path_buf())
    }

    /// Opens the transport and spawns the event pump.
    ///
    /// Settings are read here rather than only in the constructor so that a
    /// reconnect naturally picks up a scheme, `TERM` or timeout changed since the
    /// session was first opened.
    fn start(&mut self, cx: &mut Context<Self>) {
        let settings = app_settings::current(cx);
        let effective = settings.effective_terminal(&self.overrides);
        // Re-applied here, not just on construction, so a reconnect adopts a
        // scheme the user changed while the session was live.
        self.terminal
            .set_theme(TerminalTheme::by_name_or_default(&effective.scheme));
        // Before the transport is built, so the first byte of the session is
        // already read through the right decoder. A local session carries
        // default overrides and therefore lands on UTF-8, which is what a pty
        // on any platform this ships for speaks.
        self.terminal
            .set_charset(Charset::from_label_or_utf8(&effective.charset));

        let (cols, rows) = self.terminal.size();
        // The transport and its pump are built first and installed afterwards:
        // each arm reads out of `self.target`, which rules out touching the
        // fields below in the same breath.
        let (transport, pump) = match &self.target {
            Target::Ssh { profile, auth } => {
                let mut config = SshConfig::new(
                    profile.host.clone(),
                    profile.port,
                    profile.username.clone(),
                    auth.clone(),
                );
                config.cols = cols;
                config.rows = rows;
                config.term = effective.term;
                config.keepalive_secs = settings.connection.keepalive_secs;
                config.connect_timeout_secs = settings.connection.connect_timeout_secs;
                // The profile's rules are the transport's rules, one for one:
                // reading them here rather than at construction is what lets a
                // reconnect pick up a forwarding the user has since edited —
                // and what lets the answer below be re-decided per connect.
                //
                // Unless a sibling session from this profile is holding the
                // ports already, in which case the transport is handed no rules
                // at all: every one of them would fail to bind and say so in
                // the grid, over a terminal the user just opened.
                if self.tunnels_suppressed {
                    if !profile.tunnels.is_empty() {
                        log::debug!(
                            "not forwarding {} rule(s) for {}: another session holds them",
                            profile.tunnels.len(),
                            profile.label()
                        );
                    }
                } else {
                    config.tunnels = profile
                        .tunnels
                        .iter()
                        .map(|rule| TunnelForward {
                            bind_address: rule.bind_address.clone(),
                            local_port: rule.local_port,
                            remote_host: rule.remote_host.clone(),
                            remote_port: rule.remote_port,
                        })
                        .collect();
                }

                let (ssh, mut events) = SshSession::connect(config, host_key_verifier());
                let pump = cx.spawn(async move |this, cx| {
                    while let Some(event) = events.next().await {
                        let delivered =
                            this.update(cx, |session, cx| session.on_ssh_event(event, cx));
                        if delivered.is_err() {
                            break;
                        }
                    }
                });
                (Transport::Ssh(ssh), pump)
            }
            Target::Local { cwd, command, .. } => {
                let mut config = PtyConfig::new(cols, rows);
                config.term = effective.term;
                config.cwd = cwd.clone();
                // `None` here is not "nothing to run" but "run the platform's
                // own default", which is exactly what a unix local session
                // wants and what the pty layer already does.
                config.command = command.clone();

                let (pty, mut events) = PtySession::spawn(config);
                let pump = cx.spawn(async move |this, cx| {
                    while let Some(event) = events.next().await {
                        let delivered =
                            this.update(cx, |session, cx| session.on_pty_event(event, cx));
                        if delivered.is_err() {
                            break;
                        }
                    }
                });
                (Transport::Local(pty), pump)
            }
        };

        self.transport = Some(transport);
        self.status = SessionStatus::Connecting;
        // A reconnect binds every rule afresh, and may well lose ports it held
        // a moment ago to a session that grabbed them in between. Nothing from
        // the transport that just went away can arrive to say so — its pump is
        // replaced on the next line — so the slate is wiped here.
        self.tunnels.clear();
        self._pump = Some(pump);
        cx.notify();
    }

    /// Applies one SSH transport event to the session state.
    fn on_ssh_event(&mut self, event: SshEvent, cx: &mut Context<Self>) {
        // Ahead of the match, and by handing the whole event over rather than
        // by touching the list from three of the arms below: which events open
        // and close a forwarding is a rule of its own, and one worth being able
        // to assert on without a running session. The `cx.notify` at the end
        // covers the change — the strip re-reads `open_tunnels` on the next
        // frame, as it re-reads the status.
        self.tunnels.observe(&event);
        match event {
            SshEvent::Connecting => self.status = SessionStatus::Connecting,
            SshEvent::HostKey {
                algorithm,
                fingerprint,
                accepted,
            } => {
                log::debug!(
                    "{}: host key {algorithm} {fingerprint} accepted={accepted}",
                    self.label()
                );
            }
            SshEvent::Ready => self.on_transport_ready(),
            SshEvent::Data(bytes) | SshEvent::ExtendedData(bytes) => self.on_output(&bytes),
            SshEvent::ExitStatus(code) => {
                log::debug!("{}: remote shell exited with {code}", self.label());
            }
            // The tab strip is the whole report: a forwarding that came up did
            // what the user asked for, and a line in the terminal saying so
            // would push the shell's first prompt down for nothing.
            SshEvent::TunnelOpened { rule } => {
                log::debug!("{}: tunnel {rule} is open", self.label());
            }
            // Non-fatal by contract: the shell is unaffected, so the session
            // status stays as it is and nothing in the tab strip changes.
            //
            // The warning is written into the terminal instead, which is where
            // `ssh -L` puts the same complaint: it belongs next to the shell it
            // concerns, it scrolls away with the rest of the session, and it
            // reaches a user who is looking at the terminal rather than at the
            // status bar. The prefix names rulogman so the line cannot be
            // mistaken for output of the remote shell; `message` is written by
            // the transport and stays in English, like every other detail this
            // layer passes through.
            //
            // Not through `on_output`, which is the funnel for the *remote's*
            // bytes and decodes them from the session's charset: this line was
            // written here, in UTF-8, and a session on a legacy host would both
            // mangle it and lose whatever partial character its decoder was
            // holding at the time.
            SshEvent::TunnelFailed { rule, message } => {
                log::warn!("{}: tunnel {rule} failed: {message}", self.label());
                let notice = format!("\r\n\x1b[33mrulogman: tunnel {rule}: {message}\x1b[0m\r\n");
                self.terminal.feed_str(&notice);
                self.flush_terminal_replies();
            }
            SshEvent::Disconnected { reason } => {
                self.transport = None;
                self.status = SessionStatus::Disconnected { reason };
            }
            SshEvent::Error(kind, message) => {
                self.transport = None;
                self.status = SessionStatus::Failed {
                    kind: SharedString::from(kind.to_string()),
                    message,
                };
            }
        }
        cx.notify();
    }

    /// Applies one local pty event to the session state.
    ///
    /// A shell that exits is a plain disconnect rather than a failure — the
    /// user typed `exit` — and the shell that could not be started at all is
    /// the only thing the pty layer reports as an error.
    fn on_pty_event(&mut self, event: PtyEvent, cx: &mut Context<Self>) {
        match event {
            PtyEvent::Ready => self.on_transport_ready(),
            PtyEvent::Data(bytes) => self.on_output(&bytes),
            PtyEvent::Exited => {
                self.transport = None;
                self.status = SessionStatus::Disconnected {
                    reason: LOCAL_EXIT_REASON.to_owned(),
                };
            }
            PtyEvent::Error(message) => {
                self.transport = None;
                self.status = SessionStatus::Failed {
                    kind: SharedString::new_static(LOCAL_FAILURE_KIND),
                    message,
                };
            }
        }
        cx.notify();
    }

    /// The transport reports a shell on the other end.
    ///
    /// The size is pushed again because the terminal has almost certainly been
    /// laid out — and resized — while the transport was still coming up, and
    /// that resize reached a transport that had no pty yet.
    fn on_transport_ready(&mut self) {
        self.status = SessionStatus::Connected;
        let (cols, rows) = self.terminal.size();
        if let Some(transport) = &self.transport {
            transport.resize(cols, rows);
        }
    }

    /// Feeds one chunk of shell output to the emulator.
    ///
    /// A directory change needs no extra notification: both callers end in a
    /// `cx.notify` on every chunk of output anyway, so observers see the new
    /// [`Session::cwd`] on the next frame.
    fn on_output(&mut self, bytes: &[u8]) {
        let cwd_changed = self.terminal.feed(bytes);
        if cwd_changed {
            log::debug!("{}: cwd is now {:?}", self.label(), self.terminal.cwd());
        }
        self.flush_terminal_replies();
    }

    /// Writes any answer the terminal produced back to the shell.
    ///
    /// Requests such as a Device Status Report (`CSI 6 n`) or a Device
    /// Attributes query (`CSI c`) block programs like vim and tmux until the
    /// reply arrives, so this must run after every [`TerminalModel::feed`] —
    /// on a local pty just as much as on an SSH channel.
    fn flush_terminal_replies(&mut self) {
        let reply = self.terminal.take_pty_output();
        if reply.is_empty() {
            return;
        }
        if let Some(transport) = &self.transport {
            transport.send_input(reply);
        }
    }
}

/// The user's home directory, or `None` for an account that has none — the
/// pty then falls back to the application's own working directory.
fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_that_is_the_shells_own_path_folds_back_into_its_name() {
        let shell = SharedString::from("PowerShell");
        let command = vec!["powershell.exe".to_owned(), "-NoLogo".to_owned()];

        // ConPTY's default title: the full path of what was started. Stem
        // matching is what makes the install location and the extension moot.
        assert_eq!(
            local_shell_title(
                r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe",
                &shell,
                Some(&command),
            ),
            Some(SharedString::from("PowerShell"))
        );
        // Windows paths compare case-insensitively, so the title does too.
        assert_eq!(
            local_shell_title(r"C:\Tools\POWERSHELL.EXE", &shell, Some(&command)),
            Some(SharedString::from("PowerShell"))
        );

        // conhost's `path - command` form keeps what is actually running.
        let cmd = SharedString::from("cmd");
        let cmd_command = vec!["cmd.exe".to_owned()];
        assert_eq!(
            local_shell_title(
                r"C:\Windows\system32\cmd.exe - notepad",
                &cmd,
                Some(&cmd_command),
            ),
            Some(SharedString::from("cmd - notepad"))
        );
    }

    #[test]
    fn a_title_worth_showing_is_left_alone() {
        let shell = SharedString::from("PowerShell");
        let command = vec!["powershell.exe".to_owned()];

        // A directory the prompt reported, a name the user set, and a title
        // whose ` - ` tail belongs to the text rather than to conhost: none of
        // their stems name the shell's binary.
        assert_eq!(
            local_shell_title(r"C:\work\rulogman", &shell, Some(&command)),
            None
        );
        assert_eq!(
            local_shell_title("build watch", &shell, Some(&command)),
            None
        );
        assert_eq!(
            local_shell_title("project - build", &shell, Some(&command)),
            None
        );

        // Unix: no explicit command, so nothing to compare against.
        assert_eq!(local_shell_title("anything", &shell, None), None);
        assert_eq!(local_shell_title("anything", &shell, Some(&[])), None);
    }

    #[test]
    fn a_wsl_launcher_path_folds_into_the_distributions_name() {
        // A WSL tab is started through `wsl.exe`, so its default title names
        // the launcher; the tab is called after the distribution.
        let shell = SharedString::from("Ubuntu");
        let command = vec![
            "wsl.exe".to_owned(),
            "-d".to_owned(),
            "Ubuntu".to_owned(),
            "--cd".to_owned(),
            "~".to_owned(),
        ];
        assert_eq!(
            local_shell_title(r"C:\WINDOWS\system32\wsl.exe", &shell, Some(&command)),
            Some(SharedString::from("Ubuntu"))
        );
    }

    #[test]
    fn only_a_starting_or_running_session_is_live() {
        assert!(SessionStatus::Connecting.is_live());
        assert!(SessionStatus::Connected.is_live());
        assert!(
            !SessionStatus::Disconnected {
                reason: "closed by the user".to_owned()
            }
            .is_live()
        );
        assert!(
            !SessionStatus::Failed {
                kind: "authentication failed".into(),
                message: "wrong password".to_owned()
            }
            .is_live()
        );
    }

    #[test]
    fn every_status_maps_to_its_own_tab_marker() {
        assert_eq!(
            SessionStatus::Connecting.tab_status(),
            TabStatus::Connecting
        );
        assert_eq!(SessionStatus::Connected.tab_status(), TabStatus::Connected);
        assert_eq!(
            SessionStatus::Disconnected {
                reason: "the local shell exited".to_owned()
            }
            .tab_status(),
            TabStatus::Disconnected
        );
        assert_eq!(
            SessionStatus::Failed {
                kind: "local shell".into(),
                message: "could not start the local shell".to_owned()
            }
            .tab_status(),
            TabStatus::Error
        );
    }

    #[test]
    fn a_summary_carries_the_transports_own_words() {
        // The wording around them follows the locale, so only the parts that
        // come from the transport verbatim can be asserted on.
        let disconnected = SessionStatus::Disconnected {
            reason: "the local shell exited".to_owned(),
        };
        assert!(
            disconnected.summary().contains("the local shell exited"),
            "{}",
            disconnected.summary()
        );

        let failed = SessionStatus::Failed {
            kind: "local shell".into(),
            message: "could not start the local shell: No such file".to_owned(),
        };
        let summary = failed.summary();
        assert!(summary.contains("local shell"), "{summary}");
        assert!(summary.contains("No such file"), "{summary}");
    }

    #[cfg(windows)]
    #[test]
    fn the_fixed_local_shells_come_first_and_in_a_stable_order() {
        // The order is load-bearing: the connection dialog remembers the row
        // the user picked by index, and the WSL discovery replaces the list
        // under it once it answers.
        let fixed = local_shells(&[]);
        let with_wsl = local_shells(&["Ubuntu".to_owned(), "Debian".to_owned()]);

        assert_eq!(
            fixed.iter().map(|shell| &shell.name).collect::<Vec<_>>(),
            ["PowerShell", "cmd"]
        );
        assert_eq!(
            with_wsl.iter().map(|shell| &shell.name).collect::<Vec<_>>(),
            ["PowerShell", "cmd", "Ubuntu", "Debian"]
        );
        assert!(
            fixed
                .iter()
                .all(|shell| matches!(shell.filesystem, LocalFilesystem::ThisMachine))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_wsl_shell_names_its_distribution_in_both_the_command_and_the_filesystem() {
        let shells = local_shells(&["Ubuntu".to_owned()]);
        let wsl = shells.last().expect("a distribution was asked for");

        assert_eq!(
            wsl.command,
            ["wsl.exe", "-d", "Ubuntu", "--cd", "~"].map(str::to_owned)
        );
        match &wsl.filesystem {
            LocalFilesystem::Wsl { distro } => assert_eq!(distro, "Ubuntu"),
            other => panic!("a WSL shell stands in a WSL filesystem, not in {other:?}"),
        }
    }

    /// A rule as the transport names one, so the tests read like the events do.
    const A_RULE: &str = "8080:db:5432";

    /// A second one, to catch a list that only ever holds the last event.
    const ANOTHER_RULE: &str = "6379:cache:6379";

    /// What a session would answer [`Session::open_tunnels`] with.
    fn open(tunnels: &OpenTunnels) -> Vec<&str> {
        tunnels.labels.iter().map(SharedString::as_ref).collect()
    }

    /// Folds a run of events into a fresh list, as the pump would.
    fn observed(events: &[SshEvent]) -> OpenTunnels {
        let mut tunnels = OpenTunnels::default();
        for event in events {
            tunnels.observe(event);
        }
        tunnels
    }

    #[test]
    fn a_session_lists_every_forwarding_it_bound() {
        let tunnels = observed(&[
            SshEvent::TunnelOpened {
                rule: A_RULE.to_owned(),
            },
            SshEvent::TunnelOpened {
                rule: ANOTHER_RULE.to_owned(),
            },
        ]);

        assert_eq!(open(&tunnels), [A_RULE, ANOTHER_RULE]);
    }

    #[test]
    fn a_session_that_lost_the_bind_lists_nothing() {
        // A rule that lost the port to something outside rulogman — a sibling tab
        // no longer competes for it, having been told before it started not to
        // ask. The tab must stay unmarked either way: the mark says where the
        // traffic goes, and a warning is the opposite of that.
        let tunnels = observed(&[
            SshEvent::TunnelFailed {
                rule: A_RULE.to_owned(),
                message: "could not bind 127.0.0.1:8080: address in use".to_owned(),
            },
            SshEvent::Ready,
            SshEvent::Data(b"$ ".to_vec()),
        ]);

        assert!(open(&tunnels).is_empty(), "{:?}", open(&tunnels));
    }

    #[test]
    fn a_listener_that_gives_up_takes_its_mark_with_it() {
        // The one failure that names a rule already on the list: the accept
        // loop reports the same label it opened under and then closes the port.
        let tunnels = observed(&[
            SshEvent::TunnelOpened {
                rule: A_RULE.to_owned(),
            },
            SshEvent::TunnelOpened {
                rule: ANOTHER_RULE.to_owned(),
            },
            SshEvent::TunnelFailed {
                rule: A_RULE.to_owned(),
                message: "the local listener failed 16 times in a row".to_owned(),
            },
        ]);

        assert_eq!(open(&tunnels), [ANOTHER_RULE]);
    }

    #[test]
    fn a_refused_connection_leaves_the_rule_marked() {
        // A forwarding the server would not open names one *connection* of a
        // rule, not the rule: the listener is still bound, still accepting, and
        // still the reason the tab wears a mark.
        let tunnels = observed(&[
            SshEvent::TunnelOpened {
                rule: A_RULE.to_owned(),
            },
            SshEvent::TunnelFailed {
                rule: format!("{A_RULE} connection 3"),
                message: "the server refused to forward to db:5432".to_owned(),
            },
        ]);

        assert_eq!(open(&tunnels), [A_RULE]);
    }

    #[test]
    fn the_end_of_a_session_is_the_end_of_its_forwardings() {
        // Either way it ends: the listeners live on the transport's runtime, so
        // a tab that has stopped connecting anything must not go on claiming to
        // hold a port some other tab is free to take.
        for terminal in [
            SshEvent::Disconnected {
                reason: "connection closed by the remote host".to_owned(),
            },
            SshEvent::Error(
                rulogman_ssh::SshErrorKind::Io,
                "the transport went away".to_owned(),
            ),
        ] {
            let tunnels = observed(&[
                SshEvent::TunnelOpened {
                    rule: A_RULE.to_owned(),
                },
                terminal.clone(),
            ]);

            assert!(
                open(&tunnels).is_empty(),
                "{terminal:?} left {:?} behind",
                open(&tunnels)
            );
        }
    }

    #[test]
    fn a_status_with_nothing_to_quote_still_summarises() {
        // Neither of these interpolates anything, so an empty answer would mean
        // the key went missing rather than that there was nothing to say.
        assert!(!SessionStatus::Connecting.summary().is_empty());
        assert!(!SessionStatus::Connected.summary().is_empty());
    }
}
