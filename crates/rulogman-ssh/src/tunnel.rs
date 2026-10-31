//! Local and dynamic (SOCKS) port forwarding on a live session's transport.
//!
//! Nothing here connects or authenticates: by the time [`open`] runs, the
//! session already has a transport, a pty and a shell, and a forwarding is
//! only ever an extra `direct-tcpip` channel on that same transport. That is
//! the whole difference from a dedicated tunnel client — the shell pays for
//! the connection, and every forwarded socket rides along on it.
//!
//! The tasks started here live and die with the session's runtime. When the
//! worker's `run` returns, the runtime is dropped, which drops the listeners
//! (closing the local ports) and every forwarding task with them; there is no
//! teardown handshake to get wrong.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::{Channel, ChannelOpenFailure, Error as RusshError};
use tokio::net::{TcpListener, TcpStream};

use crate::config::TunnelForward;
use crate::event::SshEvent;
use crate::session::emit;
use crate::socks;

/// How long a SOCKS client has to say where it wants to go.
///
/// A client that connects and then sends nothing would otherwise hold a task
/// and a descriptor for as long as the session lives.
const SOCKS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many `accept` calls may fail back to back before a rule gives up.
///
/// A single failure is normal — a client that connects and vanishes before the
/// accept completes shows up as `ECONNABORTED`, and a process at its descriptor
/// limit as `EMFILE`. A listener that fails *every* time cannot recover, and
/// looping on it would spin the session's thread, so the streak is capped.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 16;

/// Binds every rule and starts an accept loop for each one that binds.
///
/// Returns as soon as the listeners exist, so a session becomes usable without
/// waiting on anything but a few `bind` syscalls. Each rule that binds is
/// reported as [`SshEvent::TunnelOpened`]; a rule that cannot bind is reported
/// as [`SshEvent::TunnelFailed`] and skipped — the session keeps its shell,
/// exactly as `ssh -L` does when the local port is already taken.
pub(crate) async fn open<H>(
    handle: &Arc<Handle<H>>,
    rules: &[TunnelForward],
    events: &rulogman_stream::Sender<SshEvent>,
) where
    H: client::Handler + 'static,
{
    for rule in rules {
        let label = label(rule);
        let listener = match TcpListener::bind((rule.bind_address.as_str(), rule.local_port)).await
        {
            Ok(listener) => listener,
            Err(error) => {
                let message = format!(
                    "could not bind {}:{}: {error}",
                    rule.bind_address, rule.local_port
                );
                log::warn!("tunnel {label} was not opened: {message}");
                emit(
                    events,
                    SshEvent::TunnelFailed {
                        rule: label,
                        message,
                    },
                );
                continue;
            }
        };

        log::debug!(
            "tunnel {label} listening on {}:{}",
            rule.bind_address,
            rule.local_port
        );
        // Reported as its own event, not left to the log: the local port is now
        // held by *this* session and by no other, and the shell above has no
        // other way of learning which of several sessions on one profile won
        // the bind.
        emit(
            events,
            SshEvent::TunnelOpened {
                rule: label.clone(),
            },
        );
        // Spawned rather than awaited: the loop runs for as long as the session
        // does, and the session's own main loop has to start now.
        tokio::spawn(accept_loop(
            Arc::clone(handle),
            rule.clone(),
            label,
            listener,
            events.clone(),
        ));
    }
}

/// Names a rule the way the user wrote it, for logs and events.
///
/// `-L`'s own `port:host:port` for a local forwarding; a SOCKS proxy has no
/// remote end to name, so it is its port and what it is.
fn label(rule: &TunnelForward) -> String {
    if rule.dynamic {
        format!("{} (SOCKS)", rule.local_port)
    } else {
        format!(
            "{}:{}:{}",
            rule.local_port, rule.remote_host, rule.remote_port
        )
    }
}

