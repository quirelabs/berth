//! `berth-server --connect`: the far end of the app's exec channel on a
//! host (D55). It makes sure the host's supervisor runs, then passes bytes
//! between the channel (its stdin and stdout) and the supervisor's socket,
//! unread, until either side closes. Nothing else is written to stdout: the
//! first bytes the app finds there after any the shell printed are the
//! supervisor's greeting.
//!
//! Two plain threads, one each way: a relay has nothing else to do, and
//! every write is flushed at once, so a keystroke is never held for a
//! buffer to fill. Hosts are Linux and macOS (D55), so a Unix socket.

use std::io::{Read, Write};

use super::paths::Places;
use crate::error::{Error, Result};

/// How much is moved at a time each way.
const CHUNK: usize = 64 * 1024;

/// The relay as a host runs it.
pub async fn run_process() -> Result<bool> {
    let places = Places::standard()
        .ok_or_else(|| Error::Server("no data directory for the runtime files".into()))?;
    let exe = std::env::current_exe()?;
    super::start::ensure_running(&places, &exe).await?;
    places.trusted()?;
    #[cfg(unix)]
    {
        let path = places.supervisor();
        let stream = std::os::unix::net::UnixStream::connect(&path)
            .map_err(|e| Error::Server(format!("no supervisor answers: {e}")))?;
        tokio::task::spawn_blocking(move || relay(std::io::stdin(), std::io::stdout(), stream))
            .await
            .map_err(|_| Error::Server("the relay stopped".into()))??;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        Err(Error::Server("hosts are Linux and macOS".into()))
    }
}

/// Pass bytes between `input`/`output` and `stream`, both ways, until either
/// side closes: the app's channel ending its input, or the supervisor
/// closing.
#[cfg(unix)]
pub fn relay<R, W>(
    input: R,
    output: W,
    stream: std::os::unix::net::UnixStream,
) -> std::io::Result<()>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut to_server = stream.try_clone()?;
    let mut input = input;
    std::thread::Builder::new()
        .name("berth-relay-up".into())
        .spawn(move || {
            let mut buf = vec![0u8; CHUNK];
            loop {
                match input.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if to_server.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            // The app is done sending: the supervisor sees it close.
            let _ = to_server.shutdown(std::net::Shutdown::Write);
        })?;
    let mut from_server = stream;
    let mut output = output;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = from_server.read(&mut buf)?;
        if n == 0 {
            return output.flush();
        }
        output.write_all(&buf[..n])?;
        output.flush()?;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// Bytes go both ways untouched, and the relay ends when the supervisor
    /// side closes after the app's input does.
    #[test]
    fn bytes_go_both_ways_and_it_ends_with_the_app() {
        let (relay_end, mut server) = UnixStream::pair().unwrap();
        let (app_input, mut app_writes) = UnixStream::pair().unwrap();
        let (mut app_reads, app_output) = UnixStream::pair().unwrap();
        let relaying = std::thread::spawn(move || relay(app_input, app_output, relay_end));

        app_writes.write_all(b"BRTH up").unwrap();
        let mut got = [0u8; 7];
        server.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"BRTH up");

        server.write_all(b"down").unwrap();
        let mut got = [0u8; 4];
        app_reads.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"down");

        // The app closes its side: the server sees the end of its input,
        // closes, and the relay ends.
        drop(app_writes);
        let mut rest = Vec::new();
        server.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        drop(server);
        assert!(relaying.join().unwrap().is_ok());
    }
}
