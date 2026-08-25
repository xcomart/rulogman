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

use std::path::{Path, PathBuf};
use std::sync::Once;

use gpui::{
    AnyElement, App, Context, DragMoveEvent, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseUpEvent, PathPromptOptions, Pixels, Point, Render, ScrollHandle, SharedString, Window,
    actions, div, prelude::*, px,
};
use rulogman_core::{
    AuthMethod, ProfileStore, SecretStore, SessionOverrides, SessionProfile, TunnelRule,
};
#[cfg(unix)]
use rulogman_pty::login_shell_name;
use rulogman_ssh::SshAuth;
use rulogman_term::{Charset, TerminalTheme};
use uuid::Uuid;

use crate::i18n::{input_menu_labels, ts};
#[cfg(windows)]
use crate::session::{LocalShell, local_shells};
use rugpui::{
    Button, ButtonVariant, Checkbox, ContextMenu, DraggedThumb, MenuEntry, SchemeSelect,
    SchemeSwatch, Scrollbar, ScrollbarAxis, ScrollbarState, Segmented, Select, TextInput, form_row,
    hide_later, hide_now, modal, scroll_to, scrolled, theme,
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
    /// The "SSH tunnels" disclosure button.
    pub const TUNNELS: isize = 90;
    /// First input of the first tunnel row.
    ///
    /// Every row takes [`TUNNEL_ROW_STRIDE`] indices, one per input, and is
    /// numbered from its position in the list, so the rows tab in the order
    /// they are drawn. Removing a row leaves the others where they are: the
    /// remaining indices still ascend, which is all the tab ring reads.
    pub const TUNNEL_ROWS: isize = 100;
    /// Indices one tunnel row occupies: local port, remote host, remote port.
    pub const TUNNEL_ROW_STRIDE: isize = 3;
    /// The "Add tunnel" button, past every row the numbering can reach.
    pub const TUNNEL_ADD: isize = 180;
    /// Cancel.
    pub const CANCEL: isize = 190;
    /// Connect.
    pub const CONNECT: isize = 200;
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
    /// Whether the "SSH tunnels" section is expanded.
    tunnels_open: bool,
    /// Editable port forwardings, in the order they are rendered.
    tunnel_rows: Vec<TunnelRow>,
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

        let store = ProfileStore::load().unwrap_or_else(|err| {
            log::warn!("starting with an empty profile store: {err:#}");
            ProfileStore::default()
        });

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
            tunnels_open: false,
            tunnel_rows: Vec::new(),
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

    /// The handle and bar state of one scrolling surface.
    fn surface(&mut self, surface: Surface) -> (&ScrollHandle, &mut ScrollbarState) {
        match surface {
            Surface::Body => (&self.body_scroll, &mut self.body_scrollbar),
            Surface::List => (&self.list_scroll, &mut self.list_scrollbar),
        }
    }

    /// The same pair, for the renders that only read them.
    fn surface_ref(&self, surface: Surface) -> (&ScrollHandle, &ScrollbarState) {
        match surface {
            Surface::Body => (&self.body_scroll, &self.body_scrollbar),
            Surface::List => (&self.list_scroll, &self.list_scrollbar),
        }
    }

    /// The overlay scroll indicator of one surface, as it stands.
    fn scrollbar(&self, id: &'static str, surface: Surface) -> Scrollbar {
        let (handle, state) = self.surface_ref(surface);
        Scrollbar::for_handle(id, ScrollbarAxis::Vertical, handle).fade(state.fade())
    }

    /// The same bar, listening for the pointer reaching the edge it rides.
    ///
    /// Only the bars that are drawn need it: the one the drag path builds is
    /// there to be measured, and never reaches an element tree.
    fn hovering_scrollbar(
        &self,
        id: &'static str,
        surface: Surface,
        cx: &mut Context<Self>,
    ) -> Scrollbar {
        self.scrollbar(id, surface).on_hover(cx.listener(
            move |dialog, hovered: &bool, _window, cx| {
                dialog.hover_scrollbar(surface, *hovered, cx);
            },
        ))
    }

    /// Puts each surface's bar up whenever it has been scrolled, and starts the
    /// clock that takes it down again.
    fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            let (handle, state) = self.surface(surface);
            let scrolled = scrolled(handle, ScrollbarAxis::Vertical);
            if let Some(epoch) = state.moved(scrolled) {
                hide_later(epoch, cx, move |dialog| Some(dialog.surface(surface).1));
            }
        }
    }

    /// Scrolls whichever surface's thumb has been dragged.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        for (id, surface) in SCROLLBARS {
            let Some(progress) = self.scrollbar(id, surface).dragged(event, cx) else {
                continue;
            };

            let (handle, state) = self.surface(surface);
            state.hold();
            scroll_to(handle, ScrollbarAxis::Vertical, progress);
            cx.notify();
            return;
        }
    }

    /// Lets go of whichever thumb was being held, and starts its clock again.
    fn release_scrollbars(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            if let Some(epoch) = self.surface(surface).1.release() {
                hide_later(epoch, cx, move |dialog| Some(dialog.surface(surface).1));
                cx.notify();
            }
        }
    }

    /// Puts one surface's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    ///
    /// Told which surface rather than asked to work it out: the profile column
    /// scrolls inside the body, so a pointer on the column's edge is inside both
    /// surfaces at once, and only the strip it actually reached knows which of
    /// the two bars was being asked for.
    fn hover_scrollbar(&mut self, surface: Surface, hovered: bool, cx: &mut Context<Self>) {
        let state = self.surface(surface).1;
        if hovered {
            if state.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = state.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, move |dialog| {
            Some(dialog.surface(surface).1)
        });
    }

    /// Re-translate the placeholders of the fields that have a worded one.
    ///
    /// The text fields are built once, when the dialog is created, so their
    /// hints would otherwise stay in whatever language was active at start-up
    /// after the user switches. Called from every `open_*`, which is the only
    /// moment a stale hint could become visible.
    fn refresh_placeholders(&self, cx: &mut Context<Self>) {
        self.password_input.update(cx, |input, cx| {
            input.set_placeholder(ts!("connection.password_placeholder"), cx);
        });
        self.passphrase_input.update(cx, |input, cx| {
            input.set_placeholder(ts!("connection.passphrase_placeholder"), cx);
        });
        for input in [
            &self.override_font_size_input,
            &self.override_scrollback_input,
            &self.override_term_input,
        ] {
            input.update(cx, |input, cx| {
                input.set_placeholder(ts!("connection.inherit_placeholder"), cx);
            });
        }
    }

    /// Show the dialog with an empty form.
    pub fn open_new(&mut self, cx: &mut Context<Self>) {
        self.reload_store();
        self.refresh_placeholders(cx);
        self.reset_form(cx);
        self.open = true;
        self.pending_focus = Some(FocusTarget::Host);
        cx.notify();
    }

    /// Show the dialog pre-filled from the saved profile `id`.
    ///
    /// An unknown `id` opens the empty form rather than failing.
    pub fn open_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.reload_store();
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
                if agent {
                    self.set_status(StatusLevel::Warning, ts!("connection.agent_unsupported"));
                } else if has_secret {
                    self.set_status(StatusLevel::Info, ts!("connection.saved_secret"));
                }
            }
            None => {
                log::warn!("connection dialog asked to open unknown profile {id}");
                self.pending_focus = Some(FocusTarget::Host);
            }
        }

        cx.notify();
    }

    /// Show the dialog with the saved profile `id` loaded for editing.
    ///
    /// The other half of [`Self::open_profile`]: that one is on its way to a
    /// session and puts the caret in the field a connection is still waiting
    /// on, while this one is only the form, so the caret stays where an empty
    /// form would have left it — on the first field. An unknown `id` leaves the
    /// empty form standing, which is [`Self::select_profile`]'s behaviour and
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
        self.pending_focus = None;
        // Nothing renders it while the dialog is down, so a menu left standing
        // would reappear the next time the dialog comes up.
        self.context = None;
        // Belt and braces: every `open_*` resets the form anyway, but a closed
        // dialog must not carry a selection that outlives the reason for it.
        self.clear_local_selection();
        // A closed dialog has nothing to report; leaving the last message behind
        // would let it reappear for a moment the next time the dialog opens.
        self.status = None;
        // Never keep a secret in memory longer than the dialog is on screen.
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// Saved profiles, in stored order.
    pub fn profiles(&self) -> Vec<SessionProfile> {
        self.store.profiles().to_vec()
    }

    /// Re-read the profile store so external edits are picked up when the dialog
    /// opens. A failure leaves the previously loaded profiles in place.
    fn reload_store(&mut self) {
        match ProfileStore::load() {
            Ok(store) => self.store = store,
            Err(err) => log::warn!("keeping the previously loaded profiles: {err:#}"),
        }
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
        // own, and the next profile brings its own set.
        self.tunnels_open = false;
        self.tunnel_rows.clear();

        self.body_scroll.scroll_to_item(0);
    }

    /// Copy `profile` into the form and remember that it is being edited.
    ///
    /// Secrets are never copied back into the form: an empty password field
    /// means "reuse whatever the keychain holds".
    fn fill_form(&mut self, profile: &SessionProfile, cx: &mut Context<Self>) {
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

        // Same treatment for the tunnels: a profile that forwards nothing keeps
        // the section shut, one that forwards something opens it, and either
        // way the rows of whatever profile was selected before are gone.
        self.tunnels_open = !profile.tunnels.is_empty();
        self.set_tunnel_rows(&profile.tunnels, cx);

        self.save_secret = profile.save_secret;
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
    fn collect_overrides(&self, cx: &App) -> SessionOverrides {
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

    /// Expand or collapse the "Session overrides" section.
    fn toggle_overrides(&mut self, cx: &mut Context<Self>) {
        self.overrides_open = !self.overrides_open;
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

    /// Build an empty tunnel row numbered for `position` in the list.
    fn tunnel_row(cx: &mut Context<Self>, position: usize) -> TunnelRow {
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
    fn set_tunnel_rows(&mut self, rules: &[TunnelRule], cx: &mut Context<Self>) {
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
    fn add_tunnel_row(&mut self, cx: &mut Context<Self>) {
        let row = Self::tunnel_row(cx, self.tunnel_rows.len());
        self.tunnel_rows.push(row);
        cx.notify();
    }

    /// Drop the tunnel row at `index`.
    fn remove_tunnel_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tunnel_rows.len() {
            return;
        }
        self.tunnel_rows.remove(index);
        cx.notify();
    }

    /// Expand or collapse the "SSH tunnels" section.
    fn toggle_tunnels(&mut self, cx: &mut Context<Self>) {
        self.tunnels_open = !self.tunnels_open;
        if self.tunnels_open {
            // Opening an empty section on nothing but a button says less than
            // opening it on the row the user came to fill in.
            if self.tunnel_rows.is_empty() {
                let row = Self::tunnel_row(cx, 0);
                self.tunnel_rows.push(row);
            }
            // Index of the section within the scrolled body; see `render`.
            self.body_scroll.scroll_to_item(2);
        }
        cx.notify();
    }

    /// The text of every tunnel row, in order.
    fn tunnel_fields(&self, cx: &App) -> Vec<TunnelFields> {
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
    fn tunnel_rules(&self, cx: &App) -> Option<Vec<TunnelRule>> {
        collect_tunnel_rules(&self.tunnel_fields(cx))
    }

    /// Pick the per-session scheme, or clear it back to "inherit".
    fn set_override_scheme(&mut self, id: &str, cx: &mut Context<Self>) {
        self.override_scheme = (id != INHERIT_SCHEME_ID).then(|| SharedString::from(id.to_owned()));
        cx.notify();
    }

    /// Show or hide `list`, revealing the current row as it opens so that the
    /// list does not have to be scrolled to find it.
    ///
    /// Opening one list closes the other, since both are drawn deferred and two
    /// open at once would paint over each other.
    fn set_list_open(&mut self, list: OpenList, open: bool, cx: &mut Context<Self>) {
        self.open_list = open.then_some(list);
        if open {
            let (scroll, row) = match list {
                // Asked of the catalogue rather than of the swatches the list
                // is drawn from: the two are built from the same entries in the
                // same order, offset by the one "inherit" row that leads them.
                OpenList::Scheme => {
                    let row = match self.override_scheme.as_ref() {
                        // Nothing overridden: the "inherit" row that leads the
                        // list is the one in force.
                        None => 0,
                        Some(scheme) => {
                            let selected: &str = scheme;
                            TerminalTheme::all_schemes()
                                .iter()
                                .position(|entry| entry.id == selected)
                                .map_or(0, |index| index + 1)
                        }
                    };
                    (&self.scheme_scroll, row)
                }
                OpenList::Charset => (
                    &self.charset_scroll,
                    charset_row(self.override_charset.as_deref()),
                ),
            };
            scroll.scroll_to_item(row);
        }
        cx.notify();
    }

    /// Put whichever list is showing away, and say whether there was one to put
    /// away.
    ///
    /// A list is drawn deferred, over the rest of the form, so anything that
    /// takes the user elsewhere — `Escape`, or `Tab` off the trigger — has to
    /// close it rather than leave it painted with nobody driving it.
    fn close_lists(&mut self, cx: &mut Context<Self>) -> bool {
        if self.open_list.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Replace the message strip with a single sentence.
    fn set_status(&mut self, level: StatusLevel, message: impl Into<SharedString>) {
        self.set_status_lines(level, vec![message.into()]);
    }

    /// Replace the message strip with one sentence per line.
    fn set_status_lines(&mut self, level: StatusLevel, lines: Vec<SharedString>) {
        self.status = Some(DialogStatus { level, lines });
    }

    /// Load the profile `id` into the form.
    fn select_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(profile) = self.store.get(id).cloned() else {
            return;
        };
        let has_secret = profile.save_secret;
        let agent = matches!(profile.auth, AuthMethod::Agent);
        self.fill_form(&profile, cx);
        self.status = None;
        if agent {
            self.set_status(StatusLevel::Warning, ts!("connection.agent_unsupported"));
        } else if has_secret {
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
        let Some(copy) = self.store.duplicate(id) else {
            return;
        };
        // Reported after the selection rather than before it: selecting has a
        // message of its own to put up or take down, and would clear this one.
        let written = self.store.save();
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

    /// Forget the profile `id`, together with any secret stored for it.
    ///
    /// Deleting the secret alongside the profile is what keeps the keychain from
    /// accumulating entries nothing refers to any more.
    ///
    /// Only two things can go wrong here, so all three outcomes are spelled out
    /// as whole sentences rather than clauses joined with "and": the conjunction
    /// and the clause order of such a join are not translatable.
    pub(crate) fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.store.remove(id).is_none() {
            return;
        }

        let list_error = self.store.save().err().map(|err| format!("{err:#}"));
        let secret_error = SecretStore::delete(id).err().map(|err| format!("{err:#}"));

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
    fn set_auth_kind(&mut self, kind: AuthKind, cx: &mut Context<Self>) {
        if self.auth_kind == kind {
            return;
        }
        self.auth_kind = kind;
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        self.status = match kind {
            AuthKind::Agent => Some(DialogStatus {
                level: StatusLevel::Warning,
                lines: vec![ts!("connection.agent_unsupported")],
            }),
            _ => None,
        };
        cx.notify();
    }

    /// Set the private key path, e.g. from the platform file picker.
    fn set_key_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let text = path.display().to_string();
        self.key_path_input
            .update(cx, |input, cx| input.set_content(text, cx));
        cx.notify();
    }

    /// Trimmed content of `input`.
    fn text(input: &Entity<TextInput>, cx: &App) -> String {
        input.read(cx).content().trim().to_owned()
    }

    /// The port typed into the form, or `None` when it is out of range.
    ///
    /// An empty field means [`DEFAULT_PORT`].
    fn port(&self, cx: &App) -> Option<u16> {
        let raw = Self::text(&self.port_input, cx);
        if raw.is_empty() {
            return Some(DEFAULT_PORT);
        }
        raw.parse::<u16>().ok().filter(|port| *port != 0)
    }

    /// Whether the form holds enough information to open a session.
    fn can_connect(&self, cx: &App) -> bool {
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
        // A forwarding the user started and did not finish blocks the session
        // rather than being dropped from it: see `collect_tunnel_rules`.
        self.tunnel_rules(cx).is_some()
    }

    /// `Enter` in any field: connect when the form is complete, explain why not
    /// otherwise.
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.can_connect(cx) {
            self.connect(cx);
        } else {
            self.explain_incomplete(cx);
        }
    }

    /// Fill the message strip with the reason [`Self::can_connect`] said no.
    fn explain_incomplete(&mut self, cx: &mut Context<Self>) {
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
        } else {
            ts!("connection.tunnels.incomplete")
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
    fn connect(&mut self, cx: &mut Context<Self>) {
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
        profile.save_secret = self.save_secret;
        profile.overrides = self.collect_overrides(cx);
        // The form is the whole truth about the forwardings: a rule the user
        // removed from an existing profile has to disappear from it too.
        profile.tunnels = tunnels;

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

    /// `Tab`: move focus to the next control.
    ///
    /// gpui's tab ring wraps on its own — [`Window::focus_next`] falls back to
    /// the first stop once it runs off the end — so the only thing to add is
    /// closing the dropdown the focus may be leaving.
    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        self.close_lists(cx);
        window.focus_next(cx);
    }

    /// `Shift+Tab`: move focus to the previous control, wrapping to the last.
    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.close_lists(cx);
        window.focus_prev(cx);
    }

    /// `Escape` dismisses the dialog from anywhere inside it.
    ///
    /// A row menu or an open dropdown takes the key first and only undoes
    /// itself: backing out of one must not also throw away the form behind it,
    /// which is how the workspace layers its own menus over the dialogs.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.open || event.keystroke.key != "escape" {
            return;
        }
        cx.stop_propagation();
        if self.close_context(cx) || self.close_lists(cx) {
            return;
        }
        self.dismiss(cx);
    }

    /// Open the context menu of the saved profile `id`, with its corner at `at`.
    ///
    /// The right-click deliberately does not also load the profile into the
    /// form, the way a left-click does: the menu's own Connect and Edit rows
    /// say so explicitly, and a menu that had already overwritten the form
    /// would leave a user who dismisses it worse off than before.
    fn open_context(&mut self, id: Uuid, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.context = Some((id, at));
        cx.notify();
    }

    /// Put the row menu away, and say whether there was one to put away.
    fn close_context(&mut self, cx: &mut Context<Self>) -> bool {
        if self.context.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// The open row menu, if there is one.
    ///
    /// The four commands the row already carries between its click gestures and
    /// its hover buttons, plus the copy, which has no gesture of its own. Only
    /// the profile going away can leave the menu with nothing to speak for,
    /// which is what a delete from the menu itself does.
    fn render_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let (id, position) = self.context?;
        self.store.get(id)?;
        let this = cx.entity();

        let entries = vec![
            // What a double-click on the row does.
            MenuEntry::new(ts!("connection.connect")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.select_profile(id, cx);
                        dialog.connect(cx);
                    });
                }
            }),
            // What the hover Edit button does: load the profile and put the
            // caret at the top of the form. No ellipsis — the form is already
            // on screen, so nothing further is being promised.
            MenuEntry::new(ts!("connection.edit")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.select_profile(id, cx);
                        dialog.pending_focus = Some(FocusTarget::Host);
                    });
                }
            }),
            MenuEntry::new(ts!("connection.duplicate")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| dialog.duplicate_profile(id, cx));
                }
            }),
            MenuEntry::separator(),
            MenuEntry::new(ts!("connection.delete")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| dialog.delete_profile(id, cx));
                }
            }),
        ];

        Some(
            ContextMenu::new("connection-profile-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |dialog, cx| dialog.close_context(cx));
                }),
        )
    }

    /// The saved-profile column.
    fn render_profile_list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let bar = self.hovering_scrollbar(SCROLLBARS[1].0, Surface::List, cx);
        let this = cx.entity();
        let selected = self.editing;

        let rows = self
            .store
            .profiles()
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let id = profile.id;
                let is_selected = selected == Some(id);
                let group = SharedString::from(format!("rulogman-profile-{index}"));

                div()
                    .id(ElementId::from(("connection-profile", index)))
                    .group(group.clone())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if is_selected {
                        theme.surface_active
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|style| {
                        style.bg(if is_selected {
                            theme.surface_active
                        } else {
                            theme.surface_hover
                        })
                    })
                    .on_click({
                        let this = this.clone();
                        move |event, _window, cx| {
                            let double = event.click_count() >= 2;
                            this.update(cx, |dialog, cx| {
                                dialog.select_profile(id, cx);
                                if double {
                                    dialog.connect(cx);
                                }
                            });
                        }
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let this = this.clone();
                        move |event: &MouseDownEvent, _window, cx| {
                            // The press belongs to this row, not to the list
                            // that scrolls under it.
                            cx.stop_propagation();
                            this.update(cx, |dialog, cx| {
                                dialog.open_context(id, event.position, cx);
                            });
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_grow_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(theme.text)
                                    .child(SharedString::from(profile.name.clone())),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(profile.label())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_none()
                            .gap(px(2.))
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .child(row_action(
                                ElementId::from(("connection-profile-edit", index)),
                                ts!("connection.edit"),
                                theme.text_muted,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.select_profile(id, cx);
                                            dialog.pending_focus = Some(FocusTarget::Host);
                                        });
                                    }
                                },
                            ))
                            .child(row_action(
                                ElementId::from(("connection-profile-delete", index)),
                                ts!("connection.delete"),
                                theme.danger,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.delete_profile(id, cx);
                                        });
                                    }
                                },
                            )),
                    )
            })
            .collect::<Vec<_>>();

        let empty = rows.is_empty();

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(6.))
            .w(px(LIST_WIDTH))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(ts!("connection.saved_profiles")),
            )
            .child(
                // A box of exactly the list's size, there only to hold the
                // overlay bar: the list cannot hold it itself, because its own
                // children are what scroll away underneath. Sized by the list
                // rather than stretched, so the bar's track and the bordered
                // box the eye sees are the same rectangle.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .child(
                        div()
                            .id("connection-profile-list")
                            .track_scroll(&self.list_scroll)
                            // A wheel turned over the list stops here whenever
                            // the list has anywhere to go: gpui otherwise
                            // scrolls every container under the pointer, and
                            // the body would drift along with every turn aimed
                            // at the list. gpui's own scroll handler runs
                            // before this one, so the list has already moved
                            // by the time the event is stopped. A list that
                            // fits lets the wheel through — there is nothing
                            // here for it to mean.
                            .on_scroll_wheel(cx.listener(|dialog, _, _window, cx| {
                                if dialog.list_scroll.max_offset().y > px(0.) {
                                    cx.stop_propagation();
                                }
                            }))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .p(px(4.))
                            .max_h(px(LIST_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .restrict_scroll_to_axis()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            // Pinned above everything the store holds, and
                            // separated by a rule: they are not saved profiles,
                            // they are always there, and they scroll away with
                            // them rather than staying stuck to the top of a
                            // long list.
                            .children(self.render_local_rows(cx))
                            .when(empty, |this| {
                                this.child(
                                    div()
                                        .p(px(8.))
                                        .text_size(px(12.))
                                        .text_color(theme.text_muted)
                                        .child(ts!("connection.empty_list")),
                                )
                            })
                            .children(rows),
                    )
                    .children(bar.render(&theme)),
            )
    }

    /// The pinned local rows and the rule under them.
    ///
    /// Unix pins one, the login shell. Windows pins one per shell it can start
    /// — PowerShell, `cmd`, and one per installed WSL distribution — so the
    /// dialog offers the same choice the welcome screen does, and the WSL ones
    /// appear only once [`Self::set_wsl_distros`] has been told about them.
    ///
    /// A list rather than an `Option` so that the rule is a sibling of the rows
    /// instead of being wrapped in a container that would break the list's own
    /// spacing.
    fn render_local_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();

        #[cfg(unix)]
        rows.push(Self::local_row(
            "connection-local".into(),
            ts!("connection.local.name"),
            self.local_shell.clone(),
            self.local_selected,
            |dialog, cx| dialog.select_local(cx),
            cx,
        ));

        #[cfg(windows)]
        for (index, shell) in self.local_shells.iter().enumerate() {
            rows.push(Self::local_row(
                ("connection-local", index).into(),
                shell.kind_label(),
                shell.name.clone(),
                self.local_selected == Some(index),
                move |dialog, cx| dialog.select_local(index, cx),
                cx,
            ));
        }

        // The rule belongs to the rows above it: with nothing pinned there is
        // nothing to separate the saved profiles from.
        if !rows.is_empty() {
            let rule = div()
                .h(px(1.))
                .flex_none()
                .my(px(2.))
                .mx(px(4.))
                .bg(theme(cx).border);
            rows.push(rule.into_any_element());
        }

        rows
    }

    /// One pinned local row: what kind of shell it is over the shell's own
    /// name, and the click behaviour of a saved profile row.
    ///
    /// `on_select` is what tells the rows apart — there is one of them on unix
    /// and several on Windows — and everything else about them is shared, so
    /// that a local row and a profile row cannot drift apart in looks.
    fn local_row(
        id: ElementId,
        kind: SharedString,
        shell: SharedString,
        selected: bool,
        on_select: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let this = cx.entity();

        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .py(px(6.))
            .rounded_md()
            .cursor_pointer()
            .bg(if selected {
                theme.surface_active
            } else {
                gpui::transparent_black()
            })
            .hover(move |style| {
                style.bg(if selected {
                    theme.surface_active
                } else {
                    theme.surface_hover
                })
            })
            // Selected on a single click, opened on a double one, exactly
            // like a saved profile row.
            .on_click(move |event, _window, cx| {
                let double = event.click_count() >= 2;
                this.update(cx, |dialog, cx| {
                    on_select(dialog, cx);
                    if double {
                        dialog.connect(cx);
                    }
                });
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(theme.text)
                            .child(kind),
                    )
                    // The shell's name is a value, not a word: never
                    // translated, and shown where a profile shows its
                    // `user@host`.
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(shell),
                    ),
            )
            .into_any_element()
    }

    /// The right-hand side of the dialog: the connection form, or the local
    /// panel while a pinned row is selected.
    fn render_target_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.selected_local_name() {
            Some(shell) => Self::render_local_panel(shell, cx).into_any_element(),
            None => self.render_form(cx).into_any_element(),
        }
    }

    /// What stands in for the form once a pinned local row is selected.
    ///
    /// Deliberately has no controls: a local session takes no configuration,
    /// so the panel only says what pressing Connect will do — with `shell` the
    /// name of the shell it will start.
    fn render_local_panel(shell: SharedString, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);

        // Two sentences for one thought, because only unix can call the shell
        // the user's login shell: there it is the one they were given, here it
        // is the one they just picked from several.
        #[cfg(unix)]
        let hint = ts!("connection.local.hint", shell = shell);
        #[cfg(windows)]
        let hint = ts!("connection.local.hint_shell", shell = shell);

        div()
            .flex()
            .flex_col()
            .flex_grow_1()
            .min_w_0()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(theme.text)
                    .child(ts!("connection.local.title")),
            )
            .child(
                div()
                    .max_w(px(380.))
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(hint),
            )
    }

    /// The connection form.
    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let auth_kind = self.auth_kind;

        let auth_control = Segmented::new("connection-auth")
            .options(auth_options())
            .selected(auth_kind.index())
            .tab_index(tab::AUTH)
            .on_select({
                let this = this.clone();
                move |index, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_auth_kind(AuthKind::from_index(index), cx);
                    });
                }
            });

        let key_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .w_full()
            .child(
                div()
                    .flex_grow_1()
                    .min_w_0()
                    .child(self.key_path_input.clone()),
            )
            .child(
                Button::new("connection-browse", ts!("connection.browse"))
                    .variant(ButtonVariant::Secondary)
                    .tab_index(tab::BROWSE)
                    .on_click({
                        let this = this.clone();
                        move |_, _window, cx| browse_for_key(this.clone(), cx)
                    }),
            );

        let secret_label = match auth_kind {
            AuthKind::PrivateKey => ts!("connection.remember_passphrase"),
            _ => ts!("connection.remember_password"),
        };

        let remember = Checkbox::new("connection-remember", secret_label)
            .checked(self.save_secret)
            .tab_index(tab::REMEMBER)
            .on_toggle({
                let this = this.clone();
                move |checked, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.save_secret = checked;
                        cx.notify();
                    });
                }
            });

        div()
            .flex()
            .flex_col()
            .flex_grow_1()
            .min_w_0()
            .gap(px(10.))
            .child(form_row(ts!("connection.name"), self.name_input.clone()))
            .child(form_row(ts!("connection.host"), self.host_input.clone()))
            .child(form_row(ts!("connection.port"), self.port_input.clone()))
            .child(form_row(
                ts!("connection.username"),
                self.username_input.clone(),
            ))
            .child(form_row(ts!("connection.authentication"), auth_control))
            .when(auth_kind == AuthKind::Password, |this| {
                this.child(form_row(
                    ts!("connection.password"),
                    self.password_input.clone(),
                ))
            })
            .when(auth_kind == AuthKind::PrivateKey, |this| {
                this.child(form_row(ts!("connection.key_file"), key_row))
                    .child(form_row(
                        ts!("connection.passphrase"),
                        self.passphrase_input.clone(),
                    ))
            })
            .when(auth_kind == AuthKind::Agent, |this| {
                this.child(form_row(
                    "",
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child(ts!("connection.agent_unsupported")),
                ))
            })
            .when(auth_kind != AuthKind::Agent, |this| {
                this.child(form_row("", remember))
            })
    }

    /// The message strip and the action buttons.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let connectable = self.can_connect(cx);

        // One element per sentence: a status that reports several problems
        // stacks them instead of running them together on one line.
        let status = self.status.as_ref().map(|status| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .text_size(px(12.))
                .text_color(status.level.color(&theme))
                .children(status.lines.iter().map(|line| div().child(line.clone())))
        });

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(10.))
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .children(status)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("connection-cancel", ts!("common.cancel"))
                            .variant(ButtonVariant::Secondary)
                            .tab_index(tab::CANCEL)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.dismiss(cx));
                                }
                            }),
                    )
                    .child(
                        Button::new("connection-connect", ts!("connection.connect"))
                            .variant(ButtonVariant::Primary)
                            .disabled(!connectable)
                            .tab_index(tab::CONNECT)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.connect(cx));
                                }
                            }),
                    ),
            )
    }

    /// The collapsible "Session overrides" section.
    ///
    /// Collapsed by default. Nothing inside a collapsed section is painted, so
    /// its controls drop out of the tab ring on their own.
    fn render_overrides(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let open = self.overrides_open;
        let defaults = crate::app_settings::current(cx).terminal;

        let caret = if open { "\u{25be}" } else { "\u{25b8}" };
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

        let toggle = Button::new(
            "connection-overrides-toggle",
            format!("{caret}  {}", ts!("connection.overrides.title")),
        )
        .variant(ButtonVariant::Ghost)
        .tab_index(tab::OVERRIDES)
        .on_click({
            let this = this.clone();
            move |_, _window, cx| {
                this.update(cx, |dialog, cx| dialog.toggle_overrides(cx));
            }
        });

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
            .pt(px(10.))
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

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .pt(px(8.))
                    .child(toggle)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(summary),
                    ),
            )
            .children(open.then_some(body))
    }

    /// The collapsible "SSH tunnels" section.
    ///
    /// Laid out as a table rather than as a stack of [`form_row`]s: a rule is
    /// three values read together, and one label per input would put nine of
    /// them on screen for three rules.
    fn render_tunnels(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let open = self.tunnels_open;

        let caret = if open { "\u{25be}" } else { "\u{25b8}" };
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

        let toggle = Button::new(
            "connection-tunnels-toggle",
            format!("{caret}  {}", ts!("connection.tunnels.title")),
        )
        .variant(ButtonVariant::Ghost)
        .tab_index(tab::TUNNELS)
        .on_click({
            let this = this.clone();
            move |_, _window, cx| {
                this.update(cx, |dialog, cx| dialog.toggle_tunnels(cx));
            }
        });

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
            .pt(px(10.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(ts!("connection.tunnels.hint")),
            )
            .when(!rows.is_empty(), |this| this.child(header))
            .children(rows)
            .child(div().flex().flex_row().pt(px(2.)).child(add));

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .pt(px(8.))
                    .child(toggle)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(summary),
                    ),
            )
            .children(open.then_some(body))
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

