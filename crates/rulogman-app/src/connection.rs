//! Connection dialog: saved profiles and the form used to open a session.
//!
//! This module is the only place in the application that touches
//! [`ProfileStore`] and [`SecretStore`], and every credential the shell ever
//! connects with is resolved here, by one of two entry points:
//!
//! * [`ConnectionDialog`] turns what the user typed into a ready-to-use
//!   [`SshAuth`]. By the time it emits [`ConnectionDialogEvent::Connect`] the
//!   profile is on disk and the secret is in the OS keychain, when the user
//!   asked for that.
//! * [`saved_credentials`] answers the question the dialog cannot: for a
//!   profile the user merely clicked, is everything a connection needs already
//!   stored? When it is, the shell opens the session and the dialog never
//!   appears.
//!
//! # Handling of secrets
//!
//! Passwords and key passphrases live only in the masked [`TextInput`]s, in the
//! keychain, and in the [`SshAuth`] handed to the caller. They are never
//! logged, never rendered unmasked, and never included in a status message.
//! [`ConnectionDialog`] deliberately does not implement `Debug` so that a stray
//! `{:?}` cannot leak them either, and [`SshAuth`]'s own `Debug` redacts them.

mod advanced;
mod credentials;
pub use credentials::saved_credentials;
#[cfg(test)]
use credentials::{Credentials, decide_credentials, key_opens_unlocked};
mod form;
mod navigation;
mod profiles;
mod render;
use render::{digits_only, format_number, inherit_hint, row_action, section, summary_note};
use std::path::{Path, PathBuf};
use std::sync::Once;

use gpui::{
    AnyElement, App, Context, DragMoveEvent, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseUpEvent, PathPromptOptions, Pixels, Point, Render, ScrollHandle, SharedString, Window,
    actions, div, prelude::*, px,
};
use rulogman_core::{
    AuthMethod, HopRule, ProfileStore, SecretStore, SessionOverrides, SessionProfile, TailRule,
    TunnelRule, effective_highlights,
};
#[cfg(unix)]
use rulogman_pty::login_shell_name;
use rulogman_ssh::SshAuth;
use rulogman_term::{Charset, TerminalTheme};
use uuid::Uuid;

use crate::highlight_rules::{HighlightRuleFields, HighlightRuleList, collect_highlight_rules};
use crate::i18n::{input_menu_labels, ts};
use crate::icons;
#[cfg(windows)]
use crate::session::{LocalShell, local_shells};
use rugpui::{
    Button, ButtonVariant, Checkbox, Collapsible, ContextMenu, DraggedThumb, MenuEntry,
    SchemeSelect, SchemeSwatch, Scrollbar, ScrollbarAxis, ScrollbarState, Segmented, Select,
    TextInput, form_row, hide_later, hide_now, modal, scroll_to, scrolled, theme,
};

/// The dialog's two scrolling surfaces, and the element id of each one's overlay
/// scroll indicator.
///
/// A single drag listener on the dialog root answers both, so it has to be able
/// to tell which bar a drag belongs to; these ids are how, and pairing each with
/// the surface it names keeps the two from being wired up crosswise.
const SCROLLBARS: [(&str, Surface); 2] = [
    ("connection-body-scrollbar", Surface::Body),
    ("connection-list-scrollbar", Surface::List),
];

/// Which of the dialog's scrolling surfaces is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    /// The dialog body, which scrolls behind the footer.
    Body,
    /// The saved-profile column, which scrolls inside the body.
    List,
}

/// Port pre-filled into the form and used when the port field is left empty.
const DEFAULT_PORT: u16 = 22;

/// Widest port number that still fits in a `u16`, in digits.
const MAX_PORT_DIGITS: usize = 5;

/// Widest scrollback override the field accepts, in characters.
const MAX_SCROLLBACK_DIGITS: usize = 6;

/// Widest font size override the field accepts, in characters.
const MAX_FONT_SIZE_DIGITS: usize = 5;

/// Width of a port column in the tunnel table.
///
/// Wide enough for five digits and the heading over them; the remote host takes
/// whatever is left, since it is the only value of a rule with no length limit.
const TUNNEL_PORT_WIDTH: f32 = 92.;

/// Width of the action column at the end of a tunnel row.
const TUNNEL_ACTION_WIDTH: f32 = 56.;

/// Width of the port column in the jump-host table.
///
/// The same width the tunnel table gives a port, and deliberately so: the two
/// tables sit one above the other in the same dialog, and a port field that
/// changed width between them would read as a different kind of field.
const HOP_PORT_WIDTH: f32 = 92.;

/// Width of the login-name column in the jump-host table.
const HOP_USERNAME_WIDTH: f32 = 148.;

/// Width of the action column at the end of a jump-host row.
const HOP_ACTION_WIDTH: f32 = TUNNEL_ACTION_WIDTH;

/// Width of the authentication picker on a jump-host row's second line.
///
/// Two segments rather than the form's three, and sized for the longer of the
/// two labels in any of the offered languages.
const HOP_AUTH_WIDTH: f32 = 176.;

/// Width of the action column at the end of a followed-file row.
const TAIL_ACTION_WIDTH: f32 = TUNNEL_ACTION_WIDTH;

/// Address a tunnel rule added in the form binds its local listener to.
///
/// Loopback, which is both what OpenSSH's `-L` defaults to and what
/// `rulogman-core` fills in for a stored rule that names no address. That default
/// is a private serde helper, so the value is repeated here rather than shared;
/// the form does not offer the field, and a rule loaded from disk keeps
/// whatever address it was given.
const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1";

