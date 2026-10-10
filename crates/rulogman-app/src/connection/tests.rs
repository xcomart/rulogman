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
fn agent_authentication_needs_no_secret_or_key_file() {
    for stored in [None, Some("an obsolete password".to_owned())] {
        let decision = decide_credentials(&AuthMethod::Agent, stored, never_probed);
        assert!(matches!(decision, Credentials::Ready(SshAuth::Agent)));
    }
}

#[test]
fn a_saved_agent_profile_ignores_a_legacy_remember_secret_flag() {
    let mut profile = SessionProfile::new("agent", "example.com", 22, "alice", AuthMethod::Agent);
    profile.save_secret = true;
    assert!(matches!(saved_credentials(&profile), Some(SshAuth::Agent)));
}

#[gpui::test]
fn selecting_agent_enables_a_complete_form_and_discards_typed_secrets(
    cx: &mut gpui::TestAppContext,
) {
    let dialog = cx.new(ConnectionDialog::new);
    dialog.update(cx, |dialog, cx| {
        dialog
            .host_input
            .update(cx, |input, cx| input.set_content("example.com", cx));
        dialog
            .username_input
            .update(cx, |input, cx| input.set_content("alice", cx));
        dialog
            .password_input
            .update(cx, |input, cx| input.set_content("old password", cx));
        dialog
            .passphrase_input
            .update(cx, |input, cx| input.set_content("old passphrase", cx));
        dialog.set_auth_kind(AuthKind::Agent, cx);
        assert!(dialog.can_connect(cx));
        assert!(dialog.status.is_none());
        assert!(dialog.password_input.read(cx).content().is_empty());
        assert!(dialog.passphrase_input.read(cx).content().is_empty());
        assert!(dialog.key_path_input.read(cx).content().is_empty());

        dialog.host_input.update(cx, |input, cx| input.clear(cx));
        assert!(
            !dialog.can_connect(cx),
            "agent authentication still needs a host"
        );
        dialog.explain_incomplete(cx);
        assert_eq!(
            dialog.status.as_ref().unwrap().lines,
            vec![ts!("connection.need_host")]
        );
    });
}

