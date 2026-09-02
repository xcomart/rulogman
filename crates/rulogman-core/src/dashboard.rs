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

/// Which way a [`LayoutNode::Split`] divides the space it is given.
///
/// This is a deliberate parallel to the pane-tree axis the GUI already uses to
/// arrange tail views. It is redeclared here rather than borrowed because this
/// crate has no view layer to borrow it from: `rulogman-core` depends on serde,
/// uuid and the OS keychain and *nothing* from the GUI stack (no `gpui`, no
/// `rugpui`), on purpose, so that everything it persists can be exercised from a
/// plain unit test. A geometry the user saved has to be describable without
/// pulling a windowing toolkit into the config layer, so the shape of it lives
/// here in the smallest terms that survive to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutAxis {
    /// The split runs left/right: `first` sits beside `second`.
    Horizontal,
    /// The split runs top/bottom: `first` sits above `second`.
    Vertical,
}

/// One node of a saved pane arrangement: either a single pane, or a division of
/// the space into two child arrangements.
///
/// A dashboard's [`panes`](Dashboard::panes) list stays the source of truth for
/// *which* files are shown and gates the credentials to reach them; a
/// [`LayoutNode`] tree is optional geometry laid *over* that list, recording the
/// hand-tuned arrangement the user built — which pane sits where, how each split
/// is oriented, and where each divider was dragged — so it can be restored
/// exactly on the next run instead of being re-derived as a fresh grid.
///
/// A [`Leaf`](LayoutNode::Leaf) names its pane by **index into
/// [`Dashboard::panes`]**, never by any runtime handle. The GUI's own pane-tree
/// node ids are process-local: they are minted afresh each time the tree is
/// built and mean nothing once the process exits, so a saved file that spoke in
/// them would restore to noise. The position of a pane within the dashboard's
/// list is the only identity that is stable across runs, so that is what a leaf
/// stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutNode {
    /// A single pane, identified by its index into [`Dashboard::panes`].
    ///
    /// The index is validated lazily by [`Dashboard::valid_layout`] rather than
    /// on load: a file may name an index that no longer exists (a pane was
    /// removed since the layout was saved), and that is a reason to fall back to
    /// a grid, not to reject the whole dashboard.
    Leaf {
        /// Position of the referenced pane in [`Dashboard::panes`].
        pane: usize,
    },
    /// A division of the space into two child arrangements.
    Split {
        /// Whether the divider runs left/right or top/bottom.
        axis: LayoutAxis,
        /// Fraction of the space, in `0.0..=1.0`, given to `first`; the rest
        /// goes to `second`.
        ///
        /// An `f32` because it is a screen ratio a user dragged to, wanting no
        /// more precision than that — which is also why neither this type nor
        /// [`Dashboard`] can derive `Eq`/`Hash` any longer: a float has no total
        /// equality to offer.
        ratio: f32,
        /// The arrangement filling the first (left or top) portion.
        first: Box<LayoutNode>,
        /// The arrangement filling the second (right or bottom) portion.
        second: Box<LayoutNode>,
    },
}

/// Deserialize a [`Dashboard::layout`] value leniently: a shape this build
/// cannot parse into a [`LayoutNode`] degrades to `None` instead of failing the
/// whole dashboard load.
///
/// `#[serde(default)]` alone only covers an *absent* field; a `layout` that is
/// *present* but malformed — a string `"grid"` written by an older build, an
/// object shaped by a newer one, a hand-edited botch — would otherwise abort the
/// entire load and cost the user every dashboard in the file. Routing the field
/// through here turns any unparseable value into "no saved geometry", which is
/// exactly the fallback [`Dashboard::valid_layout`] already exists to trigger.
fn lenient_layout<'de, D>(deserializer: D) -> Result<Option<LayoutNode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Land the value as generic JSON first, then attempt the real parse; a
    // failure there is swallowed to `None` rather than propagated.
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<LayoutNode>(value).ok())
}

