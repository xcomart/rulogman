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
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, SharedString, Task};
use rulogman_core::{
    AuthMethod, EffectiveTerminal, HighlightRule, HopRule, SecretStore, SessionOverrides,
    SessionProfile, effective_highlights,
};
use rulogman_pty::{PtyConfig, PtyEvent, PtySession};
// The one part of the pty surface that is not cross-platform: Windows has no
// login shell to name, and picks its command from the welcome screen instead.
#[cfg(unix)]
use rulogman_pty::login_shell_name;
use rulogman_ssh::{HopSpec, SshAuth, SshConfig, SshEvent, SshSession, TunnelForward};
use rulogman_term::{Charset, TerminalModel, TerminalTheme};
use uuid::Uuid;

use crate::app_settings;
use crate::files::{FileSource, LocalSource, SftpSource};
// The one source with no counterpart on the other platform: a WSL distribution
// exists to be reached from Windows, and nowhere else.
#[cfg(windows)]
use crate::files::WslSource;
use crate::i18n::ts;
use crate::verifier::{host_key_verifier, tofu_verifier};
use rugpui::TabStatus;

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

/// Classification put on a [`SessionStatus::Failed`] raised by a jump host that
/// could not even be attempted.
///
/// The one failure this module produces on the remote side, and the reason it
/// exists at all: everything else that can go wrong with an SSH session goes
/// wrong *inside* the transport, which names the stage itself. A hop whose
/// credential is missing or whose method rulogman cannot speak is refused
/// before a socket is opened, so there is no transport to name it — see
/// [`hop_specs`].
const HOP_FAILURE_KIND: &str = "jump host";

/// How many lines of a followed file a tail session asks for up front.
///
/// Enough to fill a tall pane and give the first screen some context, few
/// enough that a multi-gigabyte log does not spend a second being read before
/// the first live line arrives. `tail` counts these from the end of the file,
/// so the cost is the same whatever the file's size.
const TAIL_BACKLOG_LINES: u32 = 200;

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
        /// The remote file this session follows, or `None` for a shell.
        ///
        /// One field for the three things a followed file changes, because all
        /// three are the same fact seen from different sides: the channel runs
        /// [`tail_command`] instead of the login shell, the tab is named after
        /// the file rather than after the profile, and there is no shell on the
        /// other end to browse a filesystem with or to type at. Keeping the
        /// *path* rather than the finished command line is what lets the title
        /// be built from it too, and it is what a reconnect rebuilds the
        /// command from — see [`Session::start`].
        tail: Option<String>,
        /// Remote directory the shell is started in; `None` means wherever the
        /// login shell puts the user, which is their home.
        ///
        /// Set by [`Session::duplicate`], so that a split of a remote pane
        /// opens where the original shell said it was, and kept — as
        /// [`Target::Local::cwd`] is — so that a reconnect lands in the same
        /// place the session originally started in. Never set beside a
        /// [`Target::Ssh::tail`], whose command has no shell to start anywhere.
        start_dir: Option<String>,
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
        ///
        /// Always a directory on *this* machine. A WSL shell's own directory is
        /// a Linux path the pty could not enter, and is handed to `wsl.exe` on
        /// its command line instead — see [`with_wsl_start_dir`].
        cwd: Option<PathBuf>,
        /// The command line to run, or `None` for the platform's default.
        ///
        /// `None` is what a unix session nearly always uses: the pty starts the
        /// user's login shell, which is the only local shell that platform
        /// offers to *choose*. The exception is a launch that named a command
        /// — `rulogman -e btop`, the form a desktop's *Run in terminal* uses —
        /// which runs that in the shell's place. Windows has several shells —
        /// PowerShell, `cmd`, one per installed WSL distribution — so the
        /// welcome screen names the one it wants here, and a reconnect or a
        /// duplicate starts that same one again.
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