#[gpui::test]
fn editing_an_agent_profile_preserves_agent_authentication_on_its_hops(
    cx: &mut gpui::TestAppContext,
) {
    let mut profile = SessionProfile::new("agent", "example.com", 22, "alice", AuthMethod::Agent);
    profile.hops.push(HopRule {
        id: Uuid::new_v4(),
        host: "bastion".to_owned(),
        port: 22,
        username: "jumper".to_owned(),
        auth: AuthMethod::Agent,
        save_secret: false,
    });
    let dialog = cx.new(ConnectionDialog::new);
    dialog.update(cx, |dialog, cx| {
        dialog.fill_form(&profile, cx);
        assert_eq!(dialog.auth_kind, AuthKind::Agent);
        assert!(dialog.can_connect(cx));
        assert_eq!(dialog.hop_rules(cx).unwrap(), profile.hops);
        assert!(dialog.hop_secrets(cx).is_empty());

        dialog.set_hop_auth_kind(0, AuthKind::Password, cx);
        dialog.hop_rows[0]
            .secret
            .update(cx, |input, cx| input.set_content("old hop password", cx));
        dialog.set_hop_auth_kind(0, AuthKind::Agent, cx);
        assert!(dialog.hop_rows[0].secret.read(cx).content().is_empty());
        assert_eq!(dialog.hop_rules(cx).unwrap(), profile.hops);
    });
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

/// A jump-host row, as its inputs would be read.
fn hop_typed(host: &str, port: &str, username: &str) -> HopFields {
    HopFields {
        id: Uuid::new_v4(),
        host: host.to_owned(),
        port: port.to_owned(),
        username: username.to_owned(),
        auth: AuthKind::Password,
        key_path: String::new(),
        save_secret: false,
    }
}

#[test]
fn a_finished_hop_row_becomes_a_rule() {
    let row = hop_typed("bastion.example.com", "2222", "alice");
    let id = row.id;
    let rules = collect_hop_rules(&[row]).expect("the row is complete");
    assert_eq!(
        rules,
        vec![HopRule {
            id,
            host: "bastion.example.com".to_owned(),
            port: 2222,
            username: "alice".to_owned(),
            auth: AuthMethod::Password,
            save_secret: false,
        }]
    );
}

#[test]
fn a_hop_row_with_no_port_takes_the_ssh_default() {
    // The one field of a hop that means something while empty: a bastion
    // on 22 is the overwhelming majority of them.
    let rules = collect_hop_rules(&[hop_typed("bastion", "", "alice")])
        .expect("an empty port is not an omission");
    assert_eq!(rules[0].port, DEFAULT_PORT);
}

#[test]
fn untouched_hop_rows_are_dropped_without_complaint() {
    // The section always ends with the empty row "Add jump host" produced,
    // so an untouched one must not stop the connection.
    let rows = [hop_typed("bastion", "22", "alice"), hop_typed("", "", "")];
    let rules = collect_hop_rules(&rows).expect("the blank row is ignored");
    assert_eq!(rules.len(), 1);
}

#[test]
fn a_half_written_hop_row_is_refused() {
    // A hop that cannot be authenticated fails the whole connection, not
    // just itself, so neither half of a login may be missing.
    assert!(collect_hop_rules(&[hop_typed("bastion", "22", "")]).is_none());
    assert!(collect_hop_rules(&[hop_typed("", "22", "alice")]).is_none());
}

#[test]
fn a_hop_port_out_of_range_is_refused() {
    assert!(collect_hop_rules(&[hop_typed("bastion", "0", "alice")]).is_none());
    assert!(collect_hop_rules(&[hop_typed("bastion", "65536", "alice")]).is_none());
}

#[test]
fn a_key_hop_needs_a_key_file() {
    let mut row = hop_typed("bastion", "22", "alice");
    row.auth = AuthKind::PrivateKey;
    assert!(collect_hop_rules(std::slice::from_ref(&row)).is_none());

    row.key_path = "/home/alice/.ssh/id_ed25519".to_owned();
    let rules = collect_hop_rules(&[row]).expect("the row is complete");
    assert_eq!(
        rules[0].auth,
        AuthMethod::PublicKey {
            key_path: PathBuf::from("/home/alice/.ssh/id_ed25519"),
        }
    );
}

#[test]
fn an_agent_hop_keeps_its_method_without_a_key_file_or_remembered_secret() {
    let mut row = hop_typed("bastion", "22", "alice");
    row.auth = AuthKind::Agent;
    row.save_secret = true;
    let rules = collect_hop_rules(&[row]).expect("an agent hop is complete without a key file");
    assert_eq!(rules[0].auth, AuthMethod::Agent);
    assert!(!rules[0].save_secret);
}

#[test]
fn a_hop_keeps_its_id_and_its_stored_secret_flag() {
    // Both are what tie the rule to the keychain entry the row is editing:
    // a new id would abandon the secret, and a cleared flag would tell the
    // session there is none to look up.
    let mut row = hop_typed("bastion", "22", "alice");
    row.save_secret = true;
    let id = row.id;
    let rules = collect_hop_rules(&[row]).expect("the row is complete");
    assert_eq!(rules[0].id, id);
    assert!(rules[0].save_secret);
}

/// A followed-file row naming `path` and inheriting its colours.
fn tail_typed(path: &str) -> TailFields {
    TailFields {
        path: path.to_owned(),
        highlights: None,
    }
}

/// One usable highlight rule row, coloured `foreground`.
fn highlight_typed(pattern: &str, foreground: &str) -> HighlightRuleFields {
    HighlightRuleFields {
        pattern: pattern.to_owned(),
        foreground: foreground.to_owned(),
        ..HighlightRuleFields::default()
    }
}

#[test]
fn followed_files_keep_their_order_and_drop_the_blanks() {
    let rows = [
        tail_typed("/var/log/nginx/access.log"),
        TailFields::default(),
        tail_typed("/var/log/syslog"),
    ];
    let rules = collect_tail_rules(&rows).expect("every row is usable");
    assert_eq!(
        rules,
        vec![
            TailRule::new("/var/log/nginx/access.log"),
            TailRule::new("/var/log/syslog"),
        ]
    );
}

#[test]
fn a_section_of_untouched_file_rows_follows_nothing() {
    let rows = [TailFields::default(), TailFields::default()];
    assert_eq!(collect_tail_rules(&rows), Some(Vec::new()));
}

#[test]
fn a_file_with_no_tick_inherits_rather_than_carrying_an_empty_list() {
    // What every profile written before highlighting existed says, and what
    // the great majority will keep saying: nothing at all.
    let rules = collect_tail_rules(&[tail_typed("/var/log/syslog")]).expect("usable");
    assert_eq!(rules[0].highlights, None);
}

#[test]
fn a_ticked_file_carries_exactly_the_rules_it_was_given() {
    let mut row = tail_typed("/var/log/syslog");
    row.highlights = Some(vec![highlight_typed(r"\bOOM\b", "bright_red")]);
    let rules = collect_tail_rules(&[row]).expect("usable");
    let carried = rules[0].highlights.as_ref().expect("the override is kept");
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].pattern, r"\bOOM\b");
    assert_eq!(carried[0].foreground.as_deref(), Some("bright_red"));
}

