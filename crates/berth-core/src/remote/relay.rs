//! The app's end of the transport to a host's server (D55): an exec channel
//! running `berth-server --connect`, read past whatever the shell prints
//! before the server's greeting.
//!
//! A user's shell startup files may print on any channel (a motd, a
//! greeting, a `conda` banner). Those bytes are skipped until the frame that
//! opens the server's greeting, up to 64 KB and a few seconds; past either,
//! the connection fails with what was read, so the user sees why.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

use crate::error::{Error, Result};
use crate::transport::Connection;

/// How much is read before the greeting at most, and for how long.
pub const SCAN_AT_MOST: usize = 64 * 1024;
pub const SCAN_WITHIN: Duration = Duration::from_secs(5);

/// The longest a greeting's frame can be: a frame longer than this before
/// the magic is not one.
const GREETING_AT_MOST: u32 = 64 * 1024;

/// How many of the bytes read are shown when no greeting came.
const SHOWN: usize = 160;

/// The command that runs the relay: the installed server, by the path the
/// install put it at, which was checked to need no quoting beyond single
/// quotes.
pub fn command(server: &str) -> String {
    format!("'{server}' --connect")
}

/// Open the transport to the server installed at `server` on `connection`'s
/// host: a stream that starts with the server's greeting.
pub async fn open(
    connection: &Connection,
    server: &str,
) -> Result<Greeted<russh::ChannelStream<russh::client::Msg>>> {
    let stream = connection.open_stream(&command(server)).await?;
    past_banner(stream, SCAN_WITHIN, SCAN_AT_MOST).await
}

/// A host's connection as it is now. Whoever reconnects the host puts the
/// new one here, and a server reached through [`opener`] opens its next
/// relay on it: the handle's own reconnects need nothing else.
pub type Current = std::sync::Arc<std::sync::Mutex<std::sync::Arc<Connection>>>;

/// Relays to the server installed at `server`, each opened on the host's
/// connection as it is at the time: what [`crate::server::local::Server::over`]
/// reaches a host's server with.
pub fn opener(current: Current, server: String) -> crate::server::local::Opener {
    std::sync::Arc::new(move || {
        let connection = current.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let server = server.clone();
        Box::pin(async move {
            let stream: Box<dyn crate::server::client::Duplex> =
                Box::new(open(&connection, &server).await?);
            Ok(stream)
        })
    })
}

/// A stream read past whatever came before the greeting: the greeting's
/// frame first, from what was already read, then the stream itself.
pub struct Greeted<S> {
    head: Vec<u8>,
    at: usize,
    inner: S,
}

/// Read `stream` until the frame opening the server's greeting, skipping
/// what comes before it, within `within` and `at_most` bytes.
pub async fn past_banner<S>(mut stream: S, within: Duration, at_most: usize) -> Result<Greeted<S>>
where
    S: AsyncRead + Unpin,
{
    let mut read = Vec::new();
    let mut buf = vec![0u8; 4096];
    let scanning = async {
        loop {
            if let Some(start) = greeting_at(&read) {
                return Ok(start);
            }
            if read.len() >= at_most {
                return Err("too much before it");
            }
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|_| "the channel failed")?;
            if n == 0 {
                return Err("the channel closed");
            }
            read.extend_from_slice(&buf[..n]);
        }
    };
    match tokio::time::timeout(within, scanning).await {
        Ok(Ok(start)) => Ok(Greeted {
            head: read.split_off(start),
            at: 0,
            inner: stream,
        }),
        Ok(Err(why)) => Err(no_greeting(why, &read)),
        Err(_) => Err(no_greeting("none came in time", &read)),
    }
}

/// Where the greeting's frame starts in `read`: four bytes of length before
/// the magic, a length a greeting could have.
fn greeting_at(read: &[u8]) -> Option<usize> {
    let magic = &berth_proto::MAGIC;
    (4..=read.len().saturating_sub(magic.len()))
        .find(|&at| {
            read[at..].starts_with(magic) && {
                let len =
                    u32::from_le_bytes([read[at - 4], read[at - 3], read[at - 2], read[at - 1]]);
                (magic.len() as u32..=GREETING_AT_MOST).contains(&len)
            }
        })
        .map(|at| at - 4)
}

