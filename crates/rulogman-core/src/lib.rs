//! Platform-independent core of rulogman: configuration paths, application
//! settings, saved session profiles, the dashboards that arrange followed files
//! across several of them, OS keychain access, and the trusted host key
//! database.
//!
//! This crate owns everything rulogman persists on disk or in the system
//! credential store. It knows nothing about SSH transport, terminal emulation,
//! or the GUI, so it can be exercised entirely from tests.
//!
//! ```no_run
//! use rulogman_core::{AuthMethod, ProfileStore, SessionProfile};
//!
//! # fn main() -> anyhow::Result<()> {
//! rulogman_core::init_secrets().ok(); // a missing keychain is not fatal
//!
//! let mut store = ProfileStore::load()?;
//! store.upsert(SessionProfile::new(
//!     "prod",
//!     "example.com",
//!     22,
//!     "alice",
//!     AuthMethod::Agent,
//! ));
//! store.save()?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

pub mod dashboard;
pub mod known_hosts;
pub mod migrate;
pub mod paths;
pub mod profile;
pub mod secrets;
pub mod settings;

pub use dashboard::{Dashboard, DashboardPane, DashboardStore};
pub use known_hosts::{HostKeyStatus, KnownHosts};
pub use migrate::migrate_from_logman;
pub use paths::{
    config_dir, config_file, dashboards_file, known_hosts_file, schemes_dir, settings_file,
    syntaxes_dir, ui_themes_dir,
};
pub use profile::{
    AuthMethod, HopRule, ProfileStore, SessionOverrides, SessionProfile, TailRule, TunnelRule,
};
pub use secrets::{SecretStore, init as init_secrets};
pub use settings::{
    AppSettings, ConnectionSettings, DEFAULT_CHARSET, EditorSettings, EffectiveTerminal,
    FilesSettings, TerminalSettings, TitlebarStyle, WindowSettings,
};