/// Id used by the "inherit the global scheme" row in the overrides picker.
const INHERIT_SCHEME_ID: &str = "";

/// Width of the dialog panel.
///
/// Wide enough that the longest control label — the "Remember passphrase in the
/// system keychain" checkbox, plus its focus ring — still fits on one line.
const DIALOG_WIDTH: f32 = 724.;

/// Width of the saved-profile column.
const LIST_WIDTH: f32 = 260.;

/// Height at which the saved-profile column starts scrolling.
const LIST_MAX_HEIGHT: f32 = 300.;

/// Segments of the authentication picker, in [`AuthKind`] order.
///
/// The first half of each pair is an element id and is never translated; only
/// the label is. Built per call rather than declared as a `const` because the
/// labels come out of the active locale.
fn auth_options() -> [(&'static str, SharedString); 3] {
    [
        ("password", ts!("connection.auth.password")),
        ("key", ts!("connection.auth.key")),
        ("agent", ts!("connection.auth.agent")),
    ]
}

/// Segments of a jump host's authentication picker, in [`AuthKind`] order.
///
/// The form's own options minus the agent: an agent hop cannot be attempted at
/// all — `rulogman-ssh` has no agent transport — and offering it on a row would
/// mean a per-row way of explaining that, on a control the user reaches long
/// before the connection it would break. The two that remain occupy indices 0
/// and 1, which are the indices [`AuthKind::from_index`] already gives them.
fn hop_auth_options() -> [(&'static str, SharedString); 2] {
    [
        ("password", ts!("connection.auth.password")),
        ("key", ts!("connection.auth.key")),
    ]
}

/// Entries of the character set dropdown: the "inherit" row first, then the
/// offered encodings in [`Charset::SUPPORTED`]'s own order.
///
/// Only the first entry is translated. An encoding's canonical WHATWG name is
/// both what the user picks and what is written to `profiles.json`, so it reads
/// the same in every language and is never looked up in the catalog.
fn charset_options() -> Vec<SharedString> {
    let mut options = Vec::with_capacity(Charset::SUPPORTED.len() + 1);
    options.push(ts!("connection.overrides.charset_default"));
    options.extend(
        Charset::SUPPORTED
            .iter()
            .map(|charset| SharedString::from(charset.name())),
    );
    options
}

/// The override that picking row `index` of [`charset_options`] stands for.
///
/// Row 0 is the "inherit" row and yields `None`; every other row is offset by
/// that one row from [`Charset::SUPPORTED`]. Resolved by index rather than by
/// the text of the row, because the first row's text is translated.
fn charset_at(index: usize) -> Option<String> {
    index
        .checked_sub(1)
        .and_then(|index| Charset::SUPPORTED.get(index))
        .map(|charset| charset.name().to_owned())
}

/// The row of [`charset_options`] that `charset` — a label as stored — sits on.
///
/// Used to scroll the open list to where the user stands, so an encoding it
/// cannot place answers with the top of the list rather than with nothing. The
/// label is resolved before it is looked up, so an alias written by hand
/// (`euc-kr`, `windows-949`) finds the row of the encoding it names; one that
/// resolves to an encoding outside the offered list — which
/// [`Charset::for_label`] accepts and the session honours — has no row of its
/// own, and none is highlighted for it either.
fn charset_row(charset: Option<&str>) -> usize {
    let Some(charset) = charset.map(Charset::from_label_or_utf8) else {
        return 0;
    };
    Charset::SUPPORTED
        .iter()
        .position(|offered| *offered == charset)
        .map_or(0, |index| index + 1)
}

/// Key context the dialog's own shortcuts are scoped to.
///
/// `Tab` **must** stay scoped to this context. The terminal forwards `Tab` to
/// the remote shell for completion, so a binding registered against the global
/// (`None`) context would silently break it.
const KEY_CONTEXT: &str = "ConnectionDialog";

/// Guards the one-time registration of the dialog's key bindings.
static BIND_KEYS: Once = Once::new();

actions!(
    rulogman_connection,
    [
        /// Move focus to the next control in the dialog.
        FocusNext,
        /// Move focus to the previous control in the dialog.
        FocusPrev,
    ]
);

/// Tab order of the form, in visual order.
///
/// Indices are spaced so that the controls which only exist in one
/// authentication mode can be numbered without renumbering their neighbours;
/// a control that is not rendered is never painted and therefore never enters
/// the tab ring at all, so the gaps are harmless.
mod tab {
    /// Connection name.
    pub const NAME: isize = 10;
    /// Host name or address.
    pub const HOST: isize = 20;
    /// TCP port.
    pub const PORT: isize = 30;
    /// Remote login name.
    pub const USERNAME: isize = 40;
    /// Authentication method picker.
    pub const AUTH: isize = 50;
    /// Password, or the key path in private key mode.
    pub const SECRET_OR_KEY: isize = 60;
    /// The key file browser button.
    pub const BROWSE: isize = 65;
    /// Private key passphrase.
    pub const PASSPHRASE: isize = 70;
    /// "Remember ... in the system keychain".
    pub const REMEMBER: isize = 80;
    /// "Show the file panel when this connection opens".
    pub const SHOW_FILES: isize = 81;
    /// The "Session overrides" disclosure button.
    pub const OVERRIDES: isize = 82;
    /// Per-session color scheme. Only a stop while the section is expanded.
    pub const OVERRIDE_SCHEME: isize = 84;
    /// Per-session font size.
    pub const OVERRIDE_FONT_SIZE: isize = 86;
    /// Per-session scrollback depth.
    pub const OVERRIDE_SCROLLBACK: isize = 87;
    /// Per-session `TERM`.
    pub const OVERRIDE_TERM: isize = 88;
    /// Per-session character set.
    pub const OVERRIDE_CHARSET: isize = 89;
    /// The "Jump hosts" disclosure button.
    pub const HOPS: isize = 90;
    /// First input of the first jump-host row.
    ///
    /// Numbered exactly like the tunnel rows below, by
    /// [`HOP_ROW_STRIDE`] from the row's position in the list.
    pub const HOP_ROWS: isize = 100;
    /// Indices one jump-host row occupies: host, port, user, the method
    /// picker, the key path and the secret.
    ///
    /// The last two are only painted in the mode that has them, so a password
    /// hop leaves one of its indices unused; a control that is not rendered
    /// never enters the tab ring, so the gap costs nothing.
    pub const HOP_ROW_STRIDE: isize = 6;
    /// The "Add jump host" button, past every row the numbering can reach.
    pub const HOP_ADD: isize = 190;
    /// The "SSH tunnels" disclosure button.
    pub const TUNNELS: isize = 200;
    /// First input of the first tunnel row.
    ///
    /// Every row takes [`TUNNEL_ROW_STRIDE`] indices, one per input, and is
    /// numbered from its position in the list, so the rows tab in the order
    /// they are drawn. Removing a row leaves the others where they are: the
    /// remaining indices still ascend, which is all the tab ring reads.
    pub const TUNNEL_ROWS: isize = 210;
    /// Indices one tunnel row occupies: local port, remote host, remote port.
    pub const TUNNEL_ROW_STRIDE: isize = 3;
    /// The "Add tunnel" button, past every row the numbering can reach.
    pub const TUNNEL_ADD: isize = 290;
    /// The "Tail files" disclosure button.
    pub const TAILS: isize = 300;
    /// The path input of the first followed-file row.
    ///
    /// Numbered like the two sections above, by [`TAIL_ROW_STRIDE`] from the
    /// row's position in the list.
    pub const TAIL_ROWS: isize = 310;
    /// Indices one followed-file row occupies.
    ///
    /// Enormous next to a hop's six, and for one reason: a row is no longer one
    /// field but a path, a tick, and — while that tick is set — a whole
    /// highlight rule list, which numbers its own rows inside
    /// [`TAB_SPAN`](crate::highlight_rules::TAB_SPAN) indices of its own. The
    /// stride is what a row *may* take, not what it usually does; every index a
    /// collapsed row leaves unused costs nothing, because a control that is
    /// never rendered never enters the tab ring.
    pub const TAIL_ROW_STRIDE: isize = 500;
    /// Offset of a row's "Custom highlighting" tick within its block.
    pub const TAIL_CUSTOM: isize = 1;
    /// Offset of a row's highlight rule list within its block.
    ///
    /// Ten rather than two, so the row keeps room between its own two controls
    /// and the block the list numbers inside — the same spacing every other
    /// ladder in this file leaves for a control added later.
    pub const TAIL_HIGHLIGHTS: isize = 10;
    /// The "Add file" button, past every row the numbering can reach.
    pub const TAIL_ADD: isize = 10400;
    /// Cancel.
    pub const CANCEL: isize = 10500;
    /// Connect.
    pub const CONNECT: isize = 10510;
}

/// Emitted by [`ConnectionDialog`] when the user acts on it.
///
/// `Connect` is far larger than `Dismissed` — a [`SessionProfile`] grew past
/// clippy's threshold once it gained per-session overrides. Boxing the payload
/// is the usual remedy, but the shell is written against these exact field
/// types, and the event is emitted once per user action, so the size difference
/// costs nothing worth an API break.
#[allow(clippy::large_enum_variant)]
pub enum ConnectionDialogEvent {
    /// Open a session. The dialog has already persisted the profile and any
    /// secret the user asked to remember.
    Connect {
        /// Profile describing the target host.
        profile: SessionProfile,
        /// Credentials resolved from the form and the OS keychain.
        auth: SshAuth,
    },
    /// Open a shell on this machine. Carries nothing: a local session is not
    /// saved, needs no credentials, and always runs the user's login shell.
    #[cfg(unix)]
    ConnectLocal,
    /// Open the shell on this machine the user picked from the pinned rows.
    ///
    /// The Windows counterpart of [`ConnectionDialogEvent::ConnectLocal`], and
    /// the reason the two are not one variant: Windows has no single local
    /// shell, so which one was picked is the whole of the message. Still
    /// carries no credentials and saves nothing — a local session is a local
    /// session on either platform.
    #[cfg(windows)]
    ConnectLocalShell(LocalShell),
    /// The dialog was dismissed without connecting.
    Dismissed,
}

/// Authentication method offered by the form.
///
/// Mirrors [`AuthMethod`] but is ordered, because the segmented control
/// addresses its options by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    /// Password authentication.
    Password,
    /// Public key authentication with a key file on disk.
    PrivateKey,
    /// Delegate to a running SSH agent. Not implemented by `rulogman-ssh` yet.
    Agent,
}

impl AuthKind {
    /// Index of this method in [`auth_options`].
    fn index(self) -> usize {
        match self {
            Self::Password => 0,
            Self::PrivateKey => 1,
            Self::Agent => 2,
        }
    }

    /// The method at `index` in [`auth_options`], defaulting to
    /// [`AuthKind::Password`].
    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::PrivateKey,
            2 => Self::Agent,
            _ => Self::Password,
        }
    }
}