/// Accepts local connections for one rule and forwards each of them.
///
/// Takes the listener by value, so returning from here closes the local port
/// whatever the reason for returning was. Deliberately watches nothing else:
/// keepalives, liveness and shutdown all belong to the session's main loop, and
/// this loop simply stops existing when that loop's runtime goes away.
async fn accept_loop<H>(
    handle: Arc<Handle<H>>,
    rule: TunnelForward,
    label: String,
    listener: TcpListener,
    events: rulogman_stream::Sender<SshEvent>,
) where
    H: client::Handler + 'static,
{
    let mut connections = 0u64;
    let mut accept_errors = 0u32;
    // Set once a SOCKS connection has been refused by the server's forwarding
    // policy and said so; see `socks_forward`.
    let refusal_reported = Arc::new(AtomicBool::new(false));

    loop {
        match listener.accept().await {
            Ok((socket, origin)) => {
                accept_errors = 0;
                connections += 1;
                log::debug!("tunnel {label} accepted connection {connections} from {origin}");

                // Every forwarded socket gets its own task and its own SSH
                // channel, which is what lets several clients use one rule at
                // once and why a connection that is closed — or refused by the
                // server — cannot disturb the others.
                if rule.dynamic {
                    tokio::spawn(socks_forward(
                        Arc::clone(&handle),
                        socket,
                        origin,
                        format!("{label} connection {connections}"),
                        Arc::clone(&refusal_reported),
                        events.clone(),
                    ));
                    continue;
                }
                tokio::spawn(forward(
                    Arc::clone(&handle),
                    rule.remote_host.clone(),
                    rule.remote_port,
                    socket,
                    origin,
                    format!("{label} connection {connections}"),
                    events.clone(),
                ));
            }
            Err(error) => {
                accept_errors += 1;
                log::warn!("tunnel {label} could not accept a connection: {error}");
                if accept_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    let message = format!(
                        "the local listener failed {accept_errors} times in a row: {error}"
                    );
                    log::warn!("tunnel {label} gave up: {message}");
                    emit(
                        &events,
                        SshEvent::TunnelFailed {
                            rule: label,
                            message,
                        },
                    );
                    return;
                }
            }
        }
    }
}

/// Forwards one accepted socket through the session and reports the outcome.
///
/// Never returns an error: a forwarded connection is a leaf, and everything it
/// can go wrong with is either an event of its own or nothing worth telling
/// anyone about.
#[allow(clippy::too_many_arguments)]
async fn forward<H>(
    handle: Arc<Handle<H>>,
    remote_host: String,
    remote_port: u16,
    socket: TcpStream,
    origin: SocketAddr,
    label: String,
    events: rulogman_stream::Sender<SshEvent>,
) where
    H: client::Handler + 'static,
{
    // Forwarded traffic is typically a long run of small request/response
    // pairs, so waiting to coalesce them costs a round trip apiece and saves
    // nothing.
    if let Err(error) = socket.set_nodelay(true) {
        log::debug!("could not disable Nagle's algorithm on {label}: {error}");
    }

    let channel = match handle
        .channel_open_direct_tcpip(
            remote_host.clone(),
            u32::from(remote_port),
            origin.ip().to_string(),
            u32::from(origin.port()),
        )
        .await
    {
        Ok(channel) => channel,
        Err(error) => {
            let message = describe_open_failure(&error, &remote_host, remote_port);
            log::warn!("{label} was not forwarded: {message}");
            // Reported as a rule failure rather than swallowed: a refusal here
            // is nearly always the server's forwarding policy or a wrong
            // target, and both need the user to change something.
            emit(
                &events,
                SshEvent::TunnelFailed {
                    rule: label,
                    message,
                },
            );
            return;
        }
    };

    pump(channel, socket, &label).await;
}

/// Carries bytes both ways between an accepted socket and its channel until
/// either side closes.
async fn pump(channel: Channel<client::Msg>, mut local: TcpStream, label: &str) {
    // `into_stream` is what makes a channel look like a socket. Dropping the
    // stream closes the channel, so the channel's lifetime is exactly this
    // task's.
    let mut remote = channel.into_stream();

    match tokio::io::copy_bidirectional(&mut local, &mut remote).await {
        Ok((to_remote, to_local)) => {
            log::debug!("{label} closed after {to_remote} bytes out and {to_local} bytes in");
        }
        // Logged rather than escalated: one broken forwarded connection says
        // nothing about the transport, and if the transport really is gone the
        // session's main loop reports that on its own.
        Err(error) => log::debug!("{label} stopped forwarding: {error}"),
    }
}