/// The error for a channel that never greeted, with what it did say.
fn no_greeting(why: &str, read: &[u8]) -> Error {
    let shown: String = String::from_utf8_lossy(&read[..read.len().min(SHOWN)])
        .escape_debug()
        .collect();
    Error::Server(format!(
        "Berth's server did not answer on this host ({why}); it said: \"{shown}\""
    ))
}

impl<S: AsyncRead + Unpin> AsyncRead for Greeted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.at < self.head.len() {
            let n = (self.head.len() - self.at).min(out.remaining());
            let at = self.at;
            out.put_slice(&self.head[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, out)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Greeted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn greeting() -> Vec<u8> {
        let hello = crate::server::link::hello().encode().unwrap();
        berth_proto::frame(&hello).unwrap()
    }

    /// What a shell prints first is skipped, and the stream then starts
    /// with the greeting, whole, followed by what comes after.
    #[tokio::test]
    async fn a_banner_before_the_greeting_is_skipped() {
        let (mut host, app) = tokio::io::duplex(1 << 20);
        let mut sent =
            b"Welcome to berth-test\r\nLast login: today\r\nBRTH in a sentence\n".to_vec();
        sent.extend_from_slice(&greeting());
        sent.extend_from_slice(b"after");
        host.write_all(&sent).await.unwrap();
        let mut greeted = past_banner(app, Duration::from_secs(1), SCAN_AT_MOST)
            .await
            .unwrap();
        let mut got = vec![0u8; greeting().len() + 5];
        greeted.read_exact(&mut got).await.unwrap();
        assert_eq!(&got[..greeting().len()], &greeting()[..]);
        assert_eq!(&got[greeting().len()..], b"after");
    }

    /// Nothing before it is nothing to skip.
    #[tokio::test]
    async fn a_greeting_straight_away_is_read_as_it_is() {
        let (mut host, app) = tokio::io::duplex(1 << 20);
        host.write_all(&greeting()).await.unwrap();
        let mut greeted = past_banner(app, Duration::from_secs(1), SCAN_AT_MOST)
            .await
            .unwrap();
        let mut got = vec![0u8; greeting().len()];
        greeted.read_exact(&mut got).await.unwrap();
        assert_eq!(got, greeting());
    }

    /// Past 64 KB of something else, or a few seconds of nothing, or a
    /// channel that closes: a failure that shows what was said.
    #[tokio::test]
    async fn no_greeting_fails_showing_what_came() {
        let (mut host, app) = tokio::io::duplex(1 << 20);
        host.write_all(&vec![b'x'; SCAN_AT_MOST + 10])
            .await
            .unwrap();
        let error = past_banner(app, Duration::from_secs(5), SCAN_AT_MOST)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("too much before it") && error.contains("xxxx"),
            "{error}"
        );

        let (mut host, app) = tokio::io::duplex(1024);
        host.write_all(b"bash: berth-server: command not found\n")
            .await
            .unwrap();
        let error = past_banner(app, Duration::from_millis(200), SCAN_AT_MOST)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("in time") && error.contains("command not found"),
            "{error}"
        );

        let (mut host, app) = tokio::io::duplex(1024);
        host.write_all(b"\x1b[31mno\n").await.unwrap();
        drop(host);
        let error = past_banner(app, Duration::from_secs(1), SCAN_AT_MOST)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("closed") && error.contains("\\u{1b}[31mno"),
            "{error}"
        );
    }

    #[test]
    fn the_command_runs_the_installed_relay() {
        assert_eq!(
            command("/home/u/.local/share/berth/bin/0.1.0+abc/berth-server"),
            "'/home/u/.local/share/berth/bin/0.1.0+abc/berth-server' --connect"
        );
    }
}
