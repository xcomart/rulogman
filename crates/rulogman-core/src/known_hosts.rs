//! Trust-on-first-use database of SSH host key fingerprints.
//!
//! The file format is one record per line:
//!
//! ```text
//! # host port algorithm fingerprint
//! example.com 22 ssh-ed25519 SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU
//! ```
//!
//! Blank lines and lines starting with `#` are ignored, and host names are
//! compared case-insensitively. Records are keyed by host, port *and* key
//! algorithm, mirroring OpenSSH: a server may legitimately offer several key
//! types for the same address. What a host may not do is switch to a key type
//! nothing has seen from it while one is on record: that is reported as a
//! [`HostKeyStatus::Mismatch`], not as a host that is merely new.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::paths::{known_hosts_file, write_atomic};

/// Header written at the top of a saved `known_hosts` file.
const FILE_HEADER: &str = "# rulogman known hosts: <host> <port> <algorithm> <fingerprint>";

/// Result of checking a host key against the trust database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// No key of any algorithm is on record for this host and port; ask the
    /// user whether to accept it.
    Unknown,
    /// The presented fingerprint matches the stored one.
    Trusted,
    /// A different key is on record for this host and port: either another
    /// fingerprint of the same algorithm, or a key of another algorithm while
    /// none of the presented one is known. Treat this as a possible
    /// man-in-the-middle attack.
    Mismatch {
        /// The algorithm of the key on record, which differs from the presented
        /// one when the host has switched key types.
        stored_algorithm: String,
        /// The fingerprint currently on record.
        stored_fingerprint: String,
    },
}

/// One trusted host key record.
#[derive(Debug, Clone)]
struct HostKey {
    /// Host name as typed by the user, kept for round-tripping the file.
    host: String,
    /// TCP port the key was seen on.
    port: u16,
    /// Host key algorithm, e.g. `ssh-ed25519`.
    algorithm: String,
    /// Fingerprint of the key, e.g. `SHA256:...`.
    fingerprint: String,
}

impl HostKey {
    /// Whether this record describes the given host and port.
    fn matches_host(&self, host: &str, port: u16) -> bool {
        self.port == port && self.host.eq_ignore_ascii_case(host)
    }

    /// Whether this record describes the given host, port and algorithm.
    fn matches_key(&self, host: &str, port: u16, algorithm: &str) -> bool {
        self.matches_host(host, port) && self.algorithm == algorithm
    }
}

/// In-memory view of the `known_hosts` file.
#[derive(Debug, Clone, Default)]
pub struct KnownHosts {
    /// Records in file order.
    entries: Vec<HostKey>,
}

impl KnownHosts {
    /// Load the database from the default configuration location.
    ///
    /// A missing file yields an empty database.
    ///
    /// # Errors
    ///
    /// Fails when the configuration directory cannot be determined or the file
    /// cannot be read.
    pub fn load() -> Result<Self> {
        Self::load_from(&known_hosts_file()?)
    }

    /// Load the database from an explicit path.
    ///
    /// A missing file yields an empty database. Malformed lines are logged and
    /// skipped rather than failing the load, so one bad record cannot lock the
    /// user out of every saved host.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        // A byte order mark would otherwise glue itself to the first host name.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

