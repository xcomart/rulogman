//! Local port forwarding on a live session's transport.
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

use futures::channel::mpsc::UnboundedSender;
use russh::client::{self, Handle};
use russh::{ChannelOpenFailure, Error as RusshError};
use tokio::net::{TcpListener, TcpStream};

use crate::config::TunnelForward;
use crate::event::SshEvent;
use crate::session::emit;

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
    events: &UnboundedSender<SshEvent>,
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
fn label(rule: &TunnelForward) -> String {
    format!(
        "{}:{}:{}",
        rule.local_port, rule.remote_host, rule.remote_port
    )
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
    events: UnboundedSender<SshEvent>,
) where
    H: client::Handler + 'static,
{
    let mut connections = 0u64;
    let mut accept_errors = 0u32;

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
    events: UnboundedSender<SshEvent>,
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

    // `into_stream` is what makes a channel look like a socket. Dropping the
    // stream closes the channel, so the channel's lifetime is exactly this
    // task's.
    let mut remote = channel.into_stream();
    let mut local = socket;

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