/// A named arrangement of followed files from any number of connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dashboard {
    /// Stable identifier, so that renaming a dashboard is a rename rather than
    /// a delete and an insert.
    pub id: Uuid,
    /// Human-readable name shown in the UI. Not unique: identity here is the
    /// id, exactly as it is for a profile.
    pub name: String,
    /// Whether launching rulogman should open this dashboard straight away.
    ///
    /// It lives here rather than in [`AppSettings`](crate::AppSettings) because
    /// it is a property of the *arrangement*, not a preference about the
    /// application: "this is the one I watch every morning" is something the
    /// dashboard is, and it travels with the dashboard when the file is copied
    /// to another machine or the entry is renamed. Nor is it a single choice
    /// the settings could hold as one id: any number of dashboards may carry
    /// the flag, and each one that does opens its own tab at launch.
    ///
    /// `false` — the state every dashboard is created in — is omitted from the
    /// file, the way every other default rulogman persists is, so marking a
    /// dashboard is a line that appears rather than a line that changes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_at_startup: bool,
    /// The files this dashboard shows, in the order they are laid out.
    ///
    /// Empty is a legitimate state rather than a broken one — a dashboard the
    /// user has named and not yet filled in — and is omitted from the file
    /// again while it lasts, the way every other empty list rulogman persists
    /// is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<DashboardPane>,
    /// Optional saved geometry over [`panes`](Self::panes): the hand-tuned
    /// arrangement the user built, restored exactly next run.
    ///
    /// `None` — and omitted from the file — is the ordinary state: a dashboard
    /// the user has never rearranged, which the GUI lays out as a default grid.
    /// When present, the tree is still only *advisory*: it is honoured only if
    /// [`valid_layout`](Self::valid_layout) confirms it still matches the pane
    /// list, so a `panes` edit that outdates it costs nothing.
    ///
    /// The value is read through [`lenient_layout`], so a `layout` this build
    /// cannot parse loads as `None` rather than failing the dashboard.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_layout"
    )]
    pub layout: Option<LayoutNode>,
}

impl Dashboard {
    /// Create an empty dashboard with a freshly generated identifier.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            open_at_startup: false,
            panes: Vec::new(),
            layout: None,
        }
    }

    /// The saved [`layout`](Self::layout), but only when it is a faithful
    /// arrangement of *exactly* this dashboard's panes.
    ///
    /// A layout is faithful when the set of leaf indices in its tree is a
    /// permutation of `0..panes.len()`: every pane placed exactly once, every
    /// index in range, none missing and none used twice. Anything else —
    /// no layout at all, an index past the end, a pane dropped or shown twice,
    /// or any layout over an empty pane list — returns `None`, which is the
    /// caller's cue to fall back to a fresh grid rather than restoring a
    /// geometry that would drop or duplicate a pane.
    ///
    /// This is where a layout that has *drifted* from its pane list is caught:
    /// the settings editor can add or remove a pane long after the arrangement
    /// was saved, and the layout is checked against the current list every time
    /// rather than being eagerly repaired or invalidated on edit. A malformed or
    /// future-written tree never panics here — it simply fails the check.
    pub fn valid_layout(&self) -> Option<&LayoutNode> {
        let layout = self.layout.as_ref()?;

        // A layout over no panes can never be faithful, and would otherwise slip
        // through the permutation check below only if the tree were also empty —
        // which it cannot be, a tree always has at least one leaf.
        let count = self.panes.len();
        if count == 0 {
            return None;
        }

        // Collect the leaves; bail the moment one is out of range or repeats, so
        // a pathological tree costs no more than a well-formed one.
        let mut seen = vec![false; count];
        let mut leaves = 0usize;
        if !collect_leaves(layout, &mut seen, &mut leaves) {
            return None;
        }

        // Every slot filled exactly once means the count matches and, since no
        // duplicate got this far, the set is precisely `0..count`.
        if leaves == count { Some(layout) } else { None }
    }
}