        let mut entries = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_whitespace();
            let (Some(host), Some(port), Some(algorithm), Some(fingerprint)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                log::warn!(
                    "{}:{}: ignoring malformed known_hosts line",
                    path.display(),
                    number + 1
                );
                continue;
            };
            let Ok(port) = port.parse::<u16>() else {
                log::warn!(
                    "{}:{}: ignoring known_hosts line with invalid port {port:?}",
                    path.display(),
                    number + 1
                );
                continue;
            };
            entries.push(HostKey {
                host: host.to_string(),
                port,
                algorithm: algorithm.to_string(),
                fingerprint: fingerprint.to_string(),
            });
        }
        Ok(Self { entries })
    }

    /// Write the database to the default configuration location.
    ///
    /// # Errors
    ///
    /// Fails when the configuration directory cannot be determined or created,
    /// or when the file cannot be written.
    pub fn save(&self) -> Result<()> {
        self.save_to(&known_hosts_file()?)
    }

    /// Write the database to an explicit path, creating parent directories.
    ///
    /// The write is atomic: the data lands in a temporary sibling file that is
    /// then renamed over `path`.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created or the file cannot be
    /// written.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut text = String::from(FILE_HEADER);
        text.push('\n');
        for entry in &self.entries {
            let _ = writeln!(
                text,
                "{} {} {} {}",
                entry.host, entry.port, entry.algorithm, entry.fingerprint
            );
        }
        write_atomic(path, text.as_bytes())
    }

    /// Check a fingerprint presented by a server against the database.
    ///
    /// A record of the presented algorithm decides it. Without one, a record of
    /// any other algorithm for the host and port makes it a
    /// [`HostKeyStatus::Mismatch`]: an attacker who cannot forge the stored key
    /// would offer a key of another type, and trusting that on first sight would
    /// make the stored key worthless. [`HostKeyStatus::Unknown`] is left for a
    /// host and port with no record at all.
    pub fn status(
        &self,
        host: &str,
        port: u16,
        algorithm: &str,
        fingerprint: &str,
    ) -> HostKeyStatus {
        let record = self
            .entries
            .iter()
            .find(|entry| entry.matches_key(host, port, algorithm))
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|entry| entry.matches_host(host, port))
            });
        match record {
            None => HostKeyStatus::Unknown,
            Some(entry) if entry.algorithm == algorithm && entry.fingerprint == fingerprint => {
                HostKeyStatus::Trusted
            }
            Some(entry) => HostKeyStatus::Mismatch {
                stored_algorithm: entry.algorithm.clone(),
                stored_fingerprint: entry.fingerprint.clone(),
            },
        }
    }

    /// Trust `fingerprint` for the given host, port and algorithm.
    ///
    /// An existing record for the same host, port and algorithm is replaced in
    /// place, which is how a user accepts a rotated host key.
    pub fn trust(&mut self, host: &str, port: u16, algorithm: &str, fingerprint: &str) {
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.matches_key(host, port, algorithm))
        {
            Some(entry) => {
                entry.host = host.to_string();
                entry.fingerprint = fingerprint.to_string();
            }
            None => self.entries.push(HostKey {
                host: host.to_string(),
                port,
                algorithm: algorithm.to_string(),
                fingerprint: fingerprint.to_string(),
            }),
        }
    }

    /// Drop every trusted key for the given host and port, regardless of
    /// algorithm.
    pub fn forget(&mut self, host: &str, port: u16) {
        self.entries.retain(|entry| !entry.matches_host(host, port));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519: &str = "ssh-ed25519";
    const FP_A: &str = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const FP_B: &str = "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn unknown_then_trusted() {
        let mut hosts = KnownHosts::default();
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Unknown
        );

        hosts.trust("example.com", 22, ED25519, FP_A);
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn different_fingerprint_is_a_mismatch() {
        let mut hosts = KnownHosts::default();
        hosts.trust("example.com", 22, ED25519, FP_A);
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_B),
            HostKeyStatus::Mismatch {
                stored_algorithm: ED25519.to_string(),
                stored_fingerprint: FP_A.to_string(),
            }
        );
    }

    #[test]
    fn a_key_of_another_algorithm_is_a_mismatch_not_a_new_host() {
        let mut hosts = KnownHosts::default();
        hosts.trust("example.com", 22, ED25519, FP_A);
        assert_eq!(
            hosts.status("EXAMPLE.com", 22, "rsa-sha2-512", FP_B),
            HostKeyStatus::Mismatch {
                stored_algorithm: ED25519.to_string(),
                stored_fingerprint: FP_A.to_string(),
            },
            "a host known under one key type must not be trusted afresh under another"
        );
        assert_eq!(
            hosts.status("example.com", 2222, "rsa-sha2-512", FP_B),
            HostKeyStatus::Unknown,
            "another port is another host"
        );

        hosts.forget("example.com", 22);
        assert_eq!(
            hosts.status("example.com", 22, "rsa-sha2-512", FP_B),
            HostKeyStatus::Unknown,
            "forgetting the host is how a deliberate key type change is accepted"
        );
    }

    #[test]
    fn host_comparison_ignores_case_but_not_port() {
        let mut hosts = KnownHosts::default();
        hosts.trust("Example.COM", 22, ED25519, FP_A);
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            hosts.status("EXAMPLE.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            hosts.status("example.com", 2222, ED25519, FP_A),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn trust_replaces_the_existing_record() {
        let mut hosts = KnownHosts::default();
        hosts.trust("example.com", 22, ED25519, FP_A);
        hosts.trust("example.com", 22, ED25519, FP_B);
        assert_eq!(hosts.entries.len(), 1);
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_B),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn different_algorithms_coexist() {
        let mut hosts = KnownHosts::default();
        hosts.trust("example.com", 22, ED25519, FP_A);
        hosts.trust("example.com", 22, "rsa-sha2-512", FP_B);
        assert_eq!(hosts.entries.len(), 2);
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            hosts.status("example.com", 22, "rsa-sha2-512", FP_B),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn forget_drops_every_algorithm_for_the_host() {
        let mut hosts = KnownHosts::default();
        hosts.trust("example.com", 22, ED25519, FP_A);
        hosts.trust("example.com", 22, "rsa-sha2-512", FP_B);
        hosts.trust("other.net", 22, ED25519, FP_A);

        hosts.forget("EXAMPLE.com", 22);

        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            hosts.status("example.com", 22, "rsa-sha2-512", FP_B),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            hosts.status("other.net", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn save_to_load_from_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cfg").join("known_hosts");

        let mut hosts = KnownHosts::default();
        hosts.trust("example.com", 22, ED25519, FP_A);
        hosts.trust("10.0.0.1", 2222, "rsa-sha2-512", FP_B);
        hosts.save_to(&path).expect("save");

        let loaded = KnownHosts::load_from(&path).expect("load");
        assert_eq!(
            loaded.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            loaded.status("10.0.0.1", 2222, "rsa-sha2-512", FP_B),
            HostKeyStatus::Trusted
        );

        // Saving again over an existing file must work.
        loaded.save_to(&path).expect("overwrite");
        assert_eq!(
            KnownHosts::load_from(&path).expect("reload").entries.len(),
            2
        );
    }

    #[test]
    fn parsing_tolerates_a_utf8_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        // Exactly what a Windows editor writes when it saves as "UTF-8 with BOM".
        fs::write(&path, format!("\u{feff}example.com 22 {ED25519} {FP_A}\n")).expect("write");

        let hosts = KnownHosts::load_from(&path).expect("load");
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted,
            "a byte order mark must not be glued to the first host name"
        );
    }

    #[test]
    fn parsing_skips_comments_blank_and_malformed_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        let text = format!(
            "# a comment\n\
             \n\
             \t  \n\
             example.com 22 {ED25519} {FP_A}\n\
             # another comment\n\
             broken.example 22 missing-fingerprint\n\
             bad-port.example notanumber {ED25519} {FP_B}\n\
             \tindented.example  2222   {ED25519}  {FP_B}  \n"
        );
        fs::write(&path, text).expect("write");

        let hosts = KnownHosts::load_from(&path).expect("load");
        assert_eq!(hosts.entries.len(), 2);
        assert_eq!(
            hosts.status("example.com", 22, ED25519, FP_A),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            hosts.status("indented.example", 2222, ED25519, FP_B),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            hosts.status("broken.example", 22, ED25519, FP_A),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            hosts.status("bad-port.example", 22, ED25519, FP_B),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn load_from_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = KnownHosts::load_from(&dir.path().join("absent")).expect("load");
        assert!(hosts.entries.is_empty());
    }
}
