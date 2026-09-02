//! Global application settings and how a session resolves them.
//!
//! Everything here is persisted to `settings.json` next to the profile
//! database. The file is meant to be hand-editable, so loading is deliberately
//! forgiving: unknown keys are ignored (a file written by a newer rulogman still
//! opens), missing keys fall back to the documented defaults, and out-of-range
//! numbers are clamped rather than rejected. See [`AppSettings::sanitize`].

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::highlight::HighlightRule;
use crate::paths::{settings_file, strip_bom, write_atomic};
use crate::profile::SessionOverrides;

/// Lowest window opacity the UI accepts; below this the chrome is unreadable.
const MIN_BACKGROUND_OPACITY: f32 = 0.5;
/// Fully opaque window, and the default.
const MAX_BACKGROUND_OPACITY: f32 = 1.0;
/// Smallest legible terminal font size.
const MIN_FONT_SIZE: f32 = 6.0;
/// Largest terminal font size worth offering.
const MAX_FONT_SIZE: f32 = 32.0;
/// Terminal font size used when none is configured.
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// Upper bound on scrollback, to keep a runaway session from eating memory.
const MAX_SCROLLBACK_LINES: usize = 100_000;
/// Scrollback depth used when none is configured.
const DEFAULT_SCROLLBACK_LINES: usize = 5_000;
/// Color scheme used when none is configured.
const DEFAULT_SCHEME: &str = "one-dark";
/// UI chrome theme used when none is configured.
const DEFAULT_UI_THEME: &str = "one-dark";
/// Light counterpart of [`DEFAULT_UI_THEME`], named here only so the legacy
/// value `"light"` can be mapped onto it.
const LIGHT_UI_THEME: &str = "one-light";
/// Value builds before themes had ids wrote for the dark chrome.
const LEGACY_UI_THEME_DARK: &str = "dark";
/// Value builds before themes had ids wrote for the light chrome.
const LEGACY_UI_THEME_LIGHT: &str = "light";
/// `TERM` value advertised when none is configured.
const DEFAULT_TERM: &str = "xterm-256color";
/// Character set assumed for a session that does not name one.
///
/// Public because the app layer offers it as the "no override" entry in the
/// connection form, and both ends have to agree on the spelling — this is the
/// canonical WHATWG name, so it round-trips through a label lookup.
pub const DEFAULT_CHARSET: &str = "UTF-8";
/// SSH port offered by the connection form.
const DEFAULT_PORT: u16 = 22;
/// Seconds between SSH keepalive probes.
const DEFAULT_KEEPALIVE_SECS: u64 = 30;
/// Seconds to wait for a TCP connection before giving up.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Clamp `value` into `min ..= max`, replacing NaN with `fallback`.
fn clamp_f32(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_nan() {
        fallback
    } else {
        value.clamp(min, max)
    }
}

/// Clamp a terminal font size into the supported range.
fn clamp_font_size(value: f32) -> f32 {
    clamp_f32(value, MIN_FONT_SIZE, MAX_FONT_SIZE, DEFAULT_FONT_SIZE)
}

/// Clamp a scrollback depth into the supported range.
fn clamp_scrollback_lines(value: usize) -> usize {
    value.min(MAX_SCROLLBACK_LINES)
}

/// Fall back to `default` when a hand-edited string is blank.
fn non_blank(value: &str, default: &str) -> String {
    if value.trim().is_empty() {
        default.to_string()
    } else {
        value.to_string()
    }
}

/// Map a stored UI theme id onto one this build can be asked to resolve.
///
/// Builds before the UI themes had ids stored the two enum variants `"dark"`
/// and `"light"`; those name the same palettes the `one-dark` and `one-light`
/// themes carry today, so they are rewritten rather than dropped. A blank value
/// falls back to the default. Everything else is kept verbatim — exactly like
/// [`TerminalSettings::scheme`] — so an id belonging to a theme file the app
/// layer loads survives a round trip through a build that cannot see it.
fn sanitize_ui_theme(value: &str) -> String {
    let value = value.trim();
    if value.eq_ignore_ascii_case(LEGACY_UI_THEME_DARK) {
        DEFAULT_UI_THEME.to_string()
    } else if value.eq_ignore_ascii_case(LEGACY_UI_THEME_LIGHT) {
        LIGHT_UI_THEME.to_string()
    } else {
        non_blank(value, DEFAULT_UI_THEME)
    }
}

/// Who draws the window's title bar.
///
/// Read once, when the window is created: the platforms decide at that point
/// whether the window has a caption at all, so a change only shows after a
/// restart. The UI is expected to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TitlebarStyle {
    /// rulogman draws it: the toolbar doubles as the title bar. The default.
    #[default]
    Custom,
    /// The operating system draws its own caption above the app's chrome.
    System,
}

