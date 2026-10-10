//! Credentials.

use super::*;

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
    // Agent profiles never need the keychain, even if an older profile still
    // carries a remembered-secret flag from its previous authentication mode.
    if matches!(profile.auth, AuthMethod::Agent) {
        return Some(SshAuth::Agent);
    }
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
pub(super) enum Credentials {
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
pub(super) fn decide_credentials<F>(
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
        AuthMethod::Agent => Credentials::Ready(SshAuth::Agent),
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
pub(super) fn stored_secret(id: Uuid) -> Option<String> {
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
pub(super) fn key_opens_unlocked(path: &Path) -> bool {
    match russh::keys::load_secret_key(path, None) {
        Ok(_) => true,
        Err(err) => {
            log::debug!("the key at {} needs the dialog: {err}", path.display());
            false
        }
    }
}
