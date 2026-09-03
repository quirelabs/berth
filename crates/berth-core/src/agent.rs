//! The OS SSH agent.
//!
//! Berth never reads a private key file. Every signature is produced by the
//! agent, so the key material stays where the user put it. The concrete stream
//! type differs per platform, so it is boxed into one type here.

use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::ssh_key::HashAlg;

/// One agent type across platforms: a Unix socket, or a Windows named pipe.
type Stream = Box<dyn AgentStream + Send + Unpin>;

/// The pipe the Windows OpenSSH agent listens on.
#[cfg(windows)]
pub const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("no ssh agent is reachable: {0}")]
    Unreachable(String),
    #[error(transparent)]
    Keys(#[from] russh::keys::Error),
    #[error("connection closed while signing")]
    Closed,
}

impl From<russh::SendError> for AgentError {
    fn from(_: russh::SendError) -> Self {
        AgentError::Closed
    }
}

pub struct Agent {
    client: AgentClient<Stream>,
}

impl Agent {
    /// Connect to whichever agent this platform exposes.
    ///
    /// On Windows the OpenSSH agent's named pipe is tried first and Pageant
    /// second. On Unix `SSH_AUTH_SOCK` is authoritative, which is also how a
    /// 1Password agent is found, since it just sets that variable.
    pub async fn connect() -> Result<Self, AgentError> {
        #[cfg(windows)]
        let stream = match AgentClient::connect_named_pipe(WINDOWS_AGENT_PIPE).await {
            Ok(client) => client.into_inner(),
            // Report the pipe error rather than Pageant's: the pipe is what
            // almost every Windows user actually has.
            Err(pipe_error) => AgentClient::connect_pageant()
                .await
                .map_err(|_| AgentError::Unreachable(pipe_error.to_string()))?
                .into_inner(),
        };
        #[cfg(unix)]
        let stream = AgentClient::connect_env()
            .await
            .map_err(|e| AgentError::Unreachable(e.to_string()))?
            .into_inner();
        #[cfg(not(any(windows, unix)))]
        return Err(AgentError::Unreachable("unsupported platform".into()));

        #[cfg(any(windows, unix))]
        Ok(Self {
            client: AgentClient::connect(stream),
        })
    }

    /// Connect to an agent at an explicit path, honouring `IdentityAgent`.
    pub async fn connect_at(path: &str) -> Result<Self, AgentError> {
        #[cfg(windows)]
        let client = AgentClient::connect_named_pipe(path)
            .await
            .map_err(|e| AgentError::Unreachable(e.to_string()))?;
        #[cfg(unix)]
        let client = AgentClient::connect_uds(path)
            .await
            .map_err(|e| AgentError::Unreachable(e.to_string()))?;
        #[cfg(not(any(windows, unix)))]
        return Err(AgentError::Unreachable("unsupported platform".into()));

        #[cfg(any(windows, unix))]
        Ok(Self {
            client: AgentClient::connect(client.into_inner()),
        })
    }

    /// Every key the agent is willing to offer, in its preferred order.
    pub async fn identities(&mut self) -> Result<Vec<AgentIdentity>, AgentError> {
        Ok(self.client.request_identities().await?)
    }
}

impl russh::Signer for Agent {
    type Error = AgentError;

    async fn auth_sign(
        &mut self,
        key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        Ok(self.client.sign_request(key, hash_alg, to_sign).await?)
    }
}

/// The public key behind an identity, whatever form the agent offered it in.
pub fn identity_key(identity: &AgentIdentity) -> russh::keys::ssh_key::PublicKey {
    match identity {
        AgentIdentity::PublicKey { key, .. } => key.clone(),
        AgentIdentity::Certificate { certificate, .. } => russh::keys::ssh_key::PublicKey::new(
            certificate.public_key().clone(),
            certificate.comment(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn the_windows_pipe_path_is_the_openssh_default() {
        assert_eq!(WINDOWS_AGENT_PIPE, r"\\.\pipe\openssh-ssh-agent");
    }

    #[test]
    fn a_dropped_connection_maps_to_closed() {
        let error: AgentError = russh::SendError {}.into();
        assert!(matches!(error, AgentError::Closed));
    }
}
