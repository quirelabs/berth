//! A shell on this machine, behind the same handle as a remote one.
//!
//! The seam is [`Pty`] and [`PtyEvent`]: the view, the output pump and the
//! recording never learn where the bytes came from, so a local session is a
//! second constructor rather than a second architecture (D36).
//!
//! Local sessions are deliberately plain. No tmux, no persistence across
//! quitting Berth, and no reconnect machine, because there is nothing to
//! reconnect to. Every other terminal behaves this way.
//!
//! `portable-pty` does the platform work. It is blocking, so the two ends are
//! bridged to the async world by threads rather than by pretending otherwise.

use std::io::{Read, Write};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::pty::{INPUT_QUEUE_SIZE, OUTPUT_QUEUE_SIZE, Pty, PtyEvent, PtyInput};
use crate::session::TERM;

/// Read at most this much per wakeup. Matches the ssh side's appetite: output
/// arrives far faster than a UI can paint, and the pump coalesces anyway.
const READ_CHUNK: usize = 8 * 1024;

/// The shell to run, in the order it is decided.
///
/// `BERTH_SHELL` wins so a settings page has something to set. After that the
/// platform's own answer, and a fallback that exists on every machine so this
/// can never return nothing.
pub fn login_shell() -> String {
    if let Some(shell) = crate::env::shell() {
        return shell;
    }
    #[cfg(windows)]
    {
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// Open a shell on this machine.
///
/// Returns the same pair as the ssh path, so everything above it is unchanged.
pub async fn open(cols: u16, rows: u16) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
    open_with(&login_shell(), true, cols, rows).await
}

/// Open a named shell, optionally as a login shell.
///
/// Split out from [`open`] so the tests can drive a plain `sh` in a known
/// state. A login shell runs the user's profile, which is right for a session
/// and wrong for a test: it can print, prompt, or take a second, and none of
/// that is what the tests below are about.
async fn open_with(
    shell: &str,
    login: bool,
    cols: u16,
    rows: u16,
) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
    let size = PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = native_pty_system()
        .openpty(size)
        .map_err(|e| Error::Pty(format!("could not open a local pty: {e}")))?;

    let mut command = CommandBuilder::new(shell);
    // A login shell, because that is what sets PATH. Without it Homebrew is
    // missing on macOS and the first thing anyone types fails to be found,
    // which is the same class of problem as the non-interactive ssh PATH note
    // in the runbook. cmd.exe has no equivalent and needs no equivalent.
    #[cfg(not(windows))]
    if login {
        command.arg("-l");
    }
    #[cfg(windows)]
    let _ = login;
    // The parser is a real terminal, so say so. Without this, programs fall
    // back to line mode or refuse to run.
    command.env("TERM", TERM);
    if let Some(home) = dirs::home_dir() {
        command.cwd(home);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| Error::Pty(format!("could not start {shell}: {e}")))?;

    // The slave must go now. Holding it keeps the write end open, so when the
    // shell exits the master never reaches EOF, the reader blocks forever and
    // the session looks alive after it has ended.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| Error::Pty(format!("could not read the local pty: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| Error::Pty(format!("could not write the local pty: {e}")))?;

    let (input_tx, mut input_rx) = mpsc::channel::<PtyInput>(INPUT_QUEUE_SIZE);
    let (output_tx, output_rx) = mpsc::channel::<PtyEvent>(OUTPUT_QUEUE_SIZE);

    // Reading is a blocking syscall with no async equivalent that works on all
    // three platforms, so it gets a thread. `blocking_send` applies the same
    // backpressure the ssh side gets from its bounded channel.
    let reads = output_tx.clone();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                // EOF: the shell is gone.
                Ok(0) => break,
                Ok(n) => {
                    if reads
                        .blocking_send(PtyEvent::Output(buffer[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Report the exit before the close, in that order, matching ssh.
        if let Ok(status) = child.wait() {
            let _ = reads.blocking_send(PtyEvent::Exit(status.exit_code()));
        }
        let _ = reads.blocking_send(PtyEvent::Closed);
    });

    // Writes and resizes share a thread because both need the master, and
    // because a write that blocks must not stall the reader.
    let master = pair.master;
    std::thread::spawn(move || {
        while let Some(request) = input_rx.blocking_recv() {
            match request {
                PtyInput::Data(bytes) => {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                PtyInput::Resize { cols, rows } => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                PtyInput::Close => break,
            }
        }
        // Dropping the master hangs up the shell, which ends the reader.
        drop(master);
    });

    Ok((Pty { input: input_tx }, output_rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// The shell the tests drive, and never the user's. `sh` exists on every
    /// unix, speaks one dialect, and starts in the same state on a laptop and
    /// on a CI runner with no `SHELL` set at all. Windows has no equivalent
    /// that both exists everywhere and speaks this dialect, so these three run
    /// where they can be trusted rather than everywhere.
    #[cfg(unix)]
    const TEST_SHELL: &str = "/bin/sh";

    /// Collect output until `needle` appears, or give up.
    #[cfg(unix)]
    async fn read_until(events: &mut mpsc::Receiver<PtyEvent>, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Duration::from_secs(10);
        while let Ok(Some(event)) = timeout(deadline, events.recv()).await {
            match event {
                PtyEvent::Output(bytes) => {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                    if seen.contains(needle) {
                        return seen;
                    }
                }
                PtyEvent::Closed => return seen,
                PtyEvent::Exit(_) => {}
            }
        }
        seen
    }

    #[test]
    fn a_shell_is_always_named() {
        assert!(!login_shell().is_empty());
    }

    /// The whole point: bytes typed here come back through the same handle the
    /// ssh path uses.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_local_shell_echoes_what_it_is_told() {
        let (pty, mut events) = open_with(TEST_SHELL, false, 80, 24)
            .await
            .expect("a local shell should open");
        assert!(pty.write("echo berth-local-ok\n").await);
        let seen = read_until(&mut events, "berth-local-ok").await;
        assert!(seen.contains("berth-local-ok"), "got {seen:?}");
        pty.close().await;
    }

    /// The slave-drop bug: if the write end is held, the shell exiting never
    /// reaches EOF and the session looks alive forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exiting_shell_reports_closed() {
        let (pty, mut events) = open_with(TEST_SHELL, false, 80, 24)
            .await
            .expect("a local shell should open");
        assert!(pty.write("exit\n").await);

        let mut closed = false;
        while let Ok(Some(event)) = timeout(Duration::from_secs(10), events.recv()).await {
            if event == PtyEvent::Closed {
                closed = true;
                break;
            }
        }
        assert!(closed, "the shell exited and the session never closed");
    }

    /// `stty size` rather than `tput cols`: it asks the tty by ioctl, so it
    /// needs no terminfo database and answers the same on a bare runner.
    ///
    /// No sleep between the resize and the question. Both travel the same
    /// input channel and one thread drains it in order, so the ioctl has
    /// already happened by the time the shell reads the line. If that ever
    /// stops being true this test fails rather than passing slowly.
    #[cfg(unix)]
    #[tokio::test]
    async fn resizing_reaches_the_shell() {
        let (pty, mut events) = open_with(TEST_SHELL, false, 80, 24)
            .await
            .expect("a local shell should open");

        // Ask before resizing as well, so a test that stopped being able to
        // tell the two apart fails here rather than passing on both.
        assert!(pty.write("stty size\n").await);
        let before = read_until(&mut events, "24 80").await;
        assert!(before.contains("24 80"), "got {before:?}");

        assert!(pty.resize(120, 40).await);
        assert!(pty.write("stty size\n").await);
        let after = read_until(&mut events, "40 120").await;
        assert!(after.contains("40 120"), "got {after:?}");
        pty.close().await;
    }
}