/// Severity of the message strip at the bottom of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusLevel {
    /// Neutral guidance, e.g. "a saved secret will be used".
    Info,
    /// Something went wrong but the connection can still proceed.
    Warning,
    /// The action could not be completed.
    Error,
}

impl StatusLevel {
    /// Color of the message text under the active theme.
    fn color(self, theme: &rugpui::Theme) -> Hsla {
        match self {
            Self::Info => theme.text_muted,
            Self::Warning => theme.accent,
            Self::Error => theme.danger,
        }
    }
}

/// A message rendered inside the dialog.
struct DialogStatus {
    /// How loudly to render it.
    level: StatusLevel,
    /// Lines shown to the user, each a sentence of its own. Never contains a
    /// secret.
    ///
    /// A list rather than one string because a run that hits several storage
    /// problems reports each of them; stitching them into one sentence would
    /// mean assembling grammar in code, which no translation survives.
    lines: Vec<SharedString>,
}

/// Field that should receive keyboard focus the next time the dialog renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    /// The host field, for a brand new connection.
    Host,
    /// The password or passphrase field, for a profile that is already filled in.
    Secret,
}

/// One editable row of the "SSH tunnels" section.
///
/// The three inputs are the whole of what the form offers; `bind_address` is
/// carried alongside them so that a rule written by hand into `profiles.json`
/// keeps the address it names. Editing the rest of such a rule in the dialog
/// therefore preserves it, which is what a field the UI does not show has to
/// do to be worth storing at all.
struct TunnelRow {
    /// Local TCP port the listener binds, digits only.
    local_port: Entity<TextInput>,
    /// Host the remote end connects to, as the remote end resolves it.
    remote_host: Entity<TextInput>,
    /// Port on that host, digits only.
    remote_port: Entity<TextInput>,
    /// Address the listener binds; not editable in the form.
    bind_address: String,
}

