//! A connection speaking Berth's wire format: the handshake, then frames.

use berth_proto::{Agreed, FrameReader, Hello, frame, negotiate};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

use crate::error::{Error, Result};

/// How long the other side has to say hello before the connection is dropped.
const HELLO_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

const READ_CHUNK: usize = 64 * 1024;

/// The reading half: whole payloads, as they arrive.
pub struct FrameRead<R> {
    inner: R,
    frames: FrameReader,
    buf: Vec<u8>,
}

/// The writing half.
pub struct FrameWrite<W> {
    inner: W,
}

impl<R: AsyncRead + Unpin> FrameRead<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            frames: FrameReader::new(),
            buf: vec![0; READ_CHUNK],
        }
    }

    /// The next payload, or `None` when the other side has closed.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(payload) = self.frames.next_frame().map_err(protocol)? {
                return Ok(Some(payload));
            }
            let n = self.inner.read(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.frames.push(&self.buf[..n]);
        }
    }
}

impl<W: AsyncWrite + Unpin> FrameWrite<W> {
    fn new(inner: W) -> Self {
        Self { inner }
    }

    pub async fn send(&mut self, payload: &[u8]) -> Result<()> {
        let bytes = frame(payload).map_err(protocol)?;
        self.inner.write_all(&bytes).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

pub fn protocol(error: berth_proto::Error) -> Error {
    Error::Server(error.to_string())
}

/// What a handshake leaves: what was agreed, the other side's greeting, and
/// the two halves of the connection.
pub struct Opened<S> {
    pub agreed: Agreed,
    pub theirs: Hello,
    pub read: FrameRead<ReadHalf<S>>,
    pub write: FrameWrite<WriteHalf<S>>,
}

/// Greet, read the other side's greeting, and settle on a version. Both sides
/// send first, so neither waits on the other.
pub async fn open<S>(stream: S, ours: &Hello) -> Result<Opened<S>>
where
    S: AsyncRead + AsyncWrite,
{
    let (read, write) = tokio::io::split(stream);
    let mut read = FrameRead::new(read);
    let mut write = FrameWrite::new(write);
    write.send(&ours.encode().map_err(protocol)?).await?;
    let payload = tokio::time::timeout(HELLO_WITHIN, read.recv())
        .await
        .map_err(|_| Error::Server("no greeting in time".into()))??
        .ok_or_else(|| Error::Server("closed before greeting".into()))?;
    let theirs = Hello::decode(&payload).map_err(protocol)?;
    let agreed = negotiate(ours, &theirs).map_err(protocol)?;
    Ok(Opened {
        agreed,
        theirs,
        read,
        write,
    })
}

/// This build's greeting.
pub fn hello() -> Hello {
    Hello::ours(super::build(), super::FEATURES)
}
