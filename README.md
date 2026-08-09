# <img src="assets/icon.svg" width="28" alt=""> logman

[![CI](https://github.com/xcomart/logman/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/xcomart/logman/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/xcomart/logman)](https://github.com/xcomart/logman/releases)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational)

A multi-platform GUI SSH terminal written in Rust, built on
[gpui](https://gpui.rs) — the GPU-accelerated UI framework behind the Zed editor.

![logman with one tab split into two panes — a shell listing a directory beside vim editing nginx.conf — and the files panel down the left, in the One Dark theme](docs/screenshots/main-dark.png)

<details>
<summary>The settings dialog: theme cards, title bar, language, opacity, and the terminal schemes under them</summary>

![The settings dialog over a live session, six UI theme cards with live palette previews above the duplicate, edit, delete, import and export row, and the terminal colour schemes below](docs/screenshots/settings.png)

</details>

<details>
<summary>The built-in editor: a file on the server, in a tab of its own</summary>

![A .bashrc open in an editor tab with line numbers and shell syntax highlighting, the files panel of the same session beside it, and the file type, character set and caret position at the right of the status bar](docs/screenshots/editor.png)

</details>

<details>
<summary>A host that speaks EUC-KR: raw bytes in, Korean on the grid</summary>

![A session on an EUC-KR profile where printf of raw EUC-KR bytes renders on the next line as Korean text](docs/screenshots/euc-kr-session.png)

</details>

## What it does

What follows is the tour. [docs/user-guide.md](docs/user-guide.md) is the
manual — every screen, setting and shortcut in full — and each paragraph below
links into the part of it that covers the same ground.

**Profiles and connecting.** Press <kbd>Ctrl</kbd>+<kbd>T</kbd>
(<kbd>Cmd</kbd>+<kbd>T</kbd> on macOS), give the dialog a host, a user and
either a password or a private key, and connect. The profile is saved as you go,
so the second connection to that host is one click from the start screen — and
a profile whose credentials are already to hand, a remembered password or a key
that needs no passphrase, connects from that list without the dialog opening at
all. See [Getting started](docs/user-guide.md#getting-started).

**A shell on this computer, too.** Not every terminal is on another machine. The
start screen and the connection dialog both offer the shells this computer can
start — your login shell on Linux and macOS; PowerShell, `cmd` and one row per
installed WSL distribution on Windows — and choosing one opens it in a tab like
any other session, with no host to reach and nothing to authenticate. A WSL row
opens a Linux shell standing in the distribution's own filesystem, which the
files panel beside it browses as such. See
[A shell on this computer](docs/user-guide.md#a-shell-on-this-computer).

**Tabs and split panes.** Every session gets a tab, with its own connection and
its own scrollback, and the tab strip doubles as the window's title bar in the
VS Code manner — with the system caption one setting away for those who prefer
it. One shortcut splits the focused pane into a second connection to the same
host, and a right-click pulls an existing tab in beside the one you are looking
at instead; the divider between two panes drags to give either as much room as
it needs. A pane closes itself when its connection ends, while a session that
*failed* to connect stays put with the error and a **Reconnect** button. See
[Tabs and sessions](docs/user-guide.md#tabs-and-sessions) and
[Split panes](docs/user-guide.md#split-panes).

**Port forwarding saved with the profile.** Each rule listens on a port of this
computer and forwards it through the session to a host the server can reach, and
the rules open as soon as the shell is up. The tab holding them says so with a
mark that names them, and it is only ever one tab: a second session on the same
profile connects as usual and leaves the ports where they are, rather than
fighting for them. When that tab goes, the next session on the profile picks
them up. See [Port forwarding](docs/user-guide.md#port-forwarding).

**Settings that belong to one host.** A profile can override the color scheme,
the font size, the scrollback depth, `TERM` and the character set for its
sessions alone; every field is blank by default, and blank inherits the global
value. The character set is the one with nothing global behind it, deliberately
— it describes a host rather than a preference, so a host whose locale reads
`ko_KR.euc-kr` gets **EUC-KR** on its own profile and everything you type,
paste or compose leaves in the encoding it came in. See
[Session overrides](docs/user-guide.md#session-overrides).

**A files panel beside the terminal.** It browses the filesystem of the session
in the focused pane — a server over SFTP on a channel of the same connection,
this computer off its own disk, a WSL distribution through the share Windows
already serves for it — and it follows the shell's `cd` when the shell announces
one. The header path is a breadcrumb whose every piece drops down the
directories beside it, whole folders can be dragged in or saved out with a
progress bar while they move, and each session keeps its own directory,
selection and scroll position. See
[The files panel](docs/user-guide.md#the-files-panel).

**An editor built in.** Right-click a file in the panel and pick **Edit**, and
it opens in a tab of its own with line numbers, undo, find and replace, a
comment toggle and syntax highlighting for sixteen formats, drawn in the
session's own color scheme and terminal font; <kbd>Ctrl</kbd>+<kbd>S</kbd>
writes it back over the connection it came from. Files up to 10 MB open, in
UTF-8 or one of eight legacy character sets, and the status bar names both the
character set and the file type — either one can be changed there when the guess
was wrong. Beyond the sixteen, a language of your own is one YAML file in the
`syntaxes` directory. See [The editor](docs/user-guide.md#the-editor) and
[Defining a language](docs/user-guide.md#defining-a-language).

**A real terminal, not a log view.** `alacritty_terminal` drives the emulation,
so colors, cursor addressing, the alternate screen and full-screen programs —
vim, tmux, htop, less — behave the way they do in any other terminal. Selection,
copy on select, bracketed paste and a scrollback as deep as you set it all work
as expected, and dead keys, compose sequences and IMEs go through the platform's
own text input path. See [The terminal](docs/user-guide.md#the-terminal).

**Themes and color schemes, picked independently.** The UI theme colors the
chrome and the color scheme colors the terminal grid; six of each ship under
matching names, and every card in either picker previews the palette it stands
for. Both are files beyond that, and **scheme files are Windows Terminal's
format**, so the thousands of palettes published for it work unchanged. Nothing
has to be edited by hand either: duplicate, edit, delete, import and export sit
under each picker, and a palette saved in the editor repaints the window or
every open session at once. See
[Themes and colour schemes](docs/user-guide.md#themes-and-colour-schemes).

**Your language and your font.** The interface ships in eight languages and
follows the system locale unless told otherwise, the terminal font is picked
from the fonts actually installed on the machine, and the rest of the settings
dialog covers the title bar style, window opacity and blur, scrollback, `TERM`,
copy-on-select and the defaults new connections start from. Everything lands in
a `settings.json` that is meant to be edited by hand, where out-of-range values
are clamped rather than allowed to break the application. See
[Settings](docs/user-guide.md#settings).

**Driven from the keyboard.** Tabs, panes, splits, the files panel, the settings
dialog and the editor all have shortcuts, chosen to stay out of the remote
shell's way — the pane commands avoid a bare <kbd>Ctrl</kbd> because
<kbd>Ctrl</kbd>+<kbd>[</kbd> is ESC to a shell, and the files panel takes a
shifted chord because <kbd>Ctrl</kbd>+<kbd>B</kbd> is tmux's prefix key. Every
icon button names itself and its shortcut when the pointer rests on it. See
[Keyboard shortcuts](docs/user-guide.md#keyboard-shortcuts).

**Host keys and secrets.** Host keys are checked on the trust-on-first-use
convention, recorded per host, port and algorithm in a `known_hosts` file of
logman's own; a changed fingerprint aborts the connection rather than prompting,
and logs both the stored and the presented key. Passwords and key passphrases
are never written to any of the configuration files — they go to the OS
credential store, and only when you ask for them to be remembered. See
[Data and security](docs/user-guide.md#data-and-security).

**It can update itself.** logman asks GitHub once per launch whether a newer
release exists, silently ignoring every way that question can fail, and offers
the answer in a dialog you can act on, defer, or silence for that version.
**Update** downloads the build for this platform, checks it against the size and
SHA-256 digest the release published, unpacks it beside the installed copy and
moves it into that copy's place before restarting into it — nothing elevated,
nothing written outside the installation directory, and the release page in a
browser as the fallback wherever that cannot work. See
[Updating](docs/user-guide.md#updating).

## Installing

Prebuilt binaries for Windows, macOS and Linux are attached to every
[GitHub release](https://github.com/xcomart/logman/releases).

### macOS refuses to open a downloaded copy

The macOS bundle is ad-hoc signed but not notarized — there is no Apple
Developer account behind it — so Gatekeeper quarantines what the browser
downloaded and blocks the first launch with "logman.app cannot be opened"
or claims the app is damaged. The app is fine; the quarantine flag is the
whole problem. After moving `logman.app` into `/Applications`, clear it:

```bash
xattr -r -d com.apple.quarantine /Applications/logman.app
```

The next launch — and every one after it — opens normally. If running
unsigned commands from the terminal is not your thing, the long way around
works too: try to open the app once, then allow it under **System Settings →
Privacy & Security → Open Anyway**. A copy built from source (below) never
gets the quarantine flag in the first place.

## Building

Requires a Rust toolchain (edition 2024, so 1.85 or newer) and a platform
compiler toolchain — MSVC on Windows, Xcode command line tools on macOS, a C
compiler and the usual X11/Wayland development packages on Linux.

```bash
cargo run --release -p logman-app
```

The SSH layer deliberately uses russh's `ring` backend instead of the default
`aws-lc-rs`. `aws-lc-rs` needs NASM to build on Windows, which would make a
clean checkout fail to compile there; `ring` builds everywhere with no extra
tooling. Do not re-enable russh's default features.

### gpui is vendored and patched

`vendor/gpui` is gpui 0.2.2 with a small set of local patches, wired in through
`[patch.crates-io]`:

- **Windows IME fix.** Upstream's message pump calls `DispatchMessageW` without
  `TranslateMessage` and compensates by calling `TranslateMessage` re-entrantly
  from inside the window procedure. TSF correlates translated keys against the
  message queue, so ending a Korean composition with the Han/Yeong key leaves
  CTF regenerating `WM_IME_COMPOSITION` forever and the process pinned at 100%
  CPU.
- **`Window::set_titlebar_transparent`.** Upstream only decides at window
  creation whether the platform caption exists; this API flips it on a live
  window, which is what lets the title bar setting apply without a restart.
- Smaller fixes: window background blur on macOS Tahoe, and explicit `f32`
  suffixes in `taffy.rs` float literals.

The vendored source is otherwise identical to the published crate, so
`diff -r` against the registry copy shows exactly the patched files. Retiring
the vendor needs a released gpui that carries the IME fix and a public way to
retheme the caption at runtime; until then the patches ride along here.

### Release builds on Windows need `fxc.exe`

gpui precompiles its HLSL shaders only in release builds — debug builds compile
them at runtime — so `cargo build --release` needs `fxc.exe` from the Windows
SDK. gpui looks on `PATH` and then at one hardcoded location under
`C:\Program Files (x86)`, so an SDK installed anywhere else fails the build with
`Failed to find fxc.exe`. Point it at the right file:

```powershell
$env:GPUI_FXC_PATH = "D:\Windows Kits\10\bin\10.0.26100.0\x64\fxc.exe"
cargo build --release
```

Debug builds do not need it.

### Testing

```bash
cargo test --workspace
```

`logman-ssh` is tested against a real SSH server: the integration suite starts an
in-process russh server on an ephemeral port with a freshly generated host key
and drives the actual client against it — password and public key
authentication (including an encrypted key), pty parameters, data round-trip,
`window-change`, host key rejection, and teardown. No fixture keys are committed
and no external server is needed.

## How it is put together

| Crate | Responsibility |
| --- | --- |
| `logman-core` | Profiles, OS keychain, `known_hosts`, config paths. No SSH, no GUI. |
| `logman-ssh` | russh client: authentication, pty, shell, resize, and the SFTP channel behind the files panel. Owns its own thread and Tokio runtime. |
| `logman-pty` | The local shell transport: a unix pty on one side, a Windows ConPTY on the other, behind one API. |
| `logman-term` | `alacritty_terminal` wrapper: byte stream in, styled snapshot out; key encoding, and the transcoding at both edges for a session that is not UTF-8. No GUI. |
| `logman-app` | The gpui binary: widgets, terminal rendering, session management. |

Two boundaries are worth knowing about.

**SSH never blocks the UI.** Each session owns a dedicated thread running its own
Tokio runtime, and talks to the GUI only over channels. A hung network read
cannot stall a repaint.

**The terminal model knows nothing about gpui, and the GUI knows nothing about
russh.** `logman-term` turns bytes into a `TerminalSnapshot` of styled runs, and
that is all the renderer sees. Both lower crates are testable without a window:
most of the workspace's tests need neither a GUI nor a network, and the ones
that reach the network need only loopback.

### Third-party libraries

The heavy lifting is done by these projects:

| Library | Role |
| --- | --- |
| [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui) | GPU-accelerated UI framework, from the Zed editor (vendored 0.2.2, [patched](#gpui-is-vendored-and-patched)) |
| [russh](https://github.com/warp-tech/russh) | Pure-Rust SSH client: transport, authentication, pty and shell channels |
| [russh-sftp](https://github.com/AspectUnk/russh-sftp) | SFTP client for the remote files panel, on a channel of the same connection |
| [alacritty_terminal](https://github.com/alacritty/alacritty) | Terminal emulation: grid, VTE parsing, scrollback — and the unix pty behind a local session |
| [portable-pty](https://github.com/wez/wezterm/tree/main/pty) | The Windows ConPTY behind a local session |
| [tokio](https://github.com/tokio-rs/tokio) | Async runtime for the SSH transport thread |
| [keyring](https://github.com/open-source-cooperative/keyring-rs) | OS credential store: Windows Credential Manager, macOS Keychain, Secret Service |
| [directories](https://github.com/soc/directories-rs) | Per-platform configuration paths, and the home directory the save dialog opens in |
| [ureq](https://github.com/algesten/ureq) | The one HTTPS client: the update check, and the download the self-update runs on |

Supporting crates:
[serde](https://github.com/serde-rs/serde) /
[serde_json](https://github.com/serde-rs/json) (profiles and settings),
[serde_norway](https://github.com/cafkafk/serde-yaml) (the editor's
user-supplied syntax definitions),
[ropey](https://github.com/cessen/ropey) (the editor's document),
[tempfile](https://github.com/Stebalien/tempfile) (staging a file being opened
or saved),
[uuid](https://github.com/uuid-rs/uuid) (profile identity),
[anyhow](https://github.com/dtolnay/anyhow) /
[thiserror](https://github.com/dtolnay/thiserror) (errors),
[log](https://github.com/rust-lang/log) /
[env_logger](https://github.com/rust-cli/env_logger) (logging),
[futures](https://github.com/rust-lang/futures-rs) /
[async-trait](https://github.com/dtolnay/async-trait) (async glue),
[parking_lot](https://github.com/Amanieu/parking_lot),
[smallvec](https://github.com/servo/rust-smallvec),
[bitflags](https://github.com/bitflags/bitflags),
[unicode-segmentation](https://github.com/unicode-rs/unicode-segmentation)
(grapheme-safe text editing).

Windows only: [windows-rs](https://github.com/microsoft/windows-rs) (DWM
caption colors), [raw-window-handle](https://github.com/rust-windowing/raw-window-handle)
(HWND access), [winresource](https://github.com/BenjaminRi/winresource)
(icon embedding). Tests additionally use
[rand](https://github.com/rust-random/rand).

## Limitations

The honest headlines, one line each:

- **No SSH agent support and no keyboard-interactive authentication**, so
  MFA-protected servers cannot be reached yet.
- **IME composition is verified only against the Microsoft Korean IME on
  Windows**, where it also depends on the vendored gpui patch described above.
- **The files panel cannot change permissions or ownership**, and a transfer or
  a delete cannot be cancelled once it has started.
- **The editor opens text and nothing else**, up to 10 MB, saves without
  atomicity, and notices nothing that changes the file underneath it.
- **Panes can be resized but not rearranged**, and neither a split layout nor
  the files panel's width survives a restart.
- **Runtime palette changes are ignored**: a program that redefines colors with
  `OSC 4` or `OSC 10`–`11` renders with the static scheme.

The full list, with the reasoning behind each one, is in the guide:
[Known limitations](docs/user-guide.md#known-limitations).

## License

MIT — see [LICENSE](LICENSE). The vendored gpui keeps its own
Apache-2.0 notice under `vendor/gpui/`.
