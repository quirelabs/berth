//! The SSH transport: connect, verify the host key, authenticate via the agent,
//! and run commands.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use russh::client::{Config, Handle, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::{ChannelMsg, Disconnect};

use crate::agent::{Agent, identity_key};
use crate::error::{Error, Result};
use crate::host::Host;
use crate::known_hosts::{TrustPolicy, Verdict, accepts, verify};
use crate::pty::{Pty, PtyEvent};
use crate::session::Session;

/// Keepalive interval, matching the reconnect state machine.
pub const KEEPALIVE: Duration = Duration::from_secs(15);

/// Records what the handler decided so the caller can report it precisely.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Option<Verdict>>>);

/// The russh callback side of a connection.
struct Client {
    host: String,
    port: u16,
    policy: TrustPolicy,
    seen: Seen,
}

impl Handler for Client {
    type Error = Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool> {
        let public = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            russh::keys::PublicKeyOrCertificate::Certificate(cert) => {
                PublicKey::new(cert.public_key().clone(), cert.comment())
            }
        };
        let verdict = verify(&self.host, self.port, &public);
        let allowed = accepts(&verdict, self.policy);
        if let Ok(mut slot) = self.seen.0.lock() {
            *slot = Some(verdict);
        }
        Ok(allowed)
    }
}

/// The result of running one command over an exec channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<u32>,
}

impl Output {
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

pub struct Connection {
    handle: Handle<Client>,
    seen: Seen,
}

impl Connection {
    /// Connect, verify the host key, and authenticate with the agent.
    pub async fn connect(host: &Host, policy: TrustPolicy) -> Result<Self> {
        let user = host
            .user
            .clone()
            .or_else(whoami)
            .ok_or_else(|| Error::Config("no user for host and none inferable".into()))?;

        let config = Arc::new(Config {
            keepalive_interval: Some(KEEPALIVE),
            ..Default::default()
        });

        let seen = Seen::default();
        let client = Client {
            host: host.hostname.clone(),
            port: host.port,
            policy,
            seen: seen.clone(),
        };

        let mut handle =
            russh::client::connect(config, (host.hostname.as_str(), host.port), client)
                .await
                .map_err(|e| Self::explain(&seen, e))?;

        let mut agent = match &host.identity_agent {
            Some(path) => Agent::connect_at(&shellexpand::tilde(path)).await,
            None => Agent::connect().await,
        }
        .map_err(|e| Error::Agent(e.to_string()))?;

        let identities = agent
            .identities()
            .await
            .map_err(|e| Error::Agent(e.to_string()))?;
        if identities.is_empty() {
            return Err(Error::NoAgent);
        }

        // Offer each identity in the agent's own order, as OpenSSH does.
        for identity in &identities {
            let public = identity_key(identity);
            let result = handle
                .authenticate_publickey_with(user.clone(), public, None, &mut agent)
                .await
                .map_err(|e| Error::Agent(e.to_string()))?;
            if result.success() {
                return Ok(Self { handle, seen });
            }
        }

        Err(Error::AuthFailed {
            user,
            offered: identities.len(),
        })
    }

    /// Turn a connect-time failure into the host key error when that is what
    /// actually happened, since russh only reports a rejected key generically.
    fn explain(seen: &Seen, error: Error) -> Error {
        if let Ok(slot) = seen.0.lock()
            && let Some(verdict) = slot.as_ref()
        {
            match verdict {
                Verdict::Changed { line, fingerprint } => {
                    return Error::HostKeyChanged {
                        line: *line,
                        fingerprint: fingerprint.clone(),
                    };
                }
                Verdict::Unknown { fingerprint } => {
                    return Error::HostKeyUnknown {
                        fingerprint: fingerprint.clone(),
                    };
                }
                Verdict::Trusted => {}
            }
        }
        error
    }

    /// What the host key check concluded, for the UI to display.
    pub fn host_key_verdict(&self) -> Option<Verdict> {
        self.seen.0.lock().ok().and_then(|s| s.clone())
    }

    /// Run one command on an exec channel and collect everything it produced.
    pub async fn exec(&self, command: &str) -> Result<Output> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        drain(&mut channel).await
    }

    /// Send `data` to the remote as one command's stdin, and collect the reply.
    /// Used to push the probe without a second round trip for the body.
    pub async fn exec_with_stdin(&self, command: &str, stdin: &[u8]) -> Result<Output> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        channel.data(stdin).await?;
        channel.eof().await?;
        drain(&mut channel).await
    }

    /// Open an interactive shell, attaching to `session`'s tmux session.
    ///
    /// `new-session -A` makes first connect and re-attach the same code path,
    /// so reconnecting loses nothing and tmux repaints the grid.
    pub async fn open_session(
        &self,
        session: &Session,
    ) -> Result<(Pty, tokio::sync::mpsc::Receiver<PtyEvent>)> {
        let channel = self.handle.channel_open_session().await?;
        crate::pty::open(
            channel,
            Some(&session.attach_command()),
            session.cols,
            session.rows,
        )
        .await
    }

    /// Open a plain login shell, for servers without tmux.
    pub async fn open_shell(
        &self,
        cols: u16,
        rows: u16,
    ) -> Result<(Pty, tokio::sync::mpsc::Receiver<PtyEvent>)> {
        let channel = self.handle.channel_open_session().await?;
        crate::pty::open(channel, None, cols, rows).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await?;
        Ok(())
    }
}

/// Read a channel to completion.
///
/// The loop runs until russh reports the channel finished, deliberately not
/// stopping at `Eof` or `Close`. A server may send `exit-status`, and further
/// data, after EOF; breaking early loses the exit code or truncates the output,
/// and which of the two happens depends on timing.
async fn drain(channel: &mut russh::Channel<russh::client::Msg>) -> Result<Output> {
    let mut out = Output {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
    };
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
            // ext 1 is stderr; no other extended type is defined for exec.
            ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => out.exit_code = Some(exit_status),
            _ => {}
        }
    }
    Ok(out)
}

fn whoami() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_reports_success_only_on_zero() {
        let mut out = Output {
            stdout: b"hi".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        assert!(out.succeeded());
        assert_eq!(out.stdout_string(), "hi");
        out.exit_code = Some(1);
        assert!(!out.succeeded());
        out.exit_code = None;
        assert!(!out.succeeded(), "an unknown exit code is not a success");
    }

    #[test]
    fn invalid_utf8_output_does_not_panic() {
        let out = Output {
            stdout: vec![0xff, 0xfe, b'a'],
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        assert!(out.stdout_string().ends_with('a'));
    }
}