/// The text of one tunnel row, read out of its inputs.
///
/// Splitting the reading from the interpreting is what lets the rules of an
/// unfinished row be exercised without a window: [`collect_tunnel_rules`] sees
/// only strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TunnelFields {
    /// Local port as typed.
    local_port: String,
    /// Remote host as typed.
    remote_host: String,
    /// Remote port as typed.
    remote_port: String,
    /// Address the finished rule binds to.
    bind_address: String,
}

impl TunnelFields {
    /// Whether the user has typed nothing into any of the three inputs.
    fn is_blank(&self) -> bool {
        self.local_port.is_empty() && self.remote_host.is_empty() && self.remote_port.is_empty()
    }
}

/// Turn the rows of the tunnel section into rules, or refuse.
///
/// A row the user has not touched is dropped rather than complained about: the
/// section always ends with an empty row once "Add tunnel" has been pressed,
/// and an empty form is not an error. Anything else has to be complete — both
/// ports present, in range and non-zero, and a host to reach — because a
/// half-written rule cannot be forwarded and silently dropping it would open a
/// session the user believes forwards a port it does not.
///
/// `None` is that refusal; the caller turns it into the message strip.
fn collect_tunnel_rules(rows: &[TunnelFields]) -> Option<Vec<TunnelRule>> {
    let mut rules = Vec::new();
    for row in rows {
        if row.is_blank() {
            continue;
        }
        let local_port = row
            .local_port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)?;
        let remote_port = row
            .remote_port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)?;
        if row.remote_host.is_empty() {
            return None;
        }
        rules.push(TunnelRule {
            bind_address: row.bind_address.clone(),
            local_port,
            remote_host: row.remote_host.clone(),
            remote_port,
        });
    }
    Some(rules)
}

/// One editable row of the "Jump hosts" section.
///
/// Carries its own [`HopRule::id`], because that id is the account name of the
/// hop's keychain entry: the row has to be able to say which credential it is
/// editing, and a fresh row has to claim one nothing else answers to before it
/// can store anything.
///
/// The secret lives only in `secret`, masked, and is written to the keychain by
/// [`ConnectionDialog::connect`]. `save_secret` is what the *stored* profile
/// said, so an emptied secret field on a hop that already has one keeps it
/// rather than forgetting it — the same reading the form above gives its own
/// empty password field.
struct HopRow {
    /// Identifier of the rule this row edits, and of its keychain entry.
    id: Uuid,
    /// Hostname or address of the jump host.
    host: Entity<TextInput>,
    /// Port of the jump host's SSH server, digits only; blank means 22.
    port: Entity<TextInput>,
    /// Login user on the jump host.
    username: Entity<TextInput>,
    /// Method this hop authenticates with.
    ///
    /// Held on the row rather than on the dialog: every hop logs in for itself,
    /// and a bastion reached with a key in front of a host reached with a
    /// password is the ordinary case rather than the odd one.
    auth_kind: AuthKind,
    /// Path of the private key, in [`AuthKind::PrivateKey`] mode.
    key_path: Entity<TextInput>,
    /// Password or key passphrase, masked.
    secret: Entity<TextInput>,
    /// Whether the stored rule already keeps a secret under [`Self::id`].
    save_secret: bool,
}

/// The text of one jump-host row, read out of its inputs.
///
/// Plain strings and one enum, for the reason [`TunnelFields`] is: it lets the
/// rules of an unfinished row be exercised without a window. The secret is
/// deliberately absent — it never reaches the profile, only the keychain — and
/// so is any field the row does not put on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HopFields {
    /// Identifier the finished rule keeps.
    id: Uuid,
    /// Jump host as typed.
    host: String,
    /// Port as typed; empty means [`DEFAULT_PORT`].
    port: String,
    /// Login user as typed.
    username: String,
    /// Method picked for this hop.
    auth: AuthKind,
    /// Key path as typed, meaningful only in [`AuthKind::PrivateKey`] mode.
    key_path: String,
    /// Whether a secret is already stored for this hop.
    save_secret: bool,
}