impl Render for ConnectionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().id("connection-dialog");
        }

        self.apply_pending_focus(window, cx);
        self.watch_scroll(cx);
        let theme = theme(cx);
        let body_bar = self.hovering_scrollbar(SCROLLBARS[0].0, Surface::Body, cx);

        let local = self.is_local_selected();
        let title = if local {
            // Neither "New connection" nor "Connect": nothing is being
            // connected to, and nothing is being created.
            ts!("connection.local.name")
        } else if self.editing.is_some() {
            ts!("connection.title_edit")
        } else {
            ts!("connection.title_new")
        };

        // Only the form scrolls; the footer stays put. The modal caps the panel
        // at the window height, and the `min_h_0` chain from here down is what
        // turns that cap into a scrolling body instead of a clipped one.
        let body = div()
            .flex()
            .flex_col()
            .min_h_0()
            .gap(px(12.))
            .child(
                // The middle box exists only to hold the body's overlay bar,
                // for the same reason the profile column has one of its own.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(
                        div()
                            .id("connection-body")
                            .track_scroll(&self.body_scroll)
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .gap(px(12.))
                            .overflow_y_scroll()
                            .restrict_scroll_to_axis()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_none()
                                    .items_start()
                                    .gap(px(16.))
                                    .child(self.render_profile_list(cx))
                                    .child(self.render_target_panel(cx)),
                            )
                            // A local session is never saved, so there is
                            // nothing for a per-session override to be attached
                            // to — and nothing to forward a port over either.
                            .children((!local).then(|| self.render_overrides(cx)))
                            .children((!local).then(|| self.render_tunnels(cx))),
                    )
                    .children(body_bar.render(&theme)),
            )
            .child(self.render_footer(cx));

        let on_dismiss = {
            let this = cx.entity();
            move |_window: &mut Window, cx: &mut App| {
                this.update(cx, |dialog, cx| dialog.dismiss(cx));
            }
        };

        // The wrapper exists only to own the focus handle and the `Escape`
        // binding. It has to span its parent, because an absolutely positioned
        // element is laid out against its direct parent: a shrink-to-fit
        // wrapper would collapse to zero height and drag the modal off-screen
        // with it.
        div()
            .id("connection-dialog")
            .key_context(KEY_CONTEXT)
            .absolute()
            .inset_0()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_key_down(cx.listener(Self::on_key_down))
            // Both overlay bars are answered from here: gpui hands a drag move
            // to every listener of that type wherever it sits, and this is the
            // one element mounted for the whole of either drag — the profile
            // column is rebuilt from scratch whenever the list changes under it,
            // and the body scrolls away under its own bar.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |dialog, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    dialog.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbars(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbars(cx);
                }),
            )
            .child(modal(
                "connection-modal",
                title,
                px(DIALOG_WIDTH),
                body,
                on_dismiss,
            ))
            // Deferred inside, so it paints over the modal whatever its place
            // in this list, and positioned in window coordinates — which the
            // wrapper spans, so the two agree.
            .children(self.render_context(cx))
    }
}

