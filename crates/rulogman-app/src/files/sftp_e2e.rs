//! [`SftpSource`]'s elevated write, against a real `sshd` and a real `sudo`.
//!
//! Everything else about that source is a forwarding shim, and the SFTP layer
//! tests its own half against a server standing inside the test process. The
//! `sudo` half cannot be tested that way, and the reason is the point of this
//! file: what [`FileSource::root_access`] asks about is not the *protocol* but
//! the *account* — three exit statuses from three programs, a group list, a
//! password read off a command's standard input, and a `tee` that has to leave
//! a file's owner and mode where it found them. A fake server can be made to
//! answer any of that, and would then be testing the fake.
//!
//! So each test here starts a container running Ubuntu's `openssh-server` with
//! four accounts in it, each configured to be one of the four answers the probe
//! can arrive at:
//!
//! * `alice` is in the `sudo` group and has a password —
//!   [`RootAccess::NeedsPassword`], the whole life of which is one test;
//! * `bob` has a `NOPASSWD` rule and no administrative group —
//!   [`RootAccess::Granted`];
//! * `carol` has neither — [`RootAccess::None`] by way of the third gate;
//! * `dave` lives in a second image with no `sudo` binary at all —
//!   [`RootAccess::None`] by way of the first.
//!
//! Every test is `#[ignore]`d, exactly as the WSL tests beside them are and for
//! the same reason: the machine either has the thing or it does not, and a
//! developer without Docker is not failing the suite for it. Run them with
//! `cargo test -p rulogman-app --bin rulogman -- --ignored sftp_e2e`.
//!
//! **Nothing here is cleaned up on the remote side, and nothing needs to be.**
//! Every file these tests write lives inside a container started by the test
//! and destroyed by [`Container`]'s [`Drop`] — including the root-owned ones,
//! which is precisely the sort of leftover a test that shared a host would have
//! to become root again to remove.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::BackgroundExecutor;
use rulogman_ssh::{
    ExecClient, HostKeyVerifier, SshAuth, SshConfig, SshEvent, SshSession, fingerprint,
};
use russh::keys::PublicKey;

use super::{FileSource, RootAccess, SftpSource};

// ---------------------------------------------------------------------------
// The images
// ---------------------------------------------------------------------------