impl HopFields {
    /// Whether the user has typed nothing into any of the row's text fields.
    ///
    /// The method picker is not consulted: it starts on a value and can never
    /// be empty, so a row whose only "content" is the default it was born with
    /// is still a row nobody has filled in.
    fn is_blank(&self) -> bool {
        self.host.is_empty() && self.port.is_empty() && self.username.is_empty()
    }
}

/// One editable row of the "Tail files" section.
struct TailRow {
    /// Absolute path of the remote file to follow.
    path: Entity<TextInput>,
    /// Whether this file is coloured by rules of its own.
    ///
    /// Per file rather than per session because a log *format* is a property of
    /// the file: the access log and the application log on one host want
    /// different words picked out, and one list good for both is good for
    /// neither.
    custom_highlights: bool,
    /// The rules that tick reveals, built with the row and kept while it is
    /// unticked so that ticking it again brings back what was typed.
    highlights: Entity<HighlightRuleList>,
    /// Whether [`Self::highlights`] has ever been filled in.
    ///
    /// The first tick on a row that brought no rules of its own copies in the
    /// rules that apply to it *now*, so the user edits away from what the file
    /// was already showing rather than from a blank page. Only the first,
    /// though: a user who ticked the box, deleted every rule and unticked it
    /// meant to delete them, and re-ticking must not quietly undo that.
    seeded: bool,
    /// First tab index of this row's block, fixed at construction.
    ///
    /// Held rather than derived from the row's position, for the reason the
    /// dashboards' pane rows hold theirs: the path field took its index when it
    /// was built and cannot be renumbered, so deriving the controls beside it
    /// would put a row out of order the moment a row above it was removed.
    tab_base: isize,
}

/// The text of one followed-file row, read out of its controls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TailFields {
    /// Path as typed.
    path: String,
    /// The row's own highlight rules, or `None` while the tick is clear.
    ///
    /// Three-valued exactly as [`TailRule::highlights`] is, and for the same
    /// reason: `None` inherits, and `Some` of an empty list is a deliberate
    /// "colour nothing here".
    highlights: Option<Vec<HighlightRuleFields>>,
}

/// Turn the rows of the jump-host section into rules, or refuse.
///
/// Blank rows are dropped for the reason [`collect_tunnel_rules`] drops them:
/// the section always ends with the empty row "Add jump host" produced. Every
/// other row has to be a login that can actually be attempted — a host, a user,
/// a port in range, and a key file when the row is in key mode — because a hop
/// that cannot be authenticated does not fail on its own: it fails the *whole*
/// connection, several seconds in, from a host the user never typed.
///
/// A blank port is not an omission but the SSH default; it is the one field of
/// a hop that means something while empty.
///
/// `None` is the refusal; the caller turns it into the message strip.
fn collect_hop_rules(rows: &[HopFields]) -> Option<Vec<HopRule>> {
    let mut rules = Vec::new();
    for row in rows {
        if row.is_blank() {
            continue;
        }
        if row.host.is_empty() || row.username.is_empty() {
            return None;
        }
        let port = if row.port.is_empty() {
            DEFAULT_PORT
        } else {
            row.port.parse::<u16>().ok().filter(|port| *port != 0)?
        };
        let auth = match row.auth {
            AuthKind::Password => AuthMethod::Password,
            AuthKind::PrivateKey => {
                if row.key_path.is_empty() {
                    return None;
                }
                AuthMethod::PublicKey {
                    key_path: PathBuf::from(&row.key_path),
                }
            }
            // The picker offers two segments; see `hop_auth_options`. A hop
            // cannot be in a mode the transport has no implementation for.
            AuthKind::Agent => return None,
        };
        rules.push(HopRule {
            id: row.id,
            host: row.host.clone(),
            port,
            username: row.username.clone(),
            auth,
            save_secret: row.save_secret,
        });
    }
    Some(rules)
}

/// Turn the rows of the followed-file section into rules, or refuse.
///
/// A row with no path is dropped for the reason the two collectors above drop
/// theirs: the section always ends on the empty row "Add file" produced. It is
/// dropped *whole* — a half-written highlight rule on a row that names no file
/// cannot refuse anything, since there is no file for it to colour.
///
/// The refusal is the one a row that does name a file can now make. A pattern
/// that does not compile and a colour nothing can parse are both stored happily
/// by `rulogman-core` and both then do nothing at all, which from the far end of
/// a `tail -f` looks exactly like a rule that is merely wrong about the log —
/// so they are caught here, while the text is still on screen. See
/// [`collect_highlight_rules`].
///
/// A row whose tick is set but whose rules are all blank yields `Some(empty)`,
/// not `None`: "I cleared the rules for this one noisy file" is a decision, and
/// the empty list is how [`rulogman_core::effective_highlights`] hears it. A
/// clear tick is `None` — inherit — and is what every row and every profile
/// written before highlighting existed says.
///
/// `None` is the refusal; the caller turns it into the message strip.
fn collect_tail_rules(rows: &[TailFields]) -> Option<Vec<TailRule>> {
    let mut rules = Vec::with_capacity(rows.len());
    for row in rows {
        if row.path.is_empty() {
            continue;
        }
        let highlights = match &row.highlights {
            Some(fields) => Some(collect_highlight_rules(fields).ok()?),
            None => None,
        };
        rules.push(TailRule {
            path: row.path.clone(),
            highlights,
        });
    }
    Some(rules)
}

/// Which of the dialog's dropdown lists is currently showing.
///
/// A single field rather than one flag per dropdown, so that no two can be open
/// at once — their lists are drawn deferred and would overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenList {
    /// The per-session color scheme picker.
    Scheme,
    /// The character set picker.
    Charset,
}