/// Lays a "what this field inherits" hint out to the right of a control.
fn inherit_hint<E: IntoElement>(
    control: E,
    hint: SharedString,
    cx: &App,
) -> impl IntoElement + use<E> {
    let theme = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .w_full()
        .child(div().flex_1().min_w_0().child(control))
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .child(hint),
        )
}

/// Renders `value` without a trailing `.0`, so 14.0 shows as "14".
fn format_number(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// Installs an observer that keeps `input` numeric.
///
/// The text field has no input filter, so the content is rewritten after every
/// edit. Rewriting only when the text actually changes stops the observer from
/// re-triggering itself.
fn digits_only(
    cx: &mut Context<ConnectionDialog>,
    input: &Entity<TextInput>,
    decimals: bool,
    max_len: usize,
) {
    cx.observe(input, move |_this, input, cx| {
        let content = input.read(cx).content().to_owned();
        let mut seen_dot = false;
        let filtered: String = content
            .chars()
            .filter(|c| {
                if c.is_ascii_digit() {
                    true
                } else if decimals && *c == '.' && !seen_dot {
                    seen_dot = true;
                    true
                } else {
                    false
                }
            })
            .take(max_len)
            .collect();
        if filtered != content {
            input.update(cx, |input, cx| input.set_content(filtered, cx));
        }
    })
    .detach();
}

/// A compact text button used inside the profile rows.
///
/// The mouse-down handler stops propagation so that clicking an action does not
/// also select the row it lives in.
fn row_action(
    id: ElementId,
    label: SharedString,
    color: Hsla,
    hover: Hsla,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(18.))
        .px(px(6.))
        .rounded_sm()
        .whitespace_nowrap()
        .text_size(px(11.))
        .text_color(color)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _window, cx| on_click(cx))
        .child(label)
}