#[test]
fn a_ticked_file_with_no_usable_rows_turns_highlighting_off_for_itself() {
    // Not the same as an unticked row: the user cleared the rules on one
    // relentlessly noisy log, and `effective_highlights` reads the empty
    // list as "colour nothing here" rather than as "never configured".
    let mut row = tail_typed("/var/log/syslog");
    row.highlights = Some(vec![HighlightRuleFields::default()]);
    let rules = collect_tail_rules(&[row]).expect("an empty override is a decision");
    assert_eq!(rules[0].highlights, Some(Vec::new()));
}

#[test]
fn a_rule_that_cannot_be_used_refuses_the_whole_form() {
    // Both halves of what `collect_highlight_rules` refuses reach the
    // dialog as one answer: the session does not open.
    let mut bad_pattern = tail_typed("/var/log/syslog");
    bad_pattern.highlights = Some(vec![highlight_typed("(unclosed", "red")]);
    assert_eq!(collect_tail_rules(&[bad_pattern]), None);

    let mut bad_colour = tail_typed("/var/log/syslog");
    bad_colour.highlights = Some(vec![highlight_typed("boom", "reddish")]);
    assert_eq!(collect_tail_rules(&[bad_colour]), None);
}

#[test]
fn a_broken_rule_on_a_row_that_names_no_file_is_dropped_with_the_row() {
    // There is no file for it to colour, so there is nothing to refuse —
    // and refusing would strand the user on an empty row they never filled
    // in, with no path to point at.
    let row = TailFields {
        highlights: Some(vec![highlight_typed("(unclosed", "red")]),
        ..TailFields::default()
    };
    assert_eq!(collect_tail_rules(&[row]), Some(Vec::new()));
}

#[test]
fn a_followed_file_row_stays_inside_the_indices_it_was_given() {
    // A row's block has to hold its path, its tick and the whole span its
    // rule list numbers inside, and still end below the next row's base —
    // otherwise a file's rules would tab into the file under it.
    const LAST: isize = tab::TAIL_HIGHLIGHTS + crate::highlight_rules::TAB_SPAN;
    const { assert!(tab::TAIL_CUSTOM < tab::TAIL_HIGHLIGHTS) };
    const { assert!(LAST <= tab::TAIL_ROW_STRIDE) };
    const { assert!(tab::TAIL_ROWS + tab::TAIL_ROW_STRIDE <= tab::TAIL_ADD) };
    const { assert!(tab::TAIL_ADD < tab::CANCEL) };
    const { assert!(tab::CANCEL < tab::CONNECT) };
}

#[test]
fn every_word_the_followed_file_section_asks_for_has_a_translation() {
    for key in [
        "connection.tails.custom_highlights",
        "connection.tails.incomplete",
    ] {
        let label = ts!(key);
        assert!(!label.is_empty(), "{key} is empty");
        assert!(
            !label.contains("connection."),
            "untranslated {key}: {label:?}"
        );
    }
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