/// Window background treatment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    /// 0.5 ..= 1.0; values below the floor are clamped on load.
    pub background_opacity: f32,
    /// Acrylic/blur behind the window when the platform supports it.
    pub background_blur: bool,
    /// Who draws the title bar. Only read when a window is created.
    pub titlebar: TitlebarStyle,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            background_opacity: MAX_BACKGROUND_OPACITY,
            background_blur: false,
            titlebar: TitlebarStyle::default(),
        }
    }
}

impl WindowSettings {
    /// Force every field back into its supported range.
    fn sanitize(&mut self) {
        self.background_opacity = clamp_f32(
            self.background_opacity,
            MIN_BACKGROUND_OPACITY,
            MAX_BACKGROUND_OPACITY,
            MAX_BACKGROUND_OPACITY,
        );
    }
}

/// Terminal appearance and behaviour defaults for every session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Color scheme id, e.g. `"one-dark"`. Resolution lives in `rulogman-term`.
    pub scheme: String,
    /// `None` = the per-OS monospace default chosen by the app layer.
    pub font_family: Option<String>,
    /// Points/pixels; clamped to 6.0 ..= 32.0 on load.
    pub font_size: f32,
    /// Lines of scrollback kept above the screen; clamped to 0 ..= 100_000.
    pub scrollback_lines: usize,
    /// `TERM` advertised to the remote host.
    pub term: String,
    /// Copy the selection to the clipboard as soon as the mouse releases.
    pub copy_on_select: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            scheme: DEFAULT_SCHEME.to_string(),
            font_family: None,
            font_size: DEFAULT_FONT_SIZE,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            term: DEFAULT_TERM.to_string(),
            copy_on_select: false,
        }
    }
}

impl TerminalSettings {
    /// Force every field back into its supported range.
    fn sanitize(&mut self) {
        self.scheme = non_blank(&self.scheme, DEFAULT_SCHEME);
        self.font_size = clamp_font_size(self.font_size);
        self.scrollback_lines = clamp_scrollback_lines(self.scrollback_lines);
        self.term = non_blank(&self.term, DEFAULT_TERM);
        if let Some(family) = &self.font_family
            && family.trim().is_empty()
        {
            self.font_family = None;
        }
    }
}

/// Defaults applied to the connection form and new sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionSettings {
    /// Port pre-filled in the connection form.
    pub default_port: u16,
    /// Login name pre-filled in the connection form, if any.
    pub default_username: Option<String>,
    /// Seconds between SSH keepalive probes; 0 disables them.
    pub keepalive_secs: u64,
    /// Seconds to wait for the TCP connection before giving up.
    pub connect_timeout_secs: u64,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            default_port: DEFAULT_PORT,
            default_username: None,
            keepalive_secs: DEFAULT_KEEPALIVE_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
        }
    }
}

impl ConnectionSettings {
    /// Force every field back into its supported range.
    fn sanitize(&mut self) {
        if self.default_port == 0 {
            self.default_port = DEFAULT_PORT;
        }
        if self.connect_timeout_secs == 0 {
            self.connect_timeout_secs = DEFAULT_CONNECT_TIMEOUT_SECS;
        }
        if let Some(username) = &self.default_username
            && username.trim().is_empty()
        {
            self.default_username = None;
        }
    }
}

/// What the file panel does for the sessions no profile speaks for.
///
/// A remote session carries the answer on its
/// [`SessionProfile`](crate::SessionProfile), because whether a *host* is worth
/// browsing is a fact about that host. A shell on this machine comes from no
/// profile at all — there is nothing to save the answer on — so the one machine
/// every local shell shares gets one setting to speak for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesSettings {
    /// Whether a shell on this machine opens with the file panel beside it.
    pub local_panel: bool,
}

impl Default for FilesSettings {
    fn default() -> Self {
        Self { local_panel: true }
    }
}

impl FilesSettings {
    /// Force every field back into its supported range.
    ///
    /// Nothing to force yet: a flag is either set or it is not, and serde has
    /// already turned anything a hand edit could put there into one or the
    /// other. It exists so that this section is sanitised the way every other
    /// one is, and so the next field added here has somewhere to be clamped.
    fn sanitize(&mut self) {}
}

/// How the editor a file opens in behaves, for every file and every session.
///
/// Nothing here is per-connection the way the terminal's settings are: a file
/// opened over SSH and a file opened off this machine are the same document in
/// the same widget, and how long lines are dealt with is a fact about the way
/// the user reads, not about the host the bytes came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    /// Whether a line too long for the pane is broken at its width.
    ///
    /// Off, which is what the editor itself defaults to and what the file the
    /// pane most often has open — a configuration file, a script — is written
    /// for: those lines are short, and the indentation a wrapped line hides is
    /// what makes them readable. It is the log tailed in the pane beside them,
    /// with one record per line and no structure to lose, that wants the other
    /// answer — so it is offered rather than assumed.
    ///
    /// The one section here whose default is `Default`'s own, hence the derive
    /// above where every neighbour writes the impl out: `false` is what the
    /// setting means and what the field would be either way.
    pub word_wrap: bool,
}

