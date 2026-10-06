//! Claude's hook reporting to a worker (D50), instead of writing an escape
//! sequence to the terminal (D48).
//!
//! The hook is a shell script, and a shell has no portable way to talk to a
//! socket, so it calls `berth --hook` with the line it would have written.
//! That line is the same as before, token and all, and the worker checks the
//! token as the app did. One connection, one line, then closed.

use std::path::Path;
use std::time::Duration;

/// The longest line a hook sends: a token, a UUID and an encoded path, with
/// room to spare.
pub const MAX_LINE: usize = 4096;

/// How long `berth --hook` and the worker give each other. The hook must
/// never hold Claude up.
pub const WITHIN: Duration = Duration::from_secs(2);

/// Send one line to the hook socket at `socket`. Errors are the caller's to
/// ignore: a hook that cannot report changes nothing for Claude.
pub fn send(socket: &Path, line: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_write_timeout(Some(WITHIN))?;
        let line = line.trim();
        if line.len() > MAX_LINE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "hook line too long",
            ));
        }
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        // Stay until the worker has read it and closed. Closing first would
        // leave the worker unable to ask who connected (macOS answers that
        // only while both ends are open), and it refuses who it cannot name.
        stream.shutdown(std::net::Shutdown::Write)?;
        stream.set_read_timeout(Some(WITHIN))?;
        let mut rest = [0u8; 64];
        while let Ok(n) = std::io::Read::read(&mut stream, &mut rest) {
            if n == 0 {
                break;
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, line);
        Ok(())
    }
}

/// `berth --hook <line>`: send it to the socket in `BERTH_HOOK`. Always
/// succeeds from the hook's point of view.
pub fn run_process(args: &[String]) {
    let (Some(socket), Some(line)) = (crate::env::hook(), args.get(1)) else {
        return;
    };
    let _ = send(&socket, line);
}

/// One line from a hook connection, or `None` if it does not send one in
/// time or sends too much.
pub async fn read_line<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(256);
    let read = tokio::time::timeout(WITHIN, async {
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') || buf.len() > MAX_LINE {
                break;
            }
        }
        Some(())
    })
    .await;
    read.ok()??;
    if buf.len() > MAX_LINE + 1 {
        return None;
    }
    let line = String::from_utf8(buf).ok()?;
    Some(line.lines().next().unwrap_or("").to_string())
}
