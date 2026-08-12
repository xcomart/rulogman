//! Storage of connection secrets in the operating system keychain.
//!
//! Secrets are keyed by the [`SessionProfile`](crate::SessionProfile) identifier
//! inside the `com.aihouse.rulogman` service namespace, so the profile database on
//! disk never contains a password.
//!
//! The backing store is the platform default provided by `keyring` 4.x: the
//! Windows Credential Manager, the macOS Keychain, or the freedesktop Secret
//! Service. Machines without any of those (a headless Linux box, for instance)
//! are supported in a degraded mode: [`init`] reports the failure and
//! [`SecretStore::get`] then behaves as if no secret had ever been saved.
//!
//! # Secrets saved before the rename
//!
//! Releases published as `logman` used the `com.aihouse.logman` namespace, so
//! every password saved by one is invisible to the entry names this module now
//! builds. [`SecretStore::get`] therefore falls back to the old namespace when
//! the new one has no entry, and copies whatever it finds across before
//! returning it; [`SecretStore::delete`] removes both copies.
//!
//! Adopting them lazily, one profile at a time, is a deliberate choice over
//! sweeping the keychain at start-up. A start-up sweep would have to enumerate
//! the profile database to know which entries to look for, and — worse — on
//! macOS the renamed binary carries a different signature, so touching every old
//! entry at once would greet the user with a burst of authorization prompts
//! before they have done anything. Moving a secret at the moment its profile is
//! actually used puts each prompt next to the connection that explains it, and
//! never asks about profiles the user no longer opens.

use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use keyring::{Entry, Error as KeyringError};
use uuid::Uuid;

/// Service namespace used for every credential rulogman stores.
const SERVICE: &str = "com.aihouse.rulogman";

/// Service namespace used by releases published before the rename to rulogman.
///
/// Read from, and deleted from, but never written to: see the module comment.
const LEGACY_SERVICE: &str = "com.aihouse.logman";

/// Account name used by [`init`] to force the credential store to load.
///
/// Building an entry never creates a credential, so this leaves no trace in the
/// keychain.
const INIT_PROBE_ACCOUNT: &str = "__rulogman_store_probe__";

/// Cached outcome of the first [`init`] call: `None` on success, otherwise the
/// rendered error.
static INIT_OUTCOME: OnceLock<Option<String>> = OnceLock::new();

/// Install the platform credential store.
///
/// Call this once during start-up. Repeated calls are cheap and return the same
/// result as the first one; the store is installed at most once per process.
///
/// # Errors
///
/// Fails when the platform has no usable credential store (a locked or absent
/// Secret Service, for example). Callers may ignore the error and keep running:
/// [`SecretStore::get`] degrades to "no stored secret" in that case, while
/// [`SecretStore::set`] reports the failure.
pub fn init() -> Result<()> {
    // `keyring::Entry::new` installs the platform default store the first time
    // it runs, which is the only way this crate exposes that step.
    let outcome = INIT_OUTCOME.get_or_init(|| match Entry::new(SERVICE, INIT_PROBE_ACCOUNT) {
        Ok(_) => None,
        Err(err) => Some(err.to_string()),
    });
    match outcome {
        None => Ok(()),
        Some(err) => Err(anyhow!("no usable credential store on this system: {err}")),
    }
}

/// Accessor for the OS keychain, keyed by profile identifier.
///
/// The type is a namespace only; there is nothing to construct.
pub struct SecretStore;

impl SecretStore {
    /// Build the entry for `id` in `service`, or `None` when no store is
    /// installed.
    fn entry_in(service: &str, id: Uuid) -> Result<Option<Entry>> {
        match Entry::new(service, &id.to_string()) {
            Ok(entry) => Ok(Some(entry)),
            Err(KeyringError::NoDefaultStore) => Ok(None),
            Err(err) => Err(anyhow!("failed to address keychain entry for {id}: {err}")),
        }
    }

    /// Build the keychain entry for `id`, or `None` when no store is installed.
    fn entry(id: Uuid) -> Result<Option<Entry>> {
        Self::entry_in(SERVICE, id)
    }