/// Modal dialog for picking a saved profile or entering a new connection.
///
/// The dialog is an entity: create it once with [`ConnectionDialog::new`], keep
/// the handle, subscribe to [`ConnectionDialogEvent`], and render it as the last
/// child of a `relative()` root element so the backdrop covers the window.
///
/// It renders nothing at all while [`ConnectionDialog::is_open`] is `false`, so
/// it is safe to render unconditionally.
pub struct ConnectionDialog {
    /// Whether the dialog is currently visible.
    open: bool,
    /// Saved profiles, reloaded from disk every time the dialog opens.
    store: ProfileStore,
    /// Identifier of the profile the form was filled from, if any. Kept so that
    /// connecting updates the existing profile instead of duplicating it.
    editing: Option<Uuid>,
    /// Whether the pinned "Local terminal" row is the current selection.
    ///
    /// Mutually exclusive with [`Self::editing`]: between them they are the
    /// dialog's one selection, so every path that selects a profile clears this
    /// through [`Self::clear_local_selection`].
    #[cfg(unix)]
    local_selected: bool,
    /// Which of the pinned local rows is the current selection, as an index
    /// into [`Self::local_shells`].
    ///
    /// The Windows shape of the field above, and an index rather than a flag
    /// because there is more than one local shell to pin. Mutually exclusive
    /// with [`Self::editing`] in exactly the same way.
    #[cfg(windows)]
    local_selected: Option<usize>,
    /// Name of the user's login shell, resolved once when the dialog is built.
    ///
    /// Cached rather than looked up per frame: the lookup reads `$SHELL` and,
    /// failing that, the passwd database, and neither can change under a
    /// running application.
    #[cfg(unix)]
    local_shell: SharedString,
    /// The local shells the pinned rows offer, in the order they are rendered.
    ///
    /// Starts as the two shells every Windows machine has and grows by one row
    /// per WSL distribution when [`Self::set_wsl_distros`] delivers the
    /// discovery the shell started at launch. Held rather than rebuilt per
    /// frame because [`Self::local_selected`] indexes it.
    #[cfg(windows)]
    local_shells: Vec<LocalShell>,
    /// Authentication method currently selected in the form.
    auth_kind: AuthKind,
    /// Whether the secret should be written to the OS keychain.
    save_secret: bool,
    /// Whether a session opened from this profile shows the file panel.
    show_files: bool,
    /// Message strip shown under the form.
    status: Option<DialogStatus>,
    /// Focus of the dialog root; also the anchor for the `Escape` handler.
    focus_handle: FocusHandle,
    /// Field to focus on the next render, set when the dialog opens.
    pending_focus: Option<FocusTarget>,
    /// Scroll position of everything above the footer, so that expanding the
    /// overrides section can reveal it.
    body_scroll: ScrollHandle,
    /// Scroll position of the saved-profile column.
    ///
    /// Kept only so that the column's overlay bar has something to measure and
    /// to be dragged against; nothing else scrolls the list.
    list_scroll: ScrollHandle,
    /// Whether the body's overlay scroll indicator is on screen.
    body_scrollbar: ScrollbarState,
    /// Whether the profile column's overlay scroll indicator is on screen.
    list_scrollbar: ScrollbarState,
    /// Display name of the connection.
    name_input: Entity<TextInput>,
    /// Host name or address.
    host_input: Entity<TextInput>,
    /// TCP port; kept digits-only by an observer installed in [`Self::new`].
    port_input: Entity<TextInput>,
    /// Remote login name.
    username_input: Entity<TextInput>,
    /// Password, masked.
    password_input: Entity<TextInput>,
    /// Path of the private key file.
    key_path_input: Entity<TextInput>,
    /// Private key passphrase, masked.
    passphrase_input: Entity<TextInput>,
    /// Whether the "Session overrides" section is expanded.
    overrides_open: bool,
    /// Color scheme id for this session, or `None` to inherit the global one.
    override_scheme: Option<SharedString>,
    /// Per-session font size; blank inherits.
    override_font_size_input: Entity<TextInput>,
    /// Per-session scrollback depth; blank inherits.
    override_scrollback_input: Entity<TextInput>,
    /// Per-session `TERM`; blank inherits.
    override_term_input: Entity<TextInput>,
    /// Per-session character set label; `None` inherits, which means UTF-8.
    ///
    /// Stored as the label rather than as a [`Charset`] so that a value put
    /// into `profiles.json` by hand — an alias, or an encoding outside the
    /// offered list — survives a round trip through the form untouched.
    override_charset: Option<String>,
    /// Which dropdown of the overrides section, if any, is showing its list.
    open_list: Option<OpenList>,
    /// Scroll position of the color scheme list, so opening it reveals the
    /// scheme in force instead of the top of the catalogue.
    scheme_scroll: ScrollHandle,
    /// Scroll position of the character set list.
    ///
    /// Ten rows overrun the list's maximum height by a few pixels, so the last
    /// entry has to be scrollable into view; the handle is what lets opening
    /// the list and the arrow keys reveal it.
    charset_scroll: ScrollHandle,
    /// Whether the "Jump hosts" section is expanded.
    hops_open: bool,
    /// Editable jump hosts, in the order they are traversed.
    hop_rows: Vec<HopRow>,
    /// Whether the "SSH tunnels" section is expanded.
    tunnels_open: bool,
    /// Editable port forwardings, in the order they are rendered.
    tunnel_rows: Vec<TunnelRow>,
    /// Whether the "Tail files" section is expanded.
    tails_open: bool,
    /// Editable followed files, in the order they are rendered.
    tail_rows: Vec<TailRow>,
    /// The saved profile a right-click opened a context menu for, and where the
    /// pointer was when it did.
    ///
    /// Held by id for the same reason the empty state holds one: the menu
    /// outlives the frame that opened it, and two of its rows rearrange the very
    /// list the row was in.
    context: Option<(Uuid, Point<Pixels>)>,
}

