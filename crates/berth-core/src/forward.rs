//! Local port forwards. v1 binds loopback only and never listens publicly.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::Result;
use crate::transport::Connection;

/// Forwards always bind here; a public bind is not offered in v1.
pub const BIND_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

impl Forward {
    pub fn new(local_port: u16, remote_host: impl Into<String>, remote_port: u16) -> Self {
        Self {
            local_port,
            remote_host: remote_host.into(),
            remote_port,
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(BIND_ADDR, self.local_port)
    }

    /// Two forwards conflict when they want the same local port.
    pub fn conflicts_with(&self, other: &Forward) -> bool {
        self.local_port == other.local_port
    }
}

/// Parse an OpenSSH `LocalForward` argument pair.
///
/// Accepts `8080 localhost:80` and `127.0.0.1:8080 localhost:80`. A bind
/// address is parsed and then ignored, because v1 binds loopback only and
/// silently binding somewhere other than what the file says would be worse
/// than not supporting it.
pub fn parse_local_forward(value: &str) -> Option<Forward> {
    let mut parts = value.split_whitespace();
    let local = parts.next()?;
    let remote = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let local_port: u16 = match local.rsplit_once(':') {
        Some((_bind, port)) => port.parse().ok()?,
        None => local.parse().ok()?,
    };
    let (remote_host, remote_port) = remote.rsplit_once(':')?;
    if remote_host.is_empty() {
        return None;
    }
    Some(Forward::new(
        local_port,
        remote_host,
        remote_port.parse().ok()?,
    ))
}

impl std::fmt::Display for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "127.0.0.1:{} -> {}:{}",
            self.local_port, self.remote_host, self.remote_port
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_loopback_only() {
        let f = Forward::new(8080, "localhost", 80);
        assert_eq!(f.bind_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(f.bind_addr().port(), 8080);
    }

    #[test]
    fn conflicts_on_local_port() {
        let a = Forward::new(8080, "a", 80);
        let b = Forward::new(8080, "b", 443);
        let c = Forward::new(8081, "a", 80);
        assert!(a.conflicts_with(&b));
        assert!(!a.conflicts_with(&c));
    }

    #[test]
    fn displays_readably() {
        assert_eq!(
            Forward::new(5432, "db.internal", 5432).to_string(),
            "127.0.0.1:5432 -> db.internal:5432"
        );
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn a_bare_local_port_parses() {
        let f = parse_local_forward("8080 localhost:80").expect("valid");
        assert_eq!(f, Forward::new(8080, "localhost", 80));
    }

    /// The bind address is accepted and dropped: v1 is loopback only, and
    /// binding somewhere other than what the file says would be worse than
    /// not supporting it.
    #[test]
    fn a_bind_address_is_accepted_and_ignored() {
        let f = parse_local_forward("127.0.0.1:5432 db.internal:5432").expect("valid");
        assert_eq!(f, Forward::new(5432, "db.internal", 5432));
        assert_eq!(f.bind_addr().ip(), BIND_ADDR);
    }

    #[test]
    fn a_remote_without_a_port_is_rejected() {
        assert!(parse_local_forward("8080 localhost").is_none());
    }

    #[test]
    fn extra_words_are_rejected_rather_than_guessed_at() {
        assert!(parse_local_forward("8080 localhost:80 extra").is_none());
    }

    #[test]
    fn a_missing_remote_is_rejected() {
        assert!(parse_local_forward("8080").is_none());
    }

    #[test]
    fn a_port_out_of_range_is_rejected() {
        assert!(parse_local_forward("99999 localhost:80").is_none());
    }
}

/// A running local forward: a loopback listener whose connections are carried
/// over the ssh transport.
///
/// Dropping this closes the listener, which is how a reconnect retires the
/// forwards belonging to the dead connection before starting new ones against
/// the new one. Connections already in flight die with their channels, which is
/// correct: the transport they were riding on is gone.
pub struct Forwarder {
    forward: Forward,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Forwarder {
    pub fn forward(&self) -> &Forward {
        &self.forward
    }

    /// Bind the local port and serve it until dropped.
    ///
    /// Binding is awaited rather than spawned so a port already in use is
    /// reported to the caller. A forward that cannot bind must not be reported
    /// as restored.
    pub async fn start(connection: Arc<Connection>, forward: Forward) -> Result<Self> {
        let listener = TcpListener::bind(forward.bind_addr()).await?;
        let (shutdown, mut rx) = tokio::sync::watch::channel(false);

        let served = forward.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = rx.changed() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, origin)) = accepted else {
                    break;
                };
                let connection = connection.clone();
                let served = served.clone();
                tokio::spawn(async move {
                    if let Err(error) = carry(connection, served, stream, origin).await {
                        tracing::debug!(%error, "forwarded connection ended");
                    }
                });
            }
        });

        Ok(Self { forward, shutdown })
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Pump one accepted connection over a `direct-tcpip` channel.
async fn carry(
    connection: Arc<Connection>,
    forward: Forward,
    mut stream: TcpStream,
    origin: SocketAddr,
) -> Result<()> {
    let mut channel = connection
        .open_forward_channel(&forward.remote_host, forward.remote_port, origin)
        .await?;

    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        tokio::select! {
            read = stream.read(&mut buffer) => {
                let n = read?;
                if n == 0 {
                    let _ = channel.eof().await;
                    return Ok(());
                }
                channel.data(&buffer[..n]).await?;
            }
            message = channel.wait() => {
                match message {
                    Some(russh::ChannelMsg::Data { data }) => stream.write_all(&data).await?,
                    Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                        return Ok(());
                    }
                    Some(_) => {}
                }
            }
        }
    }
}