impl EditorSettings {
    /// Force every field back into its supported range.
    ///
    /// Nothing to force, for the same reason [`FilesSettings::sanitize`] has
    /// nothing: a flag is either set or it is not, and serde has already made
    /// anything a hand edit could put there into one or the other.
    fn sanitize(&mut self) {}
}

/// The highlight rules every followed file starts from.
///
/// One section rather than a bare list, so the next thing highlighting learns
/// — a default scope, a cap on how many rules a pane compiles — has somewhere
/// to live without another top-level key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightSettings {
    /// The global rule list, or `None` to use the built-in preset.
    ///
    /// Three-valued on purpose, and resolved together with the per-file list by
    /// [`effective_highlights`](crate::effective_highlights):
    ///
    /// * `None` — nothing configured; every file that does not say otherwise
    ///   uses [`highlight_preset`](crate::highlight_preset). This is what a
    ///   fresh install and every `settings.json` written before highlighting
    ///   existed both mean.
    /// * `Some(empty)` — highlighting is globally off.
    /// * `Some(rules)` — exactly these, for every file that does not override
    ///   them.
    ///
    /// This section is written whole like every other one — there is no
    /// `skip_serializing_if` anywhere in this file — so "use the preset" looks
    /// like `"highlights": {"rules": null}` on disk rather than like a missing
    /// key. That is deliberate: the file is meant to be hand-editable, and a
    /// key that is *there* and null is a discoverable invitation to fill it in,
    /// where an absent one is indistinguishable from a feature that does not
    /// exist. The dialog writes `null` back whenever the user left the preset
    /// untouched — see [`is_highlight_preset`](crate::is_highlight_preset) —
    /// so improvements to the built-in list keep arriving.
    pub rules: Option<Vec<HighlightRule>>,
}

impl HighlightSettings {
    /// Force every field back into its supported range.
    ///
    /// Two repairs, both aimed at what a hand edit or a half-finished dialog
    /// row leaves behind. A rule whose pattern is blank is dropped: it can only
    /// ever be an empty row, and keeping it would mean the app compiling an
    /// empty regex that matches every position of every line. And each colour
    /// is trimmed, with a blank one becoming `None`, so that `"foreground": " "`
    /// reads as "no colour" rather than as a spelling nothing can parse.
    ///
    /// Deliberately *not* repaired: the pattern's own surrounding whitespace,
    /// which can be significant in a regex, and a colour this build does not
    /// recognise, which is kept verbatim for the same reason an unknown
    /// `ui_theme` is — a newer build may know it.
    fn sanitize(&mut self) {
        let Some(rules) = &mut self.rules else {
            return;
        };
        rules.retain(|rule| !rule.pattern.trim().is_empty());
        for rule in rules {
            for colour in [&mut rule.foreground, &mut rule.background] {
                if let Some(text) = colour {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        *colour = None;
                    } else if trimmed.len() != text.len() {
                        *colour = Some(trimmed.to_string());
                    }
                }
            }
        }
    }
}

/// Everything rulogman persists in `settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Schema version of the file; see [`AppSettings::CURRENT_VERSION`].
    pub version: u32,
    /// BCP 47 tag of the interface language, e.g. `"ko"` or `"zh-CN"`.
    ///
    /// `None` — the default — means "follow the operating system". The list of
    /// tags rulogman actually ships translations for lives in the app layer, so
    /// nothing here validates the string: an unknown tag is resolved the same
    /// way `None` is, by falling back to the system locale and then to English.
    pub language: Option<String>,
    /// UI chrome theme id, e.g. `"one-dark"`. Resolution lives in the app
    /// layer, which also knows the themes loaded from the `themes` directory.
    pub ui_theme: String,
    /// Window background treatment.
    pub window: WindowSettings,
    /// Terminal defaults shared by every session.
    pub terminal: TerminalSettings,
    /// Defaults for new connections.
    pub connection: ConnectionSettings,
    /// What the file panel does where no profile decides it.
    pub files: FilesSettings,
    /// How the editor a file opens in behaves.
    pub editor: EditorSettings,
    /// The highlight rules a followed file uses unless it overrides them.
    pub highlights: HighlightSettings,
    /// Release tag the user asked never to be told about again, e.g. `"v0.4.0"`.
    ///
    /// Written by the start-up update check when the user picks "ignore this
    /// version", and compared against the latest tag verbatim: only that exact
    /// release is suppressed, so the next one announces itself normally. `None`
    /// — the default — means nothing has been ignored.
    ///
    /// Stored as the tag rather than as a parsed version because the tag is what
    /// GitHub answers with and what the comparison already has in hand; nothing
    /// here validates it, since an unrecognisable value can only ever fail to
    /// match a real tag, which is the harmless direction.
    pub ignored_update: Option<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            language: None,
            ui_theme: DEFAULT_UI_THEME.to_string(),
            window: WindowSettings::default(),
            terminal: TerminalSettings::default(),
            connection: ConnectionSettings::default(),
            files: FilesSettings::default(),
            editor: EditorSettings::default(),
            highlights: HighlightSettings::default(),
            ignored_update: None,
        }
    }
}