impl ConnectionDialog {
    /// Build the dialog, loading saved profiles from disk.
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Scoped to the dialog's key context on purpose: a global `tab` binding
        // would stop the terminal from sending `\t` to the remote shell.
        BIND_KEYS.call_once(|| {
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, Some(KEY_CONTEXT)),
                KeyBinding::new("shift-tab", FocusPrev, Some(KEY_CONTEXT)),
            ]);
        });

        // The name, host, port and key-path hints spell out a sample *value*
        // and read the same in every language; the rest are words, and are the
        // ones `refresh_placeholders` revisits after a language switch.
        let name_input = Self::field(cx, "web-01".into(), false, tab::NAME);
        let host_input = Self::field(cx, "web-01.example.com".into(), false, tab::HOST);
        let port_input = Self::field(cx, "22".into(), false, tab::PORT);
        let username_input = Self::field(cx, "alice".into(), false, tab::USERNAME);
        let password_input = Self::field(
            cx,
            ts!("connection.password_placeholder"),
            true,
            tab::SECRET_OR_KEY,
        );
        let key_path_input = Self::field(cx, "~/.ssh/id_ed25519".into(), false, tab::SECRET_OR_KEY);
        let passphrase_input = Self::field(
            cx,
            ts!("connection.passphrase_placeholder"),
            true,
            tab::PASSPHRASE,
        );
        let inherit = ts!("connection.inherit_placeholder");
        let override_font_size_input =
            Self::field(cx, inherit.clone(), false, tab::OVERRIDE_FONT_SIZE);
        let override_scrollback_input =
            Self::field(cx, inherit.clone(), false, tab::OVERRIDE_SCROLLBACK);
        let override_term_input = Self::field(cx, inherit, false, tab::OVERRIDE_TERM);

        port_input.update(cx, |input, cx| {
            input.set_content(DEFAULT_PORT.to_string(), cx);
        });

        digits_only(cx, &override_scrollback_input, false, MAX_SCROLLBACK_DIGITS);
        digits_only(cx, &override_font_size_input, true, MAX_FONT_SIZE_DIGITS);

        // The text field has no input filter, so the port is sanitised after the
        // fact. Rewriting only when the text actually changes stops the observer
        // from re-triggering itself.
        digits_only(cx, &port_input, false, MAX_PORT_DIGITS);

        // The one file the window opens by reading, and the one reason a test
        // that stands a workspace up would touch the machine it runs on: the
        // dialog is built with the window, long before anybody asks to see it.
        // Under test it starts empty instead, so what the developer happens to
        // have in `profiles.json` cannot reach a rendered frame. The guard is
        // here rather than inside `load`, for the reason the update check's is
        // in `main`: `cfg!(test)` compiled into a dependency is that
        // dependency's build, and only this crate can tell a test build of the
        // application from a release one.
        let store = if cfg!(test) {
            ProfileStore::default()
        } else {
            ProfileStore::load().unwrap_or_else(|err| {
                log::warn!("starting with an empty profile store: {err:#}");
                ProfileStore::default()
            })
        };

        Self {
            open: false,
            store,
            editing: None,
            #[cfg(unix)]
            local_selected: false,
            #[cfg(windows)]
            local_selected: None,
            #[cfg(unix)]
            local_shell: SharedString::from(login_shell_name()),
            // Without the distributions for now: finding them costs a process,
            // and the shell hands them over as soon as it has them.
            #[cfg(windows)]
            local_shells: local_shells(&[]),
            auth_kind: AuthKind::Password,
            save_secret: false,
            // What [`SessionProfile::new`] gives a profile nobody has said
            // anything to yet, and what every session did before it was a
            // choice.
            show_files: true,
            status: None,
            focus_handle: cx.focus_handle(),
            pending_focus: None,
            body_scroll: ScrollHandle::new(),
            list_scroll: ScrollHandle::new(),
            body_scrollbar: ScrollbarState::new(),
            list_scrollbar: ScrollbarState::new(),
            name_input,
            host_input,
            port_input,
            username_input,
            password_input,
            key_path_input,
            passphrase_input,
            overrides_open: false,
            override_scheme: None,
            override_font_size_input,
            override_scrollback_input,
            override_term_input,
            override_charset: None,
            open_list: None,
            scheme_scroll: ScrollHandle::new(),
            charset_scroll: ScrollHandle::new(),
            hops_open: false,
            hop_rows: Vec::new(),
            tunnels_open: false,
            tunnel_rows: Vec::new(),
            tails_open: false,
            tail_rows: Vec::new(),
            context: None,
        }
    }

    /// Build one text field of the form.
    ///
    /// Every field submits the whole form, so `Enter` connects from anywhere.
    /// Also used for the tunnel rows, which are created long after the dialog
    /// itself and have to behave the same way.
    fn field(
        cx: &mut Context<Self>,
        placeholder: SharedString,
        masked: bool,
        tab_index: isize,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
        cx.new(move |cx| {
            TextInput::new(cx)
                .context_menu(input_menu_labels)
                .placeholder(placeholder)
                .masked(masked)
                .tab_index(tab_index)
                .on_submit(move |_, _window, cx| {
                    // `on_submit` fires from inside the TextInput's own
                    // `update`, which means gpui has leased that entity out of
                    // the entity map. Submitting reads every field back —
                    // including the one that fired — and a `read` of a leased
                    // entity is a hard panic. Defer to the end of the effect
                    // cycle, by which point the lease has been returned.
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        weak.update(cx, |this, cx| this.submit(cx)).ok();
                    });
                })
        })
    }

    /// Offer one pinned row per WSL distribution in `distros`, on top of the
    /// shells every Windows machine has.
    ///
    /// Called once, by the shell, when the discovery it started at launch
    /// answers — the dialog does not go looking itself, because the welcome
    /// screen needs the same list and a second `wsl.exe` would be a second
    /// process for an answer already in hand.
    ///
    /// Any local selection is dropped, because it is an index into the list
    /// being replaced. In practice this costs nothing: the discovery lands
    /// seconds into the run, long before a dialog nobody has opened yet could
    /// carry a selection.
    #[cfg(windows)]
    pub fn set_wsl_distros(&mut self, distros: &[String], cx: &mut Context<Self>) {
        self.clear_local_selection();
        self.local_shells = local_shells(distros);
        cx.notify();
    }

    /// Whether one of the pinned local rows is the current selection.
    ///
    /// Hides the shape of the selection — a flag on unix, an index on Windows —
    /// so the render path can branch on it without a platform conditional of
    /// its own.
    fn is_local_selected(&self) -> bool {
        #[cfg(unix)]
        {
            self.local_selected
        }
        #[cfg(windows)]
        {
            self.local_selected.is_some()
        }
    }

    /// Name of the shell the selected pinned row would start, if one is
    /// selected.
    ///
    /// The one thing the panel on the right needs out of the selection, and
    /// the only reason it needs no platform conditional either.
    fn selected_local_name(&self) -> Option<SharedString> {
        #[cfg(unix)]
        {
            self.local_selected.then(|| self.local_shell.clone())
        }
        #[cfg(windows)]
        {
            self.selected_local_shell().map(|shell| shell.name.clone())
        }
    }

    /// The local shell the selected pinned row would start.
    ///
    /// Looked up rather than indexed: the list is replaced when the WSL
    /// discovery answers, and a stale index must read as "nothing selected"
    /// rather than panic.
    #[cfg(windows)]
    fn selected_local_shell(&self) -> Option<&LocalShell> {
        self.local_selected
            .and_then(|index| self.local_shells.get(index))
    }

    /// Drop the pinned local rows from the selection.
    ///
    /// Always defined, so the SSH paths that call it stay free of platform
    /// conditionals.
    fn clear_local_selection(&mut self) {
        #[cfg(unix)]
        {
            self.local_selected = false;
        }
        #[cfg(windows)]
        {
            self.local_selected = None;
        }
    }

    /// Make the pinned local row the selection.
    ///
    /// The form is cleared rather than left standing: it holds whatever profile
    /// was selected before, and the local panel takes its place on screen. No
    /// field is focused afterwards because the panel has none — a pending focus
    /// left over from `open_*` would land on an unpainted input.
    #[cfg(unix)]
    fn select_local(&mut self, cx: &mut Context<Self>) {
        self.reset_form(cx);
        self.local_selected = true;
        self.pending_focus = None;
        cx.notify();
    }

    /// Make the pinned local row at `index` the selection.
    ///
    /// The Windows shape of the call above, and identical to it in everything
    /// but which of the several local shells the row stands for.
    #[cfg(windows)]
    fn select_local(&mut self, index: usize, cx: &mut Context<Self>) {
        self.reset_form(cx);
        self.local_selected = Some(index);
        self.pending_focus = None;
        cx.notify();
    }

    /// Clear every field and drop any selection.
    fn reset_form(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.clear_local_selection();
        self.auth_kind = AuthKind::Password;
        self.save_secret = false;
        self.show_files = true;
        self.status = None;

        self.name_input.update(cx, |input, cx| input.clear(cx));
        self.host_input.update(cx, |input, cx| input.clear(cx));
        self.username_input.update(cx, |input, cx| input.clear(cx));
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.key_path_input.update(cx, |input, cx| input.clear(cx));
        self.passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        self.port_input.update(cx, |input, cx| {
            input.set_content(DEFAULT_PORT.to_string(), cx);
        });

        self.overrides_open = false;
        self.override_scheme = None;
        self.override_font_size_input
            .update(cx, |input, cx| input.clear(cx));
        self.override_scrollback_input
            .update(cx, |input, cx| input.clear(cx));
        self.override_term_input
            .update(cx, |input, cx| input.clear(cx));
        self.override_charset = None;
        self.open_list = None;

        // The rows are dropped rather than emptied: they are entities of their
        // own, and the next profile brings its own set. That goes for the jump
        // hosts in particular, whose rows carry both a keychain key and a
        // typed secret — neither may follow the user to the next profile.
        self.hops_open = false;
        self.hop_rows.clear();
        self.tunnels_open = false;
        self.tunnel_rows.clear();
        self.tails_open = false;
        self.tail_rows.clear();

        self.body_scroll.scroll_to_item(0);
    }

    /// Move focus into the field recorded by the last `open_*` call.
    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.pending_focus.take() else {
            return;
        };
        let input = match (target, self.auth_kind) {
            (FocusTarget::Secret, AuthKind::Password) => &self.password_input,
            (FocusTarget::Secret, AuthKind::PrivateKey) => &self.passphrase_input,
            _ => &self.host_input,
        };
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }
}

impl EventEmitter<ConnectionDialogEvent> for ConnectionDialog {}

impl Focusable for ConnectionDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests;
