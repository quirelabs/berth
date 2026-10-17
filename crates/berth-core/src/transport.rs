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

/// How long a host has to complete the handshake before Berth gives up.
///
/// Without this the OS decides, which is over a minute on macOS and can be
/// several on Windows, and a single asleep machine in the ssh config leaves a
/// row spinning at every launch with nothing to click. Ten seconds is long
/// enough for a slow link and a busy server to finish a key exchange, and short
/// enough that the answer arrives while someone is still looking.
///
/// The same number for the probe and the session on purpose: they do the same
/// work up to the key, and two numbers would only differ by accident.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Records what the handler decided, and the key it decided about, so the
/// caller can report it precisely and record exactly what it verified.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Option<(Verdict, PublicKey)>>>);

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
        let allowed = accepts(&verdict, &self.policy);
        if let Ok(mut slot) = self.seen.0.lock() {
            *slot = Some((verdict, public));
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
        Self::connect_within(host, policy, CONNECT_TIMEOUT).await
    }

    /// [`connect`](Self::connect) with the deadline named, so a test can use a
    /// short one rather than making the suite wait out the real one.
    ///
    /// The deadline covers the handshake only. Authentication follows it and
    /// talks to the local agent, which fails fast on its own.
    async fn connect_within(host: &Host, policy: TrustPolicy, within: Duration) -> Result<Self> {
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

        let mut handle = tokio::time::timeout(
            within,
            russh::client::connect(config, (host.hostname.as_str(), host.port), client),
        )
        .await
        .map_err(|_| Error::Timeout {
            endpoint: endpoint(host),
            seconds: within.as_secs(),
        })?
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

        // An RSA key is signed with the strongest hash the server takes, as
        // OpenSSH does: without one the signature is SHA-1 `ssh-rsa`, which
        // current servers refuse. A server that does not say is asked for
        // SHA-512, which every server that takes RSA at all takes now.
        let rsa_hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .unwrap_or(Some(russh::keys::HashAlg::Sha512));

        // Offer each identity in the agent's own order, as OpenSSH does.
        for identity in &identities {
            let public = identity_key(identity);
            let hash = matches!(public.algorithm(), russh::keys::Algorithm::Rsa { .. })
                .then_some(rsa_hash)
                .flatten();
            let result = handle
                .authenticate_publickey_with(user.clone(), public, hash, &mut agent)
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

    /// Ask a host for its key and stop there.
    ///
    /// The handshake runs only as far as the key check, which refuses whatever
    /// it is shown. Nothing is authenticated: a host the user has not accepted
    /// never sees an identity, and one they go on to reject is left with no
    /// session open. What comes back is the key the server put on the wire,
    /// which is the one the user is then asked about and the one recorded if
    /// they say yes.
    pub async fn offered_key(host: &Host) -> Result<(Verdict, PublicKey)> {
        Self::offered_key_within(host, CONNECT_TIMEOUT).await
    }

    /// [`offered_key`](Self::offered_key) with the deadline named. See
    /// [`connect_within`](Self::connect_within).
    async fn offered_key_within(host: &Host, within: Duration) -> Result<(Verdict, PublicKey)> {
        let seen = Seen::default();
        let client = Client {
            host: host.hostname.clone(),
            port: host.port,
            policy: TrustPolicy::Refuse,
            seen: seen.clone(),
        };
        let outcome = match tokio::time::timeout(
            within,
            russh::client::connect(
                Arc::new(Config::default()),
                (host.hostname.as_str(), host.port),
                client,
            ),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::Timeout {
                endpoint: endpoint(host),
                seconds: within.as_secs(),
            }),
        };

        if let Ok(slot) = seen.0.lock()
            && let Some(pair) = slot.clone()
        {
            return Ok(pair);
        }
        // The handshake never reached the key: a name that does not resolve, a
        // refused socket, something that is not an ssh server. That failure is
        // the honest answer, and there is no fingerprint to show.
        Err(outcome.err().unwrap_or_else(|| {
            Error::Transport("the host completed a handshake without offering a key".into())
        }))
    }

    /// Turn a connect-time failure into the host key error when that is what
    /// actually happened, since russh only reports a rejected key generically.
    fn explain(seen: &Seen, error: Error) -> Error {
        if let Ok(slot) = seen.0.lock()
            && let Some((verdict, _)) = slot.as_ref()
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
        self.seen
            .0
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|(v, _)| v.clone()))
    }

    /// The key this connection actually verified.
    ///
    /// Returned so the accept path records the key it just checked rather than
    /// asking the server a second time, which would leave a gap for a different
    /// key to be recorded than the one that was approved.
    pub fn host_key(&self) -> Option<PublicKey> {
        self.seen
            .0
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|(_, k)| k.clone()))
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

    /// Run `command` on an exec channel and hand back the channel as a byte
    /// stream both ways: no pty, and no login shell beyond the one sshd
    /// itself runs the command with. What Berth's server relay rides on
    /// (D55).
    pub async fn open_stream(
        &self,
        command: &str,
    ) -> Result<russh::ChannelStream<russh::client::Msg>> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        Ok(channel.into_stream())
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

    /// Open a `direct-tcpip` channel to `host:port` as seen from the remote.
    ///
    /// This is the outbound half of a local forward: the listener is ours, the
    /// far end of the channel is the server's connection to the target.
    pub async fn open_forward_channel(
        &self,
        host: &str,
        port: u16,
        origin: std::net::SocketAddr,
    ) -> Result<russh::Channel<russh::client::Msg>> {
        let channel = self
            .handle
            .channel_open_direct_tcpip(
                host,
                u32::from(port),
                &origin.ip().to_string(),
                u32::from(origin.port()),
            )
            .await?;
        Ok(channel)
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

/// What to call a host in an error. The name plus the port, because the port is
/// half of why a connection to the right machine can still find nothing.
fn endpoint(host: &Host) -> String {
    format!("{}:{}", host.hostname, host.port)
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

    /// A listener that completes the TCP handshake and then says nothing, ever.
    /// This is what an asleep machine, a dropped SYN-ACK and a firewall that
    /// swallows the banner all look like from here, and none of them is
    /// reachable from a CI runner, so it is built rather than found.
    async fn a_socket_that_never_speaks() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        tokio::spawn(async move {
            // Hold the accepted socket open. Dropping it would send a FIN, and
            // a closed connection is a different failure than a silent one.
            let _accepted = listener.accept().await;
            std::future::pending::<()>().await;
        });
        port
    }

    fn silent_host(port: u16) -> Host {
        let mut host = Host::new("silent", "127.0.0.1");
        host.port = port;
        host
    }

    /// Without a deadline this waits for the OS, which is over a minute on
    /// macOS, with the row stuck in Connecting and nothing to click.
    #[tokio::test]
    async fn a_host_that_never_answers_gives_up_and_says_so() {
        let port = a_socket_that_never_speaks().await;
        let started = std::time::Instant::now();
        let error = Connection::connect_within(
            &silent_host(port),
            TrustPolicy::KnownHostsOnly,
            Duration::from_millis(200),
        )
        .await
        // Not `expect_err`: that wants a `Debug` `Ok` type, and a live
        // connection is not one.
        .err()
        .expect("a socket that never speaks cannot connect");

        assert!(
            matches!(error, Error::Timeout { .. }),
            "reported as {error} rather than a timeout"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up after {:?}, so the deadline is not the one being used",
            started.elapsed()
        );
        assert!(error.to_string().contains(&port.to_string()), "{error}");
    }

    /// The probe has its own connect call, and an unverified host is exactly
    /// the one most likely to be unreachable.
    #[tokio::test]
    async fn a_probe_of_a_silent_host_gives_up_too() {
        let port = a_socket_that_never_speaks().await;
        let started = std::time::Instant::now();
        let error = Connection::offered_key_within(&silent_host(port), Duration::from_millis(200))
            .await
            .expect_err("a socket that never speaks offers no key");

        assert!(
            matches!(error, Error::Timeout { .. }),
            "reported as {error} rather than a timeout"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
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