impl AppSettings {
    /// Schema version written by this build.
    ///
    /// A file carrying a different number still loads: unknown keys are ignored
    /// and missing ones default, so the version is informational until a real
    /// migration is needed.
    pub const CURRENT_VERSION: u32 = 1;

    /// Load the settings from the default configuration file.
    ///
    /// A missing file yields [`AppSettings::default`], and the result is always
    /// passed through [`AppSettings::sanitize`].
    ///
    /// # Errors
    ///
    /// Fails when the configuration directory cannot be determined, the file
    /// cannot be read, or its contents are not valid JSON.
    pub fn load() -> Result<Self> {
        Self::load_from(&settings_file()?)
    }

    /// Load the settings from an explicit path.
    ///
    /// A missing file yields [`AppSettings::default`]. A leading UTF-8 byte
    /// order mark is tolerated, unknown keys are ignored, and every value is
    /// clamped by [`AppSettings::sanitize`] before being returned.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or does not contain valid JSON.
    pub fn load_from(path: &Path) -> Result<Self> {
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let mut settings: Self = serde_json::from_slice(strip_bom(&data))
            .with_context(|| format!("failed to parse settings from {}", path.display()))?;
        settings.sanitize();
        Ok(settings)
    }

    /// Write the settings to the default configuration file.
    ///
    /// # Errors
    ///
    /// Fails when the configuration directory cannot be determined or created,
    /// or when the file cannot be written.
    pub fn save(&self) -> Result<()> {
        self.save_to(&settings_file()?)
    }

    /// Write the settings to an explicit path, creating parent directories.
    ///
    /// The write is atomic: the data lands in a temporary sibling file that is
    /// then renamed over `path`.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created or the file cannot be
    /// written.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("failed to serialize settings")?;
        write_atomic(path, &json)
    }

    /// Force every value into its supported range.
    ///
    /// Called on every load so a hand-edited `settings.json` cannot break the
    /// app: opacities are clamped to 0.5 ..= 1.0 (NaN becomes 1.0), font sizes
    /// to 6.0 ..= 32.0 (NaN becomes 14.0), scrollback to at most 100 000 lines,
    /// and blank strings fall back to their defaults. The UI should call it
    /// again after editing values.
    ///
    /// This is also where the one migration the file has needed so far happens:
    /// a `ui_theme` written as `"dark"` or `"light"` by an older build becomes
    /// the equivalent theme id.
    pub fn sanitize(&mut self) {
        if let Some(language) = &self.language
            && language.trim().is_empty()
        {
            self.language = None;
        }
        if let Some(tag) = &self.ignored_update
            && tag.trim().is_empty()
        {
            self.ignored_update = None;
        }
        self.ui_theme = sanitize_ui_theme(&self.ui_theme);
        self.window.sanitize();
        self.terminal.sanitize();
        self.connection.sanitize();
        self.files.sanitize();
        self.editor.sanitize();
        self.highlights.sanitize();
    }

    /// Global terminal defaults with a profile's overrides applied on top.
    ///
    /// Overridden numbers go through the same clamps as the global ones, and a
    /// blank overridden string is treated as "not overridden", so a hand-edited
    /// profile cannot produce a session the terminal cannot render.
    pub fn effective_terminal(&self, overrides: &SessionOverrides) -> EffectiveTerminal {
        let base = &self.terminal;
        let scheme = match overrides.scheme.as_deref() {
            Some(scheme) if !scheme.trim().is_empty() => scheme.to_string(),
            _ => base.scheme.clone(),
        };
        let term = match overrides.term.as_deref() {
            Some(term) if !term.trim().is_empty() => term.to_string(),
            _ => base.term.clone(),
        };
        // The one resolution with no global to fall back to, deliberately: a
        // charset describes a *host*, not a preference, and every host worth
        // defaulting for has spoken UTF-8 for twenty years. A global setting
        // here would only ever be a way to break every modern session at once
        // in order to fix one legacy one.
        let charset = match overrides.charset.as_deref() {
            Some(charset) if !charset.trim().is_empty() => charset.to_string(),
            _ => DEFAULT_CHARSET.to_string(),
        };
        EffectiveTerminal {
            scheme,
            font_family: base.font_family.clone(),
            font_size: clamp_font_size(overrides.font_size.unwrap_or(base.font_size)),
            scrollback_lines: clamp_scrollback_lines(
                overrides.scrollback_lines.unwrap_or(base.scrollback_lines),
            ),
            term,
            charset,
            copy_on_select: base.copy_on_select,
        }
    }
}