    /// Read the secret saved for the profile `id`.
    ///
    /// Returns `Ok(None)` when nothing is stored, and also when the platform has
    /// no usable keychain at all, so that the application keeps working without
    /// one.
    ///
    /// A secret saved by a pre-rename release is found too, and adopted into the
    /// current namespace on the way out; see the module comment.
    ///
    /// # Errors
    ///
    /// Fails only when a working keychain refuses the read (locked store,
    /// denied access, non-UTF-8 payload).
    pub fn get(id: Uuid) -> Result<Option<String>> {
        if let Err(err) = init() {
            log::warn!("treating secret for {id} as absent: {err:#}");
            return Ok(None);
        }
        let Some(entry) = Self::entry(id)? else {
            return Ok(None);
        };
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(KeyringError::NoEntry) => Self::adopt_legacy(id),
            Err(err) => Err(anyhow!("failed to read keychain entry for {id}: {err}")),
        }
    }

    /// Look up `id` in the pre-rename namespace and copy what it finds across.
    ///
    /// Called only once the current namespace is known to hold nothing for `id`.
    /// The old entry is left in place, so a user who goes back to a `logman`
    /// build still finds their password there.
    ///
    /// Everything that can go wrong here is reported as "no secret": the
    /// migration is a convenience, and a keychain that refuses to talk about a
    /// namespace this build no longer owns must not turn into a failure to open
    /// a profile that legitimately has no password.
    ///
    /// # Errors
    ///
    /// Fails only when the old entry cannot be addressed at all, which means the
    /// keychain is in a state [`SecretStore::get`] would have failed on anyway.
    fn adopt_legacy(id: Uuid) -> Result<Option<String>> {
        let Some(entry) = Self::entry_in(LEGACY_SERVICE, id)? else {
            return Ok(None);
        };
        let secret = match entry.get_password() {
            Ok(secret) => secret,
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(err) => {
                log::warn!("failed to read the pre-rename keychain entry for {id}: {err}");
                return Ok(None);
            }
        };

        match Self::set(id, &secret) {
            Ok(()) => log::info!("adopted the secret saved for {id} by a pre-rename release"),
            // Worth carrying on for: the caller gets the password it asked for,
            // and the next read will simply try the same copy again.
            Err(err) => log::warn!("failed to adopt the pre-rename secret for {id}: {err:#}"),
        }
        Ok(Some(secret))
    }

    /// Save `secret` for the profile `id`, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Fails when no credential store is available or when the store rejects
    /// the write. Unlike [`SecretStore::get`] this never fails silently: a
    /// secret the user asked to save must not vanish unnoticed.
    pub fn set(id: Uuid, secret: &str) -> Result<()> {
        init()?;
        let entry = Self::entry(id)?
            .ok_or_else(|| anyhow!("no credential store available to save the secret for {id}"))?;
        entry
            .set_password(secret)
            .map_err(|err| anyhow!("failed to save keychain entry for {id}: {err}"))
    }

    /// Delete the secret saved for the profile `id`.
    ///
    /// Both namespaces are cleared, so a password the user removes does not
    /// reappear out of the pre-rename copy on the next read.
    ///
    /// Deleting a secret that does not exist succeeds, as does deleting on a
    /// machine without a credential store: in both cases nothing is left behind.
    ///
    /// # Errors
    ///
    /// Fails when a working keychain refuses the deletion.
    pub fn delete(id: Uuid) -> Result<()> {
        if let Err(err) = init() {
            log::warn!("nothing to delete for {id}: {err:#}");
            return Ok(());
        }
        Self::delete_in(SERVICE, id)?;
        Self::delete_in(LEGACY_SERVICE, id)
    }

    /// Delete the entry for `id` from `service`, tolerating its absence.
    ///
    /// # Errors
    ///
    /// Fails when a working keychain refuses the deletion.
    fn delete_in(service: &str, id: Uuid) -> Result<()> {
        let Some(entry) = Self::entry_in(service, id)? else {
            return Ok(());
        };
        match entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(err) => Err(anyhow!(
                "failed to delete the {service} keychain entry for {id}: {err}"
            )),
        }
    }

    /// Save `secret` for `id` in the pre-rename namespace, as an old build did.
    ///
    /// Test-only: nothing shipping may write to a namespace this build has
    /// stopped owning.
    ///
    /// # Errors
    ///
    /// Fails when no credential store is available or when the store rejects
    /// the write.
    #[cfg(test)]
    fn set_legacy(id: Uuid, secret: &str) -> Result<()> {
        init()?;
        let entry = Self::entry_in(LEGACY_SERVICE, id)?
            .ok_or_else(|| anyhow!("no credential store available to save the secret for {id}"))?;
        entry
            .set_password(secret)
            .map_err(|err| anyhow!("failed to save the pre-rename entry for {id}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_namespace_matches_the_project_id() {
        assert_eq!(SERVICE, "com.aihouse.rulogman");
    }

    #[test]
    fn legacy_service_namespace_matches_the_former_project_id() {
        // Pinned: change this string and every password saved before the rename
        // becomes unreachable.
        assert_eq!(LEGACY_SERVICE, "com.aihouse.logman");
        assert_ne!(LEGACY_SERVICE, SERVICE);
    }

    #[test]
    #[ignore = "touches the real OS keychain"]
    fn init_is_idempotent() {
        // Installing the store may legitimately fail (headless CI), but the
        // answer must be stable across calls and must never panic.
        let first = init().is_ok();
        let second = init().is_ok();
        assert_eq!(first, second);
    }

    #[test]
    #[ignore = "touches the real OS keychain"]
    fn get_of_unknown_id_is_none() {
        // On a machine with no keychain this exercises the degraded path; with
        // one, it exercises the `NoEntry` path. Either way: `Ok(None)`.
        let missing = SecretStore::get(Uuid::new_v4()).expect("get must not fail");
        assert_eq!(missing, None);
    }

    #[test]
    #[ignore = "touches the real OS keychain"]
    fn set_get_delete_round_trip() {
        init().expect("credential store");
        let id = Uuid::new_v4();

        assert_eq!(SecretStore::get(id).expect("get missing"), None);

        SecretStore::set(id, "hunter2").expect("set");
        assert_eq!(SecretStore::get(id).expect("get"), Some("hunter2".into()));

        SecretStore::set(id, "hunter3").expect("overwrite");
        assert_eq!(SecretStore::get(id).expect("get"), Some("hunter3".into()));

        SecretStore::delete(id).expect("delete");
        assert_eq!(SecretStore::get(id).expect("get deleted"), None);
    }

    #[test]
    #[ignore = "touches the real OS keychain"]
    fn get_adopts_a_secret_saved_before_the_rename() {
        init().expect("credential store");
        let id = Uuid::new_v4();

        SecretStore::set_legacy(id, "hunter2").expect("legacy set");
        // Nothing under the current namespace yet, so this has to come from the
        // fallback.
        assert_eq!(SecretStore::get(id).expect("get"), Some("hunter2".into()));

        // And the fallback must have copied it across, not merely read it: the
        // current namespace now answers on its own.
        let entry = SecretStore::entry(id).expect("entry").expect("store");
        assert_eq!(entry.get_password().expect("adopted password"), "hunter2");

        // Deleting clears both copies, so the old one cannot resurrect itself.
        SecretStore::delete(id).expect("delete");
        assert_eq!(SecretStore::get(id).expect("get deleted"), None);
    }

    #[test]
    #[ignore = "touches the real OS keychain"]
    fn current_namespace_wins_over_the_pre_rename_one() {
        init().expect("credential store");
        let id = Uuid::new_v4();

        SecretStore::set_legacy(id, "stale").expect("legacy set");
        SecretStore::set(id, "current").expect("set");
        assert_eq!(SecretStore::get(id).expect("get"), Some("current".into()));

        SecretStore::delete(id).expect("delete");
    }

    #[test]
    #[ignore = "touches the real OS keychain"]
    fn delete_missing_entry_is_ok() {
        init().expect("credential store");
        SecretStore::delete(Uuid::new_v4()).expect("delete of missing entry must succeed");
    }
}