impl LocalFilesystem {
    /// Variables a shell standing in this filesystem is started with, on top of
    /// the environment it inherits.
    ///
    /// Nothing for a shell on this machine. A WSL one is handed the prompt hook
    /// [`wsl_env`] describes.
    fn shell_env(&self) -> Vec<(String, String)> {
        match self {
            Self::ThisMachine => Vec::new(),
            #[cfg(windows)]
            Self::Wsl { .. } => wsl_env(std::env::var("WSLENV").ok().as_deref()),
        }
    }
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
    // of it. `-NoExit` keeps the shell at its prompt once the hook that
    // `-EncodedCommand` installs has run; see [`POWERSHELL_PROMPT_HOOK`].
    let mut shells = vec![
        LocalShell {
            name: "PowerShell".into(),
            command: vec![
                "powershell.exe".to_owned(),
                "-NoLogo".to_owned(),
                "-NoExit".to_owned(),
                "-EncodedCommand".to_owned(),
                encoded_command(POWERSHELL_PROMPT_HOOK),
            ],
            filesystem: LocalFilesystem::ThisMachine,
        },
        LocalShell {
            name: "cmd".into(),
            command: vec![
                "cmd.exe".to_owned(),
                "/k".to_owned(),
                "prompt".to_owned(),
                CMD_PROMPT.to_owned(),
            ],
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

/// The script a PowerShell tab runs before its first prompt, so that it reports
/// its directory over `OSC 1337` the way a configured unix prompt does over
/// `OSC 7`.
///
/// Out of the box PowerShell announces nothing, and Windows offers no cheap way
/// to ask where a shell is standing — see [`PtySession::current_dir`] — so
/// without this a split of a PowerShell pane opened in the home directory
/// whatever the original had `cd`'d into. Asking the process would not help
/// anyway: `Set-Location` moves PowerShell's own location, not the process's
/// working directory.
///
/// The prompt the profile left behind is wrapped rather than replaced, and is
/// called first so that it still sees the `$?` of the user's last command. Only
/// a location on the filesystem is reported: a registry or certificate drive
/// names nothing a new shell could start in. `OSC 1337` rather than `OSC 7`
/// because it carries the path as PowerShell spells it, with no URL to encode.
#[cfg_attr(not(windows), allow(dead_code))]
const POWERSHELL_PROMPT_HOOK: &str = "\
$global:__rulogman_prompt = $function:prompt
function global:prompt {
    $prompt = & $global:__rulogman_prompt
    $location = $executionContext.SessionState.Path.CurrentLocation
    if ($location.Provider.Name -ne 'FileSystem') { return $prompt }
    \"$([char]27)]1337;CurrentDir=$($location.ProviderPath)$([char]7)$prompt\"
}
";

/// The prompt a `cmd` tab runs under: the one it would have had, behind an
/// `OSC 1337` that reports the directory `$P` names — the same report, for the
/// same reason, as [`POWERSHELL_PROMPT_HOOK`].
///
/// `%PROMPT%` is expanded by `cmd` itself as it runs the `/k` command, which
/// keeps a `PROMPT` the user set; `cmd` defines it as `$P$G` when nobody did.
/// Free of spaces and quotes, so it reaches `cmd` exactly as written, with no
/// quoting for `/k` to strip.
#[cfg_attr(not(windows), allow(dead_code))]
const CMD_PROMPT: &str = r"$E]1337;CurrentDir=$P$E\%PROMPT%";

/// What a WSL tab's `bash` runs before each prompt: an `OSC 7` naming the
/// directory it is standing in, so that a split of the tab opens there — the
/// same report, for the same reason, as [`POWERSHELL_PROMPT_HOOK`].
///
/// A function rather than a bare `printf`, because `PROMPT_COMMAND` runs before
/// the prompt is drawn and a `PS1` that shows the last command's status would
/// otherwise show the `printf`'s; the function hands that status back. `%` is
/// the one character spelt out, as the URL needs it to be: the rest of the path
/// reaches the terminal as it is, which is how the terminal reads it.
#[cfg(any(windows, test))]
const WSL_PROMPT_COMMAND: &str = r#"__rulogman_osc7() { local status=$?; printf '\e]7;file://%s%s\a' "$HOSTNAME" "${PWD//\%/%25}"; return $status; }; __rulogman_osc7"#;

/// The environment that hands [`WSL_PROMPT_COMMAND`] to a WSL tab's shell,
/// given the `WSLENV` this process inherited.
///
/// A Windows variable reaches the Linux side only when `WSLENV` names it, so
/// `PROMPT_COMMAND` is added to whatever list is already there — `/u`, this
/// way across only — and an entry for it already on the list is replaced
/// rather than doubled. Through the environment rather than the command line
/// because `wsl.exe` starts the user's own login shell, whose startup files
/// are theirs: a `bash` that leaves `PROMPT_COMMAND` alone, as the stock
/// `.bashrc` of every common distribution does, reports its directory with no
/// setup at all. A shell that does not read `PROMPT_COMMAND` — `zsh`, `fish` —
/// reports nothing unless its own prompt does, and a split of it opens at home
/// as before.
#[cfg(any(windows, test))]
fn wsl_env(inherited: Option<&str>) -> Vec<(String, String)> {
    const FORWARDED: &str = "PROMPT_COMMAND/u";
    let mut wslenv: Vec<&str> = inherited
        .unwrap_or_default()
        .split(':')
        .filter(|entry| {
            let name = entry.split('/').next().unwrap_or_default();
            !name.is_empty() && name != "PROMPT_COMMAND"
        })
        .collect();
    wslenv.push(FORWARDED);
    vec![
        ("PROMPT_COMMAND".to_owned(), WSL_PROMPT_COMMAND.to_owned()),
        ("WSLENV".to_owned(), wslenv.join(":")),
    ]
}

/// `script` in the form PowerShell's `-EncodedCommand` takes: UTF-16LE, then
/// base64.
///
/// Encoded rather than passed to `-Command`, because a script on the command
/// line goes through the argument quoting rules of Windows and then through
/// PowerShell's own, and the quotes the script needs would have to survive
/// both.
#[cfg_attr(not(windows), allow(dead_code))]
fn encoded_command(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |group, (i, &byte)| {
            group | u32::from(byte) << (16 - 8 * i)
        });
        for i in 0..4 {
            if i <= chunk.len() {
                encoded.push(char::from(
                    ALPHABET[(group >> (18 - 6 * i) & 0x3f) as usize],
                ));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
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
/// By hand rather than through
/// [`Path::file_stem`](std::path::Path::file_stem), because the paths in
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

/// A host key the verifier refused because another key is on record for the
/// same host and port.
///
/// Everything the connection overlay needs to show the user what changed and
/// to offer the one way past it: the record's identity — host and port, which
/// may be a jump host's rather than the target's — and both keys, so the
/// fingerprints can be compared with what the server's administrator says.
/// No secret is in here; a host key is public by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyMismatch {
    /// The host the key was presented by, as the connection named it.
    pub host: String,
    /// The port it was presented on.
    pub port: u16,
    /// Algorithm of the key on record, e.g. `ssh-ed25519`.
    pub stored_algorithm: String,
    /// Fingerprint of the key on record, `SHA256:...`.
    pub stored_fingerprint: String,
    /// Algorithm of the key the server presented this time.
    pub presented_algorithm: String,
    /// Fingerprint of the key the server presented this time.
    pub presented_fingerprint: String,
}

impl HostKeyMismatch {
    /// The record's address as a sentence names it: `host:port`, the port
    /// always written, because the record is kept per port.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// The changed host key a failed connection can be recovered from, if any.
///
/// Folds transport events the way [`OpenTunnels`] does, and for the same
/// reason: the rule — which events raise the offer and which withdraw it — can
/// be asserted without a session or a gpui app. A rejected
/// [`SshEvent::HostKey`] raises it, provided the policy names a record the key
/// collided with; the [`SshErrorKind::HostKeyRejected`](rulogman_ssh::SshErrorKind::HostKeyRejected)
/// that follows keeps it; anything else that starts or ends a connection takes
/// it down, so the offer can only ever stand beside the failure it explains.
#[derive(Debug, Default)]
struct HostKeyRecovery {
    /// The mismatch the last connection failed on.
    mismatch: Option<HostKeyMismatch>,
}

impl HostKeyRecovery {
    /// Folds one transport event in.
    ///
    /// `recorded` answers what a rejected key collided with, as
    /// `(stored_algorithm, stored_fingerprint)` —
    /// [`TofuVerifier::recorded_key`](crate::verifier::TofuVerifier::recorded_key)
    /// in the application. It is only asked about a rejection, and a `None`
    /// from it leaves nothing to offer: a key refused for any other reason is
    /// not one that removing a record would let through.
    fn observe(
        &mut self,
        event: &SshEvent,
        recorded: impl FnOnce(&str, u16, &str, &str) -> Option<(String, String)>,
    ) {
        match event {
            SshEvent::HostKey {
                host,
                port,
                algorithm,
                fingerprint,
                accepted: false,
            } => {
                self.mismatch = recorded(host, *port, algorithm, fingerprint).map(
                    |(stored_algorithm, stored_fingerprint)| HostKeyMismatch {
                        host: host.clone(),
                        port: *port,
                        stored_algorithm,
                        stored_fingerprint,
                        presented_algorithm: algorithm.clone(),
                        presented_fingerprint: fingerprint.clone(),
                    },
                );
            }
            // The failure the rejection causes; the offer is for exactly this.
            SshEvent::Error(rulogman_ssh::SshErrorKind::HostKeyRejected, _) => {}
            // A key accepted — on this leg or on a hop before it — a fresh
            // attempt, or an end that is not a rejection: nothing left to fix.
            SshEvent::HostKey { accepted: true, .. }
            | SshEvent::Connecting
            | SshEvent::Ready
            | SshEvent::Disconnected { .. }
            | SshEvent::Error(..) => self.mismatch = None,
            _ => {}
        }
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
    /// The changed host key the last connection failed on, while the failure
    /// stands; see [`Session::host_key_mismatch`].
    host_key: HostKeyRecovery,
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
    /// Whether [`Session::send_input`] must throw the user's keystrokes away.
    ///
    /// Set for — and only for — a session that follows a file. There is no
    /// shell on the other end of one: the channel carries a `tail -F`, which
    /// reads nothing from its standard input and would echo every typed
    /// character back through the pty as though it had been prompted for. Worse,
    /// two of the keys a terminal user reaches for without thinking would end
    /// the session outright, `Ctrl+C` and `Ctrl+D` both being delivered to that
    /// command and to nothing else.
    ///
    /// A flag on the session rather than a rule the view keeps, because the
    /// view is not the only way in: a paste, a drag-and-drop of text and the
    /// terminal's own replies all arrive here, and the one place that can
    /// refuse them all is the one place they all pass through. Resizing is
    /// deliberately *not* covered — a `window-change` is not input, and a
    /// followed file has to reflow with its pane like anything else.
    input_locked: bool,
    /// Task draining the transport's event stream; dropping it stops the pump.
    _pump: Option<Task<()>>,
    /// Task reading the jump hosts' stored secrets before the transport can be
    /// opened; dropping it abandons the connect. See [`Session::start`].
    _credentials: Option<Task<()>>,
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
        Self::new_in(profile, auth, None, tunnels_suppressed, cx)
    }

    /// [`Session::new`], with the shell started in `start_dir` on the remote
    /// host rather than in the user's home there.
    ///
    /// Only [`Session::duplicate`] has a directory to give: a saved connection
    /// is opened where the server puts it, and nothing in a profile says
    /// otherwise.
    fn new_in(
        profile: SessionProfile,
        auth: SshAuth,
        start_dir: Option<String>,
        tunnels_suppressed: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let overrides = profile.overrides.clone();
        let mut session = Self::build(
            Target::Ssh {
                profile: Box::new(profile),
                auth,
                tail: None,
                start_dir,
            },
            overrides,
            cx,
        );
        session.tunnels_suppressed = tunnels_suppressed;
        session.start(cx);
        session
    }

    /// Builds a session that follows `tail_path` on `profile`'s host, and starts
    /// connecting straight away.
    ///
    /// The same session [`Session::new`] builds, with the login shell replaced
    /// by a `tail -F` on one file — see [`tail_command`]. It is deliberately not
    /// a second type: a followed file is a connection like any other, and every
    /// tab, pane, status dot and reconnect in the shell is written against
    /// `Entity<Session>`. What differs is written down in [`Target::Ssh::tail`]
    /// and in the three answers that read it — [`Session::title`],
    /// [`Session::files`] and [`Session::send_input`].
    ///
    /// `tunnels_suppressed` means what it means in [`Session::new`], and the
    /// caller has one sensible answer for it: a followed file is opened
    /// *beside* a shell on the same profile, and two sessions asking for one
    /// profile's local ports is the very rivalry that flag exists to settle.
    /// Handing a tail session the forwardings would take them from the shell
    /// tab that is holding them, or print a failure notice per rule over a
    /// pane that has nothing to do with any of them.
    pub fn new_tail(
        profile: SessionProfile,
        auth: SshAuth,
        tail_path: String,
        tunnels_suppressed: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let overrides = profile.overrides.clone();
        let mut session = Self::build(
            Target::Ssh {
                profile: Box::new(profile),
                auth,
                tail: Some(tail_path),
                start_dir: None,
            },
            overrides,
            cx,
        );
        session.tunnels_suppressed = tunnels_suppressed;
        // Before `start`, not after: the very first thing a pty can carry is
        // the terminal's own answer to a device query, and a session that is
        // not to be typed at must be locked before anything can be typed.
        session.input_locked = true;
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

    /// Builds a session running the user's login shell in `cwd`, and starts it
    /// straight away.
    ///
    /// [`Session::new_local`] with somewhere to start, which is what a path on
    /// the command line asks for — `rulogman /var/log`, or a folder handed over
    /// by a file manager's *Open with*. Nothing else about the session differs:
    /// the shell is still the login shell, and the directory is a starting
    /// point rather than a property of the session, since the first `cd` the
    /// user types leaves it behind.
    #[cfg(unix)]
    pub fn new_local_at(cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        Self::new_local_in(
            SharedString::from(login_shell_name()),
            Some(cwd),
            None,
            LocalFilesystem::ThisMachine,
            cx,
        )
    }

    /// Builds a session running `command` in `cwd` in place of the login
    /// shell, and starts it straight away.
    ///
    /// The launch path for a desktop's *Run in terminal*, which reaches
    /// rulogman as `rulogman -e <command…>` and nothing else. `label` names the
    /// tab until the program sets a title of its own, and `cwd` is where the
    /// launcher left this process — `None` meaning the user's home, which is
    /// what [`Session::new_local_in`] falls back to for every local session.
    ///
    /// The command's filesystem is always this machine's: unix has no local
    /// shell standing anywhere else, which is why — unlike the Windows
    /// constructors of the same name — this takes no `filesystem`.
    ///
    /// Nothing restarts when the command exits. The tab goes to the state every
    /// local session that ends goes to, holding what the program printed, which
    /// is what konsole's `--noclose` is for and the only useful answer for a
    /// command that failed on the first line.
    #[cfg(unix)]
    pub fn new_local_command_at(
        label: SharedString,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_local_in(label, cwd, Some(command), LocalFilesystem::ThisMachine, cx)
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

    /// Builds a session running `command` on this machine in `cwd`, and starts
    /// it straight away.
    ///
    /// [`Session::new_local_command`] with somewhere to start, and the Windows
    /// counterpart of [`Session::new_local_at`]: a path on the command line —
    /// `rulogman C:\logs`, or a folder opened with rulogman from Explorer —
    /// names a directory but not a shell, so the caller picks the shell the
    /// same way the welcome screen does.
    #[cfg(windows)]
    pub fn new_local_command_at(
        label: SharedString,
        command: Vec<String>,
        filesystem: LocalFilesystem,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_local_in(label, Some(cwd), Some(command), filesystem, cx)
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

    /// A session with a terminal and no shell behind it, for the tests of the
    /// views that hold one.
    ///
    /// Every public constructor starts something before it returns — a pty, a
    /// TCP connection — which a test of a *view* has no use for and no way to
    /// stop. What a view reads off a session is its label, its colour scheme and
    /// its font, and none of the three needs anything on the other end, so this
    /// is [`Session::build`] and no [`Session::start`].
    ///
    /// Test-only rather than a general "detached session": a session that never
    /// connects is a state the application has no business being able to reach,
    /// and the [`SessionStatus::Connecting`] it sits in for ever would be a lie
    /// anywhere a user could see it.
    #[cfg(test)]
    pub(crate) fn dormant(cx: &mut Context<Self>) -> Self {
        Self::build(
            Target::Local {
                shell: SharedString::from("test"),
                cwd: None,
                command: None,
                filesystem: LocalFilesystem::ThisMachine,
            },
            SessionOverrides::default(),
            cx,
        )
    }

    /// The remote counterpart of [`Session::dormant`]: a session that came from
    /// `profile` and never dialled the host it names.
    ///
    /// The same trick and the same reasoning, for the tests that need a session
    /// [`Session::is_local`] answers `false` for — which is every test about a
    /// rule the two kinds of session are judged by differently, the file panel's
    /// opening state above all. It carries the profile's overrides, because that
    /// is what [`Session::new`] does with them and a dormant session that
    /// disagreed about its own font would be a poor stand-in.
    ///
    /// A password nobody will ever send: [`Target::Ssh`] holds credentials for a
    /// reconnect that cannot happen here, and an empty one keeps the constructor
    /// down to the one thing a test has to say — which host.
    #[cfg(test)]
    pub(crate) fn dormant_remote(profile: SessionProfile, cx: &mut Context<Self>) -> Self {
        let overrides = profile.overrides.clone();
        Self::build(
            Target::Ssh {
                profile: Box::new(profile),
                auth: SshAuth::Password(String::new()),
                tail: None,
                start_dir: None,
            },
            overrides,
            cx,
        )
    }

    /// The followed-file counterpart of [`Session::dormant_remote`]: a session
    /// that would have run [`tail_command`] on `path`, and never dialled
    /// anything.
    ///
    /// Here for the same reason its sibling is, and for one more: a tail session
    /// answers four questions differently from a shell on the same profile —
    /// its title, its label, its file panel and whether it may be typed at —
    /// and every one of those answers is reachable without a transport.
    #[cfg(test)]
    pub(crate) fn dormant_tail(
        profile: SessionProfile,
        path: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let overrides = profile.overrides.clone();
        let mut session = Self::build(
            Target::Ssh {
                profile: Box::new(profile),
                auth: SshAuth::Password(String::new()),
                tail: Some(path),
                start_dir: None,
            },
            overrides,
            cx,
        );
        session.input_locked = true;
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
            host_key: HostKeyRecovery::default(),
            // The default for both constructors: a local session has no
            // forwardings to suppress, and the SSH one sets this from its
            // caller's answer before it starts.
            tunnels_suppressed: false,
            // Likewise the default: a shell is there to be typed at, and only
            // [`Session::new_tail`] takes that away.
            input_locked: false,
            _pump: None,
            _credentials: None,
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

    /// The highlight rules this session's pane should colour its output by, or
    /// `None` when there is nothing to colour.
    ///
    /// `None` for a shell — highlighting is a property of a *followed file*,
    /// and a rule list applied to an interactive shell would recolour the
    /// user's own prompt. For a tail it is the resolution
    /// [`effective_highlights`] describes, with the per-file list read off the
    /// [`TailRule`](rulogman_core::TailRule) naming the very path this session
    /// follows; a `Some(empty)` answer is highlighting deliberately switched
    /// off for this file, and stays distinct from the `None` above it.
    ///
    /// The profile read here is the copy the session was *opened with*, exactly
    /// as the tunnels are: editing a file's rules therefore reaches the panes
    /// opened after the edit, and the panes already on screen keep the rules
    /// they started with until they are reopened. The global list is not like
    /// that — it is re-read from the live settings on every call, so
    /// [`crate::workspace::Workspace::apply_settings`] pushes a change to it into every
    /// pane at once.
    ///
    /// Nothing here resolves a colour: a rule names a *slot* of the colour
    /// scheme (see [`rulogman_core::highlight_preset`], which is written
    /// entirely in slots so that a preset rule is legible over any scheme), and
    /// which colour that slot is depends on the scheme the session is running —
    /// which is why the renderer resolves it against the palette of the frame
    /// it is painting rather than this doing it once here.
    pub fn highlight_rules(&self, cx: &App) -> Option<Vec<HighlightRule>> {
        let Target::Ssh {
            profile,
            tail: Some(path),
            ..
        } = &self.target
        else {
            return None;
        };
        let per_file = profile
            .tails
            .iter()
            .find(|rule| &rule.path == path)
            .and_then(|rule| rule.highlights.as_deref());
        Some(effective_highlights(&app_settings::current(cx).highlights, per_file).into_owned())
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

    /// The changed host key this session's connection was refused over, while
    /// that refusal is what the session is showing.
    ///
    /// `Some` only on a [`SessionStatus::Failed`] that the transport reported
    /// as a host key rejection, and only when the policy could name the record
    /// the key collided with — see [`HostKeyRecovery`]. That is the one
    /// failure the user can resolve from here, by
    /// [`Session::forget_host_key`], and the overlay offers it exactly while
    /// this answers.
    pub fn host_key_mismatch(&self) -> Option<&HostKeyMismatch> {
        match self.status {
            SessionStatus::Failed { .. } => self.host_key.mismatch.as_ref(),
            _ => None,
        }
    }

    /// Removes the record [`Session::host_key_mismatch`] names from
    /// `known_hosts`, so that the next connect trusts the host's key afresh.
    ///
    /// Only the record goes; nothing reconnects here. The caller reconnects
    /// once the task answers `Ok`, through the workspace like every other
    /// reconnect, and the transport's verifier then finds the host unknown and
    /// records whatever key it presents — trust on first use, exactly as on the
    /// very first connect. Every algorithm on record for the host and port
    /// goes, see [`TofuVerifier::forget`](crate::verifier::TofuVerifier::forget).
    ///
    /// The file is read and written on the background executor: it is small,
    /// but it is a disk on the far side of a click, and the UI thread waits on
    /// nothing it does not have to. `None` when there is nothing to forget.
    pub fn forget_host_key(&self, cx: &App) -> Option<Task<anyhow::Result<()>>> {
        let mismatch = self.host_key_mismatch()?;
        let (host, port) = (mismatch.host.clone(), mismatch.port);
        log::warn!(
            "{}: forgetting the recorded host key of {host}:{port} ({} {}) at the user's request",
            self.label(),
            mismatch.stored_algorithm,
            mismatch.stored_fingerprint
        );
        Some(
            cx.background_executor()
                .spawn(async move { tofu_verifier().forget(&host, port) }),
        )
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

    /// The remote file this session is following, or `None` for a shell.
    ///
    /// Exposed for the one thing outside this module that has to build a
    /// *pane* for a session it did not open: duplicating a tab, which asks
    /// [`Session::duplicate`] for a second session and then has to know which
    /// kind of pane to put it in. Everything else a followed file changes is
    /// answered by the session itself.
    pub fn tail_path(&self) -> Option<&str> {
        match &self.target {
            Target::Ssh { tail, .. } => tail.as_deref(),
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
    ///
    /// A followed file names itself the same way it titles its tab — the file,
    /// then what it is on — because everything that prints this prints it about
    /// a *session*, and two panes on one host that differ only in which of them
    /// is a shell would otherwise be labelled identically. The connection
    /// overlay reads well either way: "Connecting to /var/log/syslog -
    /// alice@web-01" says the same thing the tab says, one host longer.
    pub fn label(&self) -> SharedString {
        match &self.target {
            Target::Ssh {
                profile,
                tail: Some(path),
                ..
            } => crate::editor_tab_label(path, &profile.label()),
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
        // Ahead of the reported title, and unconditionally: a followed file is
        // named after the file, and what scrolls through the pane is a log
        // rather than a shell. A log line is under nobody's control — it can
        // carry an `OSC 0` from a program that wrote its own output into it —
        // so a title honoured here would let a followed file rename its own
        // tab to anything at all.
        if let Target::Ssh {
            profile,
            tail: Some(path),
            ..
        } = &self.target
        {
            return crate::editor_tab_label(remote_file_name(path), &profile.name);
        }
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
        // A followed file has no filesystem to offer, for the reason given at
        // the top of this method: the panel shows what the session is looking
        // at, and this session is looking at one file. SFTP would answer
        // perfectly well — it is a service of the connection, not of the shell
        // — and that is exactly the trap: a browser beside a `tail` would
        // invite the user to open a *second* file in a pane that has no way to
        // show one, and to type at a session that refuses input. Answering
        // `None` here rather than closing the panel from the workspace keeps
        // the rule with the session it is a fact about; the tail tab opens with
        // the panel shut as well, which is what the user actually sees.
        if matches!(&self.target, Target::Ssh { tail: Some(_), .. }) {
            return None;
        }
        match (&self.status, &self.transport) {
            (SessionStatus::Connected, Some(Transport::Ssh(ssh))) => {
                // Both riders on the one session: files move over SFTP, and the
                // editor's elevated save runs `sudo` over the exec channel.
                // Neither call opens anything — see [`SftpSource::new`] — which
                // is what keeps this cheap enough to run per notification.
                //
                // The `Arc` holds something that is deliberately not `Sync`:
                // [`FileSource`]'s futures are `?Send` and are polled on the UI
                // thread, which is why that source caches what it knows in a
                // `RefCell` rather than behind a lock. The pointer is an `Arc`
                // all the same because it is the trait object's own container
                // — every source in the panel is held as one — and not because
                // anything here crosses a thread.
                #[allow(clippy::arc_with_non_send_sync)]
                Some(Arc::new(SftpSource::new(ssh.sftp(), ssh.exec())))
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
        // Dropped rather than refused out loud: see [`Session::input_locked`].
        // Not even the scroll-to-bottom below, which is the one visible thing a
        // keystroke does here — a viewport the user has scrolled back through
        // must not be yanked to the live end by a key that did nothing.
        if self.input_locked {
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
        // A connect still waiting on the keychain is abandoned with it.
        self._credentials = None;
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
    /// A duplicate of a shell starts in the directory the original shell is
    /// in, when that can be found out — see [`Session::duplicate_target`].
    /// Splitting a terminal and landing in the same place is what every other
    /// terminal does.
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
        match self.duplicate_target() {
            // The file comes along with the credentials: a duplicate of a
            // followed file is a second follower of that same file, not a
            // shell on the host it happens to live on.
            Target::Ssh {
                profile,
                auth,
                tail: Some(path),
                ..
            } => cx.new(|cx| Self::new_tail(*profile, auth, path, tunnels_suppressed, cx)),
            Target::Ssh {
                profile,
                auth,
                tail: None,
                start_dir,
            } => cx.new(|cx| Self::new_in(*profile, auth, start_dir, tunnels_suppressed, cx)),
            Target::Local {
                shell,
                cwd,
                command,
                filesystem,
            } => cx.new(|cx| Self::new_local_in(shell, cwd, command, filesystem, cx)),
        }
    }

    /// What [`Session::duplicate`] connects to: this session's own target,
    /// with the directory the shell is standing in now as the place to start.
    ///
    /// Separate from the call that starts it so that what a split *asks for* —
    /// the directory above all — can be checked without a pty or a server on
    /// the other end.
    ///
    /// * **A local shell** starts in [`Session::local_start_dir`]. The command
    ///   comes along with the directory: a duplicate of a WSL tab has to open
    ///   that same distribution, not the default shell of the platform — and in
    ///   the distribution's own directory, which only `wsl.exe` can enter.
    /// * **A remote shell** starts in [`Session::remote_start_dir`].
    /// * **A followed file** is a second follower of the same file, and has no
    ///   shell to start anywhere.
    fn duplicate_target(&self) -> Target {
        match &self.target {
            Target::Ssh {
                profile,
                auth,
                tail,
                ..
            } => Target::Ssh {
                profile: profile.clone(),
                auth: auth.clone(),
                tail: tail.clone(),
                start_dir: match tail {
                    Some(_) => None,
                    None => self.remote_start_dir(),
                },
            },
            Target::Local {
                shell,
                command,
                filesystem,
                ..
            } => {
                #[cfg(windows)]
                if let (LocalFilesystem::Wsl { distro }, Some(command)) = (filesystem, command)
                    && let Some(dir) = self.wsl_start_dir(distro)
                {
                    return Target::Local {
                        shell: shell.clone(),
                        cwd: home_dir(),
                        command: Some(with_wsl_start_dir(command, &dir)),
                        filesystem: filesystem.clone(),
                    };
                }
                Target::Local {
                    shell: shell.clone(),
                    cwd: self.local_start_dir().or_else(home_dir),
                    command: command.clone(),
                    filesystem: filesystem.clone(),
                }
            }
        }
    }

    /// How the session should be rendered in the tab strip.
    pub fn tab_status(&self) -> TabStatus {
        self.status.tab_status()
    }

    /// The directory a duplicate of this local session should start in.
    ///
    /// The shell's own report comes first — an `OSC 7` from a configured
    /// prompt, which is the directory the user sees in that prompt. Failing
    /// that, the operating system is asked where the program in the
    /// foreground of the pty is standing, which answers for every shell that
    /// announces nothing; see [`PtySession::current_dir`]. Both go through
    /// [`local_start_dir`], which keeps only a directory that exists now.
    ///
    /// The report is passed over when it came from a shell on another machine
    /// — an `ssh` typed at this one's prompt, see
    /// [`TerminalModel::cwd_on_first_host`]. Its path names a directory over
    /// there; the process in the foreground here is that `ssh`, and its
    /// directory is the one the user left this shell in.
    ///
    /// The same tests also do the work no third one has to on Windows: a WSL
    /// shell reports a Linux path such as `/home/ada`, which `is_absolute`
    /// rejects there, and the process behind the pty is `wsl.exe`, whose
    /// directory is not asked for. A WSL duplicate is given its directory by
    /// [`Session::wsl_start_dir`] instead.
    fn local_start_dir(&self) -> Option<PathBuf> {
        let reported = self.cwd().filter(|_| self.terminal.cwd_on_first_host());
        local_start_dir(reported, || match &self.transport {
            Some(Transport::Local(pty)) => pty.current_dir(),
            _ => None,
        })
    }

    /// The remote directory a duplicate of this SSH shell should start in.
    ///
    /// Only ever the shell's own report: there is no process on this machine
    /// to ask, and nothing on the server is asked either, since a split is
    /// opened on the spot. A report from a shell nested inside this one —
    /// `ssh` to a third host typed at the prompt — names a directory on that
    /// third host and is passed over; see [`remote_start_dir`] for what else
    /// is.
    fn remote_start_dir(&self) -> Option<String> {
        if !self.terminal.cwd_on_first_host() {
            return None;
        }
        remote_start_dir(self.cwd()?)
    }

    /// The Linux directory a duplicate of this WSL tab should start in.
    ///
    /// The shell's report, under the rules a remote one is held to, and then
    /// looked up through the distribution's share: a `--cd` into a directory
    /// that is not there is not one to count on `wsl.exe` shrugging off, so one
    /// removed since the report is passed over and the duplicate opens at home
    /// like a fresh tab.
    #[cfg(windows)]
    fn wsl_start_dir(&self, distro: &str) -> Option<String> {
        self.remote_start_dir()
            .filter(|dir| crate::files::wsl_is_dir(distro, dir))
    }

    /// Opens the transport and spawns the event pump — once the jump hosts'
    /// stored secrets are in hand.
    ///
    /// Settings are read here rather than only in the constructor so that a
    /// reconnect naturally picks up a scheme, `TERM` or timeout changed since the
    /// session was first opened.
    ///
    /// A hop that keeps its password or passphrase in the keychain has it read
    /// first, on the background executor, and the transport is opened when the
    /// answer is back — see [`Session::connect`]. The keychain is a synchronous
    /// platform call that can sit behind an unlock prompt for as long as the
    /// user takes to answer it — the Secret Service and the macOS keychain both
    /// raise one — and the UI thread must not wait on it: every other tab,
    /// and the prompt's own window, would freeze with it. The session shows as
    /// *connecting* meanwhile, which is what it is. A route with no stored
    /// secret on it, which is nearly every one, connects at once.
    fn start(&mut self, cx: &mut Context<Self>) {
        let effective = app_settings::current(cx).effective_terminal(&self.overrides);
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

        // Whatever an earlier start was still waiting for is moot now.
        self._credentials = None;
        let hops = match &self.target {
            Target::Ssh { profile, .. } => profile.hops.clone(),
            Target::Local { .. } => Vec::new(),
        };
        if !hops.iter().any(keeps_secret) {
            self.connect(Vec::new(), cx);
            return;
        }

        // What a starting session looks like from the outside, set now rather
        // than when the transport exists: the old transport is gone, and the
        // overlay has nothing better to say than *connecting*.
        self.transport = None;
        self._pump = None;
        self.tunnels.clear();
        self.host_key = HostKeyRecovery::default();
        self.status = SessionStatus::Connecting;
        self._credentials = Some(cx.spawn(async move |this, cx| {
            let secrets = cx
                .background_executor()
                .spawn(async move { hop_secrets(&hops) })
                .await;
            // A session closed in the meantime took this task with it, and a
            // restart replaced it; either way nothing arrives here then.
            let _ = this.update(cx, |session, cx| session.connect(secrets, cx));
        }));
        cx.notify();
    }

    /// The second half of [`Session::start`]: resolves the route with the
    /// hops' stored secrets — one per hop, in order, as [`hop_secrets`] read
    /// them — and opens the transport.
    fn connect(&mut self, secrets: Vec<Option<String>>, cx: &mut Context<Self>) {
        let settings = app_settings::current(cx);
        let effective = settings.effective_terminal(&self.overrides);
        let (cols, rows) = self.terminal.size();

        // Resolved ahead of everything else, because it is the one part of an
        // SSH configuration that can fail *here* rather than out on the
        // network: a jump host whose credential is missing, or whose method
        // rulogman cannot speak, can never be attempted, and the transport
        // would have nothing better to say about it than this does.
        //
        // Split in two so that the failure is handled with `self.target` no
        // longer borrowed — the arms below have to write the very fields a
        // failure sets.
        let hops = match &self.target {
            Target::Ssh { profile, .. } => hop_specs(&profile.hops, &secrets),
            Target::Local { .. } => Ok(Vec::new()),
        };
        let hops = match hops {
            Ok(hops) => hops,
            Err(message) => {
                log::warn!("{}: {message}", self.label());
                self.transport = None;
                // Exactly what a connection that failed out on the network
                // leaves behind, because from every side but this one it is the
                // same event: no transport, no pump, no forwardings, and a
                // reason on the status for the overlay to print and the
                // *Reconnect* button to be offered under.
                self.tunnels.clear();
                self.host_key = HostKeyRecovery::default();
                self._pump = None;
                self.status = SessionStatus::Failed {
                    kind: SharedString::new_static(HOP_FAILURE_KIND),
                    message,
                };
                cx.notify();
                return;
            }
        };

        // The transport and its pump are built first and installed afterwards:
        // each arm reads out of `self.target`, which rules out touching the
        // fields below in the same breath.
        let (transport, pump) = match &self.target {
            Target::Ssh {
                profile,
                auth,
                tail,
                start_dir,
            } => {
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
                // Every session the profile opens goes the same way round, a
                // followed file as much as a shell: the hops are how the host
                // is *reached*, not something one kind of channel does.
                config.hops = hops;
                // And the one thing that is not the same: a followed file runs
                // a command where a shell session runs a shell. Built here
                // rather than kept beside the path, so a reconnect picks up a
                // change to [`tail_command`] the way it picks up a re-edited
                // forwarding.
                config.command = remote_command(tail.as_deref(), start_dir.as_deref());
                // The profile's rules are the transport's rules, one for one:
                // reading them here rather than at construction is what lets a
                // reconnect pick up a forwarding the user has since edited —
                // and what lets the answer below be re-decided per connect.
                //
                // Unless a sibling session from this profile is holding the
                // ports already, in which case the transport is handed no rules
                // at all: every one of them would fail to bind and say so in
                // the grid, over a terminal the user just opened.
                //
                // And never for a followed file, whatever the workspace
                // decided. That flag is re-taken before every reconnect,
                // against the sessions that are live at that moment, so a tail
                // whose shell tab has since been closed would otherwise come
                // back holding the profile's ports — and take them from the
                // shell the moment the user opened one again. The forwardings
                // belong to the session the user works in.
                if self.tunnels_suppressed || tail.is_some() {
                    if !profile.tunnels.is_empty() {
                        log::debug!(
                            "not forwarding {} rule(s) for {}",
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
                    while let Some(batch) = events.next_batch().await {
                        let delivered = this.update(cx, |session, cx| {
                            for event in batch {
                                session.on_ssh_event(event, cx);
                            }
                            cx.notify();
                        });
                        if delivered.is_err() {
                            break;
                        }
                    }
                });
                (Transport::Ssh(ssh), pump)
            }
            Target::Local {
                cwd,
                command,
                filesystem,
                ..
            } => {
                let mut config = pty_config(cwd, command, cols, rows, effective.term);
                config.env = filesystem.shell_env();
                let (pty, mut events) = PtySession::spawn(config);
                let pump = cx.spawn(async move |this, cx| {
                    while let Some(batch) = events.next_batch().await {
                        let delivered = this.update(cx, |session, cx| {
                            for event in batch {
                                session.on_pty_event(event, cx);
                            }
                            cx.notify();
                        });
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
        // And the offer to replace a changed host key goes with the failure it
        // was made for: the attempt that starts here gets its own verdict.
        self.host_key = HostKeyRecovery::default();
        self._pump = Some(pump);
        cx.notify();
    }

    /// Applies one SSH transport event to the session state.
    fn on_ssh_event(&mut self, event: SshEvent, _cx: &mut Context<Self>) {
        // Ahead of the match, and by handing the whole event over rather than
        // by touching the list from three of the arms below: which events open
        // and close a forwarding is a rule of its own, and one worth being able
        // to assert on without a running session. The `cx.notify` at the end
        // covers the change — the strip re-reads `open_tunnels` on the next
        // frame, as it re-reads the status.
        self.tunnels.observe(&event);
        // Likewise: which events raise and withdraw the offer to replace a
        // changed host key is a rule of its own, asserted on its own. The
        // verifier is only asked when a key was refused.
        self.host_key
            .observe(&event, |host, port, algorithm, fingerprint| {
                tofu_verifier().recorded_key(host, port, algorithm, fingerprint)
            });
        match event {
            SshEvent::Connecting => self.status = SessionStatus::Connecting,
            SshEvent::HostKey {
                host,
                port,
                algorithm,
                fingerprint,
                accepted,
            } => {
                log::debug!(
                    "{}: host key of {host}:{port} {algorithm} {fingerprint} accepted={accepted}",
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
    }

    /// Applies one local pty event to the session state.
    ///
    /// A shell that exits is a plain disconnect rather than a failure — the
    /// user typed `exit` — and the shell that could not be started at all is
    /// the only thing the pty layer reports as an error.
    fn on_pty_event(&mut self, event: PtyEvent, _cx: &mut Context<Self>) {
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

/// What a local session's pty is started with.
fn pty_config(
    cwd: &Option<PathBuf>,
    command: &Option<Vec<String>>,
    cols: u16,
    rows: u16,
    term: String,
) -> PtyConfig {
    let mut config = PtyConfig::new(cols, rows);
    config.term = term;
    config.cwd = cwd.clone();
    // `None` here is not "nothing to run" but "run the platform's own
    // default", which is exactly what a unix local session wants and what the
    // pty layer already does.
    config.command = command.clone();
    config
}

/// The directory a duplicate of a local shell starts in: `reported`, the
/// shell's own `OSC 7`, or failing that what `process` says the shell's
/// process is standing in.
///
/// Only a directory that exists right now is worth passing on. A report can
/// name a directory that has since been removed, or — if the prompt is
/// misconfigured — something that is not a path at all; the process's answer
/// can be a directory removed from under it, which Linux spells with
/// ` (deleted)` on the end. A pty that cannot enter its working directory fails
/// to start, so anything doubtful falls through, and with nothing left the new
/// shell opens in the user's home directory.
///
/// `process` is a closure so that it is asked only when the report does not
/// do: it is a call into the kernel, cheap but not free.
fn local_start_dir(
    reported: Option<&str>,
    process: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    // Relative is a sign the answer was not a path at all: `OSC 7` carries an
    // absolute one, and resolving it against *our* directory would send the new
    // shell somewhere the user never was.
    let usable = |path: &PathBuf| path.is_absolute() && path.is_dir();
    reported
        .map(reported_local_path)
        .filter(usable)
        .or_else(|| process().filter(usable))
}

/// A local shell's reported directory as a path on this machine.
///
/// `OSC 7` carries a URL, and a Windows shell puts its directory into one the
/// way every file URL does: behind a slash, with forward slashes —
/// `/C:/Users/ada`. Windows reads that as relative to the current drive rather
/// than as the absolute path it is, so the slash comes off and the separators
/// are turned round. Anything else — a WSL shell's `/home/ada`, which names no
/// directory here, or an `OSC 1337` path already spelt the Windows way — is
/// left as it came and judged by [`local_start_dir`] as is.
fn reported_local_path(reported: &str) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = reported.strip_prefix('/') {
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if drive && bytes.get(2).is_none_or(|&b| b == b'/') {
            // `C:` alone is the drive's current directory, not its root.
            let path = if bytes.len() == 2 {
                format!("{rest}\\")
            } else {
                rest.replace('/', "\\")
            };
            return PathBuf::from(path);
        }
    }
    PathBuf::from(reported)
}

/// `reported` as a directory to start a shell in on a remote host, or `None`
/// for a report not to be acted on there.
///
/// Nothing on the far side can be checked from here, so this is about what the
/// path can do to the command line it goes into — see [`remote_command`] — and
/// the answer is cautious:
///
/// * **Only an absolute POSIX path.** A remote host that reports a directory at
///   all is nearly always a unix one, and `cd` into anything relative would be
///   relative to the home the new shell starts in.
/// * **Not a drive path** such as `/C:/Users/ada`, which is how a Windows
///   shell's `OSC 7` spells its directory. The login shell on the other end of
///   that is `cmd` or PowerShell, which would read the `cd` this goes into as
///   nonsense and end the session before it began.
/// * **No control characters.** A newline is legal in a unix file name and
///   survives a single-quoted word in `sh`, but csh and tcsh refuse a quoted
///   string that spans lines, and a split is no reason to find out how the
///   login shell takes it.
fn remote_start_dir(reported: &str) -> Option<String> {
    let rest = reported.strip_prefix('/')?;
    let first = rest.split('/').next().unwrap_or_default();
    let drive = first.len() == 2
        && first.ends_with(':')
        && first.starts_with(|c: char| c.is_ascii_alphabetic());
    if drive || reported.chars().any(char::is_control) {
        return None;
    }
    Some(reported.to_owned())
}

/// The command an SSH session runs in place of the login shell, or `None` for
/// the login shell itself.
///
/// A followed file runs [`tail_command`]. A shell opened in `start_dir` changes
/// into it and then replaces itself with the user's shell as a login shell —
/// `$SHELL -l`, which is what sshd would have started, and which every shell a
/// login can name accepts. The `cd` goes through the login shell, which is why
/// the path is quoted by [`crate::files::shell_quote`]: that rule is written for
/// the shells that may be on the other end — `sh` and its relatives, fish, csh
/// and tcsh — and it is one rule for every remote command line built out of a
/// path. `;` rather than `&&`, so that a directory removed since it was
/// reported costs an error line, not the session: the shell still starts, in
/// the home directory a session opened afresh would land in.
fn remote_command(tail: Option<&str>, start_dir: Option<&str>) -> Option<String> {
    match (tail, start_dir) {
        (Some(path), _) => Some(tail_command(path)),
        (None, Some(dir)) => Some(format!(
            "cd {}; exec \"$SHELL\" -l",
            crate::files::shell_quote(dir)
        )),
        (None, None) => None,
    }
}

/// A WSL shell's command line, made to start in the Linux directory `dir`.
///
/// `wsl.exe --cd` takes the distribution's own path, which is the only way a
/// WSL shell can be started anywhere but where the welcome screen's command
/// line puts it — `~`, see [`local_shells`]. That `--cd` is replaced rather
/// than a second one added, so that what `wsl.exe` makes of two of them never
/// comes into it; a command line without one gets one.
#[cfg(any(windows, test))]
fn with_wsl_start_dir(command: &[String], dir: &str) -> Vec<String> {
    let mut command = command.to_vec();
    match command.iter().position(|arg| arg == "--cd") {
        Some(flag) if flag + 1 < command.len() => command[flag + 1] = dir.to_owned(),
        _ => command.extend(["--cd".to_owned(), dir.to_owned()]),
    }
    command
}

/// Turns a profile's jump hosts into what the transport takes, or says why one
/// of them cannot be attempted.
///
/// All or nothing, and deliberately so: the hops are a *route*, and a route
/// with a gap in it is not a shorter route but no route at all. Attempting the
/// ones that do have credentials would open connections to bastions for nothing
/// and then fail at the gap anyway, with the user's password prompt-shaped
/// question buried under a transport error about a channel that could not be
/// opened.
///
/// `secrets` are the hops' stored secrets as [`hop_secrets`] read them, one per
/// hop and in order; a hop past the end of it has none. Nothing here touches
/// the keychain, so this runs on the UI thread with no wait behind it.
///
/// The [`Err`] is a finished sentence for the user, because it is put straight
/// on [`SessionStatus::Failed`] and printed by the connection overlay. It is
/// translated, unlike the `message`s that come up from the SSH layer: this one
/// was written here, and it asks the user to go and do something about it.
fn hop_specs(hops: &[HopRule], secrets: &[Option<String>]) -> Result<Vec<HopSpec>, String> {
    hops.iter()
        .enumerate()
        .map(|(index, hop)| hop_spec(hop, secrets.get(index).cloned().flatten()))
        .collect()
}

/// One hop resolved with its stored secret, or the reason it could not be.
fn hop_spec(hop: &HopRule, secret: Option<String>) -> Result<HopSpec, String> {
    let auth = match &hop.auth {
        // Nothing to attempt without the password: there is no unauthenticated
        // form of the method to fall back on, which is the same rule
        // [`crate::connection::saved_credentials`] applies to the target host —
        // except that there is no dialog to send the user to here, the hop
        // having no form of its own outside the connection's settings.
        AuthMethod::Password => match secret {
            Some(password) => SshAuth::Password(password),
            None => {
                return Err(ts!("session.hop_secret_missing", hop = hop_label(hop)).to_string());
            }
        },
        // A key with no stored passphrase is still worth attempting: most keys
        // need none, and the one that does fails on the hop itself, in the
        // transport's own words and naming the hop it happened on.
        AuthMethod::PublicKey { key_path } => SshAuth::PrivateKeyFile {
            path: key_path.clone(),
            passphrase: secret,
        },
        // An agent hop never needs one, whatever an older profile still says.
        AuthMethod::Agent => SshAuth::Agent,
    };
    Ok(HopSpec {
        host: hop.host.clone(),
        port: hop.port,
        username: hop.username.clone(),
        auth,
    })
}

/// Every hop's stored password or key passphrase, one per hop and in order,
/// each `None` when there is none to be had.
///
/// Reads the keychain, and so blocks for as long as the platform does —
/// behind an unlock prompt, as long as the user takes — which is why
/// [`Session::start`] runs it on the background executor. Kept apart from
/// [`hop_specs`] for that reason too: the route itself, and the sentence a
/// missing secret is refused with, are decided back on the UI thread.
fn hop_secrets(hops: &[HopRule]) -> Vec<Option<String>> {
    hops.iter().map(hop_secret).collect()
}

/// Whether `hop` says it keeps a secret in the keychain — the one question
/// that decides whether connecting through it has to wait for the keychain.
fn keeps_secret(hop: &HopRule) -> bool {
    hop.save_secret && !matches!(hop.auth, AuthMethod::Agent)
}

/// A hop's stored password or key passphrase, or `None` when there is none to
/// be had.
///
/// The keychain is only asked when the hop says something was put there, which
/// is [`crate::connection::saved_credentials`]'s rule and is worth keeping: on
/// the platforms that lock their store, asking raises an unlock prompt, and
/// raising one to be told "nothing" is the worst of both. An agent hop is never
/// asked about either. An entry that is there but empty counts as absent — it
/// is what a cleared field leaves behind — and an unreadable store is logged
/// and treated the same, since a hop that cannot be authenticated is refused
/// either way and the log line is the only place the storage error can usefully
/// go.
fn hop_secret(hop: &HopRule) -> Option<String> {
    if !keeps_secret(hop) {
        return None;
    }
    match SecretStore::get(hop.id) {
        Ok(secret) => secret.filter(|secret| !secret.is_empty()),
        Err(err) => {
            log::warn!("no stored secret for the jump host {}: {err:#}", hop.host);
            None
        }
    }
}

/// How a jump host is named in a sentence the user reads.
///
/// `user@host:port`, which is the shape the rest of the application already
/// names a connection in — [`SessionProfile::label`] writes `user@host` and the
/// port is added because a bastion is exactly the kind of host that does not
/// listen on 22. Never a secret, and never the profile's own credentials: a hop
/// is identified by where it is and who logs in to it.
fn hop_label(hop: &HopRule) -> String {
    format!("{}@{}:{}", hop.username, hop.host, hop.port)
}

/// The command a followed file runs in place of the login shell.
///
/// `-F` rather than `-f`, which is the whole reason this is not simply
/// `tail -f`: a log that is rotated out from under the follower is reopened by
/// name instead of leaving the pane silently attached to a file nobody writes
/// to any more. GNU coreutils, the BSDs and busybox all take it.
///
/// `exec` because the channel runs this through the login shell: without it the
/// shell stays in the process table doing nothing, and — more to the point —
/// the signal that ends the session has to travel through it. `--` because a
/// path is not an option, however much it may look like one; a file called
/// `-n` is a file.
///
/// [`TAIL_BACKLOG_LINES`] of history first, so the pane opens with something in
/// it rather than with a blank screen and a promise.
///
/// The path is the one part of this line the user wrote, so it is the one part
/// that could otherwise become a command; it is quoted by
/// [`crate::files::shell_quote`], which is the rule the file panel's own remote
/// commands already go through. Shared rather than restated, because an
/// escaping rule written twice is an escaping rule with one wrong copy — the
/// tests below are about *this* command line, and what a hostile path does to
/// it.
fn tail_command(path: &str) -> String {
    format!(
        "exec tail -n {TAIL_BACKLOG_LINES} -F -- {}",
        crate::files::shell_quote(path)
    )
}

/// The last component of a path on the *remote* host.
///
/// By hand rather than through
/// [`Path::file_name`](std::path::Path::file_name), for the reason
/// [`rulogman_core::TailRule::path`] is a `String` rather than a `PathBuf`: the
/// path belongs to the other end of the connection, and a Windows client
/// following `/var/log/syslog` must not have that read as one long file name.
/// A trailing separator is dropped first, so a path written with one still
/// names the thing before it; a path that is nothing but separators has no
/// component to give and is answered with itself.
pub(crate) fn remote_file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return path;
    }
    match trimmed.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `OSC 7` a prompt on `host` prints when it is standing in `dir`.
    ///
    /// A Windows path goes into the URL the way a Windows shell writes it:
    /// `C:\Users\ada` as `/C:/Users/ada`.
    fn osc_7(host: &str, dir: &str) -> Vec<u8> {
        let dir = if dir.starts_with('/') {
            dir.to_owned()
        } else {
            format!("/{}", dir.replace('\\', "/"))
        };
        format!("\x1b]7;file://{host}{dir}\x07").into_bytes()
    }

    /// A profile for the remote duplicates below, which never dial it.
    fn web_01() -> SessionProfile {
        SessionProfile::new("web-01", "example.com", 22, "alice", AuthMethod::Password)
    }

    /// `session`, made to have heard each of `reports` from its shell.
    fn reported(session: &Entity<Session>, reports: &[Vec<u8>], cx: &mut gpui::TestAppContext) {
        session.update(cx, |session, _| {
            for report in reports {
                session.terminal_mut().feed(report);
            }
        });
    }

    fn split_of(session: &Entity<Session>, cx: &mut gpui::TestAppContext) -> Target {
        session.read_with(cx, |session, _| session.duplicate_target())
    }

    #[test]
    fn a_reported_directory_that_exists_is_where_a_local_split_starts() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let reported = dir.path().to_str().expect("a UTF-8 temporary path");
        let start = local_start_dir(Some(reported), || {
            panic!("the process was asked although the report would do")
        });
        assert_eq!(start.as_deref(), Some(dir.path()));
    }

    #[test]
    fn without_a_usable_report_a_local_split_starts_where_the_process_is() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let process = || Some(dir.path().to_path_buf());

        assert_eq!(local_start_dir(None, process).as_deref(), Some(dir.path()));
        // Removed since, and not a path at all.
        let gone = dir.path().join("gone");
        let gone = gone.to_str().expect("a UTF-8 temporary path");
        assert_eq!(
            local_start_dir(Some(gone), process).as_deref(),
            Some(dir.path())
        );
        assert_eq!(
            local_start_dir(Some("relative/dir"), process).as_deref(),
            Some(dir.path())
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_path_from_a_windows_shell_is_read_as_a_windows_path() {
        assert_eq!(
            reported_local_path("/C:/Users/ada"),
            PathBuf::from(r"C:\Users\ada")
        );
        assert_eq!(reported_local_path("/c:"), PathBuf::from(r"c:\"));
        // A WSL shell's directory names nothing here and is left to fail.
        assert_eq!(reported_local_path("/home/ada"), PathBuf::from("/home/ada"));
    }

    #[test]
    fn with_neither_answer_usable_a_local_split_starts_at_home() {
        let dir = tempfile::tempdir().expect("temporary directory");
        // What Linux reads back for a directory removed from under a process.
        let deleted = PathBuf::from(format!("{} (deleted)", dir.path().display()));
        assert_eq!(local_start_dir(None, || Some(deleted.clone())), None);
        assert_eq!(local_start_dir(None, || None), None);
    }

    #[gpui::test]
    fn a_split_of_a_local_shell_is_spawned_in_its_reported_directory(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().to_str().expect("a UTF-8 temporary path");
        let session = cx.new(Session::dormant);
        reported(&session, &[osc_7("laptop", path)], cx);

        let Target::Local { cwd, command, .. } = split_of(&session, cx) else {
            panic!("a local shell split into something else");
        };
        let config = pty_config(&cwd, &command, 80, 24, "xterm-256color".to_owned());
        assert_eq!(config.cwd.as_deref(), Some(dir.path()));
    }

    #[gpui::test]
    fn a_split_follows_the_directory_a_windows_prompt_reports(cx: &mut gpui::TestAppContext) {
        // What the PowerShell and `cmd` prompts print: `OSC 1337`, the path as
        // the shell spells it, ended by BEL or by ST respectively.
        for end in ["\x07", "\x1b\\"] {
            let dir = tempfile::tempdir().expect("temporary directory");
            let path = dir.path().to_str().expect("a UTF-8 temporary path");
            let session = cx.new(Session::dormant);
            let report = format!("\x1b]1337;CurrentDir={path}{end}PS {path}> ");
            reported(&session, &[report.into_bytes()], cx);

            let Target::Local { cwd, .. } = split_of(&session, cx) else {
                panic!("a local shell split into something else");
            };
            assert_eq!(cwd.as_deref(), Some(dir.path()));
        }
    }

    #[test]
    fn a_wsl_shell_is_handed_its_prompt_hook_through_wslenv() {
        let env = |inherited| {
            let env = wsl_env(inherited);
            assert_eq!(
                env[0],
                ("PROMPT_COMMAND".to_owned(), WSL_PROMPT_COMMAND.to_owned())
            );
            assert_eq!(env[1].0, "WSLENV");
            env[1].1.clone()
        };
        assert_eq!(env(None), "PROMPT_COMMAND/u");
        assert_eq!(env(Some("")), "PROMPT_COMMAND/u");
        assert_eq!(env(Some("USERPROFILE/p")), "USERPROFILE/p:PROMPT_COMMAND/u");
        // One already forwarded is replaced, not doubled, and an empty entry
        // left by a stray colon is dropped.
        assert_eq!(
            env(Some("PROMPT_COMMAND/w::GOPATH/l")),
            "GOPATH/l:PROMPT_COMMAND/u"
        );
    }

    #[test]
    fn a_script_is_encoded_as_utf16_little_endian_base64() {
        assert_eq!(encoded_command(""), "");
        assert_eq!(encoded_command("a"), "YQA=");
        assert_eq!(encoded_command("dir"), "ZABpAHIA");
        assert_eq!(encoded_command("ab"), "YQBiAA==");
        assert_eq!(encoded_command("é"), "6QA=");
    }

    #[gpui::test]
    fn a_report_from_a_shell_on_another_host_does_not_move_a_local_split(
        cx: &mut gpui::TestAppContext,
    ) {
        let here = tempfile::tempdir().expect("temporary directory");
        let there = tempfile::tempdir().expect("temporary directory");
        let session = cx.new(Session::dormant);
        // `ssh db01` typed at the local prompt, whose own directory happens to
        // exist here too.
        reported(
            &session,
            &[
                osc_7("laptop", here.path().to_str().expect("UTF-8")),
                osc_7("db01", there.path().to_str().expect("UTF-8")),
            ],
            cx,
        );

        let Target::Local { cwd, .. } = split_of(&session, cx) else {
            panic!("a local shell split into something else");
        };
        // A dormant session has no process to ask, so it falls to home.
        assert_eq!(cwd, home_dir());
    }

    #[gpui::test]
    fn a_split_of_a_remote_shell_changes_into_its_reported_directory(
        cx: &mut gpui::TestAppContext,
    ) {
        let session = cx.new(|cx| Session::dormant_remote(web_01(), cx));
        reported(&session, &[osc_7("web01", "/srv/it's here")], cx);

        let Target::Ssh {
            tail, start_dir, ..
        } = split_of(&session, cx)
        else {
            panic!("a remote shell split into something else");
        };
        assert_eq!(start_dir.as_deref(), Some("/srv/it's here"));
        assert_eq!(
            remote_command(tail.as_deref(), start_dir.as_deref()).as_deref(),
            Some(r#"cd '/srv/it'\''s here'; exec "$SHELL" -l"#)
        );
    }

    #[gpui::test]
    fn a_split_of_a_remote_shell_with_nothing_reported_runs_the_login_shell(
        cx: &mut gpui::TestAppContext,
    ) {
        let session = cx.new(|cx| Session::dormant_remote(web_01(), cx));
        let Target::Ssh {
            tail, start_dir, ..
        } = split_of(&session, cx)
        else {
            panic!("a remote shell split into something else");
        };
        assert_eq!(start_dir, None);
        assert_eq!(remote_command(tail.as_deref(), None), None);
    }

    #[gpui::test]
    fn a_report_from_a_nested_shell_does_not_move_a_remote_split(cx: &mut gpui::TestAppContext) {
        let session = cx.new(|cx| Session::dormant_remote(web_01(), cx));
        reported(
            &session,
            &[osc_7("web01", "/home/alice"), osc_7("db01", "/var/lib/pg")],
            cx,
        );
        let Target::Ssh { start_dir, .. } = split_of(&session, cx) else {
            panic!("a remote shell split into something else");
        };
        assert_eq!(start_dir, None);
    }

    #[gpui::test]
    fn a_split_of_a_followed_file_follows_it_and_changes_nowhere(cx: &mut gpui::TestAppContext) {
        let session =
            cx.new(|cx| Session::dormant_tail(web_01(), "/var/log/syslog".to_owned(), cx));
        reported(&session, &[osc_7("web01", "/srv")], cx);
        let Target::Ssh {
            tail, start_dir, ..
        } = split_of(&session, cx)
        else {
            panic!("a followed file split into something else");
        };
        assert_eq!(start_dir, None);
        assert_eq!(
            remote_command(tail.as_deref(), start_dir.as_deref()),
            Some(tail_command("/var/log/syslog"))
        );
    }

    #[test]
    fn only_a_plain_absolute_posix_path_is_changed_into_remotely() {
        assert_eq!(remote_start_dir("/srv/app").as_deref(), Some("/srv/app"));
        assert_eq!(remote_start_dir("/").as_deref(), Some("/"));
        // `/ab:` is a directory like any other; only a drive letter is a drive.
        assert_eq!(remote_start_dir("/ab:/x").as_deref(), Some("/ab:/x"));

        assert_eq!(remote_start_dir("srv/app"), None);
        assert_eq!(remote_start_dir("/C:/Users/ada"), None);
        assert_eq!(remote_start_dir("/c:"), None);
        assert_eq!(remote_start_dir("/srv/two\nlines"), None);
    }

    #[test]
    fn a_remote_directory_is_quoted_for_every_login_shell() {
        let command = remote_command(None, Some(r"/srv/a\b c$d")).expect("a command");
        assert_eq!(command, r#"cd '/srv/a'\\'b c$d'; exec "$SHELL" -l"#);
    }

    #[test]
    fn a_wsl_split_replaces_the_directory_on_the_command_line() {
        let command: Vec<String> = ["wsl.exe", "-d", "Ubuntu", "--cd", "~"]
            .map(str::to_owned)
            .into();
        assert_eq!(
            with_wsl_start_dir(&command, "/home/ada/src"),
            ["wsl.exe", "-d", "Ubuntu", "--cd", "/home/ada/src"]
        );
        // One that never had a directory is given one.
        assert_eq!(
            with_wsl_start_dir(&command[..3], "/srv"),
            ["wsl.exe", "-d", "Ubuntu", "--cd", "/srv"]
        );
    }

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

    #[cfg(windows)]
    #[test]
    fn the_windows_shells_start_with_a_prompt_that_reports_its_directory() {
        let shells = local_shells(&[]);

        assert_eq!(
            shells[0].command,
            [
                "powershell.exe",
                "-NoLogo",
                "-NoExit",
                "-EncodedCommand",
                encoded_command(POWERSHELL_PROMPT_HOOK).as_str(),
            ]
            .map(str::to_owned)
        );
        assert_eq!(
            shells[1].command,
            ["cmd.exe", "/k", "prompt", CMD_PROMPT].map(str::to_owned)
        );
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

    /// A key event as the transport reports it for `example.com:22`.
    fn host_key(accepted: bool) -> SshEvent {
        SshEvent::HostKey {
            host: "example.com".to_owned(),
            port: 22,
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:presented".to_owned(),
            accepted,
        }
    }

    /// What the policy answers for a key that collided with a record.
    fn on_record(host: &str, port: u16, _: &str, _: &str) -> Option<(String, String)> {
        assert_eq!(
            (host, port),
            ("example.com", 22),
            "asked under the leg's name"
        );
        Some(("ssh-rsa".to_owned(), "SHA256:recorded".to_owned()))
    }

    /// What the policy answers when nothing is on record to collide with.
    fn nothing_on_record(_: &str, _: u16, _: &str, _: &str) -> Option<(String, String)> {
        None
    }

    /// A policy that must not be asked at all.
    fn never_asked(_: &str, _: u16, _: &str, _: &str) -> Option<(String, String)> {
        panic!("only a refused key is looked up")
    }

    #[test]
    fn a_refused_key_on_record_is_offered_with_both_keys() {
        let mut recovery = HostKeyRecovery::default();
        recovery.observe(&SshEvent::Connecting, never_asked);
        recovery.observe(&host_key(false), on_record);
        recovery.observe(
            &SshEvent::Error(
                rulogman_ssh::SshErrorKind::HostKeyRejected,
                "the host key presented by example.com:22 was rejected".to_owned(),
            ),
            never_asked,
        );
        let mismatch = recovery
            .mismatch
            .expect("the failure can be recovered from");
        assert_eq!(
            mismatch,
            HostKeyMismatch {
                host: "example.com".to_owned(),
                port: 22,
                stored_algorithm: "ssh-rsa".to_owned(),
                stored_fingerprint: "SHA256:recorded".to_owned(),
                presented_algorithm: "ssh-ed25519".to_owned(),
                presented_fingerprint: "SHA256:presented".to_owned(),
            }
        );
        assert_eq!(mismatch.address(), "example.com:22");
    }

    #[test]
    fn a_refusal_with_nothing_on_record_offers_nothing() {
        // A verifier that refuses for a reason of its own — not a collision
        // with a record — is not one that forgetting a record would satisfy.
        let mut recovery = HostKeyRecovery::default();
        recovery.observe(&host_key(false), nothing_on_record);
        assert_eq!(recovery.mismatch, None);
    }

    #[test]
    fn an_accepted_key_or_a_fresh_attempt_withdraws_the_offer() {
        for withdrawing in [
            host_key(true),
            SshEvent::Connecting,
            SshEvent::Ready,
            SshEvent::Disconnected {
                reason: "closed by the user".to_owned(),
            },
            SshEvent::Error(rulogman_ssh::SshErrorKind::Auth, "rejected".to_owned()),
        ] {
            let mut recovery = HostKeyRecovery::default();
            recovery.observe(&host_key(false), on_record);
            recovery.observe(&withdrawing, never_asked);
            assert_eq!(recovery.mismatch, None, "{withdrawing:?} kept the offer");
        }
    }

    #[test]
    fn output_and_forwarding_notices_leave_the_offer_alone() {
        let mut recovery = HostKeyRecovery::default();
        recovery.observe(&host_key(false), on_record);
        recovery.observe(&SshEvent::Data(b"x".to_vec()), never_asked);
        recovery.observe(
            &SshEvent::TunnelFailed {
                rule: A_RULE.to_owned(),
                message: "in use".to_owned(),
            },
            never_asked,
        );
        assert!(recovery.mismatch.is_some());
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

    /// A jump host with nothing stored for it, which is what the hop rows of a
    /// freshly typed profile look like before anything is saved.
    fn hop(auth: AuthMethod) -> HopRule {
        HopRule {
            id: Uuid::new_v4(),
            host: "bastion.example.com".to_owned(),
            port: 2222,
            username: "alice".to_owned(),
            auth,
            // The one field the tests below all lean on: with no secret stored,
            // nothing here touches the keychain of the machine running them.
            save_secret: false,
        }
    }

    #[test]
    fn a_password_hop_with_no_stored_secret_fails_the_whole_route() {
        // Not "the hops that worked": a route with a gap in it is no route.
        // The message has to name the hop, because a profile can have several
        // and only one of them is the one to go and fix.
        let hops = [hop(AuthMethod::Password)];
        let message = hop_specs(&hops, &[None]).expect_err("a hop with no password was accepted");
        assert!(
            message.contains("alice@bastion.example.com:2222"),
            "{message}"
        );
    }

    #[test]
    fn an_agent_hop_needs_no_stored_secret() {
        let hops = [hop(AuthMethod::Agent)];
        let specs = hop_specs(&hops, &[None]).expect("an agent hop needs no stored secret");
        assert_eq!(specs[0].host, "bastion.example.com");
        assert_eq!(specs[0].port, 2222);
        assert_eq!(specs[0].username, "alice");
        assert!(matches!(specs[0].auth, SshAuth::Agent));
    }

    #[test]
    fn a_key_hop_with_no_stored_passphrase_is_still_attempted() {
        // Most keys need no passphrase, and the one that does fails on the hop
        // itself with the transport's own words. Refusing here would turn every
        // unencrypted key into an error.
        let hops = [hop(AuthMethod::PublicKey {
            key_path: PathBuf::from("/home/alice/.ssh/id_ed25519"),
        })];
        let specs = hop_specs(&hops, &[None]).expect("a key hop needs no stored secret");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host, "bastion.example.com");
        assert_eq!(specs[0].port, 2222);
        assert_eq!(specs[0].username, "alice");
        match &specs[0].auth {
            SshAuth::PrivateKeyFile { path, passphrase } => {
                assert_eq!(path, std::path::Path::new("/home/alice/.ssh/id_ed25519"));
                assert_eq!(passphrase.as_deref(), None);
            }
            other => panic!("a key hop authenticates with its key, not with {other:?}"),
        }
    }

    #[test]
    fn a_hop_is_resolved_with_the_secret_read_for_it() {
        let hops = [
            hop(AuthMethod::Agent),
            hop(AuthMethod::Password),
            hop(AuthMethod::PublicKey {
                key_path: PathBuf::from("/home/alice/.ssh/id_ed25519"),
            }),
        ];
        let secrets = [
            None,
            Some("hunter2".to_owned()),
            Some("passphrase".to_owned()),
        ];
        let specs = hop_specs(&hops, &secrets).expect("every hop has what it needs");
        assert!(matches!(specs[0].auth, SshAuth::Agent));
        assert!(matches!(&specs[1].auth, SshAuth::Password(password) if password == "hunter2"));
        assert!(matches!(
            &specs[2].auth,
            SshAuth::PrivateKeyFile { passphrase: Some(passphrase), .. } if passphrase == "passphrase"
        ));
    }

    #[test]
    fn only_a_hop_that_kept_a_secret_waits_for_the_keychain() {
        let mut password = hop(AuthMethod::Password);
        assert!(!keeps_secret(&password), "nothing was stored");
        password.save_secret = true;
        assert!(keeps_secret(&password));
        // An older profile can still carry the flag from before the hop moved
        // to the agent, which needs nothing stored.
        let mut agent = hop(AuthMethod::Agent);
        agent.save_secret = true;
        assert!(!keeps_secret(&agent));
        assert_eq!(hop_secrets(&[agent]), vec![None]);
    }

    #[test]
    fn a_profile_with_no_hops_has_no_route_to_refuse() {
        assert!(
            hop_specs(&[], &[])
                .expect("an empty route is a route")
                .is_empty()
        );
    }

    #[test]
    fn a_followed_file_is_quoted_as_one_word() {
        assert_eq!(
            tail_command("/var/log/nginx/access.log"),
            "exec tail -n 200 -F -- '/var/log/nginx/access.log'"
        );
    }

    #[test]
    fn a_path_cannot_talk_its_way_out_of_its_quotes() {
        // The whole reason the quoting is a function of its own. Each of these
        // is a path a user could type — or a profile could be edited to carry —
        // and every one of them has to reach `tail` as a single argument with
        // no shell doing anything about it on the way.
        for hostile in [
            "/tmp/'; rm -rf ~; echo '",
            "/tmp/$(id)",
            "/tmp/`id`",
            "/tmp/a b c",
            "/tmp/*.log",
            "/tmp/it's here",
            "/tmp/\\; id",
            "--help",
        ] {
            let command = tail_command(hostile);
            let quoted = command
                .strip_prefix("exec tail -n 200 -F -- ")
                .unwrap_or_else(|| panic!("{command} did not start with the tail invocation"));
            assert_eq!(
                unquote(quoted),
                hostile,
                "{quoted} does not read back as one word"
            );
        }
    }

    /// A POSIX shell's own reading of a single-quoted word, for the test above.
    ///
    /// Deliberately not the inverse of [`single_quoted`] written backwards: it
    /// implements the *shell's* rule — outside quotes a `\'` opens one, inside
    /// them everything is literal until the next `'` — so that the assertion is
    /// about what a shell would do rather than about what the encoder meant.
    fn unquote(text: &str) -> String {
        let mut out = String::new();
        let mut inside = false;
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\'' => inside = !inside,
                '\\' if !inside => out.extend(chars.next()),
                other => out.push(other),
            }
        }
        assert!(!inside, "{text} left a quote open");
        out
    }

    #[test]
    fn a_followed_file_is_named_after_its_last_component() {
        assert_eq!(remote_file_name("/var/log/nginx/access.log"), "access.log");
        assert_eq!(remote_file_name("access.log"), "access.log");
        // A trailing separator names the thing before it rather than nothing.
        assert_eq!(remote_file_name("/var/log/"), "log");
        // And a path that is nothing but separators has no component to give.
        assert_eq!(remote_file_name("/"), "/");
        assert_eq!(remote_file_name(""), "");
    }

    #[test]
    fn a_status_with_nothing_to_quote_still_summarises() {
        // Neither of these interpolates anything, so an empty answer would mean
        // the key went missing rather than that there was nothing to say.
        assert!(!SessionStatus::Connecting.summary().is_empty());
        assert!(!SessionStatus::Connected.summary().is_empty());
    }
}