/// The settings a single session actually runs with.
///
/// Produced by [`AppSettings::effective_terminal`]; every field is resolved, so
/// consumers never have to look at the global settings again.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveTerminal {
    /// Color scheme id to resolve in `rulogman-term`.
    pub scheme: String,
    /// `None` = the per-OS monospace default chosen by the app layer.
    pub font_family: Option<String>,
    /// Font size, already clamped to the supported range.
    pub font_size: f32,
    /// Scrollback depth, already clamped to the supported range.
    pub scrollback_lines: usize,
    /// `TERM` to advertise to the remote host.
    pub term: String,
    /// WHATWG encoding label the session's byte stream is in; resolved to
    /// something that can transcode by `rulogman-term`'s `Charset`.
    ///
    /// Always [`DEFAULT_CHARSET`] unless the profile overrode it: there is no
    /// global setting behind this one.
    pub charset: String,
    /// Copy the selection to the clipboard as soon as the mouse releases.
    pub copy_on_select: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let settings = AppSettings::default();
        assert_eq!(settings.version, 1);
        assert_eq!(settings.language, None);
        assert_eq!(settings.ui_theme, "one-dark");
        assert_eq!(settings.window.background_opacity, 1.0);
        assert!(!settings.window.background_blur);
        assert_eq!(settings.window.titlebar, TitlebarStyle::Custom);
        assert_eq!(settings.terminal.scheme, "one-dark");
        assert_eq!(settings.terminal.font_family, None);
        assert_eq!(settings.terminal.font_size, 14.0);
        assert_eq!(settings.terminal.scrollback_lines, 5_000);
        assert_eq!(settings.terminal.term, "xterm-256color");
        assert!(!settings.terminal.copy_on_select);
        // Not a `TerminalSettings` field: the charset is per-connection only,
        // so the documented default is the constant itself.
        assert_eq!(DEFAULT_CHARSET, "UTF-8");
        assert_eq!(settings.connection.default_port, 22);
        assert_eq!(settings.connection.default_username, None);
        assert_eq!(settings.connection.keepalive_secs, 30);
        assert_eq!(settings.connection.connect_timeout_secs, 15);
        assert!(settings.files.local_panel);
        assert!(!settings.editor.word_wrap);
        // `None`, not an inlined copy of the preset: see the field's docs.
        assert_eq!(settings.highlights.rules, None);
    }

    #[test]
    fn a_settings_file_without_an_editor_section_leaves_long_lines_unwrapped() {
        // Every settings.json on disk today predates the section, and the
        // editor those builds drew never wrapped. A missing section has to go
        // on meaning exactly that.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"terminal": {"copy_on_select": true}}"#).expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert!(settings.terminal.copy_on_select);
        assert!(!settings.editor.word_wrap);
    }

    #[test]
    fn a_word_wrap_turned_on_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let mut settings = AppSettings::default();
        settings.editor.word_wrap = true;
        settings.save_to(&path).expect("save");

        assert!(
            AppSettings::load_from(&path)
                .expect("load")
                .editor
                .word_wrap
        );
    }

    #[test]
    fn a_settings_file_without_a_files_section_still_opens_the_local_panel() {
        // Every settings.json on disk today predates the section, and the file
        // panel those builds drew was a window-wide switch that started out
        // open. A missing section therefore has to keep meaning "open".
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"terminal": {"copy_on_select": true}}"#).expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert!(settings.terminal.copy_on_select);
        assert!(settings.files.local_panel);
    }

    #[test]
    fn a_local_panel_turned_off_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let mut settings = AppSettings::default();
        settings.files.local_panel = false;
        settings.save_to(&path).expect("save");

        assert!(
            !AppSettings::load_from(&path)
                .expect("load")
                .files
                .local_panel
        );
    }

    #[test]
    fn ui_theme_is_stored_as_a_plain_id() {
        let settings = AppSettings {
            ui_theme: "gruvbox-dark".to_string(),
            ..AppSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["ui_theme"], serde_json::json!("gruvbox-dark"));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        settings.save_to(&path).expect("save");
        assert_eq!(
            AppSettings::load_from(&path).expect("load").ui_theme,
            "gruvbox-dark"
        );
    }

    #[test]
    fn a_legacy_ui_theme_becomes_its_named_equivalent() {
        // Files written before the themes had ids carry the two enum variants.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        fs::write(&path, br#"{"ui_theme":"light"}"#).expect("write");
        assert_eq!(
            AppSettings::load_from(&path).expect("load").ui_theme,
            "one-light"
        );

        fs::write(&path, br#"{"ui_theme":"Dark"}"#).expect("write");
        assert_eq!(
            AppSettings::load_from(&path).expect("load").ui_theme,
            "one-dark"
        );
    }

    #[test]
    fn an_unknown_ui_theme_id_survives_sanitize() {
        // The app layer owns the theme registry — the `themes` directory
        // included — so core must not drop an id it happens not to know.
        let mut settings = AppSettings {
            ui_theme: "my-theme".to_string(),
            ..AppSettings::default()
        };
        settings.sanitize();
        assert_eq!(settings.ui_theme, "my-theme");

        settings.ui_theme = "   ".to_string();
        settings.sanitize();
        assert_eq!(settings.ui_theme, "one-dark");
    }

    #[test]
    fn titlebar_style_serializes_in_snake_case() {
        assert_eq!(
            serde_json::to_value(TitlebarStyle::System).unwrap(),
            serde_json::json!("system")
        );
        assert_eq!(
            serde_json::from_str::<TitlebarStyle>("\"custom\"").unwrap(),
            TitlebarStyle::Custom
        );
    }

    #[test]
    fn a_window_section_without_a_titlebar_key_keeps_the_default() {
        // Settings files written before the key existed have to keep opening,
        // and the custom title bar is what they get.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"window": {"background_blur": true}}"#).expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert!(settings.window.background_blur);
        assert_eq!(settings.window.titlebar, TitlebarStyle::Custom);
    }

    #[test]
    fn save_to_load_from_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cfg").join("settings.json");

        let settings = AppSettings {
            language: Some("zh-CN".to_string()),
            ui_theme: "one-light".to_string(),
            window: WindowSettings {
                background_opacity: 0.8,
                background_blur: true,
                titlebar: TitlebarStyle::System,
            },
            terminal: TerminalSettings {
                scheme: "solarized".to_string(),
                font_family: Some("Cascadia Mono".to_string()),
                font_size: 16.5,
                scrollback_lines: 20_000,
                term: "xterm".to_string(),
                copy_on_select: true,
            },
            connection: ConnectionSettings {
                default_port: 2222,
                default_username: Some("alice".to_string()),
                keepalive_secs: 60,
                connect_timeout_secs: 5,
            },
            ..AppSettings::default()
        };

        settings.save_to(&path).expect("save");
        assert_eq!(AppSettings::load_from(&path).expect("load"), settings);

        // Saving over an existing file must work too.
        settings.save_to(&path).expect("overwrite");
        assert_eq!(AppSettings::load_from(&path).expect("reload"), settings);
    }

    #[test]
    fn load_from_missing_file_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings =
            AppSettings::load_from(&dir.path().join("absent.json")).expect("load missing");
        assert_eq!(settings, AppSettings::default());
    }

    #[test]
    fn load_from_tolerates_a_utf8_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(br#"{"ui_theme":"light"}"#);
        fs::write(&path, with_bom).expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert_eq!(settings.ui_theme, "one-light");
        // Everything else falls back to the defaults.
        assert_eq!(settings.terminal, TerminalSettings::default());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{
                "version": 99,
                "ui_theme": "light",
                "future_top_level": {"anything": [1, 2, 3]},
                "terminal": {"font_size": 18.0, "future_terminal_key": "hi"}
            }"#,
        )
        .expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert_eq!(settings.version, 99);
        assert_eq!(settings.ui_theme, "one-light");
        assert_eq!(settings.terminal.font_size, 18.0);
        // Unspecified keys of a partially specified section still default.
        assert_eq!(settings.terminal.scheme, "one-dark");
    }

    #[test]
    fn empty_object_loads_as_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{}").expect("write");
        assert_eq!(
            AppSettings::load_from(&path).expect("load"),
            AppSettings::default()
        );
    }

    #[test]
    fn load_from_invalid_json_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{ nope").expect("write");
        assert!(AppSettings::load_from(&path).is_err());
    }

    #[test]
    fn sanitize_clamps_font_size() {
        let mut settings = AppSettings::default();

        settings.terminal.font_size = 500.0;
        settings.sanitize();
        assert_eq!(settings.terminal.font_size, 32.0);

        settings.terminal.font_size = 0.0;
        settings.sanitize();
        assert_eq!(settings.terminal.font_size, 6.0);

        settings.terminal.font_size = -20.0;
        settings.sanitize();
        assert_eq!(settings.terminal.font_size, 6.0);

        settings.terminal.font_size = f32::NAN;
        settings.sanitize();
        assert_eq!(settings.terminal.font_size, 14.0);
    }

    #[test]
    fn sanitize_clamps_background_opacity() {
        let mut settings = AppSettings::default();

        settings.window.background_opacity = 1.5;
        settings.sanitize();
        assert_eq!(settings.window.background_opacity, 1.0);

        settings.window.background_opacity = -1.0;
        settings.sanitize();
        assert_eq!(settings.window.background_opacity, 0.5);

        settings.window.background_opacity = 0.0;
        settings.sanitize();
        assert_eq!(settings.window.background_opacity, 0.5);

        settings.window.background_opacity = f32::NAN;
        settings.sanitize();
        assert_eq!(settings.window.background_opacity, 1.0);

        settings.window.background_opacity = f32::INFINITY;
        settings.sanitize();
        assert_eq!(settings.window.background_opacity, 1.0);

        settings.window.background_opacity = 0.75;
        settings.sanitize();
        assert_eq!(settings.window.background_opacity, 0.75);
    }

    #[test]
    fn sanitize_clamps_scrollback() {
        let mut settings = AppSettings::default();

        settings.terminal.scrollback_lines = 1_000_000_000;
        settings.sanitize();
        assert_eq!(settings.terminal.scrollback_lines, 100_000);

        settings.terminal.scrollback_lines = 0;
        settings.sanitize();
        assert_eq!(settings.terminal.scrollback_lines, 0);
    }

    #[test]
    fn sanitize_restores_blank_strings() {
        let mut settings = AppSettings {
            language: Some("  ".to_string()),
            ..AppSettings::default()
        };
        settings.terminal.scheme = "   ".to_string();
        settings.terminal.term = String::new();
        settings.terminal.font_family = Some("  ".to_string());
        settings.connection.default_username = Some(String::new());
        settings.connection.default_port = 0;
        settings.connection.connect_timeout_secs = 0;

        settings.sanitize();

        assert_eq!(settings.language, None);
        assert_eq!(settings.terminal.scheme, "one-dark");
        assert_eq!(settings.terminal.term, "xterm-256color");
        assert_eq!(settings.terminal.font_family, None);
        assert_eq!(settings.connection.default_username, None);
        assert_eq!(settings.connection.default_port, 22);
        assert_eq!(settings.connection.connect_timeout_secs, 15);
    }

    #[test]
    fn sanitize_keeps_an_unknown_language_tag() {
        // The app layer owns the list of shipped translations and degrades an
        // unknown tag to the system locale, so core must not silently drop one:
        // a typo has to survive a round trip for the UI to be able to show it.
        let mut settings = AppSettings {
            language: Some("xx-YZ".to_string()),
            ..AppSettings::default()
        };
        settings.sanitize();
        assert_eq!(settings.language.as_deref(), Some("xx-YZ"));
    }

    #[test]
    fn load_applies_sanitize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{
                "window": {"background_opacity": 0.1},
                "terminal": {"font_size": 500.0, "scrollback_lines": 1000000000}
            }"#,
        )
        .expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert_eq!(settings.window.background_opacity, 0.5);
        assert_eq!(settings.terminal.font_size, 32.0);
        assert_eq!(settings.terminal.scrollback_lines, 100_000);
    }

    #[test]
    fn effective_terminal_without_overrides_is_the_global_default() {
        let settings = AppSettings::default();
        let effective = settings.effective_terminal(&SessionOverrides::default());

        assert_eq!(effective.scheme, settings.terminal.scheme);
        assert_eq!(effective.font_family, settings.terminal.font_family);
        assert_eq!(effective.font_size, settings.terminal.font_size);
        assert_eq!(
            effective.scrollback_lines,
            settings.terminal.scrollback_lines
        );
        assert_eq!(effective.term, settings.terminal.term);
        assert_eq!(effective.charset, DEFAULT_CHARSET);
        assert_eq!(effective.copy_on_select, settings.terminal.copy_on_select);
    }

    #[test]
    fn effective_terminal_applies_partial_overrides() {
        let mut settings = AppSettings::default();
        settings.terminal.font_family = Some("Fira Code".to_string());
        settings.terminal.copy_on_select = true;

        let overrides = SessionOverrides {
            font_size: Some(20.0),
            term: Some("xterm".to_string()),
            ..SessionOverrides::default()
        };
        let effective = settings.effective_terminal(&overrides);

        // Overridden.
        assert_eq!(effective.font_size, 20.0);
        assert_eq!(effective.term, "xterm");
        // Inherited.
        assert_eq!(effective.charset, "UTF-8");
        assert_eq!(effective.scheme, "one-dark");
        assert_eq!(effective.scrollback_lines, 5_000);
        assert_eq!(effective.font_family, Some("Fira Code".to_string()));
        assert!(effective.copy_on_select);
    }

    #[test]
    fn effective_terminal_applies_every_override() {
        let settings = AppSettings::default();
        let overrides = SessionOverrides {
            scheme: Some("solarized".to_string()),
            font_size: Some(11.0),
            scrollback_lines: Some(100),
            term: Some("vt100".to_string()),
            charset: Some("EUC-KR".to_string()),
        };
        let effective = settings.effective_terminal(&overrides);

        assert_eq!(effective.scheme, "solarized");
        assert_eq!(effective.font_size, 11.0);
        assert_eq!(effective.scrollback_lines, 100);
        assert_eq!(effective.term, "vt100");
        // Passed through verbatim: only `rulogman-term` knows which labels exist.
        assert_eq!(effective.charset, "EUC-KR");
    }

    #[test]
    fn effective_terminal_clamps_overrides() {
        let settings = AppSettings::default();

        let huge = SessionOverrides {
            font_size: Some(500.0),
            scrollback_lines: Some(1_000_000_000),
            ..SessionOverrides::default()
        };
        let effective = settings.effective_terminal(&huge);
        assert_eq!(effective.font_size, 32.0);
        assert_eq!(effective.scrollback_lines, 100_000);

        let tiny = SessionOverrides {
            font_size: Some(-3.0),
            ..SessionOverrides::default()
        };
        assert_eq!(settings.effective_terminal(&tiny).font_size, 6.0);

        let nan = SessionOverrides {
            font_size: Some(f32::NAN),
            ..SessionOverrides::default()
        };
        assert_eq!(settings.effective_terminal(&nan).font_size, 14.0);
    }

    #[test]
    fn effective_terminal_ignores_blank_overrides() {
        let settings = AppSettings::default();
        let overrides = SessionOverrides {
            scheme: Some("  ".to_string()),
            term: Some(String::new()),
            charset: Some("   ".to_string()),
            ..SessionOverrides::default()
        };
        let effective = settings.effective_terminal(&overrides);

        assert_eq!(effective.scheme, "one-dark");
        assert_eq!(effective.term, "xterm-256color");
        assert_eq!(effective.charset, "UTF-8");
    }

    #[test]
    fn a_settings_file_without_a_highlights_section_uses_the_preset() {
        // Every settings.json on disk today predates the section, and the panes
        // those builds drew had no highlighting to lose. A missing section has
        // to mean "nothing configured", which is what resolves to the preset.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"terminal": {"copy_on_select": true}}"#).expect("write");

        let settings = AppSettings::load_from(&path).expect("load");
        assert!(settings.terminal.copy_on_select);
        assert_eq!(settings.highlights.rules, None);
        assert_eq!(
            crate::effective_highlights(&settings.highlights, None).as_ref(),
            &crate::highlight_preset()[..]
        );
    }

    #[test]
    fn the_highlights_section_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let mut settings = AppSettings::default();
        settings.highlights.rules = Some(vec![HighlightRule {
            pattern: r"\bOOM\b".to_string(),
            foreground: Some("bright_red".to_string()),
            background: Some("#101010".to_string()),
            bold: true,
            scope: crate::HighlightScope::Line,
            ignore_case: false,
            enabled: false,
        }]);
        settings.save_to(&path).expect("save");

        assert_eq!(AppSettings::load_from(&path).expect("load"), settings);
    }

    #[test]
    fn an_empty_highlight_list_is_not_the_same_as_no_list() {
        // "I turned highlighting off" has to survive the disk; were it to come
        // back as `None` the preset would reappear on the next launch.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let mut settings = AppSettings::default();
        settings.highlights.rules = Some(Vec::new());
        settings.save_to(&path).expect("save");

        let loaded = AppSettings::load_from(&path).expect("load");
        assert_eq!(loaded.highlights.rules, Some(Vec::new()));
        assert!(crate::effective_highlights(&loaded.highlights, None).is_empty());
    }

    #[test]
    fn the_preset_is_written_as_null_rather_than_as_a_copy_of_itself() {
        let value = serde_json::to_value(AppSettings::default()).unwrap();
        assert_eq!(value["highlights"]["rules"], serde_json::Value::Null);
    }

    #[test]
    fn sanitize_drops_blank_patterns_and_tidies_colours() {
        let mut settings = AppSettings::default();
        settings.highlights.rules = Some(vec![
            HighlightRule {
                pattern: "   ".to_string(),
                foreground: Some("red".to_string()),
                background: None,
                bold: false,
                scope: crate::HighlightScope::Match,
                ignore_case: true,
                enabled: true,
            },
            HighlightRule {
                pattern: String::new(),
                foreground: None,
                background: None,
                bold: false,
                scope: crate::HighlightScope::Match,
                ignore_case: true,
                enabled: true,
            },
            HighlightRule {
                pattern: "  boom  ".to_string(),
                foreground: Some("  bright_red \n".to_string()),
                background: Some("   ".to_string()),
                bold: false,
                scope: crate::HighlightScope::Match,
                ignore_case: true,
                enabled: true,
            },
        ]);

        settings.sanitize();

        let rules = settings.highlights.rules.expect("still some");
        assert_eq!(rules.len(), 1);
        // The pattern's own whitespace is significant in a regex, so it stays.
        assert_eq!(rules[0].pattern, "  boom  ");
        assert_eq!(rules[0].foreground.as_deref(), Some("bright_red"));
        assert_eq!(rules[0].background, None);
    }

    #[test]
    fn sanitize_keeps_an_unrecognised_colour_spelling() {
        // Same rule as an unknown `ui_theme`: a newer build may understand it,
        // and dropping it here would lose the user's typing for good.
        let mut settings = AppSettings::default();
        settings.highlights.rules = Some(vec![HighlightRule {
            pattern: "boom".to_string(),
            foreground: Some("rebeccapurple".to_string()),
            background: None,
            bold: false,
            scope: crate::HighlightScope::Match,
            ignore_case: true,
            enabled: true,
        }]);

        settings.sanitize();

        let rules = settings.highlights.rules.expect("still some");
        assert_eq!(rules[0].foreground.as_deref(), Some("rebeccapurple"));
    }

    #[test]
    fn sanitize_leaves_the_unconfigured_section_alone() {
        let mut settings = AppSettings::default();
        settings.sanitize();
        assert_eq!(settings.highlights.rules, None);
    }
}