/// Walk `node`, marking each leaf's index in `seen`. Returns `false` — stopping
/// the walk — the first time a leaf index is out of range or already marked, so
/// [`Dashboard::valid_layout`] can reject a drifted or malformed tree without
/// panicking. On success `leaves` holds the number of distinct in-range leaves.
fn collect_leaves(node: &LayoutNode, seen: &mut [bool], leaves: &mut usize) -> bool {
    match node {
        LayoutNode::Leaf { pane } => match seen.get_mut(*pane) {
            Some(slot) if !*slot => {
                *slot = true;
                *leaves += 1;
                true
            }
            _ => false,
        },
        LayoutNode::Split { first, second, .. } => {
            collect_leaves(first, seen, leaves) && collect_leaves(second, seen, leaves)
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
        //
        // `layout` is now a real field, but the guarantee is unchanged: a
        // `layout` shaped in a way this build cannot parse into a `LayoutNode`
        // — here an object with a scheme a newer build invented — must degrade
        // to no saved geometry rather than aborting the whole load. Without the
        // lenient deserializer this present-but-unparseable value would fail the
        // entire dashboard, so this test also pins that behaviour down.
        let json = r#"{
            "dashboards": [
                {
                    "id": "6f1a1d1e-0000-4000-8000-000000000001",
                    "name": "deploy",
                    "layout": { "grid": { "rows": 2, "cols": 3 } },
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
        // The unparseable layout was swallowed to None, not carried through.
        assert_eq!(store.dashboards()[0].layout, None);
    }

    #[test]
    fn a_malformed_layout_string_degrades_to_none_without_failing_the_load() {
        // The exact shape an older build wrote when `layout` was an ignored
        // free-form key: a bare string. It must not parse as a LayoutNode and
        // must not abort the load either.
        let json = r#"{
            "dashboards": [
                {
                    "id": "6f1a1d1e-0000-4000-8000-000000000001",
                    "name": "deploy",
                    "layout": "grid",
                    "panes": [
                        {
                            "profile": "6f1a1d1e-0000-4000-8000-000000000002",
                            "path": "/var/log/syslog"
                        }
                    ]
                }
            ]
        }"#;
        let store: DashboardStore = serde_json::from_str(json).expect("deserialize");
        assert_eq!(store.dashboards()[0].layout, None);
    }

    fn split(axis: LayoutAxis, ratio: f32, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn three_pane_layout() -> LayoutNode {
        // A vertical split whose second half is itself a horizontal split, so
        // the tree has some real shape: leaves 0, 1 and 2 each once.
        split(
            LayoutAxis::Vertical,
            0.5,
            LayoutNode::Leaf { pane: 0 },
            split(
                LayoutAxis::Horizontal,
                0.25,
                LayoutNode::Leaf { pane: 1 },
                LayoutNode::Leaf { pane: 2 },
            ),
        )
    }

    #[test]
    fn a_dashboard_with_a_layout_round_trips_through_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");

        let mut dashboard = Dashboard::new("tuned");
        dashboard.panes.push(pane("/var/log/a.log"));
        dashboard.panes.push(pane("/var/log/b.log"));
        dashboard.panes.push(pane("/var/log/c.log"));
        dashboard.layout = Some(three_pane_layout());

        let mut store = DashboardStore::default();
        store.upsert(dashboard.clone());
        store.save_to(&path).expect("save");

        let loaded = DashboardStore::load_from(&path).expect("load");
        assert_eq!(loaded.dashboards()[0].layout, Some(three_pane_layout()));
        assert_eq!(loaded.dashboards(), &[dashboard]);
    }

    #[test]
    fn a_layout_of_none_is_omitted_from_the_file() {
        let mut store = DashboardStore::default();
        store.upsert(sample("plain"));

        let json = serde_json::to_string(&store).expect("serialize");
        assert!(
            !json.contains("layout"),
            "an absent layout must be skipped, got {json}"
        );
    }

    #[test]
    fn a_new_dashboard_does_not_open_at_startup_and_says_nothing_about_it() {
        // The flag is a mark the user puts on a dashboard, so a fresh one
        // carries neither the mark nor a line in the file admitting it has
        // none.
        let dashboard = Dashboard::new("plain");
        assert!(!dashboard.open_at_startup);

        let mut store = DashboardStore::default();
        store.upsert(dashboard);
        let json = serde_json::to_string(&store).expect("serialize");
        assert!(
            !json.contains("open_at_startup"),
            "an unmarked dashboard must be skipped, got {json}"
        );
    }

    #[test]
    fn a_dashboard_marked_to_open_at_startup_round_trips_through_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboards.json");

        let mut morning = sample("morning");
        morning.open_at_startup = true;
        let plain = sample("plain");

        let mut store = DashboardStore::default();
        store.upsert(morning.clone());
        store.upsert(plain.clone());
        store.save_to(&path).expect("save");

        let loaded = DashboardStore::load_from(&path).expect("load");
        assert_eq!(loaded.dashboards(), &[morning, plain]);
        // Several dashboards may carry the flag, so it is read per dashboard
        // rather than as one chosen entry.
        assert!(loaded.dashboards()[0].open_at_startup);
        assert!(!loaded.dashboards()[1].open_at_startup);
    }

    #[test]
    fn a_file_written_before_the_flag_existed_loads_as_unmarked() {
        // Every dashboards.json in the wild predates the field, and none of
        // them means "open me at launch".
        let json = r#"{
            "dashboards": [
                {
                    "id": "6f1a1d1e-0000-4000-8000-000000000001",
                    "name": "deploy",
                    "panes": [
                        {
                            "profile": "6f1a1d1e-0000-4000-8000-000000000002",
                            "path": "/var/log/syslog"
                        }
                    ]
                }
            ]
        }"#;
        let store: DashboardStore = serde_json::from_str(json).expect("deserialize");
        assert!(!store.dashboards()[0].open_at_startup);
    }

    #[test]
    fn valid_layout_accepts_a_faithful_arrangement() {
        let mut dashboard = Dashboard::new("ok");
        dashboard.panes.push(pane("/a"));
        dashboard.panes.push(pane("/b"));
        dashboard.panes.push(pane("/c"));
        dashboard.layout = Some(three_pane_layout());

        assert_eq!(dashboard.valid_layout(), Some(&three_pane_layout()));
    }

    #[test]
    fn valid_layout_rejects_a_missing_pane() {
        // Leaves cover {0, 1} but the dashboard has three panes: pane 2 would
        // vanish if this layout were honoured.
        let mut dashboard = Dashboard::new("short");
        dashboard.panes.push(pane("/a"));
        dashboard.panes.push(pane("/b"));
        dashboard.panes.push(pane("/c"));
        dashboard.layout = Some(split(
            LayoutAxis::Horizontal,
            0.5,
            LayoutNode::Leaf { pane: 0 },
            LayoutNode::Leaf { pane: 1 },
        ));

        assert_eq!(dashboard.valid_layout(), None);
    }

    #[test]
    fn valid_layout_rejects_an_out_of_range_index() {
        let mut dashboard = Dashboard::new("over");
        dashboard.panes.push(pane("/a"));
        dashboard.panes.push(pane("/b"));
        dashboard.layout = Some(split(
            LayoutAxis::Vertical,
            0.5,
            LayoutNode::Leaf { pane: 0 },
            LayoutNode::Leaf { pane: 5 },
        ));

        assert_eq!(dashboard.valid_layout(), None);
    }

    #[test]
    fn valid_layout_rejects_a_duplicate_index() {
        let mut dashboard = Dashboard::new("dup");
        dashboard.panes.push(pane("/a"));
        dashboard.panes.push(pane("/b"));
        dashboard.layout = Some(split(
            LayoutAxis::Vertical,
            0.5,
            LayoutNode::Leaf { pane: 0 },
            LayoutNode::Leaf { pane: 0 },
        ));

        assert_eq!(dashboard.valid_layout(), None);
    }

    #[test]
    fn valid_layout_is_none_without_a_layout() {
        let mut dashboard = Dashboard::new("bare");
        dashboard.panes.push(pane("/a"));
        assert_eq!(dashboard.valid_layout(), None);
    }

    #[test]
    fn valid_layout_accepts_a_single_leaf() {
        let mut dashboard = Dashboard::new("one");
        dashboard.panes.push(pane("/a"));
        dashboard.layout = Some(LayoutNode::Leaf { pane: 0 });
        assert_eq!(
            dashboard.valid_layout(),
            Some(&LayoutNode::Leaf { pane: 0 })
        );
    }

    #[test]
    fn valid_layout_rejects_any_layout_over_no_panes() {
        let mut dashboard = Dashboard::new("empty");
        dashboard.layout = Some(LayoutNode::Leaf { pane: 0 });
        assert_eq!(dashboard.valid_layout(), None);
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
