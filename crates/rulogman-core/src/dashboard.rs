//! Dashboards: named arrangements of followed files, and their JSON-backed
//! store.
//!
//! A [`SessionProfile`](crate::SessionProfile) already knows which files *it*
//! follows, and that is the right place for the answer as long as the question
//! is "what does this host show me when I open it". A dashboard asks the other
//! question — "what do I want to watch while this deploy goes out" — and the
//! answer to that one rarely stops at one host: the application log on the two
//! web boxes, the slow-query log on the database, the proxy's error log. So a
//! dashboard is a list of files that names the connection to reach each one on,
//! and lives in a file of its own rather than inside any single profile.
//!
//! What a dashboard deliberately does *not* hold is a connection: a
//! [`DashboardPane`] carries a [`SessionProfile::id`](crate::SessionProfile::id)
//! and nothing else about the host. Copying host, user and credentials into
//! this file would fork them the moment the profile is edited, and there is
//! only one sensible reading of "the log on web-01" — whatever `web-01` means
//! right now.
//!
//! The price of referring rather than copying is that the reference can dangle:
//! nothing stops the user deleting a profile a dashboard points at, and nothing
//! here tries to. A pane whose profile is gone is kept exactly as it was, so
//! that the UI can show it as broken and the user can repoint it; silently
//! dropping it would lose the path, and silently rebinding it to another
//! profile would open a *different* file than the one that was asked for.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::paths::{dashboards_file, strip_bom, write_atomic};

/// One pane of a dashboard: a file to follow, on the connection that reaches
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardPane {
    /// Identifier of the [`SessionProfile`](crate::SessionProfile) this file is
    /// reached over.
    ///
    /// May dangle: the profile it names can be deleted while the dashboard
    /// still points at it. See the module documentation for why that is left
    /// alone rather than repaired.
    pub profile: Uuid,
    /// Absolute path of the file on the remote host, as that host spells it.
    ///
    /// A [`String`] rather than a `PathBuf`, for the same reason
    /// [`TailRule::path`](crate::TailRule::path) is one: the path belongs to
    /// the *remote* filesystem, and `PathBuf` would parse it with this
    /// machine's rules.
    pub path: String,
}

/// A named arrangement of followed files from any number of connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dashboard {
    /// Stable identifier, so that renaming a dashboard is a rename rather than
    /// a delete and an insert.
    pub id: Uuid,
    /// Human-readable name shown in the UI. Not unique: identity here is the
    /// id, exactly as it is for a profile.
    pub name: String,
    /// The files this dashboard shows, in the order they are laid out.
    ///
    /// Empty is a legitimate state rather than a broken one — a dashboard the
    /// user has named and not yet filled in — and is omitted from the file
    /// again while it lasts, the way every other empty list rulogman persists
    /// is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<DashboardPane>,
}

impl Dashboard {
    /// Create an empty dashboard with a freshly generated identifier.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            panes: Vec::new(),
        }
    }
}

/// Collection of saved [`Dashboard`]s, persisted as JSON.
///
/// Shaped like [`ProfileStore`](crate::ProfileStore) down to the method names,
/// because it is the same kind of thing: a user-ordered list of records keyed
/// by [`Uuid`], read whole when a dialog opens and written whole when it saves.
/// The one method it does not have is `duplicate`, which exists on the profile
/// store only because a copied profile has a keychain entry to *not* inherit; a
/// dashboard holds no secret, so a copy of one is nothing but a `clone` with a
/// fresh id and there is no reason for the store to be involved.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DashboardStore {
    /// Dashboards in user-visible order.
    #[serde(default)]
    dashboards: Vec<Dashboard>,
}

impl DashboardStore {
    /// Load the store from the default configuration file.
    ///
    /// A missing file is not an error: it yields an empty store, which is what
    /// every installation that has never made a dashboard looks like.
    ///
    /// # Errors
    ///
    /// Fails when the configuration directory cannot be determined, the file
    /// cannot be read, or its contents are not valid JSON.
    pub fn load() -> Result<Self> {
        Self::load_from(&dashboards_file()?)
    }

