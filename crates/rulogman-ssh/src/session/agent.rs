//! Authentication using keys held by the local SSH agent.

use russh::client::{AuthResult, Handle};
use russh::keys::{self, Algorithm};

use super::{ClientHandler, Failure, Leg, SshErrorKind};

/// Offers the agent's keys to one verified target or jump host.
pub(super) async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    leg: &Leg<'_>,
) -> Result<(), Failure> {
    let mut agent = connect()
        .await
        .map_err(|error| Failure::new(error.kind, format!("{}: {}", leg.label(), error.message)))?;
    let identities = agent.request_identities().await.map_err(|error| {
        Failure::new(
            SshErrorKind::KeyLoad,
            format!(
                "the SSH agent would not list its identities for {}: {error}",
                leg.label()
            ),
        )
    })?;
    if identities.is_empty() {
        return Err(Failure::new(
            SshErrorKind::KeyLoad,
            format!("the SSH agent returned no identities for {}", leg.label()),
        ));
    }

    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|error| {
            Failure::new(
                SshErrorKind::Io,
                format!(
                    "could not negotiate a signature algorithm with {}: {error}",
                    leg.label()
                ),
            )
        })?
        .flatten();
    let mut offered = false;
    for identity in identities {
        let keys::agent::AgentIdentity::PublicKey { key, .. } = identity else {
            // Certificate authentication is a separate SSH method.
            continue;
        };
        // RSA signatures need the negotiated hash; Ed25519 and ECDSA do not.
        let hash = match key.algorithm() {
            Algorithm::Rsa { .. } => rsa_hash,
            _ => None,
        };
        offered = true;
        match handle
            .authenticate_publickey_with(leg.username, key, hash, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Ok(()),
            Ok(AuthResult::Failure { .. }) => (),
            Err(error) => {
                return Err(Failure::new(
                    SshErrorKind::Io,
                    format!(
                        "SSH agent authentication with {} failed: {error}",
                        leg.label()
                    ),
                ));
            }
        }
    }

    if !offered {
        return Err(Failure::new(
            SshErrorKind::KeyLoad,
            format!(
                "the SSH agent offered only unsupported certificate identities for {}",
                leg.label()
            ),
        ));
    }
    Err(Failure::new(
        SshErrorKind::Auth,
        format!(
            "{} rejected the SSH agent keys for user {}",
            leg.label(),
            leg.username
        ),
    ))
}

/// Connects to OpenSSH and compatible agents on Unix, including macOS.
#[cfg(unix)]
async fn connect() -> Result<keys::agent::client::AgentClient<tokio::net::UnixStream>, Failure> {
    keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|error| {
            Failure::new(
                SshErrorKind::KeyLoad,
                format!("could not reach the SSH agent on $SSH_AUTH_SOCK: {error}"),
            )
        })
}

/// Tries the Windows OpenSSH service first, then Pageant.
#[cfg(windows)]
async fn connect() -> Result<
    keys::agent::client::AgentClient<Box<dyn keys::agent::client::AgentStream + Send + Unpin>>,
    Failure,
> {
    const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    match keys::agent::client::AgentClient::connect_named_pipe(OPENSSH_PIPE).await {
        Ok(agent) => return Ok(agent.dynamic()),
        Err(error) => log::debug!("no OpenSSH agent on {OPENSSH_PIPE}: {error}"),
    }
    keys::agent::client::AgentClient::connect_pageant()
        .await
        .map(keys::agent::client::AgentClient::dynamic)
        .map_err(|error| {
            Failure::new(
                SshErrorKind::KeyLoad,
                format!("could not reach an SSH agent (OpenSSH or Pageant): {error}"),
            )
        })
}