/// The fixture every test but one runs against.
///
/// A raw string, so the `\n` in each `printf` reaches the shell rather than
/// being turned into a newline by the Rust compiler, and fed to `docker build`
/// on standard input so that this file is the whole of the fixture — there is
/// no build context, and nothing is added to the repository that a reader would
/// have to go and find.
///
/// The three accounts differ in exactly the one way the probe asks about:
///
/// * `alice` is in `sudo`, which is Debian's administrative group and one of
///   the four names [`ADMIN_GROUPS`](super::ADMIN_GROUPS) knows;
/// * `bob` is in no such group but has a `NOPASSWD` rule, which is the case the
///   group check would get wrong on its own and the passwordless gate catches
///   first;
/// * `carol` has neither, and is the account the probe must offer nothing to.
///
/// `/etc/rulogman-test/owned.conf` is the file the feature exists for: root's,
/// readable by everyone, writable by nobody else. `ssh-keygen -A` is spelled
/// out rather than left to the package's own postinst, because a host without
/// keys refuses every connection and the failure would look like a client bug.
const FIXTURE_DOCKERFILE: &str = r#"FROM ubuntu:24.04
RUN set -eu; \
    apt-get update; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server sudo; \
    rm -rf /var/lib/apt/lists/*; \
    mkdir -p /run/sshd /etc/ssh/sshd_config.d; \
    ssh-keygen -A; \
    printf 'PasswordAuthentication yes\nPermitRootLogin no\n' > /etc/ssh/sshd_config.d/rulogman-test.conf; \
    useradd -m -s /bin/bash alice; echo 'alice:alice-pw' | chpasswd; usermod -aG sudo alice; \
    useradd -m -s /bin/bash bob; echo 'bob:bob-pw' | chpasswd; \
    printf 'bob ALL=(ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/bob; chmod 440 /etc/sudoers.d/bob; \
    useradd -m -s /bin/bash carol; echo 'carol:carol-pw' | chpasswd; \
    mkdir /etc/rulogman-test; chmod 755 /etc/rulogman-test; \
    printf 'the original line\n' > /etc/rulogman-test/owned.conf; \
    chmod 644 /etc/rulogman-test/owned.conf
CMD ["/usr/sbin/sshd", "-D", "-e"]
"#;

/// The second image, for the one account whose `sudo` is missing rather than
/// unhelpful.
///
/// `ubuntu:24.04` ships without `sudo`, so the `apt-get remove` is a no-op on
/// today's base image and insurance against a future one that changes its mind;
/// apt exits successfully for a package it knows and has not installed. The
/// `! command -v sudo` at the end is what makes the insurance worth having: if
/// `sudo` ever *is* present, the image fails to build and says so, rather than
/// quietly turning this into a second copy of the `carol` test.
const NO_SUDO_DOCKERFILE: &str = r#"FROM ubuntu:24.04
RUN set -eu; \
    apt-get update; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server; \
    apt-get remove -y sudo; \
    rm -rf /var/lib/apt/lists/*; \
    mkdir -p /run/sshd /etc/ssh/sshd_config.d; \
    ssh-keygen -A; \
    printf 'PasswordAuthentication yes\nPermitRootLogin no\n' > /etc/ssh/sshd_config.d/rulogman-test.conf; \
    useradd -m -s /bin/bash dave; echo 'dave:dave-pw' | chpasswd; \
    ! command -v sudo
CMD ["/usr/sbin/sshd", "-D", "-e"]
"#;

/// The root-owned directory the elevated writes land in.
const ROOT_DIR: &str = "/etc/rulogman-test";

/// The root-owned file that is already there when a container starts.
const ROOT_FILE: &str = "/etc/rulogman-test/owned.conf";

/// What [`ROOT_FILE`] holds before any test has written to it.
const ORIGINAL_BODY: &str = "the original line";

/// Built once per test binary, however many tests ask for it.
static FIXTURE_IMAGE: OnceLock<String> = OnceLock::new();

/// The same, for [`NO_SUDO_DOCKERFILE`].
static NO_SUDO_IMAGE: OnceLock<String> = OnceLock::new();

/// How long a container is given to start `sshd` and answer with its banner.
///
/// Generous because a cold Docker Desktop is: the daemon may still be starting
/// the VM the container runs in, and a wait that gave up first would fail a
/// perfectly healthy machine.
const SSHD_TIMEOUT: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------------------
// Docker
// ---------------------------------------------------------------------------

/// A `docker` invocation, with a console window suppressed on Windows.
///
/// `CREATE_NO_WINDOW` for the reason `wsl_fs` uses it: rulogman is a GUI
/// application, and a child process started without the flag flashes a console
/// on screen. It costs nothing in a test and keeps every process this tree
/// starts obeying one rule.
fn docker(arguments: &[&str]) -> Command {
    let mut command = Command::new("docker");
    command.args(arguments);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::wsl::CREATE_NO_WINDOW);
    }
    command
}

/// Runs one `docker` command to completion and answers its standard output.
///
/// `stdin`, when given, is written and the input closed — which is how the two
/// Dockerfiles reach `docker build -` without a context directory. A non-zero
/// status panics with the command line and everything the daemon said, because
/// there is no test here that can carry on without it.
fn docker_output(arguments: &[&str], stdin: Option<&str>) -> String {
    let mut child = docker(arguments)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker must be installed and on the PATH");

    if let Some(text) = stdin {
        let mut input = child.stdin.take().expect("the input was asked to be piped");
        input
            .write_all(text.as_bytes())
            .expect("docker must accept the Dockerfile on its standard input");
    }

    let output = child
        .wait_with_output()
        .expect("docker must run to completion");
    assert!(
        output.status.success(),
        "docker {} failed ({}): {}",
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The tag `dockerfile` is built under, named after its own contents.
///
/// A fixed name would be simpler and would go stale: whoever edits one of the
/// Dockerfiles above would keep running the image built from the version before
/// their edit, and would have to know to delete it. Six bytes of a SHA-256 of
/// the text is not a fixed name — an edited Dockerfile asks for a tag that is
/// not there yet, and is therefore built.
fn tag(dockerfile: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, dockerfile.as_bytes());
    let short = digest
        .as_ref()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("rulogman-sftp-test:{short}")
}

/// The image `dockerfile` describes, built at most once per run and reused
/// across runs.
///
/// Tagged rather than built anonymously, and skipped entirely when the tag is
/// already there, because an untagged build is not free the way the layer cache
/// makes it look: every `docker build` mints a fresh image, and an anonymous
/// one immediately becomes a dangling 130MB nobody will ever collect. The
/// second and later runs of this suite now build nothing at all.
fn image(cell: &'static OnceLock<String>, dockerfile: &str) -> String {
    cell.get_or_init(|| {
        let tag = tag(dockerfile);
        let known = docker(&["image", "inspect", &tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("docker must be installed and on the PATH")
            .success();
        if !known {
            docker_output(&["build", "-q", "-t", &tag, "-"], Some(dockerfile));
        }
        tag
    })
    .clone()
}

/// One running container, and the promise that it will not outlive the test.
struct Container {
    /// The `--name` it was started under, unique to this process and test.
    name: String,
    /// The host port Docker published its port 22 on.
    port: u16,
}

impl Container {
    /// Starts `image` and waits until `sshd` inside it answers.
    ///
    /// The name carries this process's id the way the WSL tests' scratch
    /// directory does, so that two runs of the suite at once — or a leftover
    /// from a crashed one — cannot collide. The *port* is not derived that way
    /// and is asked of Docker instead: `-p 127.0.0.1:0:22` publishes on a port
    /// the operating system says is free, which is the only way to be sure of
    /// it. A number computed from the process id is merely unlikely to clash,
    /// and the failure when it does looks like a broken test.
    fn start(image: &str, seed: u32) -> Self {
        let name = format!("rulogman-sftp-test-{}-{seed}", std::process::id());
        docker_output(
            &[
                "run",
                "-d",
                "--rm",
                "-p",
                "127.0.0.1:0:22",
                "--name",
                &name,
                image,
            ],
            None,
        );

        // Constructed before the wait, so that a container which never comes up
        // is still torn down by the panic that follows.
        let mut container = Self { name, port: 0 };
        container.port = container.published_port();
        container.wait_for_sshd();
        container
    }

    /// The host port `docker run` chose for the container's port 22.
    fn published_port(&self) -> u16 {
        let mapping = docker_output(&["port", &self.name, "22"], None);
        let published = mapping
            .lines()
            .next()
            .and_then(|line| line.rsplit(':').next())
            .and_then(|port| port.parse().ok());
        published.unwrap_or_else(|| panic!("docker port answered {mapping:?}, not an address"))
    }

    /// Blocks until the container answers with an SSH banner.
    ///
    /// A TCP connection is not enough on its own: Docker's port forwarding
    /// starts accepting the moment the container does, which is before `sshd`
    /// inside it has bound anything. Reading the four bytes of the version
    /// string is the cheapest thing that only a running server can do.
    fn wait_for_sshd(&self) {
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let deadline = Instant::now() + SSHD_TIMEOUT;
        loop {
            if let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut banner = [0u8; 4];
                if stream.read_exact(&mut banner).is_ok() && &banner == b"SSH-" {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} never answered on 127.0.0.1:{} within {SSHD_TIMEOUT:?}; its log says: {}",
                self.name,
                self.port,
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Everything the container has written to either output stream.
    ///
    /// Only ever read into a panic message: `sshd -D -e` explains a refused
    /// login there, and a test that fails without it leaves nothing to go on.
    fn logs(&self) -> String {
        let output = docker(&["logs", &self.name])
            .output()
            .expect("docker logs must run");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

impl Drop for Container {
    /// Removes the container, whether the test passed, failed or panicked.
    ///
    /// `--rm` alone would not do it: it covers a container that *stops*, and a
    /// panicking test leaves one running. The status is ignored because there
    /// is nothing to be done about a failure here and reporting one from a drop
    /// during a panic would replace the real diagnosis with this one.
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// Trusts the host key of a container this test started a second ago.
///
/// Accepting everything is the *correct* policy here, and the only one that
/// could work: the container generated its host key during the image build,
/// nothing has ever seen it before, and there is no `known_hosts` for it to be
/// in. What host key verification defends against is a machine in the middle of
/// a connection to a host whose key you already know — and here both ends of
/// the connection are this test, on the loopback interface, to a server whose
/// entire lifetime is this test's.
struct TrustTheContainer;

#[async_trait::async_trait]
impl HostKeyVerifier for TrustTheContainer {
    async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool {
        log::debug!(
            "trusting the test container's host key for {host}:{port} ({})",
            fingerprint(key)
        );
        true
    }
}

/// Logs into the container as `user` and answers once the session is ready.
///
/// Readiness is waited for rather than left to the queuing both clients do,
/// even though a queued request would be served just as well: a wrong password
/// or a server that refused the shell arrives on this stream as one event and
/// on a queued request as a timeout with nothing in it. Waiting here means a
/// broken fixture is reported as "the session ended: Error(Auth, …)" instead of
/// as a file operation that hung.
///
/// The receiver is handed back rather than dropped, and callers keep it alive:
/// dropping it ends the session, which would take the SFTP and exec clients
/// with it.
async fn connect(
    container: &Container,
    user: &str,
    password: &str,
) -> (SshSession, UnboundedReceiver<SshEvent>) {
    let mut config = SshConfig::new(
        "127.0.0.1",
        container.port,
        user,
        SshAuth::Password(password.to_owned()),
    );
    // Generous, because a cold container is: the handshake itself is quick, but
    // the machinery underneath it may not be warm yet.
    config.connect_timeout_secs = 30;

    let (session, mut events) = SshSession::connect(config, Arc::new(TrustTheContainer));
    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        if matches!(event, SshEvent::Ready) {
            return (session, events);
        }
        assert!(
            !matches!(event, SshEvent::Error(_, _) | SshEvent::Disconnected { .. }),
            "the session for {user} ended before it was ready: {event:?}; \
             events so far: {seen:?}; the container's log says: {}",
            container.logs()
        );
        seen.push(format!("{event:?}"));
    }
    panic!(
        "the event stream for {user} ended before the session was ready; \
         events so far: {seen:?}; the container's log says: {}",
        container.logs()
    );
}

/// The source under test, built exactly as the application builds one.
///
/// A function rather than a variable because several tests build a *second*
/// one on the same session: a source that has probed nothing and remembers no
/// password is what the editor gets on every fresh pane, and it is the only way
/// to reach the branch where the caller has to supply the password itself.
fn file_source(session: &SshSession) -> SftpSource {
    SftpSource::new(session.sftp(), session.exec())
}

/// Runs `command` on the container and answers its trimmed standard output.
///
/// The tests' way of looking at the far side without going through the code
/// under test: `stat` says who owns a file and what its mode is, which is the
/// half of the elevated write's contract that reading the file back cannot
/// show. A non-zero status panics, because every command asked here is one the
/// account is entitled to run.
async fn ask(commands: &ExecClient, command: &str) -> String {
    let output = commands
        .run(command.to_owned(), Vec::new())
        .await
        .unwrap_or_else(|error| panic!("{command} could not be run: {error}"));
    assert_eq!(
        output.exit_status,
        Some(0),
        "{command} did not succeed; it wrote {:?} to standard output and {:?} to standard error",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The owner and mode of `path` on the container, as `stat` spells them.
async fn owner_and_mode(commands: &ExecClient, path: &str) -> String {
    ask(commands, &format!("stat -c '%U %a' {path}")).await
}

/// Stages `body` under `name` in `directory`, and answers the path.
///
/// The same shape the editor's save takes: a file on this machine whose *name*
/// becomes the name on the far side, which is why every call here writes
/// `owned.conf` rather than something unique — the destination is decided by
/// this name and by nothing else.
fn stage(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, body.as_bytes()).expect("the staging file must be written");
    path
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The whole life of [`RootAccess::NeedsPassword`], against a real `sudo`.
///
/// One test rather than six because it is one story and the steps are not
/// independent: the probe has to say a password is wanted before the wrong one
/// can be refused, and the right one has to be remembered before a save with no
/// password in hand can land. Splitting it would mean starting a container per
/// step to rebuild the same state.
#[gpui::test]
#[ignore = "needs Docker on this machine"]
async fn an_administrative_account_pays_with_its_password_and_writes_as_root(
    executor: BackgroundExecutor,
) {
    // Every await below is answered by the session's own worker thread, so the
    // test executor has nothing of its own to run while one is in flight.
    executor.allow_parking();

    let container = Container::start(&image(&FIXTURE_IMAGE, FIXTURE_DOCKERFILE), 1);
    let (session, _events) = connect(&container, "alice", "alice-pw").await;
    let commands = session.exec();
    let source = file_source(&session);
    let here = tempfile::tempdir().expect("the staging directory must be created");

    // Stage one: the question the editor asks before it opens the pane, put to
    // a real OpenSSH for the first time. `owned.conf` is root's and 644, so the
    // server answers the write probe with SSH_FX_PERMISSION_DENIED.
    assert!(
        !source.writable(ROOT_FILE).await,
        "a root-owned file must not be reported writable to alice"
    );

    // Stage two: what the way out of that pane would cost. alice is in `sudo`
    // and `sudo -n` refuses her, which is the two gates that lead here.
    assert_eq!(source.root_access().await, RootAccess::NeedsPassword);

    // The fixture and the probe's restraint, corroborated out of band and only
    // now: the file is root's and 644, and asking whether it could be written
    // left it exactly as it was — the probe opens for writing without creating
    // and without truncating, which is the only reason it is safe to run on a
    // file the user has not touched yet.
    assert_eq!(owner_and_mode(&commands, ROOT_FILE).await, "root 644");
    assert_eq!(
        ask(&commands, &format!("cat {ROOT_FILE}")).await,
        ORIGINAL_BODY
    );

    // The wrong password is refused while the dialog is still on screen, which
    // is the reason `unlock_root` exists at all.
    let refusal = source
        .unlock_root(Some("not-alices-password"), false)
        .await
        .expect_err("sudo must refuse the wrong password");
    log::debug!("the container refused the wrong password with: {refusal}");
    // And the verdict is still the cached one: a refusal is an answer about the
    // password, not about the account.
    assert_eq!(source.root_access().await, RootAccess::NeedsPassword);

    source
        .unlock_root(Some("alice-pw"), true)
        .await
        .expect("sudo must accept alice's own password");

    // Stage three, the save: no password in hand, because the source was asked
    // to keep the one it was given.
    let staged = stage(here.path(), "owned.conf", "edited through the editor\n");
    let written = source
        .copy_in_as_root(staged, ROOT_DIR, None)
        .await
        .expect("the remembered password must carry the elevated write");
    assert_eq!(written, ROOT_FILE);
    assert_eq!(
        ask(&commands, &format!("cat {ROOT_FILE}")).await,
        "edited through the editor"
    );
    // The half that `cat` cannot show, and the reason the write is a `tee`
    // rather than a redirection: the file kept its identity across it. Editing
    // a root-owned file as root must not hand it to root a second time, and
    // must not widen its mode.
    assert_eq!(
        owner_and_mode(&commands, ROOT_FILE).await,
        "root 644",
        "the elevated write changed the file's owner or mode"
    );

    // A name that was not there before arrives owned by root, which is what
    // makes an elevated save of a *new* file useful rather than a file the
    // account could not write afterwards either.
    let staged = stage(here.path(), "fresh.conf", "a file only root could make\n");
    let created = source
        .copy_in_as_root(staged, ROOT_DIR, None)
        .await
        .expect("a new name must be writable as root too");
    assert_eq!(created, "/etc/rulogman-test/fresh.conf");
    assert_eq!(
        ask(&commands, &format!("cat {created}")).await,
        "a file only root could make"
    );
    assert_eq!(owner_and_mode(&commands, &created).await, "root 644");

    // The other path through the same call: a source that has remembered
    // nothing, which is what the editor holds when the user declined to have
    // the password kept and types it for every save.
    let fresh = file_source(&session);
    let staged = stage(here.path(), "owned.conf", "this must never land\n");
    fresh
        .copy_in_as_root(staged, ROOT_DIR, None)
        .await
        .expect_err("a source that remembers nothing has no password to use");

    let staged = stage(here.path(), "owned.conf", "typed in for this save\n");
    assert_eq!(
        fresh
            .copy_in_as_root(staged, ROOT_DIR, Some("alice-pw"))
            .await
            .expect("a password handed in must carry the write"),
        ROOT_FILE
    );
    assert_eq!(
        ask(&commands, &format!("cat {ROOT_FILE}")).await,
        "typed in for this save"
    );

    // And a wrong one refuses without touching the file. `sudo` never starts
    // `tee`, so there is nothing to half-write.
    let staged = stage(here.path(), "owned.conf", "nor must this\n");
    fresh
        .copy_in_as_root(staged, ROOT_DIR, Some("not-alices-password"))
        .await
        .expect_err("the wrong password must refuse the write");
    assert_eq!(
        ask(&commands, &format!("cat {ROOT_FILE}")).await,
        "typed in for this save",
        "a refused elevated write still changed the file"
    );
    assert_eq!(owner_and_mode(&commands, ROOT_FILE).await, "root 644");
}

/// An account with a `NOPASSWD` rule and no administrative group at all.
///
/// The case the group check would answer wrongly on its own, and the reason the
/// passwordless gate is asked first: bob is in no group named in
/// [`ADMIN_GROUPS`](super::ADMIN_GROUPS), so a probe that only looked at his
/// groups would offer him nothing while `sudo` stood ready to do anything he
/// asked.
#[gpui::test]
#[ignore = "needs Docker on this machine"]
async fn an_account_sudo_asks_nothing_of_needs_no_password_anywhere(executor: BackgroundExecutor) {
    executor.allow_parking();

    let container = Container::start(&image(&FIXTURE_IMAGE, FIXTURE_DOCKERFILE), 2);
    let (session, _events) = connect(&container, "bob", "bob-pw").await;
    let commands = session.exec();
    let source = file_source(&session);
    let here = tempfile::tempdir().expect("the staging directory must be created");

    assert_eq!(
        ask(&commands, "id -Gn").await,
        "bob",
        "bob must not be in an administrative group"
    );
    assert_eq!(source.root_access().await, RootAccess::Granted);

    // The press that unlocks the pane, with nothing to type into it.
    source
        .unlock_root(None, false)
        .await
        .expect("a NOPASSWD account must unlock with no password");

    let staged = stage(here.path(), "owned.conf", "written without a password\n");
    let written = source
        .copy_in_as_root(staged, ROOT_DIR, None)
        .await
        .expect("the elevated write must land with no password anywhere");
    assert_eq!(written, ROOT_FILE);
    assert_eq!(
        ask(&commands, &format!("cat {ROOT_FILE}")).await,
        "written without a password"
    );
    assert_eq!(owner_and_mode(&commands, ROOT_FILE).await, "root 644");
}

/// An account with a `sudo` that will not do anything for it.
///
/// carol is in no administrative group and has no `sudoers` rule, so the second
/// gate refuses her and the third finds nothing to recognise. The editor must
/// draw no way out of the read-only pane at all — an offer that led to a dialog
/// and then a refusal would be worse than no offer.
#[gpui::test]
#[ignore = "needs Docker on this machine"]
async fn an_account_with_no_sudo_rights_is_offered_nothing(executor: BackgroundExecutor) {
    executor.allow_parking();

    let container = Container::start(&image(&FIXTURE_IMAGE, FIXTURE_DOCKERFILE), 3);
    let (session, _events) = connect(&container, "carol", "carol-pw").await;
    let commands = session.exec();
    let source = file_source(&session);

    // The fixture, restated as an assertion: `sudo` is installed — so this is
    // the third gate answering and not the first — and carol's groups hold
    // nothing administrative.
    assert_eq!(
        ask(&commands, "command -v sudo").await,
        "/usr/bin/sudo",
        "the fixture must have sudo installed for this to be the right gate"
    );
    assert_eq!(ask(&commands, "id -Gn").await, "carol");

    assert!(!source.writable(ROOT_FILE).await);
    assert_eq!(source.root_access().await, RootAccess::None);
}

/// A host with no `sudo` on it at all.
///
/// The first gate, and the one that has to be an exit status rather than a
/// message: `command -v` writes nothing anybody could match on, and a probe
/// that read the shell's diagnostics would be reading them in the host's
/// locale.
#[gpui::test]
#[ignore = "needs Docker on this machine"]
async fn a_host_without_sudo_is_offered_nothing(executor: BackgroundExecutor) {
    executor.allow_parking();

    let container = Container::start(&image(&NO_SUDO_IMAGE, NO_SUDO_DOCKERFILE), 4);
    let (session, _events) = connect(&container, "dave", "dave-pw").await;
    let commands = session.exec();
    let source = file_source(&session);

    let probe = commands
        .run("command -v sudo >/dev/null 2>&1".to_owned(), Vec::new())
        .await
        .expect("the probe must run");
    assert_ne!(
        probe.exit_status,
        Some(0),
        "the fixture must not have sudo installed"
    );

    assert_eq!(source.root_access().await, RootAccess::None);
}

/// The ordinary road, still open.
///
/// The elevated write shares a session, a source and a `file_name` derivation
/// with the plain one, and the plain one is what every save that does not need
/// root goes down. This is the test that would notice the elevation work having
/// disturbed it — against a real OpenSSH rather than the SFTP layer's own
/// server, which is where a difference would show.
#[gpui::test]
#[ignore = "needs Docker on this machine"]
async fn an_ordinary_write_still_lands_as_the_account_itself(executor: BackgroundExecutor) {
    executor.allow_parking();

    let container = Container::start(&image(&FIXTURE_IMAGE, FIXTURE_DOCKERFILE), 5);
    let (session, _events) = connect(&container, "alice", "alice-pw").await;
    let commands = session.exec();
    let source = file_source(&session);
    let here = tempfile::tempdir().expect("the staging directory must be created");

    let home = source.home().await.expect("the server must report a home");
    assert_eq!(home, "/home/alice");

    let staged = stage(here.path(), "notes.txt", "alice wrote this herself\n");
    let written = source
        .copy_in(staged, &home, None)
        .await
        .expect("an ordinary upload must land");
    assert_eq!(written, "/home/alice/notes.txt");
    assert_eq!(
        ask(&commands, &format!("cat {written}")).await,
        "alice wrote this herself"
    );
    assert_eq!(
        ask(&commands, &format!("stat -c '%U' {written}")).await,
        "alice",
        "an ordinary upload must belong to the account that made it"
    );

    // And the probe answers the other way round for a file the account owns,
    // which is what keeps the read-only pane from appearing over her own files.
    assert!(
        source.writable(&written).await,
        "alice's own file must be reported writable"
    );

    // Removed through the source, because this is the one file these tests
    // create that the account itself could clean up — the root-owned ones die
    // with the container instead.
    source
        .remove_file(&written)
        .await
        .expect("the file must be removable");
}