    /// Load the store from an explicit path.
    ///
    /// A missing file yields an empty store.
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
        serde_json::from_slice(strip_bom(&data))
            .with_context(|| format!("failed to parse dashboards from {}", path.display()))
    }

    /// Write the store to the default configuration file.
    ///
    /// # Errors
    ///
    /// Fails when the configuration directory cannot be determined or created,
    /// or when the file cannot be written.
    pub fn save(&self) -> Result<()> {
        self.save_to(&dashboards_file()?)
    }

    /// Write the store to an explicit path, creating parent directories.
    ///
    /// The write is atomic: the data lands in a temporary sibling file that is
    /// then renamed over `path`.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created or the file cannot be
    /// written.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("failed to serialize dashboards")?;
        write_atomic(path, &json)
    }

    /// All dashboards, in insertion order.
    pub fn dashboards(&self) -> &[Dashboard] {
        &self.dashboards
    }

    /// Look up a dashboard by identifier.
    pub fn get(&self, id: Uuid) -> Option<&Dashboard> {
        self.dashboards.iter().find(|d| d.id == id)
    }

    /// Insert `dashboard`, replacing an existing entry with the same
    /// identifier.
    ///
    /// Replacement keeps the original position in the list.
    pub fn upsert(&mut self, dashboard: Dashboard) {
        match self.dashboards.iter_mut().find(|d| d.id == dashboard.id) {
            Some(slot) => *slot = dashboard,
            None => self.dashboards.push(dashboard),
        }
    }

    /// Remove the dashboard with the given identifier and return it.
    pub fn remove(&mut self, id: Uuid) -> Option<Dashboard> {
        let index = self.dashboards.iter().position(|d| d.id == id)?;
        Some(self.dashboards.remove(index))
    }

    /// Number of stored dashboards.
    pub fn len(&self) -> usize {
        self.dashboards.len()
    }

    /// Whether the store holds no dashboards.
    pub fn is_empty(&self) -> bool {
        self.dashboards.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(path: &str) -> DashboardPane {
        DashboardPane {
            profile: Uuid::new_v4(),
            path: path.to_owned(),
        }
    }

    fn sample(name: &str) -> Dashboard {
        let mut dashboard = Dashboard::new(name);
        dashboard.panes.push(pane("/var/log/syslog"));
        dashboard
    }

    #[test]
    fn new_assigns_unique_ids_and_no_panes() {
        let a = Dashboard::new("deploy");
        let b = Dashboard::new("deploy");
        assert_ne!(a.id, b.id);
        assert!(a.panes.is_empty());
    }

    #[test]
    fn save_to_load_from_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cfg").join("dashboards.json");

        let mut store = DashboardStore::default();
        let mut first = sample("deploy");
        first.panes.push(pane("/var/log/nginx/error.log"));
        let second = sample("nightly");
        store.upsert(first.clone());
        store.upsert(second.clone());

        store.save_to(&path).expect("save");
        let loaded = DashboardStore::load_from(&path).expect("load");

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.dashboards(), &[first, second]);
    }

    #[test]
    fn save_to_overwrites_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");

        DashboardStore::default()
            .save_to(&path)
            .expect("first save");

        let mut store = DashboardStore::default();
        store.upsert(sample("only"));
        store.save_to(&path).expect("second save");

        assert_eq!(DashboardStore::load_from(&path).expect("load").len(), 1);
    }

    #[test]
    fn load_from_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DashboardStore::load_from(&dir.path().join("nope.json")).expect("load");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_file_holding_no_dashboards_is_not_an_error() {
        // What the store writes once the last dashboard has been removed, and
        // what a hand-written file may perfectly well say.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");
        std::fs::write(&path, "{}").expect("write");

        assert!(DashboardStore::load_from(&path).expect("load").is_empty());
    }

    #[test]
    fn load_from_tolerates_a_utf8_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");

        let mut store = DashboardStore::default();
        store.upsert(sample("bom"));
        store.save_to(&path).expect("save");

        // Rewrite the file the way a Windows editor would.
        let saved = std::fs::read(&path).expect("read");
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(&saved);
        std::fs::write(&path, with_bom).expect("write");

        let loaded = DashboardStore::load_from(&path).expect("load");
        assert_eq!(loaded.dashboards()[0].name, "bom");
    }

    #[test]
    fn a_file_written_by_a_later_build_still_loads() {
        // Forward compatibility runs the same way the profile file's does: a
        // key this build has never heard of is ignored rather than fatal, so
        // downgrading rulogman does not cost the user their dashboards.
        let json = r#"{
            "dashboards": [
                {
                    "id": "6f1a1d1e-0000-4000-8000-000000000001",
                    "name": "deploy",
                    "layout": "grid",
                    "panes": [
                        {
                            "profile": "6f1a1d1e-0000-4000-8000-000000000002",
                            "path": "/var/log/syslog",
                            "follow": true
                        }
                    ]
                }
            ],
            "revision": 7
        }"#;
        let store: DashboardStore = serde_json::from_str(json).expect("deserialize");
        assert_eq!(store.len(), 1);
        assert_eq!(store.dashboards()[0].panes[0].path, "/var/log/syslog");
    }

    #[test]
    fn a_dashboard_with_no_panes_round_trips() {
        // The state a dashboard is in between being created and being filled
        // in. The empty list is left out of the file and read back as empty.
        let mut store = DashboardStore::default();
        let empty = Dashboard::new("blank");
        store.upsert(empty.clone());

        let json = serde_json::to_string(&store).expect("serialize");
        assert!(
            !json.contains("panes"),
            "an empty pane list must be skipped, got {json}"
        );
        let back: DashboardStore = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.dashboards(), &[empty]);
    }

    #[test]
    fn upsert_replaces_same_id_in_place() {
        let mut store = DashboardStore::default();
        let keep = sample("keep");
        let mut original = sample("original");
        store.upsert(keep.clone());
        store.upsert(original.clone());

        original.name = "renamed".to_string();
        original.panes.clear();
        store.upsert(original.clone());

        assert_eq!(store.len(), 2);
        assert_eq!(store.dashboards()[0].id, keep.id);
        assert_eq!(
            store.get(original.id).map(|d| d.name.as_str()),
            Some("renamed")
        );
        assert!(
            store
                .get(original.id)
                .expect("still there")
                .panes
                .is_empty()
        );
    }

    #[test]
    fn remove_returns_the_dashboard() {
        let mut store = DashboardStore::default();
        let dashboard = sample("victim");
        store.upsert(dashboard.clone());

        assert!(!store.is_empty());
        assert_eq!(store.remove(dashboard.id), Some(dashboard.clone()));
        assert!(store.is_empty());
        assert_eq!(store.remove(dashboard.id), None);
        assert_eq!(store.get(dashboard.id), None);
    }

    #[test]
    fn a_pane_pointing_at_a_deleted_profile_survives_a_round_trip() {
        // The reference is not checked against anything on the way in or out:
        // repairing it is the UI's job, and losing the path would be the one
        // thing the user cannot undo.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");

        let gone = Uuid::new_v4();
        let mut dashboard = Dashboard::new("orphans");
        dashboard.panes.push(DashboardPane {
            profile: gone,
            path: "/var/log/syslog".to_owned(),
        });
        let mut store = DashboardStore::default();
        store.upsert(dashboard);
        store.save_to(&path).expect("save");

        let loaded = DashboardStore::load_from(&path).expect("load");
        assert_eq!(loaded.dashboards()[0].panes[0].profile, gone);
    }
}