/// Ask the platform for a private key file and write the choice into `dialog`.
///
/// The picker is asynchronous, so the result arrives on a spawned task; a
/// cancelled dialog simply leaves the field untouched.
fn browse_for_key(dialog: Entity<ConnectionDialog>, cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(ts!("connection.select_file")),
    });

    cx.spawn(async move |cx| {
        let selection = match paths.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            Ok(Ok(None)) => None,
            Ok(Err(err)) => {
                log::warn!("the file picker could not be opened: {err:#}");
                None
            }
            Err(_) => None,
        };
        let Some(path) = selection else {
            return;
        };
        dialog.update(cx, |dialog, cx| dialog.set_key_path(path, cx));
    })
    .detach();
}

/// Credentials for `profile` that need nothing from the user, if there are any.
///
/// This is what lets a click on a saved profile open a session directly rather
/// than a dialog pre-filled with a profile the user already finished filling in
/// once. `None` means "ask": the caller should fall back to opening the dialog
/// on this profile.
///
/// `None` is also the answer whenever anything is uncertain — an unreadable
/// keychain, a missing key file, an encrypted key with no remembered
/// passphrase. Erring that way costs the user only the dialog they used to get
/// anyway, whereas erring the other way strands them in a session tab that
/// failed to authenticate and offers nowhere to type the missing secret in.
///
/// Runs on the UI thread and blocks it: reading the keychain is a synchronous
/// platform call, and a key profile also reads and parses the key file. Both
/// are what the dialog's own Connect button already does on click, so the cost
/// is not new — it has only moved one click earlier.
pub fn saved_credentials(profile: &SessionProfile) -> Option<SshAuth> {
    // A secret is only ever written for a profile that asked for one, so an
    // unticked `save_secret` means there is nothing to look up — and asking
    // anyway would raise the platform's keychain-unlock prompt for nothing.
    let stored = profile
        .save_secret
        .then(|| stored_secret(profile.id))
        .flatten();

    match decide_credentials(&profile.auth, stored, key_opens_unlocked) {
        Credentials::Ready(auth) => Some(auth),
        Credentials::Ask => None,
    }
}