/// Proxies one accepted SOCKS client through the session.
///
/// The client names its destination in the handshake, the channel is opened to
/// exactly that — a host name unresolved, for the server to resolve — and the
/// client is told how it went in its own protocol before any data flows.
///
/// A refused channel is answered to the client, which shows its own error, and
/// logged; it is not reported as a [`SshEvent::TunnelFailed`] the way a
/// refused `-L` connection is, because a browser behind a proxy opens dozens
/// of connections and an unreachable site is not something to write into the
/// user's terminal. The exception is the server's forwarding *policy* — a
/// server that refuses every destination — which the user has to hear about to
/// make sense of a proxy that never works; that is reported once per rule.
async fn socks_forward<H>(
    handle: Arc<Handle<H>>,
    mut socket: TcpStream,
    origin: SocketAddr,
    label: String,
    refusal_reported: Arc<AtomicBool>,
    events: rulogman_stream::Sender<SshEvent>,
) where
    H: client::Handler + 'static,
{
    if let Err(error) = socket.set_nodelay(true) {
        log::debug!("could not disable Nagle's algorithm on {label}: {error}");
    }

    let request =
        match tokio::time::timeout(SOCKS_HANDSHAKE_TIMEOUT, socks::accept(&mut socket)).await {
            Ok(Ok(request)) => request,
            Ok(Err(error)) => {
                log::debug!("{label} was not proxied: {error}");
                return;
            }
            Err(_) => {
                log::debug!("{label} sent no SOCKS request in time");
                return;
            }
        };
    let target = format!("{}:{}", request.host, request.port);
    log::debug!("{label} asks for {target}");

    let opened = handle
        .channel_open_direct_tcpip(
            request.host.clone(),
            u32::from(request.port),
            origin.ip().to_string(),
            u32::from(origin.port()),
        )
        .await;
    let channel = match opened {
        Ok(channel) => channel,
        Err(error) => {
            let message = describe_open_failure(&error, &request.host, request.port);
            log::warn!("{label} was not forwarded: {message}");
            let outcome = socks_outcome(&error);
            if outcome == socks::Outcome::NotAllowed
                && !refusal_reported.swap(true, Ordering::Relaxed)
            {
                emit(
                    &events,
                    SshEvent::TunnelFailed {
                        rule: label,
                        message,
                    },
                );
            }
            if let Err(error) = socks::reply(&mut socket, request.version, outcome).await {
                log::debug!("could not tell the SOCKS client about {target}: {error}");
            }
            return;
        }
    };

    if let Err(error) = socks::reply(&mut socket, request.version, socks::Outcome::Connected).await
    {
        log::debug!("{label} went away before {target} was open: {error}");
        return;
    }
    pump(channel, socket, &format!("{label} to {target}")).await;
}

/// The SOCKS reply a refused channel translates to.
///
/// "Not allowed" covers a server that will not forward at all as well as one
/// that will not forward *there*: both are policy, and both leave the user
/// with something to change on the server rather than in the client.
fn socks_outcome(error: &RusshError) -> socks::Outcome {
    match error {
        RusshError::ChannelOpenFailure(
            ChannelOpenFailure::AdministrativelyProhibited | ChannelOpenFailure::UnknownChannelType,
        ) => socks::Outcome::NotAllowed,
        RusshError::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => {
            socks::Outcome::Unreachable
        }
        _ => socks::Outcome::Failed,
    }
}

/// Explains why the server would not open a forwarding channel.
///
/// The distinction the message has to draw is *whose* configuration is at
/// fault — the server's forwarding policy, or the target host and port — since
/// that is the difference between two completely different fixes.
fn describe_open_failure(error: &RusshError, remote_host: &str, remote_port: u16) -> String {
    match error {
        RusshError::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => format!(
            "the server refused to forward to {remote_host}:{remote_port}; it most likely has \
             AllowTcpForwarding disabled, or restricts the destinations it will open"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => format!(
            "the server could not reach {remote_host}:{remote_port}; check the host name as it \
             resolves on the remote host, the port, and the target's own firewall"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::ResourceShortage) => format!(
            "the server is out of resources and would not open a channel to \
             {remote_host}:{remote_port}"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::UnknownChannelType) => format!(
            "the server does not implement direct-tcpip forwarding, so {remote_host}:\
             {remote_port} cannot be reached through it"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::Other { code, reason }) => format!(
            "the server refused to forward to {remote_host}:{remote_port} with code {code}: \
             {reason}"
        ),
        other => format!("could not forward to {remote_host}:{remote_port}: {other}"),
    }
}