/// Whether a profile can be connected without asking the user anything.
///
/// A named decision rather than an `Option<SshAuth>` so that the two outcomes
/// read as what they are at the point they are made: [`Credentials::Ask`] is
/// not "there are no credentials", it is "the dialog has to open".
#[derive(Debug)]
enum Credentials {
    /// Everything the transport needs is already known; connect straight away.
    Ready(SshAuth),
    /// Something is missing, unreadable or locked: open the dialog.
    Ask,
}

/// Decide whether what is known about a profile is enough to connect with.
///
/// Split out from [`saved_credentials`] so the policy can be exercised without
/// a keychain, a filesystem or a real key to parse. `stored_secret` is the
/// profile's remembered password or passphrase, already reduced to `None` when
/// it is absent, empty or unreadable; `key_opens_unlocked` is consulted only in
/// the single case that needs it, so a passphrase that is already known never
/// pays for a key parse.
fn decide_credentials<F>(
    method: &AuthMethod,
    stored_secret: Option<String>,
    key_opens_unlocked: F,
) -> Credentials
where
    F: FnOnce(&Path) -> bool,
{
    match method {
        // Password authentication has no unauthenticated form to fall back on:
        // without the password there is simply nothing to attempt.
        AuthMethod::Password => match stored_secret {
            Some(password) => Credentials::Ready(SshAuth::Password(password)),
            None => Credentials::Ask,
        },
        AuthMethod::PublicKey { key_path } => {
            if let Some(passphrase) = stored_secret {
                return Credentials::Ready(SshAuth::PrivateKeyFile {
                    path: key_path.clone(),
                    passphrase: Some(passphrase),
                });
            }
            // No remembered passphrase means one of two opposite things: either
            // the key needs none and is ready to use, or it needs one that only
            // the user can supply. The file itself is the only place that
            // answer exists.
            if key_opens_unlocked(key_path) {
                Credentials::Ready(SshAuth::PrivateKeyFile {
                    path: key_path.clone(),
                    passphrase: None,
                })
            } else {
                Credentials::Ask
            }
        }
        // `rulogman-ssh` has no agent transport yet, so there is nothing to
        // connect with; the dialog is where that is explained.
        AuthMethod::Agent => Credentials::Ask,
    }
}

/// The profile's remembered secret, or `None` when there is none to be had.
///
/// An empty entry counts as absent: it authenticates nothing, and connecting
/// with it would only produce a failed session. A keychain that refuses to
/// answer is treated the same way and logged, because the recovery — open the
/// dialog, type the secret — is identical either way, and the reason belongs in
/// the log rather than in the user's path. The error comes from the store's own
/// failure to read and never carries the secret.
fn stored_secret(id: Uuid) -> Option<String> {
    match SecretStore::get(id) {
        Ok(secret) => secret.filter(|secret| !secret.is_empty()),
        Err(err) => {
            log::warn!("no stored secret for {id}, so the dialog will ask: {err:#}");
            None
        }
    }
}

/// Whether the private key at `path` can be read and decoded with no passphrase.
///
/// Whether an OpenSSH key is encrypted is not visible without decoding it,
/// which is exactly what the session worker would do a moment later; doing it
/// once here, on a file of at most a few kilobytes, is nothing next to opening
/// the connection it decides. A key that cannot be read at all — moved,
/// renamed, or no longer readable — answers `false` too, which routes the user
/// to the dialog, where the path can be corrected.
///
/// No passphrase is passed in, so nothing secret can reach the log line.
fn key_opens_unlocked(path: &Path) -> bool {
    match russh::keys::load_secret_key(path, None) {
        Ok(_) => true,
        Err(err) => {
            log::debug!("the key at {} needs the dialog: {err}", path.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::fs;

    use russh::keys::ssh_key::rand_core::{TryCryptoRng, TryRng};
    use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};

    use super::*;

    /// Panics if consulted: the passphrase-is-known paths must not touch disk.
    fn never_probed(_: &Path) -> bool {
        panic!("the key file was read even though the passphrase was known");
    }

    /// Deterministic stand-in for a system RNG, for building test keys only.
    ///
    /// `ssh_key` needs *an* RNG to generate a key and to salt the KDF of an
    /// encrypted one. The tests do not care that the bytes are unpredictable,
    /// only that they exist, so an xorshift generator keeps them free of a
    /// randomness dependency and keeps their failures reproducible. It is
    /// cryptographically worthless and confined to `#[cfg(test)]`.
    struct TestRng(u64);

    impl TryRng for TestRng {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            Ok(self.try_next_u64()? as u32)
        }

        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            Ok(self.0)
        }

        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
            for chunk in dst.chunks_mut(8) {
                let word = self.try_next_u64()?.to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
            Ok(())
        }
    }

    impl TryCryptoRng for TestRng {}

    /// The password of a [`Credentials::Ready`] password decision.
    fn ready_password(credentials: Credentials) -> String {
        match credentials {
            Credentials::Ready(SshAuth::Password(password)) => password,
            other => panic!("expected a ready password, got {other:?}"),
        }
    }

    /// The passphrase of a [`Credentials::Ready`] key decision.
    fn ready_passphrase(credentials: Credentials) -> Option<String> {
        match credentials {
            Credentials::Ready(SshAuth::PrivateKeyFile { passphrase, .. }) => passphrase,
            other => panic!("expected a ready key, got {other:?}"),
        }
    }

    #[test]
    fn a_remembered_password_connects_without_asking() {
        let decision = decide_credentials(
            &AuthMethod::Password,
            Some("hunter2".to_owned()),
            never_probed,
        );
        assert_eq!(ready_password(decision), "hunter2");
    }

    #[test]
    fn a_password_profile_with_nothing_remembered_asks() {
        // `None` is what both a profile that never asked to remember its
        // password and one whose keychain entry is empty arrive as.
        let decision = decide_credentials(&AuthMethod::Password, None, never_probed);
        assert!(matches!(decision, Credentials::Ask));
    }

    #[test]
    fn a_remembered_passphrase_connects_without_reading_the_key() {
        let method = AuthMethod::PublicKey {
            key_path: PathBuf::from("/home/me/.ssh/id_ed25519"),
        };
        let decision = decide_credentials(&method, Some("open sesame".to_owned()), never_probed);
        assert_eq!(ready_passphrase(decision).as_deref(), Some("open sesame"));
    }

    #[test]
    fn an_unencrypted_key_connects_with_no_passphrase_at_all() {
        let method = AuthMethod::PublicKey {
            key_path: PathBuf::from("/home/me/.ssh/id_ed25519"),
        };
        let decision = decide_credentials(&method, None, |_| true);
        assert_eq!(ready_passphrase(decision), None);
    }

    #[test]
    fn an_encrypted_key_with_no_remembered_passphrase_asks() {
        // The case the whole probe exists for: connecting anyway would fail to
        // load the key in a tab that cannot ask for the passphrase.
        let method = AuthMethod::PublicKey {
            key_path: PathBuf::from("/home/me/.ssh/id_ed25519"),
        };
        let decision = decide_credentials(&method, None, |_| false);
        assert!(matches!(decision, Credentials::Ask));
    }

    #[test]
    fn agent_authentication_always_asks() {
        // Nothing can be remembered for a method the transport cannot perform.
        let decision = decide_credentials(&AuthMethod::Agent, None, never_probed);
        assert!(matches!(decision, Credentials::Ask));
    }

    /// A finished row, as the three inputs would be read.
    fn typed(local_port: &str, remote_host: &str, remote_port: &str) -> TunnelFields {
        TunnelFields {
            local_port: local_port.to_owned(),
            remote_host: remote_host.to_owned(),
            remote_port: remote_port.to_owned(),
            bind_address: DEFAULT_BIND_ADDRESS.to_owned(),
        }
    }

    #[test]
    fn a_finished_tunnel_row_becomes_a_rule() {
        let rules = collect_tunnel_rules(&[typed("15432", "db.internal", "5432")])
            .expect("the row is complete");
        assert_eq!(
            rules,
            vec![TunnelRule {
                bind_address: DEFAULT_BIND_ADDRESS.to_owned(),
                local_port: 15432,
                remote_host: "db.internal".to_owned(),
                remote_port: 5432,
            }]
        );
    }

    #[test]
    fn untouched_tunnel_rows_are_dropped_without_complaint() {
        // The section always ends with the empty row "Add tunnel" produced, so
        // an untouched one must not stop the connection.
        let rows = [
            typed("15432", "db.internal", "5432"),
            TunnelFields::default(),
        ];
        let rules = collect_tunnel_rules(&rows).expect("the blank row is ignored");
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn a_half_written_tunnel_row_is_refused() {
        // Each of the three fields on its own: dropping any of these would open
        // a session the user believes forwards a port it does not.
        assert!(collect_tunnel_rules(&[typed("15432", "", "")]).is_none());
        assert!(collect_tunnel_rules(&[typed("", "db.internal", "")]).is_none());
        assert!(collect_tunnel_rules(&[typed("15432", "db.internal", "")]).is_none());
        assert!(collect_tunnel_rules(&[typed("", "db.internal", "5432")]).is_none());
    }

    #[test]
    fn a_tunnel_port_out_of_range_is_refused() {
        // Port 0 binds whatever the operating system feels like, which is not
        // what anyone typing a forwarding means; 65536 does not exist at all.
        assert!(collect_tunnel_rules(&[typed("0", "db.internal", "5432")]).is_none());
        assert!(collect_tunnel_rules(&[typed("15432", "db.internal", "0")]).is_none());
        assert!(collect_tunnel_rules(&[typed("65536", "db.internal", "5432")]).is_none());
    }

    #[test]
    fn a_hand_written_bind_address_survives_an_edit() {
        // The form never shows the address, so the only way it can survive the
        // user changing the port beside it is by being carried on the row.
        let mut row = typed("8080", "127.0.0.1", "80");
        row.bind_address = "0.0.0.0".to_owned();
        let rules = collect_tunnel_rules(&[row]).expect("the row is complete");
        assert_eq!(rules[0].bind_address, "0.0.0.0");
    }

    #[test]
    fn the_probe_tells_an_encrypted_key_from_a_plain_one() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let mut rng = TestRng(0x5eed_1eaf_c0ff_ee01);
        let plain = PrivateKey::random(&mut rng, Algorithm::Ed25519).expect("a generated key");

        let plain_path = dir.path().join("id_ed25519");
        fs::write(
            &plain_path,
            plain
                .to_openssh(LineEnding::LF)
                .expect("an OpenSSH key")
                .as_bytes(),
        )
        .expect("the key file is written");
        assert!(key_opens_unlocked(&plain_path));

        let locked_path = dir.path().join("id_ed25519_locked");
        let locked = plain
            .encrypt(&mut rng, "open sesame")
            .expect("an encrypted key");
        fs::write(
            &locked_path,
            locked
                .to_openssh(LineEnding::LF)
                .expect("an OpenSSH key")
                .as_bytes(),
        )
        .expect("the key file is written");
        assert!(!key_opens_unlocked(&locked_path));

        // A path with no key behind it is the third way the probe says no.
        assert!(!key_opens_unlocked(&dir.path().join("absent")));
    }

    #[test]
    fn the_charset_rows_and_their_overrides_agree() {
        // Row 0 inherits, and nothing about it is an encoding.
        assert_eq!(charset_at(0), None);
        assert_eq!(charset_row(None), 0);

        // Every offered encoding round-trips: the row it is found on is the row
        // that yields it back, and the stored label is its canonical name.
        for (index, charset) in Charset::SUPPORTED.iter().enumerate() {
            let row = index + 1;
            let stored = charset_at(row).expect("an offered row names an encoding");
            assert_eq!(stored, charset.name());
            assert_eq!(charset_row(Some(&stored)), row);
        }

        // A row past the list belongs to nobody rather than wrapping onto one.
        assert_eq!(charset_at(Charset::SUPPORTED.len() + 1), None);

        // An alias resolves to the row of the encoding it names, so a label put
        // into `profiles.json` by hand still highlights something.
        assert_eq!(charset_row(Some("euc-kr")), charset_row(Some("EUC-KR")));
        assert_eq!(
            charset_row(Some("windows-949")),
            charset_row(Some("EUC-KR"))
        );

        // A label the registry does not know falls back to UTF-8, which is
        // itself offered, so it lands on that row rather than on the inherit one.
        assert_eq!(charset_row(Some("not-an-encoding")), 1);

        // One the registry knows but the list does not offer has no row; the
        // list opens at the top for it.
        assert!(Charset::for_label("koi8-r").is_some());
        assert_eq!(charset_row(Some("koi8-r")), 0);
    }
}
